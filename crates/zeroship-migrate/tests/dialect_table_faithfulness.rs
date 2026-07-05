//! The dialect-table CORPUS + SIDECAR-DRIFT anchor (repurposed in S0.2).
//!
//! S0.1 introduced the generated single-source dialect table
//! (`src/model/dialect_table.rs`, emitted from `dialect-support.toml` by
//! `sdks/migrate/scripts/gen-dialect-table.mjs`) and proved it mirrored the
//! engine's then-live `Op::support()` decisions. S0.2 made `Op::support` READ the
//! table (via [`Op::op_variant`]), so that agreement is now tautological. This
//! file is repurposed to the invariants that stay meaningful after the switch —
//! see the comment above the test for the full "what guards what".
//!
//! DESIGN — how it enumerates Op × dialect exhaustively:
//!   * A hand-authored REPRESENTATIVE corpus of `(kind, variant, Op)` triples.
//!     Payload-INDEPENDENT ops carry a single `"base"` variant; payload-DEPENDENT
//!     ops (whose support decision turns on a node/option) carry one triple per
//!     distinct support branch, each built to exhibit that branch.
//!   * SHARED VARIANT DERIVATION: each corpus op's `Op::op_variant()` (the ONE
//!     branch-selection `Op::support` keys the table lookup on) must equal its
//!     labelled variant — pinning the corpus and the engine against drift.
//!   * EXHAUSTIVENESS over op-KINDS: the corpus's kinds equal the schema's `Op`
//!     `oneOf` discriminants (the 54-op wire contract `op_support_matrix` pins).
//!   * EXHAUSTIVENESS over TABLE ROWS: the corpus's `(kind, variant)` set is a
//!     BIJECTION with the generated `DIALECT_TABLE`'s rows.
//!   * SIDECAR ⟷ TABLE: the generated `DIALECT_TABLE` matches the hand-authored,
//!     human-reviewed `dialect-support.toml` row-for-row (the same drift the TS
//!     `dialect-table-drift` test byte-checks; here checked Rust-side, node-free).
//!
//! The disposition vocabulary (S0.1/S0.2 have no `TransparentDegradable` source —
//! the current engine produces only Supported/Unsupported, so that disposition is
//! reserved for the redesign and appears in zero rows): portable / vendor (both
//! supported cells) and unsupported.

use std::collections::BTreeSet;
use std::path::PathBuf;

use zeroship_migrate::model::dialect_table::{Disposition, DIALECT_TABLE};
use zeroship_migrate::model::expr::Expr;
use zeroship_migrate::model::ir::{
    ColType, EmptyContainerKind, ExclusionMethod, ForEach, GrantTarget, IdentityCol, IndexElement,
    IndexMethod, IrColumn, IrConstraint, IrConstraintKind, IrDefault, IrIndex, Op, PartitionBounds,
    PartitionSpec, PolicyCmd, Privilege, RaiseLevel, SafeU64, SelectAst, SequenceRef, TableRef,
    TableRuntimeOptionsPatch, TriggerAction, TriggerEvent, TriggerStmt, TriggerTiming, ViewQuery,
};

/// The `op` wire tag (op-kind discriminant) of a concrete op, via its serde image.
fn op_tag(op: &Op) -> String {
    serde_json::to_value(op)
        .expect("op serializes")
        .get("op")
        .and_then(|v| v.as_str())
        .expect("op tag is present")
        .to_string()
}

fn sidecar_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("dialect-support.toml")
}

/// A parsed `[[row]]` of the hand-authored sidecar (kind, variant, per-dialect
/// disposition), read with the SAME restricted grammar the generator
/// (`gen-dialect-table.mjs`) enforces: blank lines, `#` comments, `[[row]]`
/// headers, and `key = "string"` assignments only.
#[derive(Debug, PartialEq, Eq)]
struct SidecarRow {
    kind: String,
    variant: String,
    pg: String,
    sqlite: String,
    mysql: String,
}

