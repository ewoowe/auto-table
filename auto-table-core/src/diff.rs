//! Compares two table snapshots and reports what has to change
//!
//! This is a pure comparison: it decides *what* differs, never *how* to apply
//! it, and it never talks to a database. Turning the result into SQL is the job
//! of the migration planner.
//!
//! `expected` normally comes from [`crate::parse::parse_create_table`] (the
//! structure the entity declares) and `actual` from
//! [`crate::schema::get_table_schema`] (the structure currently in the
//! database). Both produce the same [`TableSchema`] shape, so the two sides are
//! directly comparable.

use crate::schema::{ColumnSchema, IndexSchema, TableSchema};

/// Everything that has to change on one table
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableDiff {
    /// Name of the table that was compared
    pub table: String,
    /// Column changes, in declaration order
    pub columns: Vec<ColumnChange>,
    /// Index changes
    ///
    /// An index whose definition changed appears as a [`IndexChange::Drop`]
    /// followed by an [`IndexChange::Add`], because no backend can alter an
    /// index in place.
    pub indexes: Vec<IndexChange>,
}

impl TableDiff {
    /// Whether the two snapshots described the same table
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty() && self.indexes.is_empty()
    }
}

/// A change to a single column
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnChange {
    /// The column is declared but missing from the table
    Add(ColumnSchema),
    /// The column exists in the table but is no longer declared
    Drop {
        /// Name of the obsolete column
        name: String,
    },
    /// The column exists on both sides but is defined differently
    Alter {
        /// Name of the column
        name: String,
        /// The column as the entity declares it
        ///
        /// MySQL `MODIFY COLUMN` replaces the whole definition, so building
        /// the statement needs the complete target and not only the aspects
        /// that changed.
        to: ColumnSchema,
        /// The aspects that differ; never empty
        aspects: Vec<ColumnAspect>,
    },
}

/// One aspect of a column that is defined differently
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnAspect {
    /// The column type changed
    Type {
        /// Type currently in the database
        from: String,
        /// Type the entity declares
        to: String,
    },
    /// The nullability changed
    Nullable {
        /// Nullability currently in the database
        from: bool,
        /// Nullability the entity declares
        to: bool,
    },
    /// The default value changed
    Default {
        /// Default currently in the database
        from: Option<String>,
        /// Default the entity declares
        to: Option<String>,
    },
    /// The auto-increment flag changed
    AutoIncrement {
        /// Flag currently in the database
        from: bool,
        /// Flag the entity declares
        to: bool,
    },
}

/// A change to a single index
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexChange {
    /// The index is declared but missing from the table
    Add(IndexSchema),
    /// The index exists in the table but is no longer declared
    ///
    /// It carries the whole index rather than just its name, because dropping
    /// one needs more than the name: PostgreSQL drops a constraint instead of
    /// an index when a unique constraint is behind it, and it needs to know
    /// which kind it is dealing with.
    Drop(IndexSchema),
}

/// Compares the structure an entity declares against the one in the database
pub fn diff_table(expected: &TableSchema, actual: &TableSchema) -> TableDiff {
    TableDiff {
        table: expected.name.clone(),
        columns: diff_columns(&expected.columns, &actual.columns),
        indexes: diff_indexes(&expected.indexes, &actual.indexes),
    }
}

/// Compares two column lists by column name, ignoring declaration order
fn diff_columns(expected: &[ColumnSchema], actual: &[ColumnSchema]) -> Vec<ColumnChange> {
    let mut changes = Vec::new();
    let mut matched: Vec<&str> = Vec::new();

    for wanted in expected {
        match actual.iter().find(|column| column.name == wanted.name) {
            None => changes.push(ColumnChange::Add(wanted.clone())),
            Some(current) => {
                matched.push(current.name.as_str());
                let aspects = diff_column(wanted, current);
                if !aspects.is_empty() {
                    changes.push(ColumnChange::Alter {
                        name: wanted.name.clone(),
                        to: wanted.clone(),
                        aspects,
                    });
                }
            }
        }
    }

    for current in actual {
        if !matched.contains(&current.name.as_str()) {
            changes.push(ColumnChange::Drop {
                name: current.name.clone(),
            });
        }
    }

    changes
}

/// Compares two columns that are known to share a name
fn diff_column(wanted: &ColumnSchema, current: &ColumnSchema) -> Vec<ColumnAspect> {
    let mut aspects = Vec::new();

    if wanted.col_type != current.col_type {
        aspects.push(ColumnAspect::Type {
            from: current.col_type.clone(),
            to: wanted.col_type.clone(),
        });
    }
    if wanted.nullable != current.nullable {
        aspects.push(ColumnAspect::Nullable {
            from: current.nullable,
            to: wanted.nullable,
        });
    }
    if wanted.default != current.default {
        aspects.push(ColumnAspect::Default {
            from: current.default.clone(),
            to: wanted.default.clone(),
        });
    }
    if wanted.auto_increment != current.auto_increment {
        aspects.push(ColumnAspect::AutoIncrement {
            from: current.auto_increment,
            to: wanted.auto_increment,
        });
    }

    aspects
}

