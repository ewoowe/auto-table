//! PostgreSQL backend
//!
//! Reading structures from `information_schema` and the `pg_*` catalogues,
//! emitting one `ALTER COLUMN` clause per changed aspect, and taking the
//! advisory lock.

use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, Statement, Value,
};

use crate::diff::{ColumnAspect, ColumnChange, IndexChange, TableDiff};
use crate::migrate::{execute, TableMigration};
use crate::parse::PRIMARY_INDEX_NAME;
use crate::schema::{ColumnSchema, IndexSchema, TableSchema};
use crate::{Backend, TableError};

/// The PostgreSQL backend
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Postgres;

impl Backend for Postgres {
    async fn read_table(
        &self,
        db: &DatabaseConnection,
        table: &str,
    ) -> Result<TableSchema, TableError> {
        get_table_schema_postgres(db, table).await
    }

    fn plan(&self, diff: &TableDiff, _create_sql: &str) -> Result<TableMigration, TableError> {
        Ok(TableMigration::new(
            diff.table.clone(),
            postgres_statements(diff),
        ))
    }

    async fn acquire_lock(
        &self,
        transaction: &DatabaseTransaction,
        timeout_secs: u32,
    ) -> Result<bool, TableError> {
        acquire_postgres_lock(transaction, timeout_secs).await
    }

    async fn release_lock(&self, transaction: &DatabaseTransaction) -> Result<(), TableError> {
        release_postgres_lock(transaction).await
    }
}

/// Reads the current structure of `table_name` from a PostgreSQL database
///
/// Two things are handled here that make PostgreSQL differ from MySQL:
///
/// - `information_schema` reports its own spelling for several types, so they
///   are rewritten to what sea-query emits (see [`normalize_postgres_type`]).
/// - Constraint-backed indexes are named by PostgreSQL itself, as
///   `<table>_pkey` and `<table>_<column>_key`. They are reported under a
///   logical name instead, so they compare equal to what parsing the entity
///   produces; the physical name is only needed when emitting DDL.
pub async fn get_table_schema_postgres(
    db: &DatabaseConnection,
    table_name: &str,
) -> Result<TableSchema, TableError> {
    const COLUMNS_SQL: &str = "
        SELECT column_name, data_type, is_nullable, column_default, is_identity
        FROM information_schema.columns
        WHERE table_schema = current_schema() AND table_name = $1
        ORDER BY ordinal_position
    ";
    const INDEXES_SQL: &str = "
        SELECT i.relname AS index_name,
               a.attname AS column_name,
               ix.indisunique AS is_unique,
               ix.indisprimary AS is_primary,
               (c.contype = 'u') AS is_unique_constraint,
               array_position(ix.indkey, a.attnum) AS ordinal
        FROM pg_class t
        JOIN pg_index ix ON t.oid = ix.indrelid
        JOIN pg_class i ON i.oid = ix.indexrelid
        JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = ANY(ix.indkey)
        LEFT JOIN pg_constraint c ON c.conindid = i.oid
        WHERE t.relname = $1 AND t.relnamespace = current_schema()::regnamespace
        ORDER BY i.relname, ordinal
    ";

    let fail = |source| TableError::QuerySchemaFailed {
        table: table_name.to_string(),
        source,
    };

    let column_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            COLUMNS_SQL,
            [Value::from(table_name)],
        ))
        .await
        .map_err(fail)?;

    let mut columns = Vec::with_capacity(column_rows.len());
    for row in &column_rows {
        let (Some(name), Some(col_type)) = (
            row.try_get_by_index::<String>(0).ok(),
            row.try_get_by_index::<String>(1).ok(),
        ) else {
            continue;
        };

        let nullable = row
            .try_get_by_index::<String>(2)
            .map(|value| value.eq_ignore_ascii_case("YES"))
            .unwrap_or(false);
        let default = row
            .try_get_by_index::<String>(3)
            .ok()
            .and_then(|value| normalize_postgres_default(&value));
        let identity = row
            .try_get_by_index::<String>(4)
            .map(|value| value.eq_ignore_ascii_case("YES"))
            .unwrap_or(false);

        columns.push(ColumnSchema {
            name,
            col_type: normalize_postgres_type(&col_type),
            nullable,
            default,
            auto_increment: identity,
        });
    }

    let index_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            INDEXES_SQL,
            [Value::from(table_name)],
        ))
        .await
        .map_err(fail)?;

    // The query orders by index name and then by position within the index, so
    // consecutive rows sharing a name belong to the same index.
    let mut grouped: Vec<(String, Vec<String>, bool, bool, bool)> = Vec::new();
    for row in &index_rows {
        let (Some(index_name), Some(column)) = (
            row.try_get_by_index::<String>(0).ok(),
            row.try_get_by_index::<String>(1).ok(),
        ) else {
            continue;
        };

        let unique = row.try_get_by_index::<bool>(2).unwrap_or(false);
        let primary = row.try_get_by_index::<bool>(3).unwrap_or(false);
        let is_unique_constraint = row.try_get_by_index::<bool>(4).unwrap_or(false);

        match grouped.last_mut() {
            Some(group) if group.0 == index_name => group.1.push(column),
            _ => grouped.push((
                index_name,
                vec![column],
                unique,
                primary,
                is_unique_constraint,
            )),
        }
    }

    let mut indexes = Vec::with_capacity(grouped.len());
    for (index_name, columns, unique, primary, is_unique_constraint) in grouped {
        let name = if primary {
            PRIMARY_INDEX_NAME.to_string()
        } else if is_unique_constraint {
            // Match the name parsing the entity's CREATE TABLE produces
            columns.join("_")
        } else {
            index_name
        };

        indexes.push(IndexSchema {
            name,
            columns,
            unique,
            primary,
        });
    }

    Ok(TableSchema {
        name: table_name.to_string(),
        columns,
        indexes,
    })
}

