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
//! - **property A (no raw SQL)** — `createIndex.where` + `setColumnType.using`
//!   are closed `Expr` AST nodes (not raw SQL strings), and `createIndex.using`
//!   / `IrIndex.using` are the closed `IndexMethod` enum — a raw-SQL string or an
//!   out-of-set method is rejected at load (code-critic HIGH + MED).

use zeroship_migrate::model::ir::{CanonicalOpList, IndexElement, IrScalar, MigrationIr, Op};
use zeroship_migrate::EXPR_INVALID_NUMERIC;

// ----------------------------------------------------------------------------
// Wire casing — op-region fields are camelCase
// ----------------------------------------------------------------------------

#[test]
fn drop_table_fields_are_camel_case() {
    // **PR10** — the legacy native `if_exists`/`ifExists` boolean field is GONE; the
    // existence guard is the uniform `existenceGuard` enum (engine-synthesized via a
    // catalog probe, NOT native `IF EXISTS`). The intentional wire break.
    use zeroship_migrate::model::ir::ExistenceGuard;
    let op = Op::DropTable {
        table: "t".into(),
        cascade: Some(false),
        schema: None,
        existence_guard: Some(ExistenceGuard::IfExists),
    };
    let v = serde_json::to_value(&op).unwrap();
    assert_eq!(v["op"], "dropTable");
    assert_eq!(v["existenceGuard"], "ifExists", "the guard serializes camelCased: {v}");
    assert!(v.get("ifExists").is_none(), "the old native ifExists bool must NOT appear: {v}");
    assert!(v.get("if_exists").is_none(), "snake_case if_exists must NOT appear: {v}");
    // round-trips from the camelCase wire form
    let back: Op =
        serde_json::from_str(r#"{"op":"dropTable","table":"t","existenceGuard":"ifExists"}"#)
            .unwrap();
    match back {
        Op::DropTable { existence_guard, .. } => {
            assert_eq!(existence_guard, Some(ExistenceGuard::IfExists));
        }
        _ => panic!("expected DropTable"),
    }
    // The removed native field is rejected by deny_unknown_fields.
    assert!(
        serde_json::from_str::<Op>(r#"{"op":"dropTable","table":"t","if_exists":true}"#).is_err(),
        "the removed if_exists field must be rejected"
    );

    let view: Op =
        serde_json::from_str(r#"{"op":"dropView","name":"v","existenceGuard":"ifExists"}"#)
            .unwrap();
    match view {
        Op::DropView { existence_guard, .. } => {
            assert_eq!(existence_guard, Some(ExistenceGuard::IfExists));
        }
        _ => panic!("expected DropView"),
    }
    assert!(
        serde_json::from_str::<Op>(r#"{"op":"dropView","name":"v","if_exists":true}"#).is_err(),
        "DropView must reject the removed if_exists field"
    );
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
fn ir_column_facet_fields_are_camel_case() {
    // **Migration-first P2a (MED-2)** — the two declared-only IrColumn facets are the
    // FIRST multi-word op-region fields, so they are the first to actually exercise
    // the camelCase nested-field convention every sibling silently obeyed (each was
    // single-word). They MUST serialize camelCase (`idPrefix` / `vectorMetric`),
    // matching `FieldDescriptor`'s `#[serde(rename = …)]` and the design §4 — one
    // spelling across IR↔descriptor. And because `IrColumn` is `deny_unknown_fields`,
    // the snake_case spelling must NOT deserialize (the inverse of the bug: pre-fix
    // a camelCase `.ir.json` following the codebase convention was REJECTED).
    use zeroship_migrate::model::ir::{ColType, IrColumn, VectorMetric};

    let col = IrColumn {
        name: "id".into(),
        ty: ColType::Uuid,
        nullable: Some(false),
        default: None,
        unique: None,
        id_prefix: Some("post".into()),
        vector_metric: Some(VectorMetric::Cosine),
        mask: None,
        generated: None,
        identity: None,
    };
    let v = serde_json::to_value(&col).unwrap();
    assert_eq!(v["idPrefix"], "post", "idPrefix is camelCase on the wire: {v}");
    assert_eq!(v["vectorMetric"], "cosine", "vectorMetric is camelCase on the wire: {v}");
    assert!(v.get("id_prefix").is_none(), "snake_case id_prefix must NOT appear: {v}");
    assert!(v.get("vector_metric").is_none(), "snake_case vector_metric must NOT appear: {v}");

    // deny_unknown_fields: the camelCase wire form round-trips; snake_case is rejected.
    let camel = r#"{"name":"id","type":"uuid","idPrefix":"post"}"#;
    let back: IrColumn = serde_json::from_str(camel).expect("camelCase idPrefix round-trips");
    assert_eq!(back.id_prefix.as_deref(), Some("post"));
    let snake = r#"{"name":"id","type":"uuid","id_prefix":"post"}"#;
    assert!(
        serde_json::from_str::<IrColumn>(snake).is_err(),
        "snake_case id_prefix must be rejected by deny_unknown_fields (camelCase is the contract)"
    );
}

#[test]
fn create_table_primary_key_round_trips_and_schema_carries_field() {
    use zeroship_migrate::model::ir::{ColType, IrColumn};

    let op = Op::CreateTable {
        name: "membership".into(),
        columns: vec![
            IrColumn {
                name: "account_id".into(),
                ty: ColType::Text,
                nullable: Some(false),
                default: None,
                unique: None,
                id_prefix: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            },
            IrColumn {
                name: "team".into(),
                ty: ColType::Text,
                nullable: Some(false),
                default: None,
                unique: None,
                id_prefix: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            },
        ],
        primary_key: Some(vec!["account_id".into(), "team".into()]),
        constraints: vec![],
        indexes: vec![],

    partition_by: None,

    runtime_options: None,
        schema: None,
        existence_guard: None,
    };
    let v = serde_json::to_value(&op).unwrap();
    assert_eq!(
        v["primaryKey"],
        serde_json::json!(["account_id", "team"]),
        "composite primaryKey serializes at the createTable top level: {v}"
    );
    let back: Op = serde_json::from_value(v).unwrap();
    match back {
        Op::CreateTable { primary_key, .. } => {
            assert_eq!(primary_key, Some(vec!["account_id".into(), "team".into()]));
        }
        _ => panic!("expected CreateTable"),
    }

    let no_pk = Op::CreateTable {
        name: "audit".into(),
        columns: vec![],
        primary_key: None,
        constraints: vec![],
        indexes: vec![],

    partition_by: None,

    runtime_options: None,
        schema: None,
        existence_guard: None,
    };
    let no_pk_v = serde_json::to_value(&no_pk).unwrap();
    assert_eq!(
        no_pk_v.get("primaryKey"),
        Some(&serde_json::Value::Null),
        "None serializes explicitly as primaryKey:null: {no_pk_v}"
    );

    let schema = schemars::schema_for!(zeroship_migrate::MigrationIr);
    let schema_value = serde_json::to_value(&schema).expect("schema -> value");
    let create_table = schema_value["$defs"]["Op"]["oneOf"]
        .as_array()
        .expect("Op oneOf")
        .iter()
        .find(|branch| branch["properties"]["op"]["const"] == "createTable")
        .expect("createTable branch");
    assert!(
        create_table["properties"].get("primaryKey").is_some(),
        "createTable schema branch must carry primaryKey: {create_table}"
    );
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
fn sequence_signed_value_above_2pow53_is_rejected() {
    let json = r#"{"op":"createSequence","name":"s","increment":9007199254740992}"#;
    let err = serde_json::from_str::<Op>(json).unwrap_err();
    assert!(
        err.to_string().contains(EXPR_INVALID_NUMERIC),
        "sequence increment >= 2^53 must be rejected, got: {err}"
    );
}

#[test]
fn sequence_signed_value_below_negative_2pow53_is_rejected() {
    let json = r#"{"op":"createSequence","name":"s","start":-9007199254740992}"#;
    let err = serde_json::from_str::<Op>(json).unwrap_err();
    assert!(
        err.to_string().contains(EXPR_INVALID_NUMERIC),
        "sequence start <= -2^53 must be rejected, got: {err}"
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
fn sequence_signed_ints_below_2pow53_accepted() {
    let json = r#"{"op":"createSequence","name":"s","increment":-9007199254740991,
        "start":9007199254740991}"#;
    let op: Op = serde_json::from_str(json).unwrap();
    match op {
        Op::CreateSequence { increment, start, .. } => {
            assert_eq!(increment.expect("increment").get(), -9_007_199_254_740_991);
            assert_eq!(start.expect("start").get(), 9_007_199_254_740_991);
        }
        _ => panic!("expected CreateSequence"),
    }
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

/// The `SafeU64` JSON Schema MUST carry the same `< 2^53` upper bound the Rust
/// deserializer enforces (code-critic MED): `batchSize`/`limit`/`timeout_ms` are
/// exactly the JS-safe-integer boundary `SafeU64` exists to police, and the
/// schemars schema is the single source a JS best-effort hint validates against.
/// Without `maximum`, a schema-driven JS validator would ACCEPT a `2^53` count
/// the Rust loader REJECTS — a schema/loader divergence on the determinism
/// boundary. RED before `SafeU64` gets a hand-written `JsonSchema`.
#[test]
fn safe_u64_schema_carries_the_2pow53_upper_bound() {
    let schema = schemars::schema_for!(MigrationIr);
    let value: serde_json::Value = serde_json::to_value(&schema).expect("schema -> value");
    let def = value
        .get("$defs")
        .and_then(|d| d.get("SafeU64"))
        .expect("schema must define $defs/SafeU64");
    assert_eq!(
        def.get("minimum").and_then(serde_json::Value::as_i64),
        Some(0),
        "SafeU64 schema must pin minimum:0"
    );
    assert_eq!(
        def.get("maximum").and_then(serde_json::Value::as_i64),
        Some(9_007_199_254_740_991),
        "SafeU64 schema must pin maximum:2^53-1 to match the Rust deserializer"
    );
}

#[test]
fn safe_i64_schema_carries_the_safe_integer_bounds() {
    let schema = schemars::schema_for!(MigrationIr);
    let value: serde_json::Value = serde_json::to_value(&schema).expect("schema -> value");
    let def = value
        .get("$defs")
        .and_then(|d| d.get("SafeI64"))
        .expect("schema must define $defs/SafeI64");
    assert_eq!(
        def.get("minimum").and_then(serde_json::Value::as_i64),
        Some(-9_007_199_254_740_991),
        "SafeI64 schema must pin minimum:-(2^53-1)"
    );
    assert_eq!(
        def.get("maximum").and_then(serde_json::Value::as_i64),
        Some(9_007_199_254_740_991),
        "SafeI64 schema must pin maximum:2^53-1"
    );
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
    use zeroship_migrate::model::expr::{BinaryOp, Expr};
    use zeroship_migrate::model::ir::IrScalar;
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
    use zeroship_migrate::model::expr::Expr;
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
    use zeroship_migrate::model::expr::Expr;
    let json = r#"{"node":"colRef","name":"x","evil":1}"#;
    let err = serde_json::from_str::<Expr>(json).unwrap_err();
    assert!(
        err.to_string().contains("evil") || err.to_string().contains("unknown field"),
        "an unknown field on an expr node must be rejected, got: {err}"
    );
}

// ----------------------------------------------------------------------------
// property A (no raw SQL) — createIndex.where / .using are NOT raw strings
// (code-critic HIGH + MED: the partial-index predicate must be a closed Expr
// AST, and the index method must be a closed enum — never a raw SQL string).
// ----------------------------------------------------------------------------

#[test]
fn create_index_where_is_a_closed_expr_not_raw_sql() {
    use zeroship_migrate::model::expr::{BinaryOp, Expr};
    // A `createIndex` with a partial-index predicate authored as the closed AST
    // the JS `createIndex({where:(c)=>Expr})` emits — it MUST round-trip into a
    // typed `Expr`, exactly like IrIndex.where inside createTable.
    let json = r#"{"op":"createIndex","table":"t","columns":[{"kind":"column","name":"a"}],
        "where":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"a"},
                 "rhs":{"node":"literal","value":0}}}"#;
    let op: Op = serde_json::from_str(json).unwrap();
    match op {
        Op::CreateIndex { r#where, .. } => {
            assert_eq!(
                r#where,
                Some(Expr::BinOp {
                    op: BinaryOp::Gt,
                    lhs: Box::new(Expr::col("a")),
                    rhs: Box::new(Expr::lit(IrScalar::Int(0))),
                }),
                "createIndex.where must deserialize into a closed Expr AST node"
            );
        }
        _ => panic!("expected CreateIndex"),
    }
}

#[test]
fn create_index_where_rejects_raw_sql_string() {
    // A raw SQL string in the partial-index predicate slot is the exact
    // injection surface property A exists to eliminate — it must NOT deserialize
    // (a String is not a valid Expr node object).
    let json = r#"{"op":"createIndex","table":"t","columns":[{"kind":"column","name":"a"}],
        "where":"a > 0 OR 1=1"}"#;
    let err = serde_json::from_str::<Op>(json).unwrap_err();
    assert!(
        err.to_string().contains("expected")
            || err.to_string().contains("invalid type")
            || err.to_string().contains("node"),
        "a raw-SQL string where must be rejected (not a closed Expr), got: {err}"
    );
}

#[test]
fn create_index_using_is_a_closed_method_enum() {
    // The index method is a closed union ("btree"|"gin"|"gist"|"ivfflat"|"hnsw"
    // |"fts5", design line 648) — an arbitrary/injection-shaped string must NOT
    // deserialize, and a valid member round-trips.
    use zeroship_migrate::model::ir::IndexMethod;
    let ok = r#"{"op":"createIndex","table":"t","columns":[{"kind":"column","name":"a"}],"using":"gin"}"#;
    let op: Op = serde_json::from_str(ok).unwrap();
    match op {
        Op::CreateIndex { using, .. } => assert_eq!(using, Some(IndexMethod::Gin)),
        _ => panic!("expected CreateIndex"),
    }
    let bad = r#"{"op":"createIndex","table":"t","columns":[{"kind":"column","name":"a"}],
        "using":"btree; DROP TABLE users"}"#;
    let err = serde_json::from_str::<Op>(bad).unwrap_err();
    assert!(
        err.to_string().contains("unknown variant") || err.to_string().contains("btree"),
        "an out-of-set index method must be rejected at load, got: {err}"
    );
}

#[test]
fn create_index_element_rejects_unknown_fields() {
    let json = r#"{"op":"createIndex","table":"t",
        "columns":[{"kind":"column","name":"a","rawSql":"lower(a)"}]}"#;
    let err = serde_json::from_str::<Op>(json).unwrap_err();
    assert!(
        err.to_string().contains("rawSql") || err.to_string().contains("unknown field"),
        "an unknown field on IndexElement must be rejected, got: {err}"
    );
}

#[test]
fn comment_target_rejects_unknown_fields() {
    let json = r#"{"op":"comment",
        "target":{"kind":"table","name":"users","rawSql":"pg_class"},
        "comment":"Users"}"#;
    let err = serde_json::from_str::<Op>(json).unwrap_err();
    assert!(
        err.to_string().contains("rawSql") || err.to_string().contains("unknown field"),
        "an unknown field on CommentTarget must be rejected, got: {err}"
    );
}

#[test]
fn create_table_index_using_is_a_closed_method_enum() {
    // The same closed-enum guard applies to IrIndex.using inside createTable.
    use zeroship_migrate::model::ir::IndexMethod;
    let ok = r#"{"op":"createTable","name":"t","columns":[
        {"name":"a","type":"int"}],
        "indexes":[{"columns":[{"kind":"column","name":"a"}],"using":"hnsw"}]}"#;
    let op: Op = serde_json::from_str(ok).unwrap();
    match op {
        Op::CreateTable { indexes, .. } => {
            assert_eq!(indexes[0].using, Some(IndexMethod::Hnsw));
        }
        _ => panic!("expected CreateTable"),
    }
    let bad = r#"{"op":"createTable","name":"t","columns":[
        {"name":"a","type":"int"}],
        "indexes":[{"columns":[{"kind":"column","name":"a"}],"using":"evilmethod"}]}"#;
    let err = serde_json::from_str::<Op>(bad).unwrap_err();
    assert!(
        err.to_string().contains("unknown variant"),
        "an out-of-set IrIndex.using must be rejected at load, got: {err}"
    );
}

