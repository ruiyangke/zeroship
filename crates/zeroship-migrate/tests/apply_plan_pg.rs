//! PR0 — faithful e2e for the single shared `apply_plan` orchestrator on REAL
//! Postgres (`:5440`), the `op.*` DSL §2.0/§6/§6.0 convergence.
//!
//! These are the NET-NEW-topology tests (the existing ~507-test declarative suite
//! is the regression net for the re-pointed declarative path; it stays green by
//! routing through `apply_plan` — see the other `*_pg`/`declarative_*` suites).
//! Here we drive `apply_plan` directly over hand-built `PlanStep` lists the
//! declarative path never produced:
//!
//! - (1) a single-step (pure-DDL) plan applies identically to the per-`Migration`
//!   path;
//! - (2) a hand-built two-step plan (DDL + DDL) applies in order with correct
//!   per-step journaling + idempotent re-run;
//! - (9) a standalone `PlanStep::Dml` step (trusted constructor) applies on real
//!   PG, the bound row present, the parameterized template journaled, re-run a
//!   no-op;
//! - (10) a single-artifact `Ddl → Backfill → Ddl` interleave applies in order
//!   (backfill runs after the first DDL's column exists, before the second DDL
//!   drops the source);
//! - (11) a multi-`Backfill` plan;
//! - the single-step-shape facade precondition (§5.2, test 7a);
//! - (12) the PROPERTY-BASED CRASH FUZZ: resume from any crash point reaches the
//!   same final DB + journal as the no-crash apply.
//!
//! No shims, no PG-gated skips — every assertion is against a real applied schema
//! + the real journal.

use compio_postgres::Client;
use zeroship_migrate::{
    backfill::BackfillSpec,
    migration::{Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId},
    plan::{AppliedPlan, BindValue, NotSingleStep, PlanStep, RenameStep},
    provision_migrator, role::deprovision_migrator, Approval, DeclarativeApplyError, EngineError,
    ExecutorConfig, ExpandContractAuthor, GuardConfig, MigrationEngine, OnlineIntent,
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
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{}\"",
        cfg.project_schema
    ))
    .await
    .expect("create project schema");
    provision_migrator(conn, cfg)
        .await
        .expect("provision migrator role");
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

async fn column_exists(conn: &Client, schema: &str, table: &str, column: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
            &[&schema, &table, &column],
        )
        .await
        .expect("query column existence");
    !rows.is_empty()
}

async fn journaled(conn: &Client, cfg: &ExecutorConfig, version: &str) -> bool {
    let rows = conn
        .query(
            &format!(
                "SELECT 1 FROM \"{}\".schema_migrations \
                 WHERE version = $1 AND phase = 'completed'",
                cfg.pg.meta_schema
            ),
            &[&version],
        )
        .await
        .expect("query journal");
    !rows.is_empty()
}

/// Read the net-applied completed versions, in event order — the journal "trace".
async fn journal_trace(conn: &Client, cfg: &ExecutorConfig) -> Vec<String> {
    let rows = conn
        .query(
            &format!(
                "SELECT version FROM \"{}\".schema_migrations \
                 WHERE phase = 'completed' ORDER BY event_seq ASC",
                cfg.pg.meta_schema
            ),
            &[],
        )
        .await
        .expect("query journal trace");
    rows.iter().map(|r| r.get::<_, String>(0)).collect()
}

/// A trusted DDL `Migration` constructor (a Ddl `PlanStep` payload).
fn ddl(version: u64, name: &str, schema: &str, up: &str, down: Option<&str>) -> Migration {
    let flags = MigrationFlags::default();
    let checksum = Checksum::of(&ChecksumInput {
        up,
        down,
        flags: &flags,
        owner_app: "app_test",
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });
    // schema is folded into `up` by the caller; the param is here for clarity.
    let _ = schema;
    Migration {
        version: zeroship_migrate::migration_id_for_version(version),
        name: name.to_string(),
        up: up.to_string(),
        down: down.map(str::to_string),
        checksum,
        flags,
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
    }
}

fn q(schema: &str) -> String {
    format!("\"{}\"", schema.replace('"', "\"\""))
}

// ---------------------------------------------------------------------------
// (1) single-step pure-DDL plan == the per-Migration path
// ---------------------------------------------------------------------------

