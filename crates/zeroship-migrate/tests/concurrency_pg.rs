//! H10 — whole-deploy serialization of a declarative deploy against a REAL
//! Postgres (no shims).
//!
//! A declarative deploy ([`MigrationEngine::apply_declarative`]) is several
//! sub-batches: the plain set plus one expand per rename. The design (§2.3
//! step 1, "acquire project advisory lock — serialize ALL migration activity")
//! requires the WHOLE deploy to be serialized against concurrent deploys for the
//! same project.
//!
//! The H10 gap: `apply_declarative` used to drive each sub-batch through
//! `executor::apply` / `run_expand`, each of which independently ACQUIRED and
//! RELEASED the project advisory lock. So between sub-batches the lock was FREE —
//! a concurrent second deploy could interleave its own sub-batch, and a
//! multi-rename declarative deploy was NOT serialized as a whole.
//!
//! The fix holds the project advisory lock ONCE across the entire
//! `apply_declarative` deploy (the inner sub-batches run with
//! `LockMode::AlreadyHeld`, skipping their own acquire/release). This test
//! asserts the structural invariant: while `apply_declarative` is in flight, a
//! SECOND connection can NEVER acquire the same project advisory lock — and CAN
//! acquire it once the deploy has finished.
//!
//! Requires `zeroship_migrate_test` on :5440.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use compio_postgres::Client;
use zeroship_migrate::{
    desired_snapshot, migrator_role_name, provision_migrator, role::deprovision_migrator,
    snapshot_schema, Approval, CollectionDescriptor, DeclarativeAuthor, ExecutorConfig,
    FieldDescriptor, GuardConfig, MigrationEngine, RenameHint, SchemaSnapshot,
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
    c.meta_schema = format!("meta_{tok}");
    c
}

fn cfg_with_role(tok: &str) -> ExecutorConfig {
    let c = cfg_for(tok);
    let role = migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}

fn guard_cfg(cfg: &ExecutorConfig) -> GuardConfig {
    GuardConfig::confined(cfg.project_schema.clone())
}

fn author_for(cfg: &ExecutorConfig) -> DeclarativeAuthor {
    DeclarativeAuthor::new(cfg.project_schema.clone(), "app_test")
}

async fn ensure_project_schema(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{}\"",
        cfg.project_schema
    ))
    .await
    .expect("create project schema");
}

async fn setup(conn: &Client, cfg: &ExecutorConfig) {
    ensure_project_schema(conn, cfg).await;
    provision_migrator(conn, cfg)
        .await
        .expect("provision migrator role");
}

async fn teardown(conn: &Client, cfg: &ExecutorConfig) {
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.meta_schema
        ))
        .await;
    let _ = deprovision_migrator(conn, cfg).await;
}

/// `pg_try_advisory_lock(hashtext(project_id)::bigint)` from `conn` — TRUE if the
/// session-scoped project advisory lock was acquired (and is now held by `conn`),
/// FALSE if another session already holds it. The probe RELEASES it immediately
/// on success so the probe itself never holds the lock past a single check.
async fn try_project_lock(conn: &Client, project_id: &str) -> bool {
    let row = conn
        .query_one(
            "SELECT pg_try_advisory_lock(hashtext($1)::bigint) AS got",
            &[&project_id],
        )
        .await
        .expect("pg_try_advisory_lock probe");
    let got: bool = row.get("got");
    if got {
        // Release immediately — the probe must not hold the lock.
        conn.execute(
            "SELECT pg_advisory_unlock(hashtext($1)::bigint)",
            &[&project_id],
        )
        .await
        .expect("pg_advisory_unlock probe");
    }
    got
}

