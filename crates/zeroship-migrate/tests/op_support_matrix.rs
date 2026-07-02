use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use zeroship_migrate::model::capability::VendorCapability;
use zeroship_migrate::model::ir::{IrConstraintKind, Op, TriggerAction, ViewQuery};
use zeroship_migrate::model::support::{Dialect, RenderMode, SupportDecision, SupportTier};
use zeroship_migrate::model::validate::validate_ir_scoped;
use zeroship_migrate::{
    IrAuthor, IrFlagsOverride, LiveSchema, MigrationIr, PolicyProfile, SchemaScope, SqlDialect,
    CURRENT_IR_VERSION,
};

const DIALECTS: [Dialect; 3] = [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql];

const EXPECTED_OPS: &[&str] = &[
    "createTable",
    "dropTable",
    "renameTable",
    "addColumn",
    "dropColumn",
    "createIndex",
    "dropIndex",
    "setColumnType",
    "setColumnNotNull",
    "dropColumnNotNull",
    "setColumnDefault",
    "dropColumnDefault",
    "renameColumn",
    "setTableOptions",
    "addConstraint",
    "dropConstraint",
    "insert",
    "update",
    "delete",
    "backfill",
    "comment",
    "createView",
    "dropView",
    "createEnum",
    "dropEnum",
    "createDomain",
    "dropDomain",
    "createSequence",
    "alterSequence",
    "dropSequence",
    "createTrigger",
    "dropTrigger",
    "createSchema",
    "dropSchema",
    "createExtension",
    "dropExtension",
    "createRole",
    "alterRole",
    "dropRole",
    "dropOwnedBy",
    "grant",
    "revoke",
    "enableRls",
    "forceRls",
    "disableRls",
    "noForceRls",
    "createPolicy",
    "dropPolicy",
    "createFunction",
    "dropFunction",
    "pgRaw",
];

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/op_fixtures")
}

fn schema_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("op-ir.schema.json")
}

fn fixture_ops() -> Vec<Op> {
    let mut ops = Vec::new();
    for entry in std::fs::read_dir(fixtures_dir()).expect("read op_fixtures") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if !file_name.ends_with(".golden.json") {
            continue;
        }
        let ir: MigrationIr = serde_json::from_str(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display())),
        )
        .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
        ops.extend(ir.ops);
    }
    assert!(!ops.is_empty(), "op fixture corpus must not be empty");
    ops
}

fn op_tag(op: &Op) -> String {
    let value = serde_json::to_value(op).expect("op serializes");
    value
        .get("op")
        .and_then(|v| v.as_str())
        .expect("op tag is present")
        .to_string()
}

fn schema_op_tags() -> BTreeSet<String> {
    let schema: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(schema_path()).expect("read op-ir.schema.json"),
    )
    .expect("parse op-ir.schema.json");
    schema
        .get("$defs")
        .and_then(|d| d.get("Op"))
        .and_then(|o| o.get("oneOf"))
        .and_then(|o| o.as_array())
        .expect("Op oneOf branches")
        .iter()
        .filter_map(|branch| {
            branch
                .get("properties")
                .and_then(|p| p.get("op"))
                .and_then(|t| t.get("const"))
                .and_then(|c| c.as_str())
                .map(str::to_string)
        })
        .collect()
}

fn one_op_ir(op: Op) -> MigrationIr {
    MigrationIr {
        ir_version: CURRENT_IR_VERSION,
        name: "support_matrix".into(),
        owner_app: "app_corpus".into(),
        ops: vec![op],
        flags: IrFlagsOverride::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    }
}

fn sql_dialect(dialect: Dialect) -> SqlDialect {
    match dialect {
        Dialect::Postgres => SqlDialect::Postgres,
        Dialect::Sqlite => SqlDialect::Sqlite,
        Dialect::Mysql => SqlDialect::Mysql,
    }
}

