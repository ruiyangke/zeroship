//! A partition is a relation: it claims both namespaces, like any table.
//!
//! `createPartition` is a `CREATE TABLE ... PARTITION OF`, so it claims the
//! relation namespace exactly as a table does. It has a composite row type, so
//! it claims the type namespace too: a name already held by a table collides
//! first as `type "..." already exists`, because the type check runs before the
//! relation one.
//!
//! DETACH DOES NOT FREE THE NAME, and that is the control that shapes the rule. A
//! detached partition becomes a standalone TABLE under the same name, so the name
//! stays occupied. Only `dropPartition` releases it. Treating `detachPartition`
//! as a release would wrongly accept a later create.

use crate::support;

use zeroship_migrate::model::ir::MigrationIr;
use zeroship_migrate::model::validate::validate_ir;

fn verdict(ops: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(
        zeroship_migrate::shipping_vendors(),
        &ir,
        &zeroship_migrate_postgres::DIALECT,
    )
    .map_err(|e| format!("{}: {}", e.code, e.reason))
}

const PARENT: &str = r#"{"op":"createTable","name":"par","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"],"partitionBy":{"kind":"range","columns":["c0"]}}"#;

fn part(name: &str, from: i32, to: i32) -> String {
    format!(
        r#"{{"op":"createPartition","name":"{name}","of":"par","bounds":{{"kind":"range","from":[{{"kind":"int","value":{from}}}],"to":[{{"kind":"int","value":{to}}}]}}}}"#
    )
}
fn tbl(n: &str) -> String {
    format!(
        r#"{{"op":"createTable","name":"{n}","columns":[{{"name":"c0","type":"int","nullable":false}}],"primaryKey":["c0"]}}"#
    )
}

/// Assert the refusal is the one the test names, not merely that one happened.
///
/// A bare `expect_err` cannot tell the two apart: a sibling rule can satisfy the
/// assertion while the named rule does nothing - see the two `createPartition`
/// tests.
fn expect_refusal_mentioning(ops: &str, needles: &[&str], what: &str) -> String {
    let refusal = verdict(ops).expect_err(what);
    for needle in needles {
        assert!(
            refusal.contains(needle),
            "the refusal must be the one this test names and {needle:?} is missing, \
             so a sibling rule is satisfying it instead: {refusal}"
        );
    }
    refusal
}

// A `createPartition` claims BOTH namespaces, and the TYPE check runs first, so
// every refusal aimed at a partition's own name arrives from the type rule. The
// relation half is real and is covered from the other direction, by the
// `createTable` tests below - that asymmetry is why these two say "type
// namespace" while the file is named for the relation one.
const TYPE_NS: &str = "one type namespace";
const RELATION_NS: &str = "nothing dropped or renamed it in between";

#[test]
fn the_same_partition_name_twice_is_refused() {
    // The type rule fires here, not the relation one. Deleting partition tracking
    // from the relation namespace entirely would leave this test green, which is
    // why the `createTable` direction below carries the relation half.
    expect_refusal_mentioning(
        &format!("{PARENT},{},{}", part("p1", 0, 10), part("p1", 10, 20)),
        &[
            TYPE_NS,
            "this createPartition claims",
            "already created a partition",
        ],
        "the second createPartition retakes the name",
    );
}

#[test]
fn a_partition_may_not_take_a_live_table_name() {
    // The engine answers from the type namespace, because the table's composite
    // row type is what the partition collides with first.
    expect_refusal_mentioning(
        &format!("{PARENT},{},{}", tbl("b"), part("b", 0, 10)),
        &[
            TYPE_NS,
            "this createPartition claims",
            "already created a table",
        ],
        "a partition may not take a live table name",
    );
}

#[test]
fn a_table_may_not_take_a_live_partition_name() {
    // THE TEST THAT ACTUALLY COVERS THIS FILE'S TITLE. The reverse direction is
    // decided by the relation namespace, so this is the one that would fail if
    // partition tracking were removed from it.
    expect_refusal_mentioning(
        &format!("{PARENT},{},{}", part("p1", 0, 10), tbl("p1")),
        &[
            RELATION_NS,
            "this createTable claims",
            "already created a partition",
        ],
        "a partition occupies the name against a later table",
    );
}

