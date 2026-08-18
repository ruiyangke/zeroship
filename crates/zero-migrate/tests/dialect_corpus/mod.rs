//! The SHARED dialect-table corpus: one representative `Op` per
//! `(op-kind, variant)` row of `dialect-support.toml`.
//!
//! Lifted out of `dialect_table_faithfulness.rs` so a SECOND consumer can reach
//! it. That file still owns the offline guarantees (variant derivation,
//! kind exhaustiveness, corpus/table bijection, sidecar drift) and imports this
//! module unchanged; `dialect_conformance_live.rs` drives the SAME ops through
//! the real apply path against a live server, so the declaration a row makes and
//! the behaviour a server exhibits are checked against ONE corpus rather than two
//! that can drift apart.
//!
//! This is `docs/proposals/backend-conformance.md` decision 7 ("the corpus is
//! LIFTED from `dialect_table_faithfulness.rs`, not written fresh") at the
//! smallest scope that buys it: a `tests/` submodule rather than a crate, because
//! a crate is only needed once a backend lives outside this repo (its step 7).
//!
//! NOTHING here changed in the lift. The ops are byte-identical to the ones the
//! faithfulness test built, which is what keeps the bijection it proves meaningful
//! for the live layer too. What the live layer adds is a per-row PRELUDE
//! (`dialect_conformance_live.rs`), because these ops are built to be
//! CONSTRUCTIBLE and to select a support branch — they are not built to have their
//! referents exist.

#![allow(dead_code)] // Each consumer uses a different part of this module.

use zero_migrate::model::expr::Expr;
use zero_migrate::model::ir::{
    AlterPrimaryKeyAction, BackfillSetValue, ColType, EmptyContainerKind, ExclusionMethod, ForEach,
    GrantTarget, IdentityCol, IndexElement, IndexMethod, IrColumn, IrConstraint, IrConstraintKind,
    IrDefault, IrIndex, IrValue, Op, PartitionBounds, PartitionSpec, PolicyCmd, Privilege,
    RaiseLevel, SafeI64, SafeU64, SelectAst, SequenceRef, TableRef, TableRuntimeOptionsPatch,
    TriggerAction, TriggerEvent, TriggerStmt, TriggerTiming, ViewQuery,
};

fn col_ref() -> Expr {
    Expr::ColRef {
        name: "x".into(),
        table: None,
    }
}