#[test]
fn alter_column_type_using_is_a_closed_expr_not_raw_sql() {
    use zeroship_migrate::model::expr::{CastTarget, Expr};
    // The USING cast expression must be the closed AST (a Cast node here), never
    // raw SQL — property A is binding everywhere a transform/predicate appears.
    let json = r#"{"op":"setColumnType","table":"t","column":"a","toType":"int",
        "using":{"node":"cast","operand":{"node":"colRef","name":"a"},
                 "target":"integer"}}"#;
    let op: Op = serde_json::from_str(json).unwrap();
    match op {
        Op::SetColumnType { using, .. } => {
            assert_eq!(
                using,
                Some(Expr::Cast {
                    operand: Box::new(Expr::col("a")),
                    target: CastTarget::Integer,
                }),
                "setColumnType.using must deserialize into a closed Expr AST node"
            );
        }
        _ => panic!("expected SetColumnType"),
    }
    // A raw SQL cast string must NOT deserialize.
    let bad = r#"{"op":"setColumnType","table":"t","column":"a","toType":"int",
        "using":"a::int"}"#;
    let err = serde_json::from_str::<Op>(bad).unwrap_err();
    assert!(
        err.to_string().contains("expected")
            || err.to_string().contains("invalid type")
            || err.to_string().contains("node"),
        "a raw-SQL string using must be rejected (not a closed Expr), got: {err}"
    );
}

