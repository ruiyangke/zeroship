//! **Partition recording validates the ops inside the selected dialect leg.**
//!
//! `validate_partition_recording` replays a migration to check partition parent/child
//! state, key coverage, and bound well-formedness. It walked the top level only, so a
//! `createPartition` authored inside `dialect({ ... })` was never recorded and never
//! checked - the load gate passed a migration it had not actually validated.
//!
//! SELECTED leg, replayed INLINE through the same accumulator, which is the part that
//! took a second opinion to settle. Three properties force it together:
//!
//!   - Recording is STATEFUL across the whole migration. A parent created at the top
//!     level must be found and mutated by a child inside a leg, and the totality checks
//!     run only after the complete stream. Validating each leg as a separate
//!     mini-migration with fresh state loses that parent.
//!   - Legs are MUTUALLY EXCLUSIVE. Pooling children from two legs into one accumulator
//!     would compare siblings no target ever creates together - a false refusal on
//!     overlapping bounds, and a false accept when two legs jointly supply a totality
//!     no single target has.
//!   - The dialect is ALREADY load-bearing here: an unknown parent is tolerated on
//!     PostgreSQL and refused on SQLite and MySQL.
//!
//! The arms below pin the behaviour rather than the traversal: a malformed partition
//! must be refused when it is in the leg the target selects, and must NOT be refused
//! when it is in a leg the target skips.
//!
//! No live database: this is load-time validation.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::{validate_ir, ValidatorDialect};

/// Validate under `dialect` and return the refusal text, or `None` when it passed.
fn refusal(ops_json: &str, dialect: ValidatorDialect) -> Option<String> {
    let raw = format!(r#"{{"ir_version":1,"name":"parts","ops":{ops_json}}}"#);
    let ir: MigrationIr = serde_json::from_str(&raw).expect("the partition test IR parses");
    validate_ir(&ir, dialect, &[]).err().map(|e| e.to_string())
}

/// A `createPartition` whose parent nothing created. PostgreSQL tolerates an unknown
/// parent; SQLite refuses it. That asymmetry is the observable this file uses, because
/// it needs no valid partition scaffolding to trigger.
const ORPHAN_CHILD: &str = r#"{"op":"createPartition","name":"child_a","of":"absent_parent","bounds":{"kind":"list","values":[{"kind":"int","value":1}]}}"#;

/// The control: at the top level the orphan is refused on SQLite, so the arms below are
/// measuring the wrapper rather than a check that never fires.
#[test]
fn a_top_level_orphan_partition_is_refused_on_sqlite() {
    let error = refusal(&format!("[{ORPHAN_CHILD}]"), ValidatorDialect::Sqlite)
        .expect("SQLite refuses a partition whose parent nothing created");
    assert!(
        error.contains("absent_parent") || error.contains("child_a"),
        "the refusal names the partition or its parent: {error}"
    );
}

/// The reported defect: the same op inside the SELECTED leg was never recorded, so the
/// check did not run and the migration loaded clean.
#[test]
fn an_orphan_partition_inside_the_selected_leg_is_refused() {
    let error = refusal(
        &format!(r#"[{{"op":"dialectal","sqlite":[{ORPHAN_CHILD}]}}]"#),
        ValidatorDialect::Sqlite,
    )
    .expect("a dialect() wrapper must not hide a partition from the recording check");
    assert!(
        error.contains("absent_parent") || error.contains("child_a"),
        "the refusal names the partition or its parent: {error}"
    );
}

/// The control that keeps this SELECTED-leg rather than every-leg. SQLite skips the
/// PostgreSQL leg entirely, so nothing there is recorded and nothing is refused -
/// refusing here would reject a migration that is correct on this target.
#[test]
fn an_orphan_partition_in_an_unselected_leg_is_not_refused() {
    let error = refusal(
        &format!(r#"[{{"op":"dialectal","pg":[{ORPHAN_CHILD}]}}]"#),
        ValidatorDialect::Sqlite,
    );
    assert!(
        error.is_none(),
        "SQLite never runs the pg leg, so its partitions are not this target's to \
         validate: {error:?}"
    );
}

/// The stateful half, and the reason the legs cannot be validated with fresh state: a
/// parent created at the TOP LEVEL must still be visible to a child authored inside the
/// leg. If it were not, this well-formed pair would be refused as parentless on SQLite.
#[test]
fn a_top_level_parent_is_visible_to_a_child_inside_a_leg() {
    let ops = r#"[
      {"op":"createTable","name":"events","columns":[
        {"name":"tenant","type":"int","nullable":false}
      ],"primaryKey":["tenant"],"partitionBy":{"kind":"list","columns":["tenant"],"collapse":true}},
      {"op":"dialectal","sqlite":[
        {"op":"createPartition","name":"events_a","of":"events",
         "bounds":{"kind":"list","values":[{"kind":"int","value":1}]}},
        {"op":"createPartition","name":"events_rest","of":"events",
         "bounds":{"kind":"default"}}]}
    ]"#;
    let error = refusal(ops, ValidatorDialect::Sqlite);
    assert!(
        error.is_none(),
        "the leg's child must find the top-level parent, not start from empty state: \
         {error:?}"
    );
}
