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
        Feature::NativeAlterColumn => "Native alter column",
        Feature::AlterColumnUsing => "Custom `USING` expression",
        Feature::SequenceDefault => "Sequence-backed default",
        Feature::RenameColumnGuard => "Rename-column existence guard",
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