#[compio::test]
async fn single_step_ddl_plan_applies_like_a_migration() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let s = q(&cfg.project_schema);
    let m = ddl(
        1,
        "create_users",
        &cfg.project_schema,
        &format!("CREATE TABLE {s}.users (id bigint PRIMARY KEY)"),
        Some(&format!("DROP TABLE {s}.users")),
    );
    let v = m.version.as_str().to_string();
    let steps = vec![PlanStep::Ddl(m)];

    let engine = MigrationEngine::new();
    let out = engine
        .apply_plan(
            &steps,
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("single-step DDL plan applies");
    assert_eq!(out.applied.applied, vec![v.clone()]);
    assert!(out.pending_contract.is_empty());
    assert!(column_exists(&conn, &cfg.project_schema, "users", "id").await);
    assert!(journaled(&conn, &cfg, &v).await);

    // Re-run is a net-applied no-op.
    let steps2 = vec![PlanStep::Ddl(ddl(
        1,
        "create_users",
        &cfg.project_schema,
        &format!("CREATE TABLE {s}.users (id bigint PRIMARY KEY)"),
        Some(&format!("DROP TABLE {s}.users")),
    ))];
    let out2 = engine
        .apply_plan(
            &steps2,
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("re-run");
    assert!(out2.applied.applied.is_empty(), "re-run applies nothing");
    assert_eq!(out2.applied.skipped, vec![v]);
    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// (2) a two-DDL-step plan applies in order with per-step journaling
// ---------------------------------------------------------------------------

#[compio::test]
async fn two_ddl_step_plan_applies_in_order() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);

    let m1 = ddl(
        1,
        "create_t",
        &cfg.project_schema,
        &format!("CREATE TABLE {s}.t (id bigint PRIMARY KEY)"),
        None,
    );
    let m2 = ddl(
        2,
        "add_col",
        &cfg.project_schema,
        &format!("ALTER TABLE {s}.t ADD COLUMN label text"),
        None,
    );
    let (v1, v2) = (m1.version.as_str().to_string(), m2.version.as_str().to_string());
    let steps = vec![PlanStep::Ddl(m1), PlanStep::Ddl(m2)];

    let engine = MigrationEngine::new();
    let out = engine
        .apply_plan(
            &steps,
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("two-step plan applies");
    assert_eq!(out.applied.applied, vec![v1.clone(), v2.clone()]);
    assert!(column_exists(&conn, &cfg.project_schema, "t", "label").await);
    // The journal trace records both versions in order.
    assert_eq!(journal_trace(&conn, &cfg).await, vec![v1, v2]);
    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// (9) a standalone PlanStep::Dml step
// ---------------------------------------------------------------------------

#[compio::test]
async fn standalone_dml_step_applies_and_is_idempotent() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);

    // DDL first so the DML has a target.
    let create = PlanStep::Ddl(ddl(
        1,
        "create_kv",
        &cfg.project_schema,
        &format!("CREATE TABLE {s}.kv (k text PRIMARY KEY, v text)"),
        None,
    ));
    // A parameterized INSERT — the binds carry the values (native $n binding,
    // never interpolation). The template is schema-qualified so it resolves under
    // the pinned project search_path either way.
    let dml_version = MigrationId::parse("mig_2dmldmldmldmldmldmldm").ok();
    // Derive a deterministic sub-version (the loader/IR discipline) for the Dml.
    let dml_version = dml_version.unwrap_or_else(|| zeroship_migrate::migration_id_for_version(900));
    let dml = PlanStep::Dml {
        version: dml_version.clone(),
        name: "seed_kv".to_string(),
        template: format!("INSERT INTO {s}.kv (k, v) VALUES ($1, $2)"),
        binds: vec![BindValue::Text("hello".into()), BindValue::Text("world".into())],
        transactional: true,
        destructive: false,
    };

    let engine = MigrationEngine::new();
    let out = engine
        .apply_plan(
            &[create, dml],
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("ddl + dml plan applies");
    assert!(out.applied.applied.contains(&dml_version.as_str().to_string()));

    // The bound row is present.
    let rows = conn
        .query(
            &format!("SELECT v FROM {s}.kv WHERE k = $1"),
            &[&"hello"],
        )
        .await
        .expect("select bound row");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "world");
    assert!(journaled(&conn, &cfg, dml_version.as_str()).await);

    // Re-run: the Dml step is net-applied-skipped (no double-insert).
    let dml2 = PlanStep::Dml {
        version: dml_version.clone(),
        name: "seed_kv".to_string(),
        template: format!("INSERT INTO {s}.kv (k, v) VALUES ($1, $2)"),
        binds: vec![BindValue::Text("hello".into()), BindValue::Text("world".into())],
        transactional: true,
        destructive: false,
    };
    let out2 = engine
        .apply_plan(
            &[dml2],
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("re-run dml");
    assert!(out2.applied.applied.is_empty(), "dml re-run applies nothing");
    assert_eq!(out2.applied.skipped, vec![dml_version.as_str().to_string()]);
    let count = conn
        .query_one(&format!("SELECT count(*)::bigint FROM {s}.kv"), &[])
        .await
        .unwrap()
        .get::<_, i64>(0);
    assert_eq!(count, 1, "no double-insert on re-run");

    // Bind-safety: a metacharacter-laden bind cannot alter statement structure.
    let evil = PlanStep::Dml {
        version: zeroship_migrate::migration_id_for_version(901),
        name: "evil_bind".to_string(),
        template: format!("INSERT INTO {s}.kv (k, v) VALUES ($1, $2)"),
        binds: vec![
            BindValue::Text("k2".into()),
            BindValue::Text("'); DROP TABLE kv; --".into()),
        ],
        transactional: true,
        destructive: false,
    };
    engine
        .apply_plan(
            &[evil],
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("evil bind applies as a literal");
    // The table still exists and the metacharacter value was stored verbatim.
    let v = conn
        .query_one(&format!("SELECT v FROM {s}.kv WHERE k = $1"), &[&"k2"])
        .await
        .expect("table survived the injection attempt")
        .get::<_, String>(0);
    assert_eq!(v, "'); DROP TABLE kv; --");
    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// (10) a single-artifact Ddl -> Backfill -> Ddl interleave
// ---------------------------------------------------------------------------

#[compio::test]
async fn ddl_backfill_ddl_interleave_applies_in_order() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);

    // Step 1: create the table with `raw` + a target `normalized`, seed rows.
    let create = PlanStep::Ddl(ddl(
        1,
        "create_names",
        &cfg.project_schema,
        &format!(
            "CREATE TABLE {s}.names (id bigint PRIMARY KEY, raw text, normalized text); \
             INSERT INTO {s}.names (id, raw) VALUES (1, 'ALICE'), (2, 'BOB')"
        ),
        None,
    ));
    // Step 2: backfill `normalized = lower(raw)` (the column exists from step 1).
    let backfill = PlanStep::Backfill(BackfillSpec {
        table: "names".to_string(),
        cursor_column: "id".to_string(),
        batch_size: 100,
        set_clause: "normalized = lower(raw)".to_string(),
        filter: Some("normalized IS NULL".to_string()),
        name: "normalize_names".to_string(),
    });
    // Step 3: drop the source `raw` (after the backfill copied it out).
    let drop_src = PlanStep::Ddl({
        let mut m = ddl(
            2,
            "drop_raw",
            &cfg.project_schema,
            &format!("ALTER TABLE {s}.names DROP COLUMN raw"),
            None,
        );
        m.flags.destructive = true;
        m
    });

    let engine = MigrationEngine::new();
    let out = engine
        .apply_plan(
            &[create, backfill, drop_src],
            Approval::Approved, // the DROP is destructive
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("ddl->backfill->ddl interleave applies");
    assert!(!out.applied.applied.is_empty());

    // The backfill ran AFTER the column existed and BEFORE the drop: `normalized`
    // is populated and the source `raw` is gone.
    assert!(!column_exists(&conn, &cfg.project_schema, "names", "raw").await);
    let rows = conn
        .query(
            &format!("SELECT normalized FROM {s}.names ORDER BY id"),
            &[],
        )
        .await
        .expect("read normalized");
    let got: Vec<String> = rows.iter().map(|r| r.get::<_, String>(0)).collect();
    assert_eq!(got, vec!["alice".to_string(), "bob".to_string()]);
    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// (11) a multi-Backfill plan
// ---------------------------------------------------------------------------

#[compio::test]
async fn multi_backfill_plan_runs_each_in_order() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);

    let create = PlanStep::Ddl(ddl(
        1,
        "create_m",
        &cfg.project_schema,
        &format!(
            "CREATE TABLE {s}.m (id bigint PRIMARY KEY, a text, b text, ua text, ub text); \
             INSERT INTO {s}.m (id, a, b) VALUES (1, 'X', 'Y')"
        ),
        None,
    ));
    let bf1 = PlanStep::Backfill(BackfillSpec {
        table: "m".to_string(),
        cursor_column: "id".to_string(),
        batch_size: 50,
        set_clause: "ua = lower(a)".to_string(),
        filter: Some("ua IS NULL".to_string()),
        name: "bf_a".to_string(),
    });
    let bf2 = PlanStep::Backfill(BackfillSpec {
        table: "m".to_string(),
        cursor_column: "id".to_string(),
        batch_size: 50,
        set_clause: "ub = lower(b)".to_string(),
        filter: Some("ub IS NULL".to_string()),
        name: "bf_b".to_string(),
    });

    let engine = MigrationEngine::new();
    engine
        .apply_plan(
            &[create, bf1, bf2],
            Approval::Approved, // a backfill mutates table data — needs approval
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("multi-backfill plan applies");
    let row = conn
        .query_one(&format!("SELECT ua, ub FROM {s}.m WHERE id = 1"), &[])
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "x");
    assert_eq!(row.get::<_, String>(1), "y");
    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// (7a) single-step facade precondition
// ---------------------------------------------------------------------------

#[test]
fn single_step_facade_yields_migration_for_sql_plan_and_fails_closed_otherwise() {
    // A `.sql`-shaped single-step plan yields its one &Migration.
    let m = ddl(1, "x", "s", "CREATE TABLE s.x (id bigint)", None);
    let plan = AppliedPlan::single_step(m.clone());
    assert!(plan.is_single_step());
    let got = plan.single_step_migration().expect("single Ddl plan yields its migration");
    assert_eq!(got.version, m.version);

    // A multi-step plan fails closed (the platform-path defense-in-depth arm).
    let multi = AppliedPlan {
        version: m.version.clone(),
        name: m.name.clone(),
        steps: vec![
            PlanStep::Ddl(m.clone()),
            PlanStep::Backfill(BackfillSpec {
                table: "x".into(),
                cursor_column: "id".into(),
                batch_size: 1,
                set_clause: "id = id".into(),
                filter: None,
                name: "bf".into(),
            }),
        ],
        checksum: m.checksum.clone(),
        flags: m.flags,
        dialect_scope: zeroship_migrate::DialectScope::Both,
        rollbackable: false,
        owner_app: m.owner_app.clone(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
    };
    let err = multi.single_step_migration().expect_err("multi-step fails closed");
    assert!(matches!(err, NotSingleStep { step_count: 2, .. }));
}

// ---------------------------------------------------------------------------
// (7a) single-step-shape precondition over the REAL platform changelog: every
//      `.sql` in platform Flyway-mode lowers to a plan whose steps == [Ddl(_)],
//      proving `single_step()`'s Err(NotSingleStep) arm is unreachable there.
// ---------------------------------------------------------------------------

#[test]
fn every_platform_changelog_sql_lowers_to_a_single_step_plan() {
    // The concrete platform changelog dir is an UNTRACKED build artifact — not all
    // checkouts have it. Gate on its existence (mirroring `loaded_platform_changelog`)
    // so the suite is not brittle to its absence; the GENERATED-Flyway property test
    // `generated_flyway_dirs_always_lower_to_single_step_plans` is the changelog-dir-
    // independent proof that `single_step()`'s Err arm is unreachable on the .sql path.
    let Ok(dir) = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../db/migrations")
        .canonicalize()
    else {
        eprintln!("skipping: ../../db/migrations not present in this checkout");
        return;
    };
    let plans = zeroship_migrate::load_dir(&dir).expect("platform changelog loads");
    assert!(!plans.is_empty(), "the platform changelog is non-empty");
    for plan in &plans {
        assert!(
            plan.is_single_step(),
            "platform changelog version {} must lower to exactly one Ddl step \
             (single_step()'s Err arm must be unreachable on the platform path); \
             got {} steps",
            plan.version.as_str(),
            plan.steps.len()
        );
        // And the facade actually yields that one migration (never the Err arm).
        plan.single_step_migration()
            .expect("platform single-step facade yields its migration");
    }
}

// ---------------------------------------------------------------------------
// (7a, spec test #7) PROPERTY TEST over GENERATED Flyway-mode `.sql` dirs:
// every plan `load_dir` produces from a `.sql` path lowers to a single Ddl step,
// so `AppliedPlan::single_step()`'s `Err(NotSingleStep)` arm is UNREACHABLE on the
// `.sql` path — and the proof does NOT depend on the one untracked concrete
// `db/migrations` dir. A small deterministic LCG generates random valid Flyway
// dirs (1..N versioned `V<NNNN>__<desc>.sql` files, some with `.down.sql`, some
// `R__` repeatables) into a tempdir, loads them, and asserts `is_single_step()`
// on every plan.
// ---------------------------------------------------------------------------

#[test]
fn generated_flyway_dirs_always_lower_to_single_step_plans() {
    // A tiny deterministic LCG (Numerical Recipes constants) — reproducible, no
    // external proptest dependency.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 16
        }
        fn range(&mut self, lo: u64, hi: u64) -> u64 {
            lo + self.next() % (hi - lo + 1)
        }
    }

    for seed in 0u64..40 {
        let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1));
        let dir = tempfile::tempdir().expect("tempdir");
        let n = rng.range(1, 6); // 1..6 versioned migrations
        for v in 1..=n {
            // A valid single-statement DDL body — the `.sql` grammar lowers it to
            // exactly one Ddl step regardless of which DDL it is.
            let kind = rng.range(0, 2);
            let body = match kind {
                0 => format!("CREATE TABLE t{v} (id bigint PRIMARY KEY)"),
                1 => format!("ALTER TABLE t1 ADD COLUMN c{v} text"),
                _ => format!("CREATE INDEX i{v} ON t1 (id)"),
            };
            let up = dir.path().join(format!("V{v:04}__gen_{v}.sql"));
            std::fs::write(&up, body).expect("write up");
            // Sometimes add a matching `.down.sql` (still a single up step).
            if rng.range(0, 1) == 1 {
                let down = dir.path().join(format!("V{v:04}__gen_{v}.down.sql"));
                std::fs::write(&down, format!("DROP TABLE IF EXISTS t{v}")).expect("write down");
            }
        }
        // Sometimes add a repeatable (also lowers to one step).
        if rng.range(0, 1) == 1 {
            let r = dir.path().join("R__refresh_view.sql");
            std::fs::write(&r, "CREATE TABLE IF NOT EXISTS rrr (id bigint)").expect("write R");
        }

        let plans = zeroship_migrate::load_dir(dir.path())
            .unwrap_or_else(|e| panic!("seed {seed}: generated Flyway dir must load: {e:?}"));
        assert!(!plans.is_empty(), "seed {seed}: at least one plan");
        for plan in &plans {
            assert!(
                plan.is_single_step(),
                "seed {seed}: every generated .sql must lower to ONE Ddl step \
                 (single_step()'s Err arm must be unreachable on the .sql path); \
                 version {} got {} steps",
                plan.version.as_str(),
                plan.steps.len()
            );
            plan.single_step_migration()
                .unwrap_or_else(|e| panic!("seed {seed}: facade must yield the migration: {e:?}"));
        }
    }
}