/// Reduces a PostgreSQL `column_default` to the value it holds
///
/// PostgreSQL returns the default as an expression annotated with its type, so
/// a `DEFAULT 'member'` comes back as `'member'::character varying`. Left
/// untouched it would never compare equal to the value the entity declares.
///
/// Defaults that are not plain values, such as the `nextval(...)` behind a
/// serial column, are reported as absent: they are generated by the database
/// rather than declared by the entity.
fn normalize_postgres_default(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }

    // Drop the `::type` annotation
    let value = match value.find("::") {
        Some(position) => value[..position].trim(),
        None => value,
    };

    if value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2 {
        let inner = &value[1..value.len() - 1];
        return Some(inner.replace("''", "'"));
    }

    // An expression rather than a value: `nextval(...)`, `now()`, and friends
    if value.contains('(') || value.contains(' ') {
        return None;
    }

    Some(value.to_string())
}

/// Rewrites a PostgreSQL type into the spelling sea-query emits
///
/// PostgreSQL reports `varchar` as `character varying`, stores `decimal` as
/// `numeric` and spells `bool` as `boolean`. Left alone, every string, decimal
/// and boolean column would show up as a difference on every run.
pub fn normalize_postgres_type(reported: &str) -> String {
    match reported.trim().to_ascii_lowercase().as_str() {
        "character varying" | "varchar" => "varchar".to_string(),
        "numeric" | "decimal" => "decimal".to_string(),
        "boolean" | "bool" => "bool".to_string(),
        other => other.to_string(),
    }
}
/// MySQL there is nothing to disambiguate between schemas on one server.
const ADVISORY_LOCK_KEY: i64 = 0x6175_746f_7462_6c65;

/// Takes PostgreSQL's advisory lock
///
/// Like MySQL's `GET_LOCK` it is session scoped, so it is taken on the
/// transaction that runs the migration.
async fn acquire_postgres_lock(
    transaction: &DatabaseTransaction,
    timeout_secs: u32,
) -> Result<bool, TableError> {
    if timeout_secs == 0 {
        // Report straight away whether the lock was free
        let row = transaction
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT pg_try_advisory_lock({ADVISORY_LOCK_KEY})"),
            ))
            .await
            .map_err(|source| TableError::MigrationFailed {
                sql: "pg_try_advisory_lock".to_string(),
                source,
            })?;

        return Ok(row
            .and_then(|row| row.try_get_by_index::<bool>(0).ok())
            .unwrap_or(false));
    }

    // Wait, but not forever. A lock that times out raises an error instead of
    // returning false, so an error here is reported as "not acquired".
    execute(
        transaction,
        &format!("SET LOCAL lock_timeout = '{timeout_secs}s'"),
    )
    .await?;

    let locked = transaction
        .execute_raw(Statement::from_string(
            DbBackend::Postgres,
            format!("SELECT pg_advisory_lock({ADVISORY_LOCK_KEY})"),
        ))
        .await;

    Ok(locked.is_ok())
}

