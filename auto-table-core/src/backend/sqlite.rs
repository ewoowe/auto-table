//! SQLite backend
//!
//! Reading structures through `PRAGMA`s, and rebuilding a table whenever a
//! column definition has to change, since SQLite cannot alter one in place.

use sea_orm::{ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, Statement};

use crate::diff::{ColumnChange, IndexChange, TableDiff};
use crate::migrate::{execute, TableMigration};
use crate::parse::{parse_create_table, PRIMARY_INDEX_NAME};
use crate::schema::{unquote_literal, ColumnSchema, IndexSchema, TableSchema};
use crate::{Backend, TableError};

/// The SQLite backend
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Sqlite;

impl Backend for Sqlite {
    async fn read_table(
        &self,
        db: &DatabaseConnection,
        table: &str,
    ) -> Result<TableSchema, TableError> {
        get_table_schema_sqlite(db, table).await
    }

    fn normalize_expected(&self, schema: &mut TableSchema) {
        // SQLite stores by type affinity, so both sides have to be reduced to
        // an affinity before they can be compared.
        for column in &mut schema.columns {
            column.col_type = sqlite_type_affinity(&column.col_type);
        }
    }

    fn plan(&self, diff: &TableDiff, create_sql: &str) -> Result<TableMigration, TableError> {
        if sqlite_needs_rebuild(diff) {
            Ok(TableMigration::transactional(
                diff.table.clone(),
                sqlite_rebuild(diff, create_sql)?,
            ))
        } else {
            Ok(TableMigration::new(
                diff.table.clone(),
                sqlite_simple_alters(diff),
            ))
        }
    }

    async fn acquire_lock(
        &self,
        transaction: &DatabaseTransaction,
        timeout_secs: u32,
    ) -> Result<bool, TableError> {
        acquire_sqlite_lock(transaction, timeout_secs).await
    }

    async fn release_lock(&self, _transaction: &DatabaseTransaction) -> Result<(), TableError> {
        // SQLite releases its write lock when the transaction ends
        Ok(())
    }

    fn before_statements(&self) -> &[&str] {
        // `PRAGMA foreign_keys` is a no-op inside a transaction
        &["PRAGMA foreign_keys = OFF"]
    }

    fn after_statements(&self) -> &[&str] {
        &["PRAGMA foreign_keys = ON"]
    }
}

