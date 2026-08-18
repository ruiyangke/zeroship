//! A schema, extension, role or policy name may not be claimed twice.
//!
//! WHY THESE FOUR WERE MISSED, and it is not that nobody looked. F717's sweep
//! enumerated every op that claims a new name and checked each one. These four
//! were examined, found to be refused with `VENDOR_OP_DENIED`, and recorded as
//! "unreachable from a confined migration by construction" - a conclusion
//! `duplicate_trigger_names_pg.rs` still pins in its final test.
//!
//! That conclusion was measured, and it was still wrong. The capability gate
//! hides these ops from a CONFINED migration; it does not make them correct for
//! a PRIVILEGED one. A migration authorised by a granting profile reaches all
//! four, and the engine accepted a duplicate name in every case.
//!
//! The general lesson, which F758 had already stated one defect earlier: WHEN A
//! LEAD DIES ON A CAPABILITY DENIAL, THE DENIAL MAY BE THE PROFILE RATHER THAN
//! THE PROPERTY. Re-probing under the operator charter is what turned four
//! closed leads back into four defects. These tests therefore authorise
//! themselves - a confined probe would report `VENDOR_OP_DENIED` and prove
//! nothing, which is exactly the trap F718 fell into when an `expect_err` was
//! satisfied by a capability denial.
//!
//! MEASURED AGAINST LIVE POSTGRESQL. Second claim plain:
//!
//!     CREATE SCHEMA f759a     x2   ERROR: schema "f759a" already exists
//!     CREATE ROLE f759role    x2   ERROR: role "f759role" already exists
//!     CREATE EXTENSION citext x2   ERROR: extension "citext" already exists
//!     CREATE POLICY p ON a    x2   ERROR: policy "p" for table "a" already exists
//!
//! Second claim carrying IF NOT EXISTS - accepted, with a `skipping` notice:
//!
//!     CREATE SCHEMA IF NOT EXISTS f759b x2                            ACCEPTED
//!     CREATE SCHEMA f759c;      CREATE SCHEMA IF NOT EXISTS f759c     ACCEPTED
//!     CREATE EXTENSION citext;  CREATE EXTENSION IF NOT EXISTS citext ACCEPTED
//!
//! THE EXEMPTION IS THE POINT OF HALF THIS FIXTURE. `createTable` carries no
//! `ifNotExists`, so no sibling rule in this check has ever had to honour one; a
//! rule written by analogy with the table case would refuse three shapes the
//! server accepts. The flag is read off the SECOND op, because what decides the
//! outcome is whether the op making the repeat claim tolerates an occupant.
//!
//! SCOPING, ALSO MEASURED: schemas, extensions and roles are database- or
//! cluster-wide, so their names collide regardless of any schema qualifier;
//! policies are per table, like triggers, and the server says so in its own
//! message. One policy name reused across many tables is ordinary.

use crate::support;

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

/// Every refusal here must be about the NAME. An `expect_err` alone is satisfied
/// by a capability denial, which is the precise way F718 recorded a false green.
fn expect_name_refusal(ops: &str, what: &str) {
    let refusal = verdict(ops).expect_err(what);
    assert!(
        !refusal.contains("VENDOR_OP_DENIED"),
        "this must be refused for the NAME, not by the capability gate - a denial \
         would satisfy expect_err while proving nothing: {refusal}"
    );
    assert!(
        refusal.to_lowercase().contains("already"),
        "the refusal must say the name is already taken: {refusal}"
    );
}

const A: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"]}"#;
const B: &str = r#"{"op":"createTable","name":"b","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"]}"#;

fn policy_on(name: &str, table: &str) -> String {
    format!(
        r#"{{"op":"createPolicy","name":"{name}","table":"{table}","forCmd":"all","using":{{"node":"binOp","op":"gt","lhs":{{"node":"colRef","name":"c0"}},"rhs":{{"node":"literal","value":0}}}}}}"#
    )
}

// ---------------------------------------------------------------------------
// The four refusals.
// ---------------------------------------------------------------------------

#[test]
fn the_same_schema_name_twice_is_refused() {
    expect_name_refusal(
        r#"{"op":"createSchema","name":"s"},{"op":"createSchema","name":"s"}"#,
        "PostgreSQL rejects a repeated schema name",
    );
}

#[test]
fn the_same_extension_twice_is_refused() {
    expect_name_refusal(
        r#"{"op":"createExtension","name":"citext"},{"op":"createExtension","name":"citext"}"#,
        "PostgreSQL rejects a repeated extension",
    );
}

#[test]
fn the_same_role_name_twice_is_refused() {
    expect_name_refusal(
        r#"{"op":"createRole","name":"r"},{"op":"createRole","name":"r"}"#,
        "PostgreSQL rejects a repeated role name",
    );
}

#[test]
fn the_same_policy_name_twice_on_one_table_is_refused() {
    expect_name_refusal(
        &format!("{A},{},{}", policy_on("p", "a"), policy_on("p", "a")),
        "PostgreSQL rejects a repeated policy name on one table",
    );
}

// ---------------------------------------------------------------------------
// Controls: the shapes the server accepts and this rule must not touch.
// ---------------------------------------------------------------------------

