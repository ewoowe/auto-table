//! End-to-end tests for SQLite migrations
//!
//! SQLite ships with a bundled copy of the engine, so these need no server:
//! every test runs against its own temporary database file.
//!
//! ```sh
//! cargo test -p auto-table-core --features sqlite --test sqlite_e2e
//! ```

#![cfg(feature = "sqlite")]

use std::path::PathBuf;

use auto_table_core::{apply_migrations, create_missing_tables, plan_migrations, MigrationPlan};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection};

/// A database in its own temporary directory, removed when the test finishes
struct TempDb {
    dir: PathBuf,
    db: DatabaseConnection,
}

impl TempDb {
    async fn new(test_name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "auto-table-sqlite-{}-{test_name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the temporary directory");

        let file = dir.join("test.db");
        let url = format!("sqlite://{}?mode=rwc", file.display());
        let db = Database::connect(&url)
            .await
            .unwrap_or_else(|error| panic!("connect to `{url}` failed: {error}"));

        Self { dir, db }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn statements_for<'a>(plan: &'a MigrationPlan, table: &str) -> Vec<&'a str> {
    plan.tables
        .iter()
        .find(|migration| migration.table == table)
        .map(|migration| migration.statements.iter().map(String::as_str).collect())
        .unwrap_or_default()
}

/// Recreates a table with a legacy definition
async fn create_legacy_table(db: &DatabaseConnection, table: &str, definition: &str) {
    db.execute_unprepared(&format!("DROP TABLE IF EXISTS \"{table}\""))
        .await
        .expect("drop the table");
    db.execute_unprepared(&format!("CREATE TABLE \"{table}\" ({definition})"))
        .await
        .expect("create the legacy table");
}

/// Plans, checks the statements, applies them, then checks the plan is empty
async fn check_and_apply(db: &DatabaseConnection, table: &str, expected: &[&str]) {
    let plan = plan_migrations(db).await.expect("plan the migration");
    let statements = statements_for(&plan, table);
    assert_eq!(statements, expected, "unexpected statements for `{table}`");

    apply_migrations(db, &plan).await.expect("apply the plan");

    let plan = plan_migrations(db).await.expect("plan again after applying");
    assert!(
        statements_for(&plan, table).is_empty(),
        "`{table}` is not idempotent, it still plans: {:?}",
        statements_for(&plan, table)
    );
}

async fn scalar(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(sea_orm::Statement::from_string(
        db.get_database_backend(),
        sql.to_string(),
    ))
    .await
    .expect("run the query")
    .expect("a row")
    .try_get_by_index::<i64>(0)
    .expect("an integer")
}

// ---------------------------------------------------------------------------
// Entities. Each owns a table so the scenarios stay independent.
// ---------------------------------------------------------------------------

