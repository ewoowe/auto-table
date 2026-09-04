//! Plans and applies schema migrations
//!
//! [`plan_migrations`] compares every registered entity against the live
//! database and returns the statements that would bring it in sync. It never
//! executes anything, so it doubles as the dry-run: inspect
//! [`MigrationPlan::statements`] and only then call [`apply_migrations`].
//!
//! Only MySQL is supported for now. PostgreSQL uses a different ALTER syntax
//! and SQLite cannot alter a column at all (it needs the table rebuilt), so
//! both are reported as [`TableError::UnsupportedBackend`].

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};

use crate::diff::{diff_table, ColumnChange, IndexChange, TableDiff};
use crate::parse::{parse_create_table, PRIMARY_INDEX_NAME};
use crate::schema::{get_table_schema, ColumnSchema, IndexSchema};
use crate::{get_all_table_statements, get_existing_tables, get_table_name, TableError};

/// The statements needed to bring one table in sync
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableMigration {
    /// Name of the table
    pub table: String,
    /// Statements to run, in order
    pub statements: Vec<String>,
}

/// A complete migration plan across all registered tables
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationPlan {
    /// One entry per table that needs to change
    pub tables: Vec<TableMigration>,
}

impl MigrationPlan {
    /// Whether anything has to change at all
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// Every statement in the plan, in execution order
    pub fn statements(&self) -> Vec<&str> {
        self.tables
            .iter()
            .flat_map(|table| table.statements.iter())
            .map(String::as_str)
            .collect()
    }
}

/// Compares every registered table against the database and builds a plan
///
/// Tables that do not exist yet are skipped: creating them is the job of
/// [`crate::create_missing_tables`], not of a migration.
pub async fn plan_migrations(db: &DatabaseConnection) -> Result<MigrationPlan, TableError> {
    let backend = db.get_database_backend();
    let existing_tables = get_existing_tables(db, backend).await?;

    let mut tables = Vec::new();

    for statement in get_all_table_statements(backend) {
        let table_name = get_table_name(&statement).unwrap_or_else(|| "unknown".to_string());

        if !existing_tables.contains(&table_name) {
            continue;
        }

        let expected = parse_create_table(&backend.build(&statement).sql).map_err(|source| {
            TableError::ParseExpectedFailed {
                table: table_name,
                source,
            }
        })?;
        let actual = get_table_schema(db, &expected.name).await?;

        let diff = diff_table(&expected, &actual);
        if diff.is_empty() {
            continue;
        }

        tables.push(TableMigration {
            table: expected.name,
            statements: plan_table_statements(&diff, backend)?,
        });
    }

    Ok(MigrationPlan { tables })
}

/// Executes a plan, running its statements in order
pub async fn apply_migrations(
    db: &DatabaseConnection,
    plan: &MigrationPlan,
) -> Result<(), TableError> {
    let backend = db.get_database_backend();

    for table in &plan.tables {
        for sql in &table.statements {
            db.execute_raw(Statement::from_string(backend, sql.clone()))
                .await
                .map_err(|source| TableError::MigrationFailed {
                    sql: sql.clone(),
                    source,
                })?;
        }
    }

    Ok(())
}

/// Turns the diff of one table into the statements that apply it
pub fn plan_table_statements(
    diff: &TableDiff,
    backend: DbBackend,
) -> Result<Vec<String>, TableError> {
    match backend {
        DbBackend::MySql => Ok(mysql_statements(diff)),
        other => Err(TableError::UnsupportedBackend(other)),
    }
}

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
    let table = quote_identifier(&diff.table);
    let mut statements = Vec::new();

    for change in &diff.indexes {
        if let IndexChange::Drop { name } = change {
            statements.push(if name == PRIMARY_INDEX_NAME {
                format!("ALTER TABLE {table} DROP PRIMARY KEY")
            } else {
                format!(
                    "ALTER TABLE {table} DROP INDEX {}",
                    quote_identifier(name)
                )
            });
        }
    }

    for change in &diff.columns {
        if let ColumnChange::Drop { name } = change {
            statements.push(format!(
                "ALTER TABLE {table} DROP COLUMN {}",
                quote_identifier(name)
            ));
        }
    }

    for change in &diff.columns {
        if let ColumnChange::Add(column) = change {
            statements.push(format!(
                "ALTER TABLE {table} ADD COLUMN {}",
                column_definition(column)
            ));
        }
    }

    for change in &diff.columns {
        if let ColumnChange::Alter { to, .. } = change {
            statements.push(format!(
                "ALTER TABLE {table} MODIFY COLUMN {}",
                column_definition(to)
            ));
        }
    }

    for change in &diff.indexes {
        if let IndexChange::Add(index) = change {
            statements.push(add_index(&table, index));
        }
    }

    statements
}

/// Renders a column as it appears inside an `ALTER TABLE` statement
///
/// The type is emitted verbatim: it already comes from the statement sea-query
/// generated for this same backend, so it needs no translation.
fn column_definition(column: &ColumnSchema) -> String {
    let mut definition = format!(
        "{} {}",
        quote_identifier(&column.name),
        column.col_type
    );

    if !column.nullable {
        definition.push_str(" NOT NULL");
    }
    if let Some(default) = &column.default {
        definition.push_str(&format!(" DEFAULT {}", quote_literal(default)));
    }
    if column.auto_increment {
        definition.push_str(" AUTO_INCREMENT");
    }

    definition
}

