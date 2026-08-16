//! A `dialectal` container no longer hides the ops it holds from the stateful
//! walks.
//!
//! F765 taught the two column ACCESSORS to look inside a container, which fixed
//! reading references out of a leg. It left every walk's STATE blind: a
//! `dropTable`, `dropColumn`, `dropEnum`, `dropRole` or `renameTable` nested in a
//! leg still arrived as an opaque `Dialectal` and mutated nothing. The container
//! was TRANSPARENT FOR READS AND OPAQUE FOR WRITES, which is the worst of the two
//! options - a half-fix that looks like a fix.
//!
//! `effective_ops` now expands containers ONCE, before the walks run, and five
//! walks share it. That is the whole change: the leg is chosen in one place, and
//! a walk cannot be taught to read inside a container while forgetting to write.
//!
//! SEVEN SHAPES WERE ACCEPTED BEFORE AND ARE REFUSED NOW. Five are use-after-drop
//! rules whose measurements are recorded on the fixtures that introduced them:
//!
//!     dropTable  (F761)   relation "a" does not exist
//!     dropColumn (F765)   column "v" does not exist
//!     dropEnum   (F762)   type "e" does not exist
//!     dropRole   (F763)   role "r" does not exist
//!     renameTable(F761)   relation "a" does not exist under the old name
//!
//! The other two are NAME-CLAIM rules, and the distinction matters for what this
//! change claims: those rules were already correct and already measured. The
//! container made them UNREACHABLE, so nothing new is asserted about the server
//! here - an existing rule was restored to the ops it was written for.
//!
//! THE REPORTED INDEX IS THE CONTAINER'S. A nested op has no top-level position,
//! and the author's envelope shows the `dialectal` op at that index, so pointing
//! there is the honest choice rather than inventing a coordinate.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir_authorized, Dialect, VendorAuthority};

fn verdict_on(dialect: Dialect, ops: &str) -> Result<(), String> {
    let policy = support::operator_charter("public");
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    let authority = VendorAuthority {
        effective: &policy,
        default_schema: "public",
    };
    validate_ir_authorized(&ir, dialect, &[], None, Some(authority))
        .map_err(|e| format!("{}: {}", e.code, e.reason))
}

fn verdict(ops: &str) -> Result<(), String> {
    verdict_on(Dialect::Postgres, ops)
}

fn expect_refusal(ops: &str, what: &str) {
    let refusal = verdict(ops).expect_err(what);
    assert!(
        !refusal.contains("VENDOR_OP_DENIED"),
        "this must be refused by the walk, not by the capability gate: {refusal}"
    );
}

const A: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true}],"primaryKey":["c0"]}"#;
const IX: &str =
    r#"{"op":"createIndex","name":"ix","table":"a","columns":[{"kind":"column","name":"v"}]}"#;
const ADD_Z: &str = r#"{"op":"addColumn","table":"a","column":"z","type":"int","nullable":true}"#;

