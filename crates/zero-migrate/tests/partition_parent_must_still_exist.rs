//! A partition op naming a PARENT that an earlier op removed is refused.
//!
//! The use-after-drop check compares ONE name per op, via `touched_table()`. For
//! every partition op that name is the PARTITION's own name - so the PARENT was
//! never checked by anything.
//!
//! MEASURED, both accepted at the gate and both rejected by live PostgreSQL:
//!
//!     DROP TABLE par;
//!     CREATE TABLE p1 PARTITION OF par ...     relation "pp.par" does not exist
//!
//!     ALTER TABLE par2 RENAME TO par3;
//!     CREATE TABLE q1 PARTITION OF par2 ...    relation "pp.par2" does not exist
//!
//! HOW THIS WAS FOUND, because the route matters more than the bug. The previous
//! commit asserted in its message that "createPartition is the only other op that
//! defines a relation". That was a claim about `touched_table()` that I had not
//! actually read. Reading it showed the claim was TRUE but arrived at for
//! incomplete reasons - four partition ops report a `name` rather than a `table`,
//! and I had reasoned about one of them. The other three are correct as they
//! stand (attach, detach and drop all REQUIRE the name they report). What the
//! reading turned up instead was that NONE of them report the parent.
//!
//! A one-name-per-op check cannot see a second name by construction. That is not
//! a bug in `touched_table`; it is the boundary of what that walk can answer, and
//! the parent needs asking separately.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(ops: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres, &[]).map_err(|e| format!("{}: {}", e.code, e.reason))
}

const PARENT: &str = r#"{"op":"createTable","name":"par","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"],"partitionBy":{"kind":"range","columns":["c0"]}}"#;
const P1: &str = r#"{"op":"createPartition","name":"p1","of":"par","bounds":{"kind":"range","from":[{"kind":"int","value":0}],"to":[{"kind":"int","value":10}]}}"#;

#[test]
fn a_partition_of_a_dropped_parent_is_refused() {
    let refusal = verdict(&format!(
        r#"{PARENT},{{"op":"dropTable","table":"par"}},{P1}"#
    ))
    .expect_err("the parent was dropped earlier in this migration");
    assert!(
        refusal.to_lowercase().contains("drop"),
        "the refusal must point at the drop that removed the parent: {refusal}"
    );
}

#[test]
fn a_partition_of_a_renamed_away_parent_is_refused() {
    let refusal = verdict(&format!(
        r#"{PARENT},{{"op":"renameTable","table":"par","to":"par2"}},{P1}"#
    ))
    .expect_err("the parent no longer exists under that name");
    assert!(
        refusal.to_lowercase().contains("rename"),
        "the refusal must point at the rename, not merely a missing table: {refusal}"
    );
}

// ---------------------------------------------------------------------------
// Controls.
// ---------------------------------------------------------------------------

#[test]
fn a_partition_of_a_live_parent_is_still_allowed() {
    verdict(&format!("{PARENT},{P1}")).expect("the ordinary case must pass");
}

#[test]
fn a_partition_of_the_parents_new_name_is_still_allowed() {
    // Renaming a parent and then adding a partition under the NEW name is the
    // normal way to continue after a rename.
    verdict(&format!(
        r#"{PARENT},{{"op":"renameTable","table":"par","to":"par2"}},{{"op":"createPartition","name":"p1","of":"par2","bounds":{{"kind":"range","from":[{{"kind":"int","value":0}}],"to":[{{"kind":"int","value":10}}]}}}}"#
    ))
    .expect("continuing against the new parent name is normal");
}

#[test]
fn a_partition_of_a_parent_recreated_after_the_drop_is_allowed() {
    // The restoring move, which every member of this family has needed.
    verdict(&format!(
        r#"{PARENT},{{"op":"dropTable","table":"par"}},{PARENT},{P1}"#
    ))
    .expect("a recreated parent can take partitions again");
}

#[test]
fn dropping_an_unrelated_table_does_not_block_the_partition() {
    verdict(&format!(
        r#"{PARENT},{{"op":"createTable","name":"other","columns":[{{"name":"c0","type":"int","nullable":false}}],"primaryKey":["c0"]}},{{"op":"dropTable","table":"other"}},{P1}"#
    ))
    .expect("an unrelated drop must not block a partition");
}