#[test]
fn a_partition_may_not_take_a_live_enum_name() {
    expect_refusal_mentioning(
        &format!(
            r#"{PARENT},{{"op":"createEnum","name":"e","values":["a"]}},{}"#,
            part("e", 0, 10)
        ),
        &[
            TYPE_NS,
            "this createPartition claims",
            "already created a enum",
        ],
        "a partition has a composite row type, so it needs the type name free",
    );
}

#[test]
fn an_enum_may_not_take_a_live_partition_name() {
    expect_refusal_mentioning(
        &format!(
            r#"{PARENT},{},{{"op":"createEnum","name":"p1","values":["a"]}}"#,
            part("p1", 0, 10)
        ),
        &[
            TYPE_NS,
            "this createEnum claims",
            "already created a partition",
        ],
        "the partition row type occupies the type namespace",
    );
}

// ---------------------------------------------------------------------------
// Controls.
// ---------------------------------------------------------------------------

#[test]
fn distinct_partition_names_are_still_allowed() {
    verdict(&format!(
        "{PARENT},{},{}",
        part("p1", 0, 10),
        part("p2", 10, 20)
    ))
    .expect("two partitions of one parent with distinct names are the ordinary case");
}

#[test]
fn dropping_a_partition_frees_its_name() {
    verdict(&format!(
        r#"{PARENT},{},{{"op":"dropPartition","parent":"par","name":"p1"}},{}"#,
        part("p1", 0, 10),
        part("p1", 10, 20)
    ))
    .expect("dropping a partition releases its relation name");
}

#[test]
fn detaching_a_partition_does_not_free_its_name() {
    // THE CONTROL THAT SHAPES THE FIX. A detached partition becomes a standalone
    // TABLE under the same name, so the name is still taken. Treating detach as a
    // release would wrongly accept this.
    expect_refusal_mentioning(
        &format!(
            r#"{PARENT},{},{{"op":"detachPartition","parent":"par","name":"p1"}},{}"#,
            part("p1", 0, 10),
            tbl("p1")
        ),
        &[
            RELATION_NS,
            "this createTable claims",
            "already created a partition",
        ],
        "a detached partition still occupies its name as a standalone table",
    );
}

#[test]
fn a_partition_after_dropping_the_colliding_table_is_allowed() {
    verdict(&format!(
        r#"{PARENT},{},{{"op":"dropTable","table":"b"}},{}"#,
        tbl("b"),
        part("b", 0, 10)
    ))
    .expect("the name is free once the table is dropped");
}

// ---------------------------------------------------------------------------
// Dropping the PARENT releases its partitions' names too.
// ---------------------------------------------------------------------------

/// A partition is a dependent object, so dropping the partitioned parent drops
/// its partitions with it - and their names become free again.
///
/// The relation map must release a partition when its parent is dropped. If it
/// releases `par` but keeps `p1`, a later claim on the freed name is refused,
/// which is the failure mode `new_rules_do_not_over_refuse.rs` exists to
/// prevent. No single-rule test can surface it: it takes a drop AND a later
/// claim in the same envelope.
#[test]
fn dropping_the_parent_frees_its_partitions_names() {
    verdict(&format!(
        r#"{PARENT},{},{{"op":"dropTable","table":"par"}},{}"#,
        part("p1", 0, 10),
        tbl("p1")
    ))
    .expect("dropping the parent drops p1 with it, so the name is free again");
}

#[test]
fn a_recreated_parent_may_take_the_same_partition_names() {
    // The whole point of the shape: tear the partitioned table down and rebuild
    // it with the same partition layout.
    verdict(&format!(
        r#"{PARENT},{},{{"op":"dropTable","table":"par"}},{PARENT},{}"#,
        part("p1", 0, 10),
        part("p1", 0, 10)
    ))
    .expect("rebuilding a partitioned table with the same partition names is ordinary");
}

