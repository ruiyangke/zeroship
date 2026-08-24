//! What must an author be GRANTED before `createTrigger` / `dropTrigger` is accepted?
//!
//! The answer this file MEASURES is: **nothing, on any of the three dialects**. Trigger
//! ops are portable-core in the capability model, not members of the privileged
//! catalog-object family, so no vendor capability and no charter knob stands between an
//! author and a trigger.
//!
//! # Why the question needed measuring rather than reading
//!
//! Prose in the tree says the opposite, in more than one place, and it is wrong about
//! triggers wherever it says it:
//!
//! - [`CODE_VENDOR_OP_DENIED`]'s own doc comment lists "role/grant/RLS/policy/trigger/
//!   function/extension/schema/`raw`" as the privileged vendor family;
//! - `zero_migrate_sqlite`'s `vendor_capability_refusal` says
//!   "roles/grants/RLS/partitions/policies/triggers/functions/extensions/schemas/raw
//!   are the privileged catalog-object family".
//!
//! Neither sentence can be reached by a trigger op. The gate consults
//! [`vendor_capabilities`], whose `createTrigger`/`dropTrigger` arm returns the EMPTY
//! set, and `validate_vendor_op` returns `Ok` on an empty set before it can ask a
//! backend for a refusal or a charter for a grant. The closed
//! [`VendorCapability`] enum has no trigger variant to name, and the charter's builtin
//! knob registry has no trigger key to grant. Grepping either sentence would have
//! answered this question wrongly, which is why the claim is asserted here instead.
//!
//! # The instrument, and how each arm proves it was live
//!
//! A test that shows a trigger op is "not refused" proves nothing unless the same call,
//! on the same path, under the same posture, IS able to refuse. Every arm below is
//! therefore a CONTRAST against `createFunction`, a genuinely capability-gated op
//! (`code.function`), decided by the same code on the same call:
//!
//! - [`the_gate_reads_no_capability_for_a_trigger_op`] - at the function the gate
//!   consults, trigger ops answer with the empty set while `createFunction` answers
//!   with [`VendorCapability::Function`].
//! - [`validate_admits_trigger_ops_under_the_posture_that_grants_nothing`] - through
//!   `validate_ir_authorized` on all three dialects, under the Confined creator scope
//!   `VendorCapabilities::from_scope` maps to the capability set that grants nothing.
//!   The PostgreSQL control refuses `createFunction` there with
//!   [`CODE_VENDOR_OP_DENIED`].
//! - [`a_charter_granting_no_vendor_capability_lowers_a_trigger_drop`] and
//!   [`granting_every_vendor_capability_changes_nothing_for_a_trigger_drop`] - through
//!   the production guarded lower entry, the emptiest vendor charter and the full
//!   operator charter produce the SAME emitted SQL for a trigger drop, while those two
//!   charters DISAGREE about `createFunction`. A grant that changes nothing is not a
//!   gate.
//!
//! # The one axis this file deliberately holds constant
//!
//! Both charters here grant `safety.destructive_ops = "allow"`. That knob is the OTHER
//! rule a trigger drop meets, and it is the subject of
//! `crate::dialect_matrix::op_refused_observation`'s live red. Holding it at `allow`
//! isolates the capability axis: anything that refuses below refused for want of a
//! GRANT, which is the only question this file asks.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use zero_migrate::guard::GuardConfig;
use zero_migrate::model::capability::VendorCapability;
use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::op_support::vendor_capabilities;
use zero_migrate::model::table_shape::resolve_create_table_policy;
use zero_migrate::model::validate::{validate_ir_authorized, CODE_VENDOR_OP_DENIED};
use zero_migrate::render::lower::{
    IrAuthor, LiveSchema, LoadAndLowerGuardedError, LoweredArtifact,
};
use zero_migrate::{
    effective_policy_from_charter_toml, DialectId, EffectivePolicy, PlanStep, SchemaScope,
};

const SCHEMA: &str = "app";
const OWNER: &str = "app_a";
const TABLE: &str = "audited";
const TRIGGER: &str = "audited_touch_trg";
const FUNCTION: &str = "audited_touch_fn";

/// The three shipping dialects, so every claim below is made on all of them rather than
/// on PostgreSQL alone. `dropTrigger`/`base` is `Portable` in all three backends'
/// support tables, which is what makes one subject op answerable everywhere.
fn dialects() -> Vec<DialectId> {
    vec![
        zero_migrate_postgres::DIALECT,
        zero_migrate_mysql::DIALECT,
        zero_migrate_sqlite::DIALECT,
    ]
}

