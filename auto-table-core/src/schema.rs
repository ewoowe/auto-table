//! Structure snapshots of existing tables
//!
//! Reads the *current* structure of a table back out of the database. This is
//! the "actual" side of a schema diff: a future migration compares the
//! structure defined by the entities against the structure reported here.
//!
//! Snapshots are **normalized** so that values coming from different backends
//! (or from sea-query) can be compared directly. Types are kept as normalized
//! strings rather than an enum, which keeps the initial implementation small;
//! the normalization rules live next to the reader of each backend.

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement, Value};

use crate::TableError;

/// Normalized snapshot of a single column
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSchema {
    /// Column name
    pub name: String,
    /// Normalized column type, e.g. `varchar(255)`, `int`, `datetime`
    pub col_type: String,
    /// Whether the column accepts `NULL`
    pub nullable: bool,
    /// Default value as reported by the database, if any
    pub default: Option<String>,
    /// Whether the column is auto-incremented
    pub auto_increment: bool,
}

/// Normalized snapshot of a single index
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSchema {
    /// Index name
    pub name: String,
    /// Indexed columns, in index order
    pub columns: Vec<String>,
    /// Whether the index enforces uniqueness
    pub unique: bool,
    /// Whether this is the primary key.
    ///
    /// MySQL reports the primary key as an index literally named `PRIMARY`.
    pub primary: bool,
}

/// Normalized snapshot of a table
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableSchema {
    /// Table name
    pub name: String,
    /// Columns, in declaration order
    pub columns: Vec<ColumnSchema>,
    /// Indexes defined on the table, including the primary key
    pub indexes: Vec<IndexSchema>,
}

/// Reads the current structure of `table_name` from the database
///
/// Dispatches on the connection's backend. Returns
/// [`TableError::UnsupportedBackend`] for backends that do not have a reader
/// yet.
pub async fn get_table_schema(
    db: &DatabaseConnection,
    table_name: &str,
) -> Result<TableSchema, TableError> {
    match db.get_database_backend() {
        DbBackend::MySql => get_table_schema_mysql(db, table_name).await,
        other => Err(TableError::UnsupportedBackend(other)),
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
        let default = row.try_get_by_index::<String>(3).ok();
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
/// emits plain `int` / `bigint`. `tinyint(1)` is deliberately excluded: it is
/// the conventional mapping of a boolean and SeaORM emits it as-is.
const INT_TYPES_WITH_DISPLAY_WIDTH: [&str; 6] = [
    "tinyint",
    "smallint",
    "mediumint",
    "int",
    "integer",
    "bigint",
];

/// Normalizes a MySQL `COLUMN_TYPE` into a comparable form
///
/// - lowercases the value and collapses runs of whitespace
/// - drops the display width of integer types (`int(11)` -> `int`), except for
///   `tinyint(1)`
/// - sorts trailing modifiers so `int unsigned` and `int unsigned zerofill`
///   compare predictably
///
/// Lengths that carry meaning are preserved: `varchar(255)`, `decimal(10,2)`.
pub fn normalize_mysql_type(raw: &str) -> String {
    let lowered = raw.trim().to_ascii_lowercase();
    let mut parts = lowered.split_whitespace();

    let Some(base) = parts.next() else {
        return String::new();
    };

    let mut modifiers: Vec<&str> = parts.collect();
    modifiers.sort_unstable();

    let mut out = strip_int_display_width(base).to_string();
    for modifier in modifiers {
        out.push(' ');
        out.push_str(modifier);
    }
    out
}

/// Removes the display width from an integer type, e.g. `int(11)` -> `int`
///
/// `tinyint(1)` is left untouched because it is how MySQL represents booleans.
fn strip_int_display_width(base: &str) -> &str {
    for int_type in INT_TYPES_WITH_DISPLAY_WIDTH {
        let Some(rest) = base.strip_prefix(int_type) else {
            continue;
        };
        if !rest.starts_with('(') || !rest.ends_with(')') {
            continue;
        }
        // `tinyint(1)` is a boolean, not a display width
        if int_type == "tinyint" && rest == "(1)" {
            return base;
        }
        return int_type;
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercases_and_collapses_whitespace() {
        assert_eq!(normalize_mysql_type("  VARCHAR(255)  "), "varchar(255)");
        assert_eq!(normalize_mysql_type("datetime"), "datetime");
    }

    #[test]
    fn strips_integer_display_width() {
        assert_eq!(normalize_mysql_type("int(11)"), "int");
        assert_eq!(normalize_mysql_type("BIGINT(20)"), "bigint");
        assert_eq!(normalize_mysql_type("smallint(6)"), "smallint");
    }

    #[test]
    fn keeps_tinyint_one_because_it_is_a_boolean() {
        assert_eq!(normalize_mysql_type("tinyint(1)"), "tinyint(1)");
        assert_eq!(normalize_mysql_type("tinyint(4)"), "tinyint");
    }

    #[test]
    fn preserves_meaningful_lengths_and_precision() {
        assert_eq!(normalize_mysql_type("varchar(255)"), "varchar(255)");
        assert_eq!(normalize_mysql_type("char(32)"), "char(32)");
        assert_eq!(normalize_mysql_type("decimal(10,2)"), "decimal(10,2)");
    }

    #[test]
    fn sorts_trailing_modifiers() {
        assert_eq!(
            normalize_mysql_type("int(10) unsigned zerofill"),
            "int unsigned zerofill"
        );
        // Modifier order should not affect the result
        assert_eq!(
            normalize_mysql_type("int(10) zerofill unsigned"),
            "int unsigned zerofill"
        );
    }

    #[test]
    fn handles_empty_input() {
        assert_eq!(normalize_mysql_type(""), "");
        assert_eq!(normalize_mysql_type("   "), "");
    }
}