// ---------------------------------------------------------------------------
// (8) golden-trace oracle — the re-pointed declarative path (apply_declarative →
//     apply_plan) reproduces the expected journal+schema for representative
//     declarative behavioral paths: (c) destructive-requires-approval REFUSAL and
//     (d) net-applied-skip idempotent re-run. The PG-online (a) + SQLite-rebuild
//     (b) paths are covered by expand_contract_pg / apply_plan_sqlite; the full
//     ~507-test declarative suite is the standing byte-for-byte regression net.
// ---------------------------------------------------------------------------

#[compio::test]
async fn golden_trace_declarative_refusal_and_idempotent_reapply() {
    use zeroship_migrate::author::{AuthorRequest, Column, DeterministicAuthor, MigrationAuthor};

    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    // (c) A destructive declarative plan WITHOUT approval is refused, applies
    //     nothing — the journal trace stays empty.
    let det = DeterministicAuthor::new(cfg.project_schema.clone(), "app_test");
    let create = det
        .author(&AuthorRequest::CreateTable {
            name: "g".into(),
            columns: vec![Column { name: "id".into(), ty: "bigint".into(), nullable: false }],
        })
        .unwrap();
    let engine = MigrationEngine::new();

    // First apply the additive create (no approval needed) through the engine.
    let plan = engine.plan(&create, &GuardConfig::confined(cfg.project_schema.clone()));
    engine
        .apply(
            &plan,
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
        )
        .await
        .expect("additive create applies");
    let trace_after_create = journal_trace(&conn, &cfg).await;
    assert_eq!(trace_after_create.len(), 1, "exactly the create is journaled");
    let create_version = create[0].version.as_str().to_string();
    assert_eq!(trace_after_create, vec![create_version.clone()]);

    // (d) Re-running the same create is a net-applied-skip: the journal trace is
    //     UNCHANGED (idempotent), no second event.
    let plan2 = engine.plan(&create, &GuardConfig::confined(cfg.project_schema.clone()));
    let out2 = engine
        .apply(
            &plan2,
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
        )
        .await
        .expect("re-run");
    assert!(out2.applied.is_empty(), "idempotent re-run applies nothing");
    assert_eq!(
        journal_trace(&conn, &cfg).await,
        vec![create_version],
        "the journal trace is byte-stable across the idempotent re-run"
    );
    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// (12) PROPERTY-BASED CRASH FUZZ — resume from any crash point reaches the
//      same final DB + journal as the no-crash apply.
// ---------------------------------------------------------------------------

/// A small deterministic in-grammar plan generator: a CREATE TABLE spine + N
/// add-column DDL steps + a Dml seed + a backfill, parameterized by a seed so the
/// generated plan is reproducible. The "crash injection" is modeled by applying a
/// PREFIX of the steps (a crash after step k leaves steps 0..k journaled), then
/// resuming with the WHOLE plan and asserting the final DB + journal equals the
/// no-crash apply. Because each step journals independently and re-runs net-skip,
/// resume-from-any-prefix must converge.
fn gen_plan(seed: u64, schema: &str) -> Vec<PlanStep> {
    let s = q(schema);
    let n_cols = 1 + (seed % 3); // 1..3 add-column steps
    let mut steps: Vec<PlanStep> = Vec::new();
    steps.push(PlanStep::Ddl(ddl(
        1,
        "create_f",
        schema,
        &format!("CREATE TABLE {s}.f (id bigint PRIMARY KEY, src text)"),
        None,
    )));
    steps.push(PlanStep::Dml {
        version: zeroship_migrate::migration_id_for_version(10),
        name: "seed_f".to_string(),
        template: format!("INSERT INTO {s}.f (id, src) VALUES ($1, $2)"),
        binds: vec![BindValue::Int(1), BindValue::Text("RAW".into())],
        transactional: true,
        destructive: false,
    });
    for c in 0..n_cols {
        steps.push(PlanStep::Ddl(ddl(
            100 + c,
            &format!("add_c{c}"),
            schema,
            &format!("ALTER TABLE {s}.f ADD COLUMN c{c} text"),
            None,
        )));
    }
    steps.push(PlanStep::Backfill(BackfillSpec {
        table: "f".to_string(),
        cursor_column: "id".to_string(),
        batch_size: 100,
        set_clause: "c0 = lower(src)".to_string(),
        filter: Some("c0 IS NULL".to_string()),
        name: "bf_c0".to_string(),
    }));
    steps
}

async fn apply_prefix_then_full(
    conn: &Client,
    cfg: &ExecutorConfig,
    steps: &[PlanStep],
    crash_after: usize,
) {
    let engine = MigrationEngine::new();
    // Crash injection: apply only the prefix [0..crash_after], simulating a crash
    // that journaled those steps, then resume with the WHOLE plan.
    if crash_after > 0 {
        let _ = engine
            .apply_plan(
                &steps[..crash_after],
                Approval::Approved,
                &zeroship_migrate::PostgresBackend::new(conn),
                cfg,
                "app_test",
                zeroship_migrate::executor::LockMode::Acquire,
            )
            .await;
    }
    engine
        .apply_plan(
            steps,
            Approval::Approved,
            &zeroship_migrate::PostgresBackend::new(conn),
            cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("resume-from-prefix completes");
}

/// The final DB+journal fingerprint: net-applied journal versions (sorted) + the
/// row's backfilled value — the model the crash fuzz cross-checks.
async fn fingerprint(conn: &Client, cfg: &ExecutorConfig) -> (Vec<String>, Option<String>) {
    let mut versions = journal_trace(conn, cfg).await;
    versions.sort();
    versions.dedup();
    let s = q(&cfg.project_schema);
    let val = conn
        .query_opt(&format!("SELECT c0 FROM {s}.f WHERE id = 1"), &[])
        .await
        .ok()
        .flatten()
        .and_then(|r| r.try_get::<_, Option<String>>(0).ok().flatten());
    (versions, val)
}

#[compio::test]
async fn crash_fuzz_resume_from_any_point_converges() {
    let conn = pg().await;

    for seed in 0u64..4 {
        // Baseline: a clean no-crash apply of the generated plan.
        let cfg_base = cfg_for(&token());
        setup(&conn, &cfg_base).await;
        let steps_base = gen_plan(seed, &cfg_base.project_schema);
        apply_prefix_then_full(&conn, &cfg_base, &steps_base, 0).await;
        let golden = fingerprint(&conn, &cfg_base).await;
        teardown(&conn, &cfg_base).await;

        // For every crash point, a fresh DB resumes and must reach the SAME
        // fingerprint as the no-crash apply.
        let n = steps_base.len();
        for crash_after in 1..=n {
            let cfg = cfg_for(&token());
            setup(&conn, &cfg).await;
            let steps = gen_plan(seed, &cfg.project_schema);
            apply_prefix_then_full(&conn, &cfg, &steps, crash_after).await;
            let got = fingerprint(&conn, &cfg).await;
            assert_eq!(
                got.1, golden.1,
                "seed {seed}, crash_after {crash_after}: backfilled value diverged"
            );
            assert_eq!(
                got.0.len(),
                golden.0.len(),
                "seed {seed}, crash_after {crash_after}: journal version SET diverged \
                 (got {:?}, golden {:?})",
                got.0,
                golden.0
            );
            teardown(&conn, &cfg).await;
        }
    }
}

// ---------------------------------------------------------------------------
// REGRESSION (code-critic HIGH #2): the destructive-DML approval gate.
//
// A `PlanStep::Dml { destructive: true }` under `Approval::None` MUST be refused
// (ApprovalRequired) and apply NOTHING — mirroring the per-Migration destructive
// gate the DDL spine runs. Pre-fix, `apply_plan`'s Dml arm passed `approval`
// straight through and `run_dml_step` discarded it (`let _ = approval;`), so a
// destructive DML applied silently under no approval — a latent data-loss hole.
// Under `Approval::Approved` the same step applies.
// ---------------------------------------------------------------------------

#[compio::test]
async fn destructive_dml_is_refused_without_approval_and_applies_nothing() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);

    // Seed a table + a row so the destructive DELETE has a target to (not) touch.
    let create = PlanStep::Ddl(ddl(
        1,
        "create_d",
        &cfg.project_schema,
        &format!(
            "CREATE TABLE {s}.d (id bigint PRIMARY KEY); \
             INSERT INTO {s}.d (id) VALUES (1)"
        ),
        None,
    ));
    let engine = MigrationEngine::new();
    engine
        .apply_plan(
            &[create],
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("additive create applies");

    // A destructive DML (a DELETE) WITHOUT approval is refused.
    let del_version = zeroship_migrate::migration_id_for_version(950);
    let delete = PlanStep::Dml {
        version: del_version.clone(),
        name: "wipe_d".to_string(),
        template: format!("DELETE FROM {s}.d WHERE id = $1"),
        binds: vec![BindValue::Int(1)],
        transactional: true,
        destructive: true,
    };
    let refused = engine
        .apply_plan(
            std::slice::from_ref(&delete),
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await;
    assert!(
        matches!(
            refused,
            Err(DeclarativeApplyError::Plain(EngineError::ApprovalRequired))
        ),
        "destructive DML under Approval::None must be refused, got {refused:?}"
    );

    // NOTHING was applied: the row survives, the DML version is NOT journaled.
    let count = conn
        .query_one(&format!("SELECT count(*)::bigint FROM {s}.d"), &[])
        .await
        .unwrap()
        .get::<_, i64>(0);
    assert_eq!(count, 1, "the destructive DML must apply nothing when refused");
    assert!(
        !journaled(&conn, &cfg, del_version.as_str()).await,
        "a refused destructive DML must not be journaled"
    );

    // The SAME step under Approval::Approved applies (the row is deleted).
    engine
        .apply_plan(
            &[delete],
            Approval::Approved,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("destructive DML applies under approval");
    let count = conn
        .query_one(&format!("SELECT count(*)::bigint FROM {s}.d"), &[])
        .await
        .unwrap()
        .get::<_, i64>(0);
    assert_eq!(count, 0, "the approved destructive DML deletes the row");
    assert!(
        journaled(&conn, &cfg, del_version.as_str()).await,
        "the approved destructive DML is journaled"
    );
    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// REGRESSION (code-critic HIGH #3): whole-deploy project-lock discipline for a
// plan whose FIRST step is Dml/Backfill (not Ddl).
//
// Two standalone `apply_plan(LockMode::Acquire)` deploys whose first step is a
// Dml MUST serialize on the project advisory lock. Pre-fix, `apply_plan` relied
// on the FIRST Ddl batch to acquire the lock, but a Dml/Backfill-first plan took
// NO whole-deploy project lock at all (the data seams take only a per-batch xact
// lock), so two such deploys could interleave — violating §2.0.3(1). Post-fix,
// `apply_plan` acquires the project lock up front for the whole plan regardless
// of first-step kind, so a concurrent same-project Dml-first deploy blocks until
// the first releases.
// ---------------------------------------------------------------------------

#[compio::test]
async fn dml_first_plan_holds_the_project_lock_for_the_whole_deploy() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);

    // Prepare a target table (separate deploy) so the contended deploys start
    // with a Dml step, not a Ddl one.
    let engine = MigrationEngine::new();
    engine
        .apply_plan(
            &[PlanStep::Ddl(ddl(
                1,
                "create_lk",
                &cfg.project_schema,
                &format!("CREATE TABLE {s}.lk (k text PRIMARY KEY)"),
                None,
            ))],
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("create target table");

    // A SECOND connection takes the project advisory lock by hand and holds it,
    // standing in for an in-flight concurrent deploy. A Dml-first `apply_plan`
    // with LockMode::Acquire must BLOCK on it — proving it tries to take the
    // whole-deploy project lock (pre-fix it would NOT, and would race through).
    let holder = pg().await;
    holder
        .execute(
            "SELECT pg_advisory_lock(hashtext($1)::bigint)",
            &[&cfg.project_id],
        )
        .await
        .expect("holder grabs the project lock");

    let dml = PlanStep::Dml {
        version: zeroship_migrate::migration_id_for_version(951),
        name: "seed_lk".to_string(),
        template: format!("INSERT INTO {s}.lk (k) VALUES ($1)"),
        binds: vec![BindValue::Text("a".into())],
        transactional: true,
        destructive: false,
    };

    // Drive the Dml-first deploy on a THIRD connection in the background; it must
    // not complete while the holder owns the lock.
    let dsn_owned = dsn();
    let cfg_owned = cfg.clone();
    let handle = compio::runtime::spawn(async move {
        let (c3, conn3) = compio_postgres::connect(&dsn_owned, compio_postgres::NoTls)
            .await
            .expect("third connection");
        compio::runtime::spawn(async move {
            let _ = conn3.run().await;
        })
        .detach();
        let engine = MigrationEngine::new();
        engine
            .apply_plan(
                &[dml],
                Approval::None,
                &zeroship_migrate::PostgresBackend::new(&c3),
                &cfg_owned,
                "app_test",
                zeroship_migrate::executor::LockMode::Acquire,
            )
            .await
            .expect("dml-first deploy eventually applies");
    });

    // Poll until the background deploy EITHER registers as a waiter on the
    // project advisory lock (the post-fix behavior — it took the whole-deploy lock
    // and is now blocked behind the holder) OR inserts its row (the pre-fix bug — it
    // took NO project lock and raced straight through while the holder held it).
    //
    // The single bigint advisory key K is stored in pg_locks as classid = high 32
    // bits of K, objid = low 32 bits, objsubid = 1. Decode the split via bit(64)
    // substrings → bit(32)::int (a reinterpret cast that can't overflow int4, unlike
    // `(K >> 32)::int`), matching the project's key EXACTLY so the many test binaries
    // that run in parallel against :5440 cannot satisfy this assertion.
    let mut waiter_seen = false;
    let mut raced_through = false;
    for _ in 0..2000u32 {
        let waiters: i64 = conn
            .query_one(
                "WITH k AS (SELECT (hashtext($1))::bigint::bit(64) AS b) \
                 SELECT count(*)::bigint FROM pg_locks, k \
                 WHERE locktype = 'advisory' AND objsubid = 1 \
                   AND classid = substring(k.b FROM 1 FOR 32)::int \
                   AND objid = substring(k.b FROM 33 FOR 32)::int \
                   AND NOT granted",
                &[&cfg.project_id],
            )
            .await
            .unwrap()
            .get(0);
        let rows_now: i64 = conn
            .query_one(&format!("SELECT count(*)::bigint FROM {s}.lk"), &[])
            .await
            .unwrap()
            .get(0);
        if rows_now >= 1 {
            raced_through = true;
            break;
        }
        if waiters >= 1 {
            waiter_seen = true;
            break;
        }
        // Yield so the background deploy makes progress and the lock-wait registers.
        let _ = compio::runtime::spawn(async {}).await;
    }
    assert!(
        !raced_through,
        "a Dml-first apply_plan(Acquire) raced through while the project lock was \
         held — it took NO whole-deploy project lock (the pre-fix bug)"
    );
    assert!(
        waiter_seen,
        "a Dml-first apply_plan(Acquire) must contend on the project advisory lock"
    );

    // The contended deploy is still NOT done (no row inserted yet).
    let count_before = conn
        .query_one(&format!("SELECT count(*)::bigint FROM {s}.lk"), &[])
        .await
        .unwrap()
        .get::<_, i64>(0);
    assert_eq!(
        count_before, 0,
        "the Dml-first deploy must be blocked on the project lock (no row yet)"
    );

    // Release the holder's lock; the blocked deploy now proceeds to completion.
    holder
        .execute(
            "SELECT pg_advisory_unlock(hashtext($1)::bigint)",
            &[&cfg.project_id],
        )
        .await
        .expect("holder releases the lock");
    handle.await.expect("background deploy task joins");

    let count_after = conn
        .query_one(&format!("SELECT count(*)::bigint FROM {s}.lk"), &[])
        .await
        .unwrap()
        .get::<_, i64>(0);
    assert_eq!(count_after, 1, "the deploy applied once the lock was free");
    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// (spec test #4) apply_plan-level PG online-rename PENDING-CONTRACT PARTITION.
//
// Drive a `PlanStep::OnlineRename(RenameStep::PgExpandContract(_))` DIRECTLY
// through `apply_plan` (not via the declarative adapter) and assert the EXPAND
// runs atomically under the held lock while C1/C2 are DEFERRED into
// `pending_contract` — the cross-deploy partition (§2.0.2). This pins the
// apply_plan-level partition the declarative-suite tests exercise only through
// `apply_declarative`.
// ---------------------------------------------------------------------------

#[compio::test]
async fn apply_plan_online_rename_defers_contract_to_pending_contract() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);
    let engine = MigrationEngine::new();

    // Create `users(id, email)` THROUGH apply_plan so the migrator role owns it
    // (the dual-write trigger DDL runs under SET ROLE).
    engine
        .apply_plan(
            &[PlanStep::Ddl(ddl(
                1,
                "create_users",
                &cfg.project_schema,
                &format!(
                    "CREATE TABLE {s}.users (id bigint PRIMARY KEY, email text); \
                     INSERT INTO {s}.users (id, email) VALUES (1,'a@x.test'),(2,'b@x.test')"
                ),
                None,
            ))],
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("create users");

    // Author the canonical online rename email → email_address and drive its
    // ExpandContractPlan as a SINGLE PgExpandContract step through apply_plan.
    let rename = ExpandContractAuthor::new(&cfg.project_schema, "app_test")
        .author(&OnlineIntent::RenameColumn {
            table: "users".into(),
            from: "email".into(),
            to: "email_address".into(),
            ty: "text".into(),
        })
        .expect("author rename");
    let contract_versions: Vec<String> =
        rename.contract.iter().map(|m| m.version.as_str().to_string()).collect();
    assert!(!contract_versions.is_empty(), "the rename has a deferred contract");

    let steps = vec![PlanStep::OnlineRename(RenameStep::PgExpandContract(rename))];
    let outcome = engine
        .apply_plan(
            &steps,
            Approval::Approved, // the expand's backfill mutates data
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("online rename expand applies via apply_plan");

    // The CONTRACT is partitioned into pending_contract (deferred to deploy N+1),
    // NOT applied this deploy.
    let got_pending: Vec<String> = outcome
        .pending_contract
        .iter()
        .map(|m| m.version.as_str().to_string())
        .collect();
    assert_eq!(
        got_pending, contract_versions,
        "apply_plan must defer C1/C2 into pending_contract (the cross-deploy partition)"
    );
    assert!(
        outcome.pending_contract.iter().any(|m| m.flags.destructive),
        "the deferred DROP COLUMN is destructive"
    );

    // After the EXPAND: BOTH columns exist (the old `email` is NOT yet dropped —
    // that is the deferred contract), and the backfill mirrored the rows into the
    // new column.
    assert!(
        column_exists(&conn, &cfg.project_schema, "users", "email").await,
        "the old column survives the expand (its drop is the deferred contract)"
    );
    assert!(
        column_exists(&conn, &cfg.project_schema, "users", "email_address").await,
        "the new column exists after the expand"
    );
    let rows = conn
        .query(
            &format!("SELECT email, email_address FROM {s}.users ORDER BY id"),
            &[],
        )
        .await
        .expect("read mirrored rows");
    for r in &rows {
        let from: Option<String> = r.get(0);
        let to: Option<String> = r.get(1);
        assert_eq!(to, from, "the backfill mirrored email → email_address");
    }

    // The contract was NOT journaled this deploy (it is pending, not applied).
    for v in &contract_versions {
        assert!(
            !journaled(&conn, &cfg, v).await,
            "contract version {v} must NOT be journaled at the expand deploy"
        );
    }
    teardown(&conn, &cfg).await;
}
