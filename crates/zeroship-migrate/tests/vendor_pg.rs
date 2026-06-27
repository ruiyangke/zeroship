//! VENDOR (`@zeroship/migrate/pg`) V1 — render + capability-gate + closed-enum +
//! ir_version + checksum-neutrality faithful tests (vendor spec §6.1 PR-V1).
//!
//! Each test would FAIL pre-change (the Op variants, the capability gate, the
//! vendor renderer, and the current IR version did not exist), per
//! `feedback_regression_test_per_fix`.

use zeroship_migrate::model::capability::{VendorCapabilities, VendorCapability};
use zeroship_migrate::model::expr::{CastTarget, Expr, ScalarFn};
use zeroship_migrate::model::ir::{
    CanonicalOpList, ForEach, FuncLanguage, GrantTarget, IrScalar, MigrationIr, Op, PolicyCmd,
    Privilege, RaiseLevel, TriggerAction, TriggerEvent, TriggerStmt, TriggerTiming,
    CURRENT_IR_VERSION,
};
use zeroship_migrate::model::validate::{
    validate_ir_scoped, Dialect, CODE_UNSUPPORTED, CODE_VENDOR_OP_DENIED,
};
use zeroship_migrate::SchemaScope;
use zeroship_migrate::render::vendor::render_vendor_op;
use zeroship_migrate::{Checksum, GuardConfig, IrAuthor, LiveSchema, MigrationFlags, SqlDialect};

const SCHEMA: &str = "zeroship";

fn ir_with(ops: Vec<Op>) -> MigrationIr {
    MigrationIr {
        ir_version: CURRENT_IR_VERSION,
        name: "m".into(),
        owner_app: "app_corpus".into(),
        ops,
        flags: Default::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    }
}

/// The single rendered `up` of a one-statement vendor op.
fn render_up(op: &Op) -> String {
    let stmts = render_vendor_op(op, SCHEMA).expect("vendor render");
    assert_eq!(stmts.len(), 1, "expected one statement, got {stmts:?}");
    stmts[0].up.clone()
}

// ── 1. Render parity per vendor family ──────────────────────────────────────

