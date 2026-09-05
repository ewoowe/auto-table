//! Plans and applies schema migrations
//!
//! [`plan_migrations`] compares every registered entity against the live
//! database and returns the statements that would bring it in sync. It never
//! executes anything, so it doubles as the dry run: inspect
//! [`MigrationPlan::statements`] and only then call [`apply_migrations`].
//!
//! Which statements are produced is up to the backend, through the [`Backend`]
//! trait: everything here — comparing structures, planning, taking the lock and
//! executing — is the same regardless of the database.

use sea_orm::{
    ConnectionTrait, DatabaseConnection, DbBackend, Statement, TransactionTrait,
};

use crate::diff::{diff_table, TableDiff};
use crate::parse::parse_create_table;
use crate::backend::AnyBackend;
use crate::schema::get_table_schema;
use crate::{get_all_table_statements, get_existing_tables, get_table_name, Backend, TableError};

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
    let backend_impl = AnyBackend::for_backend(backend)?;
    let existing_tables = get_existing_tables(db, backend).await?;

    let mut tables = Vec::new();

    for statement in get_all_table_statements(backend) {
        let table_name = get_table_name(&statement).unwrap_or_else(|| "unknown".to_string());

        if !existing_tables.contains(&table_name) {
            continue;
        }

        let create_sql = backend.build(&statement).sql;
        let mut expected = parse_create_table(&create_sql).map_err(|source| {
            TableError::ParseExpectedFailed {
                table: table_name,
                source,
            }
        })?;
        backend_impl.normalize_expected(&mut expected);
        let actual = get_table_schema(db, &expected.name).await?;

        let diff = diff_table(&expected, &actual);
        if diff.is_empty() {
            continue;
        }

        tables.push(backend_impl.plan(&diff, &create_sql)?);
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
    AnyBackend::for_backend(backend)?.plan(diff, create_sql)
}

// ---------------------------------------------------------------------------
// Shared execution helpers
// ---------------------------------------------------------------------------

