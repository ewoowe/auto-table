//! Demonstrates the **full programmatic API** of `auto-table-core`.
//!
//! The attribute macros (`#[auto_table]`, `#[auto_create]`, `#[auto_migrate]`)
//! are covered separately in `macros_demo.rs`. This file shows everything you
//! can do by calling the public functions and types directly — useful when you
//! want to embed auto-table into your own startup routine, build a dry-run, or
//! inspect/plan a migration before running it.
//!
//! Run with:
//!
//! ```sh
//! cargo run --example api_demo -p auto-table-core --features sqlite
//! ```
//!
//! The lock-based flows (`locked` / `skip_if_locked`) point at a temp file:
//! `sqlite::memory:` exposes a single private connection and would deadlock
//! (the lock path re-plans after `begin()` and needs a second connection). Use
//! a file SQLite, MySQL or PostgreSQL for real multi-instance deployments.

use auto_table_core::{
    apply_migrations, apply_migrations_with, backend_for, classify,
    create_missing_tables, diff_table, ensure_schema, get_all_table_statements, get_existing_tables,
    get_table_name, get_table_schema, migrate, parse::parse_create_table, plan_migrations,
    plan_table_statements, risk::classify_changes, AnyBackend, ChangeKind, ColumnAspect,
    ColumnChange, IndexChange, LockBehavior, MigrationOutcome, MigrationPlan, MigrateOptions, Risk,
    RiskAction, RiskPolicy, SchemaSyncReport, TableCreationReport, TableError, TableSchema,
};
use auto_table_core::parse::{ParseError, PRIMARY_INDEX_NAME};
use sea_orm::{ConnectOptions, Database, DbBackend};

// Entities must still be registered with `#[auto_table]` so the inventory-based
// functions (`get_all_table_statements`, `create_missing_tables`,
// `plan_migrations`, `ensure_schema`) have something to act on.
mod users {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "api_users")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub name: String,
        pub age: i32,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod posts {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "api_posts")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub title: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

