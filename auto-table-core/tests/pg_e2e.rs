//! End-to-end tests for PostgreSQL migrations
//!
//! They need a PostgreSQL server. The connection string defaults to a local
//! server that needs no password; override it with `AUTO_TABLE_TEST_POSTGRES_URL`
//! when that does not apply, so credentials stay out of the repository:
//!
//! ```sh
//! createdb auto-table-test
//! export AUTO_TABLE_TEST_POSTGRES_URL='postgres://postgres:secret@localhost/auto-table-test'
//! cargo test -p auto-table-core --features postgres --test pg_e2e
//! ```

#![cfg(feature = "postgres")]

use std::sync::OnceLock;

use auto_table_core::{
    apply_migrations, create_missing_tables, migrate, plan_migrations, MigrateOptions,
    MigrationOutcome, MigrationPlan, TableError,
};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, TransactionTrait};

/// Must match the key the library uses (see ADVISORY_LOCK_KEY in migrate.rs)
const ADVISORY_LOCK_KEY: i64 = 0x6175_746f_7462_6c65;
use tokio::sync::Mutex;

/// Used when `AUTO_TABLE_TEST_POSTGRES_URL` is not set
///
/// Deliberately holds no password: anything machine specific belongs in the
/// environment, not in the repository.
const DEFAULT_DATABASE_URL: &str = "postgres://postgres@localhost/auto-table-test";

fn database_url() -> String {
    std::env::var("AUTO_TABLE_TEST_POSTGRES_URL")
        .unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
}

/// Cargo runs the tests of one binary concurrently, so access to the shared
/// database is serialized to keep the scenarios from stepping on each other.
static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL.get_or_init(|| Mutex::new(())).lock().await
}

async fn connect() -> DatabaseConnection {
    let url = database_url();
    Database::connect(&url)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "could not connect to the test database: {error}. \
                 Set AUTO_TABLE_TEST_POSTGRES_URL to point at a PostgreSQL instance, \
                 e.g. postgres://postgres:secret@localhost/auto-table-test"
            )
        })
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

// ---------------------------------------------------------------------------
// Entities. Each owns a table so the scenarios stay independent.
// ---------------------------------------------------------------------------

/// Every column type at once
mod baseline {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_pg_baseline")]
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
    #[sea_orm(table_name = "e2e_pg_add_column")]
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
    #[sea_orm(table_name = "e2e_pg_drop_column")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        pub email: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `score` widens from `integer` to `bigint`
mod widen {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_pg_widen")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        pub score: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `name` becomes required
mod not_null {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_pg_not_null")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        pub name: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `role` gains a default value
mod default_value {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_pg_default")]
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

/// `email` gains a unique constraint
mod add_index {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_pg_add_index")]
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
    let _guard = serial().await;
    let db = connect().await;

    db.execute_unprepared("DROP TABLE IF EXISTS \"e2e_pg_baseline\"")
        .await
        .expect("drop the table");
    create_missing_tables(&db).await.expect("create the tables");

    let plan = plan_migrations(&db).await.expect("plan the migration");

