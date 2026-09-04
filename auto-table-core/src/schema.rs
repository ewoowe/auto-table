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

use crate::parse::PRIMARY_INDEX_NAME;
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
        DbBackend::Sqlite => get_table_schema_sqlite(db, table_name).await,
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

/// Strips the quotes around a literal default value
///
/// MySQL 8 reports `DEFAULT 'member'` as `member`, while some 5.x builds keep
/// the surrounding quotes; sea-query always emits the quoted literal. Without
/// this, every string default would show up as a spurious difference.
pub(crate) fn unquote_literal(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        return s[1..s.len() - 1].to_string();
    }
    s.to_string()
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
    fn maps_tinyint_one_to_bool() {
        // MySQL stores booleans as tinyint(1) while sea-query emits `bool`
        assert_eq!(normalize_mysql_type("tinyint(1)"), "bool");
        assert_eq!(normalize_mysql_type("TINYINT(1)"), "bool");
        assert_eq!(normalize_mysql_type("boolean"), "bool");
        // A genuine tinyint keeps its own identity
        assert_eq!(normalize_mysql_type("tinyint(4)"), "tinyint");
    }

    #[test]
    fn drops_default_decimal_precision() {
        // MySQL always reports the default precision, sea-query omits it
        assert_eq!(normalize_mysql_type("decimal(10,0)"), "decimal");
        assert_eq!(normalize_mysql_type("numeric(10,0)"), "numeric");
        // An explicit precision is meaningful and must be preserved
        assert_eq!(normalize_mysql_type("decimal(10,2)"), "decimal(10,2)");
    }

    #[test]
    fn preserves_meaningful_lengths() {
        assert_eq!(normalize_mysql_type("varchar(255)"), "varchar(255)");
        assert_eq!(normalize_mysql_type("char(32)"), "char(32)");
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

    #[test]
    fn sqlite_affinity_groups_integer_spellings() {
        // int / integer / bigint all have INTEGER affinity and store the same
        // values, which is exactly why `i32` -> `i64` must not look like a change
        for declared in ["integer", "int", "bigint", "INTEGER", "Int"] {
            assert_eq!(sqlite_type_affinity(declared), "INTEGER", "{declared}");
        }
    }

    #[test]
    fn sqlite_affinity_follows_the_documented_rules() {
        assert_eq!(sqlite_type_affinity("varchar(255)"), "TEXT");
        assert_eq!(sqlite_type_affinity("char(32)"), "TEXT");
        assert_eq!(sqlite_type_affinity("clob"), "TEXT");
        assert_eq!(sqlite_type_affinity("timestamp_with_timezone_text"), "TEXT");

        assert_eq!(sqlite_type_affinity("blob"), "BLOB");
        assert_eq!(sqlite_type_affinity(""), "BLOB");

        assert_eq!(sqlite_type_affinity("real"), "REAL");
        assert_eq!(sqlite_type_affinity("double"), "REAL");
        assert_eq!(sqlite_type_affinity("real_decimal"), "REAL");

        // Anything else is NUMERIC
        assert_eq!(sqlite_type_affinity("boolean"), "NUMERIC");
        assert_eq!(sqlite_type_affinity("decimal"), "NUMERIC");
    }

    #[test]
    fn widening_i32_to_i64_is_not_a_difference_on_sqlite() {
        // Both sides normalize to the same affinity, so no rebuild is triggered
        assert_eq!(sqlite_type_affinity("int"), sqlite_type_affinity("bigint"));
        assert_eq!(
            sqlite_type_affinity("varchar(50)"),
            sqlite_type_affinity("varchar(255)")
        );
    }
}
