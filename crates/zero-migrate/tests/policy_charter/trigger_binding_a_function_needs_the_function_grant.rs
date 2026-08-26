//! Binding an existing function to fire on every row is the `code.function` power.
//!
//! `code.function`'s meaning is "this charter may introduce code into the database".
//! `createFunction` and `dropFunction` are gated on it. But a `createTrigger` whose
//! action is `executeFunction` arranges for CODE THAT ALREADY EXISTS to run on every
//! affected row, with no later statement naming it - the same power reached by a
//! different door. This file measures that the door is now the same size.
//!
//! # What was measured before the second grant existed
//!
//! At the time the gap was recorded, `createTrigger { executeFunction }` required NO
//! capability at all. That has since been closed by [`VendorCapability::Trigger`],
//! which is a different fix: it makes an author need OPERATOR authority over
//! triggers, not authority over CODE. An operator holding `code.trigger` and not
//! `code.function` could still bind any pre-existing function.
//!
//! No later stage catches it, and this file's arms are what say so rather than an
//! argument. The guarded lower is the last engine-side gate a trigger meets - there
//! is no existence probe for a trigger's bound function, no ownership or namespace
//! check on the bound name beyond the schema confinement every identifier gets, and
//! the live server cannot help: the premise of the whole shape is that the function
//! ALREADY EXISTS, so `CREATE TRIGGER … EXECUTE FUNCTION f()` is a statement
//! PostgreSQL accepts. [`the_trigger_only_charter_is_refused_at_the_guarded_lower`]
//! is the arm that pins the last of those stages.
//!
//! # The instrument
//!
//! Two contrasts under the same charter and the same call, so a green cannot come
//! from a charter that refuses everything:
//!
//! - a `createTrigger` carrying a BODY, which introduces no pre-existing code and
//!   must still be admitted by a `code.trigger`-only charter;
//! - a `dropTrigger`, likewise.
//!
//! And the other direction: a charter granting BOTH must admit the binding, or the
//! refusal above would be indistinguishable from the op being unreachable.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use zero_migrate::guard::GuardConfig;
use zero_migrate::model::capability::VendorCapability;
use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::op_support::vendor_capabilities;
use zero_migrate::model::table_shape::resolve_create_table_policy;
use zero_migrate::render::lower::{
    IrAuthor, LiveSchema, LoadAndLowerGuardedError, LoweredArtifact,
};
use zero_migrate::{effective_policy_from_charter_toml, DialectId, EffectivePolicy};

const SCHEMA: &str = "app";
const OWNER: &str = "app_a";
const TABLE: &str = "audited";
const TRIGGER: &str = "audited_touch_trg";
const FUNCTION: &str = "audited_touch_fn";

/// The SUBJECT: a trigger that binds a function this envelope does not create. The
/// function is deliberately absent - the whole point of the shape is that the code
/// already exists and the author is only arranging for it to run.
fn bind_function_op() -> Value {
    json!({
        "op": "createTrigger", "name": TRIGGER, "table": TABLE,
        "timing": "before", "events": ["insert"], "forEach": "row",
        "action": { "kind": "executeFunction", "name": FUNCTION },
    })
}

/// CONTROL: the same op with a BODY action. A body introduces no pre-existing code,
/// so it must stay gated on the trigger capability ALONE. SQLite is the dialect that
/// renders this shape.
fn body_trigger_op() -> Value {
    json!({
        "op": "createTrigger", "name": TRIGGER, "table": TABLE,
        "timing": "before", "events": ["insert"], "forEach": "row",
        "action": { "kind": "body", "statements": [
            { "stmt": "select", "expr": { "node": "colRef", "name": "val" } }] },
    })
}

/// CONTROL: dropping a trigger unbinds code rather than binding it.
fn drop_trigger_op() -> Value {
    json!({ "op": "dropTrigger", "name": TRIGGER, "table": TABLE })
}

