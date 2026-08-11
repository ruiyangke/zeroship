//! A charter whose create grant escapes its own mandatory inject must be refused by the
//! composition path the product actually uses.
//!
//! `crates/zero-migrate-policy/src/compose.rs` has always carried this check, but only
//! behind `finalize_charter`, which no shipped call site reaches. Its own comment records
//! that it was "moved here from admit" and reasons that admit's later proof of
//! `draft.create_table` being within `charter.create_table` transitively bounds every
//! draft - an argument that needs the charter-side bound checked somewhere.
//!
//! Without the check a charter can create tables outside the region where its mandatory
//! column is contributed, which is a table escaping a column the charter says is not
//! optional.

use zero_migrate::model::table_shape::effective_policy_from_charter_layers;

/// `schema.create_table` granted over everything while the mandatory inject covers only
/// `app_*`, so the creatable region is not contained by the inject.
const ESCAPING_CHARTER: &str = r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
mandatory = true
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
[[grant]]
key = "schema.create_table"
value = true
scope = "all"
"#;

/// The same charter with the create grant confined to the mandatory inject's own scope.
/// Legitimate, and must keep composing - otherwise the check above could be satisfied by
/// refusing every charter that carries a mandatory inject at all.
const CONFINED_CHARTER: &str = r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
mandatory = true
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app_*"] }
"#;

/// No mandatory inject at all, so nothing constrains where tables may be created.
const NO_MANDATORY_INJECT: &str = r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
[[grant]]
key = "schema.create_table"
value = true
scope = "all"
"#;

#[test]
fn a_create_grant_escaping_a_mandatory_inject_is_refused() {
    let composed = effective_policy_from_charter_layers(&[ESCAPING_CHARTER]);
    let Err(message) = composed else {
        panic!("composing a charter that can create tables outside its mandatory inject must be refused");
    };
    assert!(
        message.contains("CreatableEscapesMandatoryInject"),
        "the refusal must name the escape rather than failing for some other reason: {message}"
    );
}

#[test]
fn a_create_grant_confined_to_the_mandatory_inject_still_composes() {
    let composed = effective_policy_from_charter_layers(&[CONFINED_CHARTER]);
    assert!(
        composed.is_ok(),
        "confining the create grant to the mandatory inject's scope is the legitimate shape: {composed:?}"
    );
}

#[test]
fn an_optional_inject_does_not_bound_the_creatable_scope() {
    // Only `mandatory` injects gate. An advisory inject that a create can miss is not an
    // escape, so a broad create grant beside one must still compose.
    let composed = effective_policy_from_charter_layers(&[NO_MANDATORY_INJECT]);
    assert!(
        composed.is_ok(),
        "a non-mandatory inject must not constrain where tables may be created: {composed:?}"
    );
}
