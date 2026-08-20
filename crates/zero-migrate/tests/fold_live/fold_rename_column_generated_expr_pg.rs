//! Live PostgreSQL oracle for the generated-column body a column rename leaves behind.
//!
//! PostgreSQL holds a generated expression as a parse tree over ATTRIBUTE NUMBERS, so
//! `pg_get_expr` deparses the NEW name the instant the rename commits. That makes the
//! server the arbiter between the two offline replays that used to disagree: the
//! descriptor fold (`fold_to_field_defs`) rewrote the expression to follow the rename,
//! while the snapshot fold (`fold_ops`) kept the old name. Measured on PostgreSQL 18.4,
//! after `RENAME COLUMN qty_on_hand TO amount_on_hand`:
//!
//! | side                  | generated expression      |
//! |-----------------------|---------------------------|
//! | `fold_ops` (was)      | `("qty_on_hand" + 1)`     |
//! | the `FieldDef` map *  | `amount_on_hand`          |
//! | live `pg_get_expr`    | `(amount_on_hand + 1)`    |
//!
//! \* measured when `fold_to_field_defs` produced it; step 4 consumer 3 of
//! `docs/proposals/single-fold-and-effects.md` deleted that walker and the map is a
//! projection of the single fold now. The measurement stands - this test reads the
//! catalog rather than either replay, which is the whole point of it.
//!
//! So the snapshot fold was the wrong one. This test does not take the docs' word for
//! it: it reads the catalog and asserts the fold names the column the SERVER names.
//!
//! The rendered SPELLING still differs — the fold quotes identifiers and PostgreSQL's
//! deparse does not — and that is not what is asserted. What is asserted is the
//! COLUMN IDENTITY inside the body, which is the half a rename changes and the half
//! the SQLite rebuild emits DDL from.
//!
//! The rename runs as native `ALTER TABLE ... RENAME COLUMN` rather than through the
//! engine's `renameColumn` op, for the reason recorded in
//! `fold_rename_column_index_cascade_pg.rs`: the op lowers to an online
//! expand-contract whose contract phase is a separate deploy, so the live table would
//! carry BOTH names and the comparison would be against a shape neither side claims.

use crate::support;

use std::collections::BTreeMap;

use crate::support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::IrFlagsOverride;
use zero_migrate::render::fold::single_fold;
use zero_migrate::{
    fold_ops, resolve_create_table_policy, Approval, BinaryOp, ColType, EffectivePolicy,
    ExecutorConfig, Expr, GeneratedCol, GuardConfig, IrAuthor, IrColumn, IrScalar, LiveSchema,
    LockMode, MigrationEngine, MigrationIr, Op, PostgresBackend, SqlDialect,
};

/// The test-side PostgreSQL identifier spelling, written out here rather than
/// imported from the crate. It used to be
/// `zero_migrate::schema::query::quote_ident`, which was `pub`, un-dialected, and a
/// SECOND physical home for the spelling `render::backends::ansi_double_quote_ident`
/// owns; it is gone. A probe that builds its expectation by calling the emitter it is
/// checking is not an oracle anyway, so the replacement is deliberately independent —
/// the same shape the sibling `fold_rename_column_constraint_definition_pg` and
/// `fold_rename_column_check_body_pg` probes already use.
fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

const OWNER: &str = "app_fold_generated_expr_pg";
const TABLE: &str = "generated_body_rename";
const OLD_COLUMN: &str = "qty_on_hand";
const NEW_COLUMN: &str = "amount_on_hand";
const GENERATED_COLUMN: &str = "total_cents";

fn token() -> String {
    format!(
        "zm_gen_expr_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    )
}

fn col(name: &str, ty: ColType) -> IrColumn {
    IrColumn {
        name: name.to_string(),
        ty,
        nullable: Some(true),
        default: None,
        unique: None,
        value_format: None,
        references: None,
        id_prefix: None,
        collation: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
    }
}

fn ir(name: &str, ops: Vec<Op>) -> MigrationIr {
    MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: 1,
        name: name.to_string(),
        owner_app: OWNER.to_string(),
        ops,
        flags: IrFlagsOverride::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    }
}

fn create_ir() -> MigrationIr {
    let mut generated = col(GENERATED_COLUMN, ColType::Int);
    generated.generated = Some(GeneratedCol {
        expr: Expr::BinOp {
            op: BinaryOp::Add,
            lhs: Box::new(Expr::col(OLD_COLUMN)),
            rhs: Box::new(Expr::Literal {
                value: IrScalar::Int(1),
            }),
        },
        stored: true,
    });
    ir(
        "create_generated_body_rename",
        vec![Op::CreateTable {
            name: TABLE.to_string(),
            columns: vec![col(OLD_COLUMN, ColType::Int), generated],
            primary_key: None,
            constraints: Vec::new(),
            indexes: Vec::new(),
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }],
    )
}

fn rename_op() -> Op {
    Op::RenameColumn {
        table: TABLE.to_string(),
        from: OLD_COLUMN.to_string(),
        to: NEW_COLUMN.to_string(),
        ty: ColType::Int,
        schema: None,
        existence_guard: None,
    }
}

/// Which of the two column names a body mentions. `None` when it names neither, which
/// would mean the measurement is reading the wrong expression.
fn named_column(body: &str) -> Option<&'static str> {
    match (body.contains(OLD_COLUMN), body.contains(NEW_COLUMN)) {
        // `qty_on_hand` is not a substring of `amount_on_hand`, so the two are
        // distinguishable without tokenizing.
        (true, false) => Some(OLD_COLUMN),
        (false, true) => Some(NEW_COLUMN),
        _ => None,
    }
}