pub(crate) async fn execute<C: ConnectionTrait>(conn: &C, sql: &str) -> Result<(), TableError> {
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
    let backend = AnyBackend::for_connection(db)?;

    // Some backends have to prepare something outside the transaction: SQLite
    // turns `PRAGMA foreign_keys` into a no-op inside one.
    run_outside(db, backend.before_statements()).await?;

    let transaction = db
        .begin()
        .await
        .map_err(|source| TableError::MigrationFailed {
            sql: "BEGIN".to_string(),
            source,
        })?;

    let acquired = backend
        .acquire_lock(&transaction, options.lock_timeout_secs)
        .await;

    // A `false` or an error returns early, so getting past this match means the
    // lock is held.
    match acquired {
        Ok(true) => {}
        Ok(false) => {
            let _ = transaction.rollback().await;
            run_outside(db, backend.after_statements()).await?;
            return match options.lock {
                LockBehavior::Required => Err(TableError::MigrationLockNotAcquired {
                    timeout_secs: options.lock_timeout_secs,
                }),
                _ => Ok(MigrationOutcome::Skipped),
            };
        }
        Err(error) => {
            let _ = transaction.rollback().await;
            run_outside(db, backend.after_statements()).await?;
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
            backend.release_lock(&transaction).await?;
            let _ = transaction.rollback().await;
            Err(error)
        }
        None => {
            backend.release_lock(&transaction).await?;
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

    run_outside(db, backend.after_statements()).await?;

    outcome
}

/// Runs statements outside of any transaction
async fn run_outside(db: &DatabaseConnection, statements: &[&str]) -> Result<(), TableError> {
    for sql in statements {
        execute(db, sql).await?;
    }
    Ok(())
}

/// Runs a group of statements as one unit
async fn apply_in_transaction(
    db: &DatabaseConnection,
    statements: &[String],
) -> Result<(), TableError> {
    let backend = AnyBackend::for_connection(db)?;

    run_outside(db, backend.before_statements()).await?;

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

    run_outside(db, backend.after_statements()).await?;

    outcome
}


// ---------------------------------------------------------------------------
// PostgreSQL
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sqlite::*;
    use crate::diff::{ColumnAspect, ColumnChange, IndexChange};
    use crate::parse::PRIMARY_INDEX_NAME;
    use crate::schema::{ColumnSchema, IndexSchema};

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

    fn postgres_plan(columns: Vec<ColumnChange>, indexes: Vec<IndexChange>) -> Vec<String> {
        plan_table_statements(&diff(columns, indexes), DbBackend::Postgres)
            .expect("postgres is supported")
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
                    IndexChange::Drop(IndexSchema {
                        name: "email".to_string(),
                        columns: vec!["email".to_string()],
                        unique: true,
                        primary: false,
                    }),
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
                    IndexChange::Drop(IndexSchema {
                        name: PRIMARY_INDEX_NAME.to_string(),
                        columns: vec!["id".to_string()],
                        unique: true,
                        primary: true,
                    }),
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
    fn every_backend_plans_statements() {
        let empty = diff(vec![], vec![]);

        for backend in [DbBackend::MySql, DbBackend::Sqlite, DbBackend::Postgres] {
            let statements = plan_table_statements(&empty, backend)
                .unwrap_or_else(|error| panic!("{backend:?} should be supported: {error}"));
            assert!(statements.is_empty(), "{backend:?} planned {statements:?}");
        }
    }

    #[test]
    fn postgres_changes_one_aspect_per_statement() {
        // Unlike MySQL, which folds everything into one MODIFY COLUMN
        let target = ColumnSchema {
            name: "age".to_string(),
            col_type: "bigint".to_string(),
            nullable: false,
            default: Some("0".to_string()),
            auto_increment: false,
        };

        assert_eq!(
            postgres_plan(
                vec![ColumnChange::Alter {
                    name: "age".to_string(),
                    to: target,
                    aspects: vec![
                        ColumnAspect::Type {
                            from: "integer".to_string(),
                            to: "bigint".to_string(),
                        },
                        ColumnAspect::Nullable {
                            from: true,
                            to: false,
                        },
                        ColumnAspect::Default {
                            from: None,
                            to: Some("0".to_string()),
                        },
                    ],
                }],
                vec![],
            ),
            vec![
                "ALTER TABLE \"users\" ALTER COLUMN \"age\" TYPE bigint",
                "ALTER TABLE \"users\" ALTER COLUMN \"age\" SET NOT NULL",
                "ALTER TABLE \"users\" ALTER COLUMN \"age\" SET DEFAULT '0'",
            ]
        );
    }

    #[test]
    fn postgres_adds_and_drops_columns() {
        assert_eq!(
            postgres_plan(
                vec![
                    ColumnChange::Add(column("bio", "varchar")),
                    ColumnChange::Drop {
                        name: "legacy".to_string()
                    },
                ],
                vec![],
            ),
            vec![
                "ALTER TABLE \"users\" DROP COLUMN \"legacy\"",
                "ALTER TABLE \"users\" ADD COLUMN \"bio\" varchar",
            ]
        );
    }

    #[test]
    fn postgres_drops_a_unique_constraint_not_its_index() {
        // PostgreSQL refuses to drop an index that a constraint depends on
        assert_eq!(
            postgres_plan(
                vec![],
                vec![IndexChange::Drop(IndexSchema {
                    name: "email".to_string(),
                    columns: vec!["email".to_string()],
                    unique: true,
                    primary: false,
                })],
            ),
            vec!["ALTER TABLE \"users\" DROP CONSTRAINT \"users_email_key\""]
        );
    }

    #[test]
    fn postgres_adds_a_unique_constraint() {
        assert_eq!(
            postgres_plan(
                vec![],
                vec![IndexChange::Add(IndexSchema {
                    name: "email".to_string(),
                    columns: vec!["email".to_string()],
                    unique: true,
                    primary: false,
                })],
            ),
            vec!["ALTER TABLE \"users\" ADD CONSTRAINT \"users_email_key\" UNIQUE (\"email\")"]
        );
    }

    #[test]
    fn postgres_uses_its_own_primary_key_constraint_name() {
        assert_eq!(
            postgres_plan(
                vec![],
                vec![IndexChange::Drop(IndexSchema {
                    name: PRIMARY_INDEX_NAME.to_string(),
                    columns: vec!["id".to_string()],
                    unique: true,
                    primary: true,
                })],
            ),
            vec!["ALTER TABLE \"users\" DROP CONSTRAINT \"users_pkey\""]
        );
    }

    #[test]
    fn postgres_escapes_quotes_in_identifiers_and_literals() {
        assert_eq!(
            postgres_plan(
                vec![ColumnChange::Add(ColumnSchema {
                    name: "we\"ird".to_string(),
                    col_type: "varchar".to_string(),
                    nullable: false,
                    default: Some("it's".to_string()),
                    auto_increment: false,
                })],
                vec![],
            ),
            vec![
                "ALTER TABLE \"users\" ADD COLUMN \"we\"\"ird\" varchar NOT NULL DEFAULT 'it''s'"
            ]
        );
    }

    #[test]
    fn an_empty_plan_has_no_statements() {
        let plan = MigrationPlan::default();

        assert!(plan.is_empty());
        assert!(plan.statements().is_empty());
    }
}