fn validate_current(op: &Op, dialect: Dialect) -> bool {
    validate_ir_scoped(
        &one_op_ir(op.clone()),
        dialect,
        &[],
        Some(&SchemaScope::Unconfined),
        &PolicyProfile::platform(),
    )
    .is_ok()
}

fn lower_current(op: &Op, dialect: Dialect) -> bool {
    let default_schema = op.schema().unwrap_or("app");
    let author = IrAuthor::new(default_schema, "app_corpus", sql_dialect(dialect))
        .with_schema_scope(SchemaScope::Unconfined);
    author
        .lower(&one_op_ir(op.clone()), &LiveSchema::default())
        .is_ok()
}

fn assert_decision_well_formed(decision: SupportDecision, label: &str) {
    match decision {
        SupportDecision::Supported { render } => {
            assert!(
                matches!(render, RenderMode::Offline | RenderMode::LiveResolved),
                "{label}: supported cells must carry a concrete render mode"
            );
        }
        SupportDecision::Unsupported { code, reason } => {
            assert!(!code.is_empty(), "{label}: unsupported cells must carry a code");
            assert!(
                !reason.is_empty(),
                "{label}: unsupported cells must carry a reason"
            );
        }
    }
}

fn assert_current_cell_matches(op: &Op, dialect: Dialect) {
    let support = op.support();
    let decision = support.decision(dialect);
    let label = format!("{} {dialect:?}", op_tag(op));

    match decision {
        SupportDecision::Supported {
            render: RenderMode::Offline,
        } => {
            assert!(validate_current(op, dialect), "{label}: validate should accept");
            assert!(lower_current(op, dialect), "{label}: offline lower should succeed");
        }
        SupportDecision::Supported {
            render: RenderMode::LiveResolved,
        } => {
            assert!(validate_current(op, dialect), "{label}: validate should accept");
        }
        SupportDecision::Unsupported { .. } => {
            if validate_current(op, dialect) {
                assert!(
                    !lower_current(op, dialect),
                    "{label}: unsupported declaration must not validate and lower successfully"
                );
            }
        }
    }
}

fn by_tag(ops: Vec<Op>) -> BTreeMap<String, Vec<Op>> {
    let mut by_tag = BTreeMap::<String, Vec<Op>>::new();
    for op in ops {
        by_tag.entry(op_tag(&op)).or_default().push(op);
    }
    by_tag
}

fn first<'a>(ops: &'a BTreeMap<String, Vec<Op>>, tag: &str) -> &'a Op {
    ops.get(tag)
        .and_then(|items| items.first())
        .unwrap_or_else(|| panic!("fixture corpus must include {tag}"))
}

#[test]
fn support_declarations_cover_every_op_and_dialect() {
    let expected: BTreeSet<String> = EXPECTED_OPS.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(
        expected.len(),
        51,
        "matrix must mirror the closed 51-op v1 discriminant set"
    );
    assert_eq!(
        schema_op_tags(),
        expected,
        "support matrix expected set must stay in sync with op-ir.schema.json"
    );

    let ops = fixture_ops();
    let covered: BTreeSet<String> = ops.iter().map(op_tag).collect();
    let missing: Vec<&String> = expected.difference(&covered).collect();
    assert!(
        missing.is_empty(),
        "existing op fixture corpus must cover every Op discriminant; missing {missing:?}"
    );

    for op in &ops {
        let tag = op_tag(op);
        let support = op.support();
        assert_eq!(
            support.supported_dialects().is_empty(),
            DIALECTS
                .iter()
                .all(|dialect| !support.decision(*dialect).is_supported()),
            "{tag}: DialectSet must agree with per-dialect decisions"
        );

        for dialect in DIALECTS {
            assert_decision_well_formed(support.decision(dialect), &format!("{tag} {dialect:?}"));
        }

        for feature in support.features {
            for dialect in DIALECTS {
                assert_decision_well_formed(
                    feature.decision(dialect),
                    &format!("{tag} feature {:?} {dialect:?}", feature.feature),
                );
            }
        }

        match support.tier {
            SupportTier::Core => {
                let caps = op.vendor_capabilities();
                let raw_view_body_exception = matches!(
                    op,
                    Op::CreateView {
                        query: ViewQuery::Raw { .. },
                        materialized,
                        ..
                    } if !materialized.unwrap_or(false)
                ) && caps == vec![VendorCapability::RawViewBody];
                assert!(
                    caps.is_empty() || raw_view_body_exception,
                    "{tag}: core declarations must not hide vendor capabilities except the current raw view-body capability exception"
                );
            }
            SupportTier::Vendor(caps) => {
                assert!(!caps.is_empty(), "{tag}: vendor tier must name capabilities");
                assert_eq!(
                    caps,
                    op.vendor_capabilities().as_slice(),
                    "{tag}: support tier capabilities must match Op::vendor_capabilities"
                );
                assert!(
                    !support.decision(Dialect::Sqlite).is_supported()
                        && !support.decision(Dialect::Mysql).is_supported(),
                    "{tag}: vendor-tier ops must be non-PG unsupported"
                );
            }
        }
    }
}

