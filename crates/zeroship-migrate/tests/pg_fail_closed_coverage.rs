use std::collections::{BTreeMap, BTreeSet};

use zeroship_migrate::model::expr::{Expr, PgExtractField, ScalarFn};
use zeroship_migrate::model::ir::{
    FuncLanguage, GrantTarget, IrScalar, IrValue, MigrationIr, Op, PartitionBounds, PolicyCmd,
    Privilege, SelectAst, TableRef, ViewQuery, CURRENT_IR_VERSION,
};
use zeroship_migrate::model::validate::{
    validate_expr, validate_ir_scoped, Dialect, TargetScope, CODE_UNSUPPORTED,
    CODE_VENDOR_OP_DENIED,
};
use zeroship_migrate::render::dml::assemble_backfill_clauses;
use zeroship_migrate::{IrAuthor, LiveSchema, PolicyProfile, SchemaScope, SqlDialect};

const EXPECTED_PG_ONLY_EXPR_NODES: &[&str] = &[
    "FnCall::CurrentSetting",
    "FnCall::CurrentUser",
    "PgColumnSize",
    "PgExtract",
    "PgInterval",
    "PgRegexMatch",
];

const EXPECTED_PG_VENDOR_OP_KINDS: &[&str] = &[
    "AlterRole",
    "AttachPartition",
    "CreateExtension",
    "CreateFunction",
    "CreatePolicy",
    "CreateRole",
    "CreateSchema",
    "CreateView::Materialized",
    "DropExtension",
    "DropFunction",
    "DropOwnedBy",
    "DropPolicy",
    "DropRole",
    "DropSchema",
    "DropView::Materialized",
    "Grant",
    "PgRaw",
    "Revoke",
    "SetRls",
];

fn lit_str(value: &str) -> Expr {
    Expr::lit(IrScalar::Str(value.to_string()))
}

fn pg_only_expr_kind(expr: &Expr) -> Option<&'static str> {
    match expr {
        Expr::ColRef { .. }
        | Expr::Literal { .. }
        | Expr::BinOp { .. }
        | Expr::UnaryOp { .. }
        | Expr::Case { .. }
        | Expr::FnSynth { .. }
        | Expr::Cast { .. }
        | Expr::Between { .. }
        | Expr::Like { .. }
        | Expr::DistinctFrom { .. }
        | Expr::Agg { .. }
        | Expr::InList { .. }
        | Expr::Extract { .. }
        | Expr::Dialectal { .. } => None,
        Expr::FnCall { r#fn, .. } => match r#fn {
            ScalarFn::Coalesce
            | ScalarFn::Nullif
            | ScalarFn::Lower
            | ScalarFn::Upper
            | ScalarFn::Trim
            | ScalarFn::Length
            | ScalarFn::Abs
            | ScalarFn::Mod
            | ScalarFn::Round
            | ScalarFn::Floor
            | ScalarFn::Ceil
            | ScalarFn::Substr
            | ScalarFn::Replace => None,
            ScalarFn::CurrentSetting => Some("FnCall::CurrentSetting"),
            ScalarFn::CurrentUser => Some("FnCall::CurrentUser"),
        },
        Expr::PgRegexMatch { .. } => Some("PgRegexMatch"),
        Expr::PgColumnSize { .. } => Some("PgColumnSize"),
        Expr::PgExtract { .. } => Some("PgExtract"),
        Expr::PgInterval { .. } => Some("PgInterval"),
    }
}

fn pg_only_expr_samples() -> Vec<Expr> {
    vec![
        Expr::FnCall {
            r#fn: ScalarFn::CurrentSetting,
            args: vec![lit_str("zeroship.tenant_app"), Expr::lit(IrScalar::Bool(true))],
        },
        Expr::FnCall {
            r#fn: ScalarFn::CurrentUser,
            args: vec![],
        },
        Expr::PgRegexMatch {
            expr: Box::new(Expr::col("name")),
            pattern: "^a".to_string(),
        },
        Expr::PgColumnSize {
            expr: Box::new(Expr::col("name")),
        },
        Expr::PgExtract {
            field: PgExtractField::Epoch,
            from: Box::new(Expr::col("ts")),
        },
        Expr::PgInterval {
            duration: zeroship_migrate::Duration {
                years: None,
                months: None,
                days: None,
                hours: None,
                minutes: Some(1),
                seconds: None,
            },
        },
    ]
}

