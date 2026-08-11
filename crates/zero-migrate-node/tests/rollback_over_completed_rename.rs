//! A completed PostgreSQL online rename does not block rolling back other migrations.
//!
//! The rollback verb lowers EVERY envelope of the authored set before the target is
//! selected (`verbs.rs` lowers with `?`, then builds the set, then constructs the
//! request), so one envelope that cannot lower fails the whole rollback - including a
//! rollback of a later migration that never touches the rename.
//!
//! After a rename is contracted the source column is gone from the catalog, and
//! lowering it again fails:
//!
//! ```text
//! IrAuthor::lower of renameColumn on "items"."a" needs the live `a` column's type
//! (LiveSchema::table_snapshots) to reconcile the IR-carried type against the live
//! column; it is absent - refusing to lower a rename from an IR type alone
//! ```
//!
//! The apply and status lowerings already recover from that by reconstructing the
//! pre-rename catalog view and retrying. Rollback did not, so a project that had ever
//! completed one online rename could not roll back anything.

use std::collections::BTreeMap;

use zero_migrate::apply::journal::{AppliedEntry, JournaledKind, Phase};
use zero_migrate::model::snapshot::{ColumnSnapshot, SchemaSnapshot, TableSnapshot};
use zero_migrate::PlanStatusManifest;

use zero_migrate_node::lower::{
    lower_ordered_envelopes_to_plans_for_apply, lower_ordered_envelopes_to_plans_for_rollback,
};

const CHARTER: &str = r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"

[[grant]]
key = "schema.create_table"
value = true
scope = "all"

[[grant]]
key = "schema.rename"
value = true
scope = "all"

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
"#;

const OWNER: &str = "app_test";
const REGISTRY: &str = r#"{"items":"app_test","notes":"app_test"}"#;

/// One `items` table carrying exactly the named columns, all `text`.
fn items_with(columns: &[&str]) -> SchemaSnapshot {
    SchemaSnapshot {
        tables: BTreeMap::from([(
            "items".to_string(),
            TableSnapshot {
                columns: columns
                    .iter()
                    .map(|name| ColumnSnapshot {
                        name: (*name).to_string(),
                        data_type: "text".to_string(),
                        nullable: true,
                        ..Default::default()
                    })
                    .collect(),
                indexes: Vec::new(),
                constraints: Vec::new(),
                runtime_options: Default::default(),
                partition_by: None,
                comment: None,
                stored_create_sql: None,
            },
        )]),
        ..Default::default()
    }
}

/// The rename that has since been contracted away.
fn rename_envelope() -> String {
    serde_json::json!({
        "ir_version": zero_migrate::model::ir::CURRENT_IR_VERSION,
        "name": "rename_a_to_b",
        "ops": [{ "op": "renameColumn", "table": "items", "from": "a", "to": "b", "type": "text" }]
    })
    .to_string()
}

/// A later migration on a different table, entirely reversible, which is the one an
/// operator would actually be trying to unwind.
fn later_envelope() -> String {
    serde_json::json!({
        "ir_version": zero_migrate::model::ir::CURRENT_IR_VERSION,
        "name": "add_notes",
        "ops": [{ "op": "createTable", "name": "notes",
                  "columns": [{ "name": "id", "type": "text", "nullable": false }] }]
    })
    .to_string()
}

/// The journal the rename left behind, derived by lowering it against the catalog it
/// ran on - the pre-contract shape, where `a` still exists.
///
/// Built this way rather than hand-written because the recovery is gated on the
/// derived plan matching a journal VERSION, and a version invented by the test would
/// prove the gate passes for the wrong reason.
fn journal_for_the_rename() -> Vec<AppliedEntry> {
    let applied = lower_ordered_envelopes_to_plans_for_apply(
        &[rename_envelope()],
        OWNER,
        OWNER,
        "postgres",
        REGISTRY,
        &[CHARTER],
        items_with(&["a"]),
        &[],
        &[],
    )
    .expect("the rename lowers against the catalog it originally ran on");

    applied
        .iter()
        .flat_map(|artifact| {
            let manifest =
                PlanStatusManifest::from_applied_plan(&artifact.plan, &artifact.depends_on)
                    .expect("manifest projects");
            manifest.steps.into_iter().map(|step| AppliedEntry {
                version: step.version.as_str().to_string(),
                checksum: step.checksum.as_str().to_string(),
                phase: Phase::Completed,
                kind: Some(JournaledKind::Apply),
                event_seq: 0,
            })
        })
        .collect()
}

#[test]
fn a_contracted_rename_earlier_in_the_history_does_not_block_the_rollback() {
    let journal = journal_for_the_rename();
    assert!(
        !journal.is_empty(),
        "the fixture must carry real journal evidence, or the recovery would be \
         refused for a reason this test is not about"
    );

    // The catalog as it stands at rollback time: the rename is contracted, so only
    // the destination column survives.
    let artifacts = lower_ordered_envelopes_to_plans_for_rollback(
        &[rename_envelope(), later_envelope()],
        OWNER,
        OWNER,
        "postgres",
        REGISTRY,
        &[CHARTER],
        items_with(&["b"]),
        &journal,
        &[],
    )
    .expect("a contracted rename must not fail the whole rollback lowering");

    assert_eq!(
        artifacts.len(),
        2,
        "both envelopes lower, so the later reversible migration is reachable"
    );
}

#[test]
fn the_recovery_still_refuses_a_rename_with_no_journal_evidence() {
    // Same catalog, same envelopes, but an EMPTY journal. The reconstruction is only
    // ever allowed to stand in for history that actually ran, so with nothing to
    // corroborate it the original refusal has to come back. Without this, the fix
    // would be indistinguishable from simply trusting any rename that fails to lower.
    let error = lower_ordered_envelopes_to_plans_for_rollback(
        &[rename_envelope(), later_envelope()],
        OWNER,
        OWNER,
        "postgres",
        REGISTRY,
        &[CHARTER],
        items_with(&["b"]),
        &[],
        &[],
    )
    .expect_err("an uncorroborated rename reconstruction must not be accepted");

    assert!(
        error.contains("refusing to lower a rename from an IR type alone"),
        "the refusal that comes back is the ORIGINAL lowering error, not a new one \
         invented by the recovery path: {error}"
    );
}
