//! Risk classification and policy for migration changes.
//!
//! Every change is classified by [`classify`] into a [`Risk`] level
//! ([`Risk::Safe`], [`Risk::Caution`] or [`Risk::Destructive`]) and, more
//! granularly, into a [`ChangeKind`]. [`RiskPolicy`] then decides, per change,
//! whether it may be applied, using three configuration layers that take
//! precedence in this order: a specific [`ChangeKind`] (L3) overrides a
//! [`Risk`] level (L2), which overrides the global switch (L1).
//!
//! The default policy blocks only destructive changes (e.g. dropping a column)
//! unless opted in via [`crate::MigrationPlan::allow_destructive`] or
//! [`crate::MigrateOptions::allow_destructive`].

use std::collections::HashMap;

use crate::diff::{ColumnAspect, ColumnChange, IndexChange, TableDiff};

/// How dangerous a single migration change is.
///
/// The ordering is `Safe < Caution < Destructive`, so taking the maximum over a
/// plan yields the worst change it contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
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

/// A specific kind of schema change.
///
/// Used by [`RiskPolicy`] to switch behaviour per change type — the third, most
/// specific layer of configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// Adding a nullable column, or one with a default — safe.
    AddColumn,
    /// Adding a `NOT NULL` column without a default — may silently fill rows.
    AddNotNullColumn,
    /// Dropping a column — destroys data.
    DropColumn,
    /// Changing a column's type — may narrow and reject existing values.
    ChangeType,
    /// Tightening nullability (`NULL` -> `NOT NULL`) — may reject rows.
    TightenNullability,
    /// Relaxing nullability (`NOT NULL` -> `NULL`) — safe.
    RelaxNullability,
    /// Changing a column default — never touches existing rows.
    ChangeDefault,
    /// Toggling auto-increment — never touches existing rows.
    ChangeAutoIncrement,
    /// Adding an index — safe.
    AddIndex,
    /// Dropping an index — safe.
    DropIndex,
}

impl std::fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            ChangeKind::AddColumn => "add column",
            ChangeKind::AddNotNullColumn => "add NOT NULL column without default",
            ChangeKind::DropColumn => "drop column",
            ChangeKind::ChangeType => "change type",
            ChangeKind::TightenNullability => "tighten nullability",
            ChangeKind::RelaxNullability => "relax nullability",
            ChangeKind::ChangeDefault => "change default",
            ChangeKind::ChangeAutoIncrement => "change auto-increment",
            ChangeKind::AddIndex => "add index",
            ChangeKind::DropIndex => "drop index",
        };
        f.write_str(text)
    }
}

/// What to do with a change that a [`RiskPolicy`] rule matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RiskAction {
    /// Apply the change.
    #[default]
    Allow,
    /// Refuse the change before it runs.
    Block,
}

/// Three-layer switch controlling which risk items may be applied.
///
/// Layers are evaluated in increasing specificity; a more specific layer wins:
///
/// 1. `global` — applies to every change, regardless of kind or level.
/// 2. `levels` — applies to every change of a given [`Risk`] level.
/// 3. `items`  — applies to a specific [`ChangeKind`].
///
/// So a `DropColumn` rule in `items` overrides any `levels` or `global` setting
/// for dropping columns. An absent `levels`/`items` entry falls through to the
/// next layer; `global` is always present and is the baseline.
///
/// The default policy reproduces the historical behaviour: everything is allowed
/// except destructive changes, which are blocked unless opted in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskPolicy {
    /// Layer 1: applies to all risk items.
    pub global: RiskAction,
    /// Layer 2: applies to all changes of a risk level.
    pub levels: HashMap<Risk, RiskAction>,
    /// Layer 3: applies to a specific change kind.
    pub items: HashMap<ChangeKind, RiskAction>,
}

impl RiskPolicy {
    /// Resolves the action for a change, applying layers in precedence order:
    /// specific item (L3) > risk level (L2) > global (L1).
    pub fn resolve(&self, kind: ChangeKind, level: Risk) -> RiskAction {
        self.items
            .get(&kind)
            .copied()
            .or_else(|| self.levels.get(&level).copied())
            .unwrap_or(self.global)
    }
}

impl Default for RiskPolicy {
    fn default() -> Self {
        let mut levels = HashMap::new();
        levels.insert(Risk::Destructive, RiskAction::Block);
        Self {
            global: RiskAction::Allow,
            levels,
            items: HashMap::new(),
        }
    }
}