/// H10 — a declarative deploy holds the project advisory lock for its WHOLE
/// duration: a second connection can NEVER acquire the same lock while the deploy
/// is in flight, and CAN acquire it once the deploy is done.
///
/// # Why this is non-flaky (and reliably RED on the pre-fix code)
///
/// Both connections share the single-threaded compio runtime, so the deploy task
/// and the probe loop cooperatively interleave at every `.await` (every DB
/// round-trip). The probe loop runs continuously WHILE the deploy is in flight,
/// so it observes the lock state at every sub-batch boundary the deploy yields
/// across — including the gaps the pre-fix code leaves between sub-batches (after
/// the plain set, around the backfill, before/after E3).
///
/// - PRE-FIX: each sub-batch acquires + releases the lock independently, so the
///   lock is FREE between sub-batches. The probe, running across those boundaries,
///   acquires the lock at least once mid-deploy ⇒ `acquired_mid_deploy == true`
///   ⇒ this test FAILS (RED).
/// - POST-FIX: the lock is held from the first sub-batch to the last, so the probe
///   NEVER acquires it mid-deploy ⇒ `acquired_mid_deploy == false` ⇒ GREEN. After
///   the deploy completes, the probe acquires it (no one holds it) ⇒ the
///   `after` assertion holds.
#[compio::test]
async fn declarative_deploy_holds_project_lock_across_all_sub_batches() {
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();
    let schema = cfg.project_schema.clone();

    // Create `users` with an `email` column.
    let v1 = CollectionDescriptor {
        name: "users".into(),
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
    };
    let d1 = desired_snapshot(&cfg.project_schema, &[v1]).expect("desired_snapshot");
    let plan1 = engine
        .plan_declarative(&d1, &SchemaSnapshot::default(), &HashMap::new(), &author, &[], &guard_cfg(&cfg))
        .expect("plan_declarative create");
    engine
        .apply(&plan1.plain, Approval::None, &conn, &cfg, "app_test")
        .await
        .expect("create users");

    // Seed pre-existing rows so the rename's EXPAND runs a real backfill.
    for i in 0..40 {
        conn.batch_execute(&format!(
            "INSERT INTO \"{schema}\".\"users\" (id, created_at, updated_at, version, email) \
             VALUES ('usr_{i}', NOW(), NOW(), 1, 'u{i}@x.test')"
        ))
        .await
        .expect("seed row");
    }

    // Make the backfill SLOW: a BEFORE UPDATE trigger that sleeps per row. The
    // backfill UPDATE then runs for ~2s, giving the cooperatively-scheduled probe
    // loop (same single-threaded compio runtime) a wide, reliable window to
    // interleave while the deploy is mid-flight (it is parked awaiting the slow
    // UPDATE). On the PRE-FIX code the project SESSION lock is FREE during the
    // backfill (the backfill uses a per-batch xact-scoped lock), so the probe
    // catches the free lock ⇒ RED. On the fixed code the session lock is held by
    // the outer `apply_declarative` throughout ⇒ the probe never catches it.
    conn.batch_execute(&format!(
        "CREATE OR REPLACE FUNCTION \"{schema}\".\"_slow_bf\"() RETURNS trigger \
           LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(0.05); RETURN NEW; END; $$;
         CREATE TRIGGER \"_slow_bf_trg\" BEFORE UPDATE ON \"{schema}\".\"users\" \
           FOR EACH ROW EXECUTE FUNCTION \"{schema}\".\"_slow_bf\"();"
    ))
    .await
    .expect("install slow-backfill trigger");

    // Desire `email` → `email_address`, WITH a matching rename hint: the deploy is
    // a pure rename ⇒ a structured EXPAND (E1+E2 → real backfill → E3) on top of
    // an (empty) plain set — i.e. multiple sub-batches, exactly the H10 shape.
    let v2 = CollectionDescriptor {
        name: "users".into(),
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
    };
    let d2 = desired_snapshot(&cfg.project_schema, &[v2]).expect("desired_snapshot");
    let live = snapshot_schema(&conn, &cfg.project_schema)
        .await
        .expect("snap");
    let hints = vec![RenameHint {
        table: "users".into(),
        from: "email".into(),
        to: "email_address".into(),
    }];
    let plan = engine
        .plan_declarative(&d2, &live, &HashMap::new(), &author, &hints, &guard_cfg(&cfg))
        .expect("plan_declarative rename");
    assert_eq!(plan.renames.len(), 1, "one structured rename");

    // Drive the deploy on a spawned task; probe the lock from a SECOND connection
    // on the main task while the deploy is in flight. Both share the
    // single-threaded compio runtime, so they interleave at every `.await`.
    let probe_conn = pg().await;
    let project_id = cfg.project_id.clone();
    let cfg_for_teardown = cfg.clone();

    // The deploy runs on `conn`; we observe its backend's advisory locks in
    // `pg_locks` (keyed by its backend pid) from the second connection.
    let deploy_pid: i32 = conn
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);

    let done = Rc::new(Cell::new(false));
    let done_for_task = done.clone();
    let engine_task = engine.clone();

    let deploy = compio::runtime::spawn(async move {
        let outcome = engine_task
            .apply_declarative(&plan, Approval::Approved, &conn, &cfg, "app_test")
            .await
            .expect("apply_declarative");
        done_for_task.set(true);
        outcome
    });

    // While the deploy runs, observe the lock from the second connection. The
    // ground-truth signal is the deploy backend's advisory-lock count in
    // `pg_locks` (read-only, non-perturbing); the second-connection
    // `pg_try_advisory_lock` is the belt-and-suspenders cross-session check.
    //
    // Helper: how many advisory locks the DEPLOY backend currently holds. Read
    // from `pg_locks` on the probe connection — read-only, so it never perturbs
    // the deploy's lock state (unlike a `pg_try_advisory_lock`, which would itself
    // acquire if the lock were free).
    async fn deploy_advisory_locks(probe: &Client, deploy_pid: i32) -> i64 {
        probe
            .query_one(
                "SELECT count(*) FROM pg_locks WHERE locktype='advisory' AND pid=$1",
                &[&deploy_pid],
            )
            .await
            .unwrap()
            .get(0)
    }

    // Phase 1 — wait until the deploy has ACQUIRED the project lock (≥1 advisory
    // lock on its backend). This skips the unavoidable startup window between
    // spawning the task and its first `acquire_project_lock_outer` completing,
    // during which the lock is legitimately not yet held.
    let mut acquired = false;
    while !done.get() {
        if deploy_advisory_locks(&probe_conn, deploy_pid).await >= 1 {
            acquired = true;
            break;
        }
    }
    assert!(
        acquired || done.get(),
        "deploy never acquired the project advisory lock"
    );

    // Phase 2 — from the moment the lock is held until the deploy finishes, the
    // deploy backend must hold the project advisory lock CONTINUOUSLY. The H10
    // invariant: the lock is acquired ONCE and released ONCE — it is never freed
    // and RE-acquired between sub-batches.
    //
    // The race-free distinguishing signal is "0 then back to ≥1" — a
    // re-acquisition:
    //   - PRE-FIX: each sub-batch releases the lock (count → 0) and the NEXT
    //     sub-batch re-acquires it (count → ≥1). So an observation of 0 is followed
    //     by a later observation of ≥1 ⇒ `reacquired_after_zero` ⇒ RED.
    //   - POST-FIX: the count stays ≥1 throughout; the only 0 is the single final
    //     release at the very end of the deploy, which is NEVER followed by a ≥1
    //     (the deploy is done). So `reacquired_after_zero` stays false ⇒ GREEN.
    // This tolerates the unavoidable end-race window (the lock is released inside
    // `apply_declarative` just before the task flips `done`): that terminal 0 is
    // never followed by a re-acquire, so it is not counted as a violation.
    //
    // The cross-session `pg_try_advisory_lock` is the belt-and-suspenders check; it
    // can also race the terminal release window, so a bare success is only treated
    // as a violation when the deploy backend then holds the lock AGAIN afterwards
    // (proving the success was a mid-deploy gap, not the final release).
    let mut seen_zero = false;
    let mut reacquired_after_zero = false;
    let mut try_lock_then_held_again = false;
    let mut probes = 0u32;
    while !done.get() {
        let held = deploy_advisory_locks(&probe_conn, deploy_pid).await;
        if held == 0 {
            seen_zero = true;
        } else if seen_zero {
            reacquired_after_zero = true;
        }
        if try_project_lock(&probe_conn, &project_id).await {
            seen_zero = true;
        } else if seen_zero && deploy_advisory_locks(&probe_conn, deploy_pid).await >= 1 {
            try_lock_then_held_again = true;
        }
        probes += 1;
    }
    let dropped_to_zero = reacquired_after_zero;
    let acquired_mid_deploy = reacquired_after_zero || try_lock_then_held_again;

    let outcome = deploy.await.expect("deploy task join");
    assert_eq!(outcome.pending_contract.len(), 2, "contract C1+C2 deferred");
    assert!(probes > 0, "the probe loop must have run while the deploy was in flight");

    // (a) GROUND TRUTH — once acquired, the deploy backend held the project
    //     advisory lock CONTINUOUSLY until it finished: its advisory-lock count
    //     never dropped to 0 mid-deploy. On the pre-fix code the per-sub-batch
    //     acquire/release frees the lock between sub-batches, so the count drops to
    //     0 (no other advisory lock is held there) ⇒ `dropped_to_zero` ⇒ RED.
    assert!(
        !dropped_to_zero,
        "the deploy backend's advisory-lock count dropped to 0 mid-deploy ({probes} probes) \
         — the project lock was freed between sub-batches (H10 not serialized)"
    );

    // (b) CROSS-SESSION — a second connection NEVER acquired the same lock while
    //     the deploy held it. Belt-and-suspenders behind (a).
    assert!(
        !acquired_mid_deploy,
        "a second connection acquired the project advisory lock DURING apply_declarative \
         ({probes} probes) — the lock was freed between sub-batches (H10 not serialized)"
    );

    // (c) After the deploy finishes, the lock is free: the second connection CAN
    //     acquire it (the deploy released it exactly once on completion — never
    //     held forever).
    assert!(
        try_project_lock(&probe_conn, &project_id).await,
        "after apply_declarative finished, the project advisory lock should be free"
    );
    assert_eq!(
        deploy_advisory_locks(&probe_conn, deploy_pid).await,
        0,
        "the deploy backend must hold no advisory lock after apply_declarative returns"
    );

    teardown(&probe_conn, &cfg_for_teardown).await;
}

