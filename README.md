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

### 4. Migrating tables that already exist

`#[auto_create]` only creates tables that are missing. To bring a table whose
structure no longer matches the entity back in sync, use a migration:

```rust
// Build the plan; nothing has run yet, so this is the dry run
let plan = auto_table_core::plan_migrations(&db).await?;
for sql in plan.statements() {
    println!("{sql}");
}

// Run it once you are happy with it
auto_table_core::apply_migrations(&db, &plan).await?;
```

Migrations are **declarative**: every run diffs the entity definition against
the current database structure instead of replaying numbered steps. That makes
them idempotent — planning again after applying always yields nothing — and it
means a failed run can simply be retried after fixing the data, since finished
changes are not repeated.

MySQL and SQLite are supported. PostgreSQL is not: its ALTER syntax splits a
column change into several clauses, which needs a generator of its own.

#### What happens to existing rows

The table below is measured behaviour on MySQL 8 in **strict mode**, which is
the default and includes `STRICT_TRANS_TABLES`:

| Change | Existing rows | Outcome |
| --- | --- | --- |
| Add a nullable column | filled with `NULL` | safe |
| Add a `NOT NULL` column without a default | filled with `''` or `0` | silent, prefer an explicit `default_value` |
| Widen a type (`int` -> `bigint`) | values preserved | safe |
| Narrow a type with out-of-range values | — | error, the statement fails |
| Tighten to `NOT NULL` while `NULL`s exist | — | error, the statement fails |
| Add a unique index while duplicates exist | — | error, the statement fails |
| Change a default value | **existing rows untouched** | safe |
| Drop an index | data untouched | safe |
| **Drop a column** | **every value in it is lost** | **succeeds and cannot be undone** |

Two things deserve attention:

- **Dropping a column cannot be undone.** MySQL does not roll DDL back inside a
  transaction, so once it runs there is no way back.
- **Turning strict mode off changes the table above.** Narrowing a type then
  stops erroring and truncates instead (for example `3000000000` becomes
  `2147483647`), silently corrupting data.

#### SQLite migration scenarios

SQLite has neither `MODIFY COLUMN` nor `ALTER COLUMN`: **once a column is
defined, its definition cannot be changed**. Migrations therefore take one of
two routes:

- adding and dropping columns — plain `ALTER TABLE`;
- any change to a column definition (type, nullability, default) and any change
  to indexes or constraints — **rebuilding the table**: create the new table,
  copy the rows over, drop the old one, rename, recreate the indexes.

What each scenario does to existing rows (measured on SQLite 3.45):

| Change | How | Existing rows |
| --- | --- | --- |
| Add a nullable column | `ADD COLUMN` | filled with `NULL` |
| Add a `NOT NULL` column with a default | `ADD COLUMN` | filled with the default |
| Add a `NOT NULL` column without a default | — | **errors out**, nothing runs |
| Drop a column | `DROP COLUMN` (3.35+) | every value in it is lost |
| Change type within one affinity (`int` -> `bigint`) | no statements | unaffected, storage behaves identically |
| Change type across affinities | rebuild | converted to the new affinity; values that do not convert are kept as they are |
| Tighten to `NOT NULL` | rebuild | fails and rolls back while `NULL`s exist, **leaving the table intact** |
| Relax to nullable | rebuild | unaffected |
| Change a default value | rebuild | **existing rows keep their values**; only later inserts use it |
| Add a unique constraint | rebuild | fails and rolls back while duplicates exist, **leaving the table intact** |
| Drop an index | rebuild | data untouched |

Two sharp contrasts with MySQL:

- **SQLite is stricter when adding a required column.** A `NOT NULL` column
  with no default is an outright error, whereas MySQL silently fills existing
  rows with `''` or `0`.
- **A failed rebuild rolls back entirely.** MySQL cannot roll DDL back inside a
  transaction and SQLite can, so a rebuild that fails halfway leaves the table
  exactly as it was.

Bear in mind that rebuilding rewrites the whole table, which is expensive on
large ones.

#### Risk classification (design draft, not implemented)

Those dangers come in two kinds, and they should not be guarded the same way:

- **Caution (can fail)**: the migration is refused — the statement fails on MySQL
  in strict mode, and a rebuild fails and rolls back wholesale on SQLite — and in
  both cases **the data is left untouched**. Nothing is destroyed; the migration
  just stops.
