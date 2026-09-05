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
use crate::risk::{classify_changes, ChangeKind, Risk, RiskAction, RiskPolicy};
use crate::schema::get_table_schema;
use crate::{
    create_missing_tables, get_all_table_statements, get_existing_tables, get_table_name, Backend,
    TableError,
};

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
    /// The worst risk anywhere in the table, kept for reporting/diagnostics
    /// (see [`MigrationPlan::risk`]). Whether each change may actually be
    /// applied is decided per item from `changes` against a [`RiskPolicy`]; this
    /// field is never used on its own to gate execution.
    pub risk: Risk,
    /// Every change in the table, paired with its kind and risk, so the plan can
    /// be evaluated against a [`RiskPolicy`] item by item.
    pub changes: Vec<(ChangeKind, Risk)>,
}

impl TableMigration {
    /// A migration whose statements are independent of each other
    pub fn new(table: String, statements: Vec<String>) -> Self {
        Self {
            table,
            statements,
            transactional: false,
            risk: Risk::Safe,
            changes: Vec::new(),
        }
    }

    /// A migration whose statements must all succeed or all fail
    pub fn transactional(table: String, statements: Vec<String>) -> Self {
        Self {
            table,
            statements,
            transactional: true,
            risk: Risk::Safe,
            changes: Vec::new(),
        }
    }
}

/// A complete migration plan across all registered tables
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationPlan {
    /// One entry per table that needs to change
    pub tables: Vec<TableMigration>,
    /// Whether destructive changes (e.g. dropping a column) may be applied
    ///
    /// `false` by default, so [`apply_migrations`] refuses any plan that
    /// contains one until [`MigrationPlan::allow_destructive`] opts in. That
    /// method is shorthand for setting `risk_policy.levels[Destructive]` to
    /// `Allow`; an explicit item-level `Block` (L3) still wins.
    pub allow_destructive: bool,
}

impl MigrationPlan {
    /// Whether anything has to change at all
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// Allows destructive changes to be applied.
    ///
    /// By default [`apply_migrations`] refuses any plan that contains a
    /// destructive change (such as dropping a column); call this only after the
    /// change has been reviewed and approved.
    pub fn allow_destructive(mut self) -> Self {
        self.allow_destructive = true;
        self
    }

