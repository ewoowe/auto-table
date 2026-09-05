//! Plans and applies schema migrations
//!
//! [`plan_migrations`] compares every registered entity against the live
//! database and returns the statements that would bring it in sync. It never
//! executes anything, so it doubles as the dry run: inspect
//! [`MigrationPlan::statements`] and only then call [`apply_migrations`].
//!
//! MySQL and SQLite are supported. PostgreSQL is not: its ALTER syntax splits
//! a column change into several clauses, which needs its own generator.

use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, Statement, TransactionTrait,
};

use crate::diff::{diff_table, ColumnChange, IndexChange, TableDiff};
use crate::parse::{parse_create_table, PRIMARY_INDEX_NAME};
use crate::schema::{get_table_schema, sqlite_type_affinity, ColumnSchema, IndexSchema};
use crate::{get_all_table_statements, get_existing_tables, get_table_name, TableError};

/// The statements needed to bring one table in sync
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableMigration {
    /// Name of the table
    pub table: String,
    /// Statements to run, in order
    pub statements: Vec<String>,
    /// Whether the statements must run together in one transaction
    ///
    /// SQLite cannot alter a column definition, so those changes are applied by
    /// rebuilding the table. That is a sequence of destructive steps
    /// (`DROP TABLE` among them) which must either all happen or none of them.
    pub transactional: bool,
}

impl TableMigration {
    /// A migration whose statements are independent of each other
    pub fn new(table: String, statements: Vec<String>) -> Self {
        Self {
            table,
            statements,
            transactional: false,
        }
    }

    /// A migration whose statements must all succeed or all fail
    pub fn transactional(table: String, statements: Vec<String>) -> Self {
        Self {
            table,
            statements,
            transactional: true,
        }
    }
}

/// A complete migration plan across all registered tables
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationPlan {
    /// One entry per table that needs to change
    pub tables: Vec<TableMigration>,
}

impl MigrationPlan {
    /// Whether anything has to change at all
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// Every statement in the plan, in execution order
    pub fn statements(&self) -> Vec<&str> {
        self.tables
            .iter()
            .flat_map(|table| table.statements.iter())
            .map(String::as_str)
            .collect()
    }
}

/// How to behave when several instances might migrate at the same time
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LockBehavior {
    /// Run without a lock
    ///
    /// Two instances migrating at once will run the same statements and the
    /// second one fails on statements the first already applied. Use the other
    /// variants when the application is deployed more than once.
    #[default]
    None,
    /// Take the lock, and fail if it cannot be had
    Required,
    /// Take the lock, and skip the migration entirely if someone else has it
    ///
    /// The other instance is already applying the same changes, so skipping is
    /// safe.
    SkipIfLocked,
}

/// Options for [`apply_migrations_with`]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrateOptions {
    /// What to do when another instance may be migrating
    pub lock: LockBehavior,
    /// Seconds to wait for the lock; 0 returns immediately
    pub lock_timeout_secs: u32,
}

impl Default for MigrateOptions {
    fn default() -> Self {
        Self {
            lock: LockBehavior::None,
            lock_timeout_secs: 0,
        }
    }
}

impl MigrateOptions {
    /// Migrate only after taking the lock, waiting up to `timeout_secs` for it
    pub fn locked(timeout_secs: u32) -> Self {
        Self {
            lock: LockBehavior::Required,
            lock_timeout_secs: timeout_secs,
        }
    }

    /// Migrate if the lock is free, otherwise leave it to the running instance
    pub fn skip_if_locked(timeout_secs: u32) -> Self {
        Self {
            lock: LockBehavior::SkipIfLocked,
            lock_timeout_secs: timeout_secs,
        }
    }
}

/// What [`apply_migrations_with`] ended up doing
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// The plan was applied
    Applied,
    /// Another instance holds the lock, so nothing was run
    Skipped,
}

/// Compares every registered table against the database and builds a plan
///
/// Tables that do not exist yet are skipped: creating them is the job of
/// [`crate::create_missing_tables`], not of a migration.
pub async fn plan_migrations(db: &DatabaseConnection) -> Result<MigrationPlan, TableError> {
    let backend = db.get_database_backend();
    let existing_tables = get_existing_tables(db, backend).await?;

    let mut tables = Vec::new();

    for statement in get_all_table_statements(backend) {
        let table_name = get_table_name(&statement).unwrap_or_else(|| "unknown".to_string());

        if !existing_tables.contains(&table_name) {
            continue;
        }

        let create_sql = backend.build(&statement).sql;
        let expected = parse_create_table(&create_sql).map_err(|source| {
            TableError::ParseExpectedFailed {
                table: table_name,
                source,
            }
        })?;
        let expected = normalize_expected(expected, backend);
        let actual = get_table_schema(db, &expected.name).await?;

        let diff = diff_table(&expected, &actual);
        if diff.is_empty() {
            continue;
        }

        tables.push(plan_table_migration(&diff, backend, &create_sql)?);
    }

    Ok(MigrationPlan { tables })
}

