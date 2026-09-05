//! MySQL backend
//!
//! Reading structures from `INFORMATION_SCHEMA`, emitting `ALTER TABLE`
//! statements and taking the named lock with `GET_LOCK`.

use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, Statement, Value,
};

use crate::diff::{ColumnChange, IndexChange, TableDiff};
use crate::migrate::TableMigration;
use crate::schema::{unquote_literal, ColumnSchema, IndexSchema, TableSchema};
use crate::{Backend, TableError};

/// The MySQL backend
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MySql;

impl Backend for MySql {
    async fn read_table(
        &self,
        db: &DatabaseConnection,
        table: &str,
    ) -> Result<TableSchema, TableError> {
        get_table_schema_mysql(db, table).await
    }

    fn plan(&self, diff: &TableDiff, _create_sql: &str) -> Result<TableMigration, TableError> {
        Ok(TableMigration::new(
            diff.table.clone(),
            mysql_statements(diff),
        ))
    }

    async fn acquire_lock(
        &self,
        transaction: &DatabaseTransaction,
        timeout_secs: u32,
    ) -> Result<bool, TableError> {
        acquire_mysql_lock(transaction, timeout_secs).await
    }

    async fn release_lock(&self, transaction: &DatabaseTransaction) -> Result<(), TableError> {
        release_mysql_lock(transaction).await
    }
}

/// Reads the current structure of `table_name` from a MySQL database
pub async fn get_table_schema_mysql(
    db: &DatabaseConnection,
    table_name: &str,
) -> Result<TableSchema, TableError> {
    const COLUMNS_SQL: &str = "
        SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, EXTRA
        FROM INFORMATION_SCHEMA.COLUMNS
        WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?
        ORDER BY ORDINAL_POSITION
    ";
    const INDEXES_SQL: &str = "
        SELECT INDEX_NAME, COLUMN_NAME, NON_UNIQUE, SEQ_IN_INDEX
        FROM INFORMATION_SCHEMA.STATISTICS
        WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?
        ORDER BY INDEX_NAME, SEQ_IN_INDEX
    ";

    let column_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::MySql,
            COLUMNS_SQL,
            [Value::from(table_name)],
        ))
        .await
        .map_err(|source| TableError::QuerySchemaFailed {
            table: table_name.to_string(),
            source,
        })?;

    let mut columns = Vec::with_capacity(column_rows.len());
    for row in &column_rows {
        // Rows that cannot be decoded are skipped, consistent with
        // `get_existing_tables`.
        let (Some(name), Some(col_type)) = (
            row.try_get_by_index::<String>(0).ok(),
            row.try_get_by_index::<String>(1).ok(),
        ) else {
            continue;
        };

        let nullable = row
            .try_get_by_index::<String>(2)
            .map(|v| v.eq_ignore_ascii_case("YES"))
            .unwrap_or(false);
        let default = row
            .try_get_by_index::<String>(3)
            .ok()
            .map(|value| unquote_literal(&value));
        let extra = row.try_get_by_index::<String>(4).unwrap_or_default();

        columns.push(ColumnSchema {
            name,
            col_type: normalize_mysql_type(&col_type),
            nullable,
            default,
            auto_increment: extra.contains("auto_increment"),
        });
    }

    let index_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::MySql,
            INDEXES_SQL,
            [Value::from(table_name)],
        ))
        .await
        .map_err(|source| TableError::QuerySchemaFailed {
            table: table_name.to_string(),
            source,
        })?;

    // Rows are ordered by index name and then by position within the index, so
    // consecutive rows that share an index name belong to the same index.
    let mut indexes: Vec<IndexSchema> = Vec::new();
    for row in &index_rows {
        let (Some(index_name), Some(column)) = (
            row.try_get_by_index::<String>(0).ok(),
            row.try_get_by_index::<String>(1).ok(),
        ) else {
            continue;
        };

        // `NON_UNIQUE` is reported as `int` on MySQL 8 but as `bigint` on some
        // 5.x builds, so decode it as i64 to stay compatible with both.
        let non_unique = row.try_get_by_index::<i64>(2).unwrap_or(1);

        match indexes.last_mut() {
            Some(current) if current.name == index_name => current.columns.push(column),
            _ => indexes.push(IndexSchema {
                primary: index_name == "PRIMARY",
                unique: non_unique == 0,
                name: index_name,
                columns: vec![column],
            }),
        }
    }

    Ok(TableSchema {
        name: table_name.to_string(),
        columns,
        indexes,
    })
}
/// Integer types whose trailing display width carries no meaning
///
/// MySQL reports these as `int(11)`, `bigint(20)` and so on, while sea-query
/// emits plain `int` / `bigint`.
const INT_TYPES_WITH_DISPLAY_WIDTH: [&str; 6] = [
    "tinyint",
    "smallint",
    "mediumint",
    "int",
    "integer",
    "bigint",
];

