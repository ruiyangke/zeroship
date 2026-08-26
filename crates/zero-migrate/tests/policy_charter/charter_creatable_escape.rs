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

/// Two injects in ONE layer, same scope, same column name, incompatible shapes. Nothing
/// examined this pair: `admit` compares the accumulated charter against an incoming
/// DRAFT, and a single-layer charter never passes through it as one.
///
/// Measured before the refusal existed: `resolve_create_table_policy` under this charter
/// returned columns `["created_at", "created_at", "body"]`, because the resolver
/// contributes every covering spec's columns and does not dedupe. That is a CREATE TABLE
/// no database accepts.
const SAME_LAYER_CONFLICT: &str = r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "text", nullable = true } ]
"#;

/// The same two injects on scopes that cannot both cover one table. Two injects may
/// contribute the same column name to different tables, so this must keep composing.
const DISJOINT_SCOPES: &str = r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
[[inject]]
scope = { include = ["ops_*"] }
columns = [ { name = "created_at", type = "text", nullable = true } ]
"#;

#[test]
fn two_conflicting_injects_in_one_layer_are_refused() {
    let composed = effective_policy_from_charter_layers(&[SAME_LAYER_CONFLICT]);
    let Err(message) = composed else {
        panic!("two same-scope injects contributing one column with different shapes build a table carrying it twice, so the charter must be refused");
    };
    assert!(
        message.contains("CharterInjectCollision"),
        "the refusal must name the collision rather than failing for another reason: {message}"
    );
}

#[test]
fn the_same_column_on_disjoint_scopes_still_composes() {
    let composed = effective_policy_from_charter_layers(&[DISJOINT_SCOPES]);
    assert!(
        composed.is_ok(),
        "injects that cannot both cover one table do not collide: {composed:?}"
    );
}