/// Executes a plan, running its statements in order
///
/// A [`TableMigration`] marked transactional runs inside a single transaction,
/// so a failure part way through leaves the table untouched.
pub async fn apply_migrations(
    db: &DatabaseConnection,
    plan: &MigrationPlan,
) -> Result<(), TableError> {
    apply_migrations_with(db, plan, MigrateOptions::default()).await?;
    Ok(())
}

/// Plans and applies migrations in one step
///
/// This is the entry point to prefer. Under a lock the plan is built *after*
/// the lock is taken, so it is built once and cannot go stale: building it
/// beforehand would mean either trusting a plan that may already be outdated,
/// or building it a second time behind the lock.
///
/// Reach for [`plan_migrations`] separately only when the statements should be
/// reviewed before anything runs.
pub async fn migrate(
    db: &DatabaseConnection,
    options: MigrateOptions,
) -> Result<MigrationOutcome, TableError> {
    match options.lock {
        LockBehavior::None => {
            let plan = plan_migrations(db).await?;
            apply_migrations_with(db, &plan, options).await
        }
        LockBehavior::Required | LockBehavior::SkipIfLocked => apply_under_lock(db, options).await,
    }
}

/// Executes an already built plan, optionally holding a lock while doing so
///
/// Without a lock, two instances starting together apply the same statements
/// and the slower one fails on statements the faster one already ran. With a
/// lock, only the instance holding it migrates.
///
/// Under a lock the plan is rebuilt once the lock is held, so `plan` only
/// decides whether migrating is worth attempting at all. Use [`migrate`] to
/// plan and apply without building a plan twice.
pub async fn apply_migrations_with(
    db: &DatabaseConnection,
    plan: &MigrationPlan,
    options: MigrateOptions,
) -> Result<MigrationOutcome, TableError> {
    if plan.is_empty() {
        return Ok(MigrationOutcome::Applied);
    }

    if options.lock == LockBehavior::None {
        for migration in &plan.tables {
            if migration.transactional {
                apply_in_transaction(db, &migration.statements).await?;
            } else {
                run_statements(db, &migration.statements).await?;
            }
        }
        return Ok(MigrationOutcome::Applied);
    }

    apply_under_lock(db, options).await
}

/// Turns the diff of one table into the statements that apply it
///
/// Use [`plan_table_migration`] when the target is SQLite: changing a column
/// definition there rebuilds the table, which needs the full `CREATE TABLE`.
pub fn plan_table_statements(
    diff: &TableDiff,
    backend: DbBackend,
) -> Result<Vec<String>, TableError> {
    plan_table_migration(diff, backend, "").map(|migration| migration.statements)
}

/// Turns the diff of one table into a [`TableMigration`]
///
/// `create_sql` is the `CREATE TABLE` statement the entity declares. MySQL
/// ignores it; SQLite needs it to rebuild a table.
pub fn plan_table_migration(
    diff: &TableDiff,
    backend: DbBackend,
    create_sql: &str,
) -> Result<TableMigration, TableError> {
    match backend {
        DbBackend::MySql => Ok(TableMigration::new(
            diff.table.clone(),
            mysql_statements(diff),
        )),
        DbBackend::Sqlite if sqlite_needs_rebuild(diff) => Ok(TableMigration::transactional(
            diff.table.clone(),
            sqlite_rebuild(diff, create_sql)?,
        )),
        DbBackend::Sqlite => Ok(TableMigration::new(
            diff.table.clone(),
            sqlite_simple_alters(diff),
        )),
        other => Err(TableError::UnsupportedBackend(other)),
    }
}

// ---------------------------------------------------------------------------
// Shared execution helpers
// ---------------------------------------------------------------------------

async fn execute<C: ConnectionTrait>(conn: &C, sql: &str) -> Result<(), TableError> {
    let backend = conn.get_database_backend();
    conn.execute_raw(Statement::from_string(backend, sql.to_string()))
        .await
        .map_err(|source| TableError::MigrationFailed {
            sql: sql.to_string(),
            source,
        })?;
    Ok(())
}

