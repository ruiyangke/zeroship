//! Deterministic Markdown rendering for the committed feature-support matrix.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use crate::model::op_support::FEATURE_SUPPORT_REGISTRY;
use crate::model::support::{Dialect, Feature, SupportDecision};

const REGENERATE_COMMAND: &str = "ZERO_MIGRATE_UPDATE_SUPPORT_MATRIX=1 cargo test -p zero-migrate --lib model::support_matrix::committed_support_matrix_is_current -- --exact";

fn feature_label(feature: Feature) -> &'static str {
    match feature {
        Feature::PartialIndex => "Partial index",
        Feature::IndexInclude => "Included index columns",
        Feature::IndexStorageParams => "Index storage parameters",
        Feature::IndexOnly => "Index on `ONLY`",
        Feature::IndexNullsNotDistinct => "Unique index with `NULLS NOT DISTINCT`",
        Feature::IndexOpclass => "Index operator class",
        Feature::IndexCollation => "Index collation",
        Feature::ExpressionIndex => "Expression index",
        Feature::NonBtreeIndexMethod => "Non-btree index method",
        Feature::TableLevelForeignKey => "Table-level foreign key",
        Feature::TableLevelUnique => "Table-level unique constraint",
        Feature::TableLevelCheck => "Table-level check constraint",
        Feature::CompositeForeignKey => "Composite foreign key",
        Feature::ForeignKeyNoLocalColumn => "Foreign key with no local column",
        Feature::NonIdForeignKey => "Foreign key referencing a non-`id` column",
        Feature::ConstraintNotValid => "`NOT VALID` constraint",
        Feature::ExclusionConstraint => "Exclusion constraint",
        Feature::AlterColumnUsing => "Custom `USING` expression",
        Feature::SequenceDefault => "Sequence-backed default",
        Feature::RenameColumnGuard => "Rename-column existence guard",
        Feature::ExistenceGuardProbe => "`ifNotExists`/`ifExists` enforced at apply",
        Feature::InsertOnConflict => "Insert with `ON CONFLICT`",
        Feature::MaterializedView => "Materialized view",
        Feature::CreateOrReplaceMaterializedView => "`CREATE OR REPLACE MATERIALIZED VIEW`",
        Feature::TriggerMultipleEvents => "Multiple trigger events",
        Feature::TriggerTruncateEvent => "`TRUNCATE` trigger event",
        Feature::TriggerInsteadOfTiming => "`INSTEAD OF` trigger timing",
        Feature::TriggerStatementForEach => "Statement-level trigger",
        Feature::TriggerExecuteFunction => "Execute a named trigger function",
        Feature::TriggerBody => "Structured trigger body",
        Feature::TriggerWhen => "Trigger `WHEN` predicate",
        Feature::TriggerRaiseIgnore => "Trigger `RAISE IGNORE`",
        Feature::Comment => "Comment",
        Feature::Sequence => "Standalone sequence",
        Feature::RawViewBody => "Raw view body",
        Feature::RawSql => "Raw SQL",
        Feature::PartitionDdl => "Partition DDL",
    }
}

fn render_cell(
    decision: SupportDecision,
    footnote_ids: &mut BTreeMap<&'static str, usize>,
    footnotes: &mut Vec<&'static str>,
) -> String {
    match decision {
        SupportDecision::Supported { .. } => "Yes".to_string(),
        SupportDecision::Unsupported { reason, .. } => {
            let id = *footnote_ids.entry(reason).or_insert_with(|| {
                footnotes.push(reason);
                footnotes.len()
            });
            format!("No[^{id}]")
        }
    }
}

pub(crate) fn render_support_matrix() -> String {
    let mut markdown = String::new();
    writeln!(markdown, "# Feature support matrix\n").expect("writing to a String cannot fail");
    writeln!(
        markdown,
        "> **GENERATED from `crates/zero-migrate/src/model/support.rs`. Do not edit by hand.**"
    )
    .expect("writing to a String cannot fail");
    writeln!(markdown, ">").expect("writing to a String cannot fail");
    writeln!(markdown, "> Regenerate with `{REGENERATE_COMMAND}`.\n")
        .expect("writing to a String cannot fail");
    writeln!(
        markdown,
        "`Yes` means the feature's capability decision is supported for that dialect; `No` means it is unsupported for the reason in the linked note.\n"
    )
    .expect("writing to a String cannot fail");

    let mut footnote_ids = BTreeMap::new();
    let mut footnotes = Vec::new();

    for group in FEATURE_SUPPORT_REGISTRY {
        let group_label = group.label();
        assert!(
            !group_label.is_empty(),
            "support-matrix operation group label must not be empty"
        );
        writeln!(markdown, "## {group_label}\n").expect("writing to a String cannot fail");
        markdown.push_str("| Feature | PostgreSQL 18 | MySQL 8 | SQLite |\n");
        markdown.push_str("| --- | --- | --- | --- |\n");

        for feature in group.features() {
            let label = feature_label(feature.feature);
            assert!(
                !label.is_empty(),
                "support-matrix feature label must not be empty"
            );
            let postgres = render_cell(
                feature.decision(Dialect::Postgres),
                &mut footnote_ids,
                &mut footnotes,
            );
            let mysql = render_cell(
                feature.decision(Dialect::Mysql),
                &mut footnote_ids,
                &mut footnotes,
            );
            let sqlite = render_cell(
                feature.decision(Dialect::Sqlite),
                &mut footnote_ids,
                &mut footnotes,
            );
            writeln!(markdown, "| {label} | {postgres} | {mysql} | {sqlite} |")
                .expect("writing to a String cannot fail");
        }
        markdown.push('\n');
    }

    markdown.push_str("## Notes\n\n");
    for (index, reason) in footnotes.iter().enumerate() {
        writeln!(markdown, "[^{}]: {reason}", index + 1).expect("writing to a String cannot fail");
    }

    markdown
}

