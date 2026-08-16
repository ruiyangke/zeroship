//! `grant` and `revoke` may not name a table this migration dropped or renamed
//! away.
//!
//! HOW THE GAP SURVIVED. `validate_no_op_targets_a_renamed_away_table` is driven
//! by `Op::touched_table()`, which returns ONE name. `grant` and `revoke` carry
//! `GrantTarget::Table { names: Vec<String> }`, so a single-name accessor has
//! nothing to return for them and they walked past a rule that already covered
//! every other op in the engine. The shape of the accessor, not an oversight
//! about the ops, is what left the hole.
//!
//! MEASURED AGAINST LIVE POSTGRESQL, with the controls that bound the rule:
//!
//!     CREATE TABLE a; DROP TABLE a; GRANT SELECT ON a TO r
//!         ERROR: relation "a" does not exist
//!     ... the same for REVOKE
//!     CREATE TABLE c; ALTER TABLE c RENAME TO c2; GRANT SELECT ON c TO r
//!         ERROR: relation "c" does not exist
//!     GRANT SELECT ON c2 TO r                                       ACCEPTED
//!     CREATE SCHEMA x; DROP SCHEMA x; GRANT USAGE ON SCHEMA x TO r
//!         ERROR: schema "x" does not exist
//!
//! ONLY THE TABLE TARGET IS CHECKED. That last line is a real error the engine
//! still does not catch, and leaving it uncaught is deliberate: this walk tracks
//! relations, not schemas, so a schema target would have to be guessed at. An
//! under-refusal is the same behaviour as before; a wrong refusal would reject a
//! migration the server runs. The control below pins that boundary rather than
//! leaving it implicit.
//!
//! HOW IT WAS FOUND, because the method mattered more than the defect. Rather
//! than wait for a report, every op that targets a table was enumerated and run
//! through `createTable; dropTable; <op>`. The FIRST pass reported all ops
//! refused - a clean result that was wrong, because six of the envelopes had
//! malformed JSON and never reached the engine at all. `grant` and `revoke` were
//! two of the six. A mis-aimed probe returns a plausible answer, and the only
//! defence is to require every case to PARSE before believing any conclusion
//! drawn from the batch.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir_authorized, Dialect, VendorAuthority};

fn verdict(ops: &str) -> Result<(), String> {
    let policy = support::operator_charter("public");
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    let authority = VendorAuthority {
        effective: &policy,
        default_schema: "public",
    };
    validate_ir_authorized(&ir, Dialect::Postgres, &[], None, Some(authority))
        .map_err(|e| format!("{}: {}", e.code, e.reason))
}

fn expect_vacated_refusal(ops: &str, what: &str) {
    let refusal = verdict(ops).expect_err(what);
    assert!(
        !refusal.contains("VENDOR_OP_DENIED"),
        "this must be refused for naming a vacated relation, not by the capability \
         gate - a denial would satisfy expect_err while proving nothing: {refusal}"
    );
    assert!(
        refusal.contains("will not exist under that name"),
        "the refusal must be the use-after-drop one: {refusal}"
    );
}

fn tbl(name: &str) -> String {
    format!(
        r#"{{"op":"createTable","name":"{name}","columns":[{{"name":"c0","type":"int","nullable":false}}],"primaryKey":["c0"]}}"#
    )
}

fn grant_on(names: &str) -> String {
    format!(
        r#"{{"op":"grant","privileges":["select"],"on":{{"kind":"table","names":[{names}]}},"to":["r"]}}"#
    )
}

fn revoke_on(names: &str) -> String {
    format!(
        r#"{{"op":"revoke","privileges":["select"],"on":{{"kind":"table","names":[{names}]}},"from":["r"]}}"#
    )
}

// ---------------------------------------------------------------------------
// Refusals.
// ---------------------------------------------------------------------------

#[test]
fn granting_on_a_dropped_table_is_refused() {
    expect_vacated_refusal(
        &format!(
            r#"{},{{"op":"dropTable","table":"a"}},{}"#,
            tbl("a"),
            grant_on(r#""a""#)
        ),
        "the table is gone, so the GRANT cannot resolve it",
    );
}

#[test]
fn revoking_on_a_dropped_table_is_refused() {
    // REVOKE is a separate op with its own arm; a fix written only into the
    // Grant arm passes the test above and fails this one.
    expect_vacated_refusal(
        &format!(
            r#"{},{{"op":"dropTable","table":"a"}},{}"#,
            tbl("a"),
            revoke_on(r#""a""#)
        ),
        "the table is gone, so the REVOKE cannot resolve it",
    );
}

#[test]
fn granting_on_the_old_name_after_a_rename_is_refused() {
    expect_vacated_refusal(
        &format!(
            r#"{},{{"op":"renameTable","table":"a","to":"a2"}},{}"#,
            tbl("a"),
            grant_on(r#""a""#)
        ),
        "the old name no longer resolves after a rename",
    );
}

#[test]
fn one_dropped_table_in_a_longer_grant_list_is_still_refused() {
    // The list is the whole reason this op escaped the rule, so a single-name
    // check that only looked at `names[0]` must not pass here.
    expect_vacated_refusal(
        &format!(
            r#"{},{},{{"op":"dropTable","table":"b"}},{}"#,
            tbl("a"),
            tbl("b"),
            grant_on(r#""a","b""#)
        ),
        "one vacated name anywhere in the list is enough",
    );
}

// ---------------------------------------------------------------------------
// Controls.
// ---------------------------------------------------------------------------

#[test]
fn granting_on_a_live_table_is_still_allowed() {
    verdict(&format!("{},{}", tbl("a"), grant_on(r#""a""#)))
        .expect("granting on a table this migration created is the ordinary case");
}

#[test]
fn granting_on_several_live_tables_is_still_allowed() {
    verdict(&format!(
        "{},{},{}",
        tbl("a"),
        tbl("b"),
        grant_on(r#""a","b""#)
    ))
    .expect("a multi-table grant with every name live must pass");
}

#[test]
fn granting_on_the_new_name_after_a_rename_is_still_allowed() {
    // MEASURED: accepted. The rename moved the relation, it did not remove it.
    verdict(&format!(
        r#"{},{{"op":"renameTable","table":"a","to":"a2"}},{}"#,
        tbl("a"),
        grant_on(r#""a2""#)
    ))
    .expect("the new name resolves");
}

#[test]
fn granting_after_drop_then_recreate_is_still_allowed() {
    verdict(&format!(
        r#"{},{{"op":"dropTable","table":"a"}},{},{}"#,
        tbl("a"),
        tbl("a"),
        grant_on(r#""a""#)
    ))
    .expect("the name is occupied again, so the grant resolves");
}

#[test]
fn revoking_on_a_live_table_is_still_allowed() {
    verdict(&format!("{},{}", tbl("a"), revoke_on(r#""a""#)))
        .expect("revoking on a live table is ordinary");
}

#[test]
fn a_schema_grant_is_not_confused_with_a_dropped_table_of_that_name() {
    // The boundary this rule deliberately does not cross. `GRANT USAGE ON SCHEMA
    // a` names a schema, and dropping a TABLE called `a` says nothing about it -
    // so this must be accepted even though the table name is vacated. Checking
    // every GrantTarget against the relation map would refuse it.
    verdict(&format!(
        r#"{},{{"op":"dropTable","table":"a"}},{{"op":"grant","privileges":["usage"],"on":{{"kind":"schema","names":["a"]}},"to":["r"]}}"#,
        tbl("a")
    ))
    .expect("a schema target is a different namespace from a table target");
}