/// Every column type at once
mod baseline {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_sqlite_baseline")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        #[sea_orm(unique)]
        pub email: String,
        pub nickname: Option<String>,
        pub age: i32,
        pub score: i64,
        pub balance: Decimal,
        pub active: bool,
        pub created_at: DateTimeWithTimeZone,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// The table is missing `bio`
mod add_column {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_sqlite_add_column")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        pub email: String,
        pub bio: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// The table still has `obsolete`
mod drop_column {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_sqlite_drop_column")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        pub email: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `score` is declared as `i64` while the table has an integer column
mod widen {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_sqlite_widen")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        pub score: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `name` becomes required, which SQLite can only do by rebuilding
mod not_null {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_sqlite_not_null")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        pub name: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `role` gains a default value, another rebuild
mod default_value {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_sqlite_default")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        #[sea_orm(default_value = "member")]
        pub role: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `email` gains a unique index
mod add_index {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_sqlite_add_index")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        #[sea_orm(unique)]
        pub email: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_table_created_by_the_library_is_already_in_sync() {
    let db = TempDb::new("baseline").await;

    create_missing_tables(&db.db).await.expect("create the tables");

    let plan = plan_migrations(&db.db).await.expect("plan the migration");

    // SQLite stores types loosely: i32 and i64 both become INTEGER affinity,
    // `Decimal` becomes REAL, and none of that may look like a difference.
    assert!(
        statements_for(&plan, "e2e_sqlite_baseline").is_empty(),
        "a table the library just created must not change, got: {:?}",
        statements_for(&plan, "e2e_sqlite_baseline")
    );
}

#[tokio::test]
async fn adds_a_column_without_rebuilding() {
    let db = TempDb::new("add_column").await;

    create_legacy_table(
        &db.db,
        "e2e_sqlite_add_column",
        "\"id\" integer NOT NULL PRIMARY KEY AUTOINCREMENT, \"email\" varchar NOT NULL",
    )
    .await;

    // The type is reported as its affinity, which is what SQLite itself stores
    check_and_apply(
        &db.db,
        "e2e_sqlite_add_column",
        &["ALTER TABLE \"e2e_sqlite_add_column\" ADD COLUMN \"bio\" TEXT"],
    )
    .await;
}

#[tokio::test]
async fn drops_a_column_without_rebuilding() {
    let db = TempDb::new("drop_column").await;

    create_legacy_table(
        &db.db,
        "e2e_sqlite_drop_column",
        "\"id\" integer NOT NULL PRIMARY KEY AUTOINCREMENT, \"email\" varchar NOT NULL, \"obsolete\" integer",
    )
    .await;

    check_and_apply(
        &db.db,
        "e2e_sqlite_drop_column",
        &["ALTER TABLE \"e2e_sqlite_drop_column\" DROP COLUMN \"obsolete\""],
    )
    .await;
}

#[tokio::test]
async fn widening_i32_to_i64_is_not_a_change() {
    let db = TempDb::new("widen").await;

    // The column was created from an i32, the entity now declares i64. Both are
    // INTEGER affinity in SQLite, so there is nothing to rebuild.
    create_legacy_table(
        &db.db,
        "e2e_sqlite_widen",
        "\"id\" integer NOT NULL PRIMARY KEY AUTOINCREMENT, \"score\" integer NOT NULL",
    )
    .await;

    let plan = plan_migrations(&db.db).await.expect("plan the migration");

    assert!(
        statements_for(&plan, "e2e_sqlite_widen").is_empty(),
        "equal type affinities must not trigger a rebuild, got: {:?}",
        statements_for(&plan, "e2e_sqlite_widen")
    );
}

#[tokio::test]
async fn rebuilding_keeps_the_rows() {
    let db = TempDb::new("rebuild_rows").await;

    create_legacy_table(
        &db.db,
        "e2e_sqlite_not_null",
        "\"id\" integer NOT NULL PRIMARY KEY AUTOINCREMENT, \"name\" varchar",
    )
    .await;
    db.db
        .execute_unprepared(
            "INSERT INTO \"e2e_sqlite_not_null\" (\"name\") VALUES ('alice'), ('bob'), ('carol')",
        )
        .await
        .expect("insert rows");

    // Requiring `name` forces a rebuild, which must not lose the rows
    let plan = plan_migrations(&db.db).await.expect("plan the migration");
    assert!(
        !statements_for(&plan, "e2e_sqlite_not_null").is_empty(),
        "making a column NOT NULL should require a migration"
    );
    apply_migrations(&db.db, &plan).await.expect("apply the plan");

    assert_eq!(
        scalar(&db.db, "SELECT COUNT(*) FROM \"e2e_sqlite_not_null\"").await,
        3,
        "every row must survive the rebuild"
    );
    assert_eq!(
        scalar(
            &db.db,
            "SELECT COUNT(*) FROM \"e2e_sqlite_not_null\" WHERE \"name\" = 'alice'"
        )
        .await,
        1,
        "the values themselves must survive"
    );

    // And the rebuild must have reached the intended structure
    let plan = plan_migrations(&db.db).await.expect("plan again");
    assert!(
        statements_for(&plan, "e2e_sqlite_not_null").is_empty(),
        "the rebuilt table must match the entity, still plans: {:?}",
        statements_for(&plan, "e2e_sqlite_not_null")
    );
}

#[tokio::test]
async fn adds_a_default_value_by_rebuilding() {
    let db = TempDb::new("default").await;

    create_legacy_table(
        &db.db,
        "e2e_sqlite_default",
        "\"id\" integer NOT NULL PRIMARY KEY AUTOINCREMENT, \"role\" varchar NOT NULL",
    )
    .await;

    let plan = plan_migrations(&db.db).await.expect("plan the migration");
    let statements = statements_for(&plan, "e2e_sqlite_default");
    assert!(
        statements.iter().any(|sql| sql.contains("__auto_table_rebuild")),
        "changing a default must rebuild the table, got: {statements:?}"
    );

    apply_migrations(&db.db, &plan).await.expect("apply the plan");

    let plan = plan_migrations(&db.db).await.expect("plan again");
    assert!(
        statements_for(&plan, "e2e_sqlite_default").is_empty(),
        "the rebuilt table must match the entity"
    );
}

#[tokio::test]
async fn adds_a_missing_unique_index_by_rebuilding() {
    let db = TempDb::new("add_index").await;

    create_legacy_table(
        &db.db,
        "e2e_sqlite_add_index",
        "\"id\" integer NOT NULL PRIMARY KEY AUTOINCREMENT, \"email\" varchar NOT NULL",
    )
    .await;

    // A unique constraint belongs to the table definition on SQLite, so it can
    // only be added by rebuilding
    let plan = plan_migrations(&db.db).await.expect("plan the migration");
    assert!(
        statements_for(&plan, "e2e_sqlite_add_index")
            .iter()
            .any(|sql| sql.contains("__auto_table_rebuild")),
        "a new unique constraint must rebuild the table"
    );

    apply_migrations(&db.db, &plan).await.expect("apply the plan");

    let plan = plan_migrations(&db.db).await.expect("plan again");
    assert!(
        statements_for(&plan, "e2e_sqlite_add_index").is_empty(),
        "the rebuilt table must match the entity"
    );

    // The constraint really is in place afterwards
    let duplicate = db
        .db
        .execute_unprepared("INSERT INTO \"e2e_sqlite_add_index\" (\"email\") VALUES ('a@x.com'), ('a@x.com')")
        .await;
    assert!(duplicate.is_err(), "the unique constraint must be enforced");
}

#[tokio::test]
async fn a_fully_migrated_database_plans_nothing() {
    let db = TempDb::new("converge").await;

    create_missing_tables(&db.db).await.expect("create the missing tables");
    for _ in 0..5 {
        let plan = plan_migrations(&db.db).await.expect("plan the migration");
        if plan.is_empty() {
            break;
        }
        apply_migrations(&db.db, &plan).await.expect("apply the plan");
    }

    let plan = plan_migrations(&db.db).await.expect("plan the migration");
    assert!(
        plan.is_empty(),
        "a fully migrated database must plan nothing, got: {plan:?}"
    );
}


// --- enriched scenarios: every migration kind against a real (bundled) engine ---

/// `role` is added with a default value
mod not_null_default {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_sqlite_notnull_default")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        pub email: String,
        #[sea_orm(default_value = "member")]
        pub role: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// The table no longer wants the index on `bio`
mod drop_index {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_sqlite_drop_index")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        pub email: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `role` gains a default value
mod change_default {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_sqlite_change_default")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        pub email: String,
        #[sea_orm(default_value = "member")]
        pub role: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

#[tokio::test]
async fn adds_a_not_null_column_with_a_default() {
    let db = TempDb::new("notnull_default").await;

    create_legacy_table(
        &db.db,
        "e2e_sqlite_notnull_default",
        "\"id\" integer NOT NULL PRIMARY KEY AUTOINCREMENT, \"email\" varchar NOT NULL",
    )
    .await;

    check_and_apply(
        &db.db,
        "e2e_sqlite_notnull_default",
        &["ALTER TABLE \"e2e_sqlite_notnull_default\" ADD COLUMN \"role\" TEXT NOT NULL DEFAULT 'member'"],
    )
    .await;
}

#[tokio::test]
async fn drops_an_index_by_rebuilding() {
    let db = TempDb::new("drop_index").await;

    create_legacy_table(
        &db.db,
        "e2e_sqlite_drop_index",
        "\"id\" integer NOT NULL PRIMARY KEY AUTOINCREMENT, \"email\" varchar NOT NULL",
    )
    .await;
    db.db
        .execute_unprepared("CREATE INDEX \"email\" ON \"e2e_sqlite_drop_index\" (\"email\")")
        .await
        .expect("create the legacy index");

    // Dropping an index rebuilds the table on SQLite
    let plan = plan_migrations(&db.db).await.expect("plan the migration");
    let statements = statements_for(&plan, "e2e_sqlite_drop_index");
    assert!(
        statements.iter().any(|sql| sql.contains("__auto_table_rebuild")),
        "dropping an index must rebuild the table, got: {statements:?}"
    );

    apply_migrations(&db.db, &plan).await.expect("apply the plan");

    let plan = plan_migrations(&db.db).await.expect("plan again");
    assert!(
        statements_for(&plan, "e2e_sqlite_drop_index").is_empty(),
        "the rebuilt table must match the entity"
    );

    // And the index really is gone
    let count = scalar(
        &db.db,
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'email'",
    )
    .await;
    assert_eq!(count, 0, "the index must have been dropped");
}

#[tokio::test]
async fn changes_a_default_value_by_rebuilding() {
    let db = TempDb::new("change_default").await;

    create_legacy_table(
        &db.db,
        "e2e_sqlite_change_default",
        "\"id\" integer NOT NULL PRIMARY KEY AUTOINCREMENT, \"email\" varchar NOT NULL, \"role\" varchar NOT NULL",
    )
    .await;

    let plan = plan_migrations(&db.db).await.expect("plan the migration");
    assert!(
        statements_for(&plan, "e2e_sqlite_change_default")
            .iter()
            .any(|sql| sql.contains("__auto_table_rebuild")),
        "changing a default must rebuild the table"
    );

    apply_migrations(&db.db, &plan).await.expect("apply the plan");

    let plan = plan_migrations(&db.db).await.expect("plan again");
    assert!(
        statements_for(&plan, "e2e_sqlite_change_default").is_empty(),
        "the rebuilt table must match the entity"
    );
}
