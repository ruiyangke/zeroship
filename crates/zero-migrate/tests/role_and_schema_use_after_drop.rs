//! A dropped role or schema may not be named by a later operation.
//!
//! These are the two shapes F762 measured, left unfixed, and named in its own
//! fixture header as open work rather than quietly dropping them. They needed
//! namespaces the walk did not track: every other member of this family is a
//! relation or a type.
//!
//! EVERY NAMING SITE WAS MEASURED, one statement per site, rather than fixing the
//! two that prompted the work and assuming the rest:
//!
//!     DROP ROLE r; GRANT SELECT ON t TO r            role "r" does not exist
//!     DROP ROLE r; REVOKE SELECT ON t FROM r         role "r" does not exist
//!     DROP ROLE r; ALTER ROLE r RESET search_path    role "r" does not exist
//!     DROP ROLE r; CREATE SCHEMA s AUTHORIZATION r   role "r" does not exist
//!     DROP ROLE r; CREATE POLICY p ON t TO r …       role "r" does not exist
//!     DROP ROLE r; DROP OWNED BY r                   role "r" does not exist
//!     DROP SCHEMA s; CREATE TABLE s.x (…)            schema "s" does not exist
//!
//! And the control that bounds the role rule:
//!
//!     GRANT SELECT ON t TO PUBLIC                    ACCEPTED
//!
//! `"public"` IS NOT A ROLE REFERENCE in a grantee list - it is the reserved
//! `PUBLIC` sentinel, so it can never fail to resolve and is excluded from the
//! check. Nothing can successfully drop `public`, so including it would be
//! harmless in practice; it is excluded because the alternative encodes the wrong
//! meaning for whoever reads the rule next.
//!
//! THE SCHEMA SIDE REUSES `Op::schema()`, which is already exhaustive over the
//! closed op set for the confined cross-schema gate. That matters more than the
//! saved code: a new op variant must consciously declare whether it carries a
//! schema, so it cannot slip past this check the way `addColumn` slipped past
//! F762's column walk or `grant` slipped past F761's single-name accessor. Three
//! consecutive defects in this family came from a collection that could not see
//! every op; this one borrows a collection that can.

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
    validate_ir_authorized(&ir, Dialect::Postgres, None, Some(authority))
        .map_err(|e| format!("{}: {}", e.code, e.reason))
}

fn expect_use_after_drop(ops: &str, what: &str) {
    let refusal = verdict(ops).expect_err(what);
    assert!(
        !refusal.contains("VENDOR_OP_DENIED"),
        "this must be refused for naming a dropped object, not by the capability \
         gate - a denial would satisfy expect_err while proving nothing: {refusal}"
    );
    assert!(
        refusal.contains("will not exist when this runs"),
        "the refusal must be the use-after-drop one: {refusal}"
    );
}

const T: &str = r#"{"op":"createTable","name":"t","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"]}"#;
const MAKE_ROLE: &str = r#"{"op":"createRole","name":"r"}"#;
const DROP_ROLE: &str = r#"{"op":"dropRole","name":"r"}"#;
const MAKE_SCHEMA: &str = r#"{"op":"createSchema","name":"s"}"#;
const DROP_SCHEMA: &str = r#"{"op":"dropSchema","name":"s"}"#;

fn grant_to(role: &str) -> String {
    format!(
        r#"{{"op":"grant","privileges":["select"],"on":{{"kind":"table","names":["t"]}},"to":["{role}"]}}"#
    )
}

fn table_in(schema: &str) -> String {
    format!(
        r#"{{"op":"createTable","name":"x","schema":"{schema}","columns":[{{"name":"c0","type":"int","nullable":false}}],"primaryKey":["c0"]}}"#
    )
}

// ---------------------------------------------------------------------------
// Refusals - one per measured naming site.
// ---------------------------------------------------------------------------

#[test]
fn granting_to_a_dropped_role_is_refused() {
    expect_use_after_drop(
        &format!("{MAKE_ROLE},{T},{DROP_ROLE},{}", grant_to("r")),
        "the role is gone, so the grantee cannot resolve",
    );
}