fn parse_sidecar() -> Vec<SidecarRow> {
    let text = std::fs::read_to_string(sidecar_path()).expect("read dialect-support.toml");
    let mut rows: Vec<std::collections::BTreeMap<String, String>> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[row]]" {
            rows.push(std::collections::BTreeMap::new());
            continue;
        }
        let (key, rest) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("dialect-support.toml:{}: not a key = value line: {raw:?}", i + 1));
        let key = key.trim().to_string();
        // strip an optional trailing `# comment`, then the surrounding quotes.
        let value_part = rest.split('#').next().unwrap_or("").trim();
        let value = value_part
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or_else(|| panic!("dialect-support.toml:{}: value is not a quoted string: {raw:?}", i + 1))
            .to_string();
        let cur = rows
            .last_mut()
            .unwrap_or_else(|| panic!("dialect-support.toml:{}: key before any [[row]] header", i + 1));
        cur.insert(key, value);
    }
    rows.into_iter()
        .map(|mut m| SidecarRow {
            kind: m.remove("kind").expect("row has kind"),
            variant: m.remove("variant").expect("row has variant"),
            pg: m.remove("pg").expect("row has pg"),
            sqlite: m.remove("sqlite").expect("row has sqlite"),
            mysql: m.remove("mysql").expect("row has mysql"),
        })
        .collect()
}

fn disposition_token(disposition: Disposition) -> &'static str {
    match disposition {
        Disposition::Portable => "portable",
        Disposition::TransparentDegradable => "transparentDegradable",
        Disposition::Vendor => "vendor",
        Disposition::Unsupported => "unsupported",
    }
}

fn schema_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("op-ir.schema.json")
}

/// The `Op` discriminant tokens the schema declares (the 54-op wire contract).
fn schema_op_tags() -> BTreeSet<String> {
    let schema: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(schema_path()).expect("read op-ir.schema.json"))
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

fn col_ref() -> Expr {
    Expr::ColRef { name: "x".into(), table: None }
}

/// A minimal `IrColumn` with the given type / default / identity facets. Only the
/// facets `support()` inspects (default, identity) matter here.
fn column(name: &str, ty: ColType, default: Option<IrDefault>, identity: Option<IdentityCol>) -> IrColumn {
    IrColumn {
        name: name.into(),
        ty,
        nullable: Some(false),
        default,
        unique: None,
        id_prefix: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity,
    }
}

fn plain_table(name: &str, columns: Vec<IrColumn>, primary_key: Option<Vec<String>>) -> Op {
    Op::CreateTable {
        name: name.into(),
        columns,
        primary_key,
        constraints: vec![],
        indexes: vec![],
        partition_by: None,
        runtime_options: None,
        schema: None,
        existence_guard: None,
    }
}

fn nextval_default() -> IrDefault {
    IrDefault::Nextval {
        sequence: SequenceRef {
            name: "s".into(),
            schema: Some("app".into()),
        },
    }
}

fn add_constraint(kind: IrConstraintKind) -> Op {
    Op::AddConstraint {
        table: "t".into(),
        constraint: IrConstraint { name: None, kind },
        schema: None,
        existence_guard: None,
    }
}

fn fk(columns: Vec<&str>, references_columns: Vec<&str>) -> IrConstraintKind {
    IrConstraintKind::Fk {
        columns: columns.into_iter().map(String::from).collect(),
        references_table: "other".into(),
        references_columns: references_columns.into_iter().map(String::from).collect(),
        on_delete: None,
        on_update: None,
        deferrable: None,
        initially_deferred: None,
        not_valid: None,
    }
}

fn create_index(elements: Vec<IndexElement>, using: Option<IndexMethod>, r#where: Option<Expr>, include: Vec<String>) -> Op {
    Op::CreateIndex {
        table: "t".into(),
        columns: elements,
        name: Some("t_idx".into()),
        unique: None,
        using,
        r#where,
        concurrently: None,
        include,
        with: None,
        only: None,
        schema: None,
        existence_guard: None,
    }
}

fn column_element(name: &str) -> IndexElement {
    IndexElement::Column {
        name: name.into(),
        order: None,
    }
}

fn structured_view(name: &str, materialized: Option<bool>, replace: Option<bool>) -> Op {
    Op::CreateView {
        name: name.into(),
        schema: None,
        columns: None,
        query: ViewQuery::Structured {
            select: SelectAst {
                from: TableRef {
                    name: "t".into(),
                    schema: None,
                    alias: None,
                },
                projection: vec![],
                joins: vec![],
                r#where: None,
                order_by: None,
                limit: None,
            },
        },
        replace,
        materialized,
    }
}