- **Destructive (can destroy data)**: it succeeds unconditionally and **the data
  is gone with no way back**, on both backends.

Only the second one needs human approval; the first only needs a warning. The
proposed classification, with the MySQL and SQLite columns measured:

| Change | Risk | MySQL | SQLite | PostgreSQL |
| --- | --- | --- | --- | --- |
| Add a nullable column | Safe | filled with `NULL` | filled with `NULL` | to be measured |
| Add a `NOT NULL` column with a default | Safe | filled with the default | filled with the default | to be measured |
| Widen a type (`int` -> `bigint`) | Safe | lossless | no statements, same affinity | to be measured |
| Change a default value | Safe | existing rows untouched | rebuilt, existing rows keep their values | to be measured |
| Drop an index | Safe | data untouched | rebuilt, data untouched | to be measured |
| Add a `NOT NULL` column without a default | Caution | **silently filled with `''` or `0`** | errors out | to be measured |
| Narrow a type | Caution | errors in strict mode | converted to the new affinity; values that do not convert are kept | to be measured |
| Tighten to `NOT NULL` | Caution | errors while `NULL`s exist | rebuild fails and rolls back, table intact | to be measured |
| Add a unique index or constraint | Caution | errors while duplicates exist | rebuild fails and rolls back, table intact | to be measured |
| **Drop a column** | **Destructive** | **data lost for good, no rollback** | **data lost for good, no rollback** | to be measured |

Proposed API (not implemented, still subject to change):

```rust
// Every change can report its own risk
pub enum Risk { Safe, Caution, Destructive }

// Default: refuse to run anything if the plan contains a destructive change
apply_migrations(&db, &plan).await?;

// Run it only after an explicit approval
apply_migrations(&db, &plan).allow_destructive().await?;
```

Two deliberate trade-offs:

- **Check first, then run.** The whole plan is scanned before any statement
  runs, and an unapproved destructive change fails the call without executing
  anything. MySQL cannot roll DDL back, so discovering the danger halfway
  through would leave the database in a half-migrated state; on SQLite it also
  avoids carrying out an expensive table rebuild for nothing.
- **Only dropping a column counts as destructive.** A single, credible rule
  beats a broad one: if narrowing a type also required approval, nearly every
  field change would prompt, and people would click through prompts by habit,
  which defeats the protection entirely.

> The PostgreSQL column is a placeholder: the backend has no migration support
> yet, and its ALTER syntax differs enough that the behaviour needs measuring.
> Note that the same change can differ in how dangerous it is — most visibly
> "add a `NOT NULL` column without a default", which MySQL fills silently while
> SQLite refuses outright.

> This section is a design draft and is not implemented yet.

## Supported backends

Automatic table creation works with all three; **migrations cover MySQL and
SQLite**.

- MySQL
- PostgreSQL
- SQLite

## Error handling

The core library exposes a precise [`auto_table_core::TableError`](auto-table-core/src/lib.rs) via `thiserror`; upper-layer applications can propagate it automatically as `anyhow::Error` through `?`.

## Roadmap

- [ ] **Database migrations** — available on MySQL already (see "4. Migrating tables that already exist"), still to be completed:
  - Risk classification: irreversible changes such as dropping a column require explicit approval, otherwise the entire plan is refused (see the design draft above)
  - Concurrency safety: when several instances start at once, a database lock (`GET_LOCK` / `pg_advisory_lock` / an exclusive SQLite transaction) ensures only one of them migrates
- [x] **SQLite migrations (including table rebuild)** — SQLite has no `MODIFY COLUMN`, so changes to a column definition and to indexes or constraints go through "create new table → copy data → drop old → rename", inside a single transaction that rolls back on failure (see "SQLite migration scenarios")
- [ ] Rollback (down migration) support — migrations here are declarative (diff the current state against the target state), so a `down` cannot be generated reliably: data is already gone after `DROP COLUMN`, and MySQL DDL does not roll back inside a transaction (SQLite does, verified). Plan: first ship "best-effort rollback via reverse operations on failure", and make lossy steps fail loudly instead of silently continuing
- [x] Finer-grained backend feature flags (opt-in MySQL / PostgreSQL / SQLite)

## License

MIT