#[test]
fn bytes_decode_then_canonical_reencode_is_stable() {
    // The `{"bytes":…}` carrier stores DECODED bytes and re-encodes with the
    // canonical STANDARD (padded) alphabet, so the typed value (and any
    // downstream checksum) is determined by the payload, not the source
    // spelling. NB: the decoder is STRICT (`BASE64_STANDARD.decode`) — a
    // non-canonical encoding is REJECTED at load rather than normalized (a
    // defensible, deterministic choice; see the code-critic LOW finding that
    // this test's old name over-promised "normalizes non-canonical").
    let canonical: IrScalar = serde_json::from_str(r#"{"bytes":"AAEC"}"#).unwrap();
    // re-serialize must be canonical base64
    let reser = serde_json::to_string(&canonical).unwrap();
    assert_eq!(reser, r#"{"bytes":"AAEC"}"#, "bytes must re-encode canonically");
    // a valid round-trip of arbitrary bytes
    let v: IrScalar = serde_json::from_str(r#"{"bytes":"3q2+7w=="}"#).unwrap();
    let back: IrScalar = serde_json::from_str(&serde_json::to_string(&v).unwrap()).unwrap();
    assert_eq!(v, back, "bytes round-trip must be stable");

    // Strict-decode contract: a NON-canonical encoding (unpadded / wrong
    // padding) is rejected, not silently normalized.
    assert!(
        serde_json::from_str::<IrScalar>(r#"{"bytes":"AAE"}"#).is_err(),
        "non-canonical (mis-padded) base64 must be rejected by the strict decoder"
    );
}

// ----------------------------------------------------------------------------
// Absent-optional canonicalization (code-critic HIGH): an unset Option field is
// OMITTED on the wire, never emitted as `"field":null`. This is the
// cross-impl-determinism invariant: an idiomatic JS builder drops `undefined`
// keys (`JSON.stringify` omits them), so a Rust serialization that emitted
// explicit nulls would fold a DIFFERENT byte image into `Checksum::of_ir` than
// the JS side for the SAME logical migration — breaking the single-artifact /
// single-checksum invariant (items 2 & 10). These tests are RED before the
// `skip_serializing_if = "Option::is_none"` fix and GREEN after.
// ----------------------------------------------------------------------------

#[test]
fn add_column_id_prefix_is_create_only_but_metric_and_mask_are_carried() {
    // **#173** — `vectorMetric` + `mask` are now CARRIED on `Op::AddColumn` (a vector /
    // masked ADD COLUMN is meaningful), so a wire `addColumn` declaring them DESERIALIZES
    // cleanly. `idPrefix` STAYS create-only: an added column is never the system PK, so
    // `Op::AddColumn` deliberately has NO `idPrefix` slot — a hand-crafted `.ir.json`
    // carrying it is rejected fail-closed by `deny_unknown_fields` at deserialize.

    // idPrefix on addColumn: STILL rejected (no slot — create-only).
    let id_err = serde_json::from_str::<Op>(
        r#"{"op":"addColumn","table":"t","column":"x","type":"uuid","idPrefix":"post"}"#,
    )
    .unwrap_err();
    assert!(
        id_err.to_string().contains("unknown field"),
        "an addColumn carrying idPrefix must be rejected by deny_unknown_fields (create-only \
         facet, no slot), got: {id_err}"
    );

    // vectorMetric on addColumn: now ACCEPTED (the #173 slot) and round-trips.
    let metric_op: Op = serde_json::from_str(
        r#"{"op":"addColumn","table":"t","column":"x","type":{"vector":{"vector":8}},"vectorMetric":"cosine"}"#,
    )
    .expect("addColumn now carries vectorMetric (#173)");
    match &metric_op {
        Op::AddColumn { vector_metric, .. } => assert_eq!(
            *vector_metric,
            Some(zeroship_migrate::model::ir::VectorMetric::Cosine),
            "the carried vector metric deserializes onto the op"
        ),
        other => panic!("expected AddColumn, got {other:?}"),
    }

    // mask on addColumn: now ACCEPTED (the #174 slot) and round-trips.
    let mask_op: Op = serde_json::from_str(
        r#"{"op":"addColumn","table":"t","column":"ssn","type":"text","mask":{"kind":"last4","classification":"spi"}}"#,
    )
    .expect("addColumn now carries mask (#174)");
    match &mask_op {
        Op::AddColumn { mask, .. } => {
            let m = mask.expect("the carried mask deserializes onto the op");
            assert_eq!(m.kind, zeroship_migrate::model::ir::IrMaskKind::Last4);
            assert_eq!(m.classification, zeroship_migrate::model::ir::IrClassification::Spi);
        }
        other => panic!("expected AddColumn, got {other:?}"),
    }
}

#[test]
fn add_column_omits_absent_optionals() {
    // nullable:None, default:None — both MUST be absent from the JSON object,
    // exactly as an idiomatic JS `op.addColumn("t","x","int")` (no opts) emits.
    let op = Op::AddColumn {
        table: "t".into(),
        column: "x".into(),
        ty: zeroship_migrate::model::ir::ColType::Int,
        nullable: None,
        default: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
        schema: None,
        existence_guard: None,
    };
    let v = serde_json::to_value(&op).unwrap();
    let obj = v.as_object().unwrap();
    assert!(
        !obj.contains_key("nullable"),
        "absent `nullable` must be OMITTED, not serialized as null: {v}"
    );
    assert!(
        !obj.contains_key("default"),
        "absent `default` must be OMITTED, not serialized as null: {v}"
    );
    // The serialized object is EXACTLY the four present keys.
    assert_eq!(obj.len(), 4, "addColumn with no opts must emit exactly op/table/column/type: {v}");
    // …and it still round-trips.
    let back: Op = serde_json::from_value(v).unwrap();
    assert_eq!(op, back);
}

#[test]
fn create_index_omits_all_absent_optionals() {
    let op = Op::CreateIndex {
        table: "users".into(),
        columns: vec![IndexElement::Column {
            name: "email".into(),
            order: None,
        }],
        name: None,
        unique: None,
        using: None,
        r#where: None,
        include: Vec::new(),
        with: None,
        only: None,
        concurrently: None,
        schema: None,
        existence_guard: None,
    };
    let v = serde_json::to_value(&op).unwrap();
    let obj = v.as_object().unwrap();
    for absent in ["name", "unique", "using", "where", "include", "with", "only", "concurrently"] {
        assert!(
            !obj.contains_key(absent),
            "absent `{absent}` must be OMITTED, not null: {v}"
        );
    }
    assert_eq!(obj.len(), 3, "only op/table/columns present: {v}");
}

#[test]
fn nested_ir_column_index_constraint_omit_absent_optionals() {
    use zeroship_migrate::model::ir::{ColType, IrColumn, IrConstraint, IrConstraintKind, IrIndex};
    // IrColumn nullable/default/unique absent.
    let col = IrColumn {
        name: "id".into(),
        ty: ColType::Uuid,
        nullable: None,
        default: None,
        unique: None, id_prefix: None, vector_metric: None, mask: None, generated: None, identity: None };
    let cv = serde_json::to_value(&col).unwrap();
    let cobj = cv.as_object().unwrap();
    for absent in ["nullable", "default", "unique"] {
        assert!(!cobj.contains_key(absent), "IrColumn absent `{absent}` must be omitted: {cv}");
    }
    // IrIndex name/unique/using/where absent.
    let ix = IrIndex {
        name: None,
        columns: vec![IndexElement::Column {
            name: "id".into(),
            order: None,
        }],
        unique: None,
        using: None,
        r#where: None,
        include: Vec::new(),
        with: None,
        only: None,
    };
    let iv = serde_json::to_value(&ix).unwrap();
    let iobj = iv.as_object().unwrap();
    for absent in ["name", "unique", "using", "where", "include", "with", "only"] {
        assert!(!iobj.contains_key(absent), "IrIndex absent `{absent}` must be omitted: {iv}");
    }
    // IrConstraint.name absent.
    let con = IrConstraint {
        name: None,
        kind: IrConstraintKind::Pk { columns: vec!["id".into()] },
    };
    let conv = serde_json::to_value(&con).unwrap();
    assert!(
        !conv.as_object().unwrap().contains_key("name"),
        "IrConstraint absent `name` must be omitted: {conv}"
    );
}

#[test]
fn partition_ops_round_trip_and_absent_fields_stay_omitted() {
    use zeroship_migrate::model::ir::{
        ColType, IrColumn, PartitionBoundValue, PartitionBounds, PartitionSpec, SafeI64,
    };

    let parent = Op::CreateTable {
        name: "events".into(),
        columns: vec![IrColumn {
            name: "created_at".into(),
            ty: ColType::Timestamp,
            nullable: None,
            default: None,
            unique: None,
            id_prefix: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        }],
        primary_key: None,
        constraints: vec![],
        indexes: vec![],
        partition_by: Some(PartitionSpec::Range {
            columns: vec!["created_at".into()],
        }),
        runtime_options: None,
        schema: None,
        existence_guard: None,
    };
    let value = serde_json::to_value(&parent).unwrap();
    assert!(value.get("partitionBy").is_some(), "partition key must be camelCase: {value}");
    assert!(value.get("partition_by").is_none(), "snake_case partition key must not serialize: {value}");
    let back: Op = serde_json::from_value(value).unwrap();
    assert_eq!(parent, back);

    let old_shape = Op::CreateTable {
        name: "plain".into(),
        columns: vec![],
        primary_key: None,
        constraints: vec![],
        indexes: vec![],
        partition_by: None,
        runtime_options: None,
        schema: None,
        existence_guard: None,
    };
    let old_json = serde_json::to_string(&old_shape).unwrap();
    assert!(
        !old_json.contains("partitionBy"),
        "absent partition_by must contribute zero bytes: {old_json}"
    );

    let create_partition = Op::CreatePartition {
        name: "events_2026_05".into(),
        of: "events".into(),
        bounds: PartitionBounds::Range {
            from: vec![
                PartitionBoundValue::String {
                    value: "2026-05-01T00:00:00Z".into(),
                },
                PartitionBoundValue::MinValue,
            ],
            to: vec![
                PartitionBoundValue::String {
                    value: "2026-06-01T00:00:00Z".into(),
                },
                PartitionBoundValue::Int {
                    value: SafeI64::new(42).unwrap(),
                },
            ],
        },
        schema: None,
        existence_guard: None,
    };
    let detach = Op::DetachPartition {
        parent: "events".into(),
        name: "events_2026_05".into(),
        schema: None,
        concurrently: Some(true),
    };
    let drop = Op::DropPartition {
        name: "events_2026_05".into(),
        schema: None,
        existence_guard: None,
        cascade: Some(true),
    };

    for op in [create_partition, detach, drop] {
        let json = serde_json::to_string(&op).unwrap();
        let back: Op = serde_json::from_str(&json).unwrap();
        assert_eq!(op, back, "new partition op must round-trip: {json}");
        assert!(
            !json.contains("rawSql"),
            "partition bounds stay closed literals, never raw SQL: {json}"
        );
    }
}

#[test]
fn expr_case_omits_absent_else() {
    use zeroship_migrate::Expr;
    use zeroship_migrate::model::expr::{CaseBranch, UnaryOp};
    let e = Expr::Case {
        branches: vec![CaseBranch {
            condition: Expr::UnaryOp {
                op: UnaryOp::IsNull,
                operand: Box::new(Expr::col("a")),
            },
            result: Expr::lit(IrScalar::Int(1)),
        }],
        r#else: None,
    };
    let v = serde_json::to_value(&e).unwrap();
    assert!(
        !v.as_object().unwrap().contains_key("else"),
        "absent Case `else` must be OMITTED, not null: {v}"
    );
}

#[test]
fn checksum_of_ir_matches_js_idiomatic_omitted_optionals() {
    // The portable single-checksum invariant across the JS and Rust front doors
    // (code-critic HIGH, items 2 & 10): a JS builder emits ops with UNSET
    // optionals OMITTED. The Rust `Op` (built with `None`) must hash IDENTICALLY
    // to the same op deserialized from that idiomatic JS-shaped JSON. Before the
    // fix, the Rust side folded `"nullable":null,"default":null` while the JS
    // doc omitted them — distinct bytes, distinct checksum. After the fix both
    // canonicalize to the same omitted-key image.
    use zeroship_migrate::{Checksum, MigrationFlags};
    use zeroship_migrate::model::ir::ColType;

    // Rust-built op (None optionals).
    let rust_op = Op::AddColumn {
        table: "t".into(),
        column: "x".into(),
        ty: ColType::Int,
        nullable: None,
        default: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
        schema: None,
        existence_guard: None,
    };
    // The idiomatic JS-emitted document: the optional keys are simply ABSENT.
    let js_json = r#"{"op":"addColumn","table":"t","column":"x","type":"int"}"#;
    let js_op: Op = serde_json::from_str(js_json).unwrap();
    assert_eq!(rust_op, js_op, "both decode to the same logical op");

    let flags = MigrationFlags::default();
    let rust_ck = Checksum::of_ir(&CanonicalOpList(&[rust_op]), &flags, "app", &[], &[], &[]);
    let js_ck = Checksum::of_ir(&CanonicalOpList(&[js_op]), &flags, "app", &[], &[], &[]);
    assert_eq!(
        rust_ck, js_ck,
        "Rust(None) and JS(omitted) must yield the SAME of_ir checksum"
    );

    // Belt-and-braces: an explicit-`null` document — the SHAPE the buggy Rust
    // serializer produced — must ALSO decode to the same logical op AND, once we
    // omit on re-serialize, hash to the same value (deny_unknown_fields permits
    // a present-but-null optional on input; canonicalization drops it).
    let null_json =
        r#"{"op":"addColumn","table":"t","column":"x","type":"int","nullable":null,"default":null}"#;
    let null_op: Op = serde_json::from_str(null_json).unwrap();
    let null_ck = Checksum::of_ir(&CanonicalOpList(&[null_op]), &flags, "app", &[], &[], &[]);
    assert_eq!(
        rust_ck, null_ck,
        "an explicit-null input must canonicalize to the SAME checksum as the omitted form"
    );
}