/// Reads the current structure of `table_name` from a SQLite database
///
/// SQLite publishes its catalogue through `PRAGMA`s instead of
/// `information_schema`, and PRAGMAs take no bind parameters, so the table name
/// is quoted into the statement as a literal.
///
/// Two SQLite specifics are handled here:
///
/// - The primary key of a rowid table is **not** listed by `pragma_index_list`;
///   it is rebuilt from the `pk` column of `pragma_table_info` and reported
///   under the same name MySQL uses, so both sides of a diff line up.
/// - `AUTOINCREMENT` is not exposed per column at all, so it is read from the
///   stored `CREATE TABLE` statement and attributed to the primary key.
pub async fn get_table_schema_sqlite(
    db: &DatabaseConnection,
    table_name: &str,
) -> Result<TableSchema, TableError> {
    let fail = |source| TableError::QuerySchemaFailed {
        table: table_name.to_string(),
        source,
    };

    let column_rows = db
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            format!(
                "SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info({})",
                quote_sqlite_literal(table_name)
            ),
        ))
        .await
        .map_err(fail)?;

    let mut columns = Vec::with_capacity(column_rows.len());
    let mut primary_key: Vec<(i64, String)> = Vec::new();

    for row in &column_rows {
        let Some(name) = row.try_get_by_index::<String>(0).ok() else {
            continue;
        };

        // SQLite allows a column with no declared type; report it as empty so
        // that the affinity calculation treats it as BLOB, like SQLite does.
        let col_type = row.try_get_by_index::<String>(1).unwrap_or_default();
        let not_null = row.try_get_by_index::<i64>(2).unwrap_or(0);
        let default = row
            .try_get_by_index::<String>(3)
            .ok()
            .map(|value| unquote_literal(&value));
        let pk_position = row.try_get_by_index::<i64>(4).unwrap_or(0);

        if pk_position > 0 {
            primary_key.push((pk_position, name.clone()));
        }

        columns.push(ColumnSchema {
            name,
            col_type: sqlite_type_affinity(&col_type),
            nullable: not_null == 0,
            default,
            // Filled in below, once AUTOINCREMENT is known
            auto_increment: false,
        });
    }

    let auto_increment = table_is_auto_increment(db, table_name).await.map_err(fail)?;
    if auto_increment {
        for column in &mut columns {
            column.auto_increment = primary_key
                .iter()
                .any(|(_, name)| *name == column.name);
        }
    }

    let mut indexes = Vec::new();

    // The primary key first, so it lines up with how MySQL reports it
    primary_key.sort_by_key(|(position, _)| *position);
    if !primary_key.is_empty() {
        indexes.push(IndexSchema {
            name: PRIMARY_INDEX_NAME.to_string(),
            columns: primary_key
                .into_iter()
                .map(|(_, name)| name)
                .collect(),
            unique: true,
            primary: true,
        });
    }

    let index_rows = db
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            format!(
                "SELECT name, \"unique\", origin FROM pragma_index_list({})",
                quote_sqlite_literal(table_name)
            ),
        ))
        .await
        .map_err(fail)?;

    for row in &index_rows {
        let Some(index_name) = row.try_get_by_index::<String>(0).ok() else {
            continue;
        };
        let unique = row.try_get_by_index::<i64>(1).unwrap_or(0);
        let origin = row.try_get_by_index::<String>(2).unwrap_or_default();

        // The primary key is already reported above, from `pragma_table_info`
        if origin == "pk" {
            continue;
        }

        let column_rows = db
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                format!(
                    "SELECT name FROM pragma_index_info({}) ORDER BY seqno",
                    quote_sqlite_literal(&index_name)
                ),
            ))
            .await
            .map_err(fail)?;

        let columns: Vec<String> = column_rows
            .iter()
            .filter_map(|row| row.try_get_by_index::<String>(0).ok())
            .collect();
        if columns.is_empty() {
            continue;
        }

        // SQLite creates an index for every UNIQUE constraint and names it
        // `sqlite_autoindex_<table>_<n>`; such an index cannot be dropped on
        // its own. Naming it after its columns matches what parsing the
        // entity's own CREATE TABLE produces, which keeps the two sides of a
        // diff in agreement.
        let name = if origin == "u" {
            columns.join("_")
        } else {
            index_name
        };

        indexes.push(IndexSchema {
            name,
            columns,
            unique: unique != 0,
            primary: false,
        });
    }

    Ok(TableSchema {
        name: table_name.to_string(),
        columns,
        indexes,
    })
}

/// Whether the table was declared with `AUTOINCREMENT`
///
/// SQLite keeps this only in the stored `CREATE TABLE` text.
async fn table_is_auto_increment(
    db: &DatabaseConnection,
    table_name: &str,
) -> Result<bool, sea_orm::DbErr> {
    let rows = db
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            format!(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = {}",
                quote_sqlite_literal(table_name)
            ),
        ))
        .await?;

    let found = rows
        .iter()
        .filter_map(|row| row.try_get_by_index::<String>(0).ok())
        .any(|sql| sql.to_ascii_uppercase().contains("AUTOINCREMENT"));

    Ok(found)
}

