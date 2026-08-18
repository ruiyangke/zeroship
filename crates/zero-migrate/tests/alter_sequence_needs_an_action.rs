//! An `alterSequence` that asks for nothing must be refused before any SQL exists.
//!
//! `ALTER SEQUENCE <name>` with no action clause is not a statement in
//! PostgreSQL's grammar. MEASURED against PostgreSQL 18.4 through the engine's own
//! emitted SQL, the server answers `syntax error at end of input` and the whole
//! migration dies partway through applying.
//!
//! Before the gate below existed, nothing in the engine noticed. The only layer
//! that objected was the SQL guard's PARSER - a security belt that exists to stop
//! host-reaching SQL, not to spell-check the engine's own output - so an operator
//! running with a Trusted posture, or any embedder calling `lower_plan` directly,
//! got the raw unparseable statement.
//!
//! This file drives the UNGUARDED lowering path deliberately, because that is the
//! path with no parser in front of it: if the refusal holds there, it holds
//! everywhere.
//!
//! The over-refusal controls matter as much as the refusal. `restart: null` is a
//! bare `RESTART`, `minValue: null` is `NO MINVALUE` and `ownedBy: null` is
//! `OWNED BY NONE` - all three are PRESENT options carrying a null payload, and
//! all three are real actions. A gate that tested the inner value rather than the
//! option's presence would refuse three legal migrations, so each one is applied
//! against the live server here.

mod support;

use std::collections::BTreeMap;

use serde_json::json;
use support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::apply::executor::LockMode;
use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::Op;
use zero_migrate::{
    fold_ops, Approval, ExecutorConfig, IrAuthor, LiveSchema, MigrationEngine, PostgresBackend,
    SqlDialect,
};

const OWNER: &str = "app_alter_sequence_needs_an_action";
const SEQ: &str = "s";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "alterseqaction_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn envelope(name: &str, op: serde_json::Value) -> String {
    json!({ "ir_version": 1, "name": name, "owner_app": OWNER, "ops": [op] }).to_string()
}

fn create_doc() -> String {
    envelope(
        "create_s",
        json!({ "op": "createSequence", "name": SEQ, "as": "bigInt",
                "increment": 1, "start": 1, "cache": 1 }),
    )
}

/// Lower one envelope through the UNGUARDED author path and apply it.
///
/// Unguarded on purpose: `load_and_lower_guarded` puts the SQL guard's parser in
/// front of the emitted text, and the whole point of this file is that the parser
/// must not be the layer that notices.
async fn apply_doc(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    ir: &str,
    history: &mut Vec<Op>,
) -> Result<Vec<String>, String> {
    let backend = PostgresBackend::new_generic(session);
    let policy = support::no_inject(&cfg.project_schema);
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, &policy);
    let document = zero_migrate::model::load::load_ir_document(
        ir,
        OWNER,
        zero_migrate::model::validate::Dialect::Postgres,
        &BTreeMap::new(),
        None,
    )
    .map_err(|error| format!("load gate (postgres): {error}"))?;
    let folded = fold_ops(history, SqlDialect::Postgres, &cfg.project_schema, &policy)
        .map_err(|error| format!("fold the applied history: {error}"))?;
    let live = LiveSchema::from_catalog_snapshot(folded, OWNER);
    let plan = author
        .lower_plan(&document, &live)
        .map_err(|error| format!("lower the doc plan on PostgreSQL: {error}"))?;
    let emitted: Vec<String> = plan
        .steps
        .iter()
        .filter_map(|step| match step {
            zero_migrate::render::step::PlanStep::Ddl(migration) => Some(migration.up.clone()),
            _ => None,
        })
        .collect();
    MigrationEngine::new()
        .apply_plan(
            &plan.steps,
            Approval::Approved,
            &backend,
            cfg,
            OWNER,
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("apply the authored plan on PostgreSQL: {error}"))?;
    history.extend(document.ops.iter().cloned());
    Ok(emitted)
}

/// Lower one envelope through the UNGUARDED author path WITHOUT applying it, and
/// return either the emitted SQL or the engine's refusal.
fn lower_only(
    cfg: &ExecutorConfig,
    ir: &str,
    history: &[Op],
) -> Result<Result<Vec<String>, String>, String> {
    let policy = support::no_inject(&cfg.project_schema);
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, &policy);
    let document = zero_migrate::model::load::load_ir_document(
        ir,
        OWNER,
        zero_migrate::model::validate::Dialect::Postgres,
        &BTreeMap::new(),
        None,
    )
    .map_err(|error| format!("load gate (postgres): {error}"))?;
    let folded = fold_ops(history, SqlDialect::Postgres, &cfg.project_schema, &policy)
        .map_err(|error| format!("fold the applied history: {error}"))?;
    let live = LiveSchema::from_catalog_snapshot(folded, OWNER);
    Ok(match author.lower_plan(&document, &live) {
        Ok(plan) => Ok(plan
            .steps
            .iter()
            .filter_map(|step| match step {
                zero_migrate::render::step::PlanStep::Ddl(migration) => Some(migration.up.clone()),
                _ => None,
            })
            .collect()),
        Err(error) => Err(error.to_string()),
    })
}