fn pg_leg(inner: &str) -> String {
    format!(r#"{{"op":"dialectal","pg":[{inner}]}}"#)
}

// ---------------------------------------------------------------------------
// Nested state mutations now register.
// ---------------------------------------------------------------------------

#[test]
fn a_nested_drop_table_vacates_the_name() {
    expect_refusal(
        &format!(
            r#"{A},{},{ADD_Z}"#,
            pg_leg(r#"{"op":"dropTable","table":"a"}"#)
        ),
        "the nested drop removed the table",
    );
}

#[test]
fn a_nested_rename_vacates_the_old_name() {
    expect_refusal(
        &format!(
            r#"{A},{},{ADD_Z}"#,
            pg_leg(r#"{"op":"renameTable","table":"a","to":"a2"}"#)
        ),
        "the nested rename moved the table",
    );
}

#[test]
fn a_nested_drop_column_registers() {
    expect_refusal(
        &format!(
            r#"{A},{},{IX}"#,
            pg_leg(r#"{"op":"dropColumn","table":"a","column":"v"}"#)
        ),
        "the nested drop removed the column",
    );
}

#[test]
fn a_nested_drop_enum_registers() {
    expect_refusal(
        &format!(
            r#"{{"op":"createEnum","name":"e","values":["a","b"]}},{A},{},{{"op":"addColumn","table":"a","column":"z","type":{{"enum":{{"name":"e"}}}},"nullable":true}}"#,
            pg_leg(r#"{"op":"dropEnum","name":"e"}"#)
        ),
        "the nested drop removed the enum",
    );
}

#[test]
fn a_nested_drop_role_registers() {
    expect_refusal(
        &format!(
            r#"{{"op":"createRole","name":"r"}},{A},{},{{"op":"grant","privileges":["select"],"on":{{"kind":"table","names":["a"]}},"to":["r"]}}"#,
            pg_leg(r#"{"op":"dropRole","name":"r"}"#)
        ),
        "the nested drop removed the role",
    );
}

// ---------------------------------------------------------------------------
// Nested claims now register too - existing rules restored, not new claims.
// ---------------------------------------------------------------------------

#[test]
fn a_nested_create_table_claims_the_name() {
    expect_refusal(
        &format!("{A},{}", pg_leg(A)),
        "the nested createTable retakes a live name",
    );
}

#[test]
fn a_nested_create_index_claims_the_index_name() {
    expect_refusal(
        &format!("{A},{IX},{}", pg_leg(IX)),
        "the nested createIndex retakes a live index name",
    );
}

// ---------------------------------------------------------------------------
// Controls.
// ---------------------------------------------------------------------------

#[test]
fn a_nested_index_after_a_drop_is_still_allowed() {
    // Drop then recreate is how an index definition is changed, and it must keep
    // working through a container.
    verdict(&format!(
        r#"{A},{IX},{{"op":"dropIndex","name":"ix","table":"a"}},{}"#,
        pg_leg(IX)
    ))
    .expect("the index name was freed before the nested create");
}

#[test]
fn a_leg_for_another_dialect_still_mutates_nothing() {
    // THE BOUNDARY, and the one most easily lost by expanding every leg: under
    // PostgreSQL the `sqlite` leg never runs, so a drop inside it must NOT
    // vacate anything. Expanding all legs would refuse this.
    verdict(&format!(
        r#"{A},{{"op":"dialectal","sqlite":[{{"op":"dropTable","table":"a"}}]}},{ADD_Z}"#
    ))
    .expect("the sqlite leg is not emitted on PostgreSQL");
}

#[test]
fn the_same_envelope_refuses_under_the_dialect_whose_leg_runs() {
    // The other half of that boundary, so the control above cannot pass merely
    // because nothing is expanded at all.
    let ops =
        format!(r#"{A},{{"op":"dialectal","sqlite":[{{"op":"dropTable","table":"a"}}]}},{ADD_Z}"#);
    verdict_on(Dialect::Sqlite, &ops)
        .expect_err("under SQLite that leg runs, so the table really is gone");
}

#[test]
fn a_nested_op_on_untouched_state_is_still_allowed() {
    verdict(&format!("{A},{}", pg_leg(IX)))
        .expect("an ordinary nested index on a live column must pass");
}

#[test]
fn a_container_inside_a_container_is_refused_before_any_of_this_matters() {
    // WRITTEN FIRST as "a container nested in a container is expanded too",
    // asserting that the recursion in `effective_ops` was exercised. It passed -
    // and for the wrong reason. A PRE-EXISTING rule refuses a `dialectal` leg
    // holding another `dialectal` op outright, so the envelope never reaches the
    // walks at all, and the test proved nothing about the recursion.
    //
    // The fold refuses the same shape ("nested dialectal op reached fold"), so
    // the recursion is unreachable by design rather than merely untested. It
    // stays because `effective_ops` is a general helper and should not depend on
    // a rule enforced elsewhere, but no test can claim to exercise it, and this
    // one now pins the real reason instead of a fiction.
    let refusal = verdict(&format!(
        r#"{A},{},{ADD_Z}"#,
        pg_leg(&pg_leg(r#"{"op":"dropTable","table":"a"}"#))
    ))
    .expect_err("a nested container is refused on its own");
    assert!(
        refusal.contains("nested dialectal"),
        "the refusal must be the nested-container rule, not anything this change \
         added: {refusal}"
    );
}