fn trigger(
    variant_events: Vec<TriggerEvent>,
    timing: TriggerTiming,
    for_each: ForEach,
    action: TriggerAction,
    when: Option<Expr>,
) -> Op {
    Op::CreateTrigger {
        name: "tg".into(),
        table: "t".into(),
        schema: None,
        timing,
        events: variant_events,
        for_each,
        action,
        when,
    }
}

fn body(statements: Vec<TriggerStmt>) -> TriggerAction {
    TriggerAction::Body { statements }
}

fn select_stmt() -> TriggerStmt {
    TriggerStmt::Select { expr: col_ref() }
}

/// The representative corpus: `(kind, variant, Op)`. Payload-dependent ops carry
/// one entry per distinct `support()` branch (see file header).
fn corpus() -> Vec<(&'static str, &'static str, Op)> {
    let mut c: Vec<(&'static str, &'static str, Op)> = Vec::new();

    // ── Payload-independent, fully portable ──────────────────────────────────
    c.push((
        "setTableOptions",
        "base",
        Op::SetTableOptions {
            table: "t".into(),
            options: TableRuntimeOptionsPatch::default(),
            schema: None,
        },
    ));
    c.push((
        "dropTable",
        "base",
        Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: None,
            existence_guard: None,
        },
    ));
    c.push((
        "renameTable",
        "base",
        Op::RenameTable {
            table: "t".into(),
            to: "t2".into(),
            schema: None,
            existence_guard: None,
        },
    ));
    c.push((
        "dropColumn",
        "base",
        Op::DropColumn {
            table: "t".into(),
            column: "a".into(),
            schema: None,
            existence_guard: None,
        },
    ));
    c.push((
        "dropIndex",
        "base",
        Op::DropIndex {
            name: "i".into(),
            table: None,
            unique: None,
            concurrently: None,
            schema: None,
            existence_guard: None,
        },
    ));
    c.push((
        "setColumnNotNull",
        "base",
        Op::SetColumnNotNull {
            table: "t".into(),
            column: "a".into(),
            schema: None,
            existence_guard: None,
        },
    ));
    c.push((
        "dropColumnNotNull",
        "base",
        Op::DropColumnNotNull {
            table: "t".into(),
            column: "a".into(),
            schema: None,
            existence_guard: None,
        },
    ));
    c.push((
        "dropColumnDefault",
        "base",
        Op::DropColumnDefault {
            table: "t".into(),
            column: "a".into(),
            schema: None,
            existence_guard: None,
        },
    ));
    c.push((
        "dropConstraint",
        "base",
        Op::DropConstraint {
            table: "t".into(),
            name: "c".into(),
            schema: None,
            existence_guard: None,
        },
    ));
    c.push((
        "validateConstraint",
        "base",
        Op::ValidateConstraint {
            table: "t".into(),
            name: "c".into(),
            schema: None,
            existence_guard: None,
        },
    ));
    c.push((
        "update",
        "base",
        Op::Update {
            table: "t".into(),
            set: std::iter::once(("a".to_string(), col_ref())).collect(),
            r#where: None,
            batch: None,
            schema: None,
        },
    ));
    c.push((
        "delete",
        "base",
        Op::Delete {
            table: "t".into(),
            r#where: col_ref(),
            limit: None,
            schema: None,
        },
    ));
    c.push((
        "backfill",
        "base",
        Op::Backfill {
            table: "t".into(),
            cursor_column: "id".into(),
            batch_size: SafeU64::new(100).expect("safe u64"),
            set: std::iter::once(("a".to_string(), col_ref())).collect(),
            filter: None,
            name: "bf".into(),
            schema: None,
        },
    ));
    c.push((
        "createEnum",
        "base",
        Op::CreateEnum {
            name: "e".into(),
            schema: None,
            values: vec!["a".into()],
        },
    ));
    c.push((
        "dropEnum",
        "base",
        Op::DropEnum {
            name: "e".into(),
            schema: None,
            existence_guard: None,
        },
    ));
    c.push((
        "dropDomain",
        "base",
        Op::DropDomain {
            name: "d".into(),
            schema: None,
            existence_guard: None,
        },
    ));
    c.push((
        "dropTrigger",
        "base",
        Op::DropTrigger {
            name: "tg".into(),
            table: "t".into(),
            schema: None,
            if_exists: None,
        },
    ));

    // ── Payload-independent, PostgreSQL-only CORE ────────────────────────────
    c.push((
        "comment",
        "base",
        Op::Comment {
            target: zeroship_migrate::model::ir::CommentTarget::Table {
                schema: None,
                name: "t".into(),
            },
            comment: Some("hi".into()),
        },
    ));
    c.push((
        "createPartition",
        "base",
        Op::CreatePartition {
            name: "p".into(),
            of: "t".into(),
            bounds: PartitionBounds::Default,
            schema: None,
            existence_guard: None,
        },
    ));
    c.push((
        "detachPartition",
        "base",
        Op::DetachPartition {
            parent: "t".into(),
            name: "p".into(),
            schema: None,
            concurrently: None,
        },
    ));
    c.push((
        "dropPartition",
        "base",
        Op::DropPartition {
            name: "p".into(),
            schema: None,
            existence_guard: None,
            cascade: None,
        },
    ));
    c.push((
        "createSequence",
        "base",
        Op::CreateSequence {
            name: "s".into(),
            schema: None,
            as_type: None,
            increment: None,
            start: None,
            min_value: None,
            max_value: None,
            cache: None,
            cycle: None,
            owned_by: None,
        },
    ));
    c.push((
        "alterSequence",
        "base",
        Op::AlterSequence {
            name: "s".into(),
            schema: None,
            increment: None,
            restart: None,
            min_value: None,
            max_value: None,
            cache: None,
            cycle: None,
            owned_by: None,
        },
    ));
    c.push((
        "dropSequence",
        "base",
        Op::DropSequence {
            name: "s".into(),
            schema: None,
            existence_guard: None,
        },
    ));

    // ── Payload-independent, VENDOR-tier PG-only ─────────────────────────────
    c.push((
        "createSchema",
        "base",
        Op::CreateSchema {
            name: "s".into(),
            if_not_exists: None,
            authorization: None,
        },
    ));
    c.push((
        "dropSchema",
        "base",
        Op::DropSchema {
            name: "s".into(),
            if_exists: None,
            cascade: None,
        },
    ));
    c.push((
        "createExtension",
        "base",
        Op::CreateExtension {
            name: "citext".into(),
            if_not_exists: None,
            schema: None,
        },
    ));
    c.push((
        "dropExtension",
        "base",
        Op::DropExtension {
            name: "citext".into(),
            if_exists: None,
        },
    ));
    c.push((
        "alterRole",
        "base",
        Op::AlterRole {
            name: "r".into(),
            set_search_path: None,
            reset_search_path: Some(true),
        },
    ));
    c.push((
        "dropRole",
        "base",
        Op::DropRole {
            name: "r".into(),
            if_exists: None,
        },
    ));
    c.push((
        "dropOwnedBy",
        "base",
        Op::DropOwnedBy {
            roles: vec!["r".into()],
        },
    ));
    c.push((
        "grant",
        "base",
        Op::Grant {
            privileges: vec![Privilege::Select],
            on: GrantTarget::Table {
                names: vec!["t".into()],
                schema: None,
            },
            to: vec!["r".into()],
            with_grant_option: None,
        },
    ));
    c.push((
        "revoke",
        "base",
        Op::Revoke {
            privileges: vec![Privilege::Select],
            on: GrantTarget::Table {
                names: vec!["t".into()],
                schema: None,
            },
            from: vec!["r".into()],
        },
    ));
    c.push(("enableRls", "base", Op::EnableRls { table: "t".into(), schema: None }));
    c.push(("forceRls", "base", Op::ForceRls { table: "t".into(), schema: None }));
    c.push(("disableRls", "base", Op::DisableRls { table: "t".into(), schema: None }));
    c.push(("noForceRls", "base", Op::NoForceRls { table: "t".into(), schema: None }));
    c.push((
        "createPolicy",
        "base",
        Op::CreatePolicy {
            name: "p".into(),
            table: "t".into(),
            schema: None,
            for_cmd: PolicyCmd::All,
            to: None,
            using: col_ref(),
            with_check: None,
        },
    ));
    c.push((
        "dropPolicy",
        "base",
        Op::DropPolicy {
            name: "p".into(),
            table: "t".into(),
            schema: None,
            if_exists: None,
        },
    ));
    c.push((
        "createFunction",
        "base",
        Op::CreateFunction {
            name: "f".into(),
            schema: None,
            args: None,
            returns: "trigger".into(),
            language: zeroship_migrate::model::ir::FuncLanguage::Plpgsql,
            replace: None,
            volatility: None,
            body: "BEGIN RETURN NEW; END".into(),
        },
    ));
    c.push((
        "dropFunction",
        "base",
        Op::DropFunction {
            name: "f".into(),
            schema: None,
            arg_types: None,
            if_exists: None,
        },
    ));
    c.push((
        "pgRaw",
        "base",
        Op::PgRaw {
            sql: "SELECT 1".into(),
            reason: "test".into(),
        },
    ));

    // ── createTable — one entry per support() branch ─────────────────────────
    c.push(("createTable", "base", plain_table("t", vec![column("id", ColType::BigInt, None, None)], None)));
    c.push((
        "createTable",
        "partitioned",
        Op::CreateTable {
            name: "t".into(),
            columns: vec![column("id", ColType::BigInt, None, None)],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],
            partition_by: Some(PartitionSpec::Range {
                columns: vec!["id".into()],
            }),
            runtime_options: None,
            schema: None,
            existence_guard: None,
        },
    ));
    c.push((
        "createTable",
        "pgOnlyIndexFeature",
        Op::CreateTable {
            name: "t".into(),
            columns: vec![column("created_at", ColType::Timestamp, None, None)],
            primary_key: None,
            constraints: vec![],
            indexes: vec![IrIndex {
                name: Some("t_brin".into()),
                columns: vec![column_element("created_at")],
                unique: None,
                using: Some(IndexMethod::Brin),
                r#where: None,
                include: vec![],
                with: None,
                only: None,
            }],
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        },
    ));
    c.push((
        "createTable",
        "nextvalDefault",
        plain_table("t", vec![column("id", ColType::BigInt, Some(nextval_default()), None)], None),
    ));
    c.push((
        "createTable",
        "identityAlways",
        plain_table(
            "t",
            vec![column("id", ColType::BigInt, None, Some(IdentityCol { always: true }))],
            Some(vec!["id".into()]),
        ),
    ));
    c.push((
        "createTable",
        "nonportableByDefaultIdentity",
        // by-default identity that is NOT the sole single-column primary key.
        plain_table(
            "t",
            vec![
                column("id", ColType::BigInt, None, None),
                column("seq", ColType::BigInt, None, Some(IdentityCol { always: false })),
            ],
            Some(vec!["id".into()]),
        ),
    ));

    // ── addColumn ────────────────────────────────────────────────────────────
    let add_column = |default: Option<IrDefault>, identity: Option<IdentityCol>| Op::AddColumn {
        table: "t".into(),
        column: "a".into(),
        ty: ColType::BigInt,
        nullable: Some(true),
        default,
        vector_metric: None,
        case_sensitive: None,
        mask: None,
        generated: None,
        identity,
        schema: None,
        existence_guard: None,
    };
    c.push(("addColumn", "base", add_column(None, None)));
    c.push(("addColumn", "identity", add_column(None, Some(IdentityCol { always: false }))));
    c.push(("addColumn", "nextvalDefault", add_column(Some(nextval_default()), None)));

    // ── createIndex ──────────────────────────────────────────────────────────
    c.push(("createIndex", "base", create_index(vec![column_element("a")], None, None, vec![])));
    c.push((
        "createIndex",
        "pgOnlyMethodOrFeature",
        create_index(vec![column_element("a")], Some(IndexMethod::Gin), None, vec![]),
    ));
    c.push((
        "createIndex",
        "exprElement",
        create_index(vec![IndexElement::Expr { expr: col_ref() }], None, None, vec![]),
    ));
    c.push((
        "createIndex",
        "partialWhere",
        create_index(vec![column_element("a")], None, Some(col_ref()), vec![]),
    ));

    // ── setColumnType ────────────────────────────────────────────────────────
    let set_col_type = |using: Option<Expr>| Op::SetColumnType {
        table: "t".into(),
        column: "a".into(),
        to_type: ColType::Text,
        using,
        schema: None,
        existence_guard: None,
    };
    c.push(("setColumnType", "base", set_col_type(None)));
    c.push(("setColumnType", "using", set_col_type(Some(col_ref()))));

    // ── setColumnDefault ─────────────────────────────────────────────────────
    let set_col_default = |value: IrDefault| Op::SetColumnDefault {
        table: "t".into(),
        column: "a".into(),
        value,
        schema: None,
        existence_guard: None,
    };
    c.push((
        "setColumnDefault",
        "base",
        set_col_default(IrDefault::Literal {
            value: zeroship_migrate::model::ir::IrScalar::Int(1),
        }),
    ));
    c.push((
        "setColumnDefault",
        "containerOrJson",
        set_col_default(IrDefault::Container {
            kind: EmptyContainerKind::Object,
        }),
    ));
    c.push((
        "setColumnDefault",
        "fn",
        set_col_default(IrDefault::Fn {
            r#fn: zeroship_migrate::model::ir::SynthDefaultFn::Now,
        }),
    ));
    c.push(("setColumnDefault", "nextval", set_col_default(nextval_default())));

    // ── renameColumn ─────────────────────────────────────────────────────────
    let rename_column = |guard: Option<zeroship_migrate::model::ir::ExistenceGuard>| Op::RenameColumn {
        table: "t".into(),
        from: "a".into(),
        to: "b".into(),
        ty: ColType::Text,
        schema: None,
        existence_guard: guard,
    };
    c.push(("renameColumn", "base", rename_column(None)));
    c.push((
        "renameColumn",
        "existenceGuard",
        rename_column(Some(zeroship_migrate::model::ir::ExistenceGuard::IfExists)),
    ));

    // ── addConstraint ────────────────────────────────────────────────────────
    c.push(("addConstraint", "fkSimple", add_constraint(fk(vec!["a"], vec!["id"]))));
    c.push((
        "addConstraint",
        "unique",
        add_constraint(IrConstraintKind::Unique { columns: vec!["a".into()] }),
    ));
    c.push((
        "addConstraint",
        "check",
        add_constraint(IrConstraintKind::Check { expr: col_ref(), not_valid: None }),
    ));
    c.push((
        "addConstraint",
        "exclusion",
        add_constraint(IrConstraintKind::Exclusion {
            using_method: ExclusionMethod::Gist,
            elements: vec![],
            where_predicate: None,
            deferrable: None,
            initially_deferred: None,
        }),
    ));
    c.push(("addConstraint", "fkComposite", add_constraint(fk(vec!["a", "b"], vec!["id", "x"]))));
    c.push(("addConstraint", "fkNonId", add_constraint(fk(vec!["a"], vec!["other_col"]))));
    c.push((
        "addConstraint",
        "fkNotValid",
        add_constraint(IrConstraintKind::Fk {
            columns: vec!["a".into()],
            references_table: "other".into(),
            references_columns: vec!["id".into()],
            on_delete: None,
            on_update: None,
            deferrable: None,
            initially_deferred: None,
            not_valid: Some(true),
        }),
    ));
    c.push((
        "addConstraint",
        "pk",
        add_constraint(IrConstraintKind::Pk { columns: vec!["a".into()] }),
    ));
    c.push(("addConstraint", "fkNoLocalColumn", add_constraint(fk(vec![], vec!["id"]))));

    // ── insert ───────────────────────────────────────────────────────────────
    let insert = |on_conflict: Option<zeroship_migrate::model::ir::IrOnConflict>| Op::Insert {
        table: "t".into(),
        columns: vec!["a".into()],
        rows: vec![],
        on_conflict,
        schema: None,
    };
    c.push(("insert", "base", insert(None)));
    c.push((
        "insert",
        "onConflict",
        insert(Some(zeroship_migrate::model::ir::IrOnConflict {
            columns: vec!["a".into()],
            do_update: None,
        })),
    ));

    // ── createView / dropView ────────────────────────────────────────────────
    c.push(("createView", "base", structured_view("v", None, None)));
    c.push(("createView", "materialized", structured_view("v", Some(true), None)));
    c.push(("createView", "materializedReplace", structured_view("v", Some(true), Some(true))));
    c.push((
        "dropView",
        "base",
        Op::DropView {
            name: "v".into(),
            schema: None,
            existence_guard: None,
            materialized: None,
        },
    ));
    c.push((
        "dropView",
        "materialized",
        Op::DropView {
            name: "v".into(),
            schema: None,
            existence_guard: None,
            materialized: Some(true),
        },
    ));

    // ── createDomain ─────────────────────────────────────────────────────────
    let create_domain = |default: Option<IrDefault>| Op::CreateDomain {
        name: "d".into(),
        schema: None,
        as_type: ColType::Text,
        check: None,
        default,
        not_null: None,
    };
    c.push(("createDomain", "base", create_domain(None)));
    c.push(("createDomain", "nextvalDefault", create_domain(Some(nextval_default()))));

    // ── createRole ───────────────────────────────────────────────────────────
    let create_role = |superuser: Option<bool>, if_not_exists: Option<bool>| Op::CreateRole {
        name: "r".into(),
        login: None,
        password: None,
        bypass_rls: None,
        create_role: None,
        create_db: None,
        superuser,
        in_role: None,
        set_search_path: None,
        if_not_exists,
    };
    c.push(("createRole", "base", create_role(None, None)));
    c.push(("createRole", "superuserIfNotExists", create_role(Some(true), Some(true))));

    // ── createTrigger — one entry per support() branch ───────────────────────
    c.push((
        "createTrigger",
        "executeFunction",
        trigger(
            vec![TriggerEvent::Insert],
            TriggerTiming::Before,
            ForEach::Row,
            TriggerAction::ExecuteFunction { name: "f".into() },
            None,
        ),
    ));
    c.push((
        "createTrigger",
        "bodySimple",
        trigger(vec![TriggerEvent::Insert], TriggerTiming::Before, ForEach::Row, body(vec![select_stmt()]), None),
    ));
    c.push((
        "createTrigger",
        "bodyTruncateEvent",
        trigger(vec![TriggerEvent::Truncate], TriggerTiming::Before, ForEach::Row, body(vec![select_stmt()]), None),
    ));
    c.push((
        "createTrigger",
        "bodyStatementLevel",
        trigger(vec![TriggerEvent::Insert], TriggerTiming::Before, ForEach::Statement, body(vec![select_stmt()]), None),
    ));
    c.push((
        "createTrigger",
        "bodyMultipleEvents",
        trigger(
            vec![TriggerEvent::Insert, TriggerEvent::Update],
            TriggerTiming::Before,
            ForEach::Row,
            body(vec![select_stmt()]),
            None,
        ),
    ));
    c.push((
        "createTrigger",
        "bodyInsteadOf",
        trigger(vec![TriggerEvent::Insert], TriggerTiming::InsteadOf, ForEach::Row, body(vec![select_stmt()]), None),
    ));
    c.push((
        "createTrigger",
        "bodyWhen",
        trigger(vec![TriggerEvent::Insert], TriggerTiming::Before, ForEach::Row, body(vec![select_stmt()]), Some(col_ref())),
    ));
    c.push((
        "createTrigger",
        "bodyRaiseIgnore",
        trigger(
            vec![TriggerEvent::Insert],
            TriggerTiming::Before,
            ForEach::Row,
            body(vec![TriggerStmt::Raise {
                level: RaiseLevel::Ignore,
                message: "m".into(),
                errcode: None,
            }]),
            None,
        ),
    ));

    c
}