/// A minimal `IrColumn` with the given type / default / identity facets. Only the
/// facets `support()` inspects (default, identity) matter here.
fn column(
    name: &str,
    ty: ColType,
    default: Option<IrDefault>,
    identity: Option<IdentityCol>,
) -> IrColumn {
    IrColumn {
        name: name.into(),
        ty,
        nullable: Some(false),
        default,
        unique: None,
        value_format: None,
        references: None,
        id_prefix: None,
        collation: None,
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

fn create_index(
    elements: Vec<IndexElement>,
    using: Option<IndexMethod>,
    r#where: Option<Expr>,
    include: Vec<String>,
) -> Op {
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
        nulls_not_distinct: None,
        schema: None,
        existence_guard: None,
    }
}

fn column_element(name: &str) -> IndexElement {
    IndexElement::Column {
        name: name.into(),
        order: None,
        opclass: None,
        collation: None,
    }
}

fn structured_view(name: &str, materialized: Option<bool>, replace: Option<bool>) -> Op {
    Op::CreateView {
        name: name.into(),
        schema: None,
        columns: None,
        query: ViewQuery::Structured {
            select: Box::new(SelectAst {
                from: TableRef {
                    name: "t".into(),
                    schema: None,
                    alias: None,
                },
                projection: vec![],
                joins: vec![],
                r#where: None,
                group_by: Vec::new(),
                having: None,
                order_by: None,
                limit: None,
            }),
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
    trigger_on("t", variant_events, timing, for_each, action, when)
}

/// A trigger on a named target. Only the `INSTEAD OF` row needs one that is not
/// `t`, because only that timing requires a VIEW.
fn trigger_on(
    target: &str,
    variant_events: Vec<TriggerEvent>,
    timing: TriggerTiming,
    for_each: ForEach,
    action: TriggerAction,
    when: Option<Expr>,
) -> Op {
    Op::CreateTrigger {
        name: "tg".into(),
        table: target.into(),
        schema: None,
        timing,
        events: variant_events,
        for_each,
        action,
        when,
    }
}

const fn body(statements: Vec<TriggerStmt>) -> TriggerAction {
    TriggerAction::Body { statements }
}

fn select_stmt() -> TriggerStmt {
    TriggerStmt::Select { expr: col_ref() }
}

/// The representative corpus: `(kind, variant, Op)`. Payload-dependent ops carry
/// one entry per distinct `support()` branch (see file header).
// Intentional `Vec::new()` + per-op `push` for line-by-line readability of the
// hand-authored corpus; folding the first push into `vec![]` buys nothing here.
#[allow(clippy::vec_init_then_push)]
pub fn corpus() -> Vec<(&'static str, &'static str, Op)> {
    let mut c: Vec<(&'static str, &'static str, Op)> = Vec::new();

    // ── Payload-independent, fully portable ──────────────────────────────────
    c.push((
        "alterPrimaryKey",
        "base",
        Op::AlterPrimaryKey {
            table: "t".into(),
            action: AlterPrimaryKeyAction::Add {
                columns: vec!["id".into()],
            },
            schema: None,
        },
    ));
    c.push((
        "synchronizeIdentity",
        "base",
        Op::SynchronizeIdentity {
            table: "t".into(),
            column: "id".into(),
            writes_quiesced: "import_window".into(),
            schema: None,
        },
    ));
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
            set: std::iter::once(("a".to_string(), IrValue::Expr(col_ref()))).collect(),
            r#where: None,
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
            cursor_columns: vec!["id".into()],
            cursor_stability: zero_migrate::CursorStability::GuardUpdates,
            batch_size: SafeU64::new(100).expect("safe u64"),
            set: std::iter::once((
                "a".to_string(),
                BackfillSetValue::from(IrValue::Expr(col_ref())),
            ))
            .collect(),
            filter: None,
            name: "bf".into(),
            schema: None,
        },
    ));
    c.push((
        "dialectal",
        "base",
        Op::Dialectal {
            default: Some(Vec::new()),
            pg: None,
            sqlite: None,
            mysql: None,
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
            target: zero_migrate::model::ir::CommentTarget::Table {
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
        "attachPartition",
        "base",
        Op::AttachPartition {
            parent: "t".into(),
            name: "p".into(),
            bound: PartitionBounds::Default,
            schema: None,
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
            parent: "t".into(),
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
    // `alterSequence` has ONE variant, so any option selects the same branch - and
    // an alter carrying NONE of them is degenerate in the same way `insert` with
    // `rows: []` is. `ALTER SEQUENCE <name>` with no action clause is not a
    // statement (PostgreSQL: `syntax error at end of input`), so the option-less
    // shape could never apply on any dialect and the row measured nothing. The
    // increment makes the representative executable; the refusal that the
    // option-less shape now earns is pinned by
    // `tests/alter_sequence_needs_an_action.rs`.
    c.push((
        "alterSequence",
        "base",
        Op::AlterSequence {
            name: "s".into(),
            schema: None,
            increment: Some(SafeI64::new(2).expect("safe i64")),
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
    c.push((
        "setRls",
        "base",
        Op::SetRls {
            table: "t".into(),
            schema: None,
            enabled: Some(true),
            forced: Some(true),
        },
    ));
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
            language: zero_migrate::model::ir::FuncLanguage::Plpgsql,
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
    c.push((
        "createTable",
        "base",
        plain_table("t", vec![column("id", ColType::BigInt, None, None)], None),
    ));
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
                collapse: false,
            }),
            runtime_options: None,
            schema: None,
            existence_guard: None,
        },
    ));
    c.push((
        "createTable",
        "partitionedCollapse",
        Op::CreateTable {
            name: "t".into(),
            columns: vec![column("id", ColType::BigInt, None, None)],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],
            partition_by: Some(PartitionSpec::Range {
                columns: vec!["id".into()],
                collapse: true,
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
                nulls_not_distinct: None,
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
        plain_table(
            "t",
            vec![column("id", ColType::BigInt, Some(nextval_default()), None)],
            None,
        ),
    ));
    c.push((
        "createTable",
        "identityAlways",
        plain_table(
            "t",
            vec![column(
                "id",
                ColType::BigInt,
                None,
                Some(IdentityCol { always: true }),
            )],
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
                column(
                    "seq",
                    ColType::BigInt,
                    None,
                    Some(IdentityCol { always: false }),
                ),
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
        value_format: None,
        vector_metric: None,
        case_sensitive: None,
        mask: None,
        generated: None,
        identity,
        schema: None,
        existence_guard: None,
    };
    c.push(("addColumn", "base", add_column(None, None)));
    c.push((
        "addColumn",
        "identity",
        add_column(None, Some(IdentityCol { always: false })),
    ));
    c.push((
        "addColumn",
        "nextvalDefault",
        add_column(Some(nextval_default()), None),
    ));

    // ── createIndex ──────────────────────────────────────────────────────────
    c.push((
        "createIndex",
        "base",
        create_index(vec![column_element("a")], None, None, vec![]),
    ));
    c.push((
        "createIndex",
        "pgOnlyMethodOrFeature",
        create_index(
            vec![column_element("a")],
            Some(IndexMethod::Gin),
            None,
            vec![],
        ),
    ));
    c.push((
        "createIndex",
        "exprElement",
        create_index(
            vec![IndexElement::Expr { expr: col_ref() }],
            None,
            None,
            vec![],
        ),
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
            value: zero_migrate::model::ir::IrScalar::Int(1),
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
        "nextval",
        set_col_default(nextval_default()),
    ));

    // ── renameColumn ─────────────────────────────────────────────────────────
    let rename_column = |guard: Option<zero_migrate::model::ir::ExistenceGuard>| Op::RenameColumn {
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
        rename_column(Some(zero_migrate::model::ir::ExistenceGuard::IfExists)),
    ));

    // ── addConstraint ────────────────────────────────────────────────────────
    c.push((
        "addConstraint",
        "fkSimple",
        add_constraint(fk(vec!["a"], vec!["id"])),
    ));
    c.push((
        "addConstraint",
        "unique",
        add_constraint(IrConstraintKind::Unique {
            columns: vec!["a".into()],
        }),
    ));
    c.push((
        "addConstraint",
        "check",
        add_constraint(IrConstraintKind::Check {
            expr: col_ref(),
            not_valid: None,
        }),
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
    c.push((
        "addConstraint",
        "fkComposite",
        add_constraint(fk(vec!["a", "b"], vec!["id", "x"])),
    ));
    c.push((
        "addConstraint",
        "fkNonId",
        add_constraint(fk(vec!["a"], vec!["other_col"])),
    ));
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
        "fkNoLocalColumn",
        add_constraint(fk(vec![], vec!["id"])),
    ));

    // ── insert ───────────────────────────────────────────────────────────────
    let insert = |on_conflict: Option<zero_migrate::model::ir::IrOnConflict>| Op::Insert {
        table: "t".into(),
        columns: vec!["a".into()],
        rows: vec![],
        on_conflict,
        schema: None,
    };
    c.push(("insert", "base", insert(None)));
    c.push((
        "insert",
        "onConflictDoUpdate",
        insert(Some(zero_migrate::model::ir::IrOnConflict {
            columns: vec!["a".into()],
            do_update: Some(std::collections::BTreeMap::from([(
                "b".into(),
                zero_migrate::model::ir::IrValue::Scalar(zero_migrate::model::ir::IrScalar::Int(1)),
            )])),
        })),
    ));
    c.push((
        "insert",
        "onConflictDoNothing",
        insert(Some(zero_migrate::model::ir::IrOnConflict {
            columns: vec!["a".into()],
            do_update: None,
        })),
    ));

    // ── createView / dropView ────────────────────────────────────────────────
    c.push(("createView", "base", structured_view("v", None, None)));
    c.push((
        "createView",
        "materialized",
        structured_view("v", Some(true), None),
    ));
    c.push((
        "createView",
        "materializedReplace",
        structured_view("v", Some(true), Some(true)),
    ));
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
    c.push((
        "createDomain",
        "nextvalDefault",
        create_domain(Some(nextval_default())),
    ));

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
    c.push((
        "createRole",
        "superuserIfNotExists",
        create_role(Some(true), Some(true)),
    ));

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
        trigger(
            vec![TriggerEvent::Insert],
            TriggerTiming::Before,
            ForEach::Row,
            body(vec![select_stmt()]),
            None,
        ),
    ));
    c.push((
        "createTrigger",
        "bodyTruncateEvent",
        trigger(
            vec![TriggerEvent::Truncate],
            TriggerTiming::Before,
            ForEach::Row,
            body(vec![select_stmt()]),
            None,
        ),
    ));
    c.push((
        "createTrigger",
        "bodyStatementLevel",
        trigger(
            vec![TriggerEvent::Insert],
            TriggerTiming::Before,
            ForEach::Statement,
            body(vec![select_stmt()]),
            None,
        ),
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
    // The ONLY row whose target is `v` rather than `t`, and it has to be. An
    // INSTEAD OF trigger exists to make a VIEW writable, and both dialects that
    // have the timing refuse it on a table in their own words - SQLite `cannot
    // create INSTEAD OF trigger on table: t`, PostgreSQL `"t" is a table`. A
    // representative aimed at a table could therefore never apply anywhere, so it
    // measured the engine's missing gate rather than the declaration. The gate now
    // exists (`IrLowerError::InsteadOfTriggerTargetIsATable`) and is pinned by
    // `tests/instead_of_trigger_needs_a_view.rs`; this row measures the
    // declaration, which is about the timing, not the target.
    c.push((
        "createTrigger",
        "bodyInsteadOf",
        trigger_on(
            "v",
            vec![TriggerEvent::Insert],
            TriggerTiming::InsteadOf,
            ForEach::Row,
            body(vec![select_stmt()]),
            None,
        ),
    ));
    c.push((
        "createTrigger",
        "bodyWhen",
        trigger(
            vec![TriggerEvent::Insert],
            TriggerTiming::Before,
            ForEach::Row,
            body(vec![select_stmt()]),
            Some(col_ref()),
        ),
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
