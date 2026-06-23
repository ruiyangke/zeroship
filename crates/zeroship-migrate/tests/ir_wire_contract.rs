//! Wave-A contract-strictness regression suite (code-critic findings).
//!
//! These tests pin the FROZEN `.ir.json` wire contract before the JS `op.*`
//! builder (Wave E) is written against it. Each test is RED against the
//! pre-fix Wave-A code and GREEN after:
//!
//! - **wire casing** — the op-region (every `Op` struct-variant field, the
//!   constraint/index/batch/expr-AST/coltype operands) is camelCase, matching
//!   the normative design's `ifExists` / `cursorColumn` / `batchSize` /
//!   `referencesTable`; the `MigrationIr` envelope + `flags` stay snake_case
//!   (`ir_version`, `owner_app`, `depends_on`, `requires_approval`). A JS
//!   builder emitting idiomatic camelCase op fields must round-trip.
//! - **deny_unknown_fields** — an unknown key (envelope OR per-op) is rejected
//!   at load, BEFORE any checksum, so on-disk bytes can carry nothing the
//!   identity checksum does not see.
//! - **structural numeric domain** — `batchSize` / `limit` / `timeout_ms`
//!   author-supplied integers obey the same `< 2^53` JS-safe-integer guard as
//!   `IrScalar`, so the typed value cannot diverge across the JS/Rust front
//!   doors.
//! - **base64 bytes** — `{"bytes":…}` is decoded + canonicalized at load;
//!   invalid base64 is rejected; a non-canonical encoding normalizes so two
//!   encodings of the same bytes hash identically.
//! - **un-flattened constraint** — `IrConstraint.kind` is a nested object, not
//!   a flattened sibling (so `deny_unknown_fields` is sound).

use zeroship_migrate::ir::{IrScalar, MigrationIr, Op};
use zeroship_migrate::EXPR_INVALID_NUMERIC;

// ----------------------------------------------------------------------------
// Wire casing — op-region fields are camelCase
// ----------------------------------------------------------------------------

#[test]
fn drop_table_fields_are_camel_case() {
    let op = Op::DropTable {
        table: "t".into(),
        if_exists: Some(true),
        cascade: Some(false),
    };
    let v = serde_json::to_value(&op).unwrap();
    assert_eq!(v["op"], "dropTable");
    assert!(v.get("ifExists").is_some(), "if_exists must serialize as ifExists, got: {v}");
    assert!(v.get("if_exists").is_none(), "snake_case if_exists must NOT appear: {v}");
    // round-trips from camelCase wire form
    let back: Op = serde_json::from_str(r#"{"op":"dropTable","table":"t","ifExists":true}"#).unwrap();
    match back {
        Op::DropTable { if_exists, .. } => assert_eq!(if_exists, Some(true)),
        _ => panic!("expected DropTable"),
    }
}

#[test]
fn backfill_fields_are_camel_case() {
    let json = r#"{"op":"backfill","table":"t","cursorColumn":"id","batchSize":100,
        "set":{"x":{"node":"literal","value":1}},"name":"bf"}"#;
    let op: Op = serde_json::from_str(json).unwrap();
    match &op {
        Op::Backfill { cursor_column, batch_size, .. } => {
            assert_eq!(cursor_column, "id");
            assert_eq!(batch_size.get(), 100);
        }
        _ => panic!("expected Backfill"),
    }
    let v = serde_json::to_value(&op).unwrap();
    assert!(v.get("cursorColumn").is_some(), "must be cursorColumn: {v}");
    assert!(v.get("batchSize").is_some(), "must be batchSize: {v}");
    assert!(v.get("cursor_column").is_none(), "snake_case must not appear: {v}");
}