fn scope_columns() -> Vec<String> {
    ["name", "ts"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

#[test]
fn pg_only_expr_nodes_render_on_pg_and_refuse_off_pg_at_validate() {
    let columns = scope_columns();
    let scope = TargetScope::new("t", &columns);
    let mut seen = BTreeSet::new();

    for expr in pg_only_expr_samples() {
        let kind = pg_only_expr_kind(&expr).expect("sample must be PG-only");
        assert!(seen.insert(kind), "{kind} sampled twice");

        validate_expr(&expr, Dialect::Postgres, &scope, 0, None)
            .unwrap_or_else(|err| panic!("{kind} must validate on Postgres: {err:?}"));
        let mut set = BTreeMap::new();
        set.insert("out".to_string(), IrValue::Expr(expr.clone()));
        let rendered = assemble_backfill_clauses(SqlDialect::Postgres, "t", &set, Some(&expr))
            .unwrap_or_else(|err| panic!("{kind} must render on Postgres: {err:?}"));
        assert!(!rendered.set_clause.trim().is_empty(), "{kind} PG render is empty");

        for dialect in [Dialect::Sqlite, Dialect::Mysql] {
            let err = validate_expr(&expr, dialect, &scope, 0, None)
                .expect_err("PG-only expression must refuse off Postgres");
            assert_eq!(
                err.code, CODE_UNSUPPORTED,
                "{kind} on {dialect:?} must fail closed as UNSUPPORTED, got {err:?}"
            );
        }
    }

    let expected: BTreeSet<&'static str> =
        EXPECTED_PG_ONLY_EXPR_NODES.iter().copied().collect();
    assert_eq!(seen, expected, "PG-only Expr coverage drifted");
}

fn ir_with(op: Op) -> MigrationIr {
    MigrationIr {
        ir_version: CURRENT_IR_VERSION,
        name: "pg_vendor_fail_closed".to_string(),
        owner_app: "app_test".to_string(),
        ops: vec![op],
        flags: Default::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    }
}

fn true_expr() -> Expr {
    Expr::lit(IrScalar::Bool(true))
}

fn structured_view_query() -> ViewQuery {
    ViewQuery::Structured {
        select: SelectAst {
            from: TableRef {
                name: "users".to_string(),
                schema: None,
                alias: None,
            },
            projection: vec![],
            joins: vec![],
            r#where: None,
            order_by: None,
            limit: None,
        },
    }
}

fn pg_vendor_op_kind(op: &Op) -> Option<&'static str> {
    match op {
        Op::CreateTable { .. }
        | Op::CreatePartition { .. }
        | Op::DetachPartition { .. }
        | Op::DropPartition { .. }
        | Op::SetTableOptions { .. }
        | Op::DropTable { .. }
        | Op::RenameTable { .. }
        | Op::AddColumn { .. }
        | Op::DropColumn { .. }
        | Op::CreateIndex { .. }
        | Op::Comment { .. }
        | Op::DropIndex { .. }
        | Op::SetColumnType { .. }
        | Op::SetColumnNotNull { .. }
        | Op::DropColumnNotNull { .. }
        | Op::SetColumnDefault { .. }
        | Op::DropColumnDefault { .. }
        | Op::RenameColumn { .. }
        | Op::AddConstraint { .. }
        | Op::ValidateConstraint { .. }
        | Op::DropConstraint { .. }
        | Op::Insert { .. }
        | Op::Update { .. }
        | Op::Delete { .. }
        | Op::Backfill { .. }
        | Op::CreateEnum { .. }
        | Op::DropEnum { .. }
        | Op::CreateDomain { .. }
        | Op::DropDomain { .. }
        | Op::CreateSequence { .. }
        | Op::AlterSequence { .. }
        | Op::DropSequence { .. }
        | Op::CreateTrigger { .. }
        | Op::DropTrigger { .. } => None,
        Op::CreateView {
            query,
            materialized,
            ..
        } => {
            if materialized.unwrap_or(false) {
                Some("CreateView::Materialized")
            } else if matches!(query, ViewQuery::Raw { .. }) {
                None
            } else {
                None
            }
        }
        Op::DropView { materialized, .. } => {
            if materialized.unwrap_or(false) {
                Some("DropView::Materialized")
            } else {
                None
            }
        }
        Op::CreateSchema { .. } => Some("CreateSchema"),
        Op::DropSchema { .. } => Some("DropSchema"),
        Op::CreateExtension { .. } => Some("CreateExtension"),
        Op::DropExtension { .. } => Some("DropExtension"),
        Op::CreateRole { .. } => Some("CreateRole"),
        Op::AlterRole { .. } => Some("AlterRole"),
        Op::DropRole { .. } => Some("DropRole"),
        Op::DropOwnedBy { .. } => Some("DropOwnedBy"),
        Op::Grant { .. } => Some("Grant"),
        Op::Revoke { .. } => Some("Revoke"),
        Op::AttachPartition { .. } => Some("AttachPartition"),
        Op::SetRls { .. } => Some("SetRls"),
        Op::CreatePolicy { .. } => Some("CreatePolicy"),
        Op::DropPolicy { .. } => Some("DropPolicy"),
        Op::CreateFunction { .. } => Some("CreateFunction"),
        Op::DropFunction { .. } => Some("DropFunction"),
        Op::PgRaw { .. } => Some("PgRaw"),
    }
}

fn pg_vendor_op_samples() -> Vec<Op> {
    vec![
        Op::CreateSchema {
            name: "app_extra".to_string(),
            if_not_exists: Some(true),
            authorization: None,
        },
        Op::DropSchema {
            name: "app_extra".to_string(),
            if_exists: Some(true),
            cascade: None,
        },
        Op::CreateExtension {
            name: "pgcrypto".to_string(),
            if_not_exists: Some(true),
            schema: None,
        },
        Op::DropExtension {
            name: "pgcrypto".to_string(),
            if_exists: Some(true),
        },
        Op::CreateRole {
            name: "app_reader".to_string(),
            login: None,
            password: None,
            bypass_rls: None,
            create_role: None,
            create_db: None,
            superuser: None,
            in_role: None,
            set_search_path: None,
            if_not_exists: None,
        },
        Op::AlterRole {
            name: "app_reader".to_string(),
            set_search_path: Some(vec!["app".to_string()]),
            reset_search_path: None,
        },
        Op::DropRole {
            name: "app_reader".to_string(),
            if_exists: Some(true),
        },
        Op::DropOwnedBy {
            roles: vec!["app_reader".to_string()],
        },
        Op::Grant {
            privileges: vec![Privilege::Select],
            on: GrantTarget::Table {
                names: vec!["users".to_string()],
                schema: None,
            },
            to: vec!["app_reader".to_string()],
            with_grant_option: None,
        },
        Op::Revoke {
            privileges: vec![Privilege::Select],
            on: GrantTarget::Table {
                names: vec!["users".to_string()],
                schema: None,
            },
            from: vec!["app_reader".to_string()],
        },
        Op::AttachPartition {
            parent: "events".to_string(),
            name: "events_2026_11".to_string(),
            bound: PartitionBounds::Default,
            schema: None,
        },
        Op::SetRls {
            table: "users".to_string(),
            schema: None,
            enabled: Some(true),
            forced: Some(true),
        },
        Op::CreatePolicy {
            name: "tenant_isolation".to_string(),
            table: "users".to_string(),
            schema: None,
            for_cmd: PolicyCmd::All,
            to: None,
            using: true_expr(),
            with_check: None,
        },
        Op::DropPolicy {
            name: "tenant_isolation".to_string(),
            table: "users".to_string(),
            schema: None,
            if_exists: Some(true),
        },
        Op::CreateView {
            name: "users_mv".to_string(),
            schema: None,
            columns: None,
            query: structured_view_query(),
            replace: None,
            materialized: Some(true),
        },
        Op::DropView {
            name: "users_mv".to_string(),
            schema: None,
            existence_guard: None,
            materialized: Some(true),
        },
        Op::CreateFunction {
            name: "touch_user".to_string(),
            schema: None,
            args: None,
            returns: "void".to_string(),
            language: FuncLanguage::Sql,
            replace: None,
            volatility: None,
            body: "SELECT 1".to_string(),
        },
        Op::DropFunction {
            name: "touch_user".to_string(),
            schema: None,
            arg_types: None,
            if_exists: Some(true),
        },
        Op::PgRaw {
            sql: "SELECT 1".to_string(),
            reason: "coverage gate sample".to_string(),
        },
    ]
}

#[test]
fn pg_vendor_ops_render_on_pg_and_refuse_off_pg_at_validate() {
    let platform_scope = SchemaScope::Allowlist(vec!["app".to_string(), "public".to_string()]);
    let confined_scope = SchemaScope::Single("app".to_string());
    let mut seen = BTreeSet::new();

    for op in pg_vendor_op_samples() {
        let kind = pg_vendor_op_kind(&op).expect("sample must be PG vendor");
        assert!(seen.insert(kind), "{kind} sampled twice");

        let ir = ir_with(op.clone());
        validate_ir_scoped(
            &ir,
            Dialect::Postgres,
            &[],
            Some(&platform_scope),
            &PolicyProfile::platform(),
        )
        .unwrap_or_else(|err| panic!("{kind} must validate on PG under platform scope: {err:?}"));

        let migrations = IrAuthor::new("app", "app_test", SqlDialect::Postgres)
            .with_schema_scope(platform_scope.clone())
            .lower(&ir, &LiveSchema::default())
            .unwrap_or_else(|err| panic!("{kind} must render on PG: {err:?}"));
        assert!(
            migrations.iter().any(|migration| !migration.up.trim().is_empty()),
            "{kind} PG lower produced no SQL"
        );

        let confined_err = validate_ir_scoped(
            &ir,
            Dialect::Postgres,
            &[],
            Some(&confined_scope),
            &PolicyProfile::confined(),
        )
        .expect_err("confined PG scope must refuse vendor ops by capability");
        assert_eq!(
            confined_err.code, CODE_VENDOR_OP_DENIED,
            "{kind} under confined PG must fail as VENDOR_OP_DENIED, got {confined_err:?}"
        );

        for dialect in [Dialect::Sqlite, Dialect::Mysql] {
            let err = validate_ir_scoped(
                &ir,
                dialect,
                &[],
                Some(&platform_scope),
                &PolicyProfile::platform(),
            )
            .expect_err("PG vendor op must refuse off Postgres");
            assert_eq!(
                err.code, CODE_UNSUPPORTED,
                "{kind} on {dialect:?} must fail closed as UNSUPPORTED, got {err:?}"
            );
        }
    }

    let expected: BTreeSet<&'static str> =
        EXPECTED_PG_VENDOR_OP_KINDS.iter().copied().collect();
    assert_eq!(seen, expected, "PG vendor op fail-closed coverage drifted");
}

#[test]
fn raw_view_body_is_not_part_of_pg_only_vendor_sweep() {
    let raw_view = Op::CreateView {
        name: "raw_users".to_string(),
        schema: None,
        columns: None,
        query: ViewQuery::Raw {
            sql: "SELECT id FROM users".to_string(),
        },
        replace: None,
        materialized: None,
    };
    assert_eq!(
        pg_vendor_op_kind(&raw_view),
        None,
        "RawViewBody is capability-gated, but it is not a PG-only @zeroship/migrate/pg op"
    );
}
