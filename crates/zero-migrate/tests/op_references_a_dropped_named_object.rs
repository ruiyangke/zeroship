//! A column referencing an enum, domain or sequence dropped earlier in the same
//! migration is refused.
//!
//! The third member of the family whose table and column cases are fixed in
//! `op_after_rename_targets_old_name.rs` and `op_references_a_dropped_column.rs`:
//! a name stops being valid partway through an envelope, and a later operation
//! still uses it.
//!
//! Measured before the fix, and confirmed against the server:
//!
//!     CREATE TYPE "prj_ir"."e" AS ENUM ('a', 'b')
//!     DROP TYPE "prj_ir"."e"
//!     CREATE TABLE "prj_ir"."t" (..., "v" "prj_ir"."e")
//!         ERROR: type "objdrop.e" does not exist
//!
//!     CREATE SEQUENCE "prj_ir"."sq"
//!     DROP SEQUENCE "prj_ir"."sq"
//!     CREATE TABLE "prj_ir"."t" (... DEFAULT nextval('sq'::regclass))
//!         ERROR: relation "objdrop.sq" does not exist
//!
//! COVERAGE IS BOUNDED, as in the column case: this reaches type and sequence
//! names carried as plain identifiers on a column - a column's declared type and
//! a `nextval` default. COLUMN names buried in expressions are reached by
//! `expr_references_a_dropped_column.rs`; TYPE names inside an expression (a cast
//! to a dropped domain, say) are not.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(ops: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres).map_err(|e| format!("{}: {}", e.code, e.reason))
}

/// Assert the KIND of object the refusal names.
///
/// The three type/default sites here differ only in whether an enum, a domain or
/// a sequence was dropped, and the rule reports the kind. Without it the domain
/// test is satisfied by the enum refusal and neither proves the kind is tracked
/// separately - which is the one thing this fixture exists to show.
fn expect_dependency_on(ops: &str, kind: &str, name: &str, what: &str) {
    let refusal = verdict(ops).expect_err(what);
    let expected = format!("depends on {kind} {name:?}");
    assert!(
        refusal.contains(&expected),
        "{expected:?} is missing, so a sibling kind is satisfying this test: {refusal}"
    );
}

#[test]
fn a_column_typed_by_a_dropped_enum_is_refused() {
    let refusal = verdict(
        r#"{"op":"createEnum","name":"e","values":["a","b"]},{"op":"dropEnum","name":"e"},{"op":"createTable","name":"t","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":{"enum":{"name":"e"}},"nullable":true}],"primaryKey":["c0"]}"#,
    )
    .expect_err("the column names an enum type the drop removed");
    assert!(
        refusal.to_lowercase().contains("drop"),
        "the refusal must point at the drop that removed it: {refusal}"
    );
}

#[test]
fn a_column_typed_by_a_dropped_domain_is_refused() {
    expect_dependency_on(
        r#"{"op":"createDomain","name":"d","as":"text"},{"op":"dropDomain","name":"d"},{"op":"createTable","name":"t","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":{"domain":{"name":"d"}},"nullable":true}],"primaryKey":["c0"]}"#,
        "domain",
        "d",
        "the column names a domain type the drop removed",
    );
}

#[test]
fn a_default_on_a_dropped_sequence_is_refused() {
    expect_dependency_on(
        r#"{"op":"createSequence","name":"sq"},{"op":"dropSequence","name":"sq"},{"op":"createTable","name":"t","columns":[{"name":"c0","type":"bigInt","nullable":false,"default":{"nextval":{"name":"sq"}}}],"primaryKey":["c0"]}"#,
        "sequence",
        "sq",
        "the default draws from a sequence the drop removed",
    );
}

#[test]
fn using_these_objects_without_dropping_them_is_still_allowed() {
    // The control. Create and use is the ordinary case for all three kinds.
    verdict(
        r#"{"op":"createEnum","name":"e","values":["a","b"]},{"op":"createSequence","name":"sq"},{"op":"createTable","name":"t","columns":[{"name":"c0","type":"bigInt","nullable":false,"default":{"nextval":{"name":"sq"}}},{"name":"v","type":{"enum":{"name":"e"}},"nullable":true}],"primaryKey":["c0"]}"#,
    )
    .expect("creating an enum and a sequence and then using them must pass");
}

#[test]
fn recreating_a_dropped_object_before_use_is_still_allowed() {
    // Drop and recreate is how an enum's value set is replaced, and the recreated
    // type must be usable — the same restoring move the table and column checks
    // both needed.
    verdict(
        r#"{"op":"createEnum","name":"e","values":["a"]},{"op":"dropEnum","name":"e"},{"op":"createEnum","name":"e","values":["a","b"]},{"op":"createTable","name":"t","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":{"enum":{"name":"e"}},"nullable":true}],"primaryKey":["c0"]}"#,
    )
    .expect("dropping an enum and recreating it before use must remain allowed");
}

#[test]
fn dropping_an_object_nothing_later_uses_is_still_allowed() {
    verdict(
        r#"{"op":"createEnum","name":"e","values":["a"]},{"op":"dropEnum","name":"e"},{"op":"createTable","name":"t","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"]}"#,
    )
    .expect("a drop with no later reference is ordinary cleanup");
}

// ---------------------------------------------------------------------------
// Views, added as a KIND on the same map rather than as another walk.
// ---------------------------------------------------------------------------

#[test]
fn commenting_on_a_dropped_view_is_refused() {
    // Measured before the fix:
    //     DROP VIEW "prj_ir"."vw"
    //     COMMENT ON VIEW "prj_ir"."vw" IS 'x'
    let refusal = verdict(
        r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"]},{"op":"createView","name":"vw","query":{"kind":"structured","select":{"from":{"name":"a"},"projection":[{"kind":"colRef","name":"c0"}]}}},{"op":"dropView","name":"vw"},{"op":"comment","target":{"kind":"view","name":"vw"},"comment":"x"}"#,
    )
    .expect_err("the comment names a view the drop removed");
    assert!(
        refusal.to_lowercase().contains("drop"),
        "the refusal must point at the drop that removed it: {refusal}"
    );
}

#[test]
fn commenting_on_a_live_view_is_still_allowed() {
    verdict(
        r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"]},{"op":"createView","name":"vw","query":{"kind":"structured","select":{"from":{"name":"a"},"projection":[{"kind":"colRef","name":"c0"}]}}},{"op":"comment","target":{"kind":"view","name":"vw"},"comment":"x"}"#,
    )
    .expect("commenting on a view that exists is ordinary");
}

#[test]
fn recreating_a_dropped_view_before_commenting_is_still_allowed() {
    verdict(
        r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"]},{"op":"createView","name":"vw","query":{"kind":"structured","select":{"from":{"name":"a"},"projection":[{"kind":"colRef","name":"c0"}]}}},{"op":"dropView","name":"vw"},{"op":"createView","name":"vw","query":{"kind":"structured","select":{"from":{"name":"a"},"projection":[{"kind":"colRef","name":"c0"}]}}},{"op":"comment","target":{"kind":"view","name":"vw"},"comment":"x"}"#,
    )
    .expect("dropping a view and recreating it before use must remain allowed");
}