async fn sequence_increment(session: &PgDevSession, schema: &str) -> Result<i64, String> {
    session
        .query_one(
            "SELECT seqincrement AS value FROM pg_sequence \
             WHERE seqrelid = format('%I.%I', $1::text, $2::text)::regclass",
            &[schema.into(), SEQ.into()],
        )
        .await
        .map_err(|error| format!("read the live sequence increment: {error}"))?
        .try_get("value")
        .map_err(|error| format!("decode the live sequence increment: {error}"))
}

async fn sequence_min_value(session: &PgDevSession, schema: &str) -> Result<i64, String> {
    session
        .query_one(
            "SELECT seqmin AS value FROM pg_sequence \
             WHERE seqrelid = format('%I.%I', $1::text, $2::text)::regclass",
            &[schema.into(), SEQ.into()],
        )
        .await
        .map_err(|error| format!("read the live sequence minimum: {error}"))?
        .try_get("value")
        .map_err(|error| format!("decode the live sequence minimum: {error}"))
}

#[compio::test]
async fn an_option_less_alter_sequence_is_refused_before_any_sql_exists() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token();
    let cfg = ExecutorConfig::new(
        format!("project_{schema}"),
        &schema,
        support::no_inject(&schema),
    );
    let quoted_schema = quote_ident(&cfg.project_schema);
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated test schema");

    let work: Result<(), String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;

        let mut history: Vec<Op> = Vec::new();
        apply_doc(&session, &cfg, &create_doc(), &mut history).await?;

        // THE DEFECT. Every option absent, so there is no action to render.
        let bare = envelope(
            "alter_s_bare",
            json!({ "op": "alterSequence", "name": SEQ }),
        );
        match lower_only(&cfg, &bare, &history)? {
            Ok(emitted) => {
                return Err(format!(
                    "an alterSequence with no options must be refused by the engine, but it \
                     lowered to {emitted:?} - a statement with no action clause, which \
                     PostgreSQL answers with `syntax error at end of input`"
                ))
            }
            Err(refusal) => {
                if !refusal.contains("no action") {
                    return Err(format!(
                        "the refusal must say the op names no action; got: {refusal}"
                    ));
                }
                if !refusal.contains(SEQ) {
                    return Err(format!(
                        "the refusal must name the sequence {SEQ:?}; got: {refusal}"
                    ));
                }
            }
        }

        // OVER-REFUSAL CONTROL 1 - an ordinary option still applies.
        let emitted = apply_doc(
            &session,
            &cfg,
            &envelope(
                "alter_s_increment",
                json!({ "op": "alterSequence", "name": SEQ, "increment": 4 }),
            ),
            &mut history,
        )
        .await?;
        if !emitted.iter().any(|sql| sql.contains("INCREMENT BY 4")) {
            return Err(format!("the increment must reach the SQL; got {emitted:?}"));
        }
        let increment = sequence_increment(&session, &cfg.project_schema).await?;
        if increment != 4 {
            return Err(format!("the live increment must be 4, got {increment}"));
        }

        // OVER-REFUSAL CONTROL 2 - `restart: null` is a bare RESTART. The option is
        // PRESENT and its payload is null, which is a real action, not an absence.
        let emitted = apply_doc(
            &session,
            &cfg,
            &envelope(
                "alter_s_restart",
                json!({ "op": "alterSequence", "name": SEQ, "restart": null }),
            ),
            &mut history,
        )
        .await?;
        if !emitted.iter().any(|sql| sql.contains(" RESTART")) {
            return Err(format!(
                "the bare RESTART must reach the SQL; got {emitted:?}"
            ));
        }

        // OVER-REFUSAL CONTROL 3 - `minValue: null` is NO MINVALUE, likewise a
        // present option with a null payload.
        let emitted = apply_doc(
            &session,
            &cfg,
            &envelope(
                "alter_s_no_minvalue",
                json!({ "op": "alterSequence", "name": SEQ, "minValue": null }),
            ),
            &mut history,
        )
        .await?;
        if !emitted.iter().any(|sql| sql.contains("NO MINVALUE")) {
            return Err(format!("NO MINVALUE must reach the SQL; got {emitted:?}"));
        }
        let min_value = sequence_min_value(&session, &cfg.project_schema).await?;
        if min_value != 1 {
            return Err(format!(
                "NO MINVALUE must restore the bigint default of 1, got {min_value}"
            ));
        }

        Ok(())
    }
    .await;

    session
        .batch(&format!("DROP SCHEMA IF EXISTS {quoted_schema} CASCADE"))
        .await
        .expect("drop the isolated test schema");
    work.expect("the option-less alterSequence gate and its three controls");
}