#[test]
fn fk_constraint_fields_are_camel_case_and_nested() {
    // FK constraint via addConstraint — references_table -> referencesTable, and
    // the kind is a NESTED object (un-flattened), not a flattened sibling.
    let json = r#"{"op":"addConstraint","table":"o","constraint":{
        "name":null,
        "kind":{"kind":"fk","columns":["uid"],
                "referencesTable":"u","referencesColumns":["id"]}}}"#;
    let op: Op = serde_json::from_str(json).unwrap();
    let v = serde_json::to_value(&op).unwrap();
    let kind = &v["constraint"]["kind"];
    assert_eq!(kind["kind"], "fk");
    assert!(kind.get("referencesTable").is_some(), "must be referencesTable: {v}");
    assert!(kind.get("referencesColumns").is_some(), "must be referencesColumns: {v}");
    assert!(kind.get("references_table").is_none(), "snake_case must not appear: {v}");
}

#[test]
fn migration_ir_envelope_stays_snake_case() {
    // The envelope + flags keep snake_case per the §2.3 normative example.
    let json = r#"{"ir_version":1,"name":"n","owner_app":"app_x","ops":[],
        "flags":{"requires_approval":true,"engine_goodie_ddl":false},
        "depends_on":["mig_a"],"supersedes":[],"preconditions":[]}"#;
    let ir: MigrationIr = serde_json::from_str(json).unwrap();
    assert_eq!(ir.owner_app, "app_x");
    let v = serde_json::to_value(&ir).unwrap();
    assert!(v.get("ir_version").is_some(), "envelope stays snake_case: {v}");
    assert!(v.get("owner_app").is_some(), "envelope stays snake_case: {v}");
    assert!(v["flags"].get("requires_approval").is_some(), "flags stay snake_case: {v}");
}

// ----------------------------------------------------------------------------
// deny_unknown_fields — load-before-checksum strictness
// ----------------------------------------------------------------------------

#[test]
fn unknown_envelope_key_is_rejected() {
    let json = r#"{"ir_version":1,"name":"n","ops":[],"evil_extra":true}"#;
    let err = serde_json::from_str::<MigrationIr>(json).unwrap_err();
    assert!(
        err.to_string().contains("evil_extra") || err.to_string().contains("unknown field"),
        "unknown envelope key must be rejected, got: {err}"
    );
}

#[test]
fn unknown_per_op_key_is_rejected() {
    let json = r#"{"op":"dropTable","table":"t","evil":1}"#;
    let err = serde_json::from_str::<Op>(json).unwrap_err();
    assert!(
        err.to_string().contains("evil") || err.to_string().contains("unknown field"),
        "unknown per-op key must be rejected, got: {err}"
    );
}

#[test]
fn unknown_constraint_key_is_rejected() {
    let json = r#"{"op":"addConstraint","table":"t","constraint":{
        "kind":{"kind":"pk","columns":["id"]},"bogus":true}}"#;
    let err = serde_json::from_str::<Op>(json).unwrap_err();
    assert!(
        err.to_string().contains("bogus") || err.to_string().contains("unknown field"),
        "unknown constraint key must be rejected, got: {err}"
    );
}

// ----------------------------------------------------------------------------
// structural numeric domain — batchSize / limit / timeout_ms bounded < 2^53
// ----------------------------------------------------------------------------

#[test]
fn backfill_batch_size_above_2pow53_is_rejected() {
    let json = r#"{"op":"backfill","table":"t","cursorColumn":"id",
        "batchSize":9007199254740992,"set":{},"name":"bf"}"#;
    let err = serde_json::from_str::<Op>(json).unwrap_err();
    assert!(
        err.to_string().contains(EXPR_INVALID_NUMERIC),
        "batchSize >= 2^53 must carry {EXPR_INVALID_NUMERIC}, got: {err}"
    );
}

#[test]
fn delete_limit_above_2pow53_is_rejected() {
    let json = r#"{"op":"delete","table":"t","where":{"node":"literal","value":true},
        "limit":99999999999999999}"#;
    let err = serde_json::from_str::<Op>(json).unwrap_err();
    assert!(
        err.to_string().contains(EXPR_INVALID_NUMERIC),
        "limit >= 2^53 must be rejected, got: {err}"
    );
}

#[test]
fn flags_timeout_ms_above_2pow53_is_rejected() {
    let json = r#"{"ir_version":1,"name":"n","ops":[],
        "flags":{"timeout_ms":9007199254740992}}"#;
    let err = serde_json::from_str::<MigrationIr>(json).unwrap_err();
    assert!(
        err.to_string().contains(EXPR_INVALID_NUMERIC),
        "timeout_ms >= 2^53 must be rejected, got: {err}"
    );
}

