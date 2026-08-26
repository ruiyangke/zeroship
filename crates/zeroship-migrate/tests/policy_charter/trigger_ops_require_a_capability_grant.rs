//! What must an author be GRANTED before `createTrigger` / `dropTrigger` is accepted?
//!
//! The answer this file MEASURES is: [`VendorCapability::Trigger`], on every one of
//! the three dialects, at both gates. A trigger op is a member of the privileged
//! capability family; the charter knob that grants it is `code.trigger`.
//!
//! # Why the question needed measuring rather than reading
//!
//! This file previously measured the OPPOSITE and was right to: trigger ops sat in
//! `vendor_capabilities`'s portable-core arm returning the empty set, and
//! `validate_vendor_op` returns `Ok` on an empty set before it can consult a backend
//! refusal or a charter grant. Prose in two places already CLAIMED triggers were in
//! the privileged family while the gate could not see them. The claim is now true and
//! the prose is now checkable, which is what this file checks.
//!
//! # The one axis a trigger does NOT share with the rest of the family
//!
//! Every other capability in the closed [`VendorCapability`] set belongs to the
//! privileged catalog-object family, which exactly ONE registered backend renders, so
//! an artifact carrying one measures a single-dialect reach. A trigger is rendered by
//! all three, in each backend's own action shape. The op therefore keeps a
//! `SupportTier::Core` declaration and a portable reach, and the capability governs
//! AUTHORITY alone. [`the_trigger_grant_did_not_pin_the_op_to_one_dialect`] holds that
//! apart, because reading "capability-gated" as "PG-only" is exactly the one-axis
//! misreading this whole area has already produced once.
//!
//! # The instrument, and how each arm proves it was live
//!
//! A test that shows a trigger op IS refused proves nothing unless the same call, on
//! the same path, under the same posture, is able to ADMIT. Every arm below therefore
//! carries two contrasts, decided by the same code on the same call:
//!
//! - `createFunction`, a capability-gated op (`code.function`), which must still be
//!   refused where nothing is granted and admitted where everything is - the control
//!   that the gate is reached at all;
//! - `dropIndex` / a bare `createTable`, ops carrying NO capability, which must still
//!   be admitted under the very posture that refuses the trigger - the control that
//!   the refusal is a GRANT decision and not the posture refusing everything.
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
use zeroship_migrate::guard::GuardConfig;
use zeroship_migrate::model::capability::VendorCapability;
use zeroship_migrate::model::ir::MigrationIr;
use zeroship_migrate::model::op_support::vendor_capabilities;
use zeroship_migrate::model::table_shape::resolve_create_table_policy;
use zeroship_migrate::model::validate::{validate_ir_authorized, CODE_VENDOR_OP_DENIED};
use zeroship_migrate::render::lower::{
    IrAuthor, LiveSchema, LoadAndLowerGuardedError, LoweredArtifact,
};
use zeroship_migrate::{
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
        zeroship_migrate_postgres::DIALECT,
        zeroship_migrate_mysql::DIALECT,
        zeroship_migrate_sqlite::DIALECT,
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
    let action = if dialect == &zeroship_migrate_postgres::DIALECT {
        json!({ "kind": "executeFunction", "name": FUNCTION })
    } else if dialect == &zeroship_migrate_mysql::DIALECT {
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

/// The GATED CONTROL op: creating a function requires [`VendorCapability::Function`].
/// Every arm pairs its trigger claim with this one so a green cannot come from a gate
/// that was never reached.
fn create_function_op() -> Value {
    json!({
        "op": "createFunction", "name": FUNCTION, "returns": "trigger",
        "language": "procedural", "body": "BEGIN RETURN NEW; END",
    })
}

/// The UNGATED CONTROL op: dropping an index requires no vendor capability at all, and
/// is the one remaining member of `Op::is_destructive`'s non-destructive family that
/// carries none. Every arm pairs its trigger claim with this one so a green cannot come
/// from a posture that has started refusing everything.
fn drop_index_op() -> Value {
    json!({ "op": "dropIndex", "name": "audited_val_idx", "table": TABLE })
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
/// question "what must be granted" is decided here, and for a trigger op the answer
/// names [`VendorCapability::Trigger`].
#[test]
fn the_gate_reads_the_trigger_capability_for_a_trigger_op() {
    for dialect in dialects() {
        for op in [drop_trigger_op(), create_trigger_op(&dialect)] {
            let parsed: MigrationIr = serde_json::from_str(&envelope("caps", vec![op.clone()]))
                .expect("the trigger envelope parses");
            assert!(
                vendor_capabilities(&parsed.ops[0]).contains(&VendorCapability::Trigger),
                "{dialect}: a trigger op must require the trigger capability, but the gate reads \
                 {:?}: {op}",
                vendor_capabilities(&parsed.ops[0])
            );
        }
    }

    let control: MigrationIr = serde_json::from_str(&envelope("caps", vec![create_function_op()]))
        .expect("the control envelope parses");
    assert_eq!(
        vendor_capabilities(&control.ops[0]),
        vec![VendorCapability::Function],
        "the gated control op must be capability-gated, or the contrast above measures nothing"
    );

    let ungated: MigrationIr = serde_json::from_str(&envelope("caps", vec![drop_index_op()]))
        .expect("the ungated control envelope parses");
    assert_eq!(
        vendor_capabilities(&ungated.ops[0]),
        Vec::new(),
        "the ungated control op must require nothing, or this function has started \
         answering with a capability for everything"
    );
}

/// `Op::is_destructive`'s doc names a FAMILY it declares deliberately non-destructive:
/// `DROP INDEX`, `DROP ROLE`, `DROP EXTENSION`, `DROP POLICY`, `DROP TRIGGER`, `REVOKE`.
/// Reading that list as one decision invites the conclusion that its members are all
/// governed alike. They are not: "non-destructive" and "capability-gated" are two
/// axes, and this arm measures the second one member by member.
///
/// `dropTrigger` has now joined the gated majority. `dropIndex` is the sole member
/// carrying no capability, and it has the compensating control the IR's own doc
/// records instead: a UNIQUE-index drop is gated through `MigrationFlags`. Keeping it
/// here is what makes this arm a measurement rather than a restatement of "everything
/// is gated".
#[test]
fn the_index_drop_is_the_last_family_member_with_no_capability_behind_it() {
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
        (
            r#"{"op":"dropTrigger","name":"audited_touch_trg","table":"audited"}"#,
            VendorCapability::Trigger,
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

    let op_json = r#"{"op":"dropIndex","name":"audited_val_idx","table":"audited"}"#;
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

/// The grant governs AUTHORITY, not REACH. Every other capability in the closed set
/// belongs to the privileged catalog-object family that exactly one backend renders,
/// so an artifact carrying one is pinned to that dialect. A trigger is rendered by all
/// three, and gating it must not have quietly narrowed where a trigger migration can
/// be deployed.
/// `dropTrigger` is the shape every backend renders identically, so it is the one that
/// can carry the whole-universe reach claim. A `createTrigger` carries a per-dialect
/// ACTION, and which actions a backend accepts is a facet question this file does not
/// ask - so each create is only asserted to reach its own dialect.
#[test]
fn the_trigger_grant_did_not_pin_the_op_to_one_dialect() {
    let ir: MigrationIr =
        serde_json::from_str(&envelope("reach", vec![drop_trigger_op()])).expect("envelope parses");
    let supported =
        zeroship_migrate::model::op_support::support(zeroship_migrate::shipping_vendors(), &ir.ops[0])
            .supported_dialects(zeroship_migrate::shipping_vendors());
    for dialect in dialects() {
        assert!(
            supported.contains_id(&dialect),
            "{dialect}: the trigger capability must not have narrowed a trigger drop's reach"
        );
    }

    for dialect in dialects() {
        let op = create_trigger_op(&dialect);
        let ir: MigrationIr =
            serde_json::from_str(&envelope("reach", vec![op.clone()])).expect("envelope parses");
        assert!(
            zeroship_migrate::model::op_support::support(zeroship_migrate::shipping_vendors(), &ir.ops[0])
                .supported_dialects(zeroship_migrate::shipping_vendors())
                .contains_id(&dialect),
            "{dialect}: the trigger capability must not have cost this backend its own \
             create shape: {op}"
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
fn validate_refuses_trigger_ops_under_the_posture_that_grants_nothing() {
    let scope = confined_scope();
    for dialect in dialects() {
        for (label, op) in [
            ("dropTrigger", drop_trigger_op()),
            ("createTrigger", create_trigger_op(&dialect)),
        ] {
            let ir: MigrationIr = serde_json::from_str(&envelope("t", vec![create_table_op(), op]))
                .expect("the trigger envelope parses");
            let error = validate_ir_authorized(
                zeroship_migrate::shipping_vendors(),
                &ir,
                &dialect,
                Some(&scope),
                None,
            )
            .expect_err(&format!(
                "{dialect}: {label} must need a grant, but the posture that grants nothing \
                     admitted it"
            ));
            assert_eq!(
                error.code, CODE_VENDOR_OP_DENIED,
                "{dialect}: {label} was refused for the wrong reason: {}",
                error.reason
            );
            assert!(
                error.reason.contains(VendorCapability::Trigger.flag_name()),
                "{dialect}: {label}'s refusal does not name the trigger capability: {}",
                error.reason
            );
        }
    }
}

/// The first instrument check for the arm above: the SAME call, the SAME posture, on
/// the dialect that renders the privileged family, DOES refuse an op that needs a
/// different grant - so the gate is reached and keys on the capability it reads.
#[test]
fn the_same_validate_posture_still_refuses_an_op_that_needs_a_grant() {
    let scope = confined_scope();
    let ir: MigrationIr = serde_json::from_str(&envelope(
        "f",
        vec![create_table_op(), create_function_op()],
    ))
    .expect("the control envelope parses");
    let error = validate_ir_authorized(
        zeroship_migrate::shipping_vendors(),
        &ir,
        &zeroship_migrate_postgres::DIALECT,
        Some(&scope),
        None,
    )
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

/// The second instrument check, and the one the inversion of this file made necessary:
/// the SAME call under the SAME posture still ADMITS an op that needs no grant. Without
/// it, a build where the confined posture had begun refusing every op would pass the
/// trigger arm above for entirely the wrong reason.
#[test]
fn the_same_validate_posture_still_admits_an_op_that_needs_no_grant() {
    let scope = confined_scope();
    for dialect in dialects() {
        let ir: MigrationIr =
            serde_json::from_str(&envelope("i", vec![create_table_op(), drop_index_op()]))
                .expect("the ungated control envelope parses");
        validate_ir_authorized(
            zeroship_migrate::shipping_vendors(),
            &ir,
            &dialect,
            Some(&scope),
            None,
        )
        .unwrap_or_else(|e| {
            panic!(
                "{dialect}: dropIndex needs no grant, so the posture that grants nothing must \
                 still admit it: {} / {}",
                e.code, e.reason
            )
        });
    }
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
key = "code.trigger"
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
    let author = IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
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
fn a_charter_granting_no_vendor_capability_refuses_a_trigger_drop() {
    for dialect in dialects() {
        let error = lower(
            &no_vendor_capability_charter(),
            &dialect,
            vec![create_table_op(), drop_trigger_op()],
        )
        .expect_err(&format!(
            "{dialect}: a charter granting no vendor capability must refuse a trigger drop"
        ));
        let reported = format!("{error:?}");
        assert!(
            reported.contains(CODE_VENDOR_OP_DENIED)
                || reported.contains(&format!("{:?}", VendorCapability::Trigger)),
            "{dialect}: the trigger drop was refused for the wrong reason: {reported}"
        );
    }
}

/// The sharpest form of "gated": the emptiest vendor charter and the full operator
/// charter DISAGREE about a trigger drop, and the difference is exactly the DDL. A
/// grant that makes no difference is not a gate; this is the assertion that inverted
/// when the grant was introduced.
#[test]
fn granting_every_vendor_capability_is_what_lowers_a_trigger_drop() {
    for dialect in dialects() {
        lower(
            &no_vendor_capability_charter(),
            &dialect,
            vec![create_table_op(), drop_trigger_op()],
        )
        .expect_err("the ungranted charter must not lower the trigger drop");
        let granted = lower(
            &every_vendor_capability_charter(),
            &dialect,
            vec![create_table_op(), drop_trigger_op()],
        )
        .unwrap_or_else(|e| {
            panic!("{dialect}: a charter granting code.trigger must lower a trigger drop: {e:?}")
        });
        assert!(
            emitted_sql(&granted)
                .to_ascii_uppercase()
                .contains("TRIGGER"),
            "{dialect}: the granted trigger drop lowered without emitting its DDL:\n{}",
            emitted_sql(&granted)
        );
    }
}

/// The first instrument check for the two arms above: those same two charters disagree
/// about an op that is gated on a DIFFERENT capability. Without this, both arms would
/// pass on a build where the charter axis had stopped being read at all.
#[test]
fn the_same_two_charters_still_disagree_about_an_op_that_needs_a_grant() {
    let dialect = zeroship_migrate_postgres::DIALECT;
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

/// The second instrument check, made necessary by the inversion: the emptiest vendor
/// charter still LOWERS an op that needs no vendor capability. Without it, a build
/// where that charter had begun refusing everything would pass the two arms above for
/// entirely the wrong reason.
#[test]
fn the_charter_granting_no_vendor_capability_still_lowers_an_ungated_op() {
    for dialect in dialects() {
        let artifact = lower(
            &no_vendor_capability_charter(),
            &dialect,
            vec![create_table_op(), drop_index_op()],
        )
        .unwrap_or_else(|e| {
            panic!("{dialect}: dropIndex needs no vendor capability, so this charter must lower it: {e:?}")
        });
        assert!(
            emitted_sql(&artifact)
                .to_ascii_uppercase()
                .contains("INDEX"),
            "{dialect}: the ungated control lowered without emitting its DDL:\n{}",
            emitted_sql(&artifact)
        );
    }
}