#[test]
fn revoking_from_a_dropped_role_is_refused() {
    expect_use_after_drop(
        &format!(
            r#"{MAKE_ROLE},{T},{DROP_ROLE},{{"op":"revoke","privileges":["select"],"on":{{"kind":"table","names":["t"]}},"from":["r"]}}"#
        ),
        "the role is gone, so the revokee cannot resolve",
    );
}

#[test]
fn altering_a_dropped_role_is_refused() {
    expect_use_after_drop(
        &format!(
            r#"{MAKE_ROLE},{DROP_ROLE},{{"op":"alterRole","name":"r","resetSearchPath":true}}"#
        ),
        "the role is gone, so the alter cannot resolve it",
    );
}

#[test]
fn authorizing_a_schema_to_a_dropped_role_is_refused() {
    // A role named somewhere other than a grantee list. An implementation that
    // only walked `grant`/`revoke` passes the first two and fails this.
    expect_use_after_drop(
        &format!(
            r#"{MAKE_ROLE},{DROP_ROLE},{{"op":"createSchema","name":"s2","authorization":"r"}}"#
        ),
        "the authorization role is gone",
    );
}

#[test]
fn a_policy_naming_a_dropped_role_is_refused() {
    expect_use_after_drop(
        &format!(
            r#"{MAKE_ROLE},{T},{DROP_ROLE},{{"op":"createPolicy","name":"p","table":"t","forCmd":"all","to":["r"],"using":{{"node":"binOp","op":"gt","lhs":{{"node":"colRef","name":"c0"}},"rhs":{{"node":"literal","value":0}}}}}}"#
        ),
        "the policy's role is gone",
    );
}

#[test]
fn dropping_objects_owned_by_a_dropped_role_is_refused() {
    expect_use_after_drop(
        &format!(r#"{MAKE_ROLE},{DROP_ROLE},{{"op":"dropOwnedBy","roles":["r"]}}"#),
        "the owner role is gone",
    );
}

#[test]
fn creating_a_table_in_a_dropped_schema_is_refused() {
    expect_use_after_drop(
        &format!("{MAKE_SCHEMA},{DROP_SCHEMA},{}", table_in("s")),
        "the schema is gone, so the qualified table cannot be created",
    );
}

// ---------------------------------------------------------------------------
// Controls.
// ---------------------------------------------------------------------------

#[test]
fn granting_to_public_is_still_allowed() {
    // MEASURED: accepted. PUBLIC is a sentinel, not a role that can be dropped.
    // A rule that treated the grantee list as plain role names would still pass
    // this today - nothing can drop `public` - so the test pins the intent.
    verdict(&format!("{T},{}", grant_to("public"))).expect("PUBLIC always resolves");
}

#[test]
fn granting_to_a_live_role_is_still_allowed() {
    verdict(&format!("{MAKE_ROLE},{T},{}", grant_to("r")))
        .expect("granting to a role this migration created is the ordinary case");
}

#[test]
fn recreating_the_role_before_granting_is_still_allowed() {
    verdict(&format!(
        "{MAKE_ROLE},{T},{DROP_ROLE},{MAKE_ROLE},{}",
        grant_to("r")
    ))
    .expect("the recreated role resolves");
}

#[test]
fn granting_to_a_role_this_migration_never_touched_is_still_allowed() {
    // The engine cannot know which roles already exist on the server, so an
    // unmentioned role must pass. Only a role THIS envelope dropped is decidable.
    verdict(&format!("{T},{}", grant_to("someone_else")))
        .expect("a pre-existing role is none of this rule's business");
}

#[test]
fn recreating_the_schema_before_using_it_is_still_allowed() {
    verdict(&format!(
        "{MAKE_SCHEMA},{DROP_SCHEMA},{MAKE_SCHEMA},{}",
        table_in("s")
    ))
    .expect("the recreated schema resolves");
}

#[test]
fn a_different_schema_is_unaffected_by_the_drop() {
    verdict(&format!(
        "{MAKE_SCHEMA},{DROP_SCHEMA},{}",
        table_in("public")
    ))
    .expect("dropping one schema says nothing about another");
}

#[test]
fn dropping_the_schema_last_leaves_earlier_operations_alone() {
    // Order is the whole point: the same two ops in the other order are fine.
    verdict(&format!("{MAKE_SCHEMA},{},{DROP_SCHEMA}", table_in("s")))
        .expect("a drop after the use refuses nothing");
}