#[test]
fn dropping_an_unrelated_table_does_not_free_a_partition_name() {
    // THE CONTROL. Releasing every partition on any drop would pass the two tests
    // above and lose the parent-tracking protection.
    expect_refusal_mentioning(
        &format!(
            r#"{PARENT},{},{},{{"op":"dropTable","table":"other"}},{}"#,
            tbl("other"),
            part("p1", 0, 10),
            part("p1", 10, 20)
        ),
        // The later op is a createPartition, so the type rule answers - the same
        // asymmetry the two headline tests document.
        &[
            TYPE_NS,
            "this createPartition claims",
            "already created a partition",
        ],
        "an unrelated drop must not release a live partition name",
    );
}

/// A rename must carry the partition's parentage.
///
/// See the sibling test in `index_shares_the_relation_namespace.rs` for the
/// reasoning.
#[test]
fn a_rename_carries_the_partition_parentage_so_a_later_drop_still_frees_it() {
    verdict(&format!(
        r#"{PARENT},{},{{"op":"renameTable","table":"par","to":"par2"}},{{"op":"dropTable","table":"par2"}},{}"#,
        part("p1", 0, 10),
        tbl("p1")
    ))
    .expect("the partition went with the renamed parent when it was dropped");
}

/// A DETACHED partition stops being a dependent, so dropping its former parent
/// must NOT free its name.
///
/// `detachPartition` does not release the name from the relation map, and it
/// must also remove the partition from its parent's DEPENDENTS: otherwise the
/// later `dropTable` releases a name the database still holds, because a
/// detached partition is a standalone TABLE under the same name.
#[test]
fn detaching_then_dropping_the_parent_does_not_free_the_detached_name() {
    expect_refusal_mentioning(
        &format!(
            r#"{PARENT},{},{{"op":"detachPartition","parent":"par","name":"p1"}},{{"op":"dropTable","table":"par"}},{}"#,
            part("p1", 0, 10),
            tbl("p1")
        ),
        &[
            RELATION_NS,
            "this createTable claims",
            "already created a partition",
        ],
        "a detached partition survives its former parent, so its name is still taken",
    );
}

/// ATTACH is the mirror of detach, and the last lifecycle event that can touch
/// the parentage map.
///
/// Attaching an existing table makes it a dependent: dropping the parent drops
/// it too, and the name becomes reusable.
///
/// REACHABILITY: `attachPartition` is NOT portable core. `vendor_capabilities`
/// lists `createPartition`, `detachPartition` and `dropPartition` as requiring
/// no capability, while attach requires `partition` - absorbing an EXISTING
/// table is privileged in a way creating a fresh one is not. So a confined
/// migration cannot reach this at all, and the test authorises itself with the
/// operator charter to exercise the path a privileged migration takes.
#[test]
fn attaching_a_table_makes_it_a_dependent_of_the_parent() {
    use zeroship_migrate::model::validate::{validate_ir_authorized, VendorAuthority};

    let policy = support::operator_charter("public");
    let check = |ops: &str| {
        let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
        let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
        let authority = VendorAuthority {
            effective: &policy,
            default_schema: "public",
        };
        validate_ir_authorized(
            zeroship_migrate::shipping_vendors(),
            &ir,
            &zeroship_migrate_postgres::DIALECT,
            None,
            Some(authority),
        )
        .map_err(|e| format!("{}: {}", e.code, e.reason))
    };

    let attach = r#"{"op":"attachPartition","parent":"par","name":"t","bound":{"kind":"range","from":[{"kind":"int","value":0}],"to":[{"kind":"int","value":10}]}}"#;
    let t = tbl("t");

    check(&format!(
        r#"{PARENT},{t},{attach},{{"op":"dropTable","table":"par"}},{t}"#
    ))
    .expect("the attached table went with its parent, so the name is free");

    // THE CONTROL: without the drop the attached table is still there.
    let refusal = check(&format!(r#"{PARENT},{t},{attach},{t}"#))
        .expect_err("the attached table is still live, so its name is still taken");
    assert!(
        refusal.contains(RELATION_NS),
        "the refusal must be the relation-namespace one this control names: {refusal}"
    );
}