fn support_matrix_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/support-matrix.md")
}

#[test]
fn committed_support_matrix_is_current() {
    let rendered = render_support_matrix();
    let path = support_matrix_path();

    if std::env::var("ZERO_MIGRATE_UPDATE_SUPPORT_MATRIX").as_deref() == Ok("1") {
        std::fs::write(&path, rendered).unwrap_or_else(|error| {
            panic!(
                "failed to write generated support matrix at {}: {error}",
                path.display()
            )
        });
        return;
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "failed to read committed support matrix at {}: {error}; regenerate it with `{REGENERATE_COMMAND}`",
            path.display()
        )
    });
    assert_eq!(
        committed, rendered,
        "docs/support-matrix.md is stale; regenerate it with `{REGENERATE_COMMAND}`"
    );
}

/// Every `Feature` variant, so the guard below can find the ones that never
/// reach the published matrix.
///
/// This list is maintained by hand because `Feature` carries no reflection.
/// Adding a variant without adding it here weakens the guard, so keep them
/// together; `feature_label`'s exhaustive `match` is the compiler-enforced
/// reminder that a new variant needs attention in this file.
#[cfg(test)]
const ALL_FEATURES: &[Feature] = &[
    Feature::PartialIndex,
    Feature::IndexInclude,
    Feature::IndexStorageParams,
    Feature::IndexOnly,
    Feature::IndexNullsNotDistinct,
    Feature::IndexOpclass,
    Feature::IndexCollation,
    Feature::ExpressionIndex,
    Feature::NonBtreeIndexMethod,
    Feature::TableLevelForeignKey,
    Feature::TableLevelUnique,
    Feature::TableLevelCheck,
    Feature::CompositeForeignKey,
    Feature::ForeignKeyNoLocalColumn,
    Feature::NonIdForeignKey,
    Feature::ConstraintNotValid,
    Feature::ExclusionConstraint,
    Feature::AlterColumnUsing,
    Feature::SequenceDefault,
    Feature::RenameColumnGuard,
    Feature::ExistenceGuardProbe,
    Feature::InsertOnConflict,
    Feature::MaterializedView,
    Feature::CreateOrReplaceMaterializedView,
    Feature::TriggerMultipleEvents,
    Feature::TriggerTruncateEvent,
    Feature::TriggerInsteadOfTiming,
    Feature::TriggerStatementForEach,
    Feature::TriggerExecuteFunction,
    Feature::TriggerBody,
    Feature::TriggerWhen,
    Feature::TriggerRaiseIgnore,
    Feature::Comment,
    Feature::Sequence,
    Feature::RawViewBody,
    Feature::RawSql,
    Feature::PartitionDdl,
];

/// A `Feature` that carries a matrix label but appears in no support-decision
/// group renders NOTHING, silently.
///
/// `Feature::NativeAlterColumn` was exactly that: declared in the enum, given
/// the label "Native alter column", and listed in no `*_FEATURES` array, so the
/// generated matrix simply had no such row. Nothing failed. The committed
/// matrix was byte-current the whole time, because it faithfully rendered a
/// registry the feature was never in.
///
/// The concept is live elsewhere -- `Capability::NativeAlterColumn` in the
/// renderer decides it per dialect (PostgreSQL yes, SQLite NO, MySQL yes) -- so
/// this was not a feature that had been dropped, it was a second route to one
/// concept where only one route was published.
#[cfg(test)]
#[test]
fn every_feature_reaches_the_support_matrix() {
    let declared: Vec<Feature> = FEATURE_SUPPORT_REGISTRY
        .iter()
        .flat_map(|group| group.features().iter().map(|support| support.feature))
        .collect();

    let unreachable: Vec<&'static str> = ALL_FEATURES
        .iter()
        .filter(|feature| !declared.contains(feature))
        .map(|feature| feature_label(*feature))
        .collect();

    assert!(
        unreachable.is_empty(),
        "these features carry a matrix label but appear in no support-decision \
         group, so they render no row and no test notices: {unreachable:?}. \
         Either declare the feature's per-dialect decision or drop the variant \
         and its label"
    );
}