// POST-S0.2 — what now guards what.
//
// In S0.1 this file proved `generated DIALECT_TABLE == decision_to_disposition(
// Op::support())` for every op × dialect. S0.2 made `Op::support` READ the table
// (looking the disposition up by `Op::op_variant`), so that agreement is now
// TAUTOLOGICAL and has been retired. The load-bearing behavioural gate moved to
// `op_support_matrix` (`decision()` == the live validate/lower behaviour). This
// file is repurposed to the TWO invariants that remain meaningful once the table
// is the consumer's source of truth:
//
//   * SHARED VARIANT DERIVATION — every representative corpus op's
//     `Op::op_variant()` equals its labelled variant. `op_variant` is the single
//     branch-selection shared by `Op::support` and this corpus, so this pins the
//     two against drift.
//   * SIDECAR ⟷ GENERATED TABLE — the committed `dialect_table.rs`'s
//     `DIALECT_TABLE` matches the hand-authored, human-reviewed
//     `dialect-support.toml` (the same sidecar → table drift the TS
//     `dialect-table-drift` test guards with a byte-level regenerate, checked
//     here Rust-side and node-free). Together with `op_support_matrix` this closes
//     the loop: sidecar ⟷ table ⟷ (op_variant∘table) `Op::support` ⟷ validate.
#[test]
fn op_variant_matches_the_corpus_and_the_generated_table_matches_the_sidecar() {
    let corpus = corpus();

    // 1. Exhaustiveness over op-KINDS: the corpus covers exactly the schema's Op
    //    discriminants (the 55-op wire contract). No op silently uncovered.
    let corpus_kinds: BTreeSet<String> = corpus.iter().map(|(k, _, _)| (*k).to_string()).collect();
    let schema_kinds = schema_op_tags();
    assert_eq!(
        schema_kinds.len(),
        55,
        "the wire contract must still carry the closed 55-op discriminant set"
    );
    assert_eq!(
        corpus_kinds, schema_kinds,
        "faithfulness corpus op-kinds must equal the schema's Op discriminants"
    );

    // 2. SHARED VARIANT DERIVATION: each representative op reports the labelled
    //    variant AND kind through the crate's `Op::op_variant` / serde tag — the
    //    same derivation `Op::support` uses to key the table lookup. This is what
    //    keeps the corpus and the engine's variant selection from drifting.
    for (kind, variant, op) in &corpus {
        assert_eq!(
            &op.op_variant(),
            variant,
            "corpus labels {kind}/{variant} but Op::op_variant() disagrees"
        );
        assert_eq!(
            &op_tag(op).as_str(),
            kind,
            "corpus labels kind {kind} but the op's serde tag disagrees"
        );
    }

    // 3. Exhaustiveness over TABLE ROWS: the corpus's (kind, variant) pairs are a
    //    BIJECTION with the generated DIALECT_TABLE rows. Every table row is
    //    exercised by a representative op; every corpus case has a row.
    let corpus_pairs: BTreeSet<(String, String)> = corpus
        .iter()
        .map(|(k, v, _)| ((*k).to_string(), (*v).to_string()))
        .collect();
    assert_eq!(
        corpus_pairs.len(),
        corpus.len(),
        "faithfulness corpus must not contain duplicate (kind, variant) entries"
    );
    let table_pairs: BTreeSet<(String, String)> = DIALECT_TABLE
        .iter()
        .map(|row| (row.kind.to_string(), row.variant.to_string()))
        .collect();
    assert_eq!(
        corpus_pairs, table_pairs,
        "generated DIALECT_TABLE rows must be a bijection with the faithfulness corpus"
    );

    // 4. SIDECAR ⟷ TABLE: the generated const table matches the human-reviewed
    //    sidecar row-for-row (kind, variant, and each dialect's disposition token),
    //    so the committed `dialect_table.rs` cannot be hand-edited to diverge from
    //    its single source.
    let mut sidecar: Vec<SidecarRow> = parse_sidecar();
    sidecar.sort_by(|a, b| (a.kind.as_str(), a.variant.as_str()).cmp(&(&b.kind, &b.variant)));
    let mut generated: Vec<SidecarRow> = DIALECT_TABLE
        .iter()
        .map(|row| SidecarRow {
            kind: row.kind.to_string(),
            variant: row.variant.to_string(),
            pg: disposition_token(row.postgres).to_string(),
            sqlite: disposition_token(row.sqlite).to_string(),
            mysql: disposition_token(row.mysql).to_string(),
        })
        .collect();
    generated.sort_by(|a, b| (a.kind.as_str(), a.variant.as_str()).cmp(&(&b.kind, &b.variant)));
    assert_eq!(
        generated, sidecar,
        "generated dialect_table.rs drifted from dialect-support.toml — regenerate with \
         `pnpm --filter @zeroship/migrate gen:dialect-table`"
    );

    // No S0.1 row is TransparentDegradable — the current engine produces only
    // Supported/Unsupported, so that disposition is reserved for the redesign.
    assert!(
        DIALECT_TABLE.iter().all(|row| {
            row.postgres != Disposition::TransparentDegradable
                && row.sqlite != Disposition::TransparentDegradable
                && row.mysql != Disposition::TransparentDegradable
        }),
        "no S0.1 row may be TransparentDegradable (the current engine never degrades)"
    );
}