/// The type affinity SQLite assigns to a declared type
///
/// SQLite does not enforce declared types: `int`, `integer` and `bigint` all
/// end up with INTEGER affinity and store the same values. Comparing the
/// declared spellings would therefore report a difference for a change such as
/// `i32` -> `i64` and trigger a pointless table rebuild. Normalizing to the
/// five affinities keeps the comparison meaningful.
///
/// The rules are the ones SQLite itself documents, in order:
/// `INT` -> INTEGER, `CHAR`/`CLOB`/`TEXT` -> TEXT, `BLOB`/empty -> BLOB,
/// `REAL`/`FLOA`/`DOUB` -> REAL, anything else -> NUMERIC.
pub fn sqlite_type_affinity(declared: &str) -> String {
    let upper = declared.to_ascii_uppercase();

    let affinity = if upper.contains("INT") {
        "INTEGER"
    } else if upper.contains("CHAR") || upper.contains("CLOB") || upper.contains("TEXT") {
        "TEXT"
    } else if upper.contains("BLOB") || upper.trim().is_empty() {
        "BLOB"
    } else if upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB") {
        "REAL"
    } else {
        "NUMERIC"
    };

    affinity.to_string()
}

/// Quotes a string literal, escaping the quotes it contains
fn quote_sqlite_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
/// SQLite has no named locks; its write lock already serialises writers
///
/// All this does is make a concurrent writer wait for its turn instead of
/// failing immediately with SQLITE_BUSY.
async fn acquire_sqlite_lock(
    transaction: &DatabaseTransaction,
    timeout_secs: u32,
) -> Result<bool, TableError> {
    let millis = u64::from(timeout_secs) * 1000;
    execute(transaction, &format!("PRAGMA busy_timeout = {millis}")).await?;
    Ok(true)
}

/// Key for PostgreSQL's advisory lock
///

/// Suffix used for the replacement table while a table is being rebuilt
pub const REBUILD_SUFFIX: &str = "__auto_table_rebuild";

/// Whether SQLite can express this diff with plain `ALTER TABLE` statements
///
/// SQLite can add and drop columns, but it cannot touch a column definition —
/// there is no `MODIFY COLUMN` and no `ALTER COLUMN`. Anything else has to go
/// through a table rebuild, and that includes **every** index change: a unique
/// constraint is part of the table definition, and the index backing it cannot
/// be dropped on its own at all.
fn sqlite_needs_rebuild(diff: &TableDiff) -> bool {
    let changes_a_column = diff
        .columns
        .iter()
        .any(|change| matches!(change, ColumnChange::Alter { .. }));

    changes_a_column || !diff.indexes.is_empty()
}

/// Builds the SQLite statements for a diff that needs no table rebuild
fn sqlite_simple_alters(diff: &TableDiff) -> Vec<String> {
    let table = quote_sqlite_identifier(&diff.table);
    let mut statements = Vec::new();

    for change in &diff.indexes {
        if let IndexChange::Drop(index) = change {
            statements.push(format!(
                "DROP INDEX {}",
                quote_sqlite_identifier(&index.name)
            ));
        }
    }

    for change in &diff.columns {
        if let ColumnChange::Drop { name } = change {
            statements.push(format!(
                "ALTER TABLE {table} DROP COLUMN {}",
                quote_sqlite_identifier(name)
            ));
        }
    }

    for change in &diff.columns {
        if let ColumnChange::Add(column) = change {
            statements.push(format!(
                "ALTER TABLE {table} ADD COLUMN {}",
                sqlite_column_definition(column)
            ));
        }
    }

    for change in &diff.indexes {
        if let IndexChange::Add(index) = change {
            // A primary key can only be created with the table, so reaching
            // here with one means the caller should have rebuilt instead.
            if !index.primary {
                statements.push(sqlite_create_index(&diff.table, index));
            }
        }
    }

    statements
}