/// Compares two index lists by index name, ignoring declaration order
fn diff_indexes(expected: &[IndexSchema], actual: &[IndexSchema]) -> Vec<IndexChange> {
    let mut changes = Vec::new();
    let mut matched: Vec<&str> = Vec::new();

    for wanted in expected {
        match actual.iter().find(|index| index.name == wanted.name) {
            None => changes.push(IndexChange::Add(wanted.clone())),
            Some(current) => {
                matched.push(current.name.as_str());
                if current.columns != wanted.columns
                    || current.unique != wanted.unique
                    || current.primary != wanted.primary
                {
                    // Recreating an index means dropping it first, and the Drop
                    // is emitted before the Add so a caller that applies the
                    // changes in order does the right thing.
                    changes.push(IndexChange::Drop(current.clone()));
                    changes.push(IndexChange::Add(wanted.clone()));
                }
            }
        }
    }

    for current in actual {
        if !matched.contains(&current.name.as_str()) {
            changes.push(IndexChange::Drop(current.clone()));
        }
    }

    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, col_type: &str) -> ColumnSchema {
        ColumnSchema {
            name: name.to_string(),
            col_type: col_type.to_string(),
            nullable: true,
            default: None,
            auto_increment: false,
        }
    }

    fn index(name: &str, columns: &[&str], unique: bool) -> IndexSchema {
        IndexSchema {
            name: name.to_string(),
            columns: columns.iter().map(|c| c.to_string()).collect(),
            unique,
            primary: false,
        }
    }

    fn table(name: &str, columns: Vec<ColumnSchema>, indexes: Vec<IndexSchema>) -> TableSchema {
        TableSchema {
            name: name.to_string(),
            columns,
            indexes,
        }
    }

    #[test]
    fn identical_snapshots_produce_an_empty_diff() {
        let expected = table("users", vec![column("id", "int"), column("name", "varchar(255)")], vec![]);
        let actual = expected.clone();

        let diff = diff_table(&expected, &actual);

        assert!(diff.is_empty());
        assert_eq!(diff.table, "users");
    }

    #[test]
    fn detects_added_and_dropped_columns() {
        let expected = table(
            "users",
            vec![column("id", "int"), column("email", "varchar(255)")],
            vec![],
        );
        let actual = table("users", vec![column("id", "int"), column("legacy", "int")], vec![]);

        let diff = diff_table(&expected, &actual);

        assert!(!diff.is_empty());
        assert_eq!(diff.columns.len(), 2);
        assert_eq!(diff.columns[0], ColumnChange::Add(column("email", "varchar(255)")));
        assert_eq!(
            diff.columns[1],
            ColumnChange::Drop {
                name: "legacy".to_string()
            }
        );
    }

    #[test]
    fn detects_type_and_nullability_changes() {
        let wanted = ColumnSchema {
            name: "age".to_string(),
            col_type: "bigint".to_string(),
            nullable: false,
            default: None,
            auto_increment: false,
        };
        let expected = table("users", vec![wanted.clone()], vec![]);
        let actual = table("users", vec![column("age", "int")], vec![]);

        let diff = diff_table(&expected, &actual);

        assert_eq!(diff.columns.len(), 1);
        assert_eq!(
            diff.columns[0],
            ColumnChange::Alter {
                name: "age".to_string(),
                to: wanted,
                aspects: vec![
                    ColumnAspect::Type {
                        from: "int".to_string(),
                        to: "bigint".to_string(),
                    },
                    ColumnAspect::Nullable {
                        from: true,
                        to: false,
                    },
                ],
            }
        );
    }

    #[test]
    fn detects_default_and_auto_increment_changes() {
        let wanted = ColumnSchema {
            name: "id".to_string(),
            col_type: "int".to_string(),
            nullable: false,
            default: None,
            auto_increment: true,
        };
        let expected = table("users", vec![wanted.clone()], vec![]);
        let actual = table(
            "users",
            vec![ColumnSchema {
                name: "id".to_string(),
                col_type: "int".to_string(),
                nullable: false,
                default: Some("7".to_string()),
                auto_increment: false,
            }],
            vec![],
        );

        let diff = diff_table(&expected, &actual);

        assert_eq!(
            diff.columns[0],
            ColumnChange::Alter {
                name: "id".to_string(),
                to: wanted,
                aspects: vec![
                    ColumnAspect::Default {
                        from: Some("7".to_string()),
                        to: None,
                    },
                    ColumnAspect::AutoIncrement {
                        from: false,
                        to: true,
                    },
                ],
            }
        );
    }

    #[test]
    fn detects_added_and_dropped_indexes() {
        let expected = table(
            "users",
            vec![],
            vec![index("email", &["email"], true), index("age", &["age"], false)],
        );
        let actual = table(
            "users",
            vec![],
            vec![index("email", &["email"], true), index("legacy", &["legacy"], false)],
        );

        let diff = diff_table(&expected, &actual);

        assert_eq!(diff.indexes.len(), 2);
        assert_eq!(diff.indexes[0], IndexChange::Add(index("age", &["age"], false)));
        assert_eq!(
            diff.indexes[1],
            IndexChange::Drop(index("legacy", &["legacy"], false))
        );
    }

    #[test]
    fn recreates_an_index_whose_definition_changed() {
        let expected = table("users", vec![], vec![index("email", &["email", "name"], true)]);
        let actual = table("users", vec![], vec![index("email", &["email"], true)]);

        let diff = diff_table(&expected, &actual);

        // No backend can alter an index in place, so it is dropped and rebuilt
        assert_eq!(
            diff.indexes,
            vec![
                IndexChange::Drop(index("email", &["email"], true)),
                IndexChange::Add(index("email", &["email", "name"], true)),
            ]
        );
    }

    #[test]
    fn ignores_declaration_order() {
        let expected = table(
            "users",
            vec![column("id", "int"), column("name", "varchar(255)")],
            vec![index("a", &["a"], false), index("b", &["b"], false)],
        );
        let actual = table(
            "users",
            vec![column("name", "varchar(255)"), column("id", "int")],
            vec![index("b", &["b"], false), index("a", &["a"], false)],
        );

        assert!(diff_table(&expected, &actual).is_empty());
    }
}