#[test]
fn if_not_exists_on_the_second_schema_is_still_allowed() {
    // MEASURED: `NOTICE: schema "f759b" already exists, skipping`, accepted.
    verdict(
        r#"{"op":"createSchema","name":"s"},{"op":"createSchema","name":"s","ifNotExists":true}"#,
    )
    .expect("a guarded second claim is what ifNotExists is for");
}

#[test]
fn if_not_exists_on_both_schemas_is_still_allowed() {
    // The idempotent-bootstrap shape: a migration that may run against a database
    // where the schema already exists.
    verdict(
        r#"{"op":"createSchema","name":"s","ifNotExists":true},{"op":"createSchema","name":"s","ifNotExists":true}"#,
    )
    .expect("two guarded claims are accepted");
}

#[test]
fn if_not_exists_on_the_second_extension_is_still_allowed() {
    verdict(
        r#"{"op":"createExtension","name":"citext"},{"op":"createExtension","name":"citext","ifNotExists":true}"#,
    )
    .expect("a guarded second extension claim is accepted");
}

#[test]
fn if_not_exists_on_the_second_role_is_still_allowed() {
    verdict(r#"{"op":"createRole","name":"r"},{"op":"createRole","name":"r","ifNotExists":true}"#)
        .expect("a guarded second role claim is accepted");
}

#[test]
fn the_same_policy_name_on_two_tables_is_still_allowed() {
    // The line that makes the policy rule per-table rather than per-schema. One
    // `tenant_isolation` policy across every table is the ordinary RLS pattern,
    // and a schema-scoped rule would refuse all of it.
    verdict(&format!(
        "{A},{B},{},{}",
        policy_on("p", "a"),
        policy_on("p", "b")
    ))
    .expect("policy names are scoped per table");
}

#[test]
fn distinct_privileged_names_are_still_allowed() {
    verdict(
        r#"{"op":"createSchema","name":"s1"},{"op":"createSchema","name":"s2"},{"op":"createRole","name":"r1"},{"op":"createRole","name":"r2"},{"op":"createExtension","name":"citext"},{"op":"createExtension","name":"hstore"}"#,
    )
    .expect("distinct names across all three cluster-wide kinds are ordinary");
}

#[test]
fn a_schema_and_a_role_may_share_a_name() {
    // Three separate namespaces, not one. Folding them into a single set would
    // pass every refusal test above and refuse this.
    verdict(
        r#"{"op":"createSchema","name":"n"},{"op":"createRole","name":"n"},{"op":"createExtension","name":"n"}"#,
    )
    .expect("schemas, roles and extensions do not share a namespace");
}

#[test]
fn dropping_a_schema_frees_its_name() {
    verdict(
        r#"{"op":"createSchema","name":"s"},{"op":"dropSchema","name":"s"},{"op":"createSchema","name":"s"}"#,
    )
    .expect("drop then recreate under the same name is a real pattern");
}

#[test]
fn dropping_an_extension_frees_its_name() {
    verdict(
        r#"{"op":"createExtension","name":"citext"},{"op":"dropExtension","name":"citext"},{"op":"createExtension","name":"citext"}"#,
    )
    .expect("recreating a dropped extension must stay allowed");
}

#[test]
fn dropping_a_role_frees_its_name() {
    verdict(
        r#"{"op":"createRole","name":"r"},{"op":"dropRole","name":"r"},{"op":"createRole","name":"r"}"#,
    )
    .expect("recreating a dropped role must stay allowed");
}

#[test]
fn dropping_a_policy_frees_its_name() {
    verdict(&format!(
        r#"{A},{},{{"op":"dropPolicy","name":"p","table":"a"}},{}"#,
        policy_on("p", "a"),
        policy_on("p", "a")
    ))
    .expect("drop then recreate is how a policy predicate is replaced");
}

#[test]
fn dropping_the_table_frees_its_policy_names() {
    // The container lifecycle. Policies are keyed per table, so dropping the
    // table must take them with it.
    verdict(&format!(
        r#"{A},{},{{"op":"dropTable","table":"a"}},{A},{}"#,
        policy_on("p", "a"),
        policy_on("p", "a")
    ))
    .expect("a recreated table starts with a clean policy namespace");
}

#[test]
fn renaming_the_table_carries_its_policy_names() {
    // The other half of the container lifecycle, and the half F755 recorded as
    // the one that gets forgotten: after the rename the policy is live under the
    // NEW name, so claiming it again there must still be refused.
    expect_name_refusal(
        &format!(
            r#"{A},{},{{"op":"renameTable","table":"a","to":"a2"}},{}"#,
            policy_on("p", "a"),
            policy_on("p", "a2")
        ),
        "the policy moved with the table, so its name is occupied under the new one",
    );
}

#[test]
fn renaming_the_table_frees_the_old_policy_names() {
    // The same wiring seen from the other side: nothing holds a policy on the
    // OLD name any more, so a recreated table may reuse it. A rename implemented
    // as an insert-without-remove would pass the test above and fail this one.
    verdict(&format!(
        r#"{A},{},{{"op":"renameTable","table":"a","to":"a2"}},{A},{}"#,
        policy_on("p", "a"),
        policy_on("p", "a")
    ))
    .expect("the old table name has no policies left on it");
}