/// Runs a group of statements in order
async fn run_statements<C: ConnectionTrait>(
    conn: &C,
    statements: &[String],
) -> Result<(), TableError> {
    for sql in statements {
        execute(conn, sql).await?;
    }
    Ok(())
}

/// Applies a whole plan while holding a lock, so only one instance migrates
///
/// Everything runs on a single transaction. That is not incidental:
///
/// - MySQL's `GET_LOCK` is a *session* lock, so it only guards statements sent
///   over the same connection. Running the migration inside a transaction pins
///   it to one connection, which is what makes the lock effective. (DDL commits
///   the transaction implicitly, but that does not release the lock — verified
///   against MySQL 8.)
/// - SQLite has no named locks, but its own write lock already serialises
///   writers; the transaction is what the table rebuild needs anyway.
async fn apply_under_lock(
    db: &DatabaseConnection,
    options: MigrateOptions,
) -> Result<MigrationOutcome, TableError> {
    let backend = db.get_database_backend();
    let is_sqlite = backend == DbBackend::Sqlite;

    if is_sqlite {
        execute(db, "PRAGMA foreign_keys = OFF").await?;
    }

    let transaction = db
        .begin()
        .await
        .map_err(|source| TableError::MigrationFailed {
            sql: "BEGIN".to_string(),
            source,
        })?;

    let acquired = match backend {
        DbBackend::MySql => acquire_mysql_lock(&transaction, options.lock_timeout_secs).await,
        DbBackend::Sqlite => acquire_sqlite_lock(&transaction, options.lock_timeout_secs).await,
        other => Err(TableError::UnsupportedBackend(other)),
    };

    // A `false` or an error returns early, so getting past this match means the
    // lock is held.
    match acquired {
        Ok(true) => {}
        Ok(false) => {
            let _ = transaction.rollback().await;
            if is_sqlite {
                execute(db, "PRAGMA foreign_keys = ON").await?;
            }
            return match options.lock {
                LockBehavior::Required => Err(TableError::MigrationLockNotAcquired {
                    timeout_secs: options.lock_timeout_secs,
                }),
                _ => Ok(MigrationOutcome::Skipped),
            };
        }
        Err(error) => {
            let _ = transaction.rollback().await;
            if is_sqlite {
                execute(db, "PRAGMA foreign_keys = ON").await?;
            }
            return Err(error);
        }
    };

    // Plan again now that the lock is ours: while we were waiting, another
    // instance may well have applied the very same changes, and replaying a
    // stale plan would only fail on statements already applied.
    let plan = plan_migrations(db).await?;

    // Already inside a transaction, so statements run as they are: a nested
    // transaction would not work on either backend.
    let mut failure = None;
    for migration in &plan.tables {
        if let Err(error) = run_statements(&transaction, &migration.statements).await {
            failure = Some(error);
            break;
        }
    }

    let outcome = match failure {
        Some(error) => {
            release_lock(&transaction, backend).await?;
            let _ = transaction.rollback().await;
            Err(error)
        }
        None => {
            release_lock(&transaction, backend).await?;
            transaction
                .commit()
                .await
                .map_err(|source| TableError::MigrationFailed {
                    sql: "COMMIT".to_string(),
                    source,
                })?;
            Ok(MigrationOutcome::Applied)
        }
    };

    if is_sqlite {
        execute(db, "PRAGMA foreign_keys = ON").await?;
    }

    outcome
}

/// Prefix of the MySQL lock name
///
/// The database name is appended, so two applications sharing a MySQL instance
/// do not block each other.
const LOCK_NAME_PREFIX: &str = "auto-table-migration";

/// Takes MySQL's named lock, returning whether it was obtained
///
/// Must be called on the same connection that runs the migration, which the
/// surrounding transaction guarantees.
async fn acquire_mysql_lock(
    transaction: &DatabaseTransaction,
    timeout_secs: u32,
) -> Result<bool, TableError> {
    let sql = format!(
        "SELECT GET_LOCK(CONCAT('{LOCK_NAME_PREFIX}-', DATABASE()), {timeout_secs})"
    );

    let row = transaction
        .query_one_raw(Statement::from_string(DbBackend::MySql, sql))
        .await
        .map_err(|source| TableError::MigrationFailed {
            sql: "GET_LOCK".to_string(),
            source,
        })?;

    // 1 = obtained, 0 = timed out, NULL = an error occurred
    let obtained = row
        .and_then(|row| row.try_get_by_index::<i32>(0).ok())
        .unwrap_or(0);

    Ok(obtained == 1)
}