    /// The worst risk anywhere in the plan
    pub fn risk(&self) -> Risk {
        self.tables
            .iter()
            .map(|table| table.risk)
            .max()
            .unwrap_or(Risk::Safe)
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateOptions {
    /// What to do when another instance may be migrating
    pub lock: LockBehavior,
    /// Seconds to wait for the lock; 0 returns immediately
    pub lock_timeout_secs: u32,
    /// Three-layer switch controlling which risk items may be applied.
    ///
    /// `global` applies to every change, `levels` to a risk level, and `items`
    /// to a specific [`ChangeKind`]; the most specific layer wins. See
    /// [`RiskPolicy`].
    pub risk_policy: RiskPolicy,
}

impl Default for MigrateOptions {
    fn default() -> Self {
        Self {
            lock: LockBehavior::None,
            lock_timeout_secs: 0,
            risk_policy: RiskPolicy::default(),
        }
    }
}

impl MigrateOptions {
    /// Migrate only after taking the lock, waiting up to `timeout_secs` for it
    pub fn locked(timeout_secs: u32) -> Self {
        Self {
            lock: LockBehavior::Required,
            lock_timeout_secs: timeout_secs,
            risk_policy: RiskPolicy::default(),
        }
    }

    /// Migrate if the lock is free, otherwise leave it to the running instance
    pub fn skip_if_locked(timeout_secs: u32) -> Self {
        Self {
            lock: LockBehavior::SkipIfLocked,
            lock_timeout_secs: timeout_secs,
            risk_policy: RiskPolicy::default(),
        }
    }

    /// Configure the three-layer risk policy used when applying the plan.
    pub fn with_risk_policy(mut self, policy: RiskPolicy) -> Self {
        self.risk_policy = policy;
        self
    }

    /// Allow destructive changes (such as dropping a column) to be applied
    ///
    /// Shorthand for `self.risk_policy.levels.insert(Risk::Destructive,
    /// RiskAction::Allow)`. An explicit item-level `Block` in `risk_policy`
    /// (L3) still wins, because it is more specific than the level rule. Only
    /// call this after the plan has been reviewed and approved; prefer
    /// configuring `risk_policy` directly for finer control.
    pub fn allow_destructive(mut self) -> Self {
        self.risk_policy
            .levels
            .insert(Risk::Destructive, RiskAction::Allow);
        self
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

        let mut migration = backend_impl.plan(&diff, &create_sql)?;
        let changes = classify_changes(&diff);
        migration.risk = changes
            .iter()
            .map(|(_, risk)| *risk)
            .max()
            .unwrap_or(Risk::Safe);
        migration.changes = changes;
        tables.push(migration);
    }

    Ok(MigrationPlan {
        tables,
        allow_destructive: false,
    })
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
/// Builds the risk policy actually applied: the options' [`RiskPolicy`], with
/// the `Destructive` level loosened to `Allow` when the plan was opted in via
/// [`MigrationPlan::allow_destructive`].
///
/// `allow_destructive` is shorthand for "allow the `Destructive` level", so it
/// only ever loosens the configured policy. Because an explicit item-level rule
/// in `items` (L3) outranks a level rule (L2), an `allow_destructive` call
/// cannot override a policy that explicitly blocks a specific [`ChangeKind`].
fn effective_policy(plan: &MigrationPlan, options: &MigrateOptions) -> RiskPolicy {
    let mut policy = options.risk_policy.clone();
    if plan.allow_destructive {
        policy.levels.insert(Risk::Destructive, RiskAction::Allow);
    }
    policy
}

/// Refuses a plan that contains a change blocked by `policy`, listing every
/// blocked change so callers know exactly what was refused.
fn ensure_allowed(plan: &MigrationPlan, policy: &RiskPolicy) -> Result<(), TableError> {
    let mut tables = Vec::new();
    let mut blocked: Vec<String> = Vec::new();
    for table in &plan.tables {
        let mut table_blocked = false;
        for (kind, level) in &table.changes {
            if policy.resolve(*kind, *level) == RiskAction::Block {
                blocked.push(format!("{}: {}", table.table, kind));
                table_blocked = true;
            }
        }
        if table_blocked && !tables.contains(&table.table) {
            tables.push(table.table.clone());
        }
    }
    if blocked.is_empty() {
        Ok(())
    } else {
        Err(TableError::DestructiveChangesBlocked { tables, blocked })
    }
}

pub async fn apply_migrations_with(
    db: &DatabaseConnection,
    plan: &MigrationPlan,
    options: MigrateOptions,
) -> Result<MigrationOutcome, TableError> {
    if plan.is_empty() {
        return Ok(MigrationOutcome::Applied);
    }

    // Check first, run never: refuse any blocked risk item before a single
    // statement executes.
    let policy = effective_policy(plan, &options);
    ensure_allowed(plan, &policy)?;

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

/// What [`ensure_schema`] did, so callers can log or react to the result
#[derive(Debug, Clone)]
pub struct SchemaSyncReport {
    /// Registered tables that already existed and were therefore skipped
    pub existing_tables: Vec<String>,
    /// Registered tables that were newly created in this run
    pub created_tables: Vec<String>,
    /// Whether the migration step applied any change, and if not why
    pub migration: MigrationOutcome,
}

/// Creates every missing registered table, then applies the schema migrations.
///
/// This is the one entry point to call at startup: it both provisions tables
/// that have never existed (via [`create_missing_tables`]) and brings existing
/// ones in line with the entities (via [`migrate`]). The same [`MigrateOptions`]
/// (lock behaviour, risk policy, …) governs the migration half; table creation
/// never touches existing data, so it is unaffected by the risk policy.
pub async fn ensure_schema(
    db: &DatabaseConnection,
    options: MigrateOptions,
) -> Result<SchemaSyncReport, TableError> {
    let report = create_missing_tables(db).await?;
    let migration = migrate(db, options).await?;
    Ok(SchemaSyncReport {
        existing_tables: report.existing_tables,
        created_tables: report.created_tables,
        migration,
    })
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
    let mut migration = AnyBackend::for_backend(backend)?.plan(diff, create_sql)?;
    let changes = classify_changes(diff);
    migration.risk = changes
        .iter()
        .map(|(_, risk)| *risk)
        .max()
        .unwrap_or(Risk::Safe);
    migration.changes = changes;
    Ok(migration)
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

    // Refuse a plan with blocked risk items, releasing the lock and rolling back
    // before returning.
    let policy = effective_policy(&plan, &options);
    if let Err(error) = ensure_allowed(&plan, &policy) {
        backend.release_lock(&transaction).await?;
        let _ = transaction.rollback().await;
        run_outside(db, backend.after_statements()).await?;
        return Err(error);
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::DbBackend;

    use crate::diff::{ColumnChange, IndexChange, TableDiff};

    fn diff(columns: Vec<ColumnChange>, indexes: Vec<IndexChange>) -> TableDiff {
        TableDiff {
            table: "users".to_string(),
            columns,
            indexes,
        }
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
    fn an_empty_plan_has_no_statements() {
        let plan = MigrationPlan::default();

        assert!(plan.is_empty());
        assert!(plan.statements().is_empty());
    }

    #[test]
    fn risk_policy_blocks_according_to_precedence() {
        // A plan that both drops a column (Destructive) and tightens nullability
        // (Caution) — exercises the gate without touching a database.
        let mut plan = MigrationPlan::default();
        plan.tables.push(TableMigration {
            table: "t".into(),
            statements: vec!["ALTER TABLE t DROP COLUMN c".into()],
            transactional: false,
            risk: Risk::Destructive,
            changes: vec![
                (ChangeKind::DropColumn, Risk::Destructive),
                (ChangeKind::TightenNullability, Risk::Caution),
            ],
        });

        // Default policy blocks only Destructive -> the drop is refused.
        assert!(ensure_allowed(&plan, &RiskPolicy::default()).is_err());

        // L3 (item) allow for DropColumn, but L3 block for TightenNullability.
        let mut policy = RiskPolicy::default();
        policy.items.insert(ChangeKind::DropColumn, RiskAction::Allow);
        policy.items.insert(ChangeKind::TightenNullability, RiskAction::Block);
        assert!(ensure_allowed(&plan, &policy).is_err());

        // L2 (level) allow for the whole Caution level -> now everything passes.
        let mut policy = RiskPolicy::default();
        policy.items.insert(ChangeKind::DropColumn, RiskAction::Allow);
        policy.levels.insert(Risk::Caution, RiskAction::Allow);
        assert!(ensure_allowed(&plan, &policy).is_ok());

        // L1 (global) block still applies to the Caution item with no L3 rule.
        let mut policy = RiskPolicy {
            global: RiskAction::Block,
            ..RiskPolicy::default()
        };
        policy.items.insert(ChangeKind::DropColumn, RiskAction::Allow);
        assert!(ensure_allowed(&plan, &policy).is_err());
    }
}
