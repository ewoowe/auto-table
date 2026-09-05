//! Test helpers shared by the three backend end-to-end suites.
//!
//! `sqlite_e2e`, `mysql_e2e` and `pg_e2e` each include this module with
//! `mod common;` (it lives at `tests/common/mod.rs`) and then supply their own
//! connection plus the backend-specific SQL they expect. The
//! planning / asserting / applying / idempotency boilerplate lives here so it
//! is written exactly once.

use auto_table_core::{apply_migrations, plan_migrations, MigrationPlan, TableError};
use sea_orm::{ConnectionTrait, DatabaseConnection};

/// The statements a plan wants to run against one table.
pub fn statements_for<'a>(plan: &'a MigrationPlan, table: &str) -> Vec<&'a str> {
    plan.tables
        .iter()
        .find(|migration| migration.table == table)
        .map(|migration| migration.statements.iter().map(String::as_str).collect())
        .unwrap_or_default()
}

/// Recreates `table` from a legacy `definition`, standing in for a database that
/// is one migration behind. `quote` is the identifier quote of the backend
/// (`"` for SQLite/PostgreSQL, `` ` `` for MySQL).
pub async fn create_legacy_table(db: &DatabaseConnection, table: &str, quote: char, definition: &str) {
    let quote_ident = |name: &str| format!("{quote}{name}{quote}");
    db.execute_unprepared(&format!("DROP TABLE IF EXISTS {}", quote_ident(table)))
        .await
        .expect("drop the table");
    db.execute_unprepared(&format!("CREATE TABLE {} ({})", quote_ident(table), definition))
        .await
        .expect("create the legacy table");
}

/// Plans, checks the statements, applies them, then checks the plan is empty
/// (the migration must be idempotent).
pub async fn check_and_apply(db: &DatabaseConnection, table: &str, expected: &[&str]) {
    let plan = plan_migrations(db).await.expect("plan the migration");
    let statements = statements_for(&plan, table);
    assert_eq!(statements, expected, "unexpected statements for `{table}`");

    apply_migrations(db, &plan)
        .await
        .expect("apply the plan");

    let plan = plan_migrations(db).await.expect("plan again after applying");
    assert!(
        statements_for(&plan, table).is_empty(),
        "`{table}` is not idempotent, it still plans: {:?}",
        statements_for(&plan, table)
    );
}

/// Like [`check_and_apply`] but for a destructive plan: applying it without
/// approval must be refused, and only `allow_destructive` lets it run and
/// converge.
pub async fn check_and_apply_destructive(db: &DatabaseConnection, table: &str, expected: &[&str]) {
    let plan = plan_migrations(db).await.expect("plan the migration");
    let statements = statements_for(&plan, table);
    assert_eq!(statements, expected, "unexpected statements for `{table}`");

    let blocked = apply_migrations(db, &plan).await;
    assert!(
        matches!(blocked, Err(TableError::DestructiveChangesBlocked { .. })),
        "destructive change must be blocked by default, got {blocked:?}"
    );

    // `allow_destructive` consumes the plan and returns an approved copy.
    let plan = plan.allow_destructive();
    apply_migrations(db, &plan)
        .await
        .expect("apply the plan after allowing destructive changes");

    let plan = plan_migrations(db).await.expect("plan again after applying");
    assert!(
        statements_for(&plan, table).is_empty(),
        "`{table}` is not idempotent, it still plans: {:?}",
        statements_for(&plan, table)
    );
}
