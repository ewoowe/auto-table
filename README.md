# auto-table

[English](README.md) | [中文文档](README.zh-CN.md)

An automatic table-creation toolkit built on [SeaORM](https://crates.io/crates/sea-orm). It collects all entities at compile time via attribute macros and automatically creates missing tables in the database at application startup.

## Crates

This workspace contains two crates:

| Crate | Description |
| --- | --- |
| [`auto-table-core`](auto-table-core) | Core library: runtime support and error types for the `#[auto_table]` / `#[auto_create]` macros |
| [`auto-table-derive`](auto-table-derive) | Procedural macro implementation |

> In most cases you only need to depend on `auto-table-core`; it re-exports both procedural macros via `pub use`.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
auto-table-core = "0.3.0"
```

### 1. Mark an entity

Apply `#[auto_table]` to the `Model` struct of a SeaORM Entity:

```rust
use sea_orm::entity::prelude::*;

#[auto_table]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "users")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub name: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
```

### 2. Inject table-creation logic

Apply `#[auto_create(db)]` to the function that initializes the database, where `db` is the name of the `DatabaseConnection` variable inside the function body:

```rust
#[auto_create(db)]
pub async fn init_pool(database_url: &str) -> anyhow::Result<()> {
    let db = Database::connect(database_url).await?;
    db.ping().await?;
    DB.set(db).expect("...");
    Ok(())
}
```

The macro locates the `let db = ...` binding in the function body and injects the table-creation logic immediately after it, so the logic runs right after the connection is established and before `db` is moved. This does not depend on how many trailing statements the function body has.

The injected logic performs no logging. Instead, the generated table-creation function returns an [`auto_table_core::TableCreationReport`](auto-table-core/src/lib.rs), which contains `existing_tables` (tables that already existed and were skipped) and `created_tables` (tables created in this run). The injected statement binds this report to a local variable named `__auto_table_report`, which is in scope for the rest of the function body, so you can log or act on the result there. The name starts with an underscore, so leaving it unused does not trigger a warning. You can also call [`auto_table_core::create_missing_tables`](auto-table-core/src/lib.rs) directly if you need the report outside the macro.

### 3. Backend feature flags

Only the MySQL driver is enabled by default. Switch or combine as needed (`default = ["mysql"]`):

```toml
# PostgreSQL only
auto-table-core = { version = "0.3.0", default-features = false, features = ["postgres"] }

# Both MySQL and SQLite
auto-table-core = { version = "0.3.0", default-features = false, features = ["mysql", "sqlite"] }
```

Available features: `mysql`, `postgres`, `sqlite`. They are additive, so several can be enabled at once; **at least one is required**, otherwise compilation fails immediately.

> The features only decide which **database drivers** get compiled. The backend is determined at runtime by `DbBackend`, so logic such as querying existing tables works for all three backends regardless.

## Supported backends

- MySQL
- PostgreSQL
- SQLite

## Error handling

The core library exposes a precise [`auto_table_core::TableError`](auto-table-core/src/lib.rs) via `thiserror`; upper-layer applications can propagate it automatically as `anyhow::Error` through `?`.

## Roadmap

- [ ] **Database migrations** — currently the library only creates missing tables and never alters existing ones. Planned:
  - Diff entity definitions against live table schemas and generate `ALTER TABLE` statements automatically
  - Add/drop columns, change column types and constraints, manage indexes and unique constraints
  - Dry-run mode to preview the migration SQL before executing it
  - Risk classification: operations that can lose data (dropping a column, narrowing a type) require explicit opt-in and are never applied automatically
  - Concurrency safety: when several instances start at once, a database lock (`GET_LOCK` / `pg_advisory_lock` / an exclusive SQLite transaction) ensures only one of them migrates
- [ ] **SQLite table rebuild** — SQLite has no `MODIFY COLUMN` and only supports `DROP COLUMN` since 3.35, so structural changes must go through the "create new table → copy data → drop old → rename" procedure; this needs a dedicated implementation
- [ ] Rollback (down migration) support — migrations here are declarative (diff the current state against the target state), so a `down` cannot be generated reliably: data is already gone after `DROP COLUMN`, and MySQL/SQLite DDL does not roll back inside a transaction. Plan: first ship "best-effort rollback via reverse operations on failure", and make lossy steps fail loudly instead of silently continuing
- [x] Finer-grained backend feature flags (opt-in MySQL / PostgreSQL / SQLite)

## License

MIT