async fn release_mysql_lock(transaction: &DatabaseTransaction) -> Result<(), TableError> {
    let sql = format!("SELECT RELEASE_LOCK(CONCAT('{LOCK_NAME_PREFIX}-', DATABASE()))");

    transaction
        .execute_raw(Statement::from_string(DbBackend::MySql, sql))
        .await
        .map_err(|source| TableError::MigrationFailed {
            sql: "RELEASE_LOCK".to_string(),
            source,
        })?;

    Ok(())
}

/// SQLite has no named locks; its write lock already serialises writers
///
/// All this does is make a concurrent writer wait for its turn instead of
/// failing immediately with SQLITE_BUSY.
async fn acquire_sqlite_lock(
    transaction: &DatabaseTransaction,
    timeout_secs: u32,
) -> Result<bool, TableError> {
    let millis = u64::from(timeout_secs) * 1000;
    execute(transaction, &format!("PRAGMA busy_timeout = {millis}")).await?;
    Ok(true)
}

async fn release_lock(
    transaction: &DatabaseTransaction,
    backend: DbBackend,
) -> Result<(), TableError> {
    match backend {
        DbBackend::MySql => release_mysql_lock(transaction).await,
        // SQLite releases its write lock when the transaction ends
        _ => Ok(()),
    }
}

/// Runs a group of statements as one unit
async fn apply_in_transaction(
    db: &DatabaseConnection,
    statements: &[String],
) -> Result<(), TableError> {
    // SQLite turns `PRAGMA foreign_keys` into a no-op inside a transaction, so
    // it has to be toggled around the transaction rather than within it.
    let is_sqlite = db.get_database_backend() == DbBackend::Sqlite;
    if is_sqlite {
        execute(db, "PRAGMA foreign_keys = OFF").await?;
    }

    let transaction = db.begin().await.map_err(|source| TableError::MigrationFailed {
        sql: "BEGIN".to_string(),
        source,
    })?;

    let mut failure = None;
    for sql in statements {
        if let Err(error) = execute(&transaction, sql).await {
            failure = Some(error);
            break;
        }
    }

    let outcome = match failure {
        Some(error) => {
            let _ = transaction.rollback().await;
            Err(error)
        }
        None => transaction
            .commit()
            .await
            .map_err(|source| TableError::MigrationFailed {
                sql: "COMMIT".to_string(),
                source,
            }),
    };

    if is_sqlite {
        execute(db, "PRAGMA foreign_keys = ON").await?;
    }

    outcome
}

/// Rewrites a parsed structure so it compares equal to what the reader reports
///
/// SQLite stores types loosely, so the declared spelling on both sides is
/// reduced to a type affinity before they are compared.
fn normalize_expected(mut schema: crate::schema::TableSchema, backend: DbBackend) -> crate::schema::TableSchema {
    if backend == DbBackend::Sqlite {
        for column in &mut schema.columns {
            column.col_type = sqlite_type_affinity(&column.col_type);
        }
    }
    schema
}

// ---------------------------------------------------------------------------
// MySQL
// ---------------------------------------------------------------------------

/// Builds the MySQL statements for a diff
///
/// The order is deliberate:
///
/// 1. drop indexes, so nothing still points at a column that is about to change
/// 2. drop columns
/// 3. add columns
/// 4. modify columns
/// 5. add indexes, so they are built on the final column definitions
fn mysql_statements(diff: &TableDiff) -> Vec<String> {
    let table = quote_mysql_identifier(&diff.table);
    let mut statements = Vec::new();

    for change in &diff.indexes {
        if let IndexChange::Drop { name } = change {
            statements.push(if name == PRIMARY_INDEX_NAME {
                format!("ALTER TABLE {table} DROP PRIMARY KEY")
            } else {
                format!("ALTER TABLE {table} DROP INDEX {}", quote_mysql_identifier(name))
            });
        }
    }

    for change in &diff.columns {
        if let ColumnChange::Drop { name } = change {
            statements.push(format!(
                "ALTER TABLE {table} DROP COLUMN {}",
                quote_mysql_identifier(name)
            ));
        }
    }

    for change in &diff.columns {
        if let ColumnChange::Add(column) = change {
            statements.push(format!(
                "ALTER TABLE {table} ADD COLUMN {}",
                mysql_column_definition(column)
            ));
        }
    }

    for change in &diff.columns {
        if let ColumnChange::Alter { to, .. } = change {
            statements.push(format!(
                "ALTER TABLE {table} MODIFY COLUMN {}",
                mysql_column_definition(to)
            ));
        }
    }

    for change in &diff.indexes {
        if let IndexChange::Add(index) = change {
            statements.push(mysql_add_index(&table, index));
        }
    }

    statements
}