fn create_table_op() -> Value {
    json!({
        "op": "createTable", "name": TABLE, "schema": SCHEMA,
        "columns": [
            { "name": "id", "type": "bigInt", "nullable": false },
            { "name": "val", "type": "int", "nullable": true }
        ],
        "primaryKey": ["id"], "constraints": [], "indexes": [],
    })
}

fn envelope(ops: Vec<Value>) -> String {
    json!({ "ir_version": 1, "name": "bind", "owner_app": OWNER, "ops": ops }).to_string()
}

fn parse_one(op: Value) -> MigrationIr {
    serde_json::from_str(&envelope(vec![op])).expect("the envelope parses")
}

// ---------------------------------------------------------------------------
// Arm 1 - at the function the gate consults
// ---------------------------------------------------------------------------

#[test]
fn binding_a_function_requires_both_the_trigger_and_the_function_capability() {
    assert_eq!(
        vendor_capabilities(&parse_one(bind_function_op()).ops[0]),
        vec![VendorCapability::Trigger, VendorCapability::Function],
        "arranging for existing code to run on every row is the code.function power \
         as well as the trigger power"
    );

    assert_eq!(
        vendor_capabilities(&parse_one(body_trigger_op()).ops[0]),
        vec![VendorCapability::Trigger],
        "a BODY action introduces no pre-existing code, so it must not acquire the \
         function capability - without this the arm above would be satisfied by \
         gating every trigger on code.function"
    );

    assert_eq!(
        vendor_capabilities(&parse_one(drop_trigger_op()).ops[0]),
        vec![VendorCapability::Trigger],
        "unbinding code is not introducing it"
    );
}

/// The second capability is an AUTHORITY requirement, not a claim about what the op
/// renders, and the difference is visible in a diagnostic.
///
/// A backend is asked "do you render the primitive this capability names" before the
/// authority gate runs. Feeding it the author's whole requirement list made SQLite -
/// which renders no functions - answer that the TRIGGER had no SQLite analogue and
/// belonged to the privileged catalog-object family, and advise deploying it against
/// another dialect. The true refusal is narrower and about the ACTION: SQLite triggers
/// do not take the executeFunction form.
///
/// The MySQL leg is the control. Its backend refuses no capability at all, so it kept
/// the accurate message throughout and shows the wording above was the conflation
/// speaking rather than the shape genuinely being unrenderable.
#[test]
fn a_backend_that_cannot_render_the_action_says_so_about_the_action() {
    let ir: MigrationIr =
        serde_json::from_str(&envelope(vec![create_table_op(), bind_function_op()]))
            .expect("the envelope parses");

    for dialect in [zero_migrate_sqlite::DIALECT, zero_migrate_mysql::DIALECT] {
        let policy = trigger_and_function_charter();
        let error = zero_migrate::model::validate::validate_ir_authorized(
            zero_migrate::shipping_vendors(),
            &ir,
            &dialect,
            None,
            Some(zero_migrate::model::validate::VendorAuthority {
                effective: &policy,
                default_schema: SCHEMA,
            }),
        )
        .expect_err("neither backend renders the executeFunction trigger action");
        assert!(
            !error.reason.contains("privileged catalog-object family"),
            "{dialect}: a trigger is not a privileged catalog object, and telling the \
             operator to deploy it elsewhere is wrong advice: {} / {:?}",
            error.reason,
            error.suggested_fix
        );
        let said = format!("{} {:?}", error.reason, error.suggested_fix);
        assert!(
            said.to_ascii_uppercase().contains("TRIGGER"),
            "{dialect}: the refusal must be about the trigger action: {said}"
        );
    }
}

// ---------------------------------------------------------------------------
// Arm 2 - through the production guarded lower entry
// ---------------------------------------------------------------------------

const BASE_GRANTS: &str = r#"policy_version = 1

[[grant]]
key = "schema.create_table"
value = true
scope = "all"

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"

