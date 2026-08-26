//! A plan pinned to ONE dialect is refused against every other deploy target,
//! whole-plan, before a single step runs — driven through the REAL lower
//! (`IrAuthor::load_and_lower_guarded`) and the REAL apply
//! (`MigrationEngine::apply_applied_plan_with_touched_and_depends`) against a
//! real temp-file `SQLite` database.
//!
//! # The hole this closes
//!
//! `AppliedPlan::dialect_scope` was a declared safety facet with no producer and
//! no reader: `DialectScope::Only` was constructed nowhere, `admits` had zero
//! callers, and the field was written `Portable` at all five construction sites.
//! `deploy_envelopes` takes the lowering `dialect` and the apply `backend` as two
//! INDEPENDENT parameters and never checks that they agree, so an artifact whose
//! ops only one registered backend can render was applied against any target at
//! all. A `raw` op is the sharp case: its SQL text is written against one server,
//! nothing in the engine can read it, and portable-looking text lands silently on
//! the wrong database.
//!
//! # Why the fixture SQL is portable on purpose
//!
//! `CREATE TABLE main.dialect_scope_probe (id integer not null)` parses and runs on
//! BOTH backends. That is the point: a raw statement that only PostgreSQL
//! understands fails at the database and looks like a refusal, which would let a
//! broken gate pass this test. Portable text makes the un-gated behaviour a clean
//! SUCCESS, so the assertion below can only be satisfied by an actual plan-level
//! refusal rather than by a syntax error.

use crate::support;

use std::collections::BTreeMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zeroship_migrate::apply::executor::LockMode;
use zeroship_migrate::{
    Approval, DialectScope, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, MigrationEngine,
};
use zeroship_migrate_ir::ir::{MigrationIr, Op, CURRENT_IR_VERSION};
use zeroship_migrate_sqlite::SqliteBackend;

const PROJECT: &str = "prj_scope";
/// The project SCHEMA is `main`, and that is load-bearing rather than arbitrary: the
/// guard requires a schema-QUALIFIED name inside scoped `raw` SQL, and `main` is both
/// an ordinary PostgreSQL schema name and SQLite's own name for the primary database.
/// So `main.dialect_scope_probe` is one statement BOTH servers execute — which is what
/// makes the un-gated behaviour a clean success instead of a syntax error.
const SCHEMA: &str = "main";
const APP: &str = "app_scope";
const PROBE_TABLE: &str = "dialect_scope_probe";
const PROBE_SQL: &str = "CREATE TABLE main.dialect_scope_probe (id integer not null)";

/// A `createTable` plus a PARTIAL index whose predicate is a `dialect({ ... })`
/// expression carrying ONE leg. Both op kinds are portable — every registered backend
/// renders them — so the EXPRESSION is the only thing that can narrow the reach here,
/// which is exactly what the assertion needs to be measuring.
const PINNED_EXPR_ENVELOPE: &str = r#"{"ir_version":1,"name":"pinned_expr","owner_app":"app_scope","ops":[
    {"op":"createTable","name":"scope_pinned","columns":[
        {"name":"flag","type":"boolean","nullable":false}]},
    {"op":"createIndex","table":"scope_pinned","columns":[{"kind":"column","name":"flag"}],
     "where":{"node":"dialect","legs":{"postgres":{"node":"colRef","name":"flag"}}}}
]}"#;

/// The out-of-envelope `splitPart` the SQLite renderer refuses — the message whose
/// remedy clause this file holds to the escape that exists.
const OUT_OF_ENVELOPE_ENVELOPE: &str = r#"{"ir_version":1,"name":"oob","owner_app":"app_scope","ops":[
    {"op":"update","table":"t",
     "set":{"x":{"node":"fnSynth","fn":"splitPart","args":[
         {"node":"colRef","name":"v"},{"node":"literal","value":", "},{"node":"literal","value":1}]}}}
],
"irreversible":"probe fixture: the pre-image of the overwritten column is not recorded"}"#;

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths(tag: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{tag}.sqlite"));
    let journal = dir.path().join(format!("zs-{tag}.migrations.sqlite"));
    Paths {
        _dir: dir,
        app,
        journal,
    }
}

fn backend(p: &Paths) -> SqliteBackend {
    SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend")
}

fn raw_envelope() -> MigrationIr {
    MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: CURRENT_IR_VERSION,
        name: "dialect_scope_probe".into(),
        owner_app: APP.into(),
        ops: vec![Op::Raw {
            sql: PROBE_SQL.into(),
            reason: "dialect-scope pinning probe".into(),
        }],
        flags: Default::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    }
}

