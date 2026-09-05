//! Demonstrates every attribute macro provided by auto-table.
//!
//! The rest of the public API (the functions and types you call directly, such as
//! `create_missing_tables`, `migrate`, `plan_migrations`, `ensure_schema`,
//! `parse_create_table`, `diff_table`, `RiskPolicy`, …) is covered in
//! `api_demo.rs`.
//!
//! - `#[auto_table]`            — registers an entity's CREATE TABLE with the global inventory.
//! - `#[auto_create(db)]`       — injects `create_missing_tables` right after `let db = ...`.
//! - `#[auto_migrate(db)]`      — injects `migrate` with default `MigrateOptions`.
//! - `#[auto_migrate(db, opts)]`— injects `migrate` with a custom `MigrateOptions`.
//!
//! Each demo opens its own `sqlite::memory:` database, so the macros run
//! independently. For a create-then-migrate flow on one schema, call
//! `auto_table_core::ensure_schema` (which does both), or combine `#[auto_create]`
//! with a manual `migrate` call on the same `db` (see `create_then_migrate`).
//!
//! ```sh
//! cargo run --example macros_demo -p auto-table-core --features sqlite
//! ```

use auto_table_core::{
    auto_create, auto_migrate, MigrateOptions, Risk, RiskAction, RiskPolicy, TableError,
};
use std::collections::HashMap;

// 1) `#[auto_table]` registers each entity's CREATE TABLE into the global
//    inventory. `auto_create` / `auto_migrate` later iterate over everything
//    collected here.
mod users {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "demo_users")]
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

// A second entity — proves the inventory collects more than one table.
mod posts {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "demo_posts")]
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
    // 2) `#[auto_create(db)]` injects table creation after `let db = ...`.
    create_tables("sqlite::memory:").await?;

    // 3) `#[auto_migrate(db)]` — default options (no lock, destructive refused).
    migrate_default("sqlite::memory:").await?;

    // 4) `#[auto_migrate(db, options)]` — lock + custom risk policy. The lock
    //    needs a real database (a file-backed SQLite here; `:memory:` exposes a
    //    single private connection and would deadlock), so point it at a temp file.
    let file_db = {
        let path = std::env::temp_dir().join("auto_table_macros_demo.db");
        format!("sqlite://{}?mode=rwc", path.display())
    };
    migrate_with_options(&file_db).await?;
    let _ = std::fs::remove_file(std::env::temp_dir().join("auto_table_macros_demo.db"));

    // 5) Combined create + migrate on the SAME connection.
    create_then_migrate("sqlite::memory:").await?;

    Ok(())
}

// `#[auto_create(db)]` locates `let db = ...` and injects the create call right
// after it, binding the `TableCreationReport` to `__auto_table_report`.
#[auto_create(db)]
async fn create_tables(database_url: &str) -> Result<(), TableError> {
    let db = sea_orm::Database::connect(database_url).await?;
    println!(
        "[auto_create] created: {:?}, existing: {:?}",
        __auto_table_report.created_tables, __auto_table_report.existing_tables
    );
    Ok(())
}

// `#[auto_migrate(db)]` injects `migrate(&db, MigrateOptions::default())`,
// binding the `MigrationOutcome` to `__auto_table_migration`.
#[auto_migrate(db)]
async fn migrate_default(database_url: &str) -> Result<(), TableError> {
    let db = sea_orm::Database::connect(database_url).await?;
    println!(
        "[auto_migrate] default outcome: {:?}",
        __auto_table_migration
    );
    Ok(())
}

// `#[auto_migrate(db, options)]`: the second argument is any expression of type
// `MigrateOptions`. Here we take a 10s lock (`locked`) and supply a custom
// three-layer `RiskPolicy`, both written inline so no variable is in scope at the
// injection point (which sits right after `let db = ...`). This runs against a
// *file-backed* SQLite: the lock needs a connection pool with more than one
// connection sharing the same database, which `sqlite::memory:` does not provide.
// (For multi-instance production, use a file SQLite, MySQL or PostgreSQL.)
#[auto_migrate(
    db,
    MigrateOptions::locked(10).with_risk_policy(RiskPolicy {
        global: RiskAction::Allow,
        levels: {
            let mut m = HashMap::new();
            m.insert(Risk::Destructive, RiskAction::Block);
            m
        },
        items: HashMap::new(),
    })
)]
async fn migrate_with_options(database_url: &str) -> Result<(), TableError> {
    // SQLite's pool defaults to a single connection, but `apply_under_lock`
    // re-plans after `begin()` and needs a second one. Bump the pool so the lock
    // path works against a file-backed SQLite.
    let mut opts = sea_orm::ConnectOptions::new(database_url.to_string());
    opts.max_connections(2);
    let db = sea_orm::Database::connect(opts).await?;
    println!(
        "[auto_migrate + locked(file)] outcome: {:?}",
        __auto_table_migration
    );
    Ok(())
}

// Combined flow on one connection: `#[auto_create]` provisions the tables, then
// we call `migrate` manually on the same `db` so it observes the new tables.
#[auto_create(db)]
async fn create_then_migrate(database_url: &str) -> Result<(), TableError> {
    let db = sea_orm::Database::connect(database_url).await?;
    // `__auto_table_report` is bound by the injected create call above.
    let outcome = auto_table_core::migrate(&db, MigrateOptions::default()).await?;
    println!(
        "[create + migrate] report: {:?}, outcome: {:?}",
        __auto_table_report, outcome
    );
    Ok(())
}