/// End-to-end check that parse, normalization and diff line up
///
/// The tests above compare hand-written snapshots. These generate the expected
/// structure live and write the actual side the way MySQL reports it, so they
/// prove that a database in sync really does produce an empty diff.
#[cfg(test)]
mod round_trip {
    use sea_orm::entity::prelude::*;
    use sea_orm::{DbBackend, Schema};

    use crate::diff::{diff_table, ColumnChange};
    use crate::parse::parse_create_table;
    use crate::backend::mysql::normalize_mysql_type;
    use crate::schema::{ColumnSchema, IndexSchema, TableSchema};

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "round_trip")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        pub email: String,
        pub active: bool,
        pub balance: Decimal,
        pub legacy: Option<i32>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}

    /// Builds a column from the raw type as MySQL reports it
    fn column(name: &str, col_type: &str, nullable: bool, auto_increment: bool) -> ColumnSchema {
        ColumnSchema {
            name: name.to_string(),
            col_type: normalize_mysql_type(col_type),
            nullable,
            default: None,
            auto_increment,
        }
    }

    fn primary_key() -> IndexSchema {
        IndexSchema {
            name: "PRIMARY".to_string(),
            columns: vec!["id".to_string()],
            unique: true,
            primary: true,
        }
    }

    fn expected() -> TableSchema {
        let backend = DbBackend::MySql;
        let statement = Schema::new(backend).create_table_from_entity(Entity);
        parse_create_table(&backend.build(&statement).sql).expect("generated statement parses")
    }

    #[test]
    fn a_database_in_sync_produces_no_changes() {
        // Each type is written the way MySQL reports it and then normalized, so
        // this is what actually proves the two sides converge:
        //   `tinyint(1)`   -> `bool`    (a boolean is stored as TINYINT(1))
        //   `decimal(10,0)`-> `decimal` (MySQL always reports the precision)
        //   `int(11)`      -> `int`     (the display width carries no meaning)
        let actual = TableSchema {
            name: "round_trip".to_string(),
            columns: vec![
                column("id", "int", false, true),
                column("email", "varchar(255)", false, false),
                column("active", "tinyint(1)", false, false),
                column("balance", "decimal(10,0)", false, false),
                column("legacy", "int(11)", true, false),
            ],
            indexes: vec![primary_key()],
        };

        let diff = diff_table(&expected(), &actual);

        assert!(diff.is_empty(), "a synced table must not change: {diff:?}");
    }

    #[test]
    fn a_missing_column_is_reported() {
        let actual = TableSchema {
            name: "round_trip".to_string(),
            columns: vec![column("id", "int", false, true)],
            indexes: vec![primary_key()],
        };

        let diff = diff_table(&expected(), &actual);

        let added: Vec<&str> = diff
            .columns
            .iter()
            .filter_map(|change| match change {
                ColumnChange::Add(column) => Some(column.name.as_str()),
                _ => None,
            })
            .collect();

        assert!(added.contains(&"email"), "email should be added: {diff:?}");
        assert!(added.contains(&"active"), "active should be added: {diff:?}");
    }
}