#[test]
fn render_create_schema() {
    let op = Op::CreateSchema { name: "zeroship".into(), if_not_exists: Some(true), authorization: None };
    assert_eq!(render_up(&op), r#"CREATE SCHEMA IF NOT EXISTS "zeroship""#);
}

#[test]
fn render_create_extension_with_schema() {
    let op = Op::CreateExtension {
        name: "uuid-ossp".into(),
        if_not_exists: Some(true),
        schema: Some("oauth_hydra".into()),
    };
    // The hyphenated extension name must be double-quoted by the seam.
    assert_eq!(
        render_up(&op),
        r#"CREATE EXTENSION IF NOT EXISTS "uuid-ossp" WITH SCHEMA "oauth_hydra""#
    );
}

#[test]
fn render_create_role_with_search_path_is_two_statements() {
    let op = Op::CreateRole {
        name: "zeroship_auth".into(),
        login: Some(true),
        password: Some("zeroship_auth".into()),
        bypass_rls: Some(true),
        create_role: None,
        create_db: None,
        superuser: None,
        in_role: None,
        set_search_path: Some(vec!["zeroship".into(), "public".into()]),
        if_not_exists: Some(true),
    };
    let stmts = render_vendor_op(&op, SCHEMA).expect("render");
    assert_eq!(stmts.len(), 2, "createRole+setSearchPath ⇒ 2 statements");
    // The CREATE is wrapped in the engine-synthesized pg_roles existence probe.
    assert!(stmts[0].up.contains("SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_auth'"));
    assert!(stmts[0].up.contains(r#"CREATE ROLE "zeroship_auth" LOGIN PASSWORD 'zeroship_auth' BYPASSRLS"#));
    assert_eq!(
        stmts[1].up,
        r#"ALTER ROLE "zeroship_auth" SET search_path = "zeroship", "public""#
    );
}

#[test]
fn render_grant_table() {
    let op = Op::Grant {
        privileges: vec![Privilege::Select, Privilege::Insert, Privilege::Update, Privilege::Delete],
        on: GrantTarget::Table { names: vec!["users".into()], schema: Some("zeroship".into()) },
        to: vec!["zeroship_auth".into()],
        with_grant_option: None,
    };
    assert_eq!(
        render_up(&op),
        r#"GRANT SELECT, INSERT, UPDATE, DELETE ON "zeroship"."users" TO "zeroship_auth""#
    );
}

#[test]
fn render_revoke_from_public() {
    let op = Op::Revoke {
        privileges: vec![Privilege::Update, Privilege::Delete, Privilege::Truncate],
        on: GrantTarget::Table { names: vec!["audit_events".into()], schema: None },
        from: vec!["public".into()],
    };
    // `public` is the reserved PUBLIC sentinel (unquoted keyword); absent target
    // schema falls back to the effective schema.
    assert_eq!(
        render_up(&op),
        r#"REVOKE UPDATE, DELETE, TRUNCATE ON "zeroship"."audit_events" FROM PUBLIC"#
    );
}

#[test]
fn render_enable_rls() {
    let op = Op::EnableRls { table: "app_secrets".into(), schema: None };
    assert_eq!(render_up(&op), r#"ALTER TABLE "zeroship"."app_secrets" ENABLE ROW LEVEL SECURITY"#);
}

#[test]
fn render_create_policy_with_closed_predicate() {
    // app_id = current_setting('zeroship.tenant_app', true)::text  (the 0025 shape).
    let predicate = Expr::BinOp {
        op: zeroship_migrate::model::expr::BinaryOp::Eq,
        lhs: Box::new(Expr::col("app_id")),
        rhs: Box::new(Expr::Cast {
            operand: Box::new(Expr::FnCall {
                r#fn: ScalarFn::CurrentSetting,
                args: vec![
                    Expr::lit(IrScalar::Str("zeroship.tenant_app".into())),
                    Expr::lit(IrScalar::Bool(true)),
                ],
            }),
            target: CastTarget::Text,
        }),
    };
    let op = Op::CreatePolicy {
        name: "tenant_isolation".into(),
        table: "app_secrets".into(),
        schema: None,
        for_cmd: PolicyCmd::All,
        to: None,
        using: predicate,
        with_check: None,
    };
    let up = render_up(&op);
    assert!(up.starts_with(r#"CREATE POLICY "tenant_isolation" ON "zeroship"."app_secrets" FOR ALL USING ("#), "got: {up}");
    // The closed predicate renders structurally — the GUC scalar + cast, never raw.
    assert!(up.contains(r#"current_setting('zeroship.tenant_app', TRUE)"#), "got: {up}");
    assert!(up.contains("CAST("), "got: {up}");
}

fn execute_trigger_op() -> Op {
    Op::CreateTrigger {
        name: "audit_events_block_update".into(),
        table: "audit_events".into(),
        schema: None,
        timing: TriggerTiming::Before,
        events: vec![TriggerEvent::Update, TriggerEvent::Delete],
        for_each: ForEach::Row,
        action: TriggerAction::ExecuteFunction { name: "audit_events_block_tamper".into() },
        when: None,
    }
}

fn body_trigger_op(message: &str) -> Op {
    Op::CreateTrigger {
        name: "audit_events_append_only".into(),
        table: "audit_events".into(),
        schema: None,
        timing: TriggerTiming::Before,
        events: vec![TriggerEvent::Update],
        for_each: ForEach::Row,
        action: TriggerAction::Body {
            statements: vec![TriggerStmt::Raise {
                level: RaiseLevel::Abort,
                message: message.into(),
                errcode: None,
            }],
        },
        when: None,
    }
}

#[test]
fn render_create_trigger_execute_function_matches_audit_shape() {
    let op = execute_trigger_op();
    assert_eq!(
        render_up(&op),
        r#"CREATE TRIGGER "audit_events_block_update" BEFORE UPDATE OR DELETE ON "zeroship"."audit_events" FOR EACH ROW EXECUTE FUNCTION "zeroship"."audit_events_block_tamper"()"#
    );
}

#[test]
fn sqlite_body_trigger_with_raise_abort_renders_exact_closed_body() {
    let ir = ir_with(vec![body_trigger_op("append-only")]);
    let author = IrAuthor::new("app", "app_corpus", SqlDialect::Sqlite);
    let migs = author.lower(&ir, &LiveSchema::default()).expect("SQLite body trigger lowers");
    assert_eq!(migs.len(), 1);
    assert_eq!(
        migs[0].up,
        r#"CREATE TRIGGER "audit_events_append_only" BEFORE UPDATE ON "audit_events" FOR EACH ROW BEGIN SELECT RAISE(ABORT,'append-only'); END;"#
    );
}

#[test]
fn trigger_action_and_facet_refusals_are_dialect_specific() {
    let scope = SchemaScope::Single("app".into());

    let pg_body = ir_with(vec![body_trigger_op("append-only")]);
    let err = validate_ir_scoped(&pg_body, Dialect::Postgres, &[], Some(&scope))
        .expect_err("PG must reject closed trigger bodies instead of synthesizing functions");
    assert_eq!(err.code, CODE_UNSUPPORTED);
    assert_eq!(err.dialect, Dialect::Postgres);
    assert!(err.reason.contains("triggerBody"), "got {err:?}");

    let sqlite_exec = ir_with(vec![execute_trigger_op()]);
    let err = validate_ir_scoped(&sqlite_exec, Dialect::Sqlite, &[], Some(&scope))
        .expect_err("SQLite must reject ExecuteFunction trigger actions");
    assert_eq!(err.code, CODE_UNSUPPORTED);
    assert_eq!(err.dialect, Dialect::Sqlite);
    assert!(err.reason.contains("executeFunction"), "got {err:?}");

    let mut truncate = body_trigger_op("append-only");
    if let Op::CreateTrigger { events, .. } = &mut truncate {
        *events = vec![TriggerEvent::Truncate];
    }
    let err = validate_ir_scoped(&ir_with(vec![truncate]), Dialect::Sqlite, &[], Some(&scope))
        .expect_err("SQLite must reject only the TRUNCATE facet");
    assert_eq!(err.code, CODE_UNSUPPORTED);
    assert_eq!(err.dialect, Dialect::Sqlite);
    assert!(err.reason.contains("triggerEventTruncate"), "got {err:?}");

    let mut statement = body_trigger_op("append-only");
    if let Op::CreateTrigger { for_each, .. } = &mut statement {
        *for_each = ForEach::Statement;
    }
    let err = validate_ir_scoped(&ir_with(vec![statement]), Dialect::Sqlite, &[], Some(&scope))
        .expect_err("SQLite must reject only the FOR EACH STATEMENT facet");
    assert_eq!(err.code, CODE_UNSUPPORTED);
    assert_eq!(err.dialect, Dialect::Sqlite);
    assert!(err.reason.contains("forEachStatement"), "got {err:?}");
}

#[test]
fn body_trigger_is_not_vendor_gated_under_confined_validate_or_lower_guarded() {
    let ir = ir_with(vec![body_trigger_op("append-only")]);
    let scope = SchemaScope::Single("app".into());
    validate_ir_scoped(&ir, Dialect::Sqlite, &[], Some(&scope))
        .expect("Body trigger is cross-dialect core and should validate under Confined");

    let author = IrAuthor::new("app", "app_corpus", SqlDialect::Sqlite);
    let guard_cfg = GuardConfig::confined_sqlite("app");
    let (steps, fragments) = author
        .lower_guarded(&ir, &guard_cfg, &LiveSchema::default())
        .expect("Body trigger should lower through the Confined guarded SQLite path");
    assert_eq!(steps.len(), 1);
    assert_eq!(fragments.len(), 1);
    assert_eq!(
        fragments[0].sql,
        r#"CREATE TRIGGER "audit_events_append_only" BEFORE UPDATE ON "audit_events" FOR EACH ROW BEGIN SELECT RAISE(ABORT,'append-only'); END;"#
    );
}

#[test]
fn trigger_raise_message_is_sql_literal_escaped() {
    let ir = ir_with(vec![body_trigger_op("can't mutate rows")]);
    let author = IrAuthor::new("app", "app_corpus", SqlDialect::Sqlite);
    let migs = author.lower(&ir, &LiveSchema::default()).expect("SQLite body trigger lowers");
    assert_eq!(
        migs[0].up,
        r#"CREATE TRIGGER "audit_events_append_only" BEFORE UPDATE ON "audit_events" FOR EACH ROW BEGIN SELECT RAISE(ABORT,'can''t mutate rows'); END;"#
    );
    assert!(
        !migs[0].up.contains("can't mutate"),
        "embedded quote must be doubled inside the SQL literal: {}",
        migs[0].up
    );
}

#[test]
fn pg_body_trigger_lower_refuses_with_trigger_body_kind() {
    let ir = ir_with(vec![body_trigger_op("append-only")]);
    let author = IrAuthor::new(SCHEMA, "app_corpus", SqlDialect::Postgres);
    let err = author
        .lower(&ir, &LiveSchema::default())
        .expect_err("PG Body trigger must fail closed at lower");
    match err {
        zeroship_migrate::render::lower::IrLowerError::TriggerUnsupported { kind, dialect } => {
            assert_eq!(kind, "triggerBody");
            assert_eq!(dialect, SqlDialect::Postgres);
        }
        other => panic!("expected triggerBody unsupported, got {other:?}"),
    }
}

#[test]
fn render_create_function_embeds_body_verbatim() {
    let op = Op::CreateFunction {
        name: "audit_events_block_tamper".into(),
        schema: Some("zeroship".into()),
        args: None,
        returns: "trigger".into(),
        language: FuncLanguage::Plpgsql,
        replace: Some(true),
        volatility: None,
        body: "BEGIN RAISE EXCEPTION 'append-only'; END;".into(),
    };
    let up = render_up(&op);
    assert!(up.starts_with(r#"CREATE OR REPLACE FUNCTION "zeroship"."audit_events_block_tamper"() RETURNS trigger LANGUAGE plpgsql AS $zsfn$"#), "got: {up}");
    // The raw body is embedded verbatim (the one genuine escape, §2.6).
    assert!(up.contains("BEGIN RAISE EXCEPTION 'append-only'; END;"), "got: {up}");
}

#[test]
fn pg_raw_non_empty_binds_are_rejected_until_parameterized_execution_exists() {
    let op = Op::PgRaw {
        sql: "SELECT set_config('zeroship.tenant_app', $1, false)".into(),
        binds: vec![IrScalar::Str("app_demo".into())],
    };
    assert!(
        render_vendor_op(&op, SCHEMA).is_err(),
        "PgRaw binds are a false contract until PgRaw executes as a parameterized plan step"
    );
}

#[test]
fn render_drop_family() {
    assert_eq!(
        render_up(&Op::DropSchema { name: "zeroship".into(), if_exists: Some(true), cascade: Some(true) }),
        r#"DROP SCHEMA IF EXISTS "zeroship" CASCADE"#
    );
    assert_eq!(
        render_up(&Op::DropOwnedBy { roles: vec!["zeroship_auth".into()] }),
        r#"DROP OWNED BY "zeroship_auth""#
    );
}

// ── 2. The capability gate (fail-closed under Confined, allowed under Platform) ─

/// A representative vendor op of each capability family (for the gate tests).
fn vendor_samples() -> Vec<Op> {
    vec![
        Op::CreateRole {
            name: "r".into(), login: None, password: None, bypass_rls: None, create_role: None,
            create_db: None, superuser: None, in_role: None, set_search_path: None, if_not_exists: None,
        },
        Op::CreateFunction {
            name: "f".into(), schema: None, args: None, returns: "void".into(),
            language: FuncLanguage::Sql, replace: None, volatility: None, body: "SELECT 1".into(),
        },
        Op::PgRaw { sql: "SELECT 1".into(), binds: vec![] },
    ]
}

#[test]
fn confined_refuses_every_vendor_op_with_vendor_op_denied() {
    let confined = SchemaScope::Single("app1".into());
    for op in vendor_samples() {
        let ir = ir_with(vec![op.clone()]);
        let err = validate_ir_scoped(&ir, Dialect::Postgres, &[], Some(&confined))
            .expect_err("confined must refuse the vendor op");
        // The error code must be the VALIDATE-layer VENDOR_OP_DENIED (gate 1) — NOT
        // a generic guard denial; this pins that the validate gate fired, not just
        // "denied somewhere downstream".
        assert_eq!(
            err.code, CODE_VENDOR_OP_DENIED,
            "vendor op {op:?} must be refused with VENDOR_OP_DENIED under Confined; got {err:?}"
        );
    }
}

#[test]
fn platform_allowlist_permits_vendor_ops() {
    let platform = SchemaScope::Allowlist(vec!["zeroship".into(), "public".into()]);
    for op in vendor_samples() {
        let ir = ir_with(vec![op.clone()]);
        validate_ir_scoped(&ir, Dialect::Postgres, &[], Some(&platform))
            .unwrap_or_else(|e| panic!("platform must permit vendor op {op:?}; got {e:?}"));
    }
}

#[test]
fn trusted_none_scope_permits_vendor_ops() {
    // None scope (the Trusted dbmate posture) maps onto the operator preset.
    let ir = ir_with(vendor_samples());
    validate_ir_scoped(&ir, Dialect::Postgres, &[], None).expect("trusted permits vendor ops");
}

#[test]
fn capability_presets_compose_as_specified() {
    assert!(!VendorCapabilities::confined().grants(VendorCapability::Role));
    assert!(VendorCapabilities::operator().grants(VendorCapability::RawSql));
    // local: structural DDL yes, role + raw escape no.
    let local = VendorCapabilities::local();
    assert!(local.grants(VendorCapability::Policy));
    assert!(!local.grants(VendorCapability::Role));
    assert!(!local.grants(VendorCapability::RawSql));
}

// ── 3. SQLite PgOnly refusal (§4.3) ─────────────────────────────────────────

#[test]
fn sqlite_target_hard_rejects_vendor_ops_at_validate() {
    // Even under a permissive (Platform) scope, a SQLite deploy of a vendor op is
    // refused fail-closed at load — every vendor op is PgOnly.
    let platform = SchemaScope::Allowlist(vec!["zeroship".into()]);
    let ir = ir_with(vec![Op::CreateRole {
        name: "r".into(), login: None, password: None, bypass_rls: None, create_role: None,
        create_db: None, superuser: None, in_role: None, set_search_path: None, if_not_exists: None,
    }]);
    let err = validate_ir_scoped(&ir, Dialect::Sqlite, &[], Some(&platform))
        .expect_err("SQLite must refuse the vendor op");
    assert_eq!(err.code, CODE_UNSUPPORTED, "SQLite vendor op ⇒ UNSUPPORTED; got {err:?}");
}

// ── 4. Closed sub-enum rejection at deserialize ─────────────────────────────

#[test]
fn untrusted_function_language_rejected_at_deserialize() {
    // `plpythonu` is NOT in the closed FuncLanguage 2-set — serde rejects it BEFORE
    // any body deny-scan runs.
    let json = r#"{"op":"createFunction","name":"f","returns":"void","language":"plpythonu","body":"x"}"#;
    let r: Result<Op, _> = serde_json::from_str(json);
    assert!(r.is_err(), "an untrusted PL must be rejected at deserialize");
}

#[test]
fn out_of_set_privilege_rejected_at_deserialize() {
    let json = r#"{"op":"grant","privileges":["drop"],"on":{"kind":"table","names":["t"]},"to":["r"]}"#;
    let r: Result<Op, _> = serde_json::from_str(json);
    assert!(r.is_err(), "an injection-shaped privilege token must be rejected at deserialize");
}

#[test]
fn out_of_set_trigger_timing_rejected_at_deserialize() {
    let json = r#"{"op":"createTrigger","name":"t","table":"x","timing":"sideways","events":["insert"],"forEach":"row","action":{"kind":"executeFunction","name":"f"}}"#;
    let r: Result<Op, _> = serde_json::from_str(json);
    assert!(r.is_err(), "an out-of-set trigger timing must be rejected at deserialize");
}

// ── 5. ir_version 4 ─────────────────────────────────────────────────────────

#[test]
fn ir_version_is_current() {
    assert_eq!(CURRENT_IR_VERSION, 4);
    // A future version is fail-closed; the current version validates.
    let mut ir = ir_with(vec![]);
    ir.ir_version = CURRENT_IR_VERSION + 1;
    assert!(ir.check_ir_version().is_err(), "a future ir_version must be refused");
    ir.ir_version = CURRENT_IR_VERSION;
    assert!(ir.check_ir_version().is_ok());
}

// ── 6. Checksum-neutrality for a v2 (core-only) artifact ────────────────────

#[test]
fn v2_core_op_wire_image_is_unchanged() {
    // The vendor variant additions are checksum-NEUTRAL for a v2 artifact: a
    // core op's wire image is byte-identical (every new field is omitted-when-
    // absent), so its `Checksum::of_ir` is unchanged. We pin the wire image of a
    // representative core op — no vendor field leaks in.
    let op = Op::DropTable { table: "t".into(), cascade: None, schema: None, existence_guard: None };
    let json = serde_json::to_string(&op).unwrap();
    assert_eq!(json, r#"{"op":"dropTable","table":"t"}"#);

    // And a v2 artifact (ir_version = 2) still loads + checksums deterministically.
    let mut ir = ir_with(vec![op]);
    ir.ir_version = 2;
    assert!(ir.check_ir_version().is_ok(), "a v2 artifact is still understood by the v3 engine");
    let a = Checksum::of_ir(
        &CanonicalOpList(&ir.ops),
        &MigrationFlags::default(),
        &ir.owner_app,
        &[],
        &[],
        &ir.preconditions,
    );
    let b = Checksum::of_ir(
        &CanonicalOpList(&ir.ops),
        &MigrationFlags::default(),
        &ir.owner_app,
        &[],
        &[],
        &ir.preconditions,
    );
    assert_eq!(a.as_str(), b.as_str(), "the core op-list checksum is deterministic");
}