#[tokio::main]
async fn main() -> Result<(), TableError> {
    demo_registration();
    demo_parse_and_diff();
    demo_risk_policy();
    demo_backend_selection().await?;

    demo_create_and_inspect().await?;
    demo_migrate_flow().await?;
    demo_lock_flows().await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// 1. Registration & inspection — pure, no database needed
// ---------------------------------------------------------------------------
fn demo_registration() {
    println!("=== 1. Registered tables (get_all_table_statements) ===");

    // Build the CREATE TABLE statements of every `#[auto_table]` entity for a
    // given backend. The inventory is collected at compile time.
    let backend = DbBackend::Sqlite;
    let statements = get_all_table_statements(backend);
    for stmt in &statements {
        if let Some(name) = get_table_name(stmt) {
            println!("  registered table: {name}");
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Parse -> diff -> plan: the pure schema-comparison pipeline
// ---------------------------------------------------------------------------
fn demo_parse_and_diff() {
    println!("\n=== 2. parse_create_table / diff_table / plan_table_statements ===");

    // `parse_create_table` turns a generated CREATE TABLE (sea-query output)
    // into a normalized `TableSchema`. Here we generate it live from the entity.
    let backend = DbBackend::Sqlite;
    let create = sea_orm::Schema::new(backend).create_table_from_entity(users::Entity);
    let sql = backend.build(&create).sql;
    let expected: TableSchema = parse_create_table(&sql).expect("entity statement must parse");
    println!("  parsed `{}` with {} columns", expected.name, expected.columns.len());

    // The primary key is surfaced as an index named via `PRIMARY_INDEX_NAME`.
    assert_eq!(PRIMARY_INDEX_NAME, "PRIMARY");
    println!("  primary key index name constant = {PRIMARY_INDEX_NAME}");

    // Simulate the database lagging behind: drop the `age` column from the
    // "actual" snapshot. Comparing expected vs actual yields an Add change.
    let mut actual = expected.clone();
    actual.columns.retain(|c| c.name != "age");
    let diff = diff_table(&expected, &actual);

    println!("  diff (entity expects `age`, db lacks it):");
    for change in &diff.columns {
        match change {
            ColumnChange::Add(c) => println!("    add column {} ({})", c.name, c.col_type),
            ColumnChange::Drop { name } => println!("    drop column {name}"),
            ColumnChange::Alter { name, aspects, .. } => {
                for aspect in aspects {
                    match aspect {
                        ColumnAspect::Type { from, to } => {
                            println!("    change {name} type {from} -> {to}")
                        }
                        ColumnAspect::Nullable { from, to } => {
                            println!("    change {name} nullable {from} -> {to}")
                        }
                        ColumnAspect::Default { .. } => println!("    change {name} default"),
                        ColumnAspect::AutoIncrement { .. } => {
                            println!("    change {name} auto_increment")
                        }
                    }
                }
            }
        }
    }
    for change in &diff.indexes {
        match change {
            IndexChange::Add(_) => println!("    add index"),
            IndexChange::Drop(_) => println!("    drop index"),
        }
    }

    // Turn the diff into the SQL statements a backend would run.
    let statements = plan_table_statements(&diff, backend).expect("planning must succeed");
    println!("  planned statements:");
    for s in &statements {
        println!("    {s}");
    }

    // A non-CREATE-TABLE string is rejected with a typed error.
    let err = parse_create_table("SELECT 1").unwrap_err();
    assert!(matches!(err, ParseError::NotCreateTable));
    println!("  rejecting bad input yields ParseError::{err:?}");
}

// ---------------------------------------------------------------------------
// 3. Risk classification & the three-layer RiskPolicy
// ---------------------------------------------------------------------------
fn demo_risk_policy() {
    println!("\n=== 3. classify / classify_changes / RiskPolicy ===");

    // Build a diff with a destructive change (drop a column) to exercise risk.
    let backend = DbBackend::Sqlite;
    let create = sea_orm::Schema::new(backend).create_table_from_entity(users::Entity);
    let sql = backend.build(&create).sql;
    let expected = parse_create_table(&sql).unwrap();
    let mut actual = expected.clone();
    actual.columns.retain(|c| c.name != "age");
    let diff = diff_table(&expected, &actual);

    // `classify` returns the worst risk anywhere in the diff.
    let risk: Risk = classify(&diff);
    println!("  worst risk of the diff: {risk:?}");

    // `classify_changes` enumerates every change, paired with its kind + risk.
    let changes = classify_changes(&diff);
    for (kind, level) in &changes {
        println!("    {kind} -> {level:?}");
    }

    // The default policy blocks only Destructive changes.
    let policy = RiskPolicy::default();
    assert_eq!(
        policy.resolve(ChangeKind::DropColumn, Risk::Destructive),
        RiskAction::Block
    );
    assert_eq!(
        policy.resolve(ChangeKind::AddColumn, Risk::Safe),
        RiskAction::Allow
    );

    // Three layers, increasing specificity: global (L1) < level (L2) < item (L3).
    // A more specific layer always wins.
    let mut custom = RiskPolicy {
        global: RiskAction::Block, // L1: block everything
        ..RiskPolicy::default()
    };
    custom
        .levels
        .insert(Risk::Caution, RiskAction::Allow); // L2: except the whole Caution level
    custom.items.insert(
        ChangeKind::AddNotNullColumn,
        RiskAction::Block,
    ); // L3: but keep this specific item blocked

    assert_eq!(
        custom.resolve(ChangeKind::AddNotNullColumn, Risk::Caution),
        RiskAction::Block,
        "L3 (item) overrides L2 (level)"
    );
    assert_eq!(
        custom.resolve(ChangeKind::TightenNullability, Risk::Caution),
        RiskAction::Allow,
        "L2 (level) overrides L1 (global)"
    );
    assert_eq!(
        custom.resolve(ChangeKind::AddColumn, Risk::Safe),
        RiskAction::Block,
        "L1 (global) is the baseline"
    );
    println!("  three-layer precedence (global < level < item) verified");
}

// ---------------------------------------------------------------------------
// 4. Backend selection
// ---------------------------------------------------------------------------
async fn demo_backend_selection() -> Result<(), TableError> {
    println!("\n=== 4. backend_for / AnyBackend ===");

    // Pick the backend implementation for a `DbBackend`...
    let any: AnyBackend = backend_for(DbBackend::Sqlite)?;
    println!("  backend_for(Sqlite) -> {any:?}");

    // ...or build it directly, or from a live connection.
    let db = Database::connect("sqlite::memory:").await?;
    let _from_conn = AnyBackend::for_connection(&db)?;
    let _from_kind = AnyBackend::for_backend(DbBackend::Sqlite)?;
    println!("  AnyBackend::for_connection / for_backend also resolve to Sqlite");
    Ok(())
}

// ---------------------------------------------------------------------------
// 5. Create missing tables + inspect what exists
// ---------------------------------------------------------------------------
async fn demo_create_and_inspect() -> Result<(), TableError> {
    println!("\n=== 5. create_missing_tables / get_existing_tables ===");

    let db = Database::connect("sqlite::memory:").await?;

    // Creates every registered table that does not yet exist. No logging is done
    // by the library; the report tells you what happened.
    let report: TableCreationReport = create_missing_tables(&db).await?;
    println!("  created: {:?}", report.created_tables);
    println!("  existing (before): {:?}", report.existing_tables);

    // Query the live list of tables directly.
    let backend = db.get_database_backend();
    let existing = get_existing_tables(&db, backend).await?;
    println!("  get_existing_tables -> {existing:?}");

    // Read back one table's structure.
    let schema = get_table_schema(&db, "api_users").await?;
    println!(
        "  get_table_schema(api_users) -> {} columns, {} indexes",
        schema.columns.len(),
        schema.indexes.len()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 6. The full migration flow (plan, apply, migrate, ensure_schema)
// ---------------------------------------------------------------------------
async fn demo_migrate_flow() -> Result<(), TableError> {
    println!("\n=== 6. plan_migrations / apply_migrations / migrate / ensure_schema ===");

    let db = Database::connect("sqlite::memory:").await?;

    // Bring the schema up to date: create missing tables, then migrate.
    let sync: SchemaSyncReport = ensure_schema(&db, MigrateOptions::default()).await?;
    println!(
        "  ensure_schema: created={:?}, existing={:?}, migration={:?}",
        sync.created_tables, sync.existing_tables, sync.migration
    );

    // `plan_migrations` compares every existing registered table against the
    // entity and builds a plan. New tables are skipped (that's create's job).
    let plan: MigrationPlan = plan_migrations(&db).await?;
    println!(
        "  plan_migrations: is_empty={}, risk={:?}, {} statements",
        plan.is_empty(),
        plan.risk(),
        plan.statements().len()
    );

    // Apply an (empty) plan; or opt into destructive changes via the builder.
    let plan = plan.allow_destructive();
    apply_migrations(&db, &plan).await?;
    println!("  apply_migrations(allow_destructive) -> ok");

    // `apply_migrations_with` lets you pass `MigrateOptions` (lock + policy).
    apply_migrations_with(&db, &MigrationPlan::default(), MigrateOptions::default()).await?;

    // `migrate` plans and applies in one call. Variants:
    let _outcome: MigrationOutcome = migrate(&db, MigrateOptions::default()).await?;
    println!("  migrate(default) -> {:?}", _outcome);

    // Allow destructive changes through options.
    let outcome = migrate(&db, MigrateOptions::default().allow_destructive()).await?;
    println!("  migrate(allow_destructive) -> {outcome:?}");

    // Or supply a fully custom three-layer policy.
    let mut policy = RiskPolicy::default();
    policy
        .levels
        .insert(Risk::Destructive, RiskAction::Allow);
    let outcome = migrate(
        &db,
        MigrateOptions::default().with_risk_policy(policy),
    )
    .await?;
    println!("  migrate(with_risk_policy) -> {outcome:?}");
    Ok(())
}

// ---------------------------------------------------------------------------
// 7. Lock-based flows for multi-instance deployments (file-backed SQLite)
// ---------------------------------------------------------------------------
async fn demo_lock_flows() -> Result<(), TableError> {
    println!("\n=== 7. locked / skip_if_locked (file SQLite) ===");

    // `sqlite::memory:` has a single private connection and would deadlock; use
    // a temp file with a pool of at least two connections.
    let path = std::env::temp_dir().join("auto_table_api_demo.db");
    let url = format!("sqlite://{}?mode=rwc", path.display());

    let mut opts = ConnectOptions::new(url);
    opts.max_connections(2);
    let db = Database::connect(opts).await?;

    let report = ensure_schema(&db, MigrateOptions::default()).await?;
    println!(
        "  ensure_schema (file) -> created={:?}, migration={:?}",
        report.created_tables, report.migration
    );

    // Take the lock, waiting up to 10s. `LockBehavior::Required` is recorded.
    let locked = MigrateOptions::locked(10);
    assert_eq!(locked.lock, LockBehavior::Required);
    assert_eq!(locked.lock_timeout_secs, 10);
    let outcome = migrate(&db, locked).await?;
    println!("  migrate(locked(10)) -> {outcome:?}");

    // Take the lock, but skip (instead of fail) if someone else holds it.
    let skipped = migrate(&db, MigrateOptions::skip_if_locked(10)).await?;
    println!("  migrate(skip_if_locked(10)) -> {skipped:?}");

    let _ = std::fs::remove_file(path);
    Ok(())
}