/// Renders a column as it appears inside an `ALTER TABLE` statement
///
/// The type is emitted verbatim: it already comes from the statement sea-query
/// generated for this same backend, so it needs no translation.
fn mysql_column_definition(column: &ColumnSchema) -> String {
    let mut definition = format!(
        "{} {}",
        quote_mysql_identifier(&column.name),
        column.col_type
    );

    if !column.nullable {
        definition.push_str(" NOT NULL");
    }
    if let Some(default) = &column.default {
        definition.push_str(&format!(" DEFAULT {}", quote_mysql_literal(default)));
    }
    if column.auto_increment {
        definition.push_str(" AUTO_INCREMENT");
    }

    definition
}

fn mysql_add_index(table: &str, index: &IndexSchema) -> String {
    let columns = index
        .columns
        .iter()
        .map(|column| quote_mysql_identifier(column))
        .collect::<Vec<_>>()
        .join(", ");

    if index.primary {
        format!("ALTER TABLE {table} ADD PRIMARY KEY ({columns})")
    } else if index.unique {
        format!(
            "ALTER TABLE {table} ADD UNIQUE INDEX {} ({columns})",
            quote_mysql_identifier(&index.name)
        )
    } else {
        format!(
            "ALTER TABLE {table} ADD INDEX {} ({columns})",
            quote_mysql_identifier(&index.name)
        )
    }
}

