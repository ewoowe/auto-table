//! End-to-end tests against a real MySQL database
//!
//! Everything else in the crate is covered by unit tests that never touch a
//! database. These run the whole chain for real: entity -> generated DDL ->
//! parse -> read back from `information_schema` -> diff -> statements, and
//! finally apply them and check the result is idempotent.
//!
//! They need a MySQL instance. The connection string defaults to a local
//! server that needs no password; override it with
//! `AUTO_TABLE_TEST_DATABASE_URL` when that does not apply, so credentials
//! never have to be committed:
//!
//! ```sh
//! mysql -uroot -p -e "CREATE DATABASE IF NOT EXISTS \`auto-table-test\`"
//! export AUTO_TABLE_TEST_DATABASE_URL='mysql://user:secret@127.0.0.1/auto-table-test'
//! cargo test -p auto-table-core --test mysql_e2e
//! ```

use std::sync::OnceLock;

use auto_table_core::{
    apply_migrations, apply_migrations_with, create_missing_tables, plan_migrations,
    MigrateOptions, MigrationOutcome, MigrationPlan, TableError,
};
use sea_orm::{
    ConnectionTrait, Database, DatabaseConnection, DbBackend, TransactionTrait,
};
use tokio::sync::Mutex;

/// Connection string used when `AUTO_TABLE_TEST_DATABASE_URL` is not set
///
/// Deliberately holds no password: anything machine specific belongs in the
/// environment, not in the repository.
const DEFAULT_DATABASE_URL: &str = "mysql://root@localhost/auto-table-test";

fn database_url() -> String {
    std::env::var("AUTO_TABLE_TEST_DATABASE_URL")
        .unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
}

/// Cargo runs the tests of one binary concurrently, so access to the shared
/// database is serialized to keep the scenarios from stepping on each other.
static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL.get_or_init(|| Mutex::new(())).lock().await
}

async fn connect() -> DatabaseConnection {
    Database::connect(&database_url()).await.unwrap_or_else(|error| {
        panic!(
            "could not connect to the test database: {error}. \
             Set AUTO_TABLE_TEST_DATABASE_URL to point at a MySQL instance, \
             e.g. mysql://root:secret@127.0.0.1/auto-table-test"
        )
    })
}

/// The statements planned for one table
fn statements_for<'a>(plan: &'a MigrationPlan, table: &str) -> Vec<&'a str> {
    plan.tables
        .iter()
        .find(|migration| migration.table == table)
        .map(|migration| migration.statements.iter().map(String::as_str).collect())
        .unwrap_or_default()
}

/// Recreates a table with a legacy definition, standing in for a database that
/// is one migration behind
async fn create_legacy_table(db: &DatabaseConnection, table: &str, definition: &str) {
    db.execute_unprepared(&format!("DROP TABLE IF EXISTS `{table}`"))
        .await
        .expect("drop the table");
    db.execute_unprepared(&format!("CREATE TABLE `{table}` ({definition})"))
        .await
        .expect("create the legacy table");
}

/// Plans, checks the statements, applies them, then checks the plan is empty
///
/// The second check is the important one: after applying, planning again must
/// produce nothing, otherwise parsing and reading back the schema disagree.
async fn check_and_apply(db: &DatabaseConnection, table: &str, expected: &[&str]) {
    let plan = plan_migrations(db).await.expect("plan the migration");
    let statements = statements_for(&plan, table);
    assert_eq!(statements, expected, "unexpected statements for `{table}`");

    for sql in &statements {
        db.execute_unprepared(sql)
            .await
            .unwrap_or_else(|error| panic!("applying `{sql}` failed: {error}"));
    }

    let plan = plan_migrations(db).await.expect("plan again after applying");
    assert!(
        statements_for(&plan, table).is_empty(),
        "`{table}` is not idempotent, it still plans: {:?}",
        statements_for(&plan, table)
    );
}

// ---------------------------------------------------------------------------
// Entities under test. Each one owns a separate table so the scenarios do not
// interfere. All of them are registered through `#[auto_table]`.
// ---------------------------------------------------------------------------

