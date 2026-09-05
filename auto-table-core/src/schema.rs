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

use sea_orm::DatabaseConnection;

use crate::backend::AnyBackend;
use crate::{Backend, TableError};

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
/// The work is done by the backend of the connection, through the [`Backend`]
/// trait, so this stays free of anything backend-specific.
pub async fn get_table_schema(
    db: &DatabaseConnection,
    table_name: &str,
) -> Result<TableSchema, TableError> {
    AnyBackend::for_connection(db)?.read_table(db, table_name).await
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
    use crate::backend::mysql::normalize_mysql_type;
    use crate::backend::sqlite::sqlite_type_affinity;

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