/// Lower `json` through the REAL guarded load+lower at the PostgreSQL dialect, under
/// the operator charter that grants the `raw` escape.
///
/// PostgreSQL is the lowering dialect throughout this file because it is the only
/// registered backend that renders the privileged vendor family at all: SQLite and
/// MySQL refuse a `raw` op at `lower` with `VendorUnsupported`, so there is no other
/// way to GET a pinned plan to point at a foreign target.
fn lower_on_postgres(json: &str) -> zeroship_migrate::render::lower::LoweredArtifact {
    let charter = support::operator_charter(SCHEMA);
    let author = IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        SCHEMA,
        APP,
        &zeroship_migrate_postgres::DIALECT,
        &charter,
    );
    let guard_cfg = GuardConfig::from_policy(charter, zeroship_migrate_postgres::DIALECT);
    author
        .load_and_lower_guarded(
            json,
            APP,
            &BTreeMap::new(),
            &LiveSchema::default(),
            &guard_cfg,
        )
        .unwrap_or_else(|error| panic!("the fixture envelope must lower on this dialect: {error}"))
}

fn lower_raw_probe_on_postgres() -> zeroship_migrate::render::lower::LoweredArtifact {
    let json = serde_json::to_string(&raw_envelope()).expect("probe envelope serializes");
    lower_on_postgres(&json)
}

fn table_exists(p: &Paths, table: &str) -> bool {
    let conn = rusqlite::Connection::open(&p.app).expect("reopen the probed sqlite file");
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .expect("sqlite_master probe")
        > 0
}

/// THE PRODUCER. An artifact whose ops only one registered backend can render
/// lowers to `DialectScope::Only(that dialect)` — derived from the ops, never
/// authored, so it cannot disagree with them.
#[test]
fn a_vendor_op_pins_the_plan_to_the_one_dialect_that_renders_it() {
    let artifact = lower_raw_probe_on_postgres();
    assert_eq!(
        artifact.plan.dialect_scope,
        DialectScope::Only(zeroship_migrate_postgres::DIALECT),
        "a `raw` op is renderable by exactly one registered backend, so the plan is \
         pinned to it"
    );
}

/// THE PEER. An artifact every registered backend can render stays `Portable`, so
/// the pin above is a measurement of the ops rather than a constant.
#[test]
fn a_portable_artifact_stays_portable() {
    let artifact = lower_on_postgres(
        r#"{"ir_version":1,"name":"portable_notes","owner_app":"app_scope","ops":[
        {"op":"createTable","name":"scope_notes","columns":[
            {"name":"title","type":"text","nullable":false}
        ]}
    ]}"#,
    );
    assert_eq!(
        artifact.plan.dialect_scope,
        DialectScope::Portable,
        "a createTable every backend renders must not be pinned"
    );
}

/// THE OTHER PRODUCER. A single-leg `dialect({ ... })` EXPRESSION pins the plan just
/// as a vendor op does — which is what makes the remedy the SQLite renderer offers
/// (below) a true statement rather than advice about a field nobody can write.
#[test]
fn a_single_leg_dialect_expression_pins_the_plan_too() {
    let artifact = lower_on_postgres(PINNED_EXPR_ENVELOPE);
    assert_eq!(
        artifact.plan.dialect_scope,
        DialectScope::Only(zeroship_migrate_postgres::DIALECT),
        "an expression whose leg set covers ONE registered backend pins the plan to it"
    );
}

/// THE REMEDY IS TRUE. The renderer's refusal used to advise
/// `dialect_scope=PgOnly` — a variant that does not exist, on a facet no author
/// writes. This drives the REAL load gate and reads what an operator is actually
/// told, then holds that text to the escape that exists.
///
/// Pairing it with `a_single_leg_dialect_expression_pins_the_plan_too` is the point:
/// one test says the message names `dialect(`, the other says `dialect(` really does
/// pin. Renaming the escape breaks the first; removing the pinning breaks the second.
#[test]
fn the_out_of_envelope_remedy_names_the_escape_that_exists() {
    let error = zeroship_migrate::model::load::load_ir_document(
        zeroship_migrate::shipping_vendors(),
        OUT_OF_ENVELOPE_ENVELOPE,
        APP,
        &zeroship_migrate_sqlite::DIALECT,
        &BTreeMap::new(),
        None,
    )
    .expect_err("an out-of-envelope splitPart must be refused on this target");
    let text = error.to_string();
    // INSTRUMENT CHECK, and it caught a real one: several of this backend's
    // rejections name `dialect(` and this dialect, so the two assertions below
    // would pass on the WRONG message. This fragment belongs to the out-of-envelope
    // remedy alone, so a green here is a green about the message this test names.
    assert!(
        text.contains("pins the migration to that dialect"),
        "this must be reading the out-of-envelope remedy, not a neighbouring one: {text}"
    );
    assert!(
        text.contains("dialect("),
        "the remedy must name the escape an author can actually reach: {text}"
    );
    assert!(
        !text.contains("dialect_scope") && !text.contains("PgOnly"),
        "the remedy must not advise a field or a variant that does not exist: {text}"
    );
    assert!(
        text.contains(zeroship_migrate_sqlite::DIALECT.as_str()),
        "the refusing backend must name itself from its own DialectId: {text}"
    );
}