async fn release_postgres_lock(transaction: &DatabaseTransaction) -> Result<(), TableError> {
    transaction
        .execute_raw(Statement::from_string(
            DbBackend::Postgres,
            format!("SELECT pg_advisory_unlock({ADVISORY_LOCK_KEY})"),
        ))
        .await
        .map_err(|source| TableError::MigrationFailed {
            sql: "pg_advisory_unlock".to_string(),
            source,
        })?;
    Ok(())
}
/// Builds the PostgreSQL statements for a diff
///
/// The order follows the MySQL one — drop indexes, drop columns, add columns,
/// change columns, add indexes — but two things differ:
///
/// - A column change is **one clause per aspect** rather than a single
///   `MODIFY COLUMN`: type, nullability, default and identity each get their own
///   `ALTER COLUMN` statement.
/// - Indexes backed by a constraint are added and dropped as constraints.
///   PostgreSQL refuses to drop such an index with `DROP INDEX`, because the
///   constraint depends on it.
fn postgres_statements(diff: &TableDiff) -> Vec<String> {
    let table = quote_postgres_identifier(&diff.table);
    let mut statements = Vec::new();

    for change in &diff.indexes {
        if let IndexChange::Drop(index) = change {
            statements.push(postgres_drop_index(&diff.table, index));
        }
    }

    for change in &diff.columns {
        if let ColumnChange::Drop { name } = change {
            statements.push(format!(
                "ALTER TABLE {table} DROP COLUMN {}",
                quote_postgres_identifier(name)
            ));
        }
    }

    for change in &diff.columns {
        if let ColumnChange::Add(column) = change {
            statements.push(format!(
                "ALTER TABLE {table} ADD COLUMN {}",
                postgres_column_definition(column)
            ));
        }
    }

    for change in &diff.columns {
        if let ColumnChange::Alter { name, aspects, .. } = change {
            let column = quote_postgres_identifier(name);
            for aspect in aspects {
                statements.push(match aspect {
                    ColumnAspect::Type { to, .. } => {
                        format!("ALTER TABLE {table} ALTER COLUMN {column} TYPE {to}")
                    }
                    ColumnAspect::Nullable { to, .. } => {
                        let action = if *to { "DROP" } else { "SET" };
                        format!("ALTER TABLE {table} ALTER COLUMN {column} {action} NOT NULL")
                    }
                    ColumnAspect::Default { to, .. } => match to {
                        Some(value) => format!(
                            "ALTER TABLE {table} ALTER COLUMN {column} SET DEFAULT {}",
                            quote_postgres_literal(value)
                        ),
                        None => {
                            format!("ALTER TABLE {table} ALTER COLUMN {column} DROP DEFAULT")
                        }
                    },
                    ColumnAspect::AutoIncrement { to, .. } => {
                        let action = if *to { "ADD" } else { "DROP" };
                        format!(
                            "ALTER TABLE {table} ALTER COLUMN {column} {action} GENERATED BY DEFAULT AS IDENTITY"
                        )
                    }
                });
            }
        }
    }

    for change in &diff.indexes {
        if let IndexChange::Add(index) = change {
            statements.push(postgres_add_index(&diff.table, index));
        }
    }

    statements
}

fn postgres_column_definition(column: &ColumnSchema) -> String {
    let mut definition = format!(
        "{} {}",
        quote_postgres_identifier(&column.name),
        column.col_type
    );

    if !column.nullable {
        definition.push_str(" NOT NULL");
    }
    if let Some(default) = &column.default {
        definition.push_str(&format!(
            " DEFAULT {}",
            quote_postgres_literal(default)
        ));
    }

    definition
}

fn postgres_add_index(table_name: &str, index: &IndexSchema) -> String {
    let table = quote_postgres_identifier(table_name);
    let columns = postgres_column_list(&index.columns);

    if index.primary {
        format!("ALTER TABLE {table} ADD PRIMARY KEY ({columns})")
    } else if index.unique {
        let constraint = quote_postgres_identifier(&postgres_unique_constraint_name(
            table_name,
            &index.columns,
        ));
        format!("ALTER TABLE {table} ADD CONSTRAINT {constraint} UNIQUE ({columns})")
    } else {
        let name = quote_postgres_identifier(&index.name);
        format!("CREATE INDEX {name} ON {table} ({columns})")
    }
}

fn postgres_drop_index(table_name: &str, index: &IndexSchema) -> String {
    let table = quote_postgres_identifier(table_name);

    if index.primary {
        // PostgreSQL names the primary key constraint <table>_pkey
        let constraint = quote_postgres_identifier(&format!("{table_name}_pkey"));
        format!("ALTER TABLE {table} DROP CONSTRAINT {constraint}")
    } else if index.unique {
        let constraint = quote_postgres_identifier(&postgres_unique_constraint_name(
            table_name,
            &index.columns,
        ));
        format!("ALTER TABLE {table} DROP CONSTRAINT {constraint}")
    } else {
        // A plain index, with no constraint behind it
        format!("DROP INDEX {}", quote_postgres_identifier(&index.name))
    }
}

