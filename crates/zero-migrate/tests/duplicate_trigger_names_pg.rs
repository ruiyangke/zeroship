//! A trigger name may not repeat on one table.
//!
//! Found by the habit F717 established: enumerate EVERY op that claims a new
//! name and check each against the model, rather than sweeping for new shapes.
//! Of the seven such ops with no name check at all, six turned out to be
//! unreachable and one was a real defect.
//!
//! MEASURED AGAINST LIVE POSTGRESQL:
//!
//!     CREATE TRIGGER tg ... ON a;  CREATE TRIGGER tg ... ON a;
//!         ERROR: trigger "tg" for relation "a" already exists
//!     CREATE TRIGGER tg ... ON a;  CREATE TRIGGER tg ... ON b;
//!         ACCEPTED
//!
//! PER TABLE, NOT PER SCHEMA. That second line is the control that says so: one
//! audit trigger name reused across many tables is an ordinary pattern, and a
//! schema-scoped rule would refuse all of it. This is the same shape as
//! constraint names rather than the relation/type namespaces of the sibling
//! fixtures.
//!
//! THE OTHER SIX ARE CLOSED BY THE CAPABILITY GATE, not merely unexamined, and
//! that was measured rather than assumed. `createSchema`, `createExtension`,
//! `createRole` and `createPolicy` are privileged vendor primitives: each is
//! refused with VENDOR_OP_DENIED - "unreachable from a confined migration by
//! construction" - before any name check would run.
//!
//! The first draft of this fixture covered policies too, and every policy test
//! was decided by that gate. One of them PASSED, which is worse than failing: an
//! `expect_err` is satisfied by any refusal, so a capability denial reads exactly
//! like the duplicate-name refusal it was supposed to be proving. The mixed
//! pass/fail pattern across sibling controls is what exposed it.
//!
//! SCOPE: PostgreSQL, where the per-table scoping was measured. The other
//! dialects scope trigger names differently and are not covered by this check.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(ops: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres, &[]).map_err(|e| format!("{}: {}", e.code, e.reason))
}

const A: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true}],"primaryKey":["c0"]}"#;
const B: &str = r#"{"op":"createTable","name":"b","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"]}"#;

fn trigger(name: &str, table: &str) -> String {
    format!(
        r#"{{"op":"createTrigger","name":"{name}","table":"{table}","timing":"after","events":["insert"],"forEach":"row","action":{{"kind":"executeFunction","name":"fn_a"}}}}"#
    )
}

#[test]
fn the_same_trigger_name_twice_on_one_table_is_refused() {
    let refusal = verdict(&format!(
        "{A},{},{}",
        trigger("tg", "a"),
        trigger("tg", "a")
    ))
    .expect_err("PostgreSQL rejects a repeated trigger name on one relation");
    assert!(
        refusal.to_lowercase().contains("already"),
        "the refusal must be about the name, not a capability denial: {refusal}"
    );
    assert!(
        !refusal.contains("VENDOR_OP_DENIED"),
        "a capability denial would satisfy expect_err while proving nothing: {refusal}"
    );
}

// ---------------------------------------------------------------------------
// Controls.
// ---------------------------------------------------------------------------

#[test]
fn the_same_trigger_name_on_two_tables_is_still_allowed() {
    // MEASURED: accepted. This is the line that makes the rule per-table.
    verdict(&format!(
        "{A},{B},{},{}",
        trigger("tg", "a"),
        trigger("tg", "b")
    ))
    .expect("trigger names are scoped per table");
}

#[test]
fn distinct_trigger_names_on_one_table_are_still_allowed() {
    verdict(&format!(
        "{A},{},{}",
        trigger("t1", "a"),
        trigger("t2", "a")
    ))
    .expect("two differently named triggers on one table are ordinary");
}

#[test]
fn dropping_a_trigger_frees_its_name() {
    verdict(&format!(
        r#"{A},{},{{"op":"dropTrigger","name":"tg","table":"a"}},{}"#,
        trigger("tg", "a"),
        trigger("tg", "a")
    ))
    .expect("drop then recreate under the same name is a real pattern");
}

#[test]
fn dropping_the_table_frees_its_trigger_names() {
    verdict(&format!(
        r#"{A},{},{{"op":"dropTable","table":"a"}},{A},{}"#,
        trigger("tg", "a"),
        trigger("tg", "a")
    ))
    .expect("a recreated table starts with a clean trigger namespace");
}

#[test]
fn the_privileged_name_claiming_ops_are_closed_by_the_capability_gate() {
    // Pins the reason the other six are not checked here. If a capability set
    // ever grants these by default, this test fails and the gap becomes real.
    for (label, op) in [
        ("schema", r#"{"op":"createSchema","name":"s"}"#),
        ("extension", r#"{"op":"createExtension","name":"citext"}"#),
        ("role", r#"{"op":"createRole","name":"r"}"#),
    ] {
        let refusal = verdict(op).expect_err(&format!(
            "create{label} must be refused before any name check"
        ));
        assert!(
            refusal.contains("VENDOR_OP_DENIED"),
            "the {label} op must be closed by the CAPABILITY gate specifically, since that \
             is the reason this fixture does not check its names: {refusal}"
        );
    }
}