struct Measured {
    /// What `pg_get_expr` deparses from the catalog after the rename.
    live: String,
    /// What `fold_ops` renders into the snapshot.
    folded: String,
    /// What the `FieldDef` projection puts in the runtime descriptor.
    descriptor: String,
}

async fn apply_create(
    backend: &PostgresBackend<'_, PgDevSession>,
    cfg: &ExecutorConfig,
    policy: &EffectivePolicy,
    resolved: &MigrationIr,
) -> Result<(), String> {
    let resolved_source = serde_json::to_string(resolved)
        .map_err(|error| format!("serialize resolved IR: {error}"))?;
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, policy);
    let guard = GuardConfig::from_policy(policy.clone(), SqlDialect::Postgres);
    let artifact = author
        .load_and_lower_guarded(
            &resolved_source,
            OWNER,
            &BTreeMap::new(),
            &LiveSchema::default(),
            &guard,
        )
        .map_err(|error| format!("load and lower guarded IR plan: {error}"))?;
    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            backend,
            cfg,
            "fold-generated-expr-pg",
            LockMode::Acquire,
        )
        .await
        .map(|_| ())
        .map_err(|error| format!("{error}"))
}

async fn measure() -> Option<Measured> {
    let Some(url) = support::pg_url() else {
        support::announce_live_db_skip(support::PG_URL_ENV);
        return None;
    };
    let session = PgDevSession::connect(&url);
    let schema = token();
    let policy = support::no_inject(&schema);
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy.clone());
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.pg.meta_schema);
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated generated-body schema");

    let work: Result<Measured, String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;

        let policy = support::no_inject(&cfg.project_schema);
        let resolved = resolve_create_table_policy(&create_ir(), &policy, &cfg.project_schema)
            .map_err(|error| format!("resolve create-table policy: {error}"))?;
        apply_create(&backend, &cfg, &policy, &resolved).await?;

        let rename = format!(
            "ALTER TABLE {quoted_schema}.{} RENAME COLUMN {} TO {}",
            quote_ident(TABLE),
            quote_ident(OLD_COLUMN),
            quote_ident(NEW_COLUMN)
        );
        session
            .batch(&rename)
            .await
            .map_err(|error| format!("run native SQL `{rename}`: {error}"))?;

        // What the SERVER says the generated expression is, after the rename.
        let rows = session
            .query(
                &format!(
                    "SELECT pg_get_expr(d.adbin, d.adrelid) \
                     FROM pg_attribute a \
                     JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                     WHERE a.attrelid = '{}.{}'::regclass AND a.attname = '{GENERATED_COLUMN}'",
                    cfg.project_schema, TABLE
                ),
                &[],
            )
            .await
            .map_err(|error| format!("read pg_get_expr for the generated column: {error}"))?;
        let live: String = rows
            .first()
            .ok_or_else(|| "the catalog reports no generated expression".to_string())?
            .try_get(0)
            .map_err(|error| format!("decode pg_get_expr: {error}"))?;

        // What the two offline replays say, over the SAME op stream.
        let mut ops = resolved.ops.clone();
        ops.push(rename_op());
        let folded = fold_ops(&ops, SqlDialect::Postgres, &cfg.project_schema, &policy)
            .map_err(|error| format!("fold the PostgreSQL ops: {error}"))?;
        let folded = folded
            .tables
            .get(TABLE)
            .and_then(|table| table.columns.iter().find(|c| c.name == GENERATED_COLUMN))
            .and_then(|column| column.generated.as_ref())
            .map(|generated| generated.expr.clone())
            .ok_or_else(|| "the folded snapshot carries no generated body".to_string())?;

        let fields = single_fold::fold(&ops, SqlDialect::Postgres, &cfg.project_schema, &policy)
            .map(|folded| folded.project_field_defs())
            .map_err(|error| format!("fold the ops to field defs: {error}"))?;
        let descriptor = fields
            .get(TABLE)
            .and_then(|table| table.get(GENERATED_COLUMN))
            .and_then(|field| field.get("generated"))
            .map(ToString::to_string)
            .ok_or_else(|| "the descriptor fold carries no generated body".to_string())?;

        Ok(Measured {
            live,
            folded,
            descriptor,
        })
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_meta_schema} CASCADE"
        ))
        .await;
    match (work, cleanup) {
        (Ok(measured), Ok(())) => Some(measured),
        (Err(work), Ok(())) => panic!("{work}"),
        (Ok(_), Err(cleanup)) => panic!("drop PostgreSQL test schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => panic!("{work}; cleanup failed: {cleanup}"),
    }
}

// Each side is asserted SEPARATELY before the pair is compared. Asserting only that
// the two agree would pass just as well against a measurement that read the same
// value twice.
#[compio::test]
async fn a_rename_leaves_the_folded_generated_body_naming_what_the_server_names() {
    let Some(measured) = measure().await else {
        return;
    };
    let Measured {
        live,
        folded,
        descriptor,
    } = measured;

    assert_eq!(
        named_column(&live),
        Some(NEW_COLUMN),
        "the oracle: PostgreSQL deparses the generated expression from a parse tree \
         over attribute numbers, so it names the POST-rename column: {live}"
    );
    assert_eq!(
        named_column(&folded),
        Some(NEW_COLUMN),
        "the folded snapshot's generated body must name the column the server names, \
         or the SQLite rebuild emits a CREATE over a column the table does not have: \
         {folded}"
    );
    assert_eq!(
        named_column(&descriptor),
        Some(NEW_COLUMN),
        "the descriptor fold names the same column, which it has all along: \
         {descriptor}"
    );
}