/// A plan whose OPS are portable is still a plan ONE backend rendered, and the target
/// it meets must be that backend.
///
/// # Why the reach gate above does not already cover this
///
/// `dialect_scope` measures which backends COULD render these ops. For a bare
/// `createTable` the honest answer is "all of them", so the scope is `Portable` and
/// admits every target — see `a_portable_artifact_stays_portable`, which pins exactly
/// that and is correct about reach.
///
/// What it does not measure is which backend DID render them. The lowering below runs
/// on PostgreSQL, so the SQL in the plan is PostgreSQL's spelling: schema-qualified and
/// double-quoted, `"main"."scope_notes"`, where this same op lowered on SQLite would
/// read `main.scope_notes`. Two different artifacts, and only the reach was ever
/// compared.
///
/// # And it is the SILENT direction
///
/// SQLite accepts double-quoted identifiers and calls its own database `main`, so the
/// PostgreSQL rendering does not fail here — it SUCCEEDS, against a server it was not
/// rendered for. The file header already relies on that property to keep its own
/// control honest ("portable text makes the un-gated behaviour a clean SUCCESS"); this
/// test turns the same property into the thing being checked. A MySQL target would have
/// rejected the double quotes and made the bug loud; SQLite makes it quiet, which is
/// why the assertion belongs here.
#[compio::test]
async fn a_portable_plan_rendered_for_one_backend_is_refused_by_another() {
    let p = paths("rendered_for");
    let be = backend(&p);
    let artifact = lower_on_postgres(
        r#"{"ir_version":1,"name":"portable_notes","owner_app":"app_scope","ops":[
        {"op":"createTable","name":"scope_notes","columns":[
            {"name":"title","type":"text","nullable":false}
        ]}
    ]}"#,
    );

    assert_eq!(
        artifact.plan.dialect_scope,
        DialectScope::Portable,
        "the control's premise: these ops really are portable, so the REACH gate cannot \
         be what refuses this plan and anything that does refuse it is the provenance \
         gate this test is about"
    );

    let result = MigrationEngine::new(zeroship_migrate::shipping_vendors())
        .apply_applied_plan_with_touched_and_depends(
            &artifact.plan,
            &artifact.touched_tables,
            &artifact.depends_on,
            Approval::None,
            &be,
            &ExecutorConfig::new(PROJECT, SCHEMA, support::no_inject(SCHEMA)),
            "tester",
            LockMode::Acquire,
        )
        .await;

    let error = result.expect_err(
        "a plan rendered by PostgreSQL must be refused against a SQLite target, not \
         applied — its SQL is PostgreSQL's spelling and SQLite happens to accept it",
    );
    let text = error.to_string();
    assert!(
        text.contains(zeroship_migrate_postgres::DIALECT.as_str())
            && text.contains(zeroship_migrate_sqlite::DIALECT.as_str()),
        "the refusal must name the backend that RENDERED the plan and the target it met. \
         An error mentioning neither is the database complaining about syntax, which is \
         a different failure and would let a missing gate pass this test: {text}"
    );
    assert!(
        !table_exists(&p, "scope_notes"),
        "the refusal must precede every step: PostgreSQL-rendered DDL executed against a \
         SQLite database"
    );
}

/// THE REFUSAL. The pinned plan meets a real SQLite database and is declined
/// whole-plan: nothing in it executes, and the portable-looking raw statement does
/// NOT land on the wrong server.
#[compio::test]
async fn a_pinned_plan_is_refused_against_a_foreign_live_target() {
    let p = paths("dialect_scope");
    let be = backend(&p);
    let artifact = lower_raw_probe_on_postgres();

    let result = MigrationEngine::new(zeroship_migrate::shipping_vendors())
        .apply_applied_plan_with_touched_and_depends(
            &artifact.plan,
            &artifact.touched_tables,
            &artifact.depends_on,
            Approval::None,
            &be,
            &ExecutorConfig::new(PROJECT, SCHEMA, support::no_inject(SCHEMA)),
            "tester",
            LockMode::Acquire,
        )
        .await;

    let error = result.expect_err(
        "a plan pinned to another dialect must be refused against this target, not applied",
    );
    let text = error.to_string();
    assert!(
        text.contains(zeroship_migrate_postgres::DIALECT.as_str())
            && text.contains(zeroship_migrate_sqlite::DIALECT.as_str()),
        "the refusal must name both the plan's pinned dialect and the target it met: {text}"
    );
    assert!(
        !table_exists(&p, PROBE_TABLE),
        "the refusal must precede every step: the raw statement landed on the wrong database"
    );
}
