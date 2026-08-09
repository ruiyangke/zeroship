//! Vendor-capability authority on the guarded IR path comes from the charter, not
//! from the author's schema-confinement scope.
//!
//! Every test here builds the author the way the production deploy entry does
//! (`MigrationEngine`'s aggregate loop and the napi addon's host lower): one
//! `EffectivePolicy` feeds both `GuardConfig::from_policy` and `IrAuthor::new`, and
//! nothing calls `IrAuthor::with_schema_scope`. That is the shape under which a
//! `setRls` was refused under every charter, including the widest one.

mod support;

use std::collections::BTreeMap;

use zero_migrate::guard::GuardConfig;
use zero_migrate::model::capability::{VendorCapabilities, VendorCapability};
use zero_migrate::model::load::{load_ir_document, IrLoadError};
use zero_migrate::model::table_shape::resolve_create_table_policy;
use zero_migrate::model::validate::{Dialect, CODE_VENDOR_OP_DENIED};
use zero_migrate::render::lower::{
    IrAuthor, IrGuardedLowerError, IrLowerError, LiveSchema, LoadAndLowerGuardedError,
    LoweredArtifact,
};
use zero_migrate::schema::query::SqlDialect;
use zero_migrate::{effective_policy_from_charter_toml, EffectivePolicy, PlanStep, SchemaScope};

const SCHEMA: &str = "app";
const OWNER: &str = "app_a";

/// A `createTable` the charter permits, then the `setRls` under test. The load gate
/// refuses RLS on a table this envelope never declares, so the table comes first.
const RLS_ENVELOPE: &str = r#"{"ir_version":1,"name":"rls_authority","owner_app":"app_a","ops":[
    {"op":"createTable","name":"users","schema":"app","columns":[
        {"name":"id","type":"text","nullable":false}
    ],"primaryKey":["id"],"constraints":[],"indexes":[]},
    {"op":"setRls","table":"users","schema":"app","enabled":true}
]}"#;

/// The grants any charter here needs to get a plain `createTable` past the load gate,
/// independent of the vendor capability under test.
const BASE_GRANTS: &str = r#"policy_version = 1

[[grant]]
key = "schema.create_table"
value = true
scope = "all"

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
"#;

fn charter(extra: &str) -> EffectivePolicy {
    effective_policy_from_charter_toml(&format!("{BASE_GRANTS}{extra}"))
        .expect("test charter composes")
}

/// Grants `access.rls` AND the whole-universe `schema.cross_schema` the validate-time
/// cross-schema gate needs. This is the widest charter the probe measured as REFUSED.
fn rls_granted_charter() -> EffectivePolicy {
    charter(
        r#"
[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"

[[grant]]
key = "access.rls"
value = true
scope = "all"
"#,
    )
}

/// Grants `access.rls` over the whole universe while confining `schema.cross_schema`
/// to the project schema - the posture a creator charter that wants RLS on its own
/// tables actually authors. It composes to `SchemaScope::Single("app")`, which
/// `VendorCapabilities::from_scope` maps to the capability set that grants nothing, so
/// the charter's own `access.rls` grant is the only thing that can admit the op.
fn rls_granted_own_schema_charter() -> EffectivePolicy {
    charter(
        r#"
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }

[[grant]]
key = "access.rls"
value = true
scope = "all"
"#,
    )
}

/// Grants the whole-universe `schema.cross_schema` and NOTHING from `access`. Under
/// authority-by-scope this composes to `SchemaScope::Unconfined`, which
/// `VendorCapabilities::from_scope` maps to the FULL operator capability set - so this
/// charter is the one that separates a policy-derived gate from a restored
/// `with_schema_scope` call.
fn cross_schema_only_charter() -> EffectivePolicy {
    charter(
        r#"
[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"
"#,
    )
}

/// Owns exactly its own schema and grants no vendor capability - the creator posture.
fn confined_charter() -> EffectivePolicy {
    charter(
        r#"
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }
"#,
    )
}

/// Lower `RLS_ENVELOPE` through the production entry: one policy, both gates, no
/// manual scope widening.
fn lower_rls_envelope(
    policy: &EffectivePolicy,
) -> Result<LoweredArtifact, LoadAndLowerGuardedError> {
    let authored = serde_json::from_str(RLS_ENVELOPE).expect("test envelope parses");
    let resolved =
        resolve_create_table_policy(&authored, policy, SCHEMA).expect("table shape resolves");
    let resolved_json = serde_json::to_string(&resolved).expect("resolved IR serializes");
    let guard = GuardConfig::from_policy(policy.clone(), SqlDialect::Postgres);
    let author = IrAuthor::new(SCHEMA, OWNER, SqlDialect::Postgres, policy);
    author.load_and_lower_guarded(
        &resolved_json,
        OWNER,
        &BTreeMap::new(),
        &LiveSchema::default(),
        &guard,
    )
}