    // This is what proves the type normalization: PostgreSQL reports `varchar`
    // as character varying, stores `decimal` as numeric and spells `bool` as
    // boolean, and none of that may come back as a difference.
    assert!(
        statements_for(&plan, "e2e_pg_baseline").is_empty(),
        "a table the library just created must not change, got: {:?}",
        statements_for(&plan, "e2e_pg_baseline")
    );
}

#[tokio::test]
async fn a_table_that_does_not_exist_is_not_migrated() {
    let _guard = serial().await;
    let db = connect().await;

    db.execute_unprepared("DROP TABLE IF EXISTS \"e2e_pg_baseline\"")
        .await
        .expect("drop the table");

    let plan = plan_migrations(&db).await.expect("plan the migration");

    assert!(
        statements_for(&plan, "e2e_pg_baseline").is_empty(),
        "a missing table must be left to table creation"
    );
}

#[tokio::test]
async fn adds_a_column_missing_from_the_table() {
    let _guard = serial().await;
    let db = connect().await;

    create_legacy_table(
        &db,
        "e2e_pg_add_column",
        "\"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL PRIMARY KEY, \"email\" varchar NOT NULL",
    )
    .await;

    check_and_apply(
        &db,
        "e2e_pg_add_column",
        &["ALTER TABLE \"e2e_pg_add_column\" ADD COLUMN \"bio\" varchar"],
    )
    .await;
}

#[tokio::test]
async fn drops_a_column_the_entity_no_longer_declares() {
    let _guard = serial().await;
    let db = connect().await;

    create_legacy_table(
        &db,
        "e2e_pg_drop_column",
        "\"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL PRIMARY KEY, \"email\" varchar NOT NULL, \"obsolete\" integer",
    )
    .await;

    check_and_apply(
        &db,
        "e2e_pg_drop_column",
        &["ALTER TABLE \"e2e_pg_drop_column\" DROP COLUMN \"obsolete\""],
    )
    .await;
}

#[tokio::test]
async fn widens_a_column_type() {
    let _guard = serial().await;
    let db = connect().await;

    create_legacy_table(
        &db,
        "e2e_pg_widen",
        "\"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL PRIMARY KEY, \"score\" integer NOT NULL",
    )
    .await;

    check_and_apply(
        &db,
        "e2e_pg_widen",
        &["ALTER TABLE \"e2e_pg_widen\" ALTER COLUMN \"score\" TYPE bigint"],
    )
    .await;
}

#[tokio::test]
async fn makes_a_nullable_column_required() {
    let _guard = serial().await;
    let db = connect().await;

    create_legacy_table(
        &db,
        "e2e_pg_not_null",
        "\"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL PRIMARY KEY, \"name\" varchar",
    )
    .await;

    check_and_apply(
        &db,
        "e2e_pg_not_null",
        &["ALTER TABLE \"e2e_pg_not_null\" ALTER COLUMN \"name\" SET NOT NULL"],
    )
    .await;
}

#[tokio::test]
async fn adds_a_default_value() {
    let _guard = serial().await;
    let db = connect().await;

    create_legacy_table(
        &db,
        "e2e_pg_default",
        "\"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL PRIMARY KEY, \"role\" varchar NOT NULL",
    )
    .await;

    check_and_apply(
        &db,
        "e2e_pg_default",
        &["ALTER TABLE \"e2e_pg_default\" ALTER COLUMN \"role\" SET DEFAULT 'member'"],
    )
    .await;
}

#[tokio::test]
async fn adds_a_missing_unique_constraint() {
    let _guard = serial().await;
    let db = connect().await;

    create_legacy_table(
        &db,
        "e2e_pg_add_index",
        "\"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL PRIMARY KEY, \"email\" varchar NOT NULL",
    )
    .await;

    // PostgreSQL names a unique constraint <table>_<column>_key
    check_and_apply(
        &db,
        "e2e_pg_add_index",
        &["ALTER TABLE \"e2e_pg_add_index\" ADD CONSTRAINT \"e2e_pg_add_index_email_key\" UNIQUE (\"email\")"],
    )
    .await;
}

/// PostgreSQL's advisory lock keeps a second instance out
#[tokio::test]
async fn a_second_instance_skips_while_the_lock_is_held() {
    let _guard = serial().await;
    let db = connect().await;

    create_legacy_table(
        &db,
        "e2e_pg_add_column",
        "\"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL PRIMARY KEY, \"email\" varchar NOT NULL",
    )
    .await;
    assert!(
        !statements_for(&plan_migrations(&db).await.expect("plan"), "e2e_pg_add_column").is_empty(),
        "there should be something to migrate"
    );

    // Another instance holds the advisory lock
    let holder = db.begin().await.expect("begin the holder transaction");
    holder
        .execute_raw(sea_orm::Statement::from_string(
            DbBackend::Postgres,
            format!("SELECT pg_try_advisory_lock({ADVISORY_LOCK_KEY})"),
        ))
        .await
        .expect("the holder takes the lock");

    let outcome = migrate(&db, MigrateOptions::skip_if_locked(0))
        .await
        .expect("skipping is not an error");
    assert_eq!(
        outcome,
        MigrationOutcome::Skipped,
        "a locked database must be skipped"
    );

    let error = migrate(&db, MigrateOptions::locked(0))
        .await
        .expect_err("a locked database must not migrate");
    assert!(
        matches!(error, TableError::MigrationLockNotAcquired { .. }),
        "expected a lock error, got {error:?}"
    );

    holder
        .execute_raw(sea_orm::Statement::from_string(
            DbBackend::Postgres,
            format!("SELECT pg_advisory_unlock({ADVISORY_LOCK_KEY})"),
        ))
        .await
        .expect("the holder releases the lock");
    holder.rollback().await.expect("end the holder transaction");

    let outcome = migrate(&db, MigrateOptions::locked(0))
        .await
        .expect("migrate once the lock is free");
    assert_eq!(outcome, MigrationOutcome::Applied);

    assert!(
        statements_for(&plan_migrations(&db).await.expect("plan"), "e2e_pg_add_column").is_empty(),
        "the migration must have been applied"
    );
}

#[tokio::test]
async fn a_fully_migrated_database_plans_nothing() {
    let _guard = serial().await;
    let db = connect().await;

    create_missing_tables(&db).await.expect("create the missing tables");
    for _ in 0..5 {
        let plan = plan_migrations(&db).await.expect("plan the migration");
        if plan.is_empty() {
            break;
        }
        apply_migrations(&db, &plan).await.expect("apply the plan");
    }

    let plan = plan_migrations(&db).await.expect("plan the migration");
    assert!(
        plan.is_empty(),
        "a fully migrated database must plan nothing, got: {plan:?}"
    );
}


// --- enriched scenarios: every migration kind against a real server ---

/// The table no longer wants the plain index on `bio`
mod drop_plain_index {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_pg_drop_plain_index")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        pub email: String,
        pub bio: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `email` loses its unique constraint
mod drop_unique {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_pg_drop_unique")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        pub email: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `name` becomes nullable
mod make_nullable {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_pg_make_nullable")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        pub name: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `role` loses its default value
mod drop_default {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_pg_drop_default")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        pub role: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `id` gains an identity
mod identity {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_pg_identity")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        pub email: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

#[tokio::test]
async fn drops_a_plain_index() {
    let _guard = serial().await;
    let db = connect().await;

    db.execute_unprepared("DROP TABLE IF EXISTS \"e2e_pg_drop_plain_index\"")
        .await
        .expect("drop the table");
    db.execute_unprepared(
        "CREATE TABLE \"e2e_pg_drop_plain_index\" (\"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL PRIMARY KEY, \"email\" varchar NOT NULL, \"bio\" varchar NOT NULL)",
    )
    .await
    .expect("create the legacy table");
    // PostgreSQL has no inline `INDEX` clause in CREATE TABLE
    db.execute_unprepared("CREATE INDEX \"bio\" ON \"e2e_pg_drop_plain_index\" (\"bio\")")
        .await
        .expect("create the legacy index");

    check_and_apply(
        &db,
        "e2e_pg_drop_plain_index",
        &["DROP INDEX \"bio\""],
    )
    .await;
}

#[tokio::test]
async fn drops_a_unique_constraint() {
    let _guard = serial().await;
    let db = connect().await;

    db.execute_unprepared("DROP TABLE IF EXISTS \"e2e_pg_drop_unique\"")
        .await
        .expect("drop the table");
    db.execute_unprepared(
        "CREATE TABLE \"e2e_pg_drop_unique\" (\"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL PRIMARY KEY, \"email\" varchar NOT NULL, CONSTRAINT \"e2e_pg_drop_unique_email_key\" UNIQUE (\"email\"))",
    )
    .await
    .expect("create the legacy table");

    check_and_apply(
        &db,
        "e2e_pg_drop_unique",
        &["ALTER TABLE \"e2e_pg_drop_unique\" DROP CONSTRAINT \"e2e_pg_drop_unique_email_key\""],
    )
    .await;
}

#[tokio::test]
async fn makes_a_required_column_nullable() {
    let _guard = serial().await;
    let db = connect().await;

    create_legacy_table(
        &db,
        "e2e_pg_make_nullable",
        "\"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL PRIMARY KEY, \"name\" varchar NOT NULL",
    )
    .await;

    check_and_apply(
        &db,
        "e2e_pg_make_nullable",
        &["ALTER TABLE \"e2e_pg_make_nullable\" ALTER COLUMN \"name\" DROP NOT NULL"],
    )
    .await;
}

#[tokio::test]
async fn drops_a_default_value() {
    let _guard = serial().await;
    let db = connect().await;

    create_legacy_table(
        &db,
        "e2e_pg_drop_default",
        "\"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL PRIMARY KEY, \"role\" varchar NOT NULL DEFAULT 'member'",
    )
    .await;

    check_and_apply(
        &db,
        "e2e_pg_drop_default",
        &["ALTER TABLE \"e2e_pg_drop_default\" ALTER COLUMN \"role\" DROP DEFAULT"],
    )
    .await;
}

#[tokio::test]
async fn adds_an_identity_to_a_primary_key() {
    let _guard = serial().await;
    let db = connect().await;

    create_legacy_table(
        &db,
        "e2e_pg_identity",
        "\"id\" integer NOT NULL PRIMARY KEY, \"email\" varchar NOT NULL",
    )
    .await;

    check_and_apply(
        &db,
        "e2e_pg_identity",
        &["ALTER TABLE \"e2e_pg_identity\" ALTER COLUMN \"id\" ADD GENERATED BY DEFAULT AS IDENTITY"],
    )
    .await;
}