[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"

[[grant]]
key = "code.trigger"
value = true
scope = "all"
"#;

/// Operator authority over TRIGGERS, and no authority over CODE. The posture the
/// whole file is about.
fn trigger_only_charter() -> EffectivePolicy {
    effective_policy_from_charter_toml(BASE_GRANTS).expect("the trigger-only charter composes")
}

/// The same charter plus `code.function`.
fn trigger_and_function_charter() -> EffectivePolicy {
    effective_policy_from_charter_toml(&format!(
        "{BASE_GRANTS}
[[grant]]
key = \"code.function\"
value = true
scope = \"all\"
"
    ))
    .expect("the trigger-and-function charter composes")
}

fn lower(
    policy: &EffectivePolicy,
    dialect: &DialectId,
    ops: Vec<Value>,
) -> Result<LoweredArtifact, LoadAndLowerGuardedError> {
    let authored = serde_json::from_str(&envelope(ops)).expect("test envelope parses");
    let resolved =
        resolve_create_table_policy(&authored, policy, SCHEMA).expect("table shape resolves");
    let resolved_json = serde_json::to_string(&resolved).expect("resolved IR serializes");
    let guard = GuardConfig::from_policy(policy.clone(), dialect.clone());
    let author = IrAuthor::new(
        zero_migrate::shipping_vendors(),
        SCHEMA,
        OWNER,
        dialect,
        policy,
    );
    author.load_and_lower_guarded(
        &resolved_json,
        OWNER,
        &BTreeMap::new(),
        &LiveSchema::default(),
        &guard,
    )
}

/// The guarded lower is the LAST engine-side gate a trigger meets. Nothing after it
/// can help: there is no existence probe for a bound function, and the live server
/// accepts the statement precisely because the function is already there.
#[test]
fn the_trigger_only_charter_is_refused_at_the_guarded_lower() {
    let dialect = zero_migrate_postgres::DIALECT;
    let error = lower(
        &trigger_only_charter(),
        &dialect,
        vec![create_table_op(), bind_function_op()],
    )
    .expect_err(
        "a charter with operator authority over triggers but none over code must not be \
         able to bind an existing function to every row",
    );
    let reported = format!("{error:?}");
    assert!(
        reported.contains(&format!("{:?}", VendorCapability::Function))
            || reported.contains(VendorCapability::Function.flag_name())
            || reported.contains("code.function"),
        "the refusal must name the FUNCTION capability - a refusal for want of the \
         trigger grant would prove nothing here: {reported}"
    );
}

/// The instrument check: the SAME charter, the SAME call, still admits the trigger
/// shapes that introduce no pre-existing code. Without this, the arm above would pass
/// on a build where `code.trigger` had stopped being granted at all.
#[test]
fn the_same_trigger_only_charter_still_admits_a_trigger_that_binds_no_code() {
    let charter = trigger_only_charter();
    lower(
        &charter,
        &zero_migrate_sqlite::DIALECT,
        vec![create_table_op(), body_trigger_op()],
    )
    .expect("a body trigger introduces no pre-existing code and needs no function grant");
    lower(
        &charter,
        &zero_migrate_postgres::DIALECT,
        vec![create_table_op(), drop_trigger_op()],
    )
    .expect("dropping a trigger unbinds code and needs no function grant");
}

/// The other direction: granting BOTH admits the binding. Without this the refusal
/// above would be indistinguishable from the op having become unreachable.
#[test]
fn granting_the_function_capability_as_well_admits_the_binding() {
    let artifact = lower(
        &trigger_and_function_charter(),
        &zero_migrate_postgres::DIALECT,
        vec![create_table_op(), bind_function_op()],
    )
    .expect("a charter granting code.trigger AND code.function must admit the binding");
    let sql = artifact
        .plan
        .steps
        .iter()
        .filter_map(|step| match step {
            zero_migrate::PlanStep::Ddl(migration) => Some(migration.up.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_uppercase();
    assert!(
        sql.contains("TRIGGER") && sql.contains("FUNCTION"),
        "the granted binding lowered without emitting its DDL:\n{sql}"
    );
}
