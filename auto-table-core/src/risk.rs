//! Risk classification for migration changes.
//!
//! Every change is classified as [`Risk::Safe`], [`Risk::Caution`] or
//! [`Risk::Destructive`]. Only destructive changes need an explicit opt-in
//! before a plan is applied — see [`crate::MigrationPlan::allow_destructive`]
//! and [`crate::MigrateOptions::allow_destructive`]; caution changes only
//! warrant a warning and run unconditionally.

use crate::diff::{ColumnAspect, ColumnChange, TableDiff};

/// How dangerous a single migration change is.
///
/// The ordering is `Safe < Caution < Destructive`, so taking the maximum over a
/// plan yields the worst change it contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Risk {
    /// No data is touched or put at risk: adding a column, changing a default,
    /// adding or dropping an index.
    #[default]
    Safe,
    /// The change may fail (leaving data intact) or silently alter existing
    /// rows: narrowing a type, tightening nullability, or adding a `NOT NULL`
    /// column without a default.
    Caution,
    /// The change destroys data that cannot be recovered: dropping a column.
    Destructive,
}

impl Risk {
    /// Whether this level requires explicit approval before applying.
    pub fn is_destructive(self) -> bool {
        self == Risk::Destructive
    }

    /// Whether this level warrants a warning before applying.
    pub fn is_caution(self) -> bool {
        self == Risk::Caution
    }
}

/// Classifies every change in a table diff and returns the worst risk it holds.
pub fn classify(diff: &TableDiff) -> Risk {
    let mut risk = Risk::Safe;
    for column in &diff.columns {
        risk = risk.max(match column {
            ColumnChange::Add(schema) => {
                // A NOT NULL column without a default silently fills existing
                // rows on some backends and errors on others — either way it is
                // not data-safe to do blindly.
                if !schema.nullable && schema.default.is_none() {
                    Risk::Caution
                } else {
                    Risk::Safe
                }
            }
            ColumnChange::Drop { .. } => Risk::Destructive,
            ColumnChange::Alter { aspects, .. } => {
                let mut column_risk = Risk::Safe;
                for aspect in aspects {
                    match aspect {
                        // A type change may narrow and reject existing values.
                        ColumnAspect::Type { .. } => {
                            column_risk = column_risk.max(Risk::Caution);
                        }
                        // Tightening nullability can reject rows that are NULL.
                        ColumnAspect::Nullable { to, .. } => {
                            if !*to {
                                column_risk = column_risk.max(Risk::Caution);
                            }
                        }
                        // Changing a default or toggling identity never touches
                        // existing rows.
                        ColumnAspect::Default { .. } | ColumnAspect::AutoIncrement { .. } => {}
                    }
                }
                column_risk
            }
        });
    }
    // Index changes (add / drop) never destroy or risk data.
    risk
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{ColumnAspect, ColumnChange, IndexChange, TableDiff};
    use crate::schema::{ColumnSchema, IndexSchema};

    fn col(nullable: bool, default: Option<&str>) -> ColumnSchema {
        ColumnSchema {
            name: "c".into(),
            col_type: "int".into(),
            nullable,
            default: default.map(|d| d.to_string()),
            auto_increment: false,
        }
    }

    fn add_column(nullable: bool, default: Option<&str>) -> ColumnChange {
        ColumnChange::Add(col(nullable, default))
    }

    fn alter(to: ColumnSchema, aspects: Vec<ColumnAspect>) -> ColumnChange {
        ColumnChange::Alter {
            name: "c".into(),
            to,
            aspects,
        }
    }

    fn diff_with(columns: Vec<ColumnChange>) -> TableDiff {
        TableDiff {
            table: "t".into(),
            columns,
            indexes: vec![],
        }
    }

    #[test]
    fn safe_changes_classify_as_safe() {
        let diff = diff_with(vec![
            add_column(true, None),
            add_column(false, Some("0")),
            alter(
                col(false, Some("0")),
                vec![ColumnAspect::Default {
                    from: None,
                    to: Some("0".into()),
                }],
            ),
        ]);
        assert_eq!(classify(&diff), Risk::Safe);
    }

    #[test]
    fn index_add_and_drop_are_safe() {
        let mut diff = diff_with(vec![]);
        diff.indexes.push(IndexChange::Add(IndexSchema {
            name: "i".into(),
            columns: vec!["c".into()],
            unique: false,
            primary: false,
        }));
        assert_eq!(classify(&diff), Risk::Safe);
    }

    #[test]
    fn add_not_null_without_default_is_caution() {
        assert_eq!(classify(&diff_with(vec![add_column(false, None)])), Risk::Caution);
    }

    #[test]
    fn tightening_nullability_is_caution() {
        assert_eq!(
            classify(&diff_with(vec![alter(
                col(false, None),
                vec![ColumnAspect::Nullable { from: true, to: false }]
            )])),
            Risk::Caution
        );
    }

    #[test]
    fn relaxing_nullability_is_safe() {
        assert_eq!(
            classify(&diff_with(vec![alter(
                col(true, None),
                vec![ColumnAspect::Nullable { from: false, to: true }]
            )])),
            Risk::Safe
        );
    }

    #[test]
    fn type_change_is_caution() {
        assert_eq!(
            classify(&diff_with(vec![alter(
                col(false, None),
                vec![ColumnAspect::Type {
                    from: "int".into(),
                    to: "bigint".into()
                }]
            )])),
            Risk::Caution
        );
    }

    #[test]
    fn dropping_a_column_is_destructive() {
        assert_eq!(
            classify(&diff_with(vec![ColumnChange::Drop { name: "c".into() }])),
            Risk::Destructive
        );
    }

    #[test]
    fn drop_wins_over_caution() {
        assert_eq!(
            classify(&diff_with(vec![
                add_column(false, None),
                ColumnChange::Drop { name: "c".into() }
            ])),
            Risk::Destructive
        );
    }
}