/// The subject: drop a trigger by name. Byte-identical on every dialect - the op whose
/// governance this file measures.
fn drop_trigger_op() -> Value {
    json!({ "op": "dropTrigger", "name": TRIGGER, "table": TABLE })
}

/// A `createTrigger` in the shape each backend declares `Portable`: PostgreSQL executes
/// a function, MySQL and SQLite carry a body. The shapes are
/// `dialect_conformance_live::prelude`'s, measured there against real servers.
///
/// The PostgreSQL leg names a function it does NOT create. That is the measurement, not
/// an oversight: minting the function is `code.function`-gated, so a leg that created
/// one could not tell a refusal of the TRIGGER from a refusal of the FUNCTION.
fn create_trigger_op(dialect: &DialectId) -> Value {
    let action = if dialect == &zero_migrate_postgres::DIALECT {
        json!({ "kind": "executeFunction", "name": FUNCTION })
    } else if dialect == &zero_migrate_mysql::DIALECT {
        json!({ "kind": "body", "statements": [
            { "stmt": "delete", "table": TABLE,
              "where": { "node": "colRef", "name": "flag" } }] })
    } else {
        json!({ "kind": "body", "statements": [
            { "stmt": "select", "expr": { "node": "colRef", "name": "val" } }] })
    };
    json!({
        "op": "createTrigger", "name": TRIGGER, "table": TABLE,
        "timing": "before", "events": ["insert"], "forEach": "row", "action": action,
    })
}

/// The CONTROL op: creating a function requires [`VendorCapability::Function`]. Every
/// arm pairs its trigger claim with this one so a green cannot come from a gate that
/// was never able to fire.
fn create_function_op() -> Value {
    json!({
        "op": "createFunction", "name": FUNCTION, "returns": "trigger",
        "language": "procedural", "body": "BEGIN RETURN NEW; END",
    })
}

/// The table the trigger hangs on. Present so the envelope clears the load gate's
/// reference checks; it requires no vendor capability of its own.
///
/// The key is `bigInt` rather than a text column because MySQL refuses a key over an
/// unbounded TEXT column - a `DIALECT_UNSUPPORTED` that has nothing to do with grants
/// and would have masked the measurement on one of the three legs.
fn create_table_op() -> Value {
    json!({
        "op": "createTable", "name": TABLE, "schema": SCHEMA,
        "columns": [
            { "name": "id", "type": "bigInt", "nullable": false },
            { "name": "val", "type": "int", "nullable": true },
            { "name": "flag", "type": "boolean", "nullable": true }
        ],
        "primaryKey": ["id"], "constraints": [], "indexes": [],
    })
}

fn envelope(name: &str, ops: Vec<Value>) -> String {
    json!({ "ir_version": 1, "name": name, "owner_app": OWNER, "ops": ops }).to_string()
}

// ---------------------------------------------------------------------------
// Arm 1 - at the function the gate consults
// ---------------------------------------------------------------------------

/// `validate_vendor_op` returns `Ok` the moment [`vendor_capabilities`] answers with an
/// empty set, BEFORE it can consult a backend refusal or a charter grant. So the whole
/// question "what must be granted" is decided here, and for a trigger op the answer is
/// the empty set.
#[test]
fn the_gate_reads_no_capability_for_a_trigger_op() {
    for dialect in dialects() {
        for op in [drop_trigger_op(), create_trigger_op(&dialect)] {
            let parsed: MigrationIr = serde_json::from_str(&envelope("caps", vec![op.clone()]))
                .expect("the trigger envelope parses");
            assert_eq!(
                vendor_capabilities(&parsed.ops[0]),
                Vec::new(),
                "{dialect}: a trigger op must require no vendor capability, but the gate reads \
                 one: {op}"
            );
        }
    }

    let control: MigrationIr = serde_json::from_str(&envelope("caps", vec![create_function_op()]))
        .expect("the control envelope parses");
    assert_eq!(
        vendor_capabilities(&control.ops[0]),
        vec![VendorCapability::Function],
        "the control op must be capability-gated, or the contrast above measures nothing"
    );
}

