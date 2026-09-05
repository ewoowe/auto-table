//! Backend-specific code
//!
//! Every backend lives in a module named after the database it implements, and
//! each of them implements the [`Backend`] trait. The rest of the crate only
//! ever talks to a backend through that trait, so supporting another database
//! means adding a module here and nothing else: reading and diffing structures,
//! planning statements and executing them need no changes.

mod mysql;
mod postgres;
mod sqlite;

pub use mysql::MySql;
pub use postgres::Postgres;
pub use sqlite::Sqlite;

use sea_orm::{DatabaseConnection, DatabaseTransaction, DbBackend};

use crate::diff::TableDiff;
use crate::migrate::TableMigration;
use crate::schema::TableSchema;
use crate::{Backend, TableError};

/// Any of the supported backends
///
/// A `DbBackend` is a plain enum that carries no behaviour, so this turns it
/// into something that can do the work. It is an enum rather than a
/// `Box<dyn Backend>` because [`Backend`] has async methods, which do not sit
/// behind a trait object without reaching for a macro.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnyBackend {
    /// MySQL
    MySql(MySql),
    /// PostgreSQL
    Postgres(Postgres),
    /// SQLite
    Sqlite(Sqlite),
}

impl AnyBackend {
    /// The backend of a connection
    pub fn for_connection(db: &DatabaseConnection) -> Result<Self, TableError> {
        Self::for_backend(db.get_database_backend())
    }

    /// The backend matching a `DbBackend`
    pub fn for_backend(backend: DbBackend) -> Result<Self, TableError> {
        match backend {
            DbBackend::MySql => Ok(Self::MySql(MySql)),
            DbBackend::Postgres => Ok(Self::Postgres(Postgres)),
            DbBackend::Sqlite => Ok(Self::Sqlite(Sqlite)),
            other => Err(TableError::UnsupportedBackend(other)),
        }
    }
}

impl Backend for AnyBackend {
    async fn read_table(
        &self,
        db: &DatabaseConnection,
        table: &str,
    ) -> Result<TableSchema, TableError> {
        match self {
            Self::MySql(inner) => inner.read_table(db, table).await,
            Self::Postgres(inner) => inner.read_table(db, table).await,
            Self::Sqlite(inner) => inner.read_table(db, table).await,
        }
    }

    fn normalize_expected(&self, schema: &mut TableSchema) {
        match self {
            Self::MySql(inner) => inner.normalize_expected(schema),
            Self::Postgres(inner) => inner.normalize_expected(schema),
            Self::Sqlite(inner) => inner.normalize_expected(schema),
        }
    }

    fn plan(&self, diff: &TableDiff, create_sql: &str) -> Result<TableMigration, TableError> {
        match self {
            Self::MySql(inner) => inner.plan(diff, create_sql),
            Self::Postgres(inner) => inner.plan(diff, create_sql),
            Self::Sqlite(inner) => inner.plan(diff, create_sql),
        }
    }

    async fn acquire_lock(
        &self,
        transaction: &DatabaseTransaction,
        timeout_secs: u32,
    ) -> Result<bool, TableError> {
        match self {
            Self::MySql(inner) => inner.acquire_lock(transaction, timeout_secs).await,
            Self::Postgres(inner) => inner.acquire_lock(transaction, timeout_secs).await,
            Self::Sqlite(inner) => inner.acquire_lock(transaction, timeout_secs).await,
        }
    }

    async fn release_lock(
        &self,
        transaction: &DatabaseTransaction,
    ) -> Result<(), TableError> {
        match self {
            Self::MySql(inner) => inner.release_lock(transaction).await,
            Self::Postgres(inner) => inner.release_lock(transaction).await,
            Self::Sqlite(inner) => inner.release_lock(transaction).await,
        }
    }

    fn before_statements(&self) -> &[&str] {
        match self {
            Self::MySql(inner) => inner.before_statements(),
            Self::Postgres(inner) => inner.before_statements(),
            Self::Sqlite(inner) => inner.before_statements(),
        }
    }

    fn after_statements(&self) -> &[&str] {
        match self {
            Self::MySql(inner) => inner.after_statements(),
            Self::Postgres(inner) => inner.after_statements(),
            Self::Sqlite(inner) => inner.after_statements(),
        }
    }
}

/// The backend implementing a given `DbBackend`
pub fn backend_for(backend: DbBackend) -> Result<AnyBackend, TableError> {
    AnyBackend::for_backend(backend)
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use sea_orm::DbBackend;

    use crate::diff::{ColumnChange, IndexChange, TableDiff};
    use crate::migrate::plan_table_statements;
    use crate::schema::ColumnSchema;

    pub fn column(name: &str, col_type: &str) -> ColumnSchema {
        ColumnSchema {
            name: name.to_string(),
            col_type: col_type.to_string(),
            nullable: true,
            default: None,
            auto_increment: false,
        }
    }

    pub fn diff(columns: Vec<ColumnChange>, indexes: Vec<IndexChange>) -> TableDiff {
        TableDiff {
            table: "users".to_string(),
            columns,
            indexes,
        }
    }

    pub fn mysql_plan(columns: Vec<ColumnChange>, indexes: Vec<IndexChange>) -> Vec<String> {
        plan_table_statements(&diff(columns, indexes), DbBackend::MySql).expect("mysql is supported")
    }

    pub fn sqlite_plan(columns: Vec<ColumnChange>, indexes: Vec<IndexChange>) -> Vec<String> {
        plan_table_statements(&diff(columns, indexes), DbBackend::Sqlite).expect("sqlite is supported")
    }

    pub fn postgres_plan(columns: Vec<ColumnChange>, indexes: Vec<IndexChange>) -> Vec<String> {
        plan_table_statements(&diff(columns, indexes), DbBackend::Postgres).expect("postgres is supported")
    }
}