/// Builds the statements that rebuild a table with the structure the entity declares
///
/// A rebuild is the only way to change a column definition on SQLite. It creates
/// the new table from the entity's own `CREATE TABLE`, copies over the rows, then
/// swaps the tables. The caller must run these statements inside a transaction.
fn sqlite_rebuild(diff: &TableDiff, create_sql: &str) -> Result<Vec<String>, TableError> {
    if create_sql.trim().is_empty() {
        return Err(TableError::MissingCreateStatement {
            table: diff.table.clone(),
        });
    }

    let target = parse_create_table(create_sql).map_err(|source| TableError::ParseExpectedFailed {
        table: diff.table.clone(),
        source,
    })?;

    let table = quote_sqlite_identifier(&diff.table);
    let replacement_name = format!("{}{}", diff.table, REBUILD_SUFFIX);
    let replacement = quote_sqlite_identifier(&replacement_name);

    let mut statements = vec![retarget_create_table(create_sql, &diff.table, &replacement_name)];

    if let Some(insert) = sqlite_copy_rows(&target, &replacement, &table, diff) {
        statements.push(insert);
    }

    statements.push(format!("DROP TABLE {table}"));
    statements.push(format!("ALTER TABLE {replacement} RENAME TO {table}"));

    // Indexes die with the old table and have to be created again. The primary
    // key and inline unique constraints are already part of the new definition.
    for change in &diff.indexes {
        if let IndexChange::Add(index) = change {
            if !index.primary {
                // The replacement table has already been renamed back, so the
                // index is created against the original table name.
                statements.push(sqlite_create_index(&diff.table, index));
            }
        }
    }

    Ok(statements)
}

/// Builds the `INSERT INTO ... SELECT` that carries rows into the new table
///
/// Only the columns present on both sides can be copied; columns that the entity
/// no longer declares are simply left behind.
fn sqlite_copy_rows(
    target: &crate::schema::TableSchema,
    replacement: &str,
    table: &str,
    diff: &TableDiff,
) -> Option<String> {
    let added: Vec<&str> = diff
        .columns
        .iter()
        .filter_map(|change| match change {
            ColumnChange::Add(column) => Some(column.name.as_str()),
            _ => None,
        })
        .collect();

    let dropped: Vec<&str> = diff
        .columns
        .iter()
        .filter_map(|change| match change {
            ColumnChange::Drop { name } => Some(name.as_str()),
            _ => None,
        })
        .collect();

    // The old table had every column the new one declares except the added
    // ones, plus the columns that are about to be dropped.
    let mut old_columns: Vec<&str> = target
        .columns
        .iter()
        .filter(|column| !added.contains(&column.name.as_str()))
        .map(|column| column.name.as_str())
        .collect();
    old_columns.extend(dropped.iter().copied());

    let shared: Vec<&str> = target
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .filter(|name| old_columns.contains(name))
        .collect();

    if shared.is_empty() {
        return None;
    }

    let columns = shared
        .iter()
        .map(|name| quote_sqlite_identifier(name))
        .collect::<Vec<_>>()
        .join(", ");

    Some(format!(
        "INSERT INTO {replacement} ({columns}) SELECT {columns} FROM {table}"
    ))
}

/// Rewrites a `CREATE TABLE` statement to build the table under another name
fn retarget_create_table(sql: &str, table: &str, replacement: &str) -> String {
    let old = quote_sqlite_identifier(table);
    let new = quote_sqlite_identifier(replacement);

    // Only the table name right after CREATE TABLE may be replaced; the same
    // spelling cannot legitimately appear anywhere else in the statement.
    match sql.find(&old) {
        Some(position) => format!("{}{}{}", &sql[..position], new, &sql[position + old.len()..]),
        None => sql.to_string(),
    }
}

fn sqlite_column_definition(column: &ColumnSchema) -> String {
    let mut definition = format!(
        "{} {}",
        quote_sqlite_identifier(&column.name),
        column.col_type
    );

    if !column.nullable {
        definition.push_str(" NOT NULL");
    }
    if let Some(default) = &column.default {
        definition.push_str(&format!(" DEFAULT {}", quote_sqlite_literal(default)));
    }

    definition
}

fn sqlite_create_index(table: &str, index: &IndexSchema) -> String {
    let name = quote_sqlite_identifier(&index.name);
    let target = quote_sqlite_identifier(table);
    let columns = index
        .columns
        .iter()
        .map(|column| quote_sqlite_identifier(column))
        .collect::<Vec<_>>()
        .join(", ");

    if index.unique {
        format!("CREATE UNIQUE INDEX {name} ON {target} ({columns})")
    } else {
        format!("CREATE INDEX {name} ON {target} ({columns})")
    }
}

/// Quotes an identifier, SQLite style
fn quote_sqlite_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