/// Exercises every column type at once
mod baseline {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_baseline")]
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
    #[sea_orm(table_name = "e2e_add_column")]
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
    #[sea_orm(table_name = "e2e_drop_column")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        pub email: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `score` widens from `int` to `bigint`
mod change_type {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_change_type")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        pub score: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `name` becomes `NOT NULL`
mod nullability {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_nullability")]
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
    #[sea_orm(table_name = "e2e_default")]
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
    #[sea_orm(table_name = "e2e_add_index")]
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

/// The unique index on `email` is dropped
mod drop_index {
    use auto_table_core::auto_table;
    use sea_orm::entity::prelude::*;

    #[auto_table]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "e2e_drop_index")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
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

    db.execute_unprepared("DROP TABLE IF EXISTS `e2e_baseline`")
        .await
        .expect("drop the table");
    create_missing_tables(&db).await.expect("create the tables");

    let plan = plan_migrations(&db).await.expect("plan the migration");

    // This is the real proof that normalization is right: MySQL stores `bool`
    // as tinyint(1), `Decimal` as decimal(10,0) and `i64` as bigint(20), and
    // none of those spellings may come back as a difference.
    assert!(
        statements_for(&plan, "e2e_baseline").is_empty(),
        "a table the library just created must not change, got: {:?}",
        statements_for(&plan, "e2e_baseline")
    );
}

#[tokio::test]
async fn a_table_that_does_not_exist_is_not_migrated() {
    let _guard = serial().await;
    let db = connect().await;

    db.execute_unprepared("DROP TABLE IF EXISTS `e2e_baseline`")
        .await
        .expect("drop the table");

    let plan = plan_migrations(&db).await.expect("plan the migration");

    // Creating it is the job of create_missing_tables, not of a migration
    assert!(
        statements_for(&plan, "e2e_baseline").is_empty(),
        "a missing table must be left to table creation"
    );
}

#[tokio::test]
async fn adds_a_column_missing_from_the_table() {
    let _guard = serial().await;
    let db = connect().await;

    create_legacy_table(
        &db,
        "e2e_add_column",
        "`id` int NOT NULL AUTO_INCREMENT, `email` varchar(255) NOT NULL, PRIMARY KEY (`id`)",
    )
    .await;

    check_and_apply(
        &db,
        "e2e_add_column",
        &["ALTER TABLE `e2e_add_column` ADD COLUMN `bio` varchar(255)"],
    )
    .await;
}

#[tokio::test]
async fn drops_a_column_the_entity_no_longer_declares() {
    let _guard = serial().await;
    let db = connect().await;

    create_legacy_table(
        &db,
        "e2e_drop_column",
        "`id` int NOT NULL AUTO_INCREMENT, `email` varchar(255) NOT NULL, `obsolete` int DEFAULT NULL, PRIMARY KEY (`id`)",
    )
    .await;

    check_and_apply(
        &db,
        "e2e_drop_column",
        &["ALTER TABLE `e2e_drop_column` DROP COLUMN `obsolete`"],
    )
    .await;
}

#[tokio::test]
async fn widens_a_column_type() {
    let _guard = serial().await;
    let db = connect().await;

    create_legacy_table(
        &db,
        "e2e_change_type",
        "`id` int NOT NULL AUTO_INCREMENT, `score` int NOT NULL, PRIMARY KEY (`id`)",
    )
    .await;

    // MODIFY repeats the whole definition, because MySQL replaces it entirely
    check_and_apply(
        &db,
        "e2e_change_type",
        &["ALTER TABLE `e2e_change_type` MODIFY COLUMN `score` bigint NOT NULL"],
    )
    .await;
}

#[tokio::test]
async fn makes_a_nullable_column_required() {
    let _guard = serial().await;
    let db = connect().await;

    create_legacy_table(
        &db,
        "e2e_nullability",
        "`id` int NOT NULL AUTO_INCREMENT, `name` varchar(255) DEFAULT NULL, PRIMARY KEY (`id`)",
    )
    .await;

    check_and_apply(
        &db,
        "e2e_nullability",
        &["ALTER TABLE `e2e_nullability` MODIFY COLUMN `name` varchar(255) NOT NULL"],
    )
    .await;
}

#[tokio::test]
async fn adds_a_default_value() {
    let _guard = serial().await;
    let db = connect().await;

    create_legacy_table(
        &db,
        "e2e_default",
        "`id` int NOT NULL AUTO_INCREMENT, `role` varchar(255) NOT NULL, PRIMARY KEY (`id`)",
    )
    .await;

    check_and_apply(
        &db,
        "e2e_default",
        &["ALTER TABLE `e2e_default` MODIFY COLUMN `role` varchar(255) NOT NULL DEFAULT 'member'"],
    )
    .await;
}

#[tokio::test]
async fn adds_a_missing_unique_index() {
    let _guard = serial().await;
    let db = connect().await;

    create_legacy_table(
        &db,
        "e2e_add_index",
        "`id` int NOT NULL AUTO_INCREMENT, `email` varchar(255) NOT NULL, PRIMARY KEY (`id`)",
    )
    .await;

    check_and_apply(
        &db,
        "e2e_add_index",
        &["ALTER TABLE `e2e_add_index` ADD UNIQUE INDEX `email` (`email`)"],
    )
    .await;
}

#[tokio::test]
async fn drops_an_index_the_entity_no_longer_declares() {
    let _guard = serial().await;
    let db = connect().await;

    create_legacy_table(
        &db,
        "e2e_drop_index",
        "`id` int NOT NULL AUTO_INCREMENT, `email` varchar(255) NOT NULL, PRIMARY KEY (`id`), UNIQUE KEY `email` (`email`)",
    )
    .await;

    check_and_apply(
        &db,
        "e2e_drop_index",
        &["ALTER TABLE `e2e_drop_index` DROP INDEX `email`"],
    )
    .await;
}

#[tokio::test]
async fn apply_migrations_brings_every_registered_table_in_sync() {
    let _guard = serial().await;
    let db = connect().await;

    // Put one table behind on purpose
    create_legacy_table(
        &db,
        "e2e_add_column",
        "`id` int NOT NULL AUTO_INCREMENT, `email` varchar(255) NOT NULL, PRIMARY KEY (`id`)",
    )
    .await;

    let plan = plan_migrations(&db).await.expect("plan the migration");
    assert!(!plan.is_empty(), "there should be something to migrate");

    apply_migrations(&db, &plan).await.expect("apply the plan");

    let plan = plan_migrations(&db).await.expect("plan again");
    assert!(
        statements_for(&plan, "e2e_add_column").is_empty(),
        "`e2e_add_column` should be in sync after applying"
    );
}

/// The lock name the library uses, so the test can take it the same way
const LOCK_SQL: &str = "CONCAT('auto-table-migration-', DATABASE())";

/// Simulates another instance holding the lock while this one tries to migrate
#[tokio::test]
async fn a_second_instance_waits_while_the_lock_is_held() {
    let _guard = serial().await;
    let db = connect().await;

    // Put one table behind so there is something to migrate
    create_legacy_table(
        &db,
        "e2e_add_column",
        "`id` int NOT NULL PRIMARY KEY AUTO_INCREMENT, `email` varchar(255) NOT NULL",
    )
    .await;
    let plan = plan_migrations(&db).await.expect("plan the migration");
    assert!(
        !statements_for(&plan, "e2e_add_column").is_empty(),
        "there should be something to migrate"
    );

    // Another instance takes the lock and holds it
    let holder = db.begin().await.expect("begin the holder transaction");
    holder
        .execute_raw(sea_orm::Statement::from_string(
            DbBackend::MySql,
            format!("SELECT GET_LOCK({LOCK_SQL}, 0)"),
        ))
        .await
        .expect("the holder takes the lock");

    // Skipping is a valid answer: the other instance is applying the same plan
    let outcome = apply_migrations_with(&db, &plan, MigrateOptions::skip_if_locked(0))
        .await
        .expect("skipping is not an error");
    assert_eq!(
        outcome,
        MigrationOutcome::Skipped,
        "a locked database must be skipped rather than migrated"
    );

    // Insisting on the lock reports it instead
    let error = apply_migrations_with(&db, &plan, MigrateOptions::locked(0))
        .await
        .expect_err("a locked database must not migrate");
    assert!(
        matches!(error, TableError::MigrationLockNotAcquired { .. }),
        "expected a lock error, got {error:?}"
    );

    // Once the other instance is done, this one may proceed
    holder
        .execute_raw(sea_orm::Statement::from_string(
            DbBackend::MySql,
            format!("SELECT RELEASE_LOCK({LOCK_SQL})"),
        ))
        .await
        .expect("the holder releases the lock");
    holder.rollback().await.expect("end the holder transaction");

    let outcome = apply_migrations_with(&db, &plan, MigrateOptions::locked(0))
        .await
        .expect("migrate once the lock is free");
    assert_eq!(outcome, MigrationOutcome::Applied);

    let plan = plan_migrations(&db).await.expect("plan again");
    assert!(
        statements_for(&plan, "e2e_add_column").is_empty(),
        "the migration must have been applied"
    );
}

/// A plan built before someone else migrated must not be replayed
#[tokio::test]
async fn a_stale_plan_is_not_replayed_when_the_lock_is_taken() {
    let _guard = serial().await;
    let db = connect().await;

    create_legacy_table(
        &db,
        "e2e_add_column",
        "`id` int NOT NULL PRIMARY KEY AUTO_INCREMENT, `email` varchar(255) NOT NULL",
    )
    .await;
    let stale = plan_migrations(&db).await.expect("plan the migration");
    let stale_statements: Vec<String> = statements_for(&stale, "e2e_add_column")
        .into_iter()
        .map(str::to_string)
        .collect();
    assert!(!stale_statements.is_empty(), "there should be a plan to reuse");

    // Another instance takes the lock, migrates and releases it
    let other = db.begin().await.expect("begin the other transaction");
    other
        .execute_raw(sea_orm::Statement::from_string(
            DbBackend::MySql,
            format!("SELECT GET_LOCK({LOCK_SQL}, 0)"),
        ))
        .await
        .expect("the other instance takes the lock");
    for sql in &stale_statements {
        other
            .execute_raw(sea_orm::Statement::from_string(
                DbBackend::MySql,
                sql.clone(),
            ))
            .await
            .expect("the other instance migrates");
    }
    other
        .execute_raw(sea_orm::Statement::from_string(
            DbBackend::MySql,
            format!("SELECT RELEASE_LOCK({LOCK_SQL})"),
        ))
        .await
        .expect("the other instance releases the lock");
    other.commit().await.expect("the other instance commits");

    // This instance still holds the now outdated plan. Replaying it would fail
    // on the column the other instance already added, so the plan is rebuilt
    // after the lock is taken.
    let outcome = apply_migrations_with(&db, &stale, MigrateOptions::locked(0))
        .await
        .expect("a stale plan must not be replayed");
    assert_eq!(outcome, MigrationOutcome::Applied);

    let plan = plan_migrations(&db).await.expect("plan again");
    assert!(
        statements_for(&plan, "e2e_add_column").is_empty(),
        "nothing should be left to migrate"
    );
}

#[tokio::test]
async fn a_fully_migrated_database_plans_nothing() {
    let _guard = serial().await;
    let db = connect().await;

    // Bring every registered table up to date, whatever state they are in
    create_missing_tables(&db).await.expect("create the missing tables");
    loop {
        let plan = plan_migrations(&db).await.expect("plan the migration");
        if plan.is_empty() {
            break;
        }
        apply_migrations(&db, &plan).await.expect("apply the plan");
    }

    // Reading the schema back must now match the entities exactly
    let plan = plan_migrations(&db).await.expect("plan the migration");
    assert!(
        plan.is_empty(),
        "a fully migrated database must plan nothing, got: {plan:?}"
    );
}