/// The baseline risk of a single change kind, independent of any policy.
fn kind_risk(kind: ChangeKind) -> Risk {
    match kind {
        ChangeKind::AddColumn
        | ChangeKind::RelaxNullability
        | ChangeKind::ChangeDefault
        | ChangeKind::ChangeAutoIncrement
        | ChangeKind::AddIndex
        | ChangeKind::DropIndex => Risk::Safe,
        ChangeKind::AddNotNullColumn | ChangeKind::ChangeType | ChangeKind::TightenNullability => {
            Risk::Caution
        }
        ChangeKind::DropColumn => Risk::Destructive,
    }
}

/// Lists every change in a table diff, paired with its [`ChangeKind`] and
/// [`Risk`], so callers can decide per item (via [`RiskPolicy`]) whether to
/// apply it.
pub fn classify_changes(diff: &TableDiff) -> Vec<(ChangeKind, Risk)> {
    let mut out = Vec::new();
    for column in &diff.columns {
        match column {
            ColumnChange::Add(schema) => {
                let kind = if !schema.nullable && schema.default.is_none() {
                    ChangeKind::AddNotNullColumn
                } else {
                    ChangeKind::AddColumn
                };
                out.push((kind, kind_risk(kind)));
            }
            ColumnChange::Drop { .. } => out.push((ChangeKind::DropColumn, Risk::Destructive)),
            ColumnChange::Alter { aspects, .. } => {
                for aspect in aspects {
                    let kind = match aspect {
                        ColumnAspect::Type { .. } => ChangeKind::ChangeType,
                        ColumnAspect::Nullable { to, .. } => {
                            if *to {
                                ChangeKind::RelaxNullability
                            } else {
                                ChangeKind::TightenNullability
                            }
                        }
                        ColumnAspect::Default { .. } => ChangeKind::ChangeDefault,
                        ColumnAspect::AutoIncrement { .. } => ChangeKind::ChangeAutoIncrement,
                    };
                    out.push((kind, kind_risk(kind)));
                }
            }
        }
    }
    for index in &diff.indexes {
        let kind = match index {
            IndexChange::Add(..) => ChangeKind::AddIndex,
            IndexChange::Drop(..) => ChangeKind::DropIndex,
        };
        out.push((kind, kind_risk(kind)));
    }
    out
}

/// Classifies every change in a table diff and returns the worst risk it holds.
pub fn classify(diff: &TableDiff) -> Risk {
    classify_changes(diff)
        .into_iter()
        .map(|(_, risk)| risk)
        .max()
        .unwrap_or(Risk::Safe)
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

    #[test]
    fn classify_changes_lists_each_item() {
        let changes = classify_changes(&diff_with(vec![
            add_column(false, None),
            ColumnChange::Drop { name: "c".into() },
        ]));
        assert!(changes.contains(&(ChangeKind::AddNotNullColumn, Risk::Caution)));
        assert!(changes.contains(&(ChangeKind::DropColumn, Risk::Destructive)));
    }

    #[test]
    fn policy_default_blocks_only_destructive() {
        let policy = RiskPolicy::default();
        assert_eq!(policy.resolve(ChangeKind::DropColumn, Risk::Destructive), RiskAction::Block);
        assert_eq!(
            policy.resolve(ChangeKind::AddNotNullColumn, Risk::Caution),
            RiskAction::Allow
        );
        assert_eq!(policy.resolve(ChangeKind::AddColumn, Risk::Safe), RiskAction::Allow);
    }

    #[test]
    fn policy_layers_precedence_most_specific_wins() {
        let mut policy = RiskPolicy {
            global: RiskAction::Block,
            ..RiskPolicy::default()
        };
        // L2: but allow the whole Caution level.
        policy.levels.insert(Risk::Caution, RiskAction::Allow);
        // L3: except this specific item, which must stay blocked.
        policy.items.insert(ChangeKind::AddNotNullColumn, RiskAction::Block);

        // L3 wins over L2/L1.
        assert_eq!(
            policy.resolve(ChangeKind::AddNotNullColumn, Risk::Caution),
            RiskAction::Block
        );
        // L2 wins over L1: other Caution items are allowed.
        assert_eq!(
            policy.resolve(ChangeKind::TightenNullability, Risk::Caution),
            RiskAction::Allow
        );
        // L1 applies to everything else.
        assert_eq!(
            policy.resolve(ChangeKind::AddColumn, Risk::Safe),
            RiskAction::Block
        );
    }
}
