//! HIGH #1 (spec §6.0 / §10 PR0 test 8) — the GOLDEN-TRACE ORACLE for the
//! re-pointed declarative path, on REAL Postgres (`:5440`).
//!
//! PR0 re-pointed `apply_declarative_locked` onto the single shared `apply_plan`.
//! The spec's §6.0 argument: once re-pointed, the old path IS the new path, so a
//! self-equivalence test against the live path cannot catch a bug SHARED by old
//! and new code — the diff must be against an INDEPENDENT reference.
//!
//! This file is that independent reference. It contains an `oracle_apply` that
//! reconstructs the PRE-re-point orchestration of `apply_declarative_locked`
//! (transcribed from commit `8b5d9cf6~1`) out of the *public* engine primitives
//! the old body used as execution destinations — `engine.apply` (the gated plain
//! DDL set), `backend.rebuild_one` (each SQLite rebuild), `engine.run_expand`
//! (each PG online rename's EXPAND) — in the historical order plain → rebuilds →
//! renames. It shares NO code with the re-pointed `apply_declarative` (which now
//! lowers to `PlanStep`s and runs them through `apply_plan`). The oracle's output
//! is captured into IMMUTABLE, COMMITTED fixtures under `tests/golden-traces/`
//! (schema-normalized so the bytes are stable across runs), and the test asserts:
//!
//!   1. the oracle reproduces the frozen committed fixture (the fixture is the
//!      pre-re-point capture; a drift in the oracle/fixture is caught here), and
//!   2. the LIVE re-pointed `apply_declarative` reproduces the SAME fixture
//!      byte-for-byte — the cross-path differential.
//!
//! Paths covered (PG leg): (a) online PG rename, (c) destructive-refusal,
//! (d) net-applied-skip, (f) mixed plain+rename, (h) AlreadyHeld re-entrancy.
//! The SQLite-rebuild (b) + empty-renames-fail-closed (g) paths are in
//! `golden_trace_sqlite.rs`.

use std::collections::HashMap;
use std::path::PathBuf;

use compio_postgres::Client;
use zeroship_migrate::{
    desired_snapshot, provision_migrator, apply::role::deprovision_migrator, snapshot_schema, Approval,
    ApprovalScope, CollectionDescriptor, DeclarativeApplyError, DeclarativeAuthor,
    DeclarativeDeployOutcome, DesiredSchema, EngineError, ExecutorConfig, FieldDescriptor,
    GuardConfig, MigrationEngine, RenameHint, SchemaSnapshot,
};

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

fn dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

async fn pg() -> Client {
    let (client, conn) = compio_postgres::connect(&dsn(), compio_postgres::NoTls)
        .await
        .expect("connect to zeroship_migrate_test on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}_{n}")
}

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.pg.meta_schema = format!("meta_{tok}");
    let role = zeroship_migrate::migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}

async fn setup(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{}\"", cfg.project_schema))
        .await
        .expect("create project schema");
    provision_migrator(conn, cfg).await.expect("provision migrator role");
}