/// `Op::is_destructive`'s doc names a FAMILY it declares deliberately non-destructive:
/// `DROP INDEX`, `DROP ROLE`, `DROP EXTENSION`, `DROP POLICY`, `DROP TRIGGER`, `REVOKE`.
/// Reading that list as one decision invites the conclusion that a classifier arm
/// naming only the index drop is an omission.
///
/// It is not one decision. The role, extension, policy and grant members are each
/// capability-gated, and that grant is what governs them once the destructive posture
/// lets them past. The members carrying NO capability are `dropTrigger` and
/// `dropIndex` - and `dropIndex`, the only one the parser-backed classifier already
/// vouches for, has the compensating control the IR's own doc records instead: a
/// UNIQUE-index drop is gated through `MigrationFlags`.
///
/// This arm is what makes that asymmetry a measurement rather than a reading, so a
/// future change that gives trigger ops a capability breaks it here and says so.
#[test]
fn dropping_a_trigger_is_the_family_member_with_no_capability_behind_it() {
    let gated = [
        (r#"{"op":"dropRole","name":"r"}"#, VendorCapability::Role),
        (
            r#"{"op":"dropExtension","name":"citext"}"#,
            VendorCapability::Extension,
        ),
        (
            r#"{"op":"dropPolicy","name":"p","table":"audited"}"#,
            VendorCapability::Policy,
        ),
        (
            r#"{"op":"revoke","privileges":["select"],"on":{"kind":"schema","names":["app"]},"from":["reader"]}"#,
            VendorCapability::Grant,
        ),
    ];
    for (op_json, expected) in gated {
        let ir: MigrationIr = serde_json::from_str(&format!(
            r#"{{"ir_version":1,"name":"n","ops":[{op_json}]}}"#
        ))
        .expect("the family envelope parses");
        assert_eq!(
            vendor_capabilities(&ir.ops[0]),
            vec![expected],
            "this family member is expected to be governed by a capability grant: {op_json}"
        );
    }

    for op_json in [
        r#"{"op":"dropTrigger","name":"audited_touch_trg","table":"audited"}"#,
        r#"{"op":"dropIndex","name":"audited_val_idx","table":"audited"}"#,
    ] {
        let ir: MigrationIr = serde_json::from_str(&format!(
            r#"{{"ir_version":1,"name":"n","ops":[{op_json}]}}"#
        ))
        .expect("the family envelope parses");
        assert_eq!(
            vendor_capabilities(&ir.ops[0]),
            Vec::new(),
            "this family member is expected to carry no capability at all: {op_json}"
        );
    }
}

// ---------------------------------------------------------------------------
// Arm 2 - through validate, on all three dialects
// ---------------------------------------------------------------------------

/// The Confined creator posture: a `Single` schema scope and no charter.
/// `VendorCapabilities::from_scope` maps it to the set that grants NOTHING, so it is the
/// tightest posture the validate-layer gate can be asked about.
fn confined_scope() -> SchemaScope {
    SchemaScope::Single(SCHEMA.to_string())
}

#[test]
fn validate_admits_trigger_ops_under_the_posture_that_grants_nothing() {
    let scope = confined_scope();
    for dialect in dialects() {
        for (label, op) in [
            ("dropTrigger", drop_trigger_op()),
            ("createTrigger", create_trigger_op(&dialect)),
        ] {
            let ir: MigrationIr = serde_json::from_str(&envelope("t", vec![create_table_op(), op]))
                .expect("the trigger envelope parses");
            validate_ir_authorized(&ir, &dialect, Some(&scope), None).unwrap_or_else(|e| {
                panic!(
                    "{dialect}: {label} must need no grant, but the posture that grants nothing \
                     refused it: {} / {}",
                    e.code, e.reason
                )
            });
        }
    }
}

/// The instrument check for the arm above: the SAME call, the SAME posture, on the
/// dialect that renders the privileged family, DOES refuse an op that needs a grant.
#[test]
fn the_same_validate_posture_still_refuses_an_op_that_needs_a_grant() {
    let scope = confined_scope();
    let ir: MigrationIr = serde_json::from_str(&envelope(
        "f",
        vec![create_table_op(), create_function_op()],
    ))
    .expect("the control envelope parses");
    let error = validate_ir_authorized(&ir, &zero_migrate_postgres::DIALECT, Some(&scope), None)
        .expect_err("createFunction must be refused where nothing is granted");
    assert_eq!(
        error.code, CODE_VENDOR_OP_DENIED,
        "the control was refused for the wrong reason: {}",
        error.reason
    );
    assert!(
        error
            .reason
            .contains(VendorCapability::Function.flag_name()),
        "the control refusal does not name the function capability: {}",
        error.reason
    );
}

// ---------------------------------------------------------------------------
// Arm 3 - through the charter, on the production guarded lower entry
// ---------------------------------------------------------------------------

/// The grants any envelope here needs that are NOT about a vendor capability: table
/// creation, and the destructive posture this file deliberately holds at `allow`.
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

/// Owns exactly its own schema and grants NO vendor capability - the creator posture.
fn no_vendor_capability_charter() -> EffectivePolicy {
    charter(
        r#"
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }
"#,
    )
}