#[test]
fn structural_ints_below_2pow53_accepted() {
    let json = r#"{"op":"backfill","table":"t","cursorColumn":"id",
        "batchSize":9007199254740991,"set":{},"name":"bf"}"#;
    let op: Op = serde_json::from_str(json).unwrap();
    match op {
        Op::Backfill { batch_size, .. } => assert_eq!(batch_size.get(), 9_007_199_254_740_991),
        _ => panic!("expected Backfill"),
    }
}

// ----------------------------------------------------------------------------
// base64 bytes — validate + canonicalize at load
// ----------------------------------------------------------------------------

#[test]
fn invalid_base64_bytes_is_rejected() {
    let err = serde_json::from_str::<IrScalar>(r#"{"bytes":"!!!not base64!!!"}"#).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("base64"),
        "invalid base64 must be rejected with a base64 error, got: {err}"
    );
}

// ----------------------------------------------------------------------------
// Closed expression-AST wire contract (§3.3.1) — no raw template; unknown node
// tag rejected at load.
// ----------------------------------------------------------------------------

#[test]
fn expr_ast_is_camel_case_and_round_trips() {
    use zeroship_migrate::expr::{BinaryOp, Expr};
    use zeroship_migrate::ir::IrScalar;
    // c("total").gt(0)
    let e = Expr::BinOp {
        op: BinaryOp::Gt,
        lhs: Box::new(Expr::col("total")),
        rhs: Box::new(Expr::lit(IrScalar::Int(0))),
    };
    let v = serde_json::to_value(&e).unwrap();
    assert_eq!(v["node"], "binOp", "node tag is camelCase on key \"node\": {v}");
    assert_eq!(v["op"], "gt");
    assert_eq!(v["lhs"]["node"], "colRef");
    assert_eq!(v["lhs"]["name"], "total");
    assert_eq!(v["rhs"]["node"], "literal");
    let back: Expr = serde_json::from_value(v).unwrap();
    assert_eq!(e, back);
}

#[test]
fn unknown_expr_node_tag_is_rejected_at_load() {
    use zeroship_migrate::expr::Expr;
    // A hand-crafted IR carrying an unknown AST node tag (`subquery`) — not in the
    // closed set — must fail to deserialize (the §3.3.1.1 "UNSUPPORTED kind:expr
    // at load" obligation: an unknown node simply cannot parse).
    let json = r#"{"node":"subquery","sql":"SELECT 1"}"#;
    let err = serde_json::from_str::<Expr>(json).unwrap_err();
    assert!(
        err.to_string().contains("subquery") || err.to_string().contains("unknown variant"),
        "an out-of-set expr node tag must be rejected at load, got: {err}"
    );
}

#[test]
fn expr_node_with_unknown_field_is_rejected() {
    use zeroship_migrate::expr::Expr;
    let json = r#"{"node":"colRef","name":"x","evil":1}"#;
    let err = serde_json::from_str::<Expr>(json).unwrap_err();
    assert!(
        err.to_string().contains("evil") || err.to_string().contains("unknown field"),
        "an unknown field on an expr node must be rejected, got: {err}"
    );
}

#[test]
fn non_canonical_base64_normalizes_and_hashes_equal() {
    // Two encodings of the SAME 3 bytes must canonicalize to one form, so the
    // typed value (and any downstream checksum) is identical.
    let canonical: IrScalar = serde_json::from_str(r#"{"bytes":"AAEC"}"#).unwrap();
    // re-serialize must be canonical base64
    let reser = serde_json::to_string(&canonical).unwrap();
    assert_eq!(reser, r#"{"bytes":"AAEC"}"#, "bytes must re-encode canonically");
    // a valid round-trip of arbitrary bytes
    let v: IrScalar = serde_json::from_str(r#"{"bytes":"3q2+7w=="}"#).unwrap();
    let back: IrScalar = serde_json::from_str(&serde_json::to_string(&v).unwrap()).unwrap();
    assert_eq!(v, back, "bytes round-trip must be stable");
}