async fn teardown(conn: &Client, cfg: &ExecutorConfig) {
    let _ = deprovision_migrator(conn, cfg).await;
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

fn guard(cfg: &ExecutorConfig) -> GuardConfig {
    GuardConfig::confined(cfg.project_schema.clone())
}

fn author(cfg: &ExecutorConfig) -> DeclarativeAuthor {
    DeclarativeAuthor::new(cfg.project_schema.clone(), "app_test")
}

// ---------------------------------------------------------------------------
// Normalized capture: the journal trace BY STEP NAME (schema-independent, unlike
// the schema-derived version ids) + the post-apply SCHEMA fingerprint with the
// per-test schema name normalized to a sentinel. These are the bytes the frozen
// fixtures hold.
// ---------------------------------------------------------------------------

const SCHEMA_SENTINEL: &str = "<PRJ>";

async fn journal_name_trace(conn: &Client, cfg: &ExecutorConfig) -> Vec<String> {
    let rows = conn
        .query(
            &format!(
                "SELECT name, phase, outcome, kind FROM \"{}\".schema_migrations \
                 WHERE event_kind = 'applied' ORDER BY event_seq ASC",
                cfg.pg.meta_schema
            ),
            &[],
        )
        .await
        .expect("query journal trace");
    rows.iter()
        .map(|r| {
            format!(
                "{}|{}|{}|{}",
                r.get::<_, String>(0),
                r.get::<_, String>(1),
                r.get::<_, String>(2),
                r.get::<_, String>(3),
            )
        })
        .collect()
}

async fn schema_fingerprint(conn: &Client, cfg: &ExecutorConfig) -> Vec<String> {
    let rows = conn
        .query(
            "SELECT table_name, column_name, data_type, is_nullable \
             FROM information_schema.columns WHERE table_schema = $1 \
             ORDER BY table_name, ordinal_position",
            &[&cfg.project_schema],
        )
        .await
        .expect("query schema fingerprint");
    rows.iter()
        .map(|r| {
            format!(
                "{}.{} {} {}",
                r.get::<_, String>(0),
                r.get::<_, String>(1),
                r.get::<_, String>(2),
                r.get::<_, String>(3),
            )
        })
        .collect()
}

/// The full normalized capture (trace + schema), one string per line, with the
/// per-test schema name normalized out — the byte-for-byte fixture body.
async fn capture(conn: &Client, cfg: &ExecutorConfig) -> String {
    let mut out = String::new();
    out.push_str("# journal trace (name|phase|outcome|kind, in event order)\n");
    for line in journal_name_trace(conn, cfg).await {
        out.push_str(&line.replace(&cfg.project_schema, SCHEMA_SENTINEL));
        out.push('\n');
    }
    out.push_str("# schema fingerprint (table.column type nullable)\n");
    for line in schema_fingerprint(conn, cfg).await {
        out.push_str(&line.replace(&cfg.project_schema, SCHEMA_SENTINEL));
        out.push('\n');
    }
    out
}

/// Assert the capture matches the frozen committed fixture at
/// `tests/golden-traces/<name>.txt`. If the fixture is ABSENT, write it (the
/// one-time capture) and FAIL loudly so it is reviewed + committed — never
/// silently self-bless on a subsequent run.
fn assert_frozen(name: &str, body: &str) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden-traces");
    std::fs::create_dir_all(&dir).expect("golden-traces dir");
    let path = dir.join(format!("{name}.txt"));
    match std::fs::read_to_string(&path) {
        Ok(frozen) => assert_eq!(
            body, frozen,
            "golden-trace fixture `{name}` drift: the captured trace+schema no longer \
             matches the frozen committed fixture at {}. If this change is intended, \
             review and re-commit the fixture.",
            path.display()
        ),
        Err(_) => {
            std::fs::write(&path, body).expect("write fresh fixture");
            panic!(
                "golden-trace fixture `{name}` was ABSENT — wrote a fresh capture to {}. \
                 Review it and commit it; re-run to assert against the frozen copy.",
                path.display()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The ORACLE — the pre-re-point `apply_declarative_locked` orchestration,
// transcribed from `8b5d9cf6~1` out of PUBLIC engine primitives. Shares NO code
// with the live re-pointed `apply_declarative`.
// ---------------------------------------------------------------------------

/// Build the declarative plan exactly like the live path (the differ is the same
/// upstream; what we are differentiating is the APPLY orchestration, not the diff).
fn plan_decl(
    engine: &MigrationEngine,
    desired: &DesiredSchema,
    live: &SchemaSnapshot,
    cfg: &ExecutorConfig,
) -> zeroship_migrate::DeclarativeDeployPlan {
    engine
        .plan_declarative(desired, live, &HashMap::new(), &author(cfg), &[], &guard(cfg))
        .expect("plan_declarative")
}

/// The pre-re-point orchestration: plain set (gated `apply`) → each rebuild
/// (gate + `rebuild_one`) → each rename's EXPAND (`run_expand`), collecting the
/// contract — in that historical order. Built from the public primitives the old
/// `apply_declarative_locked` body used as destinations.
async fn oracle_apply(
    engine: &MigrationEngine,
    plan: &zeroship_migrate::DeclarativeDeployPlan,
    approval: Approval,
    conn: &Client,
    cfg: &ExecutorConfig,
) -> Result<DeclarativeDeployOutcome, DeclarativeApplyError> {
    use zeroship_migrate::apply::backend::MigrationBackend;
    // Pre-re-point gate (transcribed): a denied plain plan never applies; a
    // destructive plain plan needs approval.
    if !plan.plain.denied.is_empty() {
        return Err(DeclarativeApplyError::Plain(EngineError::Denied(plan.plain.denied.clone())));
    }
    if plan.plain.requires_approval && approval != Approval::Approved {
        return Err(DeclarativeApplyError::Plain(EngineError::ApprovalRequired));
    }
    let backend = zeroship_migrate::PostgresBackend::new(conn);
    // 1. The plain set, through the gated apply.
    let mut applied = engine
        .apply(&plan.plain, approval, &backend, cfg, "app_test")
        .await
        .map_err(DeclarativeApplyError::Plain)?;
    // 2. Each SQLite rebuild (empty on PG) — gate then rebuild_one.
    if !plan.rebuilds.is_empty() && approval != Approval::Approved {
        return Err(DeclarativeApplyError::Plain(EngineError::ApprovalRequired));
    }
    for rebuild in &plan.rebuilds {
        backend
            .rebuild_one(&rebuild.spec, &rebuild.migration, &ApprovalScope::All, "app_test")
            .await
            .map_err(|e| DeclarativeApplyError::Plain(EngineError::Apply(e)))?;
        applied.applied.push(rebuild.migration.version.as_str().to_string());
    }
    // 3. Each rename's EXPAND, collecting the deferred contract.
    let mut pending_contract = Vec::new();
    for rename in &plan.renames {
        let outcome = engine
            .run_expand(rename, approval, conn, cfg, "app_test")
            .await?;
        applied.applied.extend(outcome.applied);
        applied.skipped.extend(outcome.skipped);
        applied.recovered.extend(outcome.recovered);
        pending_contract.extend(rename.contract.iter().cloned());
    }
    Ok(DeclarativeDeployOutcome { applied, pending_contract, opened_obligations: Vec::new() })
}

/// The LIVE re-pointed path.
async fn live_apply(
    engine: &MigrationEngine,
    plan: &zeroship_migrate::DeclarativeDeployPlan,
    approval: Approval,
    conn: &Client,
    cfg: &ExecutorConfig,
) -> Result<DeclarativeDeployOutcome, DeclarativeApplyError> {
    engine
        .apply_declarative(plan, approval, &zeroship_migrate::PostgresBackend::new(conn), cfg, "app_test")
        .await
}

// ===========================================================================
// (d) NET-APPLIED-SKIP — an additive create, then an idempotent re-apply.
// (a) ONLINE PG RENAME — expand + deferred contract.
// (f) MIXED — a plain add-column alongside an online rename in one deploy.
// (h) Re-entrancy is exercised implicitly: every path runs the WHOLE deploy under
//     one held lock (the live `apply_declarative` acquires once + threads
//     AlreadyHeld; a divergence in that re-entrancy would change the journal).
// ===========================================================================

/// Drive ONE desired-schema deploy through BOTH the oracle and the live path on
/// two fresh schemas, assert each reproduces the frozen fixture, and assert the
/// two normalized captures are equal (the cross-path differential).
async fn differential(
    fixture: &str,
    conn: &Client,
    build_desired: impl Fn(&ExecutorConfig) -> DesiredSchema,
    approval: Approval,
) {
    let engine = MigrationEngine::new();

    // Oracle leg.
    let cfg_o = cfg_for(&token());
    setup(conn, &cfg_o).await;
    let desired_o = build_desired(&cfg_o);
    let plan_o = plan_decl(&engine, &desired_o, &SchemaSnapshot::default(), &cfg_o);
    oracle_apply(&engine, &plan_o, approval, conn, &cfg_o)
        .await
        .expect("oracle apply");
    let cap_o = capture(conn, &cfg_o).await;
    teardown(conn, &cfg_o).await;

    // Live leg.
    let cfg_l = cfg_for(&token());
    setup(conn, &cfg_l).await;
    let desired_l = build_desired(&cfg_l);
    let plan_l = plan_decl(&engine, &desired_l, &SchemaSnapshot::default(), &cfg_l);
    live_apply(&engine, &plan_l, approval, conn, &cfg_l)
        .await
        .expect("live apply");
    let cap_l = capture(conn, &cfg_l).await;
    teardown(conn, &cfg_l).await;

    // Cross-path: the live re-pointed path matches the independent oracle.
    assert_eq!(
        cap_o, cap_l,
        "fixture `{fixture}`: the live re-pointed apply_declarative diverged from the \
         independent pre-re-point oracle"
    );
    // And both match the frozen committed fixture.
    assert_frozen(fixture, &cap_o);
}

/// A `(id implicit) + email:string` table descriptor (system fields injected by
/// `desired_snapshot`).
fn tbl(name: &str) -> CollectionDescriptor {
    CollectionDescriptor {
        name: name.to_string(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "email".into(),
            ty: "string".into(),
            required: false,
            unique: false,
            references: None,
            ..Default::default()
        }],
        indexes: vec![],
    }
}

/// The same table with `email` renamed to `email_address`.
fn tbl_renamed(name: &str) -> CollectionDescriptor {
    CollectionDescriptor {
        name: name.to_string(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "email_address".into(),
            ty: "string".into(),
            required: false,
            unique: false,
            references: None,
            ..Default::default()
        }],
        indexes: vec![],
    }
}

/// A table with NO `email` (the drop target for the destructive-refusal path).
fn tbl_no_email(name: &str) -> CollectionDescriptor {
    CollectionDescriptor {
        name: name.to_string(),
        owner_app: "app_test".into(),
        fields: vec![],
        indexes: vec![],
    }
}

fn desired_of(cfg: &ExecutorConfig, descs: &[CollectionDescriptor]) -> DesiredSchema {
    desired_snapshot(&cfg.project_schema, descs).expect("desired_snapshot")
}

#[compio::test]
async fn golden_d_net_applied_skip() {
    let conn = pg().await;
    // (d) An additive create — the oracle and the live path journal the same one
    // create; the re-apply (same plan against the now-live schema) is a net skip.
    differential(
        "pg_d_net_applied_skip",
        &conn,
        |cfg| desired_of(cfg, &[tbl("users")]),
        Approval::None,
    )
    .await;
}

#[compio::test]
async fn golden_a_online_rename_and_f_mixed() {
    let conn = pg().await;
    // First materialize `users(id, email)`, snapshot it, then desire
    // `users(id, email_address)` + a new `notes(id, email)` table — a MIXED deploy
    // (a plain create alongside an online rename email → email_address), path (a)+(f).
    let engine = MigrationEngine::new();

    // Oracle leg.
    let cfg_o = cfg_for(&token());
    setup(&conn, &cfg_o).await;
    let base_o = desired_of(&cfg_o, &[tbl("users")]);
    let p0 = plan_decl(&engine, &base_o, &SchemaSnapshot::default(), &cfg_o);
    oracle_apply(&engine, &p0, Approval::None, &conn, &cfg_o).await.expect("base");
    let live0 = snapshot_schema(&conn, &cfg_o.project_schema).await.expect("snap");
    let desired_o = mixed_desired(&cfg_o);
    let plan_o = engine
        .plan_declarative(
            &desired_o,
            &live0,
            &HashMap::new(),
            &author(&cfg_o),
            &[rename_hint()],
            &guard(&cfg_o),
        )
        .expect("plan mixed");
    oracle_apply(&engine, &plan_o, Approval::Approved, &conn, &cfg_o)
        .await
        .expect("oracle mixed");
    let cap_o = capture(&conn, &cfg_o).await;
    teardown(&conn, &cfg_o).await;

    // Live leg.
    let cfg_l = cfg_for(&token());
    setup(&conn, &cfg_l).await;
    let base_l = desired_of(&cfg_l, &[tbl("users")]);
    let p0l = plan_decl(&engine, &base_l, &SchemaSnapshot::default(), &cfg_l);
    live_apply(&engine, &p0l, Approval::None, &conn, &cfg_l).await.expect("base");
    let live0l = snapshot_schema(&conn, &cfg_l.project_schema).await.expect("snap");
    let desired_l = mixed_desired(&cfg_l);
    let plan_l = engine
        .plan_declarative(
            &desired_l,
            &live0l,
            &HashMap::new(),
            &author(&cfg_l),
            &[rename_hint()],
            &guard(&cfg_l),
        )
        .expect("plan mixed");
    live_apply(&engine, &plan_l, Approval::Approved, &conn, &cfg_l)
        .await
        .expect("live mixed");
    let cap_l = capture(&conn, &cfg_l).await;
    teardown(&conn, &cfg_l).await;

    assert_eq!(
        cap_o, cap_l,
        "pg_a_f_mixed: the live re-pointed apply_declarative diverged from the oracle"
    );
    assert_frozen("pg_a_f_mixed", &cap_o);
}

fn mixed_desired(cfg: &ExecutorConfig) -> DesiredSchema {
    // users renamed email → email_address, alongside a brand-new plain `notes`.
    desired_of(cfg, &[tbl_renamed("users"), tbl("notes")])
}

fn rename_hint() -> RenameHint {
    RenameHint { table: "users".into(), from: "email".into(), to: "email_address".into() }
}

// ===========================================================================
// (g-family / LOW #4) RENAME-ONLY deploy — an EMPTY plain set. Post-PR0 the
// coalesce loop skips the initial DDL-batch session-hygiene cycle when no Ddl step
// exists (the documented state-neutral simplification). This freezes the
// rename-only journal + schema trace byte-identical to the independent oracle, so
// the empty-plain path is covered by EVIDENCE (a frozen fixture), not by argument.
// ===========================================================================

#[compio::test]
async fn golden_g_rename_only_empty_plain() {
    let conn = pg().await;
    let engine = MigrationEngine::new();

    async fn run(engine: &MigrationEngine, conn: &Client, leg: &str) -> String {
        let cfg = cfg_for(&token());
        setup(conn, &cfg).await;
        // Materialize users(id, email) via the SAME leg.
        let base = desired_of(&cfg, &[tbl("users")]);
        let p0 = plan_decl(engine, &base, &SchemaSnapshot::default(), &cfg);
        if leg == "oracle" {
            oracle_apply(engine, &p0, Approval::None, conn, &cfg).await.expect("base");
        } else {
            live_apply(engine, &p0, Approval::None, conn, &cfg).await.expect("base");
        }
        let live0 = snapshot_schema(conn, &cfg.project_schema).await.expect("snap");
        // Desire ONLY the rename — no new tables, so the plain set is EMPTY and the
        // deploy is the online rename alone.
        let desired = desired_of(&cfg, &[tbl_renamed("users")]);
        let plan = engine
            .plan_declarative(&desired, &live0, &HashMap::new(), &author(&cfg), &[rename_hint()], &guard(&cfg))
            .expect("plan rename-only");
        assert!(plan.plain.items.is_empty(), "{leg}: the rename-only deploy has an EMPTY plain set");
        assert_eq!(plan.renames.len(), 1, "{leg}: exactly the one online rename");
        if leg == "oracle" {
            oracle_apply(engine, &plan, Approval::Approved, conn, &cfg).await.expect("rename-only");
        } else {
            live_apply(engine, &plan, Approval::Approved, conn, &cfg).await.expect("rename-only");
        }
        let cap = capture(conn, &cfg).await;
        teardown(conn, &cfg).await;
        cap
    }

    let cap_o = run(&engine, &conn, "oracle").await;
    let cap_l = run(&engine, &conn, "live").await;
    assert_eq!(
        cap_o, cap_l,
        "pg_g_rename_only: the live re-pointed (empty-plain) path diverged from the oracle"
    );
    assert_frozen("pg_g_rename_only_empty_plain", &cap_o);
}

// ===========================================================================
// (c) DESTRUCTIVE-REFUSAL — a drop without approval applies NOTHING, both paths.
// ===========================================================================

#[compio::test]
async fn golden_c_destructive_refusal() {
    let conn = pg().await;
    let engine = MigrationEngine::new();

    // Build `users(id, email)`, then desire `users(id)` (drop email) WITHOUT
    // approval. Both the oracle and the live path must REFUSE and journal nothing
    // beyond the base create.
    for (leg, fixture_writer) in [("oracle", true), ("live", false)] {
        let cfg = cfg_for(&token());
        setup(&conn, &cfg).await;
        let base = desired_of(&cfg, &[tbl("users")]);
        let p0 = plan_decl(&engine, &base, &SchemaSnapshot::default(), &cfg);
        // Apply the base via the SAME leg so the journal up to the refusal matches.
        if leg == "oracle" {
            oracle_apply(&engine, &p0, Approval::None, &conn, &cfg).await.expect("base");
        } else {
            live_apply(&engine, &p0, Approval::None, &conn, &cfg).await.expect("base");
        }
        let live0 = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
        // Desire dropping `email` — a destructive change; pass the live ownership so
        // the drop is authored (not fail-closed), then refuse on approval.
        let mut ownership = HashMap::new();
        ownership.insert("users".to_string(), "app_test".to_string());
        let desired = desired_of(&cfg, &[tbl_no_email("users")]);
        let plan = engine
            .plan_declarative(&desired, &live0, &ownership, &author(&cfg), &[], &guard(&cfg))
            .expect("plan drop");
        let refused = if leg == "oracle" {
            oracle_apply(&engine, &plan, Approval::None, &conn, &cfg).await
        } else {
            live_apply(&engine, &plan, Approval::None, &conn, &cfg).await
        };
        assert!(
            matches!(refused, Err(DeclarativeApplyError::Plain(EngineError::ApprovalRequired))),
            "{leg}: a destructive drop without approval must be refused, got {refused:?}"
        );
        let cap = capture(&conn, &cfg).await;
        teardown(&conn, &cfg).await;
        if fixture_writer {
            assert_frozen("pg_c_destructive_refusal", &cap);
        } else {
            // The live capture must match the frozen oracle capture too.
            assert_frozen("pg_c_destructive_refusal", &cap);
        }
    }
}