/// Normalizes a MySQL `COLUMN_TYPE` so it can be compared against the type
/// that sea-query emits for the same Rust field
///
/// The direction matters: the goal is to rewrite what MySQL *reports* into
/// what sea-query *generates*, so that both sides converge on one spelling.
///
/// - `tinyint(1)` -> `bool`, because `BOOL` is only a MySQL alias for
///   `TINYINT(1)` and sea-query emits `bool`
/// - `decimal(10,0)` -> `decimal`, because MySQL always reports the default
///   precision while sea-query emits the bare type
/// - drops the display width of integer types (`int(11)` -> `int`)
/// - lowercases the value and sorts trailing modifiers, so `int(10) unsigned
///   zerofill` and `int zerofill unsigned` compare equal
///
/// Lengths and precisions that carry meaning are preserved, e.g.
/// `varchar(255)` and `decimal(10,2)`.
pub fn normalize_mysql_type(raw: &str) -> String {
    let lowered = raw.trim().to_ascii_lowercase();
    let mut parts = lowered.split_whitespace();

    let Some(base) = parts.next() else {
        return String::new();
    };

    let mut modifiers: Vec<&str> = parts.collect();
    modifiers.sort_unstable();

    let base = match base {
        "tinyint(1)" | "boolean" => "bool",
        "decimal(10,0)" => "decimal",
        "numeric(10,0)" => "numeric",
        other => strip_int_display_width(other),
    };

    let mut out = base.to_string();
    for modifier in modifiers {
        out.push(' ');
        out.push_str(modifier);
    }
    out
}

/// Removes the display width from an integer type, e.g. `int(11)` -> `int`
fn strip_int_display_width(base: &str) -> &str {
    for int_type in INT_TYPES_WITH_DISPLAY_WIDTH {
        let Some(rest) = base.strip_prefix(int_type) else {
            continue;
        };
        if rest.starts_with('(') && rest.ends_with(')') {
            return int_type;
        }
    }
    base
}
/// The database name is appended, so two applications sharing a MySQL instance
/// do not block each other.
const LOCK_NAME_PREFIX: &str = "auto-table-migration";

/// Takes MySQL's named lock, returning whether it was obtained
///
/// Must be called on the same connection that runs the migration, which the
/// surrounding transaction guarantees.
async fn acquire_mysql_lock(
    transaction: &DatabaseTransaction,
    timeout_secs: u32,
) -> Result<bool, TableError> {
    let sql = format!(
        "SELECT GET_LOCK(CONCAT('{LOCK_NAME_PREFIX}-', DATABASE()), {timeout_secs})"
    );

    let row = transaction
        .query_one_raw(Statement::from_string(DbBackend::MySql, sql))
        .await
        .map_err(|source| TableError::MigrationFailed {
            sql: "GET_LOCK".to_string(),
            source,
        })?;

    // 1 = obtained, 0 = timed out, NULL = an error occurred
    let obtained = row
        .and_then(|row| row.try_get_by_index::<i32>(0).ok())
        .unwrap_or(0);

    Ok(obtained == 1)
}