/// Grants every BOOLEAN vendor capability the registry defines over the whole universe.
/// `code.extension` is absent because its knob takes a name allowlist rather than a
/// bool; nothing here creates an extension.
fn every_vendor_capability_charter() -> EffectivePolicy {
    charter(
        r#"
[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"

[[grant]]
key = "schema.create_schema"
value = true
scope = "all"

[[grant]]
key = "schema.partition"
value = true
scope = "all"

[[grant]]
key = "access.role"
value = true
scope = "all"

[[grant]]
key = "access.grant"
value = true
scope = "all"

[[grant]]
key = "access.rls"
value = true
scope = "all"

[[grant]]
key = "access.policy"
value = true
scope = "all"

[[grant]]
key = "code.function"
value = true
scope = "all"

[[grant]]
key = "code.materialized_view"
value = true
scope = "all"

[[grant]]
key = "sql.raw"
value = true
scope = "all"

[[grant]]
key = "sql.raw_view_body"
value = true
scope = "all"
"#,
    )
}

/// Lower through the production entry the deploy path uses: one `EffectivePolicy` feeds
/// both `GuardConfig::from_policy` and `IrAuthor::new`, and nothing widens the scope by
/// hand.
fn lower(
    policy: &EffectivePolicy,
    dialect: &DialectId,
    ops: Vec<Value>,
) -> Result<LoweredArtifact, LoadAndLowerGuardedError> {
    let authored =
        serde_json::from_str(&envelope("trigger_grant", ops)).expect("test envelope parses");
    let resolved =
        resolve_create_table_policy(&authored, policy, SCHEMA).expect("table shape resolves");
    let resolved_json = serde_json::to_string(&resolved).expect("resolved IR serializes");
    let guard = GuardConfig::from_policy(policy.clone(), dialect.clone());
    let author = IrAuthor::new(SCHEMA, OWNER, dialect, policy);
    author.load_and_lower_guarded(
        &resolved_json,
        OWNER,
        &BTreeMap::new(),
        &LiveSchema::default(),
        &guard,
    )
}

fn emitted_sql(artifact: &LoweredArtifact) -> String {
    artifact
        .plan
        .steps
        .iter()
        .filter_map(|step| match step {
            PlanStep::Ddl(migration) => Some(migration.up.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_charter_granting_no_vendor_capability_lowers_a_trigger_drop() {
    for dialect in dialects() {
        let artifact = lower(
            &no_vendor_capability_charter(),
            &dialect,
            vec![create_table_op(), drop_trigger_op()],
        )
        .unwrap_or_else(|e| {
            panic!("{dialect}: a charter granting no vendor capability must still admit a trigger drop: {e:?}")
        });
        assert!(
            emitted_sql(&artifact)
                .to_ascii_uppercase()
                .contains("TRIGGER"),
            "{dialect}: the trigger drop lowered without emitting its DDL:\n{}",
            emitted_sql(&artifact)
        );
    }
}

/// The sharpest form of "ungated": adding every vendor grant the registry defines
/// changes the emitted SQL not at all. A grant that makes no difference is not a gate.
#[test]
fn granting_every_vendor_capability_changes_nothing_for_a_trigger_drop() {
    for dialect in dialects() {
        let ungranted = lower(
            &no_vendor_capability_charter(),
            &dialect,
            vec![create_table_op(), drop_trigger_op()],
        )
        .expect("the ungranted charter lowers the trigger drop");
        let granted = lower(
            &every_vendor_capability_charter(),
            &dialect,
            vec![create_table_op(), drop_trigger_op()],
        )
        .expect("the fully-granted charter lowers the trigger drop");
        assert_eq!(
            emitted_sql(&ungranted),
            emitted_sql(&granted),
            "{dialect}: the two charters disagree about a trigger drop, so something DOES gate it"
        );
    }
}

/// The instrument check for the two arms above: those same two charters DISAGREE about
/// an op that is capability-gated. Without this, both arms would pass on a build where
/// the charter axis had stopped being read at all.
#[test]
fn the_same_two_charters_still_disagree_about_an_op_that_needs_a_grant() {
    let dialect = zero_migrate_postgres::DIALECT;
    let ops = vec![create_table_op(), create_function_op()];
    let error = lower(&no_vendor_capability_charter(), &dialect, ops.clone())
        .expect_err("a charter granting no vendor capability must refuse createFunction");
    let reported = format!("{error:?}");
    assert!(
        reported.contains(CODE_VENDOR_OP_DENIED)
            && reported.contains(VendorCapability::Function.flag_name()),
        "the control was refused for the wrong reason: {reported}"
    );
    lower(&every_vendor_capability_charter(), &dialect, ops)
        .expect("a charter granting code.function must admit createFunction");
}