/// The refusal both gates owe an ungranted `setRls`, named at whichever one sees the
/// op first. The load gate reads the charter when the author hands it one and the
/// lower gate re-reads it before rendering, so the same missing grant surfaces as a
/// `VENDOR_OP_DENIED` authoring error or as a `VendorCapabilityDenied`. Either is the
/// contract; what is asserted is that the refusal names the RLS capability and that no
/// SQL was produced.
fn assert_rls_capability_denied(error: &LoadAndLowerGuardedError, label: &str) {
    match error {
        LoadAndLowerGuardedError::Lower(IrGuardedLowerError::Lower(
            IrLowerError::VendorCapabilityDenied { capability, .. },
        )) => assert_eq!(
            *capability,
            VendorCapability::Rls,
            "{label}: refused for the wrong capability"
        ),
        LoadAndLowerGuardedError::Load(IrLoadError::Validate(authoring)) => {
            assert_eq!(
                authoring.code, CODE_VENDOR_OP_DENIED,
                "{label}: refused by the load gate for the wrong reason"
            );
            assert!(
                authoring.reason.contains(VendorCapability::Rls.flag_name()),
                "{label}: the load refusal does not name the RLS capability: {}",
                authoring.reason
            );
        }
        other => panic!("{label}: expected an RLS capability refusal, got {other:?}"),
    }
}

fn assert_emits_enable_rls(artifact: &LoweredArtifact, label: &str) {
    let sql = artifact
        .plan
        .steps
        .iter()
        .filter_map(|step| match step {
            PlanStep::Ddl(migration) => Some(migration.up.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        sql.to_ascii_uppercase()
            .contains("ENABLE ROW LEVEL SECURITY"),
        "{label}: granted setRls lowered without emitting its DDL:\n{sql}"
    );
}

#[test]
fn charter_granting_access_rls_lowers_set_rls_and_emits_its_sql() {
    let artifact = lower_rls_envelope(&rls_granted_charter())
        .expect("a charter granting access.rls admits setRls through the production entry");
    assert_emits_enable_rls(&artifact, "whole-universe cross_schema");
}

/// The same grant as the control above, with `schema.cross_schema` narrowed to the one
/// schema the migration touches. A charter that confines itself to its own schema must
/// not lose the capability it explicitly granted: confinement answers which schemas the
/// migration may touch, never which privileged primitives it may render.
#[test]
fn charter_granting_access_rls_within_its_own_schema_lowers_set_rls_and_emits_its_sql() {
    let artifact = lower_rls_envelope(&rls_granted_own_schema_charter())
        .expect("a charter granting access.rls admits setRls even when confined to its own schema");
    assert_emits_enable_rls(&artifact, "own-schema cross_schema");
}

#[test]
fn charter_granting_cross_schema_but_not_access_rls_still_refuses_set_rls() {
    let error = lower_rls_envelope(&cross_schema_only_charter())
        .expect_err("cross_schema alone must not authorize a vendor capability");
    assert_rls_capability_denied(&error, "cross_schema without access.rls");
}

/// Why the refusal above cannot come from the author's schema scope: under a charter
/// that grants only `schema.cross_schema`, the policy-derived scope IS `Unconfined`,
/// and `VendorCapabilities::from_scope` maps `Unconfined` to the full operator set.
/// Handing that scope to the author - the one deleted line the regression removed -
/// would therefore authorize `setRls` on a charter that never granted `access.rls`.
#[test]
fn cross_schema_alone_composes_to_a_scope_that_would_grant_every_capability() {
    let policy = cross_schema_only_charter();
    let scope = GuardConfig::from_policy(policy, SqlDialect::Postgres)
        .schema_scope()
        .expect("the guard derives a schema scope");
    assert_eq!(
        scope,
        SchemaScope::Unconfined,
        "a whole-universe cross_schema grant composes to Unconfined"
    );
    assert!(
        VendorCapabilities::from_scope(Some(&scope)).grants(VendorCapability::Rls),
        "Unconfined maps to the full operator capability set, so scope is not authority"
    );
}

#[test]
fn confined_charter_still_refuses_set_rls() {
    let error = lower_rls_envelope(&confined_charter())
        .expect_err("a confined charter must keep refusing setRls");
    assert_rls_capability_denied(&error, "confined charter");
}

/// The load gate reached with no charter in hand keeps refusing. `load_ir_document`
/// is public and the node addon's load-verify entry calls it with a confined scope and
/// no policy at all, so the scope-derived capability set is the only answer available
/// there. It must stay a refusal: an artifact the widest charter would admit is still
/// denied when nothing supplies that charter.
#[test]
fn the_load_gate_without_a_charter_still_refuses_set_rls() {
    let policy = rls_granted_charter();
    let authored = serde_json::from_str(RLS_ENVELOPE).expect("test envelope parses");
    let resolved =
        resolve_create_table_policy(&authored, &policy, SCHEMA).expect("table shape resolves");
    let resolved_json = serde_json::to_string(&resolved).expect("resolved IR serializes");
    let scope = SchemaScope::Single(SCHEMA.to_string());
    let error = load_ir_document(
        &resolved_json,
        OWNER,
        Dialect::Postgres,
        &BTreeMap::new(),
        Some(&scope),
    )
    .expect_err("a load gate holding no charter has no grant to read and must refuse");
    match error {
        IrLoadError::Validate(authoring) => assert_eq!(
            authoring.code, CODE_VENDOR_OP_DENIED,
            "the charter-free load gate refused for the wrong reason"
        ),
        other => panic!("expected a VENDOR_OP_DENIED authoring refusal, got {other:?}"),
    }
}