async fn release_mysql_lock(transaction: &DatabaseTransaction) -> Result<(), TableError> {
    let sql = format!("SELECT RELEASE_LOCK(CONCAT('{LOCK_NAME_PREFIX}-', DATABASE()))");

    transaction
        .execute_raw(Statement::from_string(DbBackend::MySql, sql))
        .await
        .map_err(|source| TableError::MigrationFailed {
            sql: "RELEASE_LOCK".to_string(),
            source,
        })?;

    Ok(())
}

/// SQLite has no named locks; its write lock already serialises writers
///
/// All this does is make a concurrent writer wait for its turn instead of
/// Builds the MySQL statements for a diff
///
/// The order is deliberate:
///
/// 1. drop indexes, so nothing still points at a column that is about to change
/// 2. drop columns
/// 3. add columns
/// 4. modify columns
/// 5. add indexes, so they are built on the final column definitions
fn mysql_statements(diff: &TableDiff) -> Vec<String> {
    let table = quote_mysql_identifier(&diff.table);
    let mut statements = Vec::new();

    for change in &diff.indexes {
        if let IndexChange::Drop(index) = change {
            statements.push(if index.primary {
                format!("ALTER TABLE {table} DROP PRIMARY KEY")
            } else {
                format!(
                    "ALTER TABLE {table} DROP INDEX {}",
                    quote_mysql_identifier(&index.name)
                )
            });
        }
    }

    for change in &diff.columns {
        if let ColumnChange::Drop { name } = change {
            statements.push(format!(
                "ALTER TABLE {table} DROP COLUMN {}",
                quote_mysql_identifier(name)
            ));
        }
    }

    for change in &diff.columns {
        if let ColumnChange::Add(column) = change {
            statements.push(format!(
                "ALTER TABLE {table} ADD COLUMN {}",
                mysql_column_definition(column)
            ));
        }
    }

    for change in &diff.columns {
        if let ColumnChange::Alter { to, .. } = change {
            statements.push(format!(
                "ALTER TABLE {table} MODIFY COLUMN {}",
                mysql_column_definition(to)
            ));
        }
    }

    for change in &diff.indexes {
        if let IndexChange::Add(index) = change {
            statements.push(mysql_add_index(&table, index));
        }
    }

    statements
}

/// Renders a column as it appears inside an `ALTER TABLE` statement
///
/// The type is emitted verbatim: it already comes from the statement sea-query
/// generated for this same backend, so it needs no translation.
fn mysql_column_definition(column: &ColumnSchema) -> String {
    let mut definition = format!(
        "{} {}",
        quote_mysql_identifier(&column.name),
        column.col_type
    );

    if !column.nullable {
        definition.push_str(" NOT NULL");
    }
    if let Some(default) = &column.default {
        definition.push_str(&format!(" DEFAULT {}", quote_mysql_literal(default)));
    }
    if column.auto_increment {
        definition.push_str(" AUTO_INCREMENT");
    }

    definition
}

fn mysql_add_index(table: &str, index: &IndexSchema) -> String {
    let columns = index
        .columns
        .iter()
        .map(|column| quote_mysql_identifier(column))
        .collect::<Vec<_>>()
        .join(", ");

    if index.primary {
        format!("ALTER TABLE {table} ADD PRIMARY KEY ({columns})")
    } else if index.unique {
        format!(
            "ALTER TABLE {table} ADD UNIQUE INDEX {} ({columns})",
            quote_mysql_identifier(&index.name)
        )
    } else {
        format!(
            "ALTER TABLE {table} ADD INDEX {} ({columns})",
            quote_mysql_identifier(&index.name)
        )
    }
}

/// Quotes an identifier with backticks, MySQL style
fn quote_mysql_identifier(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// Quotes a literal, escaping the quotes it contains
fn quote_mysql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