fn add_index(table: &str, index: &IndexSchema) -> String {
    let columns = index
        .columns
        .iter()
        .map(|column| quote_identifier(column))
        .collect::<Vec<_>>()
        .join(", ");

    if index.primary {
        format!("ALTER TABLE {table} ADD PRIMARY KEY ({columns})")
    } else if index.unique {
        format!(
            "ALTER TABLE {table} ADD UNIQUE INDEX {} ({columns})",
            quote_identifier(&index.name)
        )
    } else {
        format!(
            "ALTER TABLE {table} ADD INDEX {} ({columns})",
            quote_identifier(&index.name)
        )
    }
}

/// Quotes an identifier with backticks, MySQL style
fn quote_identifier(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// Quotes a literal, escaping the quotes it contains
fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::ColumnAspect;

    fn column(name: &str, col_type: &str) -> ColumnSchema {
        ColumnSchema {
            name: name.to_string(),
            col_type: col_type.to_string(),
            nullable: true,
            default: None,
            auto_increment: false,
        }
    }

    fn diff(columns: Vec<ColumnChange>, indexes: Vec<IndexChange>) -> TableDiff {
        TableDiff {
            table: "users".to_string(),
            columns,
            indexes,
        }
    }

    fn plan(columns: Vec<ColumnChange>, indexes: Vec<IndexChange>) -> Vec<String> {
        plan_table_statements(&diff(columns, indexes), DbBackend::MySql).expect("mysql is supported")
    }

    #[test]
    fn drops_columns_before_adding_them() {
        let statements = plan(
            vec![
                ColumnChange::Add(column("email", "varchar(255)")),
                ColumnChange::Drop {
                    name: "legacy".to_string(),
                },
            ],
            vec![],
        );

        assert_eq!(
            statements,
            vec![
                "ALTER TABLE `users` DROP COLUMN `legacy`",
                "ALTER TABLE `users` ADD COLUMN `email` varchar(255)",
            ]
        );
    }

    #[test]
    fn modify_repeats_the_whole_definition() {
        // MySQL replaces the definition, so NOT NULL and DEFAULT must be restated
        // even though only the type changed.
        let target = ColumnSchema {
            name: "age".to_string(),
            col_type: "bigint".to_string(),
            nullable: false,
            default: Some("0".to_string()),
            auto_increment: false,
        };

        let statements = plan(
            vec![ColumnChange::Alter {
                name: "age".to_string(),
                to: target,
                aspects: vec![ColumnAspect::Type {
                    from: "int".to_string(),
                    to: "bigint".to_string(),
                }],
            }],
            vec![],
        );

        assert_eq!(
            statements,
            vec!["ALTER TABLE `users` MODIFY COLUMN `age` bigint NOT NULL DEFAULT '0'"]
        );
    }

    #[test]
    fn indexes_are_dropped_first_and_added_last() {
        let statements = plan(
            vec![ColumnChange::Drop {
                name: "legacy".to_string(),
            }],
            vec![
                IndexChange::Drop {
                    name: "email".to_string(),
                },
                IndexChange::Add(IndexSchema {
                    name: "email".to_string(),
                    columns: vec!["email".to_string()],
                    unique: true,
                    primary: false,
                }),
            ],
        );

        assert_eq!(
            statements,
            vec![
                "ALTER TABLE `users` DROP INDEX `email`",
                "ALTER TABLE `users` DROP COLUMN `legacy`",
                "ALTER TABLE `users` ADD UNIQUE INDEX `email` (`email`)",
            ]
        );
    }

    #[test]
    fn the_primary_key_uses_its_own_syntax() {
        let statements = plan(
            vec![],
            vec![
                IndexChange::Add(IndexSchema {
                    name: PRIMARY_INDEX_NAME.to_string(),
                    columns: vec!["id".to_string()],
                    unique: true,
                    primary: true,
                }),
                IndexChange::Drop {
                    name: PRIMARY_INDEX_NAME.to_string(),
                },
            ],
        );

        assert_eq!(
            statements,
            vec![
                "ALTER TABLE `users` DROP PRIMARY KEY",
                "ALTER TABLE `users` ADD PRIMARY KEY (`id`)",
            ]
        );
    }

    #[test]
    fn escapes_quotes_in_identifiers_and_literals() {
        let statements = plan(
            vec![ColumnChange::Add(ColumnSchema {
                name: "we`ird".to_string(),
                col_type: "varchar(255)".to_string(),
                nullable: false,
                default: Some("it's".to_string()),
                auto_increment: false,
            })],
            vec![],
        );

        assert_eq!(
            statements,
            vec![
                "ALTER TABLE `users` ADD COLUMN `we``ird` varchar(255) NOT NULL DEFAULT 'it''s'"
            ]
        );
    }

    #[test]
    fn only_mysql_is_supported() {
        let empty = diff(vec![], vec![]);

        assert!(matches!(
            plan_table_statements(&empty, DbBackend::Postgres),
            Err(TableError::UnsupportedBackend(DbBackend::Postgres))
        ));
        assert!(matches!(
            plan_table_statements(&empty, DbBackend::Sqlite),
            Err(TableError::UnsupportedBackend(DbBackend::Sqlite))
        ));
    }

    #[test]
    fn an_empty_plan_has_no_statements() {
        let plan = MigrationPlan::default();

        assert!(plan.is_empty());
        assert!(plan.statements().is_empty());
    }
}