/// Quotes an identifier with backticks, MySQL style
fn quote_mysql_identifier(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// Quotes a literal, escaping the quotes it contains
fn quote_mysql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

// ---------------------------------------------------------------------------
// SQLite
// ---------------------------------------------------------------------------

/// Suffix used for the replacement table while a table is being rebuilt
const REBUILD_SUFFIX: &str = "__auto_table_rebuild";

/// Whether SQLite can express this diff with plain `ALTER TABLE` statements
///
/// SQLite can add and drop columns, but it cannot touch a column definition —
/// there is no `MODIFY COLUMN` and no `ALTER COLUMN`. Anything else has to go
/// through a table rebuild, and that includes **every** index change: a unique
/// constraint is part of the table definition, and the index backing it cannot
/// be dropped on its own at all.
fn sqlite_needs_rebuild(diff: &TableDiff) -> bool {
    let changes_a_column = diff
        .columns
        .iter()
        .any(|change| matches!(change, ColumnChange::Alter { .. }));

    changes_a_column || !diff.indexes.is_empty()
}

/// Builds the SQLite statements for a diff that needs no table rebuild
fn sqlite_simple_alters(diff: &TableDiff) -> Vec<String> {
    let table = quote_sqlite_identifier(&diff.table);
    let mut statements = Vec::new();

    for change in &diff.indexes {
        if let IndexChange::Drop { name } = change {
            statements.push(format!("DROP INDEX {}", quote_sqlite_identifier(name)));
        }
    }

    for change in &diff.columns {
        if let ColumnChange::Drop { name } = change {
            statements.push(format!(
                "ALTER TABLE {table} DROP COLUMN {}",
                quote_sqlite_identifier(name)
            ));
        }
    }

    for change in &diff.columns {
        if let ColumnChange::Add(column) = change {
            statements.push(format!(
                "ALTER TABLE {table} ADD COLUMN {}",
                sqlite_column_definition(column)
            ));
        }
    }

    for change in &diff.indexes {
        if let IndexChange::Add(index) = change {
            // A primary key can only be created with the table, so reaching
            // here with one means the caller should have rebuilt instead.
            if !index.primary {
                statements.push(sqlite_create_index(&diff.table, index));
            }
        }
    }

    statements
}

/// Builds the statements that rebuild a table with the structure the entity declares
///
/// A rebuild is the only way to change a column definition on SQLite. It creates
/// the new table from the entity's own `CREATE TABLE`, copies over the rows, then
/// swaps the tables. The caller must run these statements inside a transaction.
fn sqlite_rebuild(diff: &TableDiff, create_sql: &str) -> Result<Vec<String>, TableError> {
    if create_sql.trim().is_empty() {
        return Err(TableError::MissingCreateStatement {
            table: diff.table.clone(),
        });
    }

    let target = parse_create_table(create_sql).map_err(|source| TableError::ParseExpectedFailed {
        table: diff.table.clone(),
        source,
    })?;

    let table = quote_sqlite_identifier(&diff.table);
    let replacement_name = format!("{}{}", diff.table, REBUILD_SUFFIX);
    let replacement = quote_sqlite_identifier(&replacement_name);

    let mut statements = vec![retarget_create_table(create_sql, &diff.table, &replacement_name)];

    if let Some(insert) = sqlite_copy_rows(&target, &replacement, &table, diff) {
        statements.push(insert);
    }

    statements.push(format!("DROP TABLE {table}"));
    statements.push(format!("ALTER TABLE {replacement} RENAME TO {table}"));

    // Indexes die with the old table and have to be created again. The primary
    // key and inline unique constraints are already part of the new definition.
    for change in &diff.indexes {
        if let IndexChange::Add(index) = change {
            if !index.primary {
                // The replacement table has already been renamed back, so the
                // index is created against the original table name.
                statements.push(sqlite_create_index(&diff.table, index));
            }
        }
    }

    Ok(statements)
}

/// Builds the `INSERT INTO ... SELECT` that carries rows into the new table
///
/// Only the columns present on both sides can be copied; columns that the entity
/// no longer declares are simply left behind.
fn sqlite_copy_rows(
    target: &crate::schema::TableSchema,
    replacement: &str,
    table: &str,
    diff: &TableDiff,
) -> Option<String> {
    let added: Vec<&str> = diff
        .columns
        .iter()
        .filter_map(|change| match change {
            ColumnChange::Add(column) => Some(column.name.as_str()),
            _ => None,
        })
        .collect();

    let dropped: Vec<&str> = diff
        .columns
        .iter()
        .filter_map(|change| match change {
            ColumnChange::Drop { name } => Some(name.as_str()),
            _ => None,
        })
        .collect();

    // The old table had every column the new one declares except the added
    // ones, plus the columns that are about to be dropped.
    let mut old_columns: Vec<&str> = target
        .columns
        .iter()
        .filter(|column| !added.contains(&column.name.as_str()))
        .map(|column| column.name.as_str())
        .collect();
    old_columns.extend(dropped.iter().copied());

    let shared: Vec<&str> = target
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .filter(|name| old_columns.contains(name))
        .collect();

    if shared.is_empty() {
        return None;
    }

    let columns = shared
        .iter()
        .map(|name| quote_sqlite_identifier(name))
        .collect::<Vec<_>>()
        .join(", ");

    Some(format!(
        "INSERT INTO {replacement} ({columns}) SELECT {columns} FROM {table}"
    ))
}

/// Rewrites a `CREATE TABLE` statement to build the table under another name
fn retarget_create_table(sql: &str, table: &str, replacement: &str) -> String {
    let old = quote_sqlite_identifier(table);
    let new = quote_sqlite_identifier(replacement);

    // Only the table name right after CREATE TABLE may be replaced; the same
    // spelling cannot legitimately appear anywhere else in the statement.
    match sql.find(&old) {
        Some(position) => format!("{}{}{}", &sql[..position], new, &sql[position + old.len()..]),
        None => sql.to_string(),
    }
}

fn sqlite_column_definition(column: &ColumnSchema) -> String {
    let mut definition = format!(
        "{} {}",
        quote_sqlite_identifier(&column.name),
        column.col_type
    );

    if !column.nullable {
        definition.push_str(" NOT NULL");
    }
    if let Some(default) = &column.default {
        definition.push_str(&format!(" DEFAULT {}", quote_sqlite_literal(default)));
    }

    definition
}

fn sqlite_create_index(table: &str, index: &IndexSchema) -> String {
    let name = quote_sqlite_identifier(&index.name);
    let target = quote_sqlite_identifier(table);
    let columns = index
        .columns
        .iter()
        .map(|column| quote_sqlite_identifier(column))
        .collect::<Vec<_>>()
        .join(", ");

    if index.unique {
        format!("CREATE UNIQUE INDEX {name} ON {target} ({columns})")
    } else {
        format!("CREATE INDEX {name} ON {target} ({columns})")
    }
}

/// Quotes an identifier, SQLite style
fn quote_sqlite_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn quote_sqlite_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::ColumnAspect;

    fn column(name: &str, col_type: &str) -> ColumnSchema {
        ColumnSchema {
            name: name.to_string(),
            col_type: col_type.to_string(),
            nullable: true,
            default: None,
            auto_increment: false,
        }
    }

    fn diff(columns: Vec<ColumnChange>, indexes: Vec<IndexChange>) -> TableDiff {
        TableDiff {
            table: "users".to_string(),
            columns,
            indexes,
        }
    }

    fn mysql_plan(columns: Vec<ColumnChange>, indexes: Vec<IndexChange>) -> Vec<String> {
        plan_table_statements(&diff(columns, indexes), DbBackend::MySql).expect("mysql is supported")
    }

    fn sqlite_plan(columns: Vec<ColumnChange>, indexes: Vec<IndexChange>) -> Vec<String> {
        plan_table_statements(&diff(columns, indexes), DbBackend::Sqlite).expect("sqlite is supported")
    }

    // ---------------------------------------------------------------- mysql

    #[test]
    fn mysql_drops_columns_before_adding_them() {
        assert_eq!(
            mysql_plan(
                vec![
                    ColumnChange::Add(column("email", "varchar(255)")),
                    ColumnChange::Drop { name: "legacy".to_string() },
                ],
                vec![],
            ),
            vec![
                "ALTER TABLE `users` DROP COLUMN `legacy`",
                "ALTER TABLE `users` ADD COLUMN `email` varchar(255)",
            ]
        );
    }

    #[test]
    fn mysql_modify_repeats_the_whole_definition() {
        let target = ColumnSchema {
            name: "age".to_string(),
            col_type: "bigint".to_string(),
            nullable: false,
            default: Some("0".to_string()),
            auto_increment: false,
        };

        assert_eq!(
            mysql_plan(
                vec![ColumnChange::Alter {
                    name: "age".to_string(),
                    to: target,
                    aspects: vec![ColumnAspect::Type {
                        from: "int".to_string(),
                        to: "bigint".to_string(),
                    }],
                }],
                vec![],
            ),
            vec!["ALTER TABLE `users` MODIFY COLUMN `age` bigint NOT NULL DEFAULT '0'"]
        );
    }

    #[test]
    fn mysql_indexes_are_dropped_first_and_added_last() {
        assert_eq!(
            mysql_plan(
                vec![ColumnChange::Drop { name: "legacy".to_string() }],
                vec![
                    IndexChange::Drop { name: "email".to_string() },
                    IndexChange::Add(IndexSchema {
                        name: "email".to_string(),
                        columns: vec!["email".to_string()],
                        unique: true,
                        primary: false,
                    }),
                ],
            ),
            vec![
                "ALTER TABLE `users` DROP INDEX `email`",
                "ALTER TABLE `users` DROP COLUMN `legacy`",
                "ALTER TABLE `users` ADD UNIQUE INDEX `email` (`email`)",
            ]
        );
    }

    #[test]
    fn mysql_primary_key_uses_its_own_syntax() {
        assert_eq!(
            mysql_plan(
                vec![],
                vec![
                    IndexChange::Add(IndexSchema {
                        name: PRIMARY_INDEX_NAME.to_string(),
                        columns: vec!["id".to_string()],
                        unique: true,
                        primary: true,
                    }),
                    IndexChange::Drop { name: PRIMARY_INDEX_NAME.to_string() },
                ],
            ),
            vec![
                "ALTER TABLE `users` DROP PRIMARY KEY",
                "ALTER TABLE `users` ADD PRIMARY KEY (`id`)",
            ]
        );
    }

    #[test]
    fn mysql_escapes_quotes_in_identifiers_and_literals() {
        assert_eq!(
            mysql_plan(
                vec![ColumnChange::Add(ColumnSchema {
                    name: "we`ird".to_string(),
                    col_type: "varchar(255)".to_string(),
                    nullable: false,
                    default: Some("it's".to_string()),
                    auto_increment: false,
                })],
                vec![],
            ),
            vec!["ALTER TABLE `users` ADD COLUMN `we``ird` varchar(255) NOT NULL DEFAULT 'it''s'"]
        );
    }

    // --------------------------------------------------------------- sqlite

    #[test]
    fn sqlite_adds_and_drops_columns_directly() {
        // These need no rebuild, so the statements stay plain ALTERs
        assert_eq!(
            sqlite_plan(
                vec![
                    ColumnChange::Add(column("bio", "TEXT")),
                    ColumnChange::Drop { name: "legacy".to_string() },
                ],
                vec![],
            ),
            vec![
                "ALTER TABLE \"users\" DROP COLUMN \"legacy\"",
                "ALTER TABLE \"users\" ADD COLUMN \"bio\" TEXT",
            ]
        );
    }

    #[test]
    fn sqlite_rebuilds_when_a_column_definition_changes() {
        let target = ColumnSchema {
            name: "name".to_string(),
            col_type: "TEXT".to_string(),
            nullable: false,
            default: None,
            auto_increment: false,
        };
        let create_sql = "CREATE TABLE \"users\" ( \"id\" integer NOT NULL PRIMARY KEY AUTOINCREMENT, \"name\" text NOT NULL )";

        let migration = plan_table_migration(
            &diff(
                vec![ColumnChange::Alter {
                    name: "name".to_string(),
                    to: target,
                    aspects: vec![ColumnAspect::Nullable { from: true, to: false }],
                }],
                vec![],
            ),
            DbBackend::Sqlite,
            create_sql,
        )
        .expect("sqlite plan");

        assert!(migration.transactional, "a rebuild must be transactional");
        assert_eq!(
            migration.statements,
            vec![
                "CREATE TABLE \"users__auto_table_rebuild\" ( \"id\" integer NOT NULL PRIMARY KEY AUTOINCREMENT, \"name\" text NOT NULL )",
                "INSERT INTO \"users__auto_table_rebuild\" (\"id\", \"name\") SELECT \"id\", \"name\" FROM \"users\"",
                "DROP TABLE \"users\"",
                "ALTER TABLE \"users__auto_table_rebuild\" RENAME TO \"users\"",
            ]
        );
    }

    #[test]
    fn sqlite_rebuild_leaves_dropped_columns_behind() {
        let create_sql = "CREATE TABLE \"users\" ( \"id\" integer NOT NULL PRIMARY KEY AUTOINCREMENT, \"name\" text NOT NULL )";

        let migration = plan_table_migration(
            &diff(
                vec![
                    ColumnChange::Drop { name: "legacy".to_string() },
                    ColumnChange::Alter {
                        name: "name".to_string(),
                        to: column("name", "TEXT"),
                        aspects: vec![ColumnAspect::Nullable { from: true, to: false }],
                    },
                ],
                vec![],
            ),
            DbBackend::Sqlite,
            create_sql,
        )
        .expect("sqlite plan");

        // `legacy` exists only in the old table, so it is not copied
        assert!(migration
            .statements
            .iter()
            .any(|sql| sql.contains("SELECT \"id\", \"name\" FROM \"users\"")));
        assert!(!migration
            .statements
            .iter()
            .any(|sql| sql.contains("legacy\") SELECT")));
    }

    #[test]
    fn sqlite_rebuild_fails_without_the_create_statement() {
        let target = ColumnSchema {
            name: "name".to_string(),
            col_type: "TEXT".to_string(),
            nullable: false,
            default: None,
            auto_increment: false,
        };

        let error = plan_table_statements(
            &diff(
                vec![ColumnChange::Alter {
                    name: "name".to_string(),
                    to: target,
                    aspects: vec![ColumnAspect::Nullable { from: true, to: false }],
                }],
                vec![],
            ),
            DbBackend::Sqlite,
        )
        .expect_err("a rebuild needs the CREATE TABLE");

        assert!(matches!(error, TableError::MissingCreateStatement { .. }));
    }

    #[test]
    fn sqlite_rebuilds_when_an_index_changes() {
        // A unique constraint is part of the table definition on SQLite, so it
        // cannot be added with CREATE INDEX nor removed with DROP INDEX.
        let create_sql =
            "CREATE TABLE \"users\" ( \"id\" integer NOT NULL PRIMARY KEY AUTOINCREMENT, \"email\" varchar NOT NULL )";

        let migration = plan_table_migration(
            &diff(
                vec![],
                vec![IndexChange::Add(IndexSchema {
                    name: "email".to_string(),
                    columns: vec!["email".to_string()],
                    unique: true,
                    primary: false,
                })],
            ),
            DbBackend::Sqlite,
            create_sql,
        )
        .expect("sqlite plan");

        assert!(migration.transactional, "an index change rebuilds the table");
        assert!(migration
            .statements
            .iter()
            .any(|sql| sql.contains(REBUILD_SUFFIX)));
    }

    #[test]
    fn only_mysql_and_sqlite_are_supported() {
        let empty = diff(vec![], vec![]);

        assert!(matches!(
            plan_table_statements(&empty, DbBackend::Postgres),
            Err(TableError::UnsupportedBackend(DbBackend::Postgres))
        ));
    }

    #[test]
    fn an_empty_plan_has_no_statements() {
        let plan = MigrationPlan::default();

        assert!(plan.is_empty());
        assert!(plan.statements().is_empty());
    }
}