#[test]
fn selected_cells_match_current_validate_and_lower_behavior() {
    let ops = by_tag(fixture_ops());

    for dialect in DIALECTS {
        assert_current_cell_matches(first(&ops, "comment"), dialect);
        assert_current_cell_matches(first(&ops, "createSequence"), dialect);
        assert_current_cell_matches(first(&ops, "alterSequence"), dialect);
        assert_current_cell_matches(first(&ops, "dropSequence"), dialect);
        assert_current_cell_matches(first(&ops, "createIndex"), dialect);
        assert_current_cell_matches(first(&ops, "renameColumn"), dialect);
        assert_current_cell_matches(first(&ops, "addConstraint"), dialect);
        assert_current_cell_matches(first(&ops, "dropConstraint"), dialect);
    }

    let insert_with_on_conflict = ops
        .get("insert")
        .and_then(|items| {
            items.iter().find(|op| {
                matches!(
                    op,
                    Op::Insert {
                        on_conflict: Some(_),
                        ..
                    }
                )
            })
        })
        .expect("fixture corpus must include insert with onConflict");
    for dialect in DIALECTS {
        assert_current_cell_matches(insert_with_on_conflict, dialect);
    }

    let trigger_execute_function = ops
        .get("createTrigger")
        .and_then(|items| {
            items.iter().find(|op| {
                matches!(
                    op,
                    Op::CreateTrigger {
                        action: TriggerAction::ExecuteFunction { .. },
                        ..
                    }
                )
            })
        })
        .expect("fixture corpus must include execute-function trigger");
    let trigger_body = ops
        .get("createTrigger")
        .and_then(|items| {
            items.iter().find(|op| {
                matches!(
                    op,
                    Op::CreateTrigger {
                        action: TriggerAction::Body { .. },
                        ..
                    }
                )
            })
        })
        .expect("fixture corpus must include body trigger");
    for dialect in DIALECTS {
        assert_current_cell_matches(trigger_execute_function, dialect);
        assert_current_cell_matches(trigger_body, dialect);
    }

    for dialect in DIALECTS {
        assert_current_cell_matches(first(&ops, "pgRaw"), dialect);
    }

    let exclusion_constraint = ops
        .get("addConstraint")
        .and_then(|items| {
            items.iter().find(|op| {
                matches!(
                    op,
                    Op::AddConstraint {
                        constraint,
                        ..
                    } if matches!(constraint.kind, IrConstraintKind::Exclusion { .. })
                )
            })
        })
        .expect("fixture corpus must include addConstraint exclusion");
    for dialect in DIALECTS {
        assert_current_cell_matches(exclusion_constraint, dialect);
    }
}