/// The name PostgreSQL gives a unique constraint: `<table>_<columns>_key`
fn postgres_unique_constraint_name(table_name: &str, columns: &[String]) -> String {
    format!("{table_name}_{}_key", columns.join("_"))
}

fn postgres_column_list(columns: &[String]) -> String {
    columns
        .iter()
        .map(|column| quote_postgres_identifier(column))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Quotes an identifier, PostgreSQL style
fn quote_postgres_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn quote_postgres_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
#[cfg(test)]
mod tests {
    use crate::backend::test_helpers::*;
    use crate::diff::{ColumnAspect, ColumnChange, IndexChange};
    use crate::parse::PRIMARY_INDEX_NAME;
    use crate::schema::{ColumnSchema, IndexSchema};

    #[test]
    fn postgres_changes_one_aspect_per_statement() {
        // Unlike MySQL, which folds everything into one MODIFY COLUMN
        let target = ColumnSchema {
            name: "age".to_string(),
            col_type: "bigint".to_string(),
            nullable: false,
            default: Some("0".to_string()),
            auto_increment: false,
        };

        assert_eq!(
            postgres_plan(
                vec![ColumnChange::Alter {
                    name: "age".to_string(),
                    to: target,
                    aspects: vec![
                        ColumnAspect::Type {
                            from: "integer".to_string(),
                            to: "bigint".to_string(),
                        },
                        ColumnAspect::Nullable {
                            from: true,
                            to: false,
                        },
                        ColumnAspect::Default {
                            from: None,
                            to: Some("0".to_string()),
                        },
                    ],
                }],
                vec![],
            ),
            vec![
                "ALTER TABLE \"users\" ALTER COLUMN \"age\" TYPE bigint",
                "ALTER TABLE \"users\" ALTER COLUMN \"age\" SET NOT NULL",
                "ALTER TABLE \"users\" ALTER COLUMN \"age\" SET DEFAULT '0'",
            ]
        );
    }

    #[test]
    fn postgres_adds_and_drops_columns() {
        assert_eq!(
            postgres_plan(
                vec![
                    ColumnChange::Add(column("bio", "varchar")),
                    ColumnChange::Drop {
                        name: "legacy".to_string()
                    },
                ],
                vec![],
            ),
            vec![
                "ALTER TABLE \"users\" DROP COLUMN \"legacy\"",
                "ALTER TABLE \"users\" ADD COLUMN \"bio\" varchar",
            ]
        );
    }

    #[test]
    fn postgres_drops_a_unique_constraint_not_its_index() {
        // PostgreSQL refuses to drop an index that a constraint depends on
        assert_eq!(
            postgres_plan(
                vec![],
                vec![IndexChange::Drop(IndexSchema {
                    name: "email".to_string(),
                    columns: vec!["email".to_string()],
                    unique: true,
                    primary: false,
                })],
            ),
            vec!["ALTER TABLE \"users\" DROP CONSTRAINT \"users_email_key\""]
        );
    }

    #[test]
    fn postgres_adds_a_unique_constraint() {
        assert_eq!(
            postgres_plan(
                vec![],
                vec![IndexChange::Add(IndexSchema {
                    name: "email".to_string(),
                    columns: vec!["email".to_string()],
                    unique: true,
                    primary: false,
                })],
            ),
            vec!["ALTER TABLE \"users\" ADD CONSTRAINT \"users_email_key\" UNIQUE (\"email\")"]
        );
    }

    #[test]
    fn postgres_uses_its_own_primary_key_constraint_name() {
        assert_eq!(
            postgres_plan(
                vec![],
                vec![IndexChange::Drop(IndexSchema {
                    name: PRIMARY_INDEX_NAME.to_string(),
                    columns: vec!["id".to_string()],
                    unique: true,
                    primary: true,
                })],
            ),
            vec!["ALTER TABLE \"users\" DROP CONSTRAINT \"users_pkey\""]
        );
    }

    #[test]
    fn postgres_escapes_quotes_in_identifiers_and_literals() {
        assert_eq!(
            postgres_plan(
                vec![ColumnChange::Add(ColumnSchema {
                    name: "we\"ird".to_string(),
                    col_type: "varchar".to_string(),
                    nullable: false,
                    default: Some("it's".to_string()),
                    auto_increment: false,
                })],
                vec![],
            ),
            vec![
                "ALTER TABLE \"users\" ADD COLUMN \"we\"\"ird\" varchar NOT NULL DEFAULT 'it''s'"
            ]
        );
    }

}
