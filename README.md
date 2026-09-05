# auto-table

[English](README.md) | [中文文档](README.zh-CN.md)

A table-creation and migration toolkit built on [SeaORM](https://crates.io/crates/sea-orm). It collects all entities at compile time via attribute macros, creates missing tables at application startup, and can bring existing tables back in line with the entity definitions (MySQL and SQLite).

## Crates

This workspace contains two crates:

| Crate | Description |
| --- | --- |
| [`auto-table-core`](auto-table-core) | Core library: runtime support for the `#[auto_table]` / `#[auto_create]` macros, schema reading and diffing, migration planning and execution |
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

> The features only decide which **database drivers** get compiled. The backend is determined at runtime by `DbBackend`, so logic such as querying existing tables is compiled in for all three; connecting to a given backend still requires its driver.

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

Migrations work on MySQL, SQLite and PostgreSQL. The statements they need
differ per backend, as the sections below describe.

### 5. Concurrency (more than one instance)

If several instances start at once and all try to migrate, the ones that arrive
later re-run statements that are already applied and fail. A lock prevents it:

```rust
use auto_table_core::{migrate, MigrateOptions};

// Wait for the lock, and fail if the wait runs out
migrate(&db, MigrateOptions::locked(10)).await?;

// Skip instead: another instance is applying the very same changes
migrate(&db, MigrateOptions::skip_if_locked(0)).await?;
```

`apply_migrations` takes **no lock**, keeping the previous behaviour; a single
instance needs none.

**Prefer `migrate` over planning first and then applying.** `migrate` builds the
plan *after* the lock is held, so it plans once and the plan cannot be stale;
planning beforehand means either trusting a plan that may already be outdated or
building it a second time behind the lock. `plan_migrations` on its own is for
the one case where it is still worth it: reviewing the statements before
anything runs.

Two notes on how it works:

- **MySQL** uses the named lock `GET_LOCK`. It is a *session* lock, so the whole
  migration runs inside one transaction to pin it to a connection — otherwise
  the lock would guard nothing. The lock name includes the database, so two
  databases on one server do not block each other.
- **PostgreSQL** uses `pg_advisory_lock` on a fixed key. It is session-scoped
  like `GET_LOCK`, so the migration runs inside one transaction to keep the lock
  effective; `pg_try_advisory_lock` backs the non-blocking `skip` mode.
- **SQLite** has no named locks and relies on its own write lock; taking the
  lock only adds a `busy_timeout` so a concurrent instance queues instead of
  failing immediately with `SQLITE_BUSY`.

On all three backends the plan is **rebuilt after the lock is taken**: while waiting
for the lock, another instance may well have finished migrating, and replaying a
stale plan would only fail.

For finer-grained control the building blocks are public as well:

- [`get_table_schema`](auto-table-core/src/schema.rs) — read the current structure of a table
- [`parse_create_table`](auto-table-core/src/parse.rs) — parse the entity's `CREATE TABLE` into the same structure
- [`diff_table`](auto-table-core/src/diff.rs) — compare two structures and get the list of changes
- [`plan_table_migration`](auto-table-core/src/migrate.rs) — turn the changes of one table into statements (MySQL and SQLite)

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

#### PostgreSQL migration scenarios

PostgreSQL splits a column change into **one statement per aspect**, rather
than folding it into a single `MODIFY COLUMN` the way MySQL does:

| Change | Statement |
| --- | --- |
| Add a column | `ALTER TABLE t ADD COLUMN "c" type [NOT NULL] [DEFAULT x]` |
| Drop a column | `ALTER TABLE t DROP COLUMN "c"` |
| Change the type | `ALTER TABLE t ALTER COLUMN "c" TYPE new_type` |
| Tighten to `NOT NULL` | `ALTER TABLE t ALTER COLUMN "c" SET NOT NULL` |
| Relax to nullable | `ALTER TABLE t ALTER COLUMN "c" DROP NOT NULL` |
| Set a default | `ALTER TABLE t ALTER COLUMN "c" SET DEFAULT x` |
| Drop a default | `ALTER TABLE t ALTER COLUMN "c" DROP DEFAULT` |
| Add a unique constraint | `ALTER TABLE t ADD CONSTRAINT "t_c_key" UNIQUE ("c")` |
| Drop a unique constraint | `ALTER TABLE t DROP CONSTRAINT "t_c_key"` |

A change to one column can therefore produce several statements: changing both
its type and its nullability emits two.

Two more things need handling:

- **Constraint names follow PostgreSQL.** The primary key is `<table>_pkey` and
  a unique constraint is `<table>_<column>_key`. Structures are read under a
  logical name and only turned back into the physical one when emitting DDL,
  otherwise the two sides would never agree.
- **Type names are normalized.** PostgreSQL reports `varchar` as `character
  varying`, stores `decimal` as `numeric` and calls `bool` `boolean`; without
  normalizing them, every string, decimal and boolean column looks like a
  difference on every run.

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
| Add a nullable column | Safe | filled with `NULL` | filled with `NULL` | filled with `NULL` |
| Add a `NOT NULL` column with a default | Safe | filled with the default | filled with the default | filled with the default |
| Widen a type (`int` -> `bigint`) | Safe | lossless | no statements, same affinity | lossless |
| Change a default value | Safe | existing rows untouched | rebuilt, existing rows keep their values | existing rows untouched |
| Drop an index | Safe | data untouched | rebuilt, data untouched | data untouched |
| Add a `NOT NULL` column without a default | Caution | **silently filled with `''` or `0`** | errors out | **errors out if rows exist** |
| Narrow a type | Caution | errors in strict mode | converted to the new affinity; values that do not convert are kept | errors when a value does not fit |
| Tighten to `NOT NULL` | Caution | errors while `NULL`s exist | rebuild fails and rolls back, table intact | errors while `NULL`s exist |
| Add a unique index or constraint | Caution | errors while duplicates exist | rebuild fails and rolls back, table intact | errors while duplicates exist |
| **Drop a column** | **Destructive** | **data lost for good, no rollback** | **data lost for good, no rollback** | **data lost for good** |

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

> PostgreSQL is now fully supported and measured. One notable divergence: "add a
> `NOT NULL` column without a default" **errors out** in both PostgreSQL and
> SQLite (unless the table is empty), whereas MySQL silently fills existing rows
> with `''` or `0`.

> This section is a design draft and is not implemented yet.

## Supported backends

Automatic table creation and migrations work with all three.

- MySQL
- PostgreSQL
- SQLite

## Error handling

The core library exposes a precise [`auto_table_core::TableError`](auto-table-core/src/lib.rs) via `thiserror`; upper-layer applications can propagate it automatically as `anyhow::Error` through `?`.

## Roadmap

- [x] **Database migrations** — available on MySQL, SQLite and PostgreSQL (see "4. Migrating tables that already exist" and "PostgreSQL migration scenarios"). The concurrency lock uses each backend's native mechanism — MySQL `GET_LOCK`, SQLite its write lock plus a `busy_timeout`, PostgreSQL `pg_advisory_lock` (see "5. Concurrency") — and the plan is re-planned after the lock is taken so a stale plan is never replayed.
  - Risk classification (see the design draft in "Risk classification") is not yet enforced: dropping a column is still destructive and is only protected once the proposed approval API lands.
- [x] **SQLite migrations (including table rebuild)** — SQLite has no `MODIFY COLUMN`, so changes to a column definition and to indexes or constraints go through "create new table → copy data → drop old → rename", inside a single transaction that rolls back on failure (see "SQLite migration scenarios")
- [ ] Rollback (down migration) support — migrations here are declarative (diff the current state against the target state), so a `down` cannot be generated reliably: data is already gone after `DROP COLUMN`, and MySQL DDL does not roll back inside a transaction (SQLite does, verified). Plan: first ship "best-effort rollback via reverse operations on failure", and make lossy steps fail loudly instead of silently continuing
- [x] Finer-grained backend feature flags (opt-in MySQL / PostgreSQL / SQLite)

## Test coverage

The library is verified by two layers that together exercise every migration scenario on all three backends:

- **Unit tests** live next to the planner in `auto-table-core/src/backend/{mysql,postgres,sqlite}.rs`. They assert the exact DDL each `IndexChange` / `ColumnAspect` produces, plus type normalization and statement ordering — no database required. (72 tests.)
- **End-to-end tests** in `auto-table-core/tests/{mysql,pg,sqlite}_e2e.rs` drive the whole path — read the live schema → diff against the entity → plan → apply real DDL → assert the plan is empty on the next run. They run against a live MySQL 8, PostgreSQL 18 and the bundled SQLite. (19 + 15 + 11 = 45 tests.)

`cargo test -p auto-table-core --all-features` with `AUTO_TABLE_TEST_DATABASE_URL` / `AUTO_TABLE_TEST_POSTGRES_URL` set runs all **117 tests**; the suite currently passes with **zero warnings**.

### Scenario matrix

| Migration scenario | MySQL | PostgreSQL | SQLite | Caveats |
| --- | --- | --- | --- | --- |
| Create missing tables / already in sync | ✅ baseline | ✅ baseline | ✅ baseline | `create_missing_tables`, then an empty plan once in sync |
| Add a column | ✅ `adds_a_column_missing_from_the_table` | ✅ `adds_a_column_missing_from_the_table` | ✅ `adds_a_column_without_rebuilding` | SQLite adds without a rebuild (additive) |
| Drop a column | ✅ `drops_a_column_…` | ✅ `drops_a_column_…` | ✅ `drops_a_column_without_rebuilding` | destructive — irreversible |
| Widen a type (`int` → `bigint`) | ✅ `widens_a_column_type` | ✅ `widens_a_column_type` | ✅ `widening_i32_to_i64_is_not_a_change` | SQLite: same affinity → no statements at all |
| Tighten to `NOT NULL` | ✅ `makes_a_nullable_column_required` | ✅ `makes_a_required_column_nullable` | ✅ (rebuild) | SQLite rebuilds the table |
| Relax to nullable | ◐ unit | ✅ `makes_a_required_column_nullable` | ✅ (rebuild) | MySQL repeats the whole column definition |
| Change a default value | ✅ `adds_a_default_value` | ✅ `adds_a_default_value` | ✅ `adds_a_default_value_by_rebuilding` / `changes_a_default_value_by_rebuilding` | SQLite rebuilds the table |
| Add `NOT NULL` column with a default | ✅ `adds_a_not_null_column_with_a_default` | ◐ unit | ✅ `adds_a_not_null_column_with_a_default` | — |
| Add `NOT NULL` column without a default | ✅ `adds_a_required_column_without_a_default` | ◐ unit | ⚠️ errors (documented) | MySQL strict mode *silently* fills `''`/`0`; SQLite refuses the statement |
| Add a unique index / constraint | ✅ `adds_a_missing_unique_index` | ✅ `adds_a_missing_unique_constraint` | ✅ `adds_a_missing_unique_index_by_rebuilding` | SQLite rebuilds the table |
| Drop a unique index / constraint | ✅ `drops_an_index_…` | ✅ `drops_a_unique_constraint` | ✅ (rebuild) | — |
| Add a **plain (non-unique)** index | ⚠️ not covered | ⚠️ not covered | ⚠️ not covered | `parse_create_table` ignores table-level `INDEX` clauses, so the expected schema can never hold one; only the **drop** direction is reachable |
| Drop a plain index | ✅ `drops_a_plain_index` | ✅ `drops_a_plain_index` | ✅ `drops_an_index_by_rebuilding` | PG index must be created with a separate `CREATE INDEX`, not inline in `CREATE TABLE` |
| Add `AUTO_INCREMENT` / identity | ✅ `adds_auto_increment_to_a_primary_key` | ✅ `adds_an_identity_to_a_primary_key` | — | SQLite `INTEGER PRIMARY KEY` is already auto-increment |
| Drop a default value | ◐ unit | ✅ `drops_a_default_value` | ✅ (rebuild) | — |
| Table rebuild keeps existing rows | — | — | ✅ `rebuilding_keeps_the_rows` | SQLite-only rebuild path |
| Concurrency lock (run once) | ✅ `a_second_instance_waits…`, `migrate_*` | ✅ `a_second_instance_skips…` | ◐ unit | MySQL `GET_LOCK`; PG `skip_if_locked`; SQLite `busy_timeout` |
| Idempotency (empty re-plan after apply) | ✅ `a_fully_migrated_database_plans_nothing` | ✅ same | ✅ same | declarative migrations always converge |

Legend: ✅ covered by an e2e test (passes) · ◐ covered at the planner level by unit tests only · ⚠️ known gap or documented error · — not applicable.

The one structural gap worth flagging: because `parse_create_table` does not capture plain (non-unique) table-level indexes, an index declared via SeaORM's `#[sea_orm(indexed)]` is invisible to the expected schema, so it can never be *added* by a migration — only detected and *dropped* when it already exists on the table. This is a limitation of the schema parser, not the DDL planner (which can still emit `ADD INDEX` for it when the diff is built by hand).

## License

MIT
