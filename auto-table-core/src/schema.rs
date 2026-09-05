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
