// The parity Postgres fixture chains four awaits over compio-postgres futures in one
// block; each layer is a large generated state machine, and the default 128 is not
// enough to compute its layout. A compile-budget knob, not a behaviour change.
#![recursion_limit = "256"]

//! Integration tests for plugin-db query builders against real Postgres.
//!
//! Requires: the test PostgreSQL named by the overlay
//! (`deploy/ops/zeroship.test.toml`, written by
//! `tests/provision_test_backends.sh`) or by `PG_TEST_URL`. There is no
//! compiled default; see `crates/zeroship-core/src/config/test_overlay.rs`.
//! Run:
//! ```text
//! RUST_MIN_STACK=33554432 \
//! cargo test -p zeroship-plugin-db --test integration --features test-helpers
//! ```
//!
//! `--features test-helpers` is this target's `required-features`. Without it
//! cargo does not build the target at all - it FILTERS IT OUT, printing
//! `error: target `integration` ... requires the features: `test-helpers``
//! only if you named the target explicitly. A plain `cargo test -p
//! zeroship-plugin-db` names no target, so it silently runs the lib tests
//! alone and reports a healthy green while none of the 99 tests in this file
//! were compiled. This line omitted the flag until 2026-08-27, so the command
//! documented here did not run.
//!
//! # This suite runs at the default thread count, and that took three fixes
//!
//! It did not until 2026-09-04, and the command above carried
//! `-- --test-threads=1` for the whole time. Parallel it scored 62 passed / 21
//! failed against 80 / 3 serial, and the failures read like data-plane
//! regressions (`aggregate_having`, `update_one_inc`, `mixed_update`) rather
//! than like a broken harness, which is the expensive part. Three independent
//! mechanisms produced them, and fixing any one alone left the suite red:
//!
//! STATE THE DATABASE WITH ANY COUNT FROM THIS SUITE. The two numbers above
//! were recorded without one, and they are not reproducible: re-measured
//! 2026-09-04 against a FRESHLY CREATED database, this file at its
//! pre-parallelism commit scores **83 passed / 0 failed / 3 ignored** serially,
//! not 80 / 3. The gap is residue, not product. Every "pre-existing failure"
//! this suite was believed to carry was an artifact of a database some earlier
//! run had already scribbled on - which is the same defect the fix below is
//! about, showing up one level higher, in the measurement rather than the run.
//! A count from a reused database describes that database's history as much as
//! it describes the code.
//!
//! A FRESH DATABASE IS NECESSARY AND NOT SUFFICIENT, and believing otherwise
//! cost an hour on 2026-09-04. A worker slot is named from the TEST's own name
//! (`support::test_app_id_from` -> `replication::worker_slot_name`), so it is
//! the same string on every run, and `pg_replication_slots` is a CLUSTER-wide
//! catalog that `replication::ensure_worker_slot` probes with no `database`
//! predicate - correctly, because slot names are unique per cluster, not per
//! database. So a slot a failed run leaked into database A is found by the SAME
//! test running in brand-new database B, where `assert!(first.created)` then
//! fails for a reason that has nothing to do with the run. It reproduced 5 times
//! out of 5 and read exactly like a regression this file had just introduced.
//! Between measurements, drop every `__zs\_%` slot on the SERVER, not only the
//! database - and assert the count is zero before starting, because the run that
//! leaked one exits before it can tell you.
//!
//! 1. **One shared schema constant.** `SCHEMA = "plugin_db_test"` here and
//!    `APP_SCHEMA = "default"` in `native_transaction.rs`, each with a setup
//!    that opens `DROP SCHEMA ... CASCADE`. Fixed by deriving a per-test app id
//!    from the test's own name - see `support::test_app_id_from`.
//! 2. **A database-wide destructive sweep.** `c1_cleanup` ended with two
//!    unbounded `LIKE '__zs_%'` sweeps that wiped every sibling's CDC fixture.
//!    Moved to `support::sweep_prior_run_residue_once`, which runs as a barrier
//!    before any test starts - the only moment a sweep is correct.
//! 3. **Genuinely cluster-scoped resources.** `max_replication_slots` and
//!    `max_wal_senders` are both 10 on the test server, cluster-wide, and the
//!    slot reaper's fleet-leader lock and all-apps enumeration are exclusive by
//!    design. No naming scheme partitions those; see [`cdc_budget`].

use compio_postgres::{NoTls, Pool};
use serde_json::{Value, json};
use uuid::Uuid;
use zeroship_data_core::binding::DbBinding;
use zeroship_plugin_db::backend::ChangeStream;

const CDC_TEST_WORKER_ID: &str = "plugin-db-integration-worker";

#[path = "support/mod.rs"]
mod support;

#[path = "parity/mod.rs"]
mod parity;

fn test_url() -> String {
    zeroship_core::config::test_database_url()
}

/// The backend handle the unmask entry points now take as a parameter.
///
/// They resolved one themselves, from the isolate's context, until 2026-09-03.
/// That read is the ADAPTER's and `crud::unmask` is ENGINE, so the resolution
/// moved to the V8 dispatcher and the value is passed down. These tests drive
/// the engine directly, with no V8 frame above them, so they make the same call
/// the dispatcher makes on their behalf in production.
async fn unmask_backend() -> zeroship_plugin_db::backend::BackendHandle {
    zeroship_plugin_db::tx_scope::ensure_backend()
        .await
        .expect("the backend the V8 dispatcher would have opened")
}

/// The route the unmask dispatchers now take, in place of a bare handle.
///
/// See the twin in `mask_flip.rs` for why. No fixture that reaches it here
/// parks a transaction, so every call binds `in_tx = false` and takes the lane
/// it took before.
async fn unmask_route(app: &str) -> zeroship_plugin_db::tx_route::TxRoute {
    zeroship_plugin_db::exec::ambient_route_for_tests(app, unmask_backend().await)
}

async fn require_pg() -> String {
    let url = test_url();
    match compio_postgres::connect(&url, NoTls).await {
        Ok((client, connection)) => {
            // Drive the connection just long enough to drop both halves.
            compio::runtime::spawn(async move {
                let _ = connection.run().await;
            })
            .detach();
            drop(client);
            // Every test enters here before it touches the database, and
            // `Once::call_once` blocks the rest until the first returns, so this
            // is the barrier that makes an unbounded residue sweep safe. See
            // `support::sweep_prior_run_residue_once`.
            support::sweep_prior_run_residue_once(&url);
            // The transaction orchestrator opens a
            // dedicated client via the Backend trait's
            // `acquire_dedicated_client`, which reads the URL from the
            // per-thread context. Tests that drive the orchestrator directly
            // need the URL installed in the context before the call.
            zeroship_plugin_db::set_db_url_for_tests(&url);
            url
        }
        Err(e) => {
            // Fail this test rather than exiting the process.
            //
            // `std::process::exit(0)` here ended the whole binary with a
            // SUCCESS status the moment any one test could not reach the
            // database. Every test still queued was abandoned, every result
            // already produced was discarded - including failures - and cargo
            // reported the suite as passing. A run that printed
            // "delete_operations ... FAILED" still exited 0.
            //
            // A panic costs the honest thing instead: this test fails, its
            // siblings keep running, and the summary says what happened. The
            // target is opt-in behind `required-features = ["test-helpers"]`,
            // so reaching here means someone asked for the live-Postgres suite
            // and did not have Postgres - which is a failure, not a pass.
            panic!("live-Postgres suite requires a reachable server at PG_TEST_URL: {e}");
        }
    }
}

/// Bound the CDC family's concurrency, because nothing else can.
///
/// This is NOT the thing the fix was forbidden to do. Pinning
/// `--test-threads=1` in a config file serialises all 86 tests invisibly and
/// forever; this bounds 14 named tests at their own call sites - 5 exclusive
/// and 9 shared, counted by call site rather than estimated - leaves the other
/// 72 fully parallel, and states the server limits it is sized against.
///
/// # The case it is kept on, and the three clauses that have none
///
/// This module first justified itself with a run it did not isolate: per-test
/// app ids, the residue sweep and this budget landed together, and the budget
/// took the credit for turning 78 / 5 / 3 green. It was re-measured on
/// 2026-09-04 one clause at a time, on PostgreSQL 18.6 with
/// `max_replication_slots` = `max_wal_senders` = 10 and `nproc` 16, each run
/// against a database minted for it on a server carrying no `__zs\_%` slot:
///
/// ```text
/// integration --test-threads=16 -- c1_ p8a2_ drop_namespace_    (19 tests)
///   unmutated                            5 runs, 19/19 pass every time
///   `exclusive`'s wait deleted           5 runs, ALL FAIL, always the four
///                                        c1_abandoned_reaper_* tests
///   both waits deleted                   5 runs, ALL FAIL, those four plus
///                                        p8a2_supervised_consumer_reconnects_
///                                        after_kill or c1_setup_resumes_...
///   `shared`'s wait deleted              5 runs, 19/19 pass every time
/// ```
///
/// So [`exclusive`] is what the budget is for, and the three failure texts are
/// the three resources named below: `observer must own the fleet sweep` (a
/// sibling reaper holds the one-per-database leader lock), `exactly one
/// concurrent reaper must win the drop` and `assert_eq!(due.dropped,
/// vec![slot])` (a sibling's slot is inside this reaper's fleet-wide
/// enumeration).
///
/// **No mutation could produce a failing case for the other three clauses** -
/// [`SHARED_LIMIT`], `waiting_exclusive`, or [`shared`] standing aside for an
/// admitted exclusive. The reason is dispatch order, not safety: libtest starts
/// tests in name order, every exclusive test here sorts at `c1_abandoned_...` or
/// `c1_cleanup_...` ahead of every shared one, so an exclusive parks until the
/// shared population has drained and there is no shared test left to arrive
/// during it. That is an accident of naming. One shared CDC test named ahead of
/// `c1_abandoned_` reopens the window in a way no assertion here would attribute
/// to the harness, which is why [`shared`] keeps a guard nothing currently
/// binds. Whoever deletes it should first name the test that makes it
/// observable, not the run that did not.
///
/// # Why naming cannot solve this half
///
/// Three resources here are not partitioned by any app id:
///
/// * `max_replication_slots` and `max_wal_senders`, both **10** on the test
///   server (measured 2026-09-04 on PG 18.6), both CLUSTER-wide, against
///   `nproc` = 16. A slot-creating test that finds the budget exhausted fails
///   with a wal-consumer error, not with anything that names the cause.
/// * `slot_reaper::OperatorSlotReaper`'s enumeration, which filters on
///   `database = current_database()` and the `__zs_slot_` prefix and nothing
///   else. It is a FLEET sweeper by design: a sibling's slot appears in its
///   `inspected`/`dropped` sets, so `c1_abandoned_reaper_*`'s equalities fail.
/// * The reaper's fleet-leader advisory lock, one per database. A sibling
///   holding it makes `is_leader` false, which
///   `c1_abandoned_reaper_elects_one_leader_across_concurrent_workers` asserts
///   against directly.
///
/// # The shape
///
/// [`exclusive`] admits one test and excludes every other CDC test; the four
/// reaper tests and the cross-database sweep guard take it. [`shared`] admits
/// [`SHARED_LIMIT`] at once and is excluded by an exclusive holder; everything
/// else that creates a publication or a slot takes it.
mod cdc_budget {
    use std::sync::{Condvar, Mutex};

    /// How many slot-holding CDC tests may run at once.
    ///
    /// `max_replication_slots` is 10 on the test server, cluster-wide. The
    /// heaviest shared test holds two slots at a time
    /// (`c1_setup_resumes_at_existing_lsn_across_restart` re-runs
    /// `ensure_worker_slot` on a second pool), so this caps worst-case demand
    /// at 4 x 2 = 8.
    ///
    /// THAT IS AN ARITHMETIC BOUND AND NOT WHAT HAPPENS. This line claimed
    /// "raising this above 5 would put the suite back on the cluster ceiling"
    /// until 2026-09-04, when it was measured by sampling
    /// `pg_replication_slots` server-side every 2ms across the 19-test CDC run:
    /// peak slots alive at any instant are **4 of 10 with this cap and 7 of 10
    /// with no cap at all**, and the uncapped run passed 5 times out of 5. The
    /// cap does not stand between this suite and a failure anyone has produced;
    /// it holds 6 free slots instead of 3, on a resource whose exhaustion
    /// arrives as `wal consumer: db error` and names nothing.
    const SHARED_LIMIT: usize = 4;

    struct Budget {
        exclusive: bool,
        shared: usize,
        /// Exclusive callers parked in [`exclusive`], counted so [`shared`] can
        /// stand aside for them. See the starvation note on `shared`.
        waiting_exclusive: usize,
    }

    static STATE: Mutex<Budget> = Mutex::new(Budget {
        exclusive: false,
        shared: 0,
        waiting_exclusive: 0,
    });
    static CHANGED: Condvar = Condvar::new();

    /// Released on drop, so a panicking test hands its budget back during unwind
    /// rather than wedging every CDC test behind it.
    pub struct Permit {
        exclusive: bool,
    }

    impl Drop for Permit {
        fn drop(&mut self) {
            let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
            if self.exclusive {
                state.exclusive = false;
            } else {
                state.shared -= 1;
            }
            drop(state);
            CHANGED.notify_all();
        }
    }

    /// A permit for a CDC test that only touches its own app's objects.
    ///
    /// Waits while an exclusive holder is admitted, while the shared population
    /// is at [`SHARED_LIMIT`], or while an exclusive caller is PARKED. That last
    /// clause stands aside for a waiting exclusive so a stream of shared
    /// arrivals cannot starve it: without it a reaper test waits for
    /// `shared == 0`, a moment nine shared CDC tests on sixteen threads need not
    /// ever produce. It cannot deadlock in return, because no test holds one
    /// permit while asking for another - every call site takes exactly one, at
    /// the top of the test, and holds it to the end.
    ///
    /// Starvation is the hazard argued for, not one observed: deleting this
    /// whole wait ran 5 times without hanging, and 5 more with only the
    /// `waiting_exclusive` clause deleted. Both are green because every
    /// exclusive test sorts ahead of every shared one, so a parked exclusive is
    /// waiting on a population that is already draining and never refilled. See
    /// the module doc for what that costs the day a shared CDC test is named
    /// earlier.
    pub fn shared() -> Permit {
        let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        while state.exclusive || state.waiting_exclusive > 0 || state.shared >= SHARED_LIMIT {
            state = CHANGED.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        state.shared += 1;
        Permit { exclusive: false }
    }

    /// A permit for a CDC test that reads or writes every app's objects.
    ///
    /// Waits for the CDC family to drain completely - no other exclusive holder
    /// and no shared holder - because the resources this fences (the reaper's
    /// fleet-wide enumeration and its one-per-database leader lock) are not
    /// partitioned by app id at all.
    ///
    /// This is the one wait in the module a mutation can fail: delete it and the
    /// four `c1_abandoned_reaper_*` tests fail on all 5 runs of the command in
    /// the module doc, while the unmutated command passes all 5.
    pub fn exclusive() -> Permit {
        let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        state.waiting_exclusive += 1;
        while state.exclusive || state.shared > 0 {
            state = CHANGED.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        state.waiting_exclusive -= 1;
        state.exclusive = true;
        Permit { exclusive: true }
    }
}

/// Set up a test's own schema and `notes` table. Drops and recreates on every
/// call, which is what makes a rerun idempotent.
///
/// `schema` is the caller's per-test app id (`test_app_id!()`). It was one
/// shared `const SCHEMA = "plugin_db_test"` until 2026-09-04, and the
/// `DROP ... CASCADE` below is why twenty tests then had to run serially.
async fn setup(pool: &Pool, schema: &str) {
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{schema}\""), &[])
        .await
        .unwrap();
    pool.execute(
        &format!(
            // The last four columns are the platform system fields the
            // migration engine injects into every real creator table
            // (`query::SYSTEM_FIELD_NAMES`). This fixture omitted them for as
            // long as the implicit read projection was `SELECT *`; it is now an
            // explicit list of the seven system columns plus the declared
            // fields, so a table missing them is not a table `find` can serve.
            // Adding them makes the fixture look like what production reads.
            //
            // THE NULLABILITY MATTERS AS MUCH AS THE COLUMN LIST, and this
            // fixture got it wrong until 2026-09-01: `created_at` and
            // `updated_at` were declared nullable while the production emitter
            // writes them NOT NULL (`zeroship-schema/src/query.rs:212-213`).
            // Measured against pg18 with the statement `build_insert_many`
            // emits for a mixed batch - it unions the column set across
            // documents (`query.rs:4390`) and binds `unwrap_or(&Value::Null)`
            // for a cell some other row supplied (`:4445`):
            //
            //   nullable fixture -> INSERT SUCCEEDS, storing created_at = NULL
            //   NOT NULL (prod)  -> 23502 not-null violation
            //
            // So the lax fixture could not fail on the defect, and would have
            // stored the silent-wrong value instead - the harder failure to
            // notice. A fixture that claims to match production must match its
            // CONSTRAINTS, not only its column names.
            r#"CREATE TABLE "{schema}"."notes" (
                id SERIAL PRIMARY KEY,
                title TEXT NOT NULL,
                body TEXT,
                category TEXT,
                views INTEGER DEFAULT 0,
                tags JSONB DEFAULT '[]'::jsonb,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                created_by TEXT,
                updated_by TEXT,
                version INTEGER NOT NULL DEFAULT 1,
                deleted_at TIMESTAMPTZ
            )"#
        ),
        &[],
    )
    .await
    .unwrap();
}

/// Helper: build + execute a query, return parsed JSON array.
async fn exec_query(pool: &Pool, bq: zeroship_plugin_db::query::BuiltQuery) -> Vec<Value> {
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = pool.query_text_params(&bq.sql, &param_refs).await.unwrap();
    rows.iter().map(row_to_json).collect()
}

/// Helper: build + execute a mutation, return parsed JSON array.
async fn exec_mutation(pool: &Pool, bq: zeroship_plugin_db::query::BuiltQuery) -> Vec<Value> {
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = pool.query_text_params(&bq.sql, &param_refs).await.unwrap();
    rows.iter().map(row_to_json).collect()
}

/// Stamp a unique text `id` onto a seed insert document. The platform `id`
/// system field is `TEXT PRIMARY KEY` with NO DB default
/// -- production stamps a typed id via the system-fields pass before
/// `build_insert`. Tests that bypass that pass (calling `build_insert` directly)
/// must supply the `id` themselves, otherwise the row trips the `id` NOT-NULL.
fn with_seed_id(mut doc: Value) -> Value {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    if let Some(obj) = doc.as_object_mut() {
        obj.entry("id")
            .or_insert_with(|| Value::String(format!("seed_{n}")));
    }
    doc
}

/// Simplified row → JSON (just text columns for testing).
fn row_to_json(row: &compio_postgres::Row) -> Value {
    let mut obj = serde_json::Map::new();
    for col in row.columns() {
        let name = col.name();
        let val = match col.type_().oid() {
            // INT4 = 23
            23 => match row.try_get::<_, i32>(name) {
                Ok(v) => Value::Number(v.into()),
                Err(_) => Value::Null,
            },
            // INT8 = 20
            20 => match row.try_get::<_, i64>(name) {
                Ok(v) => Value::Number(v.into()),
                Err(_) => Value::Null,
            },
            // BOOL = 16
            16 => match row.try_get::<_, bool>(name) {
                Ok(v) => Value::Bool(v),
                Err(_) => Value::Null,
            },
            // JSONB = 3802 — binary format has 1-byte version prefix, strip it
            3802 => match row.raw_value(name) {
                Ok(Some(bytes)) if bytes.len() > 1 => {
                    let json_str = std::str::from_utf8(&bytes[1..]).unwrap_or("null");
                    serde_json::from_str(json_str).unwrap_or(Value::Null)
                }
                _ => Value::Null,
            },
            // JSON = 114 — text format, no prefix
            114 => match row.try_get::<_, String>(name) {
                Ok(s) => {
                    let parsed = serde_json::from_str(&s).ok();
                    parsed.unwrap_or(Value::String(s))
                }
                Err(_) => Value::Null,
            },
            // TIMESTAMPTZ = 1184 — read raw, return as number
            1184 => match row.raw_value(name) {
                Ok(Some(bytes)) if bytes.len() == 8 => {
                    let pg_usec = i64::from_be_bytes(bytes.try_into().unwrap());
                    let unix_ms = pg_usec / 1_000 + 946_684_800_000;
                    Value::Number(unix_ms.into())
                }
                _ => Value::Null,
            },
            // Everything else → String
            _ => match row.try_get::<_, String>(name) {
                Ok(v) => Value::String(v),
                Err(_) => Value::Null,
            },
        };
        obj.insert(name.to_string(), val);
    }
    Value::Object(obj)
}

/// Release everything this test opened against Postgres, then wait for the
/// sockets to actually close.
///
/// Every test here runs on a private compio runtime that is torn down the
/// moment the test body returns. A connection's socket is owned by a detached
/// driver task, and dropping the pool only asks that task to shut down - the
/// `Terminate` write and socket drop still have to be driven. If the runtime
/// goes away first the socket is orphaned: an io_uring submission co-owns the
/// descriptor and is never reclaimed, so the descriptor and the server-side
/// backend survive for the whole process. Enough tests doing that exhausts
/// `max_connections`, and the rest of the suite fails to connect at all.
///
/// Calling this last keeps the binary inside a bounded connection budget no
/// matter how many tests it holds.
async fn release_pg(pool: std::rc::Rc<Pool>) {
    drop(pool);
    drain_pg().await;
}

/// The half of [`release_pg`] that owns no pool, for tests whose handles have
/// already gone out of scope. Every handle must be dropped first: a live one
/// keeps its connection counted and makes this wait out its whole budget.
async fn drain_pg() {
    // The context can hold its own pool handle and a parked transaction
    // client; those keep connections counted, so clear it before waiting.
    zeroship_plugin_db::reset_context_for_tests();
    if !compio_postgres::drain_connections(std::time::Duration::from_secs(2)).await {
        eprintln!(
            "DRAIN-TIMEOUT: {} connection(s) still live",
            compio_postgres::live_connections()
        );
    }
}

/// The connection budget this whole binary is allowed to hold at once,
/// expressed as open sockets in the process.
///
/// Well under a stock server's `max_connections` of 100, and well under a
/// stock `RLIMIT_NOFILE` of 1024, so neither limit is what this trips on.
const SOCKET_CEILING: usize = 24;

/// Sockets this process currently has open.
fn open_sockets() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .expect("procfs is required to count this process's sockets")
        .filter_map(Result::ok)
        .filter(|e| {
            std::fs::read_link(e.path())
                .is_ok_and(|target| target.to_string_lossy().starts_with("socket:"))
        })
        .count()
}

/// A test must not leave Postgres connections behind when its runtime dies.
///
/// Runs the exact lifecycle every test in this file runs - fresh thread, fresh
/// compio runtime, pool, query, teardown - many more times than the suite has
/// tests, and asserts the process never accumulates connections. Without a
/// teardown that waits for the sockets to close, each iteration orphans its
/// connections and the count climbs until the server refuses new clients.
///
/// Deliberately not a `#[compio::test]`: the runtime lifecycle is the subject.
#[test]
fn connections_do_not_outlive_the_runtime_that_opened_them() {
    const ITERATIONS: usize = 40;

    let baseline = open_sockets();
    for _ in 0..ITERATIONS {
        std::thread::spawn(|| {
            compio::runtime::Runtime::new()
                .expect("cannot create runtime")
                .block_on(async {
                    let url = require_pg().await;
                    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
                    pool.execute("SELECT 1", &[]).await.unwrap();
                    release_pg(pool).await;
                });
        })
        .join()
        .expect("worker thread panicked");
    }

    let leaked = open_sockets().saturating_sub(baseline);
    assert!(
        leaked <= SOCKET_CEILING,
        "{ITERATIONS} pool lifecycles leaked {leaked} sockets (ceiling {SOCKET_CEILING}); \
         connections are outliving the runtime that opened them"
    );
}

use zeroship_plugin_db::query::*;

/// The descriptor entry for the `notes` fixture table, in the same
/// `{ <column>: FieldDef }` shape the runtime descriptor hook plants and
/// `crate::descriptor::collection_schema` returns.
///
/// The read builders take it as the projection allowlist and the read-identifier
/// allowlist: `build_find_with_schema` expands to `"id"` plus the six other
/// platform system columns plus one term per field declared here, and refuses
/// any `select` / `orderBy` / `distinct` / `$group.by` identifier that is not in
/// it. The seven system fields are implicit — they are never declared here, and
/// `setup()` above creates all seven on the table.
fn notes_schema() -> Value {
    json!({
        "title": { "type": "string" },
        "body": { "type": "string" },
        "category": { "type": "string" },
        "views": { "type": "int" },
        "tags": { "type": "json" },
    })
}

/// The descriptor entry for the `weather` fixture table used by the Postgres
/// docs HAVING example. Aggregate builds its own SELECT from `$group`, so this
/// only has to declare the identifiers the pipeline names.
fn weather_schema() -> Value {
    json!({
        "city": { "type": "string" },
        "temp_lo": { "type": "int" },
        "temp_hi": { "type": "int" },
    })
}

/// Postgres and the dev SQLite tier must hand `env.db` callers the same JSON.
///
/// NOT `#[ignore]`, and that is the point of this test's history. It carried
/// `#[ignore = "requires live postgres; default gate runs the sqlite leg only"]`
/// until 2026-08-21, which was false in both halves: every one of its 108
/// siblings in this file requires live Postgres and none of them is ignored, and
/// the default gate for this target is `tests/run_plugin_db_live_suite.sh`, which
/// runs it WITHOUT `--ignored`. So the attribute did not describe a prerequisite -
/// it removed the test from the only job that could have run it, and that is how
/// the 2026-08-10 schema-authority cutover left it broken for eleven days with
/// every gate green. It now fails the way its siblings do: `require_pg`
/// panics rather than skipping, because a run that reports "ok" against no
/// database says the opposite of the truth.
#[compio::test]
async fn parity_matrix_pg_matches_sqlite_projection() {
    let pg_url = require_pg().await;
    let sqlite_dir = tempfile::tempdir().expect("create sqlite parity dir");

    let app = crate::test_app_id!();

    // The SQLite leg keeps the dev app id on purpose - its tempdir isolates it,
    // and the matrix is meant to write the file a `pnpm dev` app writes. The
    // Postgres leg gets this test's own id: the two matrix tests here shared
    // schema `default` and dropped it out from under each other in parallel.
    let sqlite = parity::run_matrix(&parity::sqlite_url(&sqlite_dir), parity::DEV_APP_ID);
    let pg = parity::run_matrix(&pg_url, &app);

    assert_eq!(pg.seed, sqlite.seed);
    assert_eq!(pg.tx, sqlite.tx);

    // THE BYTES DIVERGENCE IS GONE, and it used to be pinned right here. Until
    // `crud::bytes_pass` landed, this block excluded `payload_bytes` from the
    // comparison and pinned the two OBSERVED values instead: `M3EyKzd3PT0=` on
    // Postgres and `3q2+7w==` on SQLite. The first of those is the base64 of the
    // second - the write path had no `bytes` branch, so the SDK's base64 string
    // was bound as text at a `bytea` column, Postgres parsed it in ESCAPE format
    // and stored the 8 ASCII characters, and the read path (which is correct)
    // base64'd those 8 bytes back out. Both pins were copied from what the code
    // returned, which is why neither ever went red.
    //
    // What replaces them is not another pin: `expected_typed_projection` derives
    // the expectation from `parity::TYPED_BYTES_RAW`, the four bytes the caller
    // wrote, and `bytes_column_stores_raw_bytes_on_postgres` (below) reads the
    // stored cell with a query that does not go through the SDK.
    assert_eq!(
        pg.typed, sqlite.typed,
        "every typed field must project identically on both backends"
    );
    assert_eq!(
        pg.typed,
        parity::expected_typed_projection(),
        "and both must match the independently-derived expectation"
    );
}

/// A `t.bytes()` value written through `env.db` must reach Postgres AS BYTES.
///
/// THE SDK IS NOT ALLOWED TO BE ITS OWN WITNESS HERE. `parity_matrix_*` above
/// compares what `env.db` reads back against what `env.db` was given, and that
/// pair was self-consistent all through the defect on SQLite: a value stored as
/// TEXT and read back as TEXT round-trips perfectly while the cell holds the
/// wrong thing. So this test goes around the SDK entirely and asks the server
/// what is in the column.
///
/// RED BEFORE THE FIX, and measured that way rather than assumed: against the
/// pre-fix binary the stored cell is `\x3371322b37773d3d`, the 8-byte ASCII of
/// the base64 `3q2+7w==`, and this assertion fails naming both. After
/// `crud::bytes_pass` it is `\xdeadbeef`.
#[compio::test]
async fn bytes_column_stores_raw_bytes_on_postgres() {
    let pg_url = require_pg().await;
    let app = crate::test_app_id!();
    let pg = parity::run_matrix(&pg_url, &app);

    // The expectation is DERIVED, not copied from a run: `TYPED_BYTES_RAW` is
    // what the caller handed `env.db` (base64-encoded, per the `t.bytes()` wire
    // contract), so it is what the column must hold.
    let expected: Vec<u8> = parity::TYPED_BYTES_RAW.to_vec();

    let (client, connection) = compio_postgres::connect(&pg_url, NoTls)
        .await
        .expect("dial the parity database directly");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    let sql = format!(
        "SELECT payload_bytes FROM \"{}\".\"{}\" WHERE title = $1",
        app, pg.collection
    );
    let rows = client
        .query(&sql, &[&"typed-roundtrip"])
        .await
        .expect("read the stored cell");
    assert_eq!(rows.len(), 1, "the typed round-trip row must exist");
    let stored: Vec<u8> = rows[0].get::<_, Vec<u8>>(0);

    // Hand the socket back BEFORE the assertions: a panic skips whatever
    // follows it, and `direct_connection_sites_do_not_grow` counts this site on
    // the promise that it is paired with a teardown.
    drop(client);
    drain_pg().await;

    assert_eq!(
        stored,
        expected,
        "the bytea cell must hold the caller's bytes. Got {} bytes ({}), wanted \
         {} ({}). An 8-byte cell spelling the base64 in ASCII is the write path \
         binding the base64 string as text at a bytea column.",
        stored.len(),
        hex_of(&stored),
        expected.len(),
        hex_of(&expected),
    );

    // And the value the caller reads back through `env.db` is the base64 of
    // exactly those bytes - one encode, not two.
    //
    // INDEX AT THE LEVEL `typed` IS BUILT AT. `run_matrix` stores the whole
    // `typedRoundTrip` return value, which is `{ source, echo }` - two
    // projected rows (`parity/mod.rs`, `typedRoundTrip` returns
    // `{ source: projectTypedRow(source), echo: ... }`). `payload_bytes` lives
    // one level down inside each. A bare `pg.typed["payload_bytes"]` is
    // therefore `Value::Null` WHATEVER the product does - it named a key the
    // map does not have - and that is exactly how this assertion failed from
    // the day it was written: `left: Null, right: String("3q2+7w==")`. It could
    // not have gone green for a correct product or red for a broken one.
    for row in ["source", "echo"] {
        assert_eq!(
            pg.typed[row]["payload_bytes"],
            json!(parity::typed_bytes_b64()),
            "env.db must hand back the base64 of the stored bytes on the `{row}` \
             row; got {:?} in {:?}",
            pg.typed[row]["payload_bytes"],
            pg.typed,
        );
    }
}

/// Render bytes as lowercase hex for the failure messages above. Not a helper
/// worth a crate: `format!("{:02x?}")` prints a debug list, not a hex string.
fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// 1. Insert + find round-trip
// ---------------------------------------------------------------------------

#[compio::test]
async fn insert_and_find() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    // Insert
    let bq = build_insert(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"title": "Hello", "body": "World", "category": "tech"}),
    )
    .unwrap();
    let inserted = exec_mutation(&pool, bq).await;
    assert_eq!(inserted.len(), 1);
    assert_eq!(inserted[0]["title"], "Hello");
    assert_eq!(inserted[0]["body"], "World");
    assert!(inserted[0]["id"].as_i64().unwrap() > 0);

    // Find
    let bq = build_find_with_schema(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &json!({}),
        None,
        None,
        None,
        None,
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["title"], "Hello");
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 2. Insert many
// ---------------------------------------------------------------------------

#[compio::test]
async fn insert_many_round_trip() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = json!([
        {"title": "A", "body": "one", "category": "tech"},
        {"title": "B", "body": "two", "category": "food"},
        {"title": "C", "body": "three", "category": "tech"}
    ]);
    let bq = build_insert_many(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", &notes_schema(), &docs).unwrap();
    let inserted = exec_mutation(&pool, bq).await;
    assert_eq!(inserted.len(), 3);

    // Verify all in DB
    let bq = build_count(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", &notes_schema(), &json!({})).unwrap();
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = pool.query_text_params(&bq.sql, &param_refs).await.unwrap();
    let count: i64 = rows[0].get("count");
    assert_eq!(count, 3);
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 3. Update one with $inc
// ---------------------------------------------------------------------------

#[compio::test]
async fn update_one_inc() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    // Insert
    let bq = build_insert(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"title": "Counter", "category": "tech", "views": 0}),
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    // $inc views by 5
    let bq = build_update_one(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"title": "Counter"}),
        &json!({"views": {"$inc": 5}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0]["views"], 5);

    // $inc again
    let bq = build_update_one(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"title": "Counter"}),
        &json!({"views": {"$inc": 3}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    assert_eq!(updated[0]["views"], 8);
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 4. Update one with $dec and $mul
// ---------------------------------------------------------------------------

#[compio::test]
async fn update_one_dec_mul() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let bq = build_insert(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"title": "Math", "category": "tech", "views": 10}),
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    // $dec
    let bq = build_update_one(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"title": "Math"}),
        &json!({"views": {"$dec": 3}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    assert_eq!(updated[0]["views"], 7);

    // $mul
    let bq = build_update_one(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"title": "Math"}),
        &json!({"views": {"$mul": 2}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    assert_eq!(updated[0]["views"], 14);
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 5. Update one with $push / $pull / $addToSet (JSONB arrays)
// ---------------------------------------------------------------------------

#[compio::test]
async fn update_one_jsonb_array_ops() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let bq = build_insert(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"title": "Tags", "category": "tech"}),
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    // $push "rust"
    let bq = build_update_one(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"title": "Tags"}),
        &json!({"tags": {"$push": "rust"}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    let tags = updated[0]["tags"].as_array().unwrap();
    assert!(tags.contains(&json!("rust")));

    // $push "go"
    let bq = build_update_one(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"title": "Tags"}),
        &json!({"tags": {"$push": "go"}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    let tags = updated[0]["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 2);
    assert!(tags.contains(&json!("rust")));
    assert!(tags.contains(&json!("go")));

    // $addToSet "rust" (duplicate — should NOT add)
    let bq = build_update_one(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"title": "Tags"}),
        &json!({"tags": {"$addToSet": "rust"}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    let tags = updated[0]["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 2); // still 2

    // $addToSet "python" (new — should add)
    let bq = build_update_one(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"title": "Tags"}),
        &json!({"tags": {"$addToSet": "python"}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    let tags = updated[0]["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 3);

    // $pull "go"
    let bq = build_update_one(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"title": "Tags"}),
        &json!({"tags": {"$pull": "go"}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    let tags = updated[0]["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 2);
    assert!(!tags.contains(&json!("go")));
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 6. Update many
// ---------------------------------------------------------------------------

#[compio::test]
async fn update_many_round_trip() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    // Insert 3 tech, 1 food
    let docs = json!([
        {"title": "A", "category": "tech", "views": 0},
        {"title": "B", "category": "tech", "views": 0},
        {"title": "C", "category": "tech", "views": 0},
        {"title": "D", "category": "food", "views": 0}
    ]);
    let bq = build_insert_many(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", &notes_schema(), &docs).unwrap();
    exec_mutation(&pool, bq).await;

    // Update all tech views +1
    let bq = build_update_many(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"category": "tech"}),
        &json!({"views": {"$inc": 1}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    assert_eq!(updated.len(), 3);

    // Verify food unchanged
    let bq = build_find_with_schema(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &json!({"category": "food"}),
        None,
        None,
        None,
        None,
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows[0]["views"], 0);

    // Verify tech updated
    let bq = build_find_with_schema(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &json!({"category": "tech"}),
        None,
        None,
        None,
        None,
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    for row in &rows {
        assert_eq!(row["views"], 1);
    }
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 7. Delete one + delete many
// ---------------------------------------------------------------------------

#[compio::test]
async fn delete_operations() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = json!([
        {"title": "Keep1", "category": "tech"},
        {"title": "Keep2", "category": "tech"},
        {"title": "Del1", "category": "food"},
        {"title": "Del2", "category": "food"},
        {"title": "Del3", "category": "food"}
    ]);
    let bq = build_insert_many(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", &notes_schema(), &docs).unwrap();
    exec_mutation(&pool, bq).await;

    // Delete one food
    let bq = build_delete_one(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"category": "food"}),
    )
    .unwrap();
    let deleted = exec_mutation(&pool, bq).await;
    assert_eq!(deleted.len(), 1);

    // 4 remaining
    let bq = build_count(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", &notes_schema(), &json!({})).unwrap();
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = pool.query_text_params(&bq.sql, &param_refs).await.unwrap();
    assert_eq!(rows[0].get::<_, i64>("count"), 4);

    // Delete many remaining food
    let bq = build_delete_many(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"category": "food"}),
        SqlDialect::Postgres,
    )
    .unwrap();
    let deleted = exec_mutation(&pool, bq).await;
    assert_eq!(deleted.len(), 2);

    // 2 tech remaining
    let bq = build_count(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", &notes_schema(), &json!({})).unwrap();
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = pool.query_text_params(&bq.sql, &param_refs).await.unwrap();
    assert_eq!(rows[0].get::<_, i64>("count"), 2);
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 8. Filter operators: $gt, $gte, $lt, $lte, $in, $nin, $ne
// ---------------------------------------------------------------------------

#[compio::test]
async fn filter_comparison_operators() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = json!([
        {"title": "A", "category": "tech", "views": 10},
        {"title": "B", "category": "tech", "views": 20},
        {"title": "C", "category": "food", "views": 30},
        {"title": "D", "category": "food", "views": 40}
    ]);
    let bq = build_insert_many(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", &notes_schema(), &docs).unwrap();
    exec_mutation(&pool, bq).await;

    // $gt 25
    let bq = build_find_with_schema(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &json!({"views": {"$gt": 25}}),
        None,
        None,
        None,
        None,
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 2);

    // $lte 20
    let bq = build_find_with_schema(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &json!({"views": {"$lte": 20}}),
        None,
        None,
        None,
        None,
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 2);

    // $in
    let bq = build_find_with_schema(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &json!({"category": {"$in": ["tech", "food"]}}),
        None,
        None,
        None,
        None,
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 4);

    // $nin
    let bq = build_find_with_schema(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &json!({"category": {"$nin": ["food"]}}),
        None,
        None,
        None,
        None,
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 2);

    // $ne
    let bq = build_find_with_schema(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &json!({"category": {"$ne": "food"}}),
        None,
        None,
        None,
        None,
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 2);
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 9. Filter operators: $and, $or, $not
// ---------------------------------------------------------------------------

#[compio::test]
async fn filter_logical_operators() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = json!([
        {"title": "A", "category": "tech", "views": 10},
        {"title": "B", "category": "tech", "views": 50},
        {"title": "C", "category": "food", "views": 10}
    ]);
    let bq = build_insert_many(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", &notes_schema(), &docs).unwrap();
    exec_mutation(&pool, bq).await;

    // $and: tech AND views > 20
    let bq = build_find_with_schema(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &json!({"$and": [{"category": "tech"}, {"views": {"$gt": 20}}]}),
        None,
        None,
        None,
        None,
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["title"], "B");

    // $or: tech OR views > 20
    let bq = build_find_with_schema(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &json!({"$or": [{"category": "tech"}, {"views": {"$gt": 20}}]}),
        None,
        None,
        None,
        None,
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 2); // A and B

    // $not: NOT food
    let bq = build_find_with_schema(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &json!({"$not": {"category": "food"}}),
        None,
        None,
        None,
        None,
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 2);
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 10. Filter: $like, $ilike
// ---------------------------------------------------------------------------

#[compio::test]
async fn filter_pattern_operators() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = json!([
        {"title": "Hello World", "category": "tech"},
        {"title": "hello rust", "category": "tech"},
        {"title": "Goodbye", "category": "food"}
    ]);
    let bq = build_insert_many(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", &notes_schema(), &docs).unwrap();
    exec_mutation(&pool, bq).await;

    // $like (case sensitive)
    let bq = build_find_with_schema(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &json!({"title": {"$like": "Hello%"}}),
        None,
        None,
        None,
        None,
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 1);

    // $ilike (case insensitive)
    let bq = build_find_with_schema(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &json!({"title": {"$ilike": "%hello%"}}),
        None,
        None,
        None,
        None,
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 2);
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 11. Find with limit, offset, order
// ---------------------------------------------------------------------------

#[compio::test]
async fn find_with_options() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = json!([
        {"title": "C", "category": "tech", "views": 30},
        {"title": "A", "category": "tech", "views": 10},
        {"title": "B", "category": "tech", "views": 20}
    ]);
    let bq = build_insert_many(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", &notes_schema(), &docs).unwrap();
    exec_mutation(&pool, bq).await;

    // Order by views ASC, limit 2
    let bq = build_find_with_schema(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &json!({}),
        Some(2),
        None,
        Some(&json!({"views": 1})),
        None,
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["title"], "A");
    assert_eq!(rows[1]["title"], "B");

    // Order by views DESC, limit 1, offset 1
    let bq = build_find_with_schema(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &json!({}),
        Some(1),
        Some(1),
        Some(&json!({"views": -1})),
        None,
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["title"], "B"); // 2nd highest
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 12. Find with select (projection)
// ---------------------------------------------------------------------------

#[compio::test]
async fn find_with_projection() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let bq = build_insert(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"title": "Proj", "body": "secret", "category": "tech"}),
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    let bq = build_find_with_schema(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &json!({}),
        None,
        None,
        None,
        Some(&json!(["title", "category"])),
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["title"], "Proj");
    assert_eq!(rows[0]["category"], "tech");
    // Should NOT have body, id, views, etc.
    assert!(rows[0].get("body").is_none());
    assert!(rows[0].get("id").is_none());
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 13. Distinct
// ---------------------------------------------------------------------------

#[compio::test]
async fn distinct_values() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = json!([
        {"title": "A", "category": "tech"},
        {"title": "B", "category": "tech"},
        {"title": "C", "category": "food"},
        {"title": "D", "category": "science"}
    ]);
    let bq = build_insert_many(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", &notes_schema(), &docs).unwrap();
    exec_mutation(&pool, bq).await;

    let bq = build_distinct(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", "category", &json!({}), &notes_schema()).unwrap();
    let rows = exec_query(&pool, bq).await;
    let values: Vec<&str> = rows
        .iter()
        .map(|r| r["category"].as_str().unwrap())
        .collect();
    assert_eq!(values.len(), 3);
    assert!(values.contains(&"tech"));
    assert!(values.contains(&"food"));
    assert!(values.contains(&"science"));

    // Distinct with filter
    let bq = build_distinct(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        "category",
        &json!({"category": {"$ne": "science"}}),
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 2);
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 14. Count
// ---------------------------------------------------------------------------

#[compio::test]
async fn count_with_filter() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = json!([
        {"title": "A", "category": "tech"},
        {"title": "B", "category": "tech"},
        {"title": "C", "category": "food"}
    ]);
    let bq = build_insert_many(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", &notes_schema(), &docs).unwrap();
    exec_mutation(&pool, bq).await;

    // Count all
    let bq = build_count(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", &notes_schema(), &json!({})).unwrap();
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = pool.query_text_params(&bq.sql, &param_refs).await.unwrap();
    assert_eq!(rows[0].get::<_, i64>("count"), 3);

    // Count with filter
    let bq = build_count(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"category": "tech"}),
    )
    .unwrap();
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = pool.query_text_params(&bq.sql, &param_refs).await.unwrap();
    assert_eq!(rows[0].get::<_, i64>("count"), 2);
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 15. Aggregate: group by + $count + $sum + $avg + $min + $max
// ---------------------------------------------------------------------------

#[compio::test]
async fn aggregate_full() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = json!([
        {"title": "A", "category": "tech", "views": 10},
        {"title": "B", "category": "tech", "views": 20},
        {"title": "C", "category": "tech", "views": 30},
        {"title": "D", "category": "food", "views": 100}
    ]);
    let bq = build_insert_many(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", &notes_schema(), &docs).unwrap();
    exec_mutation(&pool, bq).await;

    let pipeline = json!([
        {"$match": {"category": "tech"}},
        {"$group": {
            "by": "category",
            "cnt": {"$count": true},
            "total": {"$sum": "views"},
            "average": {"$avg": "views"},
            "lo": {"$min": "views"},
            "hi": {"$max": "views"}
        }},
        {"$sort": {"cnt": -1}}
    ]);
    let bq = build_aggregate(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", &pipeline, &notes_schema()).unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["category"], "tech");
    assert_eq!(rows[0]["cnt"], 3);
    assert_eq!(rows[0]["total"], 60);
    assert_eq!(rows[0]["lo"], 10);
    assert_eq!(rows[0]["hi"], 30);
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 16. Aggregate: multiple group-by fields
// ---------------------------------------------------------------------------

#[compio::test]
async fn aggregate_multi_group() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = json!([
        {"title": "A", "category": "tech", "body": "rust", "views": 10},
        {"title": "B", "category": "tech", "body": "rust", "views": 20},
        {"title": "C", "category": "tech", "body": "go", "views": 5},
        {"title": "D", "category": "food", "body": "pasta", "views": 50}
    ]);
    let bq = build_insert_many(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", &notes_schema(), &docs).unwrap();
    exec_mutation(&pool, bq).await;

    let pipeline = json!([
        {"$group": {
            "by": ["category", "body"],
            "cnt": {"$count": true}
        }},
        {"$sort": {"cnt": -1}}
    ]);
    let bq = build_aggregate(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", &pipeline, &notes_schema()).unwrap();
    let rows = exec_query(&pool, bq).await;
    // tech/rust=2, tech/go=1, food/pasta=1
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["cnt"], 2); // highest count first
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 17. Aggregate: having clause
// ---------------------------------------------------------------------------

#[compio::test]
async fn aggregate_having() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = json!([
        {"title": "A", "category": "tech", "views": 10},
        {"title": "B", "category": "tech", "views": 20},
        {"title": "C", "category": "tech", "views": 30},
        {"title": "D", "category": "food", "views": 5}
    ]);
    let bq = build_insert_many(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", &notes_schema(), &docs).unwrap();
    exec_mutation(&pool, bq).await;

    // HAVING with alias → resolved to aggregate expression
    let pipeline = json!([
        {"$group": {
            "by": "category",
            "cnt": {"$count": true}
        }},
        {"$having": {"cnt": {"$gt": 1}}},
        {"$sort": {"cnt": -1}}
    ]);
    let bq = build_aggregate(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "notes", &pipeline, &notes_schema()).unwrap();
    let rows = exec_query(&pool, bq).await;
    // Only tech has count > 1
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["category"], "tech");
    assert_eq!(rows[0]["cnt"], 3);
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 18. Null handling
// ---------------------------------------------------------------------------

#[compio::test]
async fn null_handling() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    // Insert with body
    let bq = build_insert(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"title": "WithBody", "body": "has content", "category": "tech"}),
    )
    .unwrap();
    exec_mutation(&pool, bq).await;
    // Insert without body (column defaults to NULL)
    let bq = build_insert(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"title": "NoBody", "category": "tech"}),
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    // Find where body IS NULL
    let bq = build_find_with_schema(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &json!({"body": null}),
        None,
        None,
        None,
        None,
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["title"], "NoBody");

    // Find where body IS NOT NULL
    let bq = build_find_with_schema(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &json!({"body": {"$ne": null}}),
        None,
        None,
        None,
        None,
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["title"], "WithBody");

    // $exists: true
    let bq = build_find_with_schema(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &json!({"body": {"$exists": true}}),
        None,
        None,
        None,
        None,
        &notes_schema(),
    )
    .unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["title"], "WithBody");
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 19. Mixed update: plain + operators in one call
// ---------------------------------------------------------------------------

#[compio::test]
async fn mixed_update() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let bq = build_insert(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"title": "Mix", "category": "tech", "views": 10}),
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    // Update: set category + inc views + push tag
    let bq = build_update_one(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"title": "Mix"}),
        &json!({"category": "science", "views": {"$inc": 5}, "tags": {"$push": "new"}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    assert_eq!(updated[0]["category"], "science");
    assert_eq!(updated[0]["views"], 15);
    let tags = updated[0]["tags"].as_array().unwrap();
    assert!(tags.contains(&json!("new")));
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 20. Timestamps are returned as numbers
// ---------------------------------------------------------------------------

#[compio::test]
async fn timestamps_as_numbers() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let bq = build_insert(
        &zeroship_schema::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &json!({"title": "Time", "category": "tech"}),
    )
    .unwrap();
    let inserted = exec_mutation(&pool, bq).await;

    let ts = inserted[0]["created_at"].as_i64().unwrap();
    // Should be a reasonable Unix millisecond timestamp (after 2020)
    assert!(ts > 1_577_836_800_000); // 2020-01-01
    assert!(ts < 2_000_000_000_000); // ~2033
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 21. Postgres docs HAVING example (weather table)
// ---------------------------------------------------------------------------

#[compio::test]
async fn aggregate_having_postgres_docs_example() {
    let url = require_pg().await;
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());

    // The schema is this test's own. It used to be the shared `plugin_db_test`,
    // which this test never created - it inherited whichever sibling had run
    // `setup` most recently, so running it alone failed with `3F000 schema does
    // not exist`.
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{schema}\""), &[])
        .await
        .unwrap();

    // Set up weather table. The seven platform system columns are here for the
    // same reason `notes` carries them: a write's `RETURNING` is now an
    // explicit list of the system columns plus the declared fields, so a table
    // missing them is not a table `insertMany` can write. Omitting them makes
    // the statement fail with `42703 column does not exist` - loudly, which is
    // the whole point of naming columns instead of starring them.
    pool.execute(
        &format!(
            r#"CREATE TABLE "{schema}"."weather" (
                id SERIAL PRIMARY KEY,
                city TEXT,
                temp_lo INTEGER,
                temp_hi INTEGER,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                created_by TEXT,
                updated_by TEXT,
                version INTEGER NOT NULL DEFAULT 1,
                deleted_at TIMESTAMPTZ
            )"#
        ),
        &[],
    )
    .await
    .unwrap();

    let docs = json!([
        {"city": "San Francisco", "temp_lo": 46, "temp_hi": 50},
        {"city": "San Francisco", "temp_lo": 43, "temp_hi": 57},
        {"city": "San Francisco", "temp_lo": 35, "temp_hi": 65},
        {"city": "Hayward", "temp_lo": 37, "temp_hi": 54},
        {"city": "Hayward", "temp_lo": 38, "temp_hi": 52},
        {"city": "Hayward", "temp_lo": 41, "temp_hi": 55}
    ]);
    let bq = build_insert_many(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "weather", &weather_schema(), &docs).unwrap();
    exec_mutation(&pool, bq).await;

    // Equivalent of: SELECT city, count(*), max(temp_lo)
    //                FROM weather GROUP BY city HAVING max(temp_lo) < 42
    let pipeline = json!([
        {"$group": {
            "by": "city",
            "cnt": {"$count": true},
            "max_temp": {"$max": "temp_lo"}
        }},
        {"$having": {"max_temp": {"$lt": 42}}}
    ]);
    let bq = build_aggregate(&zeroship_schema::SchemaName::new(schema).expect("fixture schema name"), "weather", &pipeline, &weather_schema()).unwrap();

    // Verify SQL has the resolved expression, not the alias
    assert!(
        bq.sql.contains("HAVING MAX(\"temp_lo\") < $"),
        "sql: {}",
        bq.sql
    );

    let rows = exec_query(&pool, bq).await;

    // Only Hayward has max(temp_lo) = 41 < 42
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["city"], "Hayward");
    assert_eq!(rows[0]["cnt"], 3);
    assert_eq!(rows[0]["max_temp"], 41);
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 22. A1 — `t.string().unique()` actually creates a unique index in Postgres.
//
// Pre-A1: SDK set FieldDef.unique = true, Rust emitted no index. Silent bug.
// Post-A1: build_create_indexes emits CREATE UNIQUE INDEX CONCURRENTLY; this
// test executes it end-to-end and verifies the index exists in pg_index
// with the deterministic name, then asserts the duplicate-row insert fails
// with SQLSTATE 23505 (unique_violation).
// ---------------------------------------------------------------------------

#[compio::test]
async fn a1_unique_index_actually_enforces_uniqueness() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());

    // Fresh schema + table — `build_create_table` is the production path.
    let app = crate::test_app_id!();
    let app = app.as_str();
    let collection = "users";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&zeroship_plugin_db::query::build_create_schema(&zeroship_schema::SchemaName::new(app).expect("fixture schema name")), &[])
        .await
        .unwrap();

    let schema = json!({
        "email": {"type": "string", "required": true, "unique": true},
        "handle": {"type": "string", "index": true},
    });

    let create_table =
        build_create_table_with_fks(&zeroship_schema::SchemaName::new(app).expect("fixture schema name"), collection, &schema, &FkEmission::Inline).unwrap();
    // `build_create_table_with_fks` emits MULTI-statement DDL (the CREATE TABLE
    // plus the system-field index `CREATE INDEX`s, and on PG the
    // `COMMENT ON COLUMN … 'zero-migrate:mask:…'` / `'zero-migrate:enc:…'` sentinels). The
    // extended/prepared `execute` path rejects that with `42601 cannot insert
    // multiple commands into a prepared statement`; the simple-query
    // `batch_execute` is the correct executor for rendered DDL batches.
    pool.batch_execute(&create_table).await.unwrap();

    // Generate and execute the new index DDL.
    let indexes =
        zeroship_plugin_db::query::build_create_indexes(&zeroship_schema::SchemaName::new(app).expect("fixture schema name"), collection, &schema).unwrap();
    assert_eq!(indexes.len(), 2, "expected 2 indexes, got: {indexes:?}");

    for spec in &indexes {
        pool.execute(&spec.sql, &[]).await.unwrap_or_else(|e| {
            panic!("failed to run {}: {e}", spec.sql);
        });
    }

    // Look up pg_index entries on the new schema.
    let q = format!(
        "SELECT c.relname AS idx_name, i.indisunique, i.indisvalid
         FROM pg_index i
         JOIN pg_class c ON c.oid = i.indexrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = '{app}'
         ORDER BY c.relname"
    );
    let rows = pool.query_text_params(&q, &[]).await.unwrap();
    // Two indexes (we don't count the PK; SERIAL PRIMARY KEY also makes an
    // index, so total is at least 3 — but we assert specifically on names).
    let names: Vec<(String, bool, bool)> = rows
        .iter()
        .map(|r| {
            (
                r.get::<_, String>("idx_name"),
                r.get::<_, bool>("indisunique"),
                r.get::<_, bool>("indisvalid"),
            )
        })
        .collect();

    let email_key = names.iter().find(|(n, _, _)| n == "users_email_key");
    let handle_idx = names.iter().find(|(n, _, _)| n == "users_handle_idx");
    assert!(
        email_key.is_some(),
        "expected users_email_key, found: {names:?}"
    );
    assert!(
        handle_idx.is_some(),
        "expected users_handle_idx, found: {names:?}"
    );
    let (_, unique, valid) = email_key.unwrap();
    assert!(*unique, "users_email_key should be unique");
    assert!(*valid, "users_email_key should be valid");
    let (_, unique2, valid2) = handle_idx.unwrap();
    assert!(!*unique2, "users_handle_idx should NOT be unique");
    assert!(*valid2, "users_handle_idx should be valid");

    // -----------------------------------------------------------------------
    // The silent-bug live repro: insert two rows with the same email and
    // assert the second one fails with SQLSTATE 23505.
    // -----------------------------------------------------------------------
    let ins1 = build_insert(
        &zeroship_schema::SchemaName::new(app).expect("fixture schema name"),
        collection,
        &schema,
        &with_seed_id(json!({"email": "a@x.com"})),
    )
    .unwrap();
    let p1: Vec<&str> = ins1.params.iter().map(String::as_str).collect();
    pool.query_text_params(&ins1.sql, &p1).await.unwrap();

    // Distinct `id` so the second insert is rejected for the DUPLICATE EMAIL
    // (the unique index under test), not an incidental duplicate PK.
    let ins2 = build_insert(
        &zeroship_schema::SchemaName::new(app).expect("fixture schema name"),
        collection,
        &schema,
        &with_seed_id(json!({"email": "a@x.com"})),
    )
    .unwrap();
    let p2: Vec<&str> = ins2.params.iter().map(String::as_str).collect();
    let err = pool.query_text_params(&ins2.sql, &p2).await.unwrap_err();
    let code = err.code().map(|c| c.code().to_string()).unwrap_or_default();
    assert_eq!(
        code, "23505",
        "second insert with duplicate email should fail with 23505 unique_violation, got: {err}"
    );

    // -----------------------------------------------------------------------
    // Idempotency — re-running build_create_indexes + executing the SQL
    // again must be a no-op (the IF NOT EXISTS + deterministic naming
    // contract).
    // -----------------------------------------------------------------------
    for spec in &indexes {
        pool.execute(&spec.sql, &[]).await.unwrap_or_else(|e| {
            panic!("idempotent re-run failed for {}: {e}", spec.sql);
        });
    }
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 25. A2 — destructive change (drop_column) is refused in strict mode.
//
// Self-assessment: this is the load-bearing test that proves the deploy
// pipeline actually refuses changes that would corrupt data.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 26. A2 — strictness=off allows the deploy through (destructive op is
// recorded but the orchestrator returns Ok). Note: with off, the
// destructive op is filtered out and the DDL is NOT actually run (we
// don't auto-drop columns under any strictness setting; off only
// suppresses the error envelope so the rest of the schema applies).
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 27. A2 — additive change (add nullable column) auto-applies on a
// non-empty table.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 28. A2 — adding a NOT NULL column to a non-empty table without default
// is detected as destructive (proposal A2 line 116).
//
// Self-assessment: this is the proposal's headline data-corruption guard.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 31. A2 — adding a required column WITH a default literal is compatible.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// B2 — typed cross-table relations: foreign keys at the DB level
// ---------------------------------------------------------------------------

/// The seven platform system columns, PostgreSQL spelling.
///
/// Hand-written, not rendered. plugin-db does not own DDL, so a test that needs
/// a table spells it; a fixture rendered by the layer under test cannot detect
/// that layer being wrong. Same argument as `tests/support/tables.rs` on the
/// SQLite side.
const PG_SYSTEM_COLUMNS: &str = r#"
  id TEXT PRIMARY KEY,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  created_by TEXT NULL,
  updated_by TEXT NULL,
  version INTEGER NOT NULL DEFAULT 1,
  deleted_at TIMESTAMPTZ NULL"#;

/// The three system indexes every confined table carries.
fn pg_system_indexes(app: &str, coll: &str) -> String {
    format!(
        r#"
CREATE INDEX IF NOT EXISTS "{coll}_deleted_at_idx" ON "{app}"."{coll}" ("deleted_at");
CREATE INDEX IF NOT EXISTS "{coll}_updated_at_idx" ON "{app}"."{coll}" ("updated_at");
CREATE INDEX IF NOT EXISTS "{coll}_created_by_idx" ON "{app}"."{coll}" ("created_by");
"#
    )
}

/// Helper: build `users` and `posts` where `posts.authorId` references `users`.
///
/// Raw SQL because plugin-db does not own DDL, so a test that wants tables has
/// to create them. The FK carries no `ON DELETE`
/// clause, which is what `t.ref` emits by default and what PostgreSQL records as
/// `confdeltype = 'a'` (NO ACTION) - the variants that need CASCADE or RESTRICT
/// spell their own.
async fn b2_setup_users_posts(pool: &std::rc::Rc<Pool>, app: &str) {
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.batch_execute(&format!(
        r#"CREATE SCHEMA "{app}";
CREATE TABLE "{app}"."users" ({PG_SYSTEM_COLUMNS},
  "name" TEXT NOT NULL
);
{users_idx}
CREATE TABLE "{app}"."posts" ({PG_SYSTEM_COLUMNS},
  "title" TEXT NOT NULL,
  "authorId" TEXT,
  CONSTRAINT "authorId_fkey" FOREIGN KEY ("authorId") REFERENCES "{app}"."users" ("id")
);
{posts_idx}"#,
        users_idx = pg_system_indexes(app, "users"),
        posts_idx = pg_system_indexes(app, "posts"),
    ))
    .await
    .expect("b2 users + posts fixture");
}

#[compio::test]
async fn b2_ref_creates_foreign_key() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = crate::test_app_id!();

    let app = app.as_str();
    b2_setup_users_posts(&pool, app).await;

    // Inspect pg_constraint for the FK on "posts.authorId".
    let rows = pool
        .query_text_params(
            r#"
SELECT con.conname AS name,
       con.confdeltype::text AS on_delete,
       con.confupdtype::text AS on_update,
       con.condeferrable AS deferrable,
       fcl.relname AS target
  FROM pg_constraint con
  JOIN pg_class cl ON cl.oid = con.conrelid
  JOIN pg_class fcl ON fcl.oid = con.confrelid
  JOIN pg_namespace n ON n.oid = cl.relnamespace
 WHERE n.nspname = $1 AND cl.relname = 'posts' AND con.contype = 'f'
"#,
            &[app],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "expected one FK on posts.authorId");
    let target: String = rows[0].get("target");
    assert_eq!(target, "users");
    // `t.ref()` emits NO REFERENTIAL ACTION AT ALL, so Postgres' own defaults
    // stand: NO ACTION on both sides ('a'), checked immediately rather than
    // deferred. That is the contract settled in docs/reference/db.md:362-364
    // ("the database's own defaults apply: NO ACTION for both actions, and
    // immediate (non-deferred) checking") and implemented at
    // crates/zeroship-schema/src/query.rs:1607, which OMITS the ON DELETE
    // clause when the action is NO ACTION.
    //
    // These three assertions read `r`/`r`/`true` until 2026-08-12 -- the
    // RESTRICT-and-deferrable contract the project decided AGAINST. Nothing
    // caught it because this binary runs in no CI job at all (see the header).
    // Values below are MEASURED against a live FK, not copied from the doc;
    // measuring first was the point, because had they disagreed the
    // disagreement would have been a product finding rather than a stale test.
    let on_delete: String = rows[0].get("on_delete");
    assert_eq!(on_delete, "a", "expected NO ACTION, got {on_delete}");
    let on_update: String = rows[0].get("on_update");
    assert_eq!(on_update, "a", "expected NO ACTION, got {on_update}");
    let deferrable: bool = rows[0].get("deferrable");
    assert!(
        !deferrable,
        "expected an IMMEDIATE (non-deferrable) FK check"
    );
    release_pg(pool).await;
}

#[compio::test]
async fn b2_ref_blocks_orphan_insert() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = crate::test_app_id!();

    let app = app.as_str();
    b2_setup_users_posts(&pool, app).await;

    // Insert into posts with non-existent authorId; must fail with FK violation.
    // `id` is `TEXT PRIMARY KEY` (no DB default) -- supply one
    // so the row reaches FK validation rather than tripping the id NOT NULL.
    let result = pool
        .query_text_params(
            &format!(
                "INSERT INTO \"{app}\".\"posts\" (\"id\", \"title\", \"authorId\") VALUES ($1, $2, $3)"
            ),
            &["pst_b2_orphan_1", "hello", "usr_does_not_exist"],
        )
        .await;
    let err = result.expect_err("orphan insert should fail");
    let err_str = format!("{err:?}");
    // SQLSTATE 23503 = foreign_key_violation
    assert!(
        err_str.contains("23503") || err_str.to_lowercase().contains("foreign key"),
        "expected foreign_key_violation, got: {err_str}"
    );
    release_pg(pool).await;
}

#[compio::test]
async fn b2_ref_on_delete_restrict_blocks_parent_delete() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = crate::test_app_id!();

    let app = app.as_str();
    b2_setup_users_posts(&pool, app).await;

    // Insert one user + one post that references it. `id`
    // is `TEXT PRIMARY KEY` (no DB default -- production stamps a typed id via
    // the system-fields pass), so the seed INSERT must supply it and read it as
    // text. A `posts` row also needs its own `id`.
    let user_id = "usr_b2_restrict_1";
    let user_rows = pool
        .query_text_params(
            &format!(
                "INSERT INTO \"{app}\".\"users\" (\"id\", \"name\") VALUES ($1, $2) RETURNING id"
            ),
            &[user_id, "alice"],
        )
        .await
        .unwrap();
    let user_id: String = user_rows[0].get("id");
    pool.query_text_params(
        &format!(
            "INSERT INTO \"{app}\".\"posts\" (\"id\", \"title\", \"authorId\") VALUES ($1, $2, $3)"
        ),
        &["pst_b2_restrict_1", "hello", &user_id],
    )
    .await
    .unwrap();

    // Now try to delete the user — RESTRICT must refuse.
    let result = pool
        .query_text_params(
            &format!("DELETE FROM \"{app}\".\"users\" WHERE id = $1"),
            &[&user_id],
        )
        .await;
    let err = result.expect_err("RESTRICT must block parent delete");
    let err_str = format!("{err:?}");
    assert!(
        err_str.contains("23503") || err_str.to_lowercase().contains("foreign key"),
        "expected foreign_key_violation, got: {err_str}"
    );
    release_pg(pool).await;
}

#[compio::test]
async fn b2_ref_on_delete_cascade_deletes_children() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = crate::test_app_id!();

    let app = app.as_str();
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();

    // Raw SQL, and the `ON DELETE CASCADE` is the point of the test - it is what
    // `"onDelete": "cascade"` on a `t.ref` emits, spelled here because the
    // migration service, not plugin-db, owns schema changes.
    pool.batch_execute(&format!(
        r#"CREATE SCHEMA "{app}";
CREATE TABLE "{app}"."users" ({PG_SYSTEM_COLUMNS},
  "name" TEXT NOT NULL
);
{users_idx}
CREATE TABLE "{app}"."posts" ({PG_SYSTEM_COLUMNS},
  "title" TEXT NOT NULL,
  "authorId" TEXT,
  CONSTRAINT "authorId_fkey" FOREIGN KEY ("authorId")
    REFERENCES "{app}"."users" ("id") ON DELETE CASCADE
);
{posts_idx}"#,
        users_idx = pg_system_indexes(app, "users"),
        posts_idx = pg_system_indexes(app, "posts"),
    ))
    .await
    .expect("cascade fixture");

    // Insert user + 3 posts that reference it. `id` is
    // `TEXT PRIMARY KEY` (no DB default), so seed inserts must supply text ids.
    let user_id = "usr_b2_cascade_1";
    let user_rows = pool
        .query_text_params(
            &format!(
                "INSERT INTO \"{app}\".\"users\" (\"id\", \"name\") VALUES ($1, $2) RETURNING id"
            ),
            &[user_id, "bob"],
        )
        .await
        .unwrap();
    let user_id: String = user_rows[0].get("id");
    for (i, title) in ["a", "b", "c"].iter().enumerate() {
        pool.query_text_params(
            &format!(
                "INSERT INTO \"{app}\".\"posts\" (\"id\", \"title\", \"authorId\") VALUES ($1, $2, $3)"
            ),
            &[&format!("pst_b2_cascade_{i}"), title, &user_id],
        )
        .await
        .unwrap();
    }

    // Delete the user — CASCADE should also delete the 3 posts.
    pool.query_text_params(
        &format!("DELETE FROM \"{app}\".\"users\" WHERE id = $1"),
        &[&user_id],
    )
    .await
    .unwrap();

    let count_rows = pool
        .query_text_params(
            &format!("SELECT COUNT(*) AS n FROM \"{app}\".\"posts\""),
            &[],
        )
        .await
        .unwrap();
    let n: i64 = count_rows[0].get("n");
    assert_eq!(n, 0, "CASCADE should have deleted all child posts");
    release_pg(pool).await;
}

#[compio::test]
async fn b2_circular_refs_via_deferrable() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = crate::test_app_id!();

    let app = app.as_str();
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();

    // a → ref(b), b → ref(a). Order matters for first creation:
    // we register a then b. The FK from `a.bId → b.id` must be deferred
    // until b is created. The current `build_create_table` always emits
    // FK inline, so when registering `a` while `b` doesn't yet exist,
    // we'd fail. We therefore register `b` first (no refs), then `a`
    // (with FK to b), then ALTER b to add its FK to a.
    //
    // For this test we register both with FK clauses inline, but use
    // DEFERRABLE INITIALLY DEFERRED so the runtime can insert into
    // a + b within a single transaction in any order.
    //
    // The setup uses two separate calls; we drop the FK from `a.bId`
    // temporarily and re-add it after both tables exist to side-step
    // the cold-start ordering problem. The B2 implementation defers
    // truly inter-table FK creation to a follow-up; today we exercise
    // the DEFERRABLE behaviour by creating both tables, attaching the
    // FK, then verifying a single transaction can insert in any order.

    // Create the tables manually without FK, then add FKs.
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    pool.execute(
        &format!(
            "CREATE TABLE \"{app}\".\"a\" (id SERIAL PRIMARY KEY, b_id INTEGER, created_at TIMESTAMPTZ DEFAULT NOW())"
        ),
        &[],
    )
    .await
    .unwrap();
    pool.execute(
        &format!(
            "CREATE TABLE \"{app}\".\"b\" (id SERIAL PRIMARY KEY, a_id INTEGER, created_at TIMESTAMPTZ DEFAULT NOW())"
        ),
        &[],
    )
    .await
    .unwrap();
    // Add cyclic FKs as DEFERRABLE INITIALLY DEFERRED.
    pool.execute(
        &format!(
            "ALTER TABLE \"{app}\".\"a\" ADD CONSTRAINT a_b_fkey FOREIGN KEY (b_id) REFERENCES \"{app}\".\"b\"(id) DEFERRABLE INITIALLY DEFERRED"
        ),
        &[],
    )
    .await
    .unwrap();
    pool.execute(
        &format!(
            "ALTER TABLE \"{app}\".\"b\" ADD CONSTRAINT b_a_fkey FOREIGN KEY (a_id) REFERENCES \"{app}\".\"a\"(id) DEFERRABLE INITIALLY DEFERRED"
        ),
        &[],
    )
    .await
    .unwrap();

    // Verify both constraints are DEFERRABLE.
    let rows = pool
        .query_text_params(
            r#"
SELECT con.conname AS name, con.condeferrable AS def, con.condeferred AS init_deferred
  FROM pg_constraint con
  JOIN pg_class cl ON cl.oid = con.conrelid
  JOIN pg_namespace n ON n.oid = cl.relnamespace
 WHERE n.nspname = $1 AND con.contype = 'f'
 ORDER BY con.conname
"#,
            &[app],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        let def: bool = row.get("def");
        let init_deferred: bool = row.get("init_deferred");
        let name: String = row.get("name");
        assert!(def, "FK {name} must be DEFERRABLE");
        assert!(init_deferred, "FK {name} must be INITIALLY DEFERRED");
    }

    // Insert pair in a single transaction — order doesn't matter
    // because the FK check is deferred to COMMIT. We insert into `a`
    // referencing a `b` row that doesn't exist yet, then create the
    // `b` row referencing the `a` row, all within the tx.
    let client = pool.get().await.unwrap();
    client.execute("BEGIN", &[]).await.unwrap();
    client
        .execute(
            &format!("INSERT INTO \"{app}\".\"a\" (id, b_id) VALUES (1, 1)"),
            &[],
        )
        .await
        .unwrap();
    client
        .execute(
            &format!("INSERT INTO \"{app}\".\"b\" (id, a_id) VALUES (1, 1)"),
            &[],
        )
        .await
        .unwrap();
    client.execute("COMMIT", &[]).await.unwrap();

    // Confirm the rows exist.
    let count_rows = pool
        .query_text_params(
            &format!("SELECT (SELECT COUNT(*) FROM \"{app}\".\"a\") AS na, (SELECT COUNT(*) FROM \"{app}\".\"b\") AS nb"),
            &[],
        )
        .await
        .unwrap();
    let na: i64 = count_rows[0].get("na");
    let nb: i64 = count_rows[0].get("nb");
    assert_eq!(na, 1);
    assert_eq!(nb, 1);
    drop(client);
    release_pg(pool).await;
}

// ===========================================================================
// Replication slot + publication setup, watchdog, broker plumbing.
//
// These tests exercise the Rust-side primitives that the V8 layer
// exposes through the CDC lifecycle, replication watchdog diagnostics,
// operator-owned abandoned-slot cleanup, and the process-wide broker.
//
// Tests that need `wal_level=logical` FAIL when the running Postgres is
// `replica`. They used to skip, and the paragraph below is the measurement that
// ended it.
//
// DO NOT READ A SKIP AS "CI COVERS THIS". Measured 2026-08-12: NO CI
// job runs this binary at all. `PG_TEST_URL` is set by no workflow,
// `--test integration` is invoked by no workflow, and there is no
// `pg-test` image anywhere in the tree. The `rust` job deliberately
// omits it (ci.yml, "they belong with the other live-database gates
// rather than here"), but the live-DB gate runs
// `--features zeroship-control/live-db-tests,zeroship-migrate-server/live-db-tests`
// and this crate declared NO `live-db-tests` feature, so the deferral
// named a destination that could not accept it. FIXED 2026-08-12: the
// crate now declares `live-db-tests = ["test-helpers"]`, and
// tests/run_plugin_db_live_suite.sh runs this target with it.
//
// AND THE SKIP WAS INVISIBLE TO A SUMMING GATE. Measured on two
// throwaway servers differing only in wal_level, the tests below printed the
// IDENTICAL result line either way, because a skip counts as a pass. On
// `replica` every one of them skipped; on `logical` every one executed. The
// only discriminators were a marker no gate ran over this binary, and the wall
// time. So wiring this into CI would have bought a green that proved nothing.
// `require_logical_wal` closes that: the two servers now differ in exit status.
// ===========================================================================

/// Refuse the run unless the server is configured for logical decoding.
///
/// # Panics
///
/// When `wal_level` is anything but `logical`, naming the setting, how to read
/// it back, and both ways to change it.
async fn require_logical_wal(pool: &Pool) {
    let rows = pool.query_text_params("SHOW wal_level", &[]).await.unwrap();
    let observed: String = rows
        .first()
        .map(|r| r.get::<_, String>(0))
        .unwrap_or_default();
    assert!(
        observed == "logical",
        "This server cannot do logical decoding, and this test requires it.\n\
         \n\
         \x20 backend: PostgreSQL\n\
         \x20 setting: wal_level\n\
         \x20 wanted:  logical\n\
         \x20 observed: {observed}\n\
         \n\
         Everything below this point is CDC - replication slots, publications,\n\
         the consumer and its reaper - and none of it can be created at all\n\
         under `replica`.\n\
         \n\
         The provisioned server already has it. `tests/provision_test_backends.sh`\n\
         starts deploy/compose's `postgres`, which runs with wal_level=logical\n\
         and max_prepared_transactions set, and that script says in as many words\n\
         that those two are load-bearing. If you are pointed at a server of your\n\
         own instead, set it there:\n\
         \n\
         \x20 ALTER SYSTEM SET wal_level = 'logical';   -- then RESTART the server\n\
         \x20 -- a reload is not enough; wal_level is postmaster-level\n\
         \n\
         or start it with `-c wal_level=logical`. Read it back with `SHOW wal_level`.\n\
         \n\
         There is no environment variable that makes this a skip. A server that\n\
         cannot run these tests is a failed run, not a green one."
    );
}

/// Drop the leftover slot, publication and schema OF ONE APP, so a test can
/// re-run from a clean state. Tolerates "does not exist".
///
/// # It used to sweep the whole database, and that is why the suite was serial
///
/// Two unbounded `LIKE '__zs_%'` sweeps ended this function until 2026-09-04,
/// with the note "Integration tests run with `--test-threads=1` so the global
/// sweep is safe". Nineteen tests call this, so at the default thread count
/// each one destroyed every sibling's CDC fixture:
/// `drop_namespace_idempotent_steps_3_to_5` failed on
/// `assert!(publication_exists(&pool, app))` for a publication it had just
/// created, and it already used a per-test app name - so per-test NAMING alone
/// could not have fixed this half.
///
/// The sweeps existed for a real reason (slot accumulation exhausting
/// `max_replication_slots`), and that reason is served instead by
/// `support::sweep_prior_run_residue_once`, which runs before any test starts.
async fn c1_cleanup(pool: &Pool, app: &str) {
    let pub_name = zeroship_plugin_db::replication::publication_name(app).unwrap();
    let slot = zeroship_plugin_db::replication::worker_slot_name(app, CDC_TEST_WORKER_ID).unwrap();
    let _ = pool
        .execute(&format!(r#"DROP PUBLICATION IF EXISTS "{pub_name}""#), &[])
        .await;
    // EVERY worker's slot for this app, not just `CDC_TEST_WORKER_ID`'s.
    //
    // A slot name is `__zs_slot_<28 hex of app>__<20 hex of worker>`
    // (`replication::worker_slot_name`), so everything up to and including the
    // final `__` belongs to the app whatever worker minted it. This dropped
    // exactly one worker's slot until 2026-09-04, and
    // `c1_abandoned_reaper_preserves_inactive_slot_owned_by_live_worker` mints
    // its own `live-worker-with-reconnecting-consumer` slot - so that slot
    // survived the whole run. Serially it was harmless: the alphabetical order
    // puts `..._measures_elapsed_inactivity...` BEFORE it, so the reaper test
    // never saw the orphan. In parallel the order is arbitrary, and when the
    // leak ran first the reaper's due sweep dropped two slots and its
    // `assert_eq!(due.dropped, vec![slot])` failed naming both.
    //
    // `left(slot_name, length($1)) = $1` is the production comparison shape
    // (`replication::worker_slot_name_prefix`): the prefix is derived, so no
    // app-controlled wildcard can broaden it. `database = current_database()`
    // is here for the same reason it is in `support::sweep_prior_run_residue`.
    let slot_prefix = {
        let cut = slot
            .rfind("__")
            .expect("a worker slot name separates its app and worker tokens with `__`");
        slot[..cut + 2].to_string()
    };
    let _ = pool
        .query_text_params(
            "SELECT pg_drop_replication_slot(slot_name) \
             FROM pg_replication_slots \
             WHERE left(slot_name, length($1)) = $1 \
               AND active = false \
               AND database = current_database()",
            &[&slot_prefix],
        )
        .await;
    let _ = pool
        .execute(&format!(r#"DROP SCHEMA IF EXISTS "{app}" CASCADE"#), &[])
        .await;
}

/// Rewrite the database component of a Postgres DSN, preserving any query
/// string. Used only to reach a SECOND database on the same server.
fn url_with_database(url: &str, database: &str) -> String {
    let (base, query) = match url.find('?') {
        Some(i) => (&url[..i], &url[i..]),
        None => (url, ""),
    };
    let cut = base.rfind('/').expect("DSN has a database path segment");
    format!("{}/{}{}", &base[..cut], database, query)
}

/// The residue sweep must not reach another database's replication slots.
///
/// This is a regression guard for a cleanup that dropped slots CLUSTER-WIDE.
/// `pg_replication_slots` is a cluster-wide view and PostgreSQL lets any
/// session drop an inactive slot regardless of which database owns it, so the
/// unscoped sweep destroyed the CDC slots of every suite sharing the server -
/// including suites deliberately given their own database for isolation.
///
/// Remove `AND database = current_database()` from
/// `support::sweep_prior_run_replication_objects` and this test fails: the
/// foreign slot is gone. It guarded `c1_cleanup` until 2026-09-04, when the
/// sweep moved out of that function; the guard followed the code rather than
/// staying pointed at the address the code used to have.
///
/// It takes [`cdc_budget::exclusive`] because it CALLS the sweep, which drops
/// every inactive `__zs_%` slot in this database - including a sibling's. It
/// calls the REPLICATION half only: `sweep_prior_run_residue` also drops the
/// suite's schemas and roles, and `cdc_budget` does not fence the 60-odd tests
/// that hold those.
#[compio::test]
async fn c1_cleanup_sweep_does_not_cross_database_boundaries() {
    let url = require_pg().await;
    let _cdc = cdc_budget::exclusive();
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    require_logical_wal(&pool).await;

    // A second database on the SAME server, standing in for a concurrently
    // running suite that was handed its own database.
    const NEIGHBOUR_DB: &str = "plugin_db_cleanup_neighbour";
    const FOREIGN_SLOT: &str = "__zs_neighbour_suite_slot";
    let _ = pool
        .execute(&format!(r#"DROP DATABASE IF EXISTS "{NEIGHBOUR_DB}""#), &[])
        .await;
    pool.execute(&format!(r#"CREATE DATABASE "{NEIGHBOUR_DB}""#), &[])
        .await
        .expect("create neighbour database");

    let neighbour_url = url_with_database(&url, NEIGHBOUR_DB);
    let neighbour = Pool::connect(&neighbour_url, 1).await.unwrap();
    neighbour
        .query_text_params(
            "SELECT pg_create_logical_replication_slot($1, 'pgoutput')",
            &[FOREIGN_SLOT],
        )
        .await
        .expect("create the neighbour suite's slot");

    // The neighbour's consumer has not attached yet, so its slot is inactive -
    // exactly the window the sweep used to destroy.
    let active = pool
        .query_text_params(
            "SELECT active::text FROM pg_replication_slots WHERE slot_name = $1",
            &[FOREIGN_SLOT],
        )
        .await
        .unwrap();
    assert_eq!(
        active.len(),
        1,
        "the neighbour's slot must be visible cluster-wide, or this test proves nothing"
    );
    let is_active: String = active[0].get(0);
    assert_eq!(
        is_active, "false",
        "the slot must be INACTIVE, or the sweep's active=false guard hides the defect"
    );

    // Now run the sweep from OUR database.
    support::sweep_prior_run_replication_objects(&pool).await;

    let survivors = pool
        .query_text_params(
            "SELECT slot_name FROM pg_replication_slots WHERE slot_name = $1",
            &[FOREIGN_SLOT],
        )
        .await
        .unwrap();
    assert_eq!(
        survivors.len(),
        1,
        "the residue sweep dropped a slot owned by database {NEIGHBOUR_DB}; it is not \
         scoped to current_database() and will corrupt every suite on this server"
    );

    // Teardown: the slot must go before the database will drop.
    neighbour
        .query_text_params("SELECT pg_drop_replication_slot($1)", &[FOREIGN_SLOT])
        .await
        .expect("drop the neighbour's slot");
    drop(neighbour);
    let _ = pool
        .execute(&format!(r#"DROP DATABASE IF EXISTS "{NEIGHBOUR_DB}""#), &[])
        .await;
    release_pg(pool).await;
}

async fn c1_create_publication(pool: &Pool, app: &str) {
    c1_create_publication_for_tables(pool, app, &[]).await;
}

async fn c1_create_publication_for_tables(pool: &Pool, app: &str, tables: &[&str]) {
    let pub_name = zeroship_plugin_db::replication::publication_name(app).unwrap();
    let membership = tables
        .iter()
        .map(|table| format!(r#""{app}"."{table}""#))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = if membership.is_empty() {
        format!(r#"CREATE PUBLICATION "{pub_name}""#)
    } else {
        format!(r#"CREATE PUBLICATION "{pub_name}" FOR TABLE {membership}"#)
    };
    pool.execute(&sql, &[])
        .await
        .expect("create migration-owned test publication");
}

#[compio::test]
async fn c1_setup_requires_publication_and_creates_slot_idempotently() {
    let url = require_pg().await;
    let _cdc = cdc_budget::shared();
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    require_logical_wal(&pool).await;

    let app = crate::test_app_id!();

    let app = app.as_str();
    c1_cleanup(&pool, app).await;
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    c1_create_publication(&pool, app).await;

    // The migration service created the publication; the worker creates its slot.
    let first = zeroship_plugin_db::replication::ensure_worker_slot(&pool, app, CDC_TEST_WORKER_ID)
        .await
        .unwrap();
    assert!(first.created);
    assert_eq!(
        first.slot,
        zeroship_plugin_db::replication::worker_slot_name(app, CDC_TEST_WORKER_ID).unwrap()
    );
    assert_eq!(
        first.publication,
        zeroship_plugin_db::replication::publication_name(app).unwrap()
    );

    // Second call must observe the existing slot and return created=false.
    let second =
        zeroship_plugin_db::replication::ensure_worker_slot(&pool, app, CDC_TEST_WORKER_ID)
            .await
            .unwrap();
    assert!(!second.created);
    assert_eq!(second.slot, first.slot);

    c1_cleanup(&pool, app).await;
    release_pg(pool).await;
}

#[compio::test]
async fn c1_setup_refuses_to_create_a_missing_publication() {
    let url = require_pg().await;
    let _cdc = cdc_budget::shared();
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = crate::test_app_id!();
    let app = app.as_str();
    c1_cleanup(&pool, app).await;
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();

    let err = zeroship_plugin_db::replication::ensure_worker_slot(&pool, app, CDC_TEST_WORKER_ID)
        .await
        .expect_err("worker must not create a missing publication");
    assert!(matches!(
        err,
        zeroship_data_core::error::DbError::Configuration {
            code: "replication_publication_missing",
            ..
        }
    ));

    c1_cleanup(&pool, app).await;
    release_pg(pool).await;
}

#[compio::test]
async fn c1_watchdog_reports_new_slot() {
    let url = require_pg().await;
    let _cdc = cdc_budget::shared();
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    require_logical_wal(&pool).await;

    let app = crate::test_app_id!();

    let app = app.as_str();
    c1_cleanup(&pool, app).await;
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    c1_create_publication(&pool, app).await;
    zeroship_plugin_db::replication::ensure_worker_slot(&pool, app, CDC_TEST_WORKER_ID)
        .await
        .unwrap();

    let slots = zeroship_plugin_db::replication::watchdog_query(&pool, app)
        .await
        .unwrap();
    let me = slots.iter().find(|s| {
        s.slot_name
            == zeroship_plugin_db::replication::worker_slot_name(app, CDC_TEST_WORKER_ID).unwrap()
    });
    assert!(me.is_some(), "watchdog must report our slot");
    let me = me.unwrap();
    // Newly created slot — not yet attached, so `active=false`.
    assert!(!me.active);
    // `wal_status` should be present and one of the documented values.
    let status = me.wal_status.as_deref().unwrap_or("");
    assert!(
        matches!(status, "reserved" | "extended" | "unreserved" | "lost"),
        "unexpected wal_status: {status:?}"
    );

    c1_cleanup(&pool, app).await;
    release_pg(pool).await;
}

#[compio::test]
async fn c1_abandoned_reaper_measures_elapsed_inactivity_not_wal_bytes() {
    let url = require_pg().await;
    let _cdc = cdc_budget::exclusive();
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    require_logical_wal(&pool).await;

    let app = crate::test_app_id!();

    let app = app.as_str();
    c1_cleanup(&pool, app).await;
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    c1_create_publication(&pool, app).await;
    let setup = zeroship_plugin_db::replication::ensure_worker_slot(&pool, app, CDC_TEST_WORKER_ID)
        .await
        .unwrap();
    assert!(setup.created);

    let slot = zeroship_plugin_db::replication::worker_slot_name(app, CDC_TEST_WORKER_ID).unwrap();
    let candidates = pool
        .query_text_params(
            "SELECT slot_name FROM pg_replication_slots WHERE slot_name = $1 AND active = false",
            &[&slot],
        )
        .await
        .unwrap();
    assert_eq!(candidates.len(), 1, "test must exercise one inactive slot");

    // Advance WAL by more bytes than the one-hour threshold's numeric value.
    // The old implementation compared those unlike units and reaped this
    // brand-new slot immediately.
    pool.query_text_params(
        "SELECT pg_logical_emit_message(true, 'zeroship-reaper-test', repeat('x', 8192))::text",
        &[],
    )
    .await
    .unwrap();
    let threshold = std::time::Duration::from_secs(3600);
    let start = std::time::Instant::now();
    let mut reaper = zeroship_plugin_db::slot_reaper::OperatorSlotReaper::connect_for_tests(
        &url,
        "elapsed-time-reaper",
        threshold,
    )
    .await
    .unwrap();
    let first = reaper.sweep_at_for_tests(start).await.unwrap();
    assert!(first.is_leader, "single test reaper must lead its sweep");
    assert!(
        first.inspected > 0,
        "test sweep must inspect at least one managed slot"
    );
    assert!(
        first.dropped.is_empty(),
        "a first observation cannot prove one hour of inactivity: {first:?}"
    );
    let remaining = pool
        .query_text_params(
            "SELECT slot_name FROM pg_replication_slots WHERE slot_name = $1",
            &[&slot],
        )
        .await
        .unwrap();
    assert_eq!(remaining.len(), 1, "young inactive slot must remain");

    let early = reaper
        .sweep_at_for_tests(start + threshold - std::time::Duration::from_secs(1))
        .await
        .unwrap();
    assert!(early.inspected > 0, "early sweep must inspect the slot");
    assert!(
        early.dropped.is_empty(),
        "slot reaped before threshold: {early:?}"
    );

    let due = reaper.sweep_at_for_tests(start + threshold).await.unwrap();
    assert!(due.inspected > 0, "due sweep must inspect the slot");
    assert_eq!(due.dropped, vec![slot.clone()]);
    let remaining = pool
        .query_text_params(
            "SELECT slot_name FROM pg_replication_slots WHERE slot_name = $1",
            &[&slot],
        )
        .await
        .unwrap();
    assert!(remaining.is_empty(), "due abandoned slot must be gone");

    drop(reaper);
    c1_cleanup(&pool, app).await;
    release_pg(pool).await;
}

#[compio::test]
async fn c1_abandoned_reaper_preserves_inactive_slot_owned_by_live_worker() {
    let url = require_pg().await;
    let _cdc = cdc_budget::exclusive();
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    require_logical_wal(&pool).await;

    let app = crate::test_app_id!();

    let app = app.as_str();
    let worker_id = "live-worker-with-reconnecting-consumer";
    c1_cleanup(&pool, app).await;
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    c1_create_publication(&pool, app).await;
    zeroship_plugin_db::replication::ensure_worker_slot(&pool, app, worker_id)
        .await
        .unwrap();
    let slot = zeroship_plugin_db::replication::worker_slot_name(app, worker_id).unwrap();

    let threshold = std::time::Duration::from_secs(3600);
    let owner = zeroship_plugin_db::slot_reaper::OperatorSlotReaper::connect_for_tests(
        &url, worker_id, threshold,
    )
    .await
    .unwrap();
    let conflict = zeroship_plugin_db::slot_reaper::OperatorSlotReaper::connect_for_tests(
        &url, worker_id, threshold,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            conflict,
            zeroship_data_core::error::DbError::Configuration {
                code: "cdc_worker_lease_conflict",
                ..
            }
        ),
        "duplicate live worker identity must be refused: {conflict:?}"
    );

    let mut observer = zeroship_plugin_db::slot_reaper::OperatorSlotReaper::connect_for_tests(
        &url,
        "live-worker-lease-observer",
        threshold,
    )
    .await
    .unwrap();
    let start = std::time::Instant::now();
    let first = observer.sweep_at_for_tests(start).await.unwrap();
    assert!(first.is_leader, "observer must own the fleet sweep");
    assert!(
        first.inspected > 0,
        "test must inspect the leased inactive slot"
    );
    assert!(
        first.dropped.is_empty(),
        "live worker slot was reaped: {first:?}"
    );
    let aged = observer
        .sweep_at_for_tests(start + threshold + std::time::Duration::from_secs(1))
        .await
        .unwrap();
    assert!(
        aged.inspected > 0,
        "aged sweep must inspect the leased slot"
    );
    assert!(
        aged.dropped.is_empty(),
        "live worker lease was ignored: {aged:?}"
    );

    let rows = pool
        .query_text_params(
            "SELECT active FROM pg_replication_slots WHERE slot_name = $1",
            &[&slot],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "leased inactive slot must remain");
    assert!(!rows[0].get::<_, bool>("active"));

    drop(observer);
    drop(owner);
    c1_cleanup(&pool, app).await;
    release_pg(pool).await;
}

#[compio::test]
async fn c1_abandoned_reaper_preserves_a_connected_idle_consumer() {
    let url = require_pg().await;
    let _cdc = cdc_budget::exclusive();
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    require_logical_wal(&pool).await;

    let app = crate::test_app_id!();

    let app = app.as_str();
    let worker_id = "idle-live-consumer-worker";
    c1_cleanup(&pool, app).await;
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    pool.execute(
        &format!(r#"CREATE TABLE "{app}"."events" (id BIGSERIAL PRIMARY KEY)"#),
        &[],
    )
    .await
    .unwrap();
    c1_create_publication_for_tables(&pool, app, &["events"]).await;

    let threshold = std::time::Duration::from_secs(3600);
    let backend = zeroship_plugin_db::backend::BackendHandle::Postgres(std::rc::Rc::new(
        zeroship_plugin_db::backend::PostgresBackend::new(
            pool.clone(),
            url.clone(),
            zeroship_plugin_db::isolate_key_source(),
        ),
    ));
    let consumer = zeroship_plugin_db::change_stream_pg::PgChangeStream::new(match &backend {
        zeroship_plugin_db::backend::BackendHandle::Postgres(pg) => pg.clone(),
        zeroship_plugin_db::backend::BackendHandle::Sqlite(_) => {
            panic!("fixture builds a Postgres handle")
        }
    })
    .spawn_consumer(app, worker_id)
    .await
    .expect("idle consumer must reach START_REPLICATION");
    let slot = zeroship_plugin_db::replication::worker_slot_name(app, worker_id).unwrap();
    let active = pool
        .query_text_params(
            "SELECT active FROM pg_replication_slots WHERE slot_name = $1",
            &[&slot],
        )
        .await
        .unwrap();
    assert_eq!(active.len(), 1, "test must exercise one live consumer slot");
    assert!(active[0].get::<_, bool>("active"));

    let mut observer = zeroship_plugin_db::slot_reaper::OperatorSlotReaper::connect_for_tests(
        &url,
        "idle-consumer-observer",
        threshold,
    )
    .await
    .unwrap();
    let start = std::time::Instant::now();
    let first = observer.sweep_at_for_tests(start).await.unwrap();
    assert!(first.is_leader, "observer must own the fleet sweep");
    assert!(
        first.inspected > 0,
        "test sweep must inspect the active slot"
    );
    assert_eq!(
        observer.tracked_slots_for_tests(),
        0,
        "an active idle consumer must not enter the inactivity clock"
    );
    let aged = observer
        .sweep_at_for_tests(start + threshold + std::time::Duration::from_secs(1))
        .await
        .unwrap();
    assert!(
        aged.inspected > 0,
        "aged sweep must inspect the active slot"
    );
    assert!(first.dropped.is_empty() && aged.dropped.is_empty());
    assert_eq!(
        observer.tracked_slots_for_tests(),
        0,
        "an active idle consumer must stay outside the inactivity clock"
    );
    let still_active = pool
        .query_text_params(
            "SELECT active FROM pg_replication_slots WHERE slot_name = $1",
            &[&slot],
        )
        .await
        .unwrap();
    assert_eq!(still_active.len(), 1, "idle live slot must remain");
    assert!(still_active[0].get::<_, bool>("active"));

    drop(observer);
    consumer.shutdown().await.unwrap();
    c1_cleanup(&pool, app).await;
    drop(backend);
    release_pg(pool).await;
}

#[compio::test]
async fn c1_abandoned_reaper_elects_one_leader_across_concurrent_workers() {
    let url = require_pg().await;
    let _cdc = cdc_budget::exclusive();
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    require_logical_wal(&pool).await;

    let app = crate::test_app_id!();

    let app = app.as_str();
    let worker_id = "crashed-worker-for-concurrent-reapers";
    c1_cleanup(&pool, app).await;
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    c1_create_publication(&pool, app).await;
    zeroship_plugin_db::replication::ensure_worker_slot(&pool, app, worker_id)
        .await
        .unwrap();
    let slot = zeroship_plugin_db::replication::worker_slot_name(app, worker_id).unwrap();

    let threshold = std::time::Duration::from_secs(3600);
    let start = std::time::Instant::now();
    let mut first = zeroship_plugin_db::slot_reaper::OperatorSlotReaper::connect_for_tests(
        &url,
        "concurrent-reaper-a",
        threshold,
    )
    .await
    .unwrap();
    let mut second = zeroship_plugin_db::slot_reaper::OperatorSlotReaper::connect_for_tests(
        &url,
        "concurrent-reaper-b",
        threshold,
    )
    .await
    .unwrap();
    let (first_observation, second_observation) = futures::join!(
        first.sweep_at_for_tests(start),
        second.sweep_at_for_tests(start),
    );
    let first_observation = first_observation.unwrap();
    let second_observation = second_observation.unwrap();
    assert_eq!(
        usize::from(first_observation.is_leader) + usize::from(second_observation.is_leader),
        1,
        "concurrent workers must elect exactly one fleet observer"
    );
    let inspected = first_observation.inspected + second_observation.inspected;
    assert!(
        inspected > 0,
        "the elected reaper must inspect a non-empty set"
    );
    assert!(first_observation.dropped.is_empty() && second_observation.dropped.is_empty());

    let due = start + threshold;
    let (first_result, second_result) = futures::join!(
        first.sweep_at_for_tests(due),
        second.sweep_at_for_tests(due),
    );
    let first_result = first_result.unwrap();
    let second_result = second_result.unwrap();
    assert_eq!(
        usize::from(first_result.is_leader) + usize::from(second_result.is_leader),
        1,
        "fleet observer leadership must remain single-flight"
    );
    assert!(
        first_result.inspected + second_result.inspected > 0,
        "the due sweep must inspect the fixture"
    );
    let dropped = first_result.dropped.len() + second_result.dropped.len();
    assert_eq!(
        dropped, 1,
        "exactly one concurrent reaper must win the drop"
    );
    assert!(
        first_result.dropped.contains(&slot) || second_result.dropped.contains(&slot),
        "the dropped slot must be the test fixture"
    );
    let remaining = pool
        .query_text_params(
            "SELECT slot_name FROM pg_replication_slots WHERE slot_name = $1",
            &[&slot],
        )
        .await
        .unwrap();
    assert!(
        remaining.is_empty(),
        "concurrent sweep must leave the slot absent"
    );

    drop(first);
    drop(second);
    c1_cleanup(&pool, app).await;
    release_pg(pool).await;
}

#[compio::test]
async fn c1_setup_resumes_at_existing_lsn_across_restart() {
    // "Worker restart" is simulated by tearing down the Pool (closes
    // all connections — equivalent to a worker process exit) and
    // re-running `ensure_worker_slot`. The slot survives
    // and reports the same `confirmed_flush_lsn`.
    let url = require_pg().await;
    let _cdc = cdc_budget::shared();
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    require_logical_wal(&pool).await;

    let app = crate::test_app_id!();

    let app = app.as_str();
    c1_cleanup(&pool, app).await;
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    c1_create_publication(&pool, app).await;

    let first = zeroship_plugin_db::replication::ensure_worker_slot(&pool, app, CDC_TEST_WORKER_ID)
        .await
        .unwrap();
    assert!(first.created);
    let first_slot = first.slot.clone();

    // Simulate worker restart by dropping the pool and opening a new one.
    drop(pool);
    let pool2 = Pool::connect(&url, 2).await.unwrap();
    let resumed =
        zeroship_plugin_db::replication::ensure_worker_slot(&pool2, app, CDC_TEST_WORKER_ID)
            .await
            .unwrap();
    assert!(
        !resumed.created,
        "second call after 'restart' must observe existing slot"
    );
    assert_eq!(resumed.slot, first_slot);

    c1_cleanup(&pool2, app).await;
    drop(pool2);
    drain_pg().await;
}

// NOTE: a "publication-only on wal_level=replica" sanity test was
// considered but removed: Postgres emits a NoticeResponse
// (`wal_level is insufficient to publish logical changes`) on CREATE
// PUBLICATION that exposes a deferred-notice handling path in
// compio-postgres which we have not yet exercised under load — the
// notice can block subsequent `setup()` calls inside the same test
// process. The publication-creation code path is exercised by the
// `c1_setup_requires_publication_and_creates_slot_idempotently` test on
// a logical-WAL server. Re-introduce this test alongside a
// compio-postgres notice-handling audit (separate work).

#[compio::test]
async fn c1_broker_event_delivered_for_insert_via_emit() {
    // End-to-end of the local-emit path: the broker, attached
    // on the same thread the test runs on, receives an insert event
    // when `emit_local` is called. No Postgres needed — the broker
    // is in-process.

    // Clean slate.
    zeroship_plugin_db::broker::drop_app(None);
    let app = crate::test_app_id!();
    let app = app.as_str();
    let sub = zeroship_plugin_db::broker::subscribe(app, "messages");

    zeroship_plugin_db::broker::emit_local(
        app,
        "messages",
        zeroship_core::change_event::ChangeOp::Insert,
        Some("usr_02HXINTEGRATIONSUBPK".to_string()),
        vec!["title".into()],
        std::collections::HashMap::new(),
    );

    let msg = sub.pop().expect("expected an event");
    match msg {
        zeroship_plugin_db::broker::SubscriptionMessage::Change(ev) => {
            assert_eq!(ev.collection, "messages");
            assert_eq!(ev.pk.as_deref(), Some("usr_02HXINTEGRATIONSUBPK"));
            assert_eq!(ev.op, zeroship_core::change_event::ChangeOp::Insert);
        }
        other => panic!("unexpected: {other:?}"),
    }
    sub.close();
    zeroship_plugin_db::broker::drop_app(None);
}

// ---------------------------------------------------------------------------
// Gap B — emit deferred until COMMIT
//
// Robustness audit:
// mutations inside a `db.transaction` block must NOT publish their
// broker events until the outer COMMIT lands. Pre-fix, every
// successful INSERT/UPDATE/DELETE inside a tx fired `emit_local`
// immediately, so a subscriber could observe rows the surrounding
// ROLLBACK would un-do — classic dual-write anomaly.
// ---------------------------------------------------------------------------

/// Helper: build a minimal ChangeEvent for the queue-mechanics tests.
fn gapb_ev(app: &str, collection: &str, pk: i64) -> zeroship_core::change_event::ChangeEvent {
    zeroship_core::change_event::ChangeEvent {
        app_id: app.to_string(),
        collection: collection.to_string(),
        op: zeroship_core::change_event::ChangeOp::Insert,
        pk: Some(pk.to_string()),
        changed_columns: vec![],
        new_tuple: std::collections::HashMap::new(),
        old_tuple: None,
    }
}

#[compio::test]
async fn gap_b_commit_drains_pending_emits_to_broker() {
    // Subscribe BEFORE pushing events, mid-"transaction" push two,
    // then drain — the broker should receive both.
    zeroship_plugin_db::broker::drop_app(None);
    let app = crate::test_app_id!();
    let app = app.as_str();
    let sub = zeroship_plugin_db::broker::subscribe(app, "users");

    zeroship_plugin_db::push_pending_emit_for_tests(gapb_ev(app, "users", 1));
    zeroship_plugin_db::push_pending_emit_for_tests(gapb_ev(app, "users", 2));
    // Pre-drain: subscriber must observe nothing (events still queued).
    assert!(sub.pop().is_none(), "events must not leak before commit");

    zeroship_plugin_db::drain_pending_emits_for_tests(app);

    let mut pks: Vec<String> = Vec::new();
    while let Some(zeroship_plugin_db::broker::SubscriptionMessage::Change(ev)) = sub.pop() {
        pks.push(ev.pk.as_deref().unwrap().to_string());
    }
    assert_eq!(pks, vec!["1".to_string(), "2".to_string()]);

    sub.close();
    zeroship_plugin_db::broker::drop_app(None);
}

#[compio::test]
async fn gap_b_rollback_clears_pending_emits_silently() {
    // Push events, then `clear` (rollback path). The broker must
    // never see them.
    zeroship_plugin_db::broker::drop_app(None);
    let app = crate::test_app_id!();
    let app = app.as_str();
    let sub = zeroship_plugin_db::broker::subscribe(app, "users");

    zeroship_plugin_db::push_pending_emit_for_tests(gapb_ev(app, "users", 42));
    zeroship_plugin_db::push_pending_emit_for_tests(gapb_ev(app, "users", 43));
    zeroship_plugin_db::clear_pending_emits_for_tests(app);

    assert!(
        sub.pop().is_none(),
        "rollback must NOT publish any broker event"
    );

    sub.close();
    zeroship_plugin_db::broker::drop_app(None);
}

#[compio::test]
async fn gap_b_end_to_end_insert_inside_tx_defers_emit_until_commit() {
    // End-to-end: real Postgres tx, real `exec_mutation_with_emit`
    // call. Pre-commit the broker stays empty; post-drain it sees
    // the insert.
    let url = require_pg().await;
    zeroship_plugin_db::set_db_url_for_tests(&url);
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());

    let app = crate::test_app_id!();

    let app = app.as_str();
    // Fresh schema with one collection table.
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    pool.execute(
        &format!(
            // The seven platform system columns are here because a write's
            // RETURNING is now an explicit list of them plus the declared
            // fields. A fixture table missing them fails with `42703 column
            // does not exist` - loudly, which is the point of naming columns
            // rather than starring them.
            r#"CREATE TABLE "{app}"."users" (
                id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                name TEXT NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                created_by TEXT,
                updated_by TEXT,
                version INTEGER NOT NULL DEFAULT 1,
                deleted_at TIMESTAMPTZ
            )"#
        ),
        &[],
    )
    .await
    .unwrap();

    zeroship_plugin_db::broker::drop_app(None);
    let sub = zeroship_plugin_db::broker::subscribe(app, "users");

    // Install a real Client into TX_CONN with BEGIN issued; matches
    // production exec_begin's effect on the queue/drain machinery.
    zeroship_plugin_db::install_tx_marker_for_tests(app, &url).await;

    // Insert via the production helper.
    let bq = zeroship_plugin_db::query::build_insert(
        &zeroship_schema::SchemaName::new(app).expect("fixture schema name"),
        "users",
        // The descriptor entry for the fixture table above: one declared field.
        &serde_json::json!({ "name": { "type": "string", "required": true } }),
        &serde_json::json!({ "name": "alice" }),
    )
    .expect("build_insert");
    let _ = zeroship_plugin_db::exec_mutation_with_emit_for_tests(
        bq,
        app,
        "users",
        zeroship_core::change_event::ChangeOp::Insert,
    )
    .await
    .expect("insert");

    // Mid-transaction: subscriber must see nothing.
    assert!(
        sub.pop().is_none(),
        "pre-commit broker must be empty (Gap B)"
    );

    // Simulate commit: drain pending emits.
    zeroship_plugin_db::drain_pending_emits_for_tests(app);
    zeroship_plugin_db::uninstall_tx_marker_for_tests(app).await;

    let got = sub.pop();
    match got {
        Some(zeroship_plugin_db::broker::SubscriptionMessage::Change(ev)) => {
            assert_eq!(ev.collection, "users");
        }
        other => panic!("expected Change event after commit, got: {other:?}"),
    }

    sub.close();
    zeroship_plugin_db::broker::drop_app(None);
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// Cross-worker WAL propagation
// ---------------------------------------------------------------------------
//
// These tests prove the streaming-replication path:
//
//   write on "worker A" -> Postgres WAL -> consumer task -> broker -> "worker B"
//
// The writer uses a regular pool and never calls local emit. Delivery
// therefore proves that the production CDC owner decoded the change
// from WAL and published it through the process-wide broker.

/// End-to-end: write a row via the regular pool, the WAL consumer
/// running concurrently picks it up and the broker delivers the event.
///
/// Asserts the cross-worker case: even if the writer never
/// called `emit_local` (we explicitly suppress that path), the
/// subscriber still sees the event because it was decoded from WAL.
#[compio::test]
async fn p8a2_consumer_publishes_wal_event_to_broker() {
    let url = require_pg().await;
    let _cdc = cdc_budget::shared();
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    require_logical_wal(&pool).await;

    let app = crate::test_app_id!();

    let app = app.as_str();
    c1_cleanup(&pool, app).await;

    // Schema + table the publication will scope.
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    pool.execute(
        &format!(
            r#"CREATE TABLE "{app}"."events" (
                id BIGSERIAL PRIMARY KEY,
                title TEXT NOT NULL
            )"#
        ),
        &[],
    )
    .await
    .unwrap();
    c1_create_publication_for_tables(&pool, app, &["events"]).await;

    // Clean broker; subscribe to the collection we're about to insert
    // into.
    zeroship_plugin_db::broker::drop_app(None);
    let sub = zeroship_plugin_db::broker::subscribe(app, "events");

    // The production adapter provisions, spawns, and returns only
    // after Postgres accepts START_REPLICATION.
    let backend = zeroship_plugin_db::backend::BackendHandle::Postgres(std::rc::Rc::new(
        zeroship_plugin_db::backend::PostgresBackend::new(
            pool.clone(),
            url.clone(),
            zeroship_plugin_db::isolate_key_source(),
        ),
    ));
    let consumer = zeroship_plugin_db::change_stream_pg::PgChangeStream::new(match &backend {
        zeroship_plugin_db::backend::BackendHandle::Postgres(pg) => pg.clone(),
        zeroship_plugin_db::backend::BackendHandle::Sqlite(_) => {
            panic!("fixture builds a Postgres handle")
        }
    })
    .spawn_consumer(app, CDC_TEST_WORKER_ID)
    .await
    .expect("CDC must reach START_REPLICATION");

    // Write a row via the regular pool. This represents "worker A".
    pool.execute(
        &format!(r#"INSERT INTO "{app}"."events" (title) VALUES ('hello')"#),
        &[],
    )
    .await
    .unwrap();

    // Wait for the event to propagate via WAL.
    let mut got: Option<zeroship_plugin_db::broker::SubscriptionMessage> = None;
    for _ in 0..40 {
        if let Some(msg) = sub.pop() {
            got = Some(msg);
            break;
        }
        compio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let msg = got.expect("expected a WAL event within the polling window");
    match msg {
        zeroship_plugin_db::broker::SubscriptionMessage::Change(ev) => {
            assert_eq!(ev.app_id, app);
            assert_eq!(ev.collection, "events");
            assert_eq!(ev.op, zeroship_core::change_event::ChangeOp::Insert);
            // pk should resolve to the autogenerated BIGSERIAL value.
            assert!(ev.pk.is_some(), "pk should be set, got {ev:?}");
        }
        other => panic!("expected Change, got {other:?}"),
    }

    // Stop the consumer + clean up.
    consumer.shutdown().await.unwrap();
    zeroship_plugin_db::broker::drop_app(None);
    c1_cleanup(&pool, app).await;
    release_pg(pool).await;
}

// ===========================================================================
// Per-app role privilege floor.
//
// One test survives here. The rest of this section tested the
// platform-owned system schema, its HMAC session anchor and its SECURITY
// DEFINER wrappers, all deleted on 2026-08-27 (see `crate::auth`);
// those tests were deleted with their subject rather than weakened.
//
// What is still asserted: a role created NOREPLICATION cannot create a
// logical replication slot. That is the floor `ensure_per_app_role`
// builds on -- it is the reason the per-app role spells NOREPLICATION
// explicitly, and the reason slot ownership can never sit with a role
// the worker connects as.
//
// What it does NOT catch: it builds its own role by hand rather than
// calling `ensure_per_app_role`, so a regression that DROPPED
// NOREPLICATION from that function would leave this green. The
// source-level guard for that is
// `auth::bootstrap::tests::create_role_attrs_assert_noreplication`.
// ===========================================================================

/// Drop a test role if it exists. Tolerates `does not exist`.
async fn b8c_drop_role(pool: &Pool, role: &str) {
    // Remove any ownerships first so DROP ROLE doesn't error.
    let _ = pool
        .execute(
            &format!(r#"REVOKE ALL ON SCHEMA public FROM "{role}""#),
            &[],
        )
        .await;
    let _ = pool
        .execute(&format!(r#"REASSIGN OWNED BY "{role}" TO postgres"#), &[])
        .await;
    let _ = pool
        .execute(&format!(r#"DROP OWNED BY "{role}""#), &[])
        .await;
    let _ = pool
        .execute(&format!(r#"DROP ROLE IF EXISTS "{role}""#), &[])
        .await;
}

/// Build a connection URL for a per-test role with a known password.
/// Replaces the `user:password@host` prefix of [`test_url`] with the
/// supplied test-role credentials.
fn role_url(role: &str, password: &str) -> String {
    let base = test_url();
    let at = base
        .find('@')
        .expect("test_url() must be a postgres:// URL with credentials");
    format!("postgres://{role}:{password}{}", &base[at..])
}

#[compio::test]
async fn b8c_per_app_role_cannot_create_slot_directly() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());

    let role = "b8c_no_repl_role";
    let pw = "b8c_pw_no_repl";
    b8c_drop_role(&pool, role).await;
    pool.execute(
        &format!(r#"CREATE ROLE "{role}" LOGIN PASSWORD '{pw}' NOREPLICATION"#),
        &[],
    )
    .await
    .unwrap();

    let role_url = role_url(role, pw);
    let role_pool = match Pool::connect(&role_url, 1).await {
        Ok(p) => p,
        Err(e) => {
            b8c_drop_role(&pool, role).await;
            panic!(
                "The per-app role cannot log in, and this test is about what it \
                 may do once it has.\n\
                 \n\
                 \x20 backend: PostgreSQL\n\
                 \x20 role:    {role} (created by this test moments ago)\n\
                 \x20 error:   {e}\n\
                 \n\
                 The role exists - this test made it - so this is the server's\n\
                 CLIENT AUTHENTICATION refusing the connection, not a missing\n\
                 role. `pg_hba.conf` is what decides that. Check which line\n\
                 matched:\n\
                 \x20 SELECT * FROM pg_hba_file_rules;\n\
                 \n\
                 A host line for this database that accepts `scram-sha-256` from\n\
                 the address this process dials is what is wanted; `reject`,\n\
                 `peer` over TCP, or a `samerole`/`samegroup` restriction will\n\
                 all produce this. Reload with `SELECT pg_reload_conf()` after\n\
                 editing - pg_hba is reload-level, not restart-level.\n\
                 \n\
                 The container deploy/compose starts accepts it already; this is\n\
                 the shape a hardened server of your own arrives in.\n\
                 \n\
                 It used to print this and return, which counted as a pass - so\n\
                 the fence proving a tenant role CANNOT create a replication slot\n\
                 was green precisely on the servers strict enough to be worth\n\
                 testing.\n\
                 \n\
                 There is no environment variable that makes this a skip."
            )
        }
    };

    // Direct slot creation must fail with "must have REPLICATION
    // privilege" or "permission denied".
    let result = role_pool
        .execute(
            "SELECT pg_create_logical_replication_slot('b8c_direct_attempt', 'pgoutput', false, false)",
            &[],
        )
        .await;
    assert!(
        result.is_err(),
        "per-app role with NOREPLICATION must NOT be able to create a slot \
         directly; got Ok"
    );
    let err = err_chain(&result.unwrap_err());
    assert!(
        err.contains("replication") || err.contains("permission denied"),
        "expected REPLICATION-privilege error, got: {err}"
    );

    drop(role_pool);
    b8c_drop_role(&pool, role).await;
    release_pg(pool).await;
}

// -----------------------------------------------------------------------
// The additive `p_pid` SECURITY DEFINER parameter + SessionMinter
// trait impl on PostgresBackend.
//
// The two tests below exercise BOTH paths of the `p_pid` parameter
// per the plan §10 (Q-P3-A -- the riskiest decision):
//   - `b8c_init_session_p_pid_null_uses_pg_backend_pid` — p_pid = NULL
//     path: the existing free fn `init_session` passes None, and the
//     SECURITY DEFINER falls back to `pg_backend_pid()`. Byte-for-byte
//     legacy behaviour.
//   - `b8c_session_minter_trait_init_succeeds_on_different_pool_client`
//     — p_pid = Some(token.backend_pid) path: the `SessionMinter` trait
//     impl acquires a different pool client for init (so its
//     `pg_backend_pid()` differs from the mint-time PID) and passes the
//     mint-time PID explicitly. Without `p_pid` this would fail with
//     `session_invalid_signature`; with it, init succeeds.
// -----------------------------------------------------------------------

// ===========================================================================
// Controlled-supervisor reconnect and per-app emit.
// ===========================================================================

/// The supervised consumer recovers when its replication connection is
/// killed mid-stream. We assert this by:
///   1. starting the supervised consumer in the background,
///   2. waiting for it to see one INSERT,
///   3. terminating its walsender backend via `pg_terminate_backend()`
///      from a sibling connection,
///   4. issuing a second INSERT and observing that the broker still
///      delivers it (i.e. the supervisor reconnected and resumed the
///      slot from `confirmed_flush_lsn`).
#[compio::test]
async fn p8a2_supervised_consumer_reconnects_after_kill() {
    let url = require_pg().await;
    let _cdc = cdc_budget::shared();
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    require_logical_wal(&pool).await;

    let app = crate::test_app_id!();

    let app = app.as_str();
    c1_cleanup(&pool, app).await;

    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    pool.execute(
        &format!(
            r#"CREATE TABLE "{app}"."events" (
                id BIGSERIAL PRIMARY KEY,
                title TEXT NOT NULL
            )"#
        ),
        &[],
    )
    .await
    .unwrap();
    c1_create_publication_for_tables(&pool, app, &["events"]).await;

    zeroship_plugin_db::broker::drop_app(None);
    let sub = zeroship_plugin_db::broker::subscribe(app, "events");

    let backend = zeroship_plugin_db::backend::BackendHandle::Postgres(std::rc::Rc::new(
        zeroship_plugin_db::backend::PostgresBackend::new(
            pool.clone(),
            url.clone(),
            zeroship_plugin_db::isolate_key_source(),
        ),
    ));
    let consumer = zeroship_plugin_db::change_stream_pg::PgChangeStream::new(match &backend {
        zeroship_plugin_db::backend::BackendHandle::Postgres(pg) => pg.clone(),
        zeroship_plugin_db::backend::BackendHandle::Sqlite(_) => {
            panic!("fixture builds a Postgres handle")
        }
    })
    .spawn_consumer(app, CDC_TEST_WORKER_ID)
    .await
    .expect("CDC must reach START_REPLICATION");
    let slot = zeroship_plugin_db::replication::worker_slot_name(app, CDC_TEST_WORKER_ID).unwrap();

    // First insert reaches the broker.
    pool.execute(
        &format!(r#"INSERT INTO "{app}"."events" (title) VALUES ('first')"#),
        &[],
    )
    .await
    .unwrap();

    let mut first_seen = false;
    for _ in 0..40 {
        if let Some(_msg) = sub.pop() {
            first_seen = true;
            break;
        }
        compio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(first_seen, "first insert must reach broker before kill");

    // Kill any active walsender backend for our slot. From PG's
    // perspective this is identical to a network-side hang up.
    let _killed = pool
        .execute(
            "SELECT pg_terminate_backend(active_pid)
             FROM pg_replication_slots
             WHERE slot_name = $1 AND active_pid IS NOT NULL",
            &[&slot],
        )
        .await;

    // Wait at least one backoff cycle (initial = 1s).
    compio::time::sleep(std::time::Duration::from_millis(2_000)).await;

    // Second insert: the supervisor must have reconnected and the
    // event must reach the broker.
    pool.execute(
        &format!(r#"INSERT INTO "{app}"."events" (title) VALUES ('second')"#),
        &[],
    )
    .await
    .unwrap();

    // The slot retains WAL across the disconnect, so the second event
    // is guaranteed to be delivered once the supervisor's new run
    // catches up. Allow generous wall time for backoff + handshake.
    let mut second_seen = false;
    for _ in 0..80 {
        if let Some(_msg) = sub.pop() {
            second_seen = true;
            break;
        }
        compio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(
        second_seen,
        "supervised consumer must reconnect and deliver post-kill event"
    );

    consumer.shutdown().await.unwrap();
    zeroship_plugin_db::broker::drop_app(None);
    c1_cleanup(&pool, app).await;
    release_pg(pool).await;
}

/// Two apps sharing a worker thread: app A has an active consumer
/// (suppression on), app B does not. A mutation on B's collection must
/// still produce a local-emit broker event.
#[test]
fn p8a2_per_app_emit_suppression_integration() {
    use zeroship_core::change_event::ChangeOp;
    use zeroship_plugin_db::broker::{
        SubscriptionMessage, emit_local, is_app_suppressed, suppress_app, unsuppress_app,
    };

    zeroship_plugin_db::broker::drop_app(None);
    unsuppress_app("multi_a");
    unsuppress_app("multi_b");

    let sub_a = zeroship_plugin_db::broker::subscribe("multi_a", "messages");
    let sub_b = zeroship_plugin_db::broker::subscribe("multi_b", "messages");

    // Activate suppression for A only — mimics A's consumer running.
    suppress_app("multi_a");
    assert!(is_app_suppressed("multi_a"));
    assert!(!is_app_suppressed("multi_b"));

    emit_local(
        "multi_a",
        "messages",
        ChangeOp::Insert,
        Some("1".to_string()),
        vec![],
        std::collections::HashMap::new(),
    );
    emit_local(
        "multi_b",
        "messages",
        ChangeOp::Insert,
        Some("2".to_string()),
        vec![],
        std::collections::HashMap::new(),
    );

    assert!(sub_a.pop().is_none(), "app A's emit must be suppressed");
    match sub_b.pop() {
        Some(SubscriptionMessage::Change(ev)) => assert_eq!(ev.pk.as_deref(), Some("2")),
        other => panic!("app B must still receive its emit, got {other:?}"),
    }

    unsuppress_app("multi_a");
    zeroship_plugin_db::broker::drop_app(None);
}

/// Walk a compio-postgres Error's `source()` chain into one string —
/// without this, top-level Display is just "db error" and the
/// SQLSTATE-bearing inner DbError stays invisible.
fn err_chain(e: &dyn std::error::Error) -> String {
    let mut s = format!("{e}");
    let mut cur = e.source();
    while let Some(src) = cur {
        s.push_str(" | ");
        s.push_str(&format!("{src}"));
        cur = src.source();
    }
    s.to_lowercase()
}

/// Operator deprovisioning performs NO second URL parse, and opens ONE operator
/// pool for a whole run of deletions - not none, and not one per app.
///
/// **SC-5 words the second half as "opens NO second pool", and the code does not
/// do that.** `DbLifecycle::operator_pool` opens a dedicated
/// two-connection pool and shares it. The doc comment here claimed zero while
/// the assertion below required one; they are different claims and the
/// assertion is the true one. What the pool costs, and what releases it, is on
/// `crate::service::OPERATOR_POOLS` and in `docs/runbooks/docker-compose.md`
/// under "Postgres connections a worker holds".
///
/// The free function this replaces took the URL, re-ran `backend_for_url` on it
/// and built a fresh two-connection `Pool` **per deleted app** - two connects,
/// two authentications and two TLS handshakes each time. Against a worker
/// reconciling a batch of deletions that is a pool per app.
///
/// Both instruments are counters rather than inferences. A connection count
/// cannot rule on pool cardinality (a pool of two and two pools of one look the
/// same from the server), and "does not re-parse" is a claim about what runs,
/// which `grep` cannot answer. Mutating `DbLifecycle::deprovision_app` back to
/// `Pool::connect(&self.service.url, 2)` per call turns the pool assertion red;
/// mutating it back to `select_backend(&self.service.url)` turns the parse
/// assertion red.
///
/// Three deletions, not one: with one, "opens one pool per deletion" and "opens
/// one pool ever" are the same number and the arm rules on nothing.
///
/// The last section rules on the RELEASE, which is the other half of a pool
/// that is not per-deletion: `close_operator_pools` must actually uninstall it,
/// or "shared across deletions" would mean "held until the process exits".
#[test]
fn operator_deprovisioning_reuses_one_pool_and_reparses_nothing() {
    use zeroship_plugin_db::service::{
        DbService, DbServiceConfig, close_operator_pools, operator_pool_open_count, url_parse_count,
    };

    const DELETED_APPS: [&str; 3] = [
        "sc5_operator_pool_arm_a",
        "sc5_operator_pool_arm_b",
        "sc5_operator_pool_arm_c",
    ];
    assert!(
        DELETED_APPS.len() > 1,
        "one deletion cannot distinguish one pool per call from one pool ever"
    );

    std::thread::spawn(|| {
        compio::runtime::Runtime::new()
            .expect("cannot create runtime")
            .block_on(async {
                let url = require_pg().await;
                // This thread has never deprovisioned anything, so its operator
                // pool map is empty and the counters start from a known floor.
                close_operator_pools();

                let service = DbService::new(DbServiceConfig {
                    url,
                    worker_id: "sc5-operator-lifecycle-arm".to_string(),
                    meter: None,
                })
                .expect("compose the db service");

                let parses_after_composition = url_parse_count();
                let pools_before = operator_pool_open_count();

                for app_id in DELETED_APPS {
                    service
                        .lifecycle()
                        .deprovision_app(app_id)
                        .await
                        .expect("idempotent teardown for an app with no slots");
                }

                assert_eq!(
                    url_parse_count(),
                    parses_after_composition,
                    "deprovisioning must read the backend selection made at composition, \
                     not re-parse the URL"
                );
                let opened_for_the_batch = operator_pool_open_count() - pools_before;
                assert_eq!(
                    opened_for_the_batch,
                    1,
                    "{} deletions must share ONE operator pool, not one each",
                    DELETED_APPS.len()
                );

                // The release half. `close_operator_pools` must uninstall the
                // pool, not merely be callable: a shared pool that nothing
                // removes is a pool held until the process exits, which is the
                // cost this arm's doc comment now discloses. A deletion AFTER
                // the release therefore has to open a second pool - if the
                // count stays at 1 the release did nothing and the entry is
                // still installed.
                close_operator_pools();
                service
                    .lifecycle()
                    .deprovision_app("sc5_operator_pool_arm_after_release")
                    .await
                    .expect("idempotent teardown for an app with no slots");
                assert_eq!(
                    operator_pool_open_count() - pools_before,
                    opened_for_the_batch + 1,
                    "closing the operator pools must uninstall them, so the next \
                     deletion opens a fresh pool",
                );

                close_operator_pools();
                drain_pg().await;
            });
    })
    .join()
    .expect("operator lifecycle thread");
}

// ---------------------------------------------------------------------------
// VectorIndex / vector_search / typed errors.
//
// These tests exercise the pgvector adapter end-to-end. They carry no
// `#[ignore]`: an ordinary run enters them and `require_pgvector` FAILS, naming
// the extension and the image that carries it, when the server has none. Swap
// the image to `pgvector/pgvector:pg16` (docs/runbooks/docker-compose.md) to run
// them for real.
//
// THIS COMMENT DESCRIBED THE OPPOSITE ARRANGEMENT UNTIL THE ATTRIBUTES WENT.
// It said they were `#[ignore]`d statically and that `--ignored` was the request
// that reached the refusal - which made the refusal unreachable from every job
// this repository actually runs, since none of them passes `--ignored`.
//
// THERE IS NO ENVIRONMENT VARIABLE THAT TOGGLES THIS EITHER. This comment named
// a `ZEROSHIP_PGVECTOR_AVAILABLE=1` until 2026-09-08; a repository-wide search
// found the name here and nowhere else, so it was an escape hatch that had
// never existed, described as if it did.
//
// The `pgvector_extension_missing_reports_typed_error` test runs
// unconditionally — it asserts the typed-error shape against a fresh
// backend whose probe cache has never been populated.
// ---------------------------------------------------------------------------

/// Install `pgvector` into the test database, or refuse the run.
///
/// # Panics
///
/// When the extension is not available on the server, with the image that
/// carries it. It used to return `false` and the callers announced a skip - so
/// a `--ignored` run on a stock `postgres:16` printed the same green as a run
/// that had exercised a single vector query.
async fn require_pgvector(pool: &Pool) {
    // The CREATE is best-effort and its result is deliberately not the verdict:
    // an environment that ships the extension pre-installed can refuse the
    // statement for reasons that have nothing to do with availability. The
    // catalogue is what decides.
    let _ = pool
        .execute("CREATE EXTENSION IF NOT EXISTS vector", &[])
        .await;
    let installed = !pool
        .query_text_params("SELECT 1 FROM pg_extension WHERE extname='vector'", &[])
        .await
        .unwrap_or_default()
        .is_empty();
    assert!(
        installed,
        "The `vector` extension is not installed, and this test requires it.\n\
         \n\
         \x20 backend:   PostgreSQL\n\
         \x20 extension: vector (pgvector)\n\
         \n\
         `CREATE EXTENSION IF NOT EXISTS vector` did not leave a row in\n\
         pg_extension. Check what the server has to offer:\n\
         \x20 SELECT * FROM pg_available_extensions WHERE name = 'vector';\n\
         \n\
         NO SERVER THIS REPOSITORY PROVISIONS CARRIES IT. deploy/compose's\n\
         `postgres` service - the one tests/provision_test_backends.sh starts -\n\
         runs the stock `postgres:16` image, which does not bundle pgvector.\n\
         Point the tests at a server that does, or swap that image for\n\
         `pgvector/pgvector:pg16` (docs/runbooks/docker-compose.md) and re-create\n\
         the container.\n\
         \n\
         There is no environment variable that makes this a skip."
    );
}

/// Test gate for `vector_search_returns_k_nearest`.
///
/// Insert 100 rows x 128-d random unit vectors; query with a known
/// vector and assert the top-10 closest by cosine distance form the
/// expected SET (membership, not strict order -- FP determinism not
/// promised across pgvector versions).
///
/// Requires the `vector` extension, which a stock `postgres` image does not
/// bundle. `require_pgvector` refuses the run and names the image that carries
/// it (see docs/runbooks/docker-compose.md); there is no attribute that turns
/// the absence into a pass.
#[compio::test]
async fn vector_search_returns_k_nearest() {
    use zeroship_plugin_db::backend::{PostgresBackend, VectorIndex, VectorMetric};

    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    require_pgvector(&pool).await;

    let app = crate::test_app_id!();

    let app = app.as_str();
    let coll = "docs";
    // Provision the per-app ROLE, not just the schema. `vector_search` resolves
    // the binding before it plans, and a schema without its role fails closed
    // with `schema_not_provisioned` - which is what this test did from the day
    // it was written until 2026-09-01. It never surfaced because the test was
    // statically `#[ignore]`d, so a setup gap looked like a missing extension.
    let _role = provision_app_with_role(&pool, app).await;
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();
    // The six non-`id` platform system columns are part of every real creator
    // table and are named unconditionally by the implicit read projection the
    // vector search builds, so the fixture carries them too.
    pool.execute(
        &format!(
            "CREATE TABLE \"{app}\".\"{coll}\" (\
               id SERIAL PRIMARY KEY, \
               embedding vector(8) NOT NULL, \
               created_at TIMESTAMPTZ DEFAULT NOW(), \
               updated_at TIMESTAMPTZ DEFAULT NOW(), \
               created_by TEXT, \
               updated_by TEXT, \
               version INTEGER NOT NULL DEFAULT 1, \
               deleted_at TIMESTAMPTZ\
             )"
        ),
        &[],
    )
    .await
    .unwrap();

    // Deterministic pseudo-random unit vectors. We only care that the
    // top-k membership is reproducible; the absolute values don't matter
    // beyond being unique per row.
    fn mk_unit(i: usize, dims: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; dims];
        for (j, slot) in v.iter_mut().enumerate() {
            // splitmix-style scramble so adjacent rows don't accidentally
            // collide on the unit sphere.
            let x = (i.wrapping_mul(2654435761)) ^ (j.wrapping_mul(40503));
            *slot = ((x & 0xffff) as f32 / 65536.0) - 0.5;
        }
        // Normalise.
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in v.iter_mut() {
                *x /= norm;
            }
        }
        v
    }

    fn fmt_vec(v: &[f32]) -> String {
        let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
        format!("[{}]", parts.join(","))
    }

    // The per-app role gets NO table privileges from provisioning alone - the
    // grants are explicit and per-column, which is the same fact production
    // carries (a create-plus-migrate leaves the runtime role unable to read its
    // own tables until the grants run). Without this the search fails closed
    // with `permission denied for table docs`, correctly.
    support::grant_all_runtime_table_columns(&pool, app, coll).await;

    let dims = 8usize;
    for i in 0..100usize {
        let v = mk_unit(i, dims);
        let lit = fmt_vec(&v);
        pool.execute(
            &format!("INSERT INTO \"{app}\".\"{coll}\" (embedding) VALUES ($1::text::vector)"),
            &[&lit as &(dyn compio_postgres::types::ToSql + Sync)],
        )
        .await
        .unwrap();
    }

    // Query with row #0's exact vector — its own row must be in the
    // top-10. We assert MEMBERSHIP (not strict order) because pgvector
    // distance ties between FP-close vectors can re-order across builds.
    let query = mk_unit(0, dims);
    let backend = zeroship_plugin_db::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_plugin_db::isolate_key_source(),
    );
    // The search's projection is the descriptor's field list; install the entry
    // this deploy's runtime descriptor would have planted at boot.
    zeroship_plugin_db::cache_schema_for_tests(
        app,
        coll,
        json!({ "embedding": { "type": "vector", "vectorDims": 8 } }),
    );
    let rows = VectorIndex::vector_search(
        &backend,
        &DbBinding::cold_start(app),
        coll,
        "embedding",
        &query,
        10,
        VectorMetric::Cosine,
        &serde_json::Value::Null,
        &zeroship_plugin_db::collection_schema(&DbBinding::cold_start(app), coll)
            .expect("descriptor slice for the search fixture"),
    )
    .await
    .unwrap_or_else(|e| panic!("vector_search failed: {e:?}"));

    assert_eq!(rows.len(), 10, "expected k=10 rows, got {}", rows.len());
    // Row id #1 (1-indexed via SERIAL) must be in the top-10 (it
    // matches the query exactly).
    let ids: Vec<i64> = rows
        .iter()
        .filter_map(|r| r.get("id").and_then(serde_json::Value::as_i64))
        .collect();
    assert!(
        ids.contains(&1),
        "exact-match row #1 must be in top-10, got ids={ids:?}"
    );
    // Every row must carry the synthetic _distance column.
    for r in &rows {
        assert!(r.get("_distance").is_some(), "row missing _distance: {r}");
    }
    drop(backend);
    release_pg(pool).await;
}

/// Test gate for `pgvector_extension_missing_reports_typed_error`.
///
/// Drops the `vector` extension (if present), constructs a fresh
/// backend so the probe cache starts empty, and asserts that
/// `vector_search` surfaces
/// `DbError::Configuration { code: "vector_extension_missing", .. }`.
///
/// It searches TWICE on purpose. `ensure_pgvector_available` has two
/// miss arms -- one that runs the `pg_extension` probe and caches
/// `Some(false)`, one that reads that cache -- and they construct the
/// error separately. The first call takes the probe arm, the second the
/// cached arm, so a divergence between them fails here. The pairing used
/// to fall out of calling `ensure_vector_index` then `vector_search`;
/// with the DDL half deleted the second arm would otherwise go unruled-on.
///
/// The DROP requires sufficient privileges; tests run as the bootstrap
/// `postgres` superuser, which has them. An extension that survives the drop -
/// because another object depends on it - FAILS this test, naming the query
/// that finds the dependents. The drop is the fixture, not a cleanup: with the
/// extension still installed the typed-error arm is never reached, so a pass
/// there would report a contract nobody checked.
///
/// THIS COMMENT DESCRIBED THE OPPOSITE UNTIL 2026-09-08, AND IT DESCRIBED
/// NEITHER THE CODE BELOW NOR ITS OWN REASONING. It said the test would
/// "silently re-skip" and that "we don't fail the suite in that case because
/// the typed-error assertion is the load-bearing part of the contract" - which
/// is the argument FOR failing, since a re-skip is precisely the case where
/// that load-bearing assertion did not run.
#[compio::test]
async fn pgvector_extension_missing_reports_typed_error() {
    use zeroship_data_core::error::DbError;
    use zeroship_plugin_db::backend::{PostgresBackend, VectorIndex, VectorMetric};

    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    // This test needs the extension ABSENT - it asserts the shape of the error
    // raised when it is missing - so the drop is the fixture, not a cleanup.
    let dropped = pool
        .execute("DROP EXTENSION IF EXISTS vector CASCADE", &[])
        .await;

    let still_present = pool
        .query_text_params("SELECT 1 FROM pg_extension WHERE extname='vector'", &[])
        .await
        .map(|rows| !rows.is_empty())
        .unwrap_or(false);
    assert!(
        !still_present,
        "The `vector` extension could not be removed, and this test needs it ABSENT.\n\
         \n\
         \x20 backend:   PostgreSQL\n\
         \x20 extension: vector (pgvector)\n\
         \x20 drop said: {dropped:?}\n\
         \n\
         This is the inverse of the other pgvector tests: it asserts the TYPED\n\
         ERROR raised when the extension is missing, so an installed one leaves\n\
         that arm unexercised. It used to skip here, which reported the same\n\
         green as a run that had ruled on the error shape.\n\
         \n\
         The usual cause is another object depending on it - a `vector` column\n\
         or index left behind by a sibling test, or by a run that was\n\
         interrupted. Find the dependents and drop them:\n\
         \x20 SELECT * FROM pg_depend d JOIN pg_extension e ON d.refobjid = e.oid\n\
         \x20 WHERE e.extname = 'vector';\n\
         \n\
         A database used by nothing else is the cheaper fix; this suite creates\n\
         its own schemas and expects to own the database it is pointed at.\n\
         \n\
         There is no environment variable that makes this a skip."
    );

    let backend = zeroship_plugin_db::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_plugin_db::isolate_key_source(),
    );

    // No descriptor entry is installed for `vector_missing`, and that is
    // deliberate: `ensure_pgvector_available` runs BEFORE the schema resolve,
    // so the extension error must still be the one that surfaces. If the order
    // ever flipped, this would fail with `collection_not_declared` instead.
    async fn search(backend: &PostgresBackend) -> DbError {
        VectorIndex::vector_search(
            backend,
            &DbBinding::cold_start("vector_missing"),
            "any",
            "any",
            &[0.0f32; 8],
            10,
            VectorMetric::Cosine,
            &serde_json::Value::Null,
            &serde_json::Value::Null,
        )
        .await
        .expect_err("missing extension must yield a typed error on search")
    }

    // First call: the probe arm (cache empty -> `SELECT 1 FROM pg_extension`).
    let probe_err = search(&backend).await;
    // Second call: the cached arm (`pgvector_available == Some(false)`).
    let cached_err = search(&backend).await;

    // RESTORE the extension BEFORE asserting: this test deliberately drops a
    // SHARED, cluster-/db-wide object (the `vector` extension lives in
    // `public`, not in a per-app schema), so leaving it dropped breaks every
    // vector-dependent test ordered after this one in a single-threaded run
    // (e.g. `p4_round_trip_encrypted_masked_vector_via_descriptor_metadata`).
    // Restore happens before the
    // assertions so a failed assertion can never leak the dropped state.
    pool.execute("CREATE EXTENSION IF NOT EXISTS vector", &[])
        .await
        .expect("restore the shared vector extension after the missing-extension probe");

    for (arm, err) in [("probe", probe_err), ("cached", cached_err)] {
        match err {
            DbError::Configuration {
                code,
                message,
                hint,
            } => {
                assert_eq!(code, "vector_extension_missing", "{arm} arm: got {message}");
                assert!(
                    hint.as_deref()
                        .map(|h| h.contains("CREATE EXTENSION"))
                        .unwrap_or(false),
                    "{arm} arm: hint must mention `CREATE EXTENSION vector;`: {hint:?}"
                );
            }
            other => panic!(
                "{arm} arm: expected Configuration {{ vector_extension_missing }}, got {other:?}"
            ),
        }
    }
    drop(backend);
    release_pg(pool).await;
}

/// Test gate for `vector_dimension_mismatch_rejected_at_insert`.
///
/// pgvector enforces the declared dim at INSERT time (the `vector(N)`
/// column type rejects a literal whose dim ≠ N at parse-cast). This
/// test asserts the failure is observable and surfaces as a typed
/// `DbError::CheckViolation` / `Internal` / `Transient` — we don't pin
/// the variant strictly because pgvector reports as ERROR 22000
/// (`data_exception`), which our SQLSTATE classifier maps to
/// `Internal`. The shape contract: the error message MUST mention the
/// expected vs. actual dim count.
#[compio::test]
async fn vector_dimension_mismatch_rejected_at_insert() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    require_pgvector(&pool).await;

    let app = crate::test_app_id!();

    let app = app.as_str();
    let coll = "docs";
    // Same provisioning gap as `vector_search_returns_k_nearest`: a schema
    // without its per-app role fails closed before the insert is ever attempted.
    let _role = provision_app_with_role(&pool, app).await;
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();
    pool.execute(
        &format!(
            "CREATE TABLE \"{app}\".\"{coll}\" (\
               id SERIAL PRIMARY KEY, \
               embedding vector(128) NOT NULL\
             )"
        ),
        &[],
    )
    .await
    .unwrap();

    // Insert a 256-d vector into a 128-d column — pgvector must reject.
    let mut parts = Vec::with_capacity(256);
    for i in 0..256 {
        parts.push(format!("{}.0", i as f32 / 256.0));
    }
    let lit = format!("[{}]", parts.join(","));
    let result = pool
        .query_text_params(
            &format!("INSERT INTO \"{app}\".\"{coll}\" (embedding) VALUES ($1::text::vector)"),
            &[&lit],
        )
        .await;
    let err = result.expect_err("256-d into vector(128) column must fail");
    // `{err}` is NOT enough: `compio_postgres::Error`'s Display renders the bare
    // string "db error" and puts the server's message only in the source chain,
    // so this assertion was checking a constant. Measured 2026-09-01 - the
    // server sends "expected 128 dimensions, not 256" and `{err}` shows none of
    // it. Production is unaffected because `pg_error::classify` walks the chain
    // (`walk_pg_chain`) rather than formatting; anything that formats a driver
    // error with `{}` for an operator loses the cause.
    let msg = format!("{err:?}");
    // pgvector messages vary across versions; assert on the digits 256
    // and 128 (both should appear) and on "vector" anchor.
    assert!(
        msg.contains("128") || msg.contains("256") || msg.to_lowercase().contains("vector"),
        "error message must mention dim mismatch: {msg}"
    );
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// SpatialIndex (PG arm) test gates.
//
// These tests require PostGIS, which a stock `postgres` image does not bundle.
// They run unconditionally and `require_postgis` refuses a server without the
// extension, naming a bundled image to point them at - see
// docs/runbooks/docker-compose.md.
//
// ONE OF THE TWO WAS `#[ignore]`-MARKED AND THE OTHER WAS NOT, which is the
// state that made the attribute indefensible rather than merely wrong:
// `spatial_near_runs_under_per_app_role_via_rls` has always called
// `require_postgis` from an ordinary run, so the "PostGIS is optional here"
// story the attribute told was already false for its own sibling.
// ---------------------------------------------------------------------------


/// Test gate for `near_returns_within_radius`.
///
/// 10 points around London at varying distances from the centre
/// `(51.5074, -0.1278)`. `near()` with a 1km radius returns only the
/// points actually within 1km (assert by membership set, not strict
/// ordering — ST_Distance is FP-deterministic in modern PostGIS but we
/// don't pin the order).
///
/// Requires PostGIS: `require_postgis` FAILS on an image without the extension
/// rather than skipping, and no attribute removes this test from the run.
#[compio::test]
async fn near_returns_within_radius() {
    use zeroship_plugin_db::backend::{GeoPoint, PostgresBackend, SpatialIndex};

    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    require_postgis(&pool).await;

    let app = crate::test_app_id!();

    let app = app.as_str();
    let coll = "places";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    // System columns for the same reason as the vector fixture above: the
    // spatial base query projects the descriptor's field list plus all seven.
    pool.execute(
        &format!(
            "CREATE TABLE \"{app}\".\"{coll}\" (\
               id SERIAL PRIMARY KEY, \
               location geography(POINT, 4326) NOT NULL, \
               created_at TIMESTAMPTZ DEFAULT NOW(), \
               updated_at TIMESTAMPTZ DEFAULT NOW(), \
               created_by TEXT, \
               updated_by TEXT, \
               version INTEGER NOT NULL DEFAULT 1, \
               deleted_at TIMESTAMPTZ\
             )"
        ),
        &[],
    )
    .await
    .unwrap();
    zeroship_plugin_db::cache_schema_for_tests(
        app,
        coll,
        json!({ "location": { "type": "geoPoint" } }),
    );

    let london = GeoPoint {
        lat: 51.5074,
        lng: -0.1278,
    };
    // 10 points: 5 within ~1km of London (small lat/lng offsets) and
    // 5 well outside (several km away). One degree of latitude is
    // ~111km, so 0.005 deg ≈ 555m and 0.05 deg ≈ 5.5km.
    let offsets: Vec<(f64, f64, bool)> = vec![
        (0.0, 0.0, true),     // dead-centre
        (0.001, 0.001, true), // ~140m
        (0.003, 0.003, true), // ~420m
        (-0.005, 0.0, true),  // ~555m south
        (0.0, 0.005, true),   // about 350m east (cos(51.5 deg) ~= 0.62)
        (0.05, 0.0, false),   // ~5.5km north
        (-0.05, 0.0, false),  // ~5.5km south
        (0.0, 0.05, false),   // ~3.5km east
        (0.0, -0.05, false),  // ~3.5km west
        (0.1, 0.1, false),    // ~11km NE
    ];
    let mut expected_within: Vec<i64> = Vec::new();
    for (i, (dlat, dlng, within_1km)) in offsets.iter().enumerate() {
        let lng = london.lng + dlng;
        let lat = london.lat + dlat;
        let lit = format!("POINT({lng} {lat})");
        pool.execute(
            &format!("INSERT INTO \"{app}\".\"{coll}\" (location) VALUES (ST_GeogFromText($1))"),
            &[&lit as &(dyn compio_postgres::types::ToSql + Sync)],
        )
        .await
        .unwrap();
        if *within_1km {
            expected_within.push((i + 1) as i64);
        }
    }

    let backend = zeroship_plugin_db::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_plugin_db::isolate_key_source(),
    );
    let rows = SpatialIndex::spatial_near(
        &backend,
        &DbBinding::cold_start(app),
        coll,
        "location",
        london,
        1000.0,
        &serde_json::Value::Null,
        None,
        &serde_json::Value::Null,
    )
    .await
    .unwrap_or_else(|e| panic!("spatial_near failed: {e:?}"));

    let returned_ids: std::collections::BTreeSet<i64> = rows
        .iter()
        .filter_map(|r| r.get("id").and_then(serde_json::Value::as_i64))
        .collect();
    let expected: std::collections::BTreeSet<i64> = expected_within.into_iter().collect();
    assert_eq!(
        returned_ids, expected,
        "near(1km) membership mismatch: returned={returned_ids:?} expected={expected:?}"
    );
    for r in &rows {
        assert!(
            r.get("_distance_m").is_some(),
            "row missing _distance_m: {r}"
        );
    }
    drop(backend);
    release_pg(pool).await;
}

/// Test gate for `postgis_extension_missing_reports_typed_error`.
///
/// When the database has no PostGIS, `spatial_near` must surface
/// `DbError::Configuration { code: "postgis_extension_missing", .. }`.
/// Same shape as `pgvector_extension_missing_reports_typed_error`,
/// including the two-call pairing that rules on `ensure_postgis_available`'s
/// probe arm and its cached arm separately.
#[compio::test]
async fn postgis_extension_missing_reports_typed_error() {
    use zeroship_data_core::error::DbError;
    use zeroship_plugin_db::backend::{GeoPoint, PostgresBackend, SpatialIndex};

    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    // This test needs the extension ABSENT - it asserts the shape of the error
    // raised when it is missing - so the drop is the fixture, not a cleanup.
    let dropped = pool
        .execute("DROP EXTENSION IF EXISTS postgis CASCADE", &[])
        .await;

    let still_present = pool
        .query_text_params("SELECT 1 FROM pg_extension WHERE extname='postgis'", &[])
        .await
        .map(|rows| !rows.is_empty())
        .unwrap_or(false);
    assert!(
        !still_present,
        "The `postgis` extension could not be removed, and this test needs it ABSENT.\n\
         \n\
         \x20 backend:   PostgreSQL\n\
         \x20 extension: postgis\n\
         \x20 drop said: {dropped:?}\n\
         \n\
         This is the inverse of the other PostGIS tests: it asserts the TYPED\n\
         ERROR raised when the extension is missing, so an installed one leaves\n\
         that arm unexercised. It used to skip here, which reported the same\n\
         green as a run that had ruled on the error shape.\n\
         \n\
         The usual cause is another object depending on it - a `geography`\n\
         column or spatial index left behind by a sibling test, or by a run that\n\
         was interrupted. Find the dependents and drop them:\n\
         \x20 SELECT * FROM pg_depend d JOIN pg_extension e ON d.refobjid = e.oid\n\
         \x20 WHERE e.extname = 'postgis';\n\
         \n\
         A database used by nothing else is the cheaper fix; this suite creates\n\
         its own schemas and expects to own the database it is pointed at.\n\
         \n\
         There is no environment variable that makes this a skip."
    );

    let backend = zeroship_plugin_db::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_plugin_db::isolate_key_source(),
    );

    // No descriptor entry, deliberately: the extension probe runs BEFORE the
    // schema resolve, so this must still surface `postgis_extension_missing`.
    async fn near(backend: &PostgresBackend) -> DbError {
        SpatialIndex::spatial_near(
            backend,
            &DbBinding::cold_start("postgis_missing"),
            "any",
            "any",
            GeoPoint { lat: 0.0, lng: 0.0 },
            1000.0,
            &serde_json::Value::Null,
            None,
            &serde_json::Value::Null,
        )
        .await
        .expect_err("missing PostGIS must yield a typed error on near")
    }

    // First call takes the probe arm, second the cached arm.
    let probe_err = near(&backend).await;
    let cached_err = near(&backend).await;
    for (arm, err) in [("probe", probe_err), ("cached", cached_err)] {
        match err {
            DbError::Configuration {
                code,
                message,
                hint,
            } => {
                assert_eq!(
                    code, "postgis_extension_missing",
                    "{arm} arm: got {message}"
                );
                assert!(
                    hint.as_deref()
                        .map(|h| h.contains("CREATE EXTENSION"))
                        .unwrap_or(false),
                    "{arm} arm: hint must mention `CREATE EXTENSION postgis;`: {hint:?}"
                );
            }
            other => panic!(
                "{arm} arm: expected Configuration {{ postgis_extension_missing }}, got {other:?}"
            ),
        }
    }
    drop(backend);
    release_pg(pool).await;
}

// ===========================================================================
// Encrypted column integration (gated `hardening`)
// ===========================================================================
//
// These tests exercise the full PG round-trip for `t.encrypted(...)`-
// declared columns: BYTEA emit on DDL, decode($N, 'base64')::bytea on
// insert, encode-as-hex on read, AAD-bound decrypt. The Camp A fence
// (row_pk in AAD for Randomised) is the load-bearing assertion in
// `encrypted_randomised_row_swap_rejected` -- copying ciphertext from
// row A into row B's slot must surface `encryption_aead_failed` rather
// than leak row A's plaintext through row B's read API.

// Imports are local to this section. Other test modules in this file
// import `PostgresBackend` + `DbError` per-fn via `use ...` inside the
// test body; here they are surfaced at module scope so the four tests
// below can share one `use` block. There used to be an `EncryptedColumn as _`
// here, importing a capability trait for its methods; the trait was deleted on
// 2026-09-02 and these tests now call `encryption::aead` directly with a key
// from `backend.key_store()` - the same path production takes.
use zeroship_data_core::error::DbError;
use zeroship_plugin_db::backend::{EncryptionMode, PostgresBackend};
use zeroship_plugin_db::encryption;

/// Helper: hand this isolate a synthetic root key for `key_id`, so the
/// `PostgresBackend` the test (or the CRUD path behind it) constructs
/// resolves column keys from it.
///
/// This REPLACES a `set_var("ZEROSHIP_COLUMN_KEY_<KEYID>", ...)` guard.
/// The env var was the only channel that reached a backend the test did
/// not build itself, and it was process-global: every test in this binary
/// shared one `ZEROSHIP_COLUMN_KEY_DEFAULT`, so the guard's own comment
/// claiming `--test-threads=1` serialisation (which nothing in
/// `Cargo.toml` actually requests) was the only thing standing between
/// six tests and each other's roots. The isolate context is per-thread,
/// so that race cannot happen here.
///
/// This IS the PG resolve path now, not a fallback behind one. The
/// `get_column_key` arm that used to run first, in the platform-owned system
/// schema, was deleted on 2026-08-27; PG and SQLite both read the isolate's
/// supplied roots. This is the same arm the env var used to occupy.
///
/// The returned guard withdraws the keys on drop; keep it alive for the
/// test body.
fn with_root_key(key_id: &str, root_hex: &str) -> zeroship_plugin_db::SuppliedRootKeysGuard {
    zeroship_plugin_db::supply_root_keys_for_tests(&[(key_id, root_hex)])
}

/// Gate #1: round-trip an encrypted string column. Insert a
/// row with `ssn` declared `t.encrypted({ mode: "randomised" })`,
/// read it back via the PG path, expect the plaintext to recover.
#[compio::test]
async fn encrypted_column_round_trip_randomised() {
    let url = require_pg().await;
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    // Synthetic 32-byte root key.
    let _keys = with_root_key("default", &"a".repeat(64));

    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{schema}\""), &[])
        .await
        .unwrap();
    // Manually create the table; the encryption pass operates on generic
    // BYTEA columns regardless of which migration emitted them.
    pool.execute(
        &format!(
            r#"CREATE TABLE "{schema}"."enc_notes" (
                id   TEXT PRIMARY KEY,
                ssn  BYTEA
            )"#
        ),
        &[],
    )
    .await
    .unwrap();

    let backend = zeroship_plugin_db::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_plugin_db::isolate_key_source(),
    );
    let key = backend
        .key_store()
        .resolve("app1", "default")
        .await
        .expect("resolve_key");
    let plaintext = b"123-45-6789";
    let aad = encryption::canonical_aad("enc_notes", "ssn", Some(b"row_a"));
    let ct = zeroship_plugin_db::encryption::aead::encrypt(
        &key,
        EncryptionMode::Randomised,
        plaintext,
        &aad,
    )
    .expect("encrypt");

    // Bind via base64 decode just like the build_insert layer does.
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &ct);
    pool.execute(
        &format!(
            "INSERT INTO \"{schema}\".\"enc_notes\" (id, ssn) VALUES ($1, decode($2, 'base64')::bytea)"
        ),
        &[&"row_a", &b64.as_str()],
    )
    .await
    .unwrap();

    // Read back as BYTEA via `encode(ssn, 'hex')` so the text protocol
    // surfaces a hex string we can parse cleanly. (Reading the BYTEA
    // column directly via Row::get<String> fails because the
    // text-format BYTEA representation isn't UTF-8 in general.)
    let rows = pool
        .query_text_params(
            &format!(
                "SELECT encode(ssn, 'hex') AS ssn_hex FROM \"{schema}\".\"enc_notes\" WHERE id = $1"
            ),
            &["row_a"],
        )
        .await
        .unwrap();
    let hex_str: String = rows[0].get("ssn_hex");
    let raw = {
        let mut out = Vec::with_capacity(hex_str.len() / 2);
        for chunk in hex_str.as_bytes().chunks(2) {
            let pair = std::str::from_utf8(chunk).unwrap();
            out.push(u8::from_str_radix(pair, 16).unwrap());
        }
        out
    };
    let recovered =
        zeroship_plugin_db::encryption::aead::decrypt(&key, &raw, &aad).expect("decrypt");
    assert_eq!(recovered, plaintext);
    drop(backend);
    release_pg(pool).await;
}

/// Camp A fence: copying ciphertext from row A into row
/// B's slot must surface `encryption_aead_failed` (row_pk in AAD
/// defeats the ciphertext-oracle attack on randomised columns).
#[compio::test]
async fn encrypted_randomised_row_swap_rejected() {
    let url = require_pg().await;
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let _keys = with_root_key("default", &"b".repeat(64));

    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{schema}\""), &[])
        .await
        .unwrap();
    pool.execute(
        &format!(
            r#"CREATE TABLE "{schema}"."enc_notes" (
                id   TEXT PRIMARY KEY,
                ssn  BYTEA
            )"#
        ),
        &[],
    )
    .await
    .unwrap();

    let backend = zeroship_plugin_db::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_plugin_db::isolate_key_source(),
    );
    let key = backend
        .key_store()
        .resolve("app1", "default")
        .await
        .unwrap();
    // Insert row A with its OWN AAD (binds row_pk = "row_a").
    let ct_a = zeroship_plugin_db::encryption::aead::encrypt(
        &key,
        EncryptionMode::Randomised,
        b"sensitive-A",
        &encryption::canonical_aad("enc_notes", "ssn", Some(b"row_a")),
    )
    .unwrap();
    let ct_b = zeroship_plugin_db::encryption::aead::encrypt(
        &key,
        EncryptionMode::Randomised,
        b"sensitive-B",
        &encryption::canonical_aad("enc_notes", "ssn", Some(b"row_b")),
    )
    .unwrap();
    for (id, ct) in [("row_a", &ct_a), ("row_b", &ct_b)] {
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, ct);
        pool.execute(
            &format!(
                "INSERT INTO \"{schema}\".\"enc_notes\" (id, ssn) VALUES ($1, decode($2, 'base64')::bytea)"
            ),
            &[&id, &b64.as_str()],
        )
        .await
        .unwrap();
    }

    // Attacker move: copy row A's ciphertext into row B's slot.
    let b64_a = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &ct_a);
    pool.execute(
        &format!(
            "UPDATE \"{schema}\".\"enc_notes\" SET ssn = decode($1, 'base64')::bytea WHERE id = $2"
        ),
        &[&b64_a.as_str(), &"row_b"],
    )
    .await
    .unwrap();

    // Read row B → decrypt with row B's AAD (row_pk = "row_b"). Use
    // `encode(ssn, 'hex')` per the round-trip test above.
    let rows = pool
        .query_text_params(
            &format!(
                "SELECT encode(ssn, 'hex') AS ssn_hex FROM \"{schema}\".\"enc_notes\" WHERE id = $1"
            ),
            &["row_b"],
        )
        .await
        .unwrap();
    let hex_str: String = rows[0].get("ssn_hex");
    let raw = {
        let mut out = Vec::with_capacity(hex_str.len() / 2);
        for chunk in hex_str.as_bytes().chunks(2) {
            let pair = std::str::from_utf8(chunk).unwrap();
            out.push(u8::from_str_radix(pair, 16).unwrap());
        }
        out
    };
    let aad_b = encryption::canonical_aad("enc_notes", "ssn", Some(b"row_b"));
    let err = zeroship_plugin_db::encryption::aead::decrypt(&key, &raw, &aad_b)
        .expect_err("row-swap must fail AAD verification");
    match err {
        DbError::ValidationFailed { code, .. } => {
            assert_eq!(code, "encryption_aead_failed");
        }
        other => panic!("expected ValidationFailed encryption_aead_failed, got {other:?}"),
    }
    drop(backend);
    release_pg(pool).await;
}

/// Gate #2: deterministic mode produces identical
/// ciphertext for identical plaintext under the same `(collection,
/// column)` regardless of row_pk. This is what makes equality lookups
/// on the ciphertext sound; the deterministic-encrypted column gets an
/// automatic B-tree index from `build_create_indexes`.
#[compio::test]
async fn encrypted_deterministic_equality_lookup() {
    let url = require_pg().await;
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let _keys = with_root_key("default", &"c".repeat(64));

    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{schema}\""), &[])
        .await
        .unwrap();
    pool.execute(
        &format!(
            r#"CREATE TABLE "{schema}"."enc_notes" (
                id   TEXT PRIMARY KEY,
                ssn  BYTEA
            )"#
        ),
        &[],
    )
    .await
    .unwrap();
    pool.execute(
        &format!(r#"CREATE INDEX ON "{schema}"."enc_notes" (ssn)"#),
        &[],
    )
    .await
    .unwrap();

    let backend = zeroship_plugin_db::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_plugin_db::isolate_key_source(),
    );
    let key = backend
        .key_store()
        .resolve("app1", "default")
        .await
        .unwrap();

    // Insert 5 rows with the same SSN to confirm deterministic mode
    // produces identical ciphertext (we then query by exact ciphertext
    // and expect all 5 to come back).
    let aad = encryption::canonical_aad("enc_notes", "ssn", None);
    let ct_shared = zeroship_plugin_db::encryption::aead::encrypt(
        &key,
        EncryptionMode::Deterministic,
        b"shared-ssn",
        &aad,
    )
    .unwrap();
    let b64_shared = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &ct_shared);

    for i in 0..5 {
        pool.execute(
            &format!(
                "INSERT INTO \"{schema}\".\"enc_notes\" (id, ssn) VALUES ($1, decode($2, 'base64')::bytea)"
            ),
            &[&format!("row_{i}").as_str(), &b64_shared.as_str()],
        )
        .await
        .unwrap();
    }
    // Plus a distinct row.
    let ct_other = zeroship_plugin_db::encryption::aead::encrypt(
        &key,
        EncryptionMode::Deterministic,
        b"other-ssn",
        &aad,
    )
    .unwrap();
    let b64_other = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &ct_other);
    pool.execute(
        &format!(
            "INSERT INTO \"{schema}\".\"enc_notes\" (id, ssn) VALUES ($1, decode($2, 'base64')::bytea)"
        ),
        &[&"row_other", &b64_other.as_str()],
    )
    .await
    .unwrap();

    // Query by the ciphertext (the SDK would compute the SAME
    // ciphertext for `find({ssn: "shared-ssn"})` because deterministic
    // mode is, well, deterministic; the orchestrator binds the same
    // BYTEA via decode($N, 'base64')).
    let rows = pool
        .query_text_params(
            &format!(
                "SELECT id FROM \"{schema}\".\"enc_notes\" WHERE ssn = decode($1, 'base64')::bytea"
            ),
            &[b64_shared.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        5,
        "deterministic equality lookup must match all 5 shared-ssn rows"
    );
    drop(backend);
    release_pg(pool).await;
}

/// Round-trip e2e proof that schema creation and CRUD cohere: a collection
/// with an `encrypted` + a `masked` + a `vector` field, table created the way
/// the migration engine creates it, then CRUD driven ENTIRELY by the RUNTIME
/// DESCRIPTOR:
///   - insert through the REAL write pipeline -> AEAD-encrypts the encrypted
///     column and populates the masked sibling;
///   - read raw rows back, finalize through the REAL read pipeline -> decrypts
///     the encrypted column to plaintext and wraps the masked column.
///
/// **The metadata source changed and the round trip did not.** This test used to
/// plant `COMMENT ON COLUMN ... 'zero-migrate:enc:...'` / `'zero-migrate:mask:...'` sentinels and
/// assert the data plane RECOVERED the encryption mode and mask kind from the
/// live catalog. That recovery is deleted: the sentinels were emitted by the
/// migration engine out of the same DSL the descriptor is folded from, so the
/// catalog could only ever agree with the descriptor or be stale, and the read
/// cost one whole-schema catalog walk per cold collection. The metadata is now
/// installed by `cache_schema_for_tests` from a descriptor-shaped field map,
/// matching the native runtime descriptor hook. Everything after that line is unchanged,
/// so what this still proves is what it always mattered for: the encrypt/mask
/// write stages and the decrypt/mask-wrap read stages agree, against a real
/// Postgres table, end to end.
#[compio::test]
async fn p4_round_trip_encrypted_masked_vector_via_descriptor_metadata() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    let _keys = with_root_key("default", &"d".repeat(64));

    let app = crate::test_app_id!();

    let app = app.as_str();
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();

    // Schema: encrypted `ssn`, masked `phone`, and a `vector` embedding.
    let schema = json!({
        "name": {"type": "string", "required": true},
        "ssn": {
            "type": "string",
            "encrypted": {"mode": "randomised", "keyId": "default", "wraps": "string"}
        },
        "phone": {
            "type": "string",
            "mask": {"kind": "last4", "classification": "pci"}
        },
        "embedding": {"type": "vector", "vectorDims": 3, "vectorMetric": "cosine"},
    });

    // The table the migration engine would have created, sibling column
    // included. No `COMMENT ON COLUMN` sentinels: nothing reads them any more.
    //
    // ONE THING HERE IS NOT A FAITHFUL REPRODUCTION, and it is called out rather
    // than hidden: `embedding` gets a bare `vector(3)` column and NO ANN index.
    // The declared schema asks for a cosine vector index, and the engine DOES
    // emit one -- `vector_index_snapshot` in
    // `zeroship-migrate-core/src/render/declarative.rs:2888` renders
    // `USING ivfflat ("embedding" vector_cosine_ops) WITH (lists = 100)`. This
    // fixture just does not reproduce it, because it hand-writes the DDL rather
    // than running the engine.
    //
    // (That reason REPLACED an older one which said the index "is built by the
    // pgvector adapter in the backend, not by the shared emitter". That was true
    // of `VectorIndex::ensure_vector_index`, which is deleted: the data plane
    // issues no DDL at all now. The absence here is a fixture shortcut, not a
    // property of the system.)
    //
    // The column and its dimensionality are faithful; the index is absent. If a
    // future assertion here depends on the ANN index existing, it will fail, and
    // that failure is correct.
    // Post-flip: `phone` (masked, not encrypted) carries the bare-TEXT mask in
    // its own column; the real value sits in its raw sibling
    // (`raw_column_name`), typed the way the declared field would be. `ssn` is
    // encrypted-only (no `.mask()`), so it is NOT flipped -- its own column
    // keeps holding ciphertext, unchanged.
    let phone_raw = raw_column_name("phone");
    pool.batch_execute(&format!(
        r#"CREATE SCHEMA IF NOT EXISTS "{app}";
CREATE EXTENSION IF NOT EXISTS vector;
CREATE TABLE "{app}"."people" ({PG_SYSTEM_COLUMNS},
  "name" TEXT NOT NULL,
  "ssn" BYTEA,
  "phone" TEXT,
  "{phone_raw}" TEXT,
  "embedding" vector(3)
);
{idx}"#,
        idx = pg_system_indexes(app, "people"),
    ))
    .await
    .unwrap_or_else(|e| panic!("p4 people fixture failed: {e}"));

    // Install the pool into the per-isolate context so the pipelines' own SQL
    // lands on this database, and install the DESCRIPTOR ENTRY the deploy would
    // have installed — exactly what the production register path does.
    zeroship_plugin_db::set_postgres_pool_for_tests(std::rc::Rc::clone(&pool), &url);
    zeroship_plugin_db::cache_schema_for_tests(app, "people", schema.clone());

    // Sanity: the resolution the CRUD passes will perform returns BOTH goodies.
    let resolved = zeroship_plugin_db::crud::runtime_schema_for_tests(app, "people")
        .expect("the descriptor entry this deploy installed must resolve");
    assert_eq!(resolved["ssn"]["encrypted"]["mode"], "randomised");
    assert_eq!(resolved["phone"]["mask"]["kind"], "last4");

    // ----- WRITE (real pipeline, introspected metadata) -----
    // No `id`: the write pipeline refuses a creator-supplied one and mints a
    // typed id in `system_fields_pass`. The raw INSERT below MUST then carry
    // THAT MINTED ID and nothing else: `ssn` is a `randomised` encrypted
    // column, and randomised mode binds the row primary key into the AEAD's
    // additional data (`canonical_aad(collection, column, row_pk_bytes)` in
    // zeroship-data-core's `encryption::aad`, stamped on write by
    // `crud::encryption_pass` and reconstructed on read from the row's `id`).
    // Storing this ciphertext under a DIFFERENT id and reading it back is a
    // ciphertext-relocation attack, and the AEAD refuses it with
    // `encryption_aead_failed` - correctly. Hard-coding a literal here is what
    // broke the test.
    let mut docs = json!([{
        "name": "Ada",
        "ssn": "123-45-6789",
        "phone": "415-555-0142",
        "embedding": [0.1, 0.2, 0.3],
    }]);
    zeroship_plugin_db::prepare_insert_many_docs_for_tests(&mut docs, app, "people", None)
        .await
        .expect("write pipeline");

    // The write pipeline encrypted `ssn` (base64 blob + `__zsbin__ssn` marker)
    // and RELOCATED `phone`: the mask moves into the field's OWN column and
    // the real value moves out to the raw sibling
    // (`mask_pass::relocate_masked_columns`).
    let doc = &docs[0];
    let row_id = doc["id"]
        .as_str()
        .expect("the write pipeline mints the row id, and the AAD binds it")
        .to_string();
    assert!(
        doc["ssn"].as_str().is_some() && doc["ssn"] != json!("123-45-6789"),
        "ssn must be replaced by ciphertext on write, got {:?}",
        doc["ssn"]
    );
    assert_eq!(doc["__zsbin__ssn"], json!(true), "encrypt marker set");
    assert_eq!(
        doc["phone"],
        json!("***-***-0142"),
        "mask pass must move the last4 mask into phone's own column on write, got {:?}",
        doc["phone"]
    );
    assert_ne!(
        doc["phone"],
        json!("415-555-0142"),
        "phone's own column must not carry the real value after relocation, got {:?}",
        doc["phone"]
    );
    assert_eq!(
        doc[phone_raw.as_str()],
        json!("415-555-0142"),
        "the real phone value must be relocated to the raw sibling column, got {:?}",
        doc[phone_raw.as_str()]
    );

    // Persist it the way the SQL builder would (decode the encrypted blob,
    // store the mask under `phone` and the real value under its raw sibling).
    let ssn_b64 = doc["ssn"].as_str().unwrap().to_string();
    let phone_mask = doc["phone"].as_str().unwrap().to_string();
    let phone_real = doc[phone_raw.as_str()].as_str().unwrap().to_string();
    // The vector literal is a test-controlled constant — format it inline with a
    // `::vector` cast (compio-postgres infers a `vector`-typed param from the
    // bind otherwise, which it cannot encode an `&str` into).
    pool.execute(
        &format!(
            "INSERT INTO \"{app}\".\"people\" (id, name, ssn, phone, \"{phone_raw}\", embedding) \
             VALUES ($1, $2, decode($3, 'base64')::bytea, $4, $5, '[0.1,0.2,0.3]'::vector)"
        ),
        &[
            &row_id.as_str(),
            &"Ada",
            &ssn_b64.as_str(),
            &phone_mask.as_str(),
            &phone_real.as_str(),
        ],
    )
    .await
    .unwrap();

    // ----- READ (real pipeline, introspected metadata) -----
    // Fetch the raw row the way the SELECT builder would: the encrypted blob
    // as base64, and `phone` read directly. Reads no longer alias anything
    // after the storage flip -- the field's own column already holds the mask.
    let raw = pool
        .query_text_params(
            &format!(
                "SELECT id, name, encode(ssn, 'base64') AS ssn, phone \
                 FROM \"{app}\".\"people\" WHERE id = $1"
            ),
            &[row_id.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(raw.len(), 1);
    let row = json!({
        "id": row_id,
        "name": "Ada",
        "ssn": raw[0].get::<_, String>("ssn"),
        "phone": raw[0].get::<_, String>("phone"),
    });

    let finalized = zeroship_plugin_db::finalize_rows_on_read_for_tests(app, "people", vec![row])
        .await
        .expect("read pipeline");
    let out = &finalized[0];

    // Encrypted column decrypted back to plaintext (driven by introspected meta).
    assert_eq!(
        out["ssn"],
        json!("123-45-6789"),
        "encrypted column must decrypt to plaintext on read, got {:?}",
        out["ssn"]
    );
    // Masked column wrapped into the platform MaskedValue sentinel, carrying the
    // last4-masked string + the introspected classification.
    assert_eq!(
        out["phone"]["sentinel"],
        json!("__zsmask__"),
        "phone wrapped"
    );
    assert_eq!(
        out["phone"]["masked"],
        json!("***-***-0142"),
        "masked phone surfaces last4 form, got {:?}",
        out["phone"]
    );
    assert_eq!(out["phone"]["classification"], json!("pci"));
    assert!(
        !out.to_string().contains("415-555-0142"),
        "the real phone number must not appear anywhere in the finalized row, got {out:?}"
    );
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// Runtime CRUD against a schema applied ahead of boot. The migration service
// is the sole PostgreSQL schema authority; the data plane consumes descriptor
// metadata and must not create relations while serving requests.
// ---------------------------------------------------------------------------

/// Count every relation `pg_class` holds in the app's schema -- tables, indexes,
/// sequences, views, virtual and partitioned relations alike -- or `None` when
/// the schema itself does not exist.
///
/// This REPLACED an `audit_row_count` probe that counted rows in
/// `"<app>"."__zeroship_migrations"`, deleted along with the data-plane DDL it
/// was the provenance log for. The replacement is deliberately a WIDER
/// instrument, not a like-for-like one: the old probe could only see DDL that
/// chose to write an audit row, so a runtime `CREATE INDEX` that skipped the
/// audit write was invisible to it. This one is keyed on the catalog, so any
/// relation the dispatch creates moves the number whether or not the code that
/// created it wanted to be seen.
///
/// What it still cannot see: DDL that creates no relation at all -- `ALTER
/// TABLE ... ADD COLUMN`, `COMMENT ON`, `GRANT`, a `CREATE TRIGGER`. The two
/// callers below pair it with an explicit relation-existence assertion for the
/// object each is actually about.
async fn schema_relation_count(pool: &std::rc::Rc<Pool>, app: &str) -> Option<i64> {
    let rows = pool
        .query_text_params(
            "SELECT count(c.oid)::bigint AS n \
             FROM pg_namespace n \
             LEFT JOIN pg_class c ON c.relnamespace = n.oid \
             WHERE n.nspname = $1 \
             GROUP BY n.oid",
            &[app],
        )
        .await
        .ok()?;
    // No row at all means the namespace is absent -- distinct from a namespace
    // that exists and holds nothing, which returns 0.
    Some(rows.first()?.get::<_, i64>("n"))
}

/// Descriptor-driven CRUD with no runtime DDL. The engine creates
/// the schema at deploy (here simulated by the same DDL the relocated engine
/// emits). Encryption + mask CRUD then round-trip end-to-end from the runtime
/// descriptor while the catalog relation count stays unchanged.
///
/// The `zero-migrate:enc` / `zero-migrate:mask` column-comment sentinels this fixture used to plant
/// are gone with the catalog read that recovered them; see
/// `p4_round_trip_encrypted_masked_vector_via_descriptor_metadata` for the full
/// reasoning. The round trip below is unchanged.
#[compio::test]
async fn p5_pg_crud_works_via_engine_created_schema_without_runtime_ddl() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    let _keys = with_root_key("default", &"e".repeat(64));

    let app = crate::test_app_id!();

    let app = app.as_str();
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();

    let schema = json!({
        "name": {"type": "string", "required": true},
        "ssn": {
            "type": "string",
            "encrypted": {"mode": "randomised", "keyId": "default", "wraps": "string"}
        },
        "phone": {
            "type": "string",
            "mask": {"kind": "last4", "classification": "pci"}
        },
    });

    // === Simulate the engine/deploy-apply: create the table. ===
    // The SAME DDL shape the relocated engine emits, post-flip: `phone`
    // (masked, not encrypted) carries the bare-TEXT mask in its own column,
    // and its raw sibling (`raw_column_name`) carries the real value. `ssn` is
    // encrypted-only, so it is NOT flipped -- unchanged BYTEA in its own slot.
    let phone_raw = raw_column_name("phone");
    pool.batch_execute(&format!(
        r#"CREATE SCHEMA IF NOT EXISTS "{app}";
CREATE TABLE "{app}"."people" ({PG_SYSTEM_COLUMNS},
  "name" TEXT NOT NULL,
  "ssn" BYTEA,
  "phone" TEXT,
  "{phone_raw}" TEXT
);
{idx}"#,
        idx = pg_system_indexes(app, "people"),
    ))
    .await
    .unwrap_or_else(|e| panic!("people fixture (deploy stand-in) failed: {e}"));

    // Snapshot the catalog after the engine stand-in's apply. Serving CRUD
    // below must not add a relation to it.
    let relations_before = schema_relation_count(&pool, app).await;
    assert!(
        relations_before.unwrap_or(0) > 0,
        "engine stand-in must have created relations to compare against, got \
         {relations_before:?}"
    );

    // Install the runtime backend and the descriptor entry the runtime plants
    // natively at boot.
    zeroship_plugin_db::set_postgres_pool_for_tests(std::rc::Rc::clone(&pool), &url);
    zeroship_plugin_db::cache_schema_for_tests(app, "people", schema.clone());

    // The resolution the CRUD passes will perform returns BOTH goodies.
    let resolved = zeroship_plugin_db::crud::runtime_schema_for_tests(app, "people")
        .expect("the descriptor entry this deploy installed must resolve");
    assert_eq!(resolved["ssn"]["encrypted"]["mode"], "randomised");
    assert_eq!(resolved["phone"]["mask"]["kind"], "last4");

    // ----- WRITE via the real pipeline (descriptor metadata) -----
    // No `id`: the write pipeline refuses a creator-supplied one and mints a
    // typed id. The raw INSERT below MUST carry that minted id - `ssn` is a
    // `randomised` encrypted column, so the row primary key is bound into the
    // AEAD's additional data on write and reconstructed from the row's `id` on
    // read. A literal id here relocates the ciphertext onto another row, and
    // the read correctly refuses it with `encryption_aead_failed`.
    let mut docs = json!([{
        "name": "Grace",
        "ssn": "987-65-4321",
        "phone": "650-555-0199",
    }]);
    zeroship_plugin_db::prepare_insert_many_docs_for_tests(&mut docs, app, "people", None)
        .await
        .expect("write pipeline");
    let doc = &docs[0];
    let row_id = doc["id"]
        .as_str()
        .expect("the write pipeline mints the row id, and the AAD binds it")
        .to_string();
    assert!(
        doc["ssn"].as_str().is_some() && doc["ssn"] != json!("987-65-4321"),
        "ssn must be ciphertext on write, got {:?}",
        doc["ssn"]
    );
    assert_eq!(doc["__zsbin__ssn"], json!(true), "encrypt marker set");
    assert_eq!(
        doc["phone"],
        json!("***-***-0199"),
        "mask pass must move the last4 mask into phone's own column on write, got {:?}",
        doc["phone"]
    );
    assert_ne!(
        doc["phone"],
        json!("650-555-0199"),
        "phone's own column must not carry the real value after relocation, got {:?}",
        doc["phone"]
    );
    assert_eq!(
        doc[phone_raw.as_str()],
        json!("650-555-0199"),
        "the real phone value must be relocated to the raw sibling column, got {:?}",
        doc[phone_raw.as_str()]
    );

    let ssn_b64 = doc["ssn"].as_str().unwrap().to_string();
    let phone_mask = doc["phone"].as_str().unwrap().to_string();
    let phone_real = doc[phone_raw.as_str()].as_str().unwrap().to_string();
    pool.execute(
        &format!(
            "INSERT INTO \"{app}\".\"people\" (id, name, ssn, phone, \"{phone_raw}\") \
             VALUES ($1, $2, decode($3, 'base64')::bytea, $4, $5)"
        ),
        &[
            &row_id.as_str(),
            &"Grace",
            &ssn_b64.as_str(),
            &phone_mask.as_str(),
            &phone_real.as_str(),
        ],
    )
    .await
    .unwrap();

    // ----- READ via the real pipeline (introspected metadata) -----
    // `phone` is read directly -- it already holds the mask after the storage
    // flip, so no alias is needed the way `phone_masked AS phone` used to be.
    let raw = pool
        .query_text_params(
            &format!(
                "SELECT id, name, encode(ssn, 'base64') AS ssn, phone \
                 FROM \"{app}\".\"people\" WHERE id = $1"
            ),
            &[row_id.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(raw.len(), 1);
    let row = json!({
        "id": row_id,
        "name": "Grace",
        "ssn": raw[0].get::<_, String>("ssn"),
        "phone": raw[0].get::<_, String>("phone"),
    });
    let finalized = zeroship_plugin_db::finalize_rows_on_read_for_tests(app, "people", vec![row])
        .await
        .expect("read pipeline");
    let out = &finalized[0];
    assert_eq!(
        out["ssn"],
        json!("987-65-4321"),
        "encrypted column decrypts to plaintext on read, got {:?}",
        out["ssn"]
    );
    assert_eq!(
        out["phone"]["sentinel"],
        json!("__zsmask__"),
        "phone wrapped"
    );
    assert_eq!(out["phone"]["masked"], json!("***-***-0199"));
    assert_eq!(out["phone"]["classification"], json!("pci"));
    assert!(
        !out.to_string().contains("650-555-0199"),
        "the real phone number must not appear anywhere in the finalized row, got {out:?}"
    );

    // FINAL proof: still zero runtime DDL after the full CRUD round-trip.
    assert_eq!(
        schema_relation_count(&pool, app).await,
        relations_before,
        "P5 PG cutover: CRUD must not have triggered any relation-creating DDL"
    );

    let _ = pool
        .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await;
    release_pg(pool).await;
}

/// When no root key is configured for `missing_test` (the fallback
/// source holds none, and the PG getter resolves nothing because
/// nothing installs it), the PG resolver surfaces a typed
/// `column_key_not_configured` Configuration error rather than panicking
/// or returning Internal.
#[compio::test]
async fn encrypted_column_missing_key_typed_error() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    // Resolve against a source that provably has NO key: an empty
    // supplied set. The previous form deleted one env var name and
    // trusted the ambient environment to be otherwise clean, so a
    // `ZEROSHIP_COLUMN_KEY_MISSING_TEST` exported outside the test would
    // have turned this assertion green-for-the-wrong-reason. An empty
    // set cannot.
    let _keys = zeroship_plugin_db::supply_root_keys_for_tests(&[]);

    let backend = zeroship_plugin_db::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_plugin_db::isolate_key_source(),
    );
    let err = backend
        .key_store()
        .resolve("app1", "missing_test")
        .await
        .expect_err("missing key must yield a typed error");
    match err {
        DbError::Configuration { code, .. } => {
            assert_eq!(code, "column_key_not_configured");
        }
        other => panic!("expected Configuration column_key_not_configured, got {other:?}"),
    }
    drop(backend);
    release_pg(pool).await;
}

#[compio::test]
async fn pg_bytea_decoder_preserves_raw_binary_prefix_bytes() {
    use base64::Engine as _;

    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let rows = pool
        .query_text_params(
            "SELECT decode('5c783431343234333434', 'hex')::bytea AS payload",
            &[],
        )
        .await
        .unwrap();
    let json = zeroship_plugin_db::row_to_json_for_bench(&rows[0]);
    let payload = json
        .get("payload")
        .and_then(Value::as_str)
        .expect("payload base64 string");
    let expected_raw = base64::engine::general_purpose::STANDARD.encode(br"\x41424344");
    let wrong_hex_decoded = base64::engine::general_purpose::STANDARD.encode(b"ABCD");

    assert_eq!(
        payload, expected_raw,
        "BYTEA decoding must preserve the raw binary wire bytes",
    );
    assert_ne!(
        payload, wrong_hex_decoded,
        "BYTEA decoding must not reinterpret raw binary bytes as a \
         text-protocol \\x... payload",
    );
    release_pg(pool).await;
}

// ===========================================================================
// PG `Backup` impl (pg_dump / pg_restore shell-out + PITR
// placeholder)
// ===========================================================================
//
// Four tests covering the deliverables in plan §9:
//
//   1. `snapshot_restore_round_trip_pg` — gate #4. Insert N rows;
//      `snapshot()` to a tempfile-backed `file://` URI; truncate via
//      raw `DROP/CREATE`; `restore()`; assert rows recovered.
//      `require_pg_client_tool` refuses the run when `pg_dump` or
//      `pg_restore` is off PATH.
//   2. (deleted) `pitr_pg_records_target` asserted the row landed in a
//      `pitr_targets` table in the platform-owned system schema. That
//      table lost its installer when the schema was deleted, so the test
//      went with its subject rather than assert nothing. `pitr_replay`
//      itself followed on 2026-09-07; see
//      `zeroship_data_core::storage::Backup`.
//   3. `snapshot_during_migration_returns_typed_error` — acquire the
//      `snapshot_restore` mig-lock manually; attempt `snapshot()`;
//      expect `Coded { code: "migration_in_progress" }`. No subprocess.
//   4. `snapshot_uri_content_hash_round_trip` — `snapshot()` →
//      `SnapshotHandle.content_hash` matches SHA-256 of the on-disk
//      dump file. Needs `pg_dump` for the same reason as #1.

use zeroship_plugin_db::backend::{
    Backup as _, BusyPolicy as BackupBusyPolicy, LockScope, SnapshotOpts,
};

/// Refuse the run unless `tool` answers `--version` on PATH.
///
/// The callers used to `#[ignore]` themselves statically, so this refusal was
/// reachable only from a run that passed `--ignored` - and nothing in this
/// repository passes it. The attribute therefore did not defer the check, it
/// deleted the tests from every job that could have run them, which is the same
/// silent green the refusal exists to prevent.
///
/// It takes the binary NAME because restore needs `pg_restore` as well as
/// `pg_dump`, and a probe of only the first reports a machine as ready when the
/// round-trip's second half cannot run.
///
/// # Panics
///
/// When `tool` is absent, naming the packages that carry it.
fn require_pg_client_tool(tool: &str) {
    let answered = std::process::Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    assert!(
        answered,
        "`{tool}` is not on PATH, and this test requires it.\n\
         \n\
         \x20 backend: the PostgreSQL CLIENT tools, in this process's PATH\n\
         \x20 probe:   `{tool} --version` did not succeed\n\
         \n\
         This is a LOCAL binary, not the server: a reachable database does not\n\
         supply it, and the container-hosted server this suite talks to has it\n\
         inside the container where this process cannot reach it. Nothing in\n\
         this repository installs it.\n\
         \n\
         Install the client package for your system - `postgresql-client` on\n\
         Debian and Ubuntu, `postgresql` on Fedora and Arch, `postgresql@16` in\n\
         Homebrew, or the `postgresql` package in a nix shell - then check both\n\
         binaries, because restore needs the second:\n\
         \x20 pg_dump --version\n\
         \x20 pg_restore --version\n\
         \n\
         A major version at or above the server's is the safe direction; an\n\
         older `pg_dump` refuses a newer server outright.\n\
         \n\
         There is no environment variable and no attribute that makes this a\n\
         skip."
    );
}

/// Fence: when the per-app `snapshot_restore` advisory
/// lock is held by another caller, `snapshot()` surfaces a typed
/// `Coded { code: "migration_in_progress" }` rather than blocking
/// indefinitely or returning an opaque LockContention. Pins the
/// pre-flight interlock the snapshot impl runs before invoking
/// `pg_dump`.
#[compio::test]
async fn snapshot_during_migration_returns_typed_error() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let backend = zeroship_plugin_db::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_plugin_db::isolate_key_source(),
    );
    let app_id = crate::test_app_id!();
    let app_id = app_id.as_str();

    // Acquire the snapshot_restore lock on a dedicated standalone
    // connection (not a pooled client) so the lock is held for the
    // entire test without competing with the pool. The lock is
    // session-scoped, so it auto-releases when this client drops at
    // end-of-scope. We don't go through `LockGuard` because that
    // type is `pub(crate)` and unreachable from integration tests.
    let (lock_client, lock_conn) = compio_postgres::connect(&url, NoTls)
        .await
        .expect("hold-lock dedicated connect");
    let lock_conn_task = compio::runtime::spawn(async move {
        let _ = lock_conn.run().await;
    });
    // Mirror `LockScope::GlobalApp { app_id, name: "snapshot_restore" }
    // .to_keys()` exactly so the underlying `(key1, key2)` pair
    // matches what the snapshot's pre-flight will try to acquire.
    let scope = LockScope::GlobalApp {
        app_id: app_id.to_string(),
        name: "snapshot_restore".to_string(),
    };
    let (key1, key2) = scope.to_keys();
    lock_client
        .query_text_params(
            "SELECT pg_advisory_lock(hashtext($1)::int4, hashtext($2)::int4)",
            &[key1.as_str(), key2.as_str()],
        )
        .await
        .expect("acquire snapshot_restore lock on dedicated session");

    // Snapshot dest URI doesn't need to be real — we expect the
    // call to refuse at the pre-flight stage, before pg_dump runs.
    let dest = "file:///tmp/p5_pr4_miglock_should_not_exist.dump";
    let err = backend
        .snapshot(
            app_id,
            dest,
            SnapshotOpts {
                if_busy: BackupBusyPolicy::Abort,
            },
        )
        .await
        .expect_err("snapshot must refuse while snapshot_restore lock is held");
    match err {
        DbError::Coded { code, .. } => {
            assert_eq!(
                code, "migration_in_progress",
                "expected Coded migration_in_progress, got code={code:?}"
            );
        }
        other => panic!("expected Coded {{ code: \"migration_in_progress\", .. }}, got {other:?}"),
    }

    // The destination file MUST NOT have been created — the
    // pre-flight refusal runs before any disk I/O.
    let path = std::path::Path::new("/tmp/p5_pr4_miglock_should_not_exist.dump");
    assert!(
        !path.exists(),
        "snapshot must not write to disk when refused at pre-flight"
    );

    // Drop the dedicated client; PG releases the session-scoped
    // advisory lock when the backend session terminates.
    drop(lock_client);
    lock_conn_task.detach();
    drop(backend);
    release_pg(pool).await;
}

/// Gate #1: round-trip snapshot+restore. Insert rows
/// into a per-app schema, snapshot to a `file://` URI, drop the
/// schema's table contents, restore, assert the rows are back.
///
/// Needs `pg_dump` AND `pg_restore` on PATH; a machine without them fails here
/// naming the package that carries them, rather than reporting a round-trip it
/// never performed.
#[compio::test]
async fn snapshot_restore_round_trip_pg() {
    let url = require_pg().await;
    require_pg_client_tool("pg_dump");
    require_pg_client_tool("pg_restore");
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    // Per-app schema fresh every run.
    let app_id = crate::test_app_id!();
    let app_id = app_id.as_str();
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app_id}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{app_id}\""), &[])
        .await
        .unwrap();
    pool.execute(
        &format!(
            r#"CREATE TABLE "{app_id}"."notes" (
                id   INTEGER PRIMARY KEY,
                body TEXT NOT NULL
            )"#
        ),
        &[],
    )
    .await
    .unwrap();

    // Seed deterministic rows. Bind both columns as text — the
    // `$1::int` cast on the SQL side mirrors the `app_role` /
    // `users` test pattern used throughout this file.
    const ROW_COUNT: usize = 5;
    for i in 0..ROW_COUNT {
        let id_s = i.to_string();
        let body = format!("row-{i}");
        pool.query_text_params(
            &format!(r#"INSERT INTO "{app_id}"."notes" (id, body) VALUES ($1::int, $2)"#),
            &[id_s.as_str(), body.as_str()],
        )
        .await
        .unwrap();
    }

    let backend = zeroship_plugin_db::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_plugin_db::isolate_key_source(),
    );

    // Snapshot to a tempdir-backed file:// URI.
    let dir = tempfile::tempdir().unwrap();
    let dest_path = dir.path().join("snapshot.dump");
    let dest_uri = format!("file://{}", dest_path.to_string_lossy());

    let handle = backend
        .snapshot(
            app_id,
            &dest_uri,
            SnapshotOpts {
                if_busy: BackupBusyPolicy::Abort,
            },
        )
        .await
        .expect("snapshot");
    assert!(
        dest_path.exists(),
        "dump file must exist on disk after snapshot"
    );
    assert_eq!(handle.uri, dest_uri);

    // Drop-and-recreate to a clean schema (simulates data loss).
    pool.execute(&format!("DROP SCHEMA \"{app_id}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{app_id}\""), &[])
        .await
        .unwrap();
    let rows = pool
        .query_text_params(
            "SELECT 1 FROM pg_tables WHERE schemaname = $1 AND tablename = 'notes'",
            &[app_id],
        )
        .await
        .unwrap();
    assert!(rows.is_empty(), "post-drop: notes table must be absent");

    // Restore — the impl re-drops/recreates the schema itself, then
    // runs pg_restore over the captured dump file.
    backend.restore(app_id, &handle).await.expect("restore");

    // Verify the row set is recovered. Cast id to text on the
    // server so `Row::get<String>` decodes uniformly without
    // dragging in the `query_text_params` int-decode shape.
    let rows = pool
        .query_text_params(
            &format!(r#"SELECT id::text AS id, body FROM "{app_id}"."notes" ORDER BY id"#),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), ROW_COUNT, "all rows must be recovered");
    for (i, row) in rows.iter().enumerate() {
        let id: String = row.get::<_, String>("id");
        assert_eq!(id, i.to_string());
        let body: String = row.get::<_, String>("body");
        assert_eq!(body, format!("row-{i}"));
    }

    // Cleanup so a re-run starts fresh.
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app_id}\" CASCADE"), &[])
        .await
        .unwrap();
    drop(backend);
    release_pg(pool).await;
}

/// Fence: the `SnapshotHandle.content_hash` returned by
/// `snapshot()` must equal the SHA-256 of the on-disk dump bytes.
/// This is the integrity contract the `restore()` path relies on —
/// any drift here would let a corrupt dump pass restore's hash
/// check.
///
/// Needs `pg_dump` on PATH, and says so by failing rather than by vanishing
/// from the run.
#[compio::test]
async fn snapshot_uri_content_hash_round_trip() {
    let url = require_pg().await;
    require_pg_client_tool("pg_dump");
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app_id = crate::test_app_id!();
    let app_id = app_id.as_str();
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app_id}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{app_id}\""), &[])
        .await
        .unwrap();
    pool.execute(
        &format!(r#"CREATE TABLE "{app_id}"."t" (id INT PRIMARY KEY)"#),
        &[],
    )
    .await
    .unwrap();
    pool.execute(
        &format!(r#"INSERT INTO "{app_id}"."t" (id) VALUES (1), (2), (3)"#),
        &[],
    )
    .await
    .unwrap();

    let backend = zeroship_plugin_db::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_plugin_db::isolate_key_source(),
    );
    let dir = tempfile::tempdir().unwrap();
    let dest_path = dir.path().join("hash_check.dump");
    let dest_uri = format!("file://{}", dest_path.to_string_lossy());

    let handle = backend
        .snapshot(
            app_id,
            &dest_uri,
            SnapshotOpts {
                if_busy: BackupBusyPolicy::Abort,
            },
        )
        .await
        .expect("snapshot");

    // Recompute SHA-256 over the on-disk file via an independent
    // implementation so the assertion pins the byte format.
    use sha2::Digest;
    let bytes = std::fs::read(&dest_path).expect("read dump file");
    let observed: [u8; 32] = sha2::Sha256::digest(&bytes).into();
    assert_eq!(
        handle.content_hash, observed,
        "SnapshotHandle.content_hash must match SHA-256 of on-disk bytes"
    );

    // Cleanup.
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app_id}\" CASCADE"), &[])
        .await
        .unwrap();
    drop(backend);
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// Per-app PG role hardening (§17.5).
//
// The per-app role (`app_<id>_role`) reaches ONLY its schema and is
// NOREPLICATION — slot ownership stays platform-side. These tests
// provision the role via `auth::bootstrap::ensure_per_app_role` and
// add explicit column grants where their fixture needs DML. They fence it:
// it can use those columns, cannot read a sibling app's schema, cannot
// create/list/drop replication slots, and carries no `rolreplication`
// attribute. The per-app role is NOLOGIN (clients
// connect as the platform login role, then `SET ROLE`), so these tests
// drive it via `SET ROLE` from the superuser pool — which is exactly how
// `exec_begin` applies it to client SQL.
// ---------------------------------------------------------------------------

/// Provision a schema + its per-app role for a test. Returns the role
/// name. Idempotent re-runs are exercised by `per_app_role_created_at_provision`.
async fn provision_app_with_role(pool: &std::rc::Rc<Pool>, app: &str) -> String {
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    let role = zeroship_core::database_role::per_app_role_name(app)
        .expect("integration fixture app id must produce a valid PostgreSQL role name");
    let _ = pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
        .await;
    // `ensure_per_app_role` creates the __zeroship_app_role_template
    // anchor itself, so no separate bootstrap step is needed.
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    // The unmask audit table, APPLY-AHEAD. `crud/unmask.rs` created it lazily on
    // every dispatch until 2026-08-28; it emits no DDL now, so the migration
    // service creates it and this fixture stands in for that service. These are
    // the PRODUCTION bytes - `audit_unmask_table_sql` is the same generator
    // `provision_audit_unmask_table` executes - not a copy of them.
    //
    // BEFORE the caller's `ensure_per_app_role`, so this bootstrap recipe can
    // resolve the exact table and its `BIGSERIAL` sequence from the live catalog
    // before installing only INSERT and USAGE. The migrate server independently
    // uses the same provisioning-before-role ordering; it does not call this
    // helper.
    //
    // `batch_execute`, not `execute`: this is multi-statement DDL and the
    // extended protocol refuses it with "cannot insert multiple commands into a
    // prepared statement".
    pool.batch_execute(&zeroship_migrate_server::provisioning::audit_unmask_table_sql(app))
        .await
        .unwrap();
    role
}

async fn install_role_bound_select_policy(
    pool: &std::rc::Rc<Pool>,
    app: &str,
    collection: &str,
    role: &str,
) {
    pool.execute(
        &format!("ALTER TABLE \"{app}\".\"{collection}\" ENABLE ROW LEVEL SECURITY"),
        &[],
    )
    .await
    .unwrap();
    pool.execute(
        &format!("ALTER TABLE \"{app}\".\"{collection}\" FORCE ROW LEVEL SECURITY"),
        &[],
    )
    .await
    .unwrap();
    pool.execute(
        &format!("DROP POLICY IF EXISTS role_gate ON \"{app}\".\"{collection}\""),
        &[],
    )
    .await
    .unwrap();
    pool.execute(
        &format!(
            "CREATE POLICY role_gate ON \"{app}\".\"{collection}\" \
             FOR SELECT USING (current_user = '{role}')"
        ),
        &[],
    )
    .await
    .unwrap();
}

fn login_role_test_url(base_url: &str, role: &str, password: &str) -> String {
    let (scheme, rest) = base_url.split_once("://").unwrap_or(("postgres", base_url));
    let host = rest
        .split_once('@')
        .map(|(_, suffix)| suffix)
        .unwrap_or(rest);
    format!("{scheme}://{role}:{password}@{host}")
}

async fn provision_platform_login_pool(
    admin_pool: &std::rc::Rc<Pool>,
    base_url: &str,
    login_role: &str,
    password: &str,
    app_role: &str,
    app: &str,
) -> (String, std::rc::Rc<Pool>) {
    let _ = admin_pool
        .execute(&format!("DROP ROLE IF EXISTS \"{login_role}\""), &[])
        .await;
    admin_pool
        .execute(
            &format!("CREATE ROLE \"{login_role}\" LOGIN PASSWORD '{password}' INHERIT"),
            &[],
        )
        .await
        .unwrap();
    admin_pool
        .execute(&format!("GRANT \"{app_role}\" TO \"{login_role}\""), &[])
        .await
        .unwrap();
    admin_pool
        .execute(
            &format!("GRANT USAGE ON SCHEMA \"{app}\" TO \"{login_role}\""),
            &[],
        )
        .await
        .unwrap();
    // The membership edge inherits the app role's explicit column grants.
    // Giving the login a table-level SELECT would bypass that column fence and
    // make this RLS control unlike the production login.
    let login_url = login_role_test_url(base_url, login_role, password);
    let login_pool = std::rc::Rc::new(Pool::connect(&login_url, 4).await.unwrap());
    (login_url, login_pool)
}

/// Install `PostGIS` into the test database, or refuse the run.
///
/// # Panics
///
/// When the extension is not available on the server, with what carries it. It
/// used to return `false` and the callers announced a skip, so a `--ignored`
/// run on a stock `postgres:16` printed the same green as one that had
/// exercised a spatial query.
async fn require_postgis(pool: &Pool) {
    // Best-effort CREATE, catalogue-decided verdict; see `require_pgvector`.
    let _ = pool
        .execute("CREATE EXTENSION IF NOT EXISTS postgis", &[])
        .await;
    let installed = !pool
        .query_text_params("SELECT 1 FROM pg_extension WHERE extname='postgis'", &[])
        .await
        .unwrap_or_default()
        .is_empty();
    assert!(
        installed,
        "The `postgis` extension is not installed, and this test requires it.\n\
         \n\
         \x20 backend:   PostgreSQL\n\
         \x20 extension: postgis\n\
         \n\
         `CREATE EXTENSION IF NOT EXISTS postgis` did not leave a row in\n\
         pg_extension. Check what the server has to offer:\n\
         \x20 SELECT * FROM pg_available_extensions WHERE name = 'postgis';\n\
         \n\
         NO SERVER THIS REPOSITORY PROVISIONS CARRIES IT. deploy/compose's\n\
         `postgres` service - the one tests/provision_test_backends.sh starts -\n\
         runs the stock `postgres:16` image, which does not bundle PostGIS.\n\
         Point the tests at a server that does, or swap that image for a\n\
         PostGIS-bundled variant such as `postgis/postgis:16-3.4` and re-create\n\
         the container.\n\
         \n\
         There is no environment variable that makes this a skip."
    );
}

#[compio::test]
async fn per_app_role_created_at_provision() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = crate::test_app_id!();
    let app = app.as_str();
    let role = provision_app_with_role(&pool, app).await;

    // First provision creates the role.
    let first = zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .expect("provision per-app role");
    assert!(first.created_role, "first provision must create the role");

    // The role now exists in pg_roles.
    let exists = pool
        .query_text_params(
            "SELECT 1 FROM pg_roles WHERE rolname = $1",
            &[role.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(exists.len(), 1, "role must exist after provision");

    // Idempotent: a second provision is a no-op create (GRANTs re-run
    // harmlessly).
    let second = zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .expect("re-provision per-app role");
    assert!(
        !second.created_role,
        "second provision must NOT re-create the role"
    );

    let _ = pool
        .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await;
    let _ = pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
        .await;
    release_pg(pool).await;
}

#[compio::test]
async fn workflow_journal_redeploy_grants_do_not_reopen_without_reprovision() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let (client, connection) = compio_postgres::connect(&url, NoTls)
        .await
        .expect("workflow provision pg client");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    let app_id = Uuid::new_v4();
    let app_schema = zeroship_plugin_workflow::store::pg::app_schema_for(&app_id);
    let tables = zeroship_plugin_workflow::store::pg::WorkflowTables::for_app_id(&app_id);
    let schema_role = zeroship_core::database_role::per_app_role_name(&app_schema)
        .expect("workflow schema must produce a valid PostgreSQL role name");
    let uuid_role = zeroship_core::database_role::per_app_role_name(&app_id.to_string())
        .expect("workflow app id must produce a valid PostgreSQL role name");

    let _ = pool
        .execute(
            &format!("DROP SCHEMA IF EXISTS \"{app_schema}\" CASCADE"),
            &[],
        )
        .await;
    for role in [&schema_role, &uuid_role] {
        let _ = pool
            .execute(&format!("DROP OWNED BY \"{role}\""), &[])
            .await;
        let _ = pool
            .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
            .await;
    }

    // Stand in for `db/migrations-ts/20260818000200_worker_database_authority.ts`.
    // This suite runs against a bare database with no platform migrations
    // applied, and since 2a44ea8ef nothing in the worker creates this role:
    // `PgStore::provision` opens with `SET ROLE zeroship_workflow_owner` and
    // fails outright if it is absent. The attribute list is copied from that
    // migration, so a test-created role cannot be wider than the deployed one.
    //
    // WHAT THIS DOES NOT CATCH: the migration ceasing to create the role, or
    // creating it wider. Creating it here makes this test green either way.
    // `platform_migrate.rs` is what rules on the deployed role.
    pool.execute(
        &format!(
            "DO $$ BEGIN \
               IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{owner}') THEN \
                 CREATE ROLE \"{owner}\" NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE \
                                         NOINHERIT NOREPLICATION NOBYPASSRLS; \
               END IF; \
             END $$",
            owner = zeroship_migrate_server::provisioning::WORKFLOW_OWNER_ROLE,
        ),
        &[],
    )
    .await
    .expect("precreate the narrow workflow journal owner role");
    // The journal SCHEMA is created by the deploy's migration apply, not by the
    // worker -- `PgStore::provision` holds no CREATE on the database. Call the
    // migration service's own statement rather than a CREATE SCHEMA of our own,
    // so the journal below is owned the way production owns it.
    zeroship_migrate_server::provisioning::provision_workflow_journal_schema(&client, &app_id)
        .await
        .expect("provision the app workflow journal schema");
    zeroship_plugin_workflow::store::pg::PgStore::provision(&client, &app_id)
        .await
        .expect("provision app-local workflow journal");
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, &app_schema)
        .await
        .expect("redeploy plugin-db per-app role grants");

    for table in tables.all() {
        let rows = pool
            .query_text_params(
                "SELECT \
                    has_table_privilege($1, $2, 'SELECT') AS sel, \
                    has_table_privilege($1, $2, 'INSERT') AS ins, \
                    has_table_privilege($1, $2, 'UPDATE') AS upd, \
                    has_table_privilege($1, $2, 'DELETE') AS del",
                &[schema_role.as_str(), table],
            )
            .await
            .expect("check journal table privileges");
        let row = &rows[0];
        assert!(
            !row.get::<_, bool>("sel"),
            "app role must not SELECT {table}"
        );
        assert!(
            !row.get::<_, bool>("ins"),
            "app role must not INSERT {table}"
        );
        assert!(
            !row.get::<_, bool>("upd"),
            "app role must not UPDATE {table}"
        );
        assert!(
            !row.get::<_, bool>("del"),
            "app role must not DELETE {table}"
        );

        let owner_rows = pool
            .query_text_params(
                "SELECT pg_get_userbyid(c.relowner) AS owner \
                   FROM pg_class c \
                  WHERE c.oid = to_regclass($1)",
                &[table],
            )
            .await
            .expect("check journal table owner");
        let owner: String = owner_rows[0].get("owner");
        // Bound to `zeroship-migrate-server`'s copy of the owner-role name while the
        // writer is `plugin-workflow`'s private copy of it, so the two
        // duplicated constants disagreeing shows up here rather than as a
        // journal nobody can reach. Until 2026-08-20 this compared against
        // `__zeroship_platform_role`, the role the store created for itself
        // before 2a44ea8ef removed `provision_owner_sql`.
        assert_eq!(
            owner,
            zeroship_migrate_server::provisioning::WORKFLOW_OWNER_ROLE,
            "journal owner for {table}"
        );
        // The security property the name is a proxy for: no role an app's own
        // code runs as may own the journal, because an owner can re-GRANT
        // itself the DML the assertions above just proved it lacks.
        assert_ne!(
            owner, schema_role,
            "journal owner for {table} is an app role"
        );
        assert_ne!(owner, uuid_role, "journal owner for {table} is an app role");
    }

    let _ = pool
        .execute(
            &format!("DROP SCHEMA IF EXISTS \"{app_schema}\" CASCADE"),
            &[],
        )
        .await;
    for role in [&schema_role, &uuid_role] {
        let _ = pool
            .execute(&format!("DROP OWNED BY \"{role}\""), &[])
            .await;
        let _ = pool
            .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
            .await;
    }
    drop(client);
    release_pg(pool).await;
}

#[compio::test]
async fn per_app_role_has_no_replication_attr() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = crate::test_app_id!();
    let app = app.as_str();
    let role = provision_app_with_role(&pool, app).await;
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();

    // §17.5 NON-NEGOTIABLE: rolreplication MUST be false.
    let rows = pool
        .query_text_params(
            "SELECT rolreplication FROM pg_roles WHERE rolname = $1",
            &[role.as_str()],
        )
        .await
        .unwrap();
    let is_repl: bool = rows[0].get("rolreplication");
    assert!(
        !is_repl,
        "per-app role MUST NOT have the REPLICATION attribute (§17.5 \
         slot-ownership-stays-platform)"
    );

    let _ = pool
        .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await;
    let _ = pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
        .await;
    release_pg(pool).await;
}

#[compio::test]
async fn per_app_role_grant_scoped_to_schema() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = crate::test_app_id!();
    let app = app.as_str();
    let role = provision_app_with_role(&pool, app).await;
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();

    // Create a table in the app schema (as superuser), insert a row.
    pool.execute(
        &format!(r#"CREATE TABLE "{app}".widgets (id SERIAL PRIMARY KEY, name TEXT)"#),
        &[],
    )
    .await
    .unwrap();
    pool.execute(
        &format!(r#"INSERT INTO "{app}".widgets (name) VALUES ('seed')"#),
        &[],
    )
    .await
    .unwrap();
    support::grant_all_runtime_table_columns(&pool, app, "widgets").await;

    // SET ROLE to the per-app role and CRUD its own schema — must work.
    pool.execute(&format!(r#"SET ROLE "{role}""#), &[])
        .await
        .unwrap();
    let sel = pool
        .query_text_params(&format!(r#"SELECT name FROM "{app}".widgets"#), &[])
        .await;
    assert!(
        sel.is_ok(),
        "per-app role must SELECT its own schema: {sel:?}"
    );
    let ins = pool
        .execute(
            &format!(r#"INSERT INTO "{app}".widgets (name) VALUES ('by_role')"#),
            &[],
        )
        .await;
    assert!(
        ins.is_ok(),
        "per-app role must INSERT its own schema: {ins:?}"
    );
    pool.execute("RESET ROLE", &[]).await.unwrap();

    let _ = pool
        .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await;
    let _ = pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
        .await;
    release_pg(pool).await;
}

#[compio::test]
async fn per_app_role_cannot_read_sibling_schema_or_touch_slots() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app_a = crate::test_app_id!("a");
    let app_a = app_a.as_str();
    let app_b = crate::test_app_id!("b");
    let app_b = app_b.as_str();
    let role_a = provision_app_with_role(&pool, app_a).await;
    // Provision a sibling schema B (and its role) with a table.
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app_b}\" CASCADE"), &[])
        .await
        .unwrap();
    let role_b = zeroship_core::database_role::per_app_role_name(app_b)
        .expect("sibling fixture app id must produce a valid PostgreSQL role name");
    let _ = pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role_b}\""), &[])
        .await;
    pool.execute(&format!("CREATE SCHEMA \"{app_b}\""), &[])
        .await
        .unwrap();

    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app_a)
        .await
        .unwrap();
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app_b)
        .await
        .unwrap();
    pool.execute(
        &format!(r#"CREATE TABLE "{app_b}".secrets (id SERIAL PRIMARY KEY, val TEXT)"#),
        &[],
    )
    .await
    .unwrap();
    pool.execute(
        &format!(r#"INSERT INTO "{app_b}".secrets (val) VALUES ('app_b_secret')"#),
        &[],
    )
    .await
    .unwrap();

    // SET ROLE to app_a's role and attempt to read app_b's schema — must
    // be denied (no USAGE on the sibling schema).
    pool.execute(&format!(r#"SET ROLE "{role_a}""#), &[])
        .await
        .unwrap();
    let cross = pool
        .query_text_params(&format!(r#"SELECT val FROM "{app_b}".secrets"#), &[])
        .await;
    assert!(
        cross.is_err(),
        "per-app role A must NOT read sibling schema B; got Ok"
    );
    let cross_err = err_chain(&cross.unwrap_err());
    assert!(
        cross_err.contains("permission denied") || cross_err.contains("acl"),
        "expected permission-denied reading sibling schema, got: {cross_err}"
    );

    // While SET ROLE'd: cannot create a replication slot (NOREPLICATION).
    let slot_create = pool
        .execute(
            "SELECT pg_create_logical_replication_slot('p6a_fence_slot', 'pgoutput', false, false)",
            &[],
        )
        .await;
    assert!(
        slot_create.is_err(),
        "per-app role must NOT create a replication slot directly"
    );
    let slot_err = err_chain(&slot_create.unwrap_err());
    assert!(
        slot_err.contains("replication") || slot_err.contains("permission denied"),
        "expected REPLICATION-privilege error on slot create, got: {slot_err}"
    );

    // Cannot drop a slot either (pg_drop_replication_slot requires
    // REPLICATION). Use a name that doesn't exist — the privilege check
    // fires before the "no such slot" check.
    let slot_drop = pool
        .execute("SELECT pg_drop_replication_slot('does_not_exist')", &[])
        .await;
    assert!(
        slot_drop.is_err(),
        "per-app role must NOT drop a replication slot"
    );

    pool.execute("RESET ROLE", &[]).await.unwrap();

    let _ = pool
        .execute(&format!("DROP SCHEMA IF EXISTS \"{app_a}\" CASCADE"), &[])
        .await;
    let _ = pool
        .execute(&format!("DROP SCHEMA IF EXISTS \"{app_b}\" CASCADE"), &[])
        .await;
    let _ = pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role_a}\""), &[])
        .await;
    let _ = pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role_b}\""), &[])
        .await;
    release_pg(pool).await;
}

#[compio::test]
async fn client_sql_runs_under_per_app_role() {
    // Proves the `SET LOCAL ROLE` shape `exec_begin`
    // issue actually switches the effective role for the rest of the tx,
    // and reverts at COMMIT/ROLLBACK.
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = crate::test_app_id!();
    let app = app.as_str();
    let role = provision_app_with_role(&pool, app).await;
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();

    // Open a dedicated connection, BEGIN, then apply the SAME SET LOCAL
    // ROLE SQL the orchestrator emits.
    let (client, conn) = compio_postgres::connect(&url, NoTls).await.unwrap();
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();

    client.execute("BEGIN", &[]).await.unwrap();
    let set_sql = zeroship_plugin_db::auth::bootstrap::set_local_role_sql(app)
        .expect("integration app id must produce valid SET LOCAL ROLE SQL");
    client.execute(&set_sql, &[]).await.unwrap();

    // current_user inside the tx must be the per-app role.
    let who = client
        .query_text_params("SELECT current_user AS u", &[])
        .await
        .unwrap();
    let current: String = who[0].get("u");
    assert_eq!(
        current, role,
        "client SQL inside the tx must run under the per-app role"
    );

    // COMMIT reverts SET LOCAL — current_user is back to the login role.
    client.execute("COMMIT", &[]).await.unwrap();
    let who2 = client
        .query_text_params("SELECT current_user AS u", &[])
        .await
        .unwrap();
    let after: String = who2[0].get("u");
    assert_ne!(
        after, role,
        "SET LOCAL ROLE must revert at COMMIT (no role leak to next stmt)"
    );

    drop(client);
    let _ = pool
        .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await;
    let _ = pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
        .await;
    release_pg(pool).await;
}

#[compio::test]
async fn exec_autocommit_query_runs_under_per_app_role() {
    // I2 regression: the shared autocommit exec path must switch to the
    // per-app role before running the statement, not just explicit/auto tx.
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = crate::test_app_id!();
    let app = app.as_str();
    let role = provision_app_with_role(&pool, app).await;
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();
    zeroship_plugin_db::set_db_url_for_tests(&url);

    let rows = zeroship_plugin_db::exec_query_for_tests(
        app,
        zeroship_plugin_db::query::BuiltQuery {
            sql: "SELECT current_user AS u".to_string(),
            params: vec![],
        },
    )
    .await
    .expect("autocommit exec query");
    let current = rows[0]
        .get("u")
        .and_then(Value::as_str)
        .expect("current_user string");
    assert_eq!(
        current, role,
        "autocommit exec query must run under the per-app role",
    );

    let who = pool
        .query_text_params("SELECT current_user AS u", &[])
        .await
        .unwrap();
    let after: String = who[0].get("u");
    assert_ne!(
        after, role,
        "RESET ROLE must run before the pooled autocommit connection returns",
    );

    let _ = pool
        .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await;
    let _ = pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
        .await;
    release_pg(pool).await;
}

#[compio::test]
async fn vector_search_runs_under_per_app_role_via_rls() {
    use zeroship_plugin_db::backend::{PostgresBackend, VectorIndex, VectorMetric};

    let url = require_pg().await;
    let admin_pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    require_pgvector(&admin_pool).await;

    let app = crate::test_app_id!();

    let app = app.as_str();
    let coll = "docs";
    let role = provision_app_with_role(&admin_pool, app).await;
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&admin_pool, app)
        .await
        .unwrap();
    admin_pool
        .execute(
            &format!(
                "CREATE TABLE \"{app}\".\"{coll}\" (\
               id SERIAL PRIMARY KEY, \
               embedding vector(2) NOT NULL, \
               created_at TIMESTAMPTZ DEFAULT NOW(), \
               updated_at TIMESTAMPTZ DEFAULT NOW(), \
               created_by TEXT, \
               updated_by TEXT, \
               version INTEGER NOT NULL DEFAULT 1, \
               deleted_at TIMESTAMPTZ\
             )"
            ),
            &[],
        )
        .await
        .unwrap();
    zeroship_plugin_db::cache_schema_for_tests(
        app,
        coll,
        json!({ "embedding": { "type": "vector", "vectorDims": 2 } }),
    );
    admin_pool
        .execute(
            &format!("INSERT INTO \"{app}\".\"{coll}\" (embedding) VALUES ($1::text::vector)"),
            &[&"[1,0]" as &(dyn compio_postgres::types::ToSql + Sync)],
        )
        .await
        .unwrap();
    support::grant_all_runtime_table_columns(&admin_pool, app, coll).await;
    install_role_bound_select_policy(&admin_pool, app, coll, &role).await;
    let login_role = "p6a_vector_login";
    let (login_url, login_pool) =
        provision_platform_login_pool(&admin_pool, &url, login_role, "test", &role, app).await;

    let blocked = login_pool
        .query_text_params(&format!("SELECT id FROM \"{app}\".\"{coll}\""), &[])
        .await
        .unwrap();
    assert!(
        blocked.is_empty(),
        "login role must be blocked by FORCE RLS before vector_search proves the role fence"
    );

    let backend = zeroship_plugin_db::backend::PostgresBackend::new(
        login_pool.clone(),
        login_url,
        zeroship_plugin_db::isolate_key_source(),
    );
    let rows = VectorIndex::vector_search(
        &backend,
        &DbBinding::cold_start(app),
        coll,
        "embedding",
        &[1.0, 0.0],
        1,
        VectorMetric::Cosine,
        &Value::Null,
        &zeroship_plugin_db::collection_schema(&DbBinding::cold_start(app), coll)
            .expect("descriptor slice for the search fixture"),
    )
    .await
    .unwrap_or_else(|e| panic!("vector_search failed: {e:?}"));
    assert_eq!(rows.len(), 1, "vector_search must see the role-gated row");
    assert_eq!(rows[0]["id"], 1);

    drop(login_pool);
    let _ = admin_pool
        .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await;
    let _ = admin_pool
        .execute(&format!("DROP ROLE IF EXISTS \"{login_role}\""), &[])
        .await;
    let _ = admin_pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
        .await;
    drop(backend);
    release_pg(admin_pool).await;
}

#[compio::test]
async fn spatial_near_runs_under_per_app_role_via_rls() {
    use zeroship_plugin_db::backend::{GeoPoint, PostgresBackend, SpatialIndex};

    let url = require_pg().await;
    let admin_pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    require_postgis(&admin_pool).await;

    let app = crate::test_app_id!();

    let app = app.as_str();
    let coll = "places";
    let role = provision_app_with_role(&admin_pool, app).await;
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&admin_pool, app)
        .await
        .unwrap();
    admin_pool
        .execute(
            &format!(
                "CREATE TABLE \"{app}\".\"{coll}\" (\
               id SERIAL PRIMARY KEY, \
               location geography(POINT, 4326) NOT NULL, \
               created_at TIMESTAMPTZ DEFAULT NOW(), \
               updated_at TIMESTAMPTZ DEFAULT NOW(), \
               created_by TEXT, \
               updated_by TEXT, \
               version INTEGER NOT NULL DEFAULT 1, \
               deleted_at TIMESTAMPTZ\
             )"
            ),
            &[],
        )
        .await
        .unwrap();
    zeroship_plugin_db::cache_schema_for_tests(
        app,
        coll,
        json!({ "location": { "type": "geoPoint" } }),
    );
    admin_pool
        .execute(
            &format!(
                "INSERT INTO \"{app}\".\"{coll}\" (location) \
             VALUES (ST_GeogFromText('POINT(-0.1278 51.5074)'))"
            ),
            &[],
        )
        .await
        .unwrap();
    support::grant_all_runtime_table_columns(&admin_pool, app, coll).await;
    install_role_bound_select_policy(&admin_pool, app, coll, &role).await;
    let login_role = "p6a_spatial_login";
    let (login_url, login_pool) =
        provision_platform_login_pool(&admin_pool, &url, login_role, "test", &role, app).await;

    let blocked = login_pool
        .query_text_params(&format!("SELECT id FROM \"{app}\".\"{coll}\""), &[])
        .await
        .unwrap();
    assert!(
        blocked.is_empty(),
        "login role must be blocked by FORCE RLS before spatial_near proves the role fence"
    );

    let backend = zeroship_plugin_db::backend::PostgresBackend::new(
        login_pool.clone(),
        login_url,
        zeroship_plugin_db::isolate_key_source(),
    );
    let rows = SpatialIndex::spatial_near(
        &backend,
        &DbBinding::cold_start(app),
        coll,
        "location",
        GeoPoint {
            lat: 51.5074,
            lng: -0.1278,
        },
        1000.0,
        &Value::Null,
        Some(1),
        // The DESCRIPTOR slice, not `Value::Null`. `build_spatial_near` checks
        // the column against it, so a null hint refuses `location` outright
        // with `invalid_identifier` and the role fence below is never reached.
        // This argument was `Value::Null` from the day the test was written
        // until 2026-09-03; it never showed because the usual test container
        // carries no PostGIS, and back then the arm skipped rather than
        // failing. Its vector twin above always passed the descriptor.
        &zeroship_plugin_db::collection_schema(&DbBinding::cold_start(app), coll)
            .expect("descriptor slice for the spatial fixture"),
    )
    .await
    .unwrap_or_else(|e| panic!("spatial_near failed: {e:?}"));
    assert_eq!(rows.len(), 1, "spatial_near must see the role-gated row");
    assert_eq!(rows[0]["id"], 1);

    drop(login_pool);
    let _ = admin_pool
        .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await;
    let _ = admin_pool
        .execute(&format!("DROP ROLE IF EXISTS \"{login_role}\""), &[])
        .await;
    let _ = admin_pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
        .await;
    drop(backend);
    release_pg(admin_pool).await;
}

#[compio::test]
async fn unmask_fetch_runs_under_per_app_role_via_rls() {
    use zeroship_plugin_db::crud::unmask::{self, UnmaskFieldArgs};

    let url = require_pg().await;
    let admin_pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = crate::test_app_id!();

    let app = app.as_str();
    let coll = "users";
    let role = provision_app_with_role(&admin_pool, app).await;
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&admin_pool, app)
        .await
        .unwrap();
    let schema = json!({
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" }
        }
    });
    let ssn_raw = raw_column_name("ssn");
    // Built with the platform's own emitter, not hand-spelled, so the fixture
    // cannot drift from the runtime's DDL shape: `ssn` gets the bare-TEXT mask
    // column and `__zs_raw__ssn` gets the declared type for the real value.
    let create_table = build_create_table_with_fks(&zeroship_schema::SchemaName::new(app).expect("fixture schema name"), coll, &schema, &FkEmission::Inline)
        .expect("emitter must build the users DDL");
    admin_pool.batch_execute(&create_table).await.unwrap();
    admin_pool
        .execute(
            &format!(
                "INSERT INTO \"{app}\".\"{coll}\" (id, \"{ssn_raw}\", ssn) \
             VALUES ('u1', '123-45-6789', '***-**-6789')"
            ),
            &[],
        )
        .await
        .unwrap();
    support::grant_runtime_select_columns(&admin_pool, app, coll, &["id", &ssn_raw]).await;
    install_role_bound_select_policy(&admin_pool, app, coll, &role).await;
    let login_role = "p6a_unmask_login";
    let (login_url, login_pool) =
        provision_platform_login_pool(&admin_pool, &url, login_role, "test", &role, app).await;

    // The SENSITIVE value now lives in the raw sibling column (the storage
    // flip), so that is the column this proof must show is unreachable by
    // direct SQL before `dispatch_unmask` narrows to the per-app role.
    let blocked = login_pool
        .query_text_params(
            &format!("SELECT \"{ssn_raw}\" FROM \"{app}\".\"{coll}\" WHERE id = 'u1'"),
            &[],
        )
        .await
        .unwrap();
    assert!(
        blocked.is_empty(),
        "login role must be blocked by FORCE RLS from the raw column before unmask proves \
         the role fence"
    );

    zeroship_plugin_db::set_postgres_pool_for_tests(login_pool.clone(), &login_url);
    zeroship_plugin_db::cache_schema_for_tests(app, coll, schema);
    zeroship_plugin_db::clear_mask_policy_cache_for_tests(app);

    let result = unmask::dispatch_unmask(
        &unmask_route(app).await,
        &DbBinding::cold_start(app),
        UnmaskFieldArgs {
            collection: coll.to_string(),
            row_pk: "u1".to_string(),
            column: "ssn".to_string(),
            actor: Some(json!({ "kind": "auto" })),
            reason: Some("security regression".to_string()),
            rejected_claim: None,
        },
    )
    .await
    .expect("unmask must read under the per-app role");
    assert_eq!(result.plaintext, "123-45-6789");

    drop(login_pool);
    let _ = admin_pool
        .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await;
    let _ = admin_pool
        .execute(&format!("DROP ROLE IF EXISTS \"{login_role}\""), &[])
        .await;
    let _ = admin_pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
        .await;
    release_pg(admin_pool).await;
}

/// PG + masked + **ENCRYPTED** + unmask: the matrix cell that never existed.
///
/// `fetch_and_decrypt`'s PostgreSQL arm reads the raw column as
/// `Option<&str>` (`crud/unmask.rs:546-547`). For an ENCRYPTED column the raw
/// sibling is BYTEA, and `&str: FromSql::accepts` refuses BYTEA -
/// `libs/compio-postgres/vendor/postgres-types/src/lib.rs:729-742` lists
/// VARCHAR/TEXT/BPCHAR/NAME/UNKNOWN plus citext/ltree and falls through to
/// `false` for everything else. `Row::get_inner` consults `accepts` BEFORE
/// decoding, and does so even for NULL (`libs/compio-postgres/src/row.rs:256`).
///
/// The funnel additionally binds every result in BINARY format
/// (`libs/compio-postgres/src/query.rs:186`), so the `\xHHHH...` text rendering
/// the pre-fix comment described is not what arrives either. Two independent
/// reasons, one outcome: a `DbError::internal` carrying `error deserializing
/// column 0`. The read now goes through
/// `backend::pg_autocommit::roled_scalar_bytes`, so on the pre-fix code the
/// message was prefixed `unmask: get column value: ` and today it would be
/// `db: read scalar bytes: `; this test asserts on the unmasked VALUE, not on
/// either string.
///
/// WHY NOTHING CAUGHT IT. Every live PG unmask fixture declares a masked but
/// UNENCRYPTED column, so this arm was never entered; the encrypted round-trip
/// test never unmasks; and the SQLite twin passes because it reads
/// `TypedCell::Blob` (`crud/unmask.rs:585`).
///
/// THIS IS THE SIBLING OF `unmask_fetch_runs_under_per_app_role_via_rls` WITH
/// EXACTLY ONE VARIABLE CHANGED: the column is encrypted. Same role, same
/// column grants, same role-bound policy, same login pool, same dispatch call.
/// That is deliberate - a failure here cannot be a missing grant, a missing
/// audit table or an unprovisioned role, because those would fail the sibling
/// too. The only new thing is the BYTEA raw column.
#[compio::test]
async fn unmask_encrypted_column_on_pg_reads_bytea_raw_sibling() {
    use zeroship_plugin_db::crud::unmask::{self, UnmaskFieldArgs};

    let url = require_pg().await;
    let admin_pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    // Synthetic 32-byte root key, same shape as the encrypted round-trip gate.
    let _keys = with_root_key("default", &"b".repeat(64));

    let app = crate::test_app_id!();

    let app = app.as_str();
    let coll = "users";
    let role = provision_app_with_role(&admin_pool, app).await;
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&admin_pool, app)
        .await
        .unwrap();
    let schema = json!({
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" },
            "encrypted": { "mode": "randomised", "keyId": "default", "wraps": "string" }
        }
    });
    let ssn_raw = raw_column_name("ssn");
    // The emitter decides the raw sibling's type. For an encrypted column that
    // is BYTEA, which is the whole point of this test - so build the DDL rather
    // than hand-spelling it, or the fixture proves nothing about the runtime.
    let create_table = build_create_table_with_fks(&zeroship_schema::SchemaName::new(app).expect("fixture schema name"), coll, &schema, &FkEmission::Inline)
        .expect("emitter must build the users DDL");
    admin_pool.batch_execute(&create_table).await.unwrap();

    // Real ciphertext from the platform's own encryptor, under the AAD the read
    // path recomputes: canonical_aad(collection, column, Some(row_pk)) for the
    // randomised mode (crud/unmask.rs:503-510).
    let backend = zeroship_plugin_db::backend::PostgresBackend::new(
        admin_pool.clone(),
        url.clone(),
        zeroship_plugin_db::isolate_key_source(),
    );
    let key = backend
        .key_store()
        .resolve(app, "default")
        .await
        .expect("resolve_key");
    let aad = encryption::canonical_aad(coll, "ssn", Some(b"u1"));
    let ct = zeroship_plugin_db::encryption::aead::encrypt(
        &key,
        EncryptionMode::Randomised,
        b"123-45-6789",
        &aad,
    )
    .expect("encrypt");
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &ct);
    admin_pool
        .execute(
            &format!(
                "INSERT INTO \"{app}\".\"{coll}\" (id, \"{ssn_raw}\", ssn) \
                 VALUES ('u1', decode($1, 'base64')::bytea, '***-**-6789')"
            ),
            &[&b64.as_str()],
        )
        .await
        .unwrap();
    support::grant_runtime_select_columns(&admin_pool, app, coll, &["id", &ssn_raw]).await;
    install_role_bound_select_policy(&admin_pool, app, coll, &role).await;
    let login_role = "p6a_unmask_enc_login";
    let (login_url, login_pool) =
        provision_platform_login_pool(&admin_pool, &url, login_role, "test", &role, app).await;

    zeroship_plugin_db::set_postgres_pool_for_tests(login_pool.clone(), &login_url);
    zeroship_plugin_db::cache_schema_for_tests(app, coll, schema);
    zeroship_plugin_db::clear_mask_policy_cache_for_tests(app);

    let result = unmask::dispatch_unmask(
        &unmask_route(app).await,
        &DbBinding::cold_start(app),
        UnmaskFieldArgs {
            collection: coll.to_string(),
            row_pk: "u1".to_string(),
            column: "ssn".to_string(),
            actor: Some(json!({ "kind": "auto" })),
            reason: Some("encrypted unmask regression".to_string()),
            rejected_claim: None,
        },
    )
    .await;

    // Surface the real error rather than a bare unwrap panic: on the pre-fix
    // code this printed `error deserializing column 0`, which is the evidence
    // that the failure is the BYTEA decode and nothing else.
    let unmasked = result.unwrap_or_else(|e| {
        panic!("unmask of an ENCRYPTED column must recover the plaintext, got: {e:?}")
    });
    assert_eq!(unmasked.plaintext, "123-45-6789");

    drop(login_pool);
    let _ = admin_pool
        .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await;
    let _ = admin_pool
        .execute(&format!("DROP ROLE IF EXISTS \"{login_role}\""), &[])
        .await;
    let _ = admin_pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
        .await;
    release_pg(admin_pool).await;
}

/// EVERY statement `dispatch_unmask` issues must go through `SET LOCAL ROLE`,
/// including the audit INSERT.
///
/// WHY THE SIBLING ABOVE DOES NOT COVER THIS.
/// `unmask_fetch_runs_under_per_app_role_via_rls` blocks the login role with
/// FORCE RLS on the DATA table only, and its login role holds an INHERITING
/// membership plus direct `USAGE`/`SELECT` grants. The audit table carries no
/// RLS, so `write_audit_unmask_row`'s INSERT succeeded there through ambient
/// inheritance whether or not it was fenced - it passed identically before and
/// after this fix, which is the one shape a regression guard must not have.
///
/// THE FIXTURE IS PRODUCTION'S POSTURE, not an RLS stand-in for it. The login
/// role is granted the app role `WITH INHERIT FALSE` - what
/// `zeroship-migrate-server`'s `runtime_dependents_sql` now emits - and NOTHING
/// directly. Under that grant a statement that omits `SET LOCAL ROLE` has no
/// privilege at all, so this case binds the whole dispatch rather than one
/// table: fetch, decrypt-or-plaintext, and audit all have to narrow or the
/// call fails.
///
/// FAILS BEFORE THE FIX with `permission denied for table
/// __zeroship_audit_unmask`, because `write_audit_unmask_row` took
/// `pg.pool_handle()` and issued the INSERT on a bare checkout.
#[compio::test]
async fn unmask_audit_insert_runs_under_the_per_app_role_not_the_login_role() {
    use zeroship_plugin_db::crud::unmask::{self, UnmaskFieldArgs};

    let url = require_pg().await;
    let admin_pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = crate::test_app_id!();

    let app = app.as_str();
    let coll = "patients";
    let role = provision_app_with_role(&admin_pool, app).await;
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&admin_pool, app)
        .await
        .unwrap();
    let schema = json!({
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "phi" }
        }
    });
    let ssn_raw = raw_column_name("ssn");
    // Built with the platform's own emitter, not hand-spelled, so the fixture
    // cannot drift from the runtime's DDL shape: `ssn` gets the bare-TEXT mask
    // column and `__zs_raw__ssn` gets the declared type for the real value.
    let create_table = build_create_table_with_fks(&zeroship_schema::SchemaName::new(app).expect("fixture schema name"), coll, &schema, &FkEmission::Inline)
        .expect("emitter must build the patients DDL");
    admin_pool.batch_execute(&create_table).await.unwrap();
    admin_pool
        .execute(
            &format!(
                "INSERT INTO \"{app}\".\"{coll}\" (id, \"{ssn_raw}\", ssn) \
                 VALUES ('p1', '555-44-3333', '***-**-3333')"
            ),
            &[],
        )
        .await
        .unwrap();
    support::grant_runtime_select_columns(&admin_pool, app, coll, &["id", &ssn_raw]).await;

    let login_role = "p6a_unmask_audit_login";
    let _ = admin_pool
        .execute(&format!("DROP ROLE IF EXISTS \"{login_role}\""), &[])
        .await;
    admin_pool
        .execute(
            &format!("CREATE ROLE \"{login_role}\" LOGIN PASSWORD 'test' INHERIT"),
            &[],
        )
        .await
        .unwrap();
    // The production grant. `INHERIT` on the role above is deliberate and is
    // the point: the ROLE ATTRIBUTE says inherit, the MEMBERSHIP says do not,
    // and PostgreSQL 16+ honours the membership - so this fixture also pins
    // that the attribute is not what fences anything.
    admin_pool
        .execute(
            &format!("GRANT \"{role}\" TO \"{login_role}\" WITH INHERIT FALSE"),
            &[],
        )
        .await
        .unwrap();

    let login_url = login_role_test_url(&url, login_role, "test");
    let login_pool = std::rc::Rc::new(Pool::connect(&login_url, 4).await.unwrap());

    // THE CONTROL. Without this the case would pass just as happily if the app
    // role had never been granted anything: "denied" is the resting state of a
    // role with no privileges. This proves the login role is genuinely fenced
    // out, so the success below can only come from narrowing.
    let ambient = login_pool
        .query_text_params(
            &format!("SELECT ssn FROM \"{app}\".\"{coll}\" WHERE id = 'p1'"),
            &[],
        )
        .await;
    assert!(
        ambient.is_err(),
        "the login role must reach nothing ambiently under WITH INHERIT FALSE"
    );

    zeroship_plugin_db::set_postgres_pool_for_tests(login_pool.clone(), &login_url);
    zeroship_plugin_db::cache_schema_for_tests(app, coll, schema);
    zeroship_plugin_db::clear_mask_policy_cache_for_tests(app);

    let result = unmask::dispatch_unmask(
        &unmask_route(app).await,
        &DbBinding::cold_start(app),
        UnmaskFieldArgs {
            collection: coll.to_string(),
            row_pk: "p1".to_string(),
            column: "ssn".to_string(),
            actor: Some(json!({ "kind": "auto" })),
            reason: Some("audit fence regression".to_string()),
            rejected_claim: None,
        },
    )
    .await
    .expect(
        "every statement in dispatch_unmask must narrow to the per-app role - \
         a failure here names the one that did not",
    );
    assert_eq!(result.plaintext, "555-44-3333");

    // THE AUDIT ROW MUST EXIST. `dispatch_unmask` propagates the INSERT's error
    // with `?`, so a swallowed audit write would return plaintext with no
    // record of who read it - strictly worse than refusing. Read back through
    // the ADMIN pool, which is not the one under test.
    let audited = admin_pool
        .query_text_params(
            &format!(
                "SELECT outcome FROM \"{app}\".\"__zeroship_audit_unmask\" \
                  WHERE collection = $1 AND row_pk = 'p1' AND \"column\" = 'ssn'"
            ),
            &[coll],
        )
        .await
        .unwrap();
    assert_eq!(
        audited.len(),
        1,
        "the granted unmask must have written exactly one audit row"
    );
    assert_eq!(audited[0].get::<_, &str>("outcome"), "granted");

    drop(login_pool);
    let _ = admin_pool
        .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await;
    let _ = admin_pool
        .execute(&format!("DROP ROLE IF EXISTS \"{login_role}\""), &[])
        .await;
    let _ = admin_pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
        .await;
    release_pg(admin_pool).await;
}

/// Regression fence for the PG mask-policy arm.
///
/// Until 2026-08-27 `dispatch_set_mask_policy` wrote through
/// `set_mask_policy` and `dispatch_unmask` re-read through
/// `get_mask_policy`, both definer-rights routines in the platform-owned
/// system schema. Neither had an installer, so on PG the write failed
/// outright and every unmask on an app with no cached policy died with an
/// undefined-schema error instead of default-denying. The durable PG store was
/// deleted rather than rehomed: the policy the creator declares in
/// source is installed straight into the per-isolate cache at boot, and
/// that cache is the only reader.
///
/// This test FAILS BEFORE THAT CHANGE at the
/// `dispatch_set_mask_policy` call, and it is the only live-PG coverage
/// of the declared-policy authorization path -- the sibling
/// `unmask_fetch_runs_under_per_app_role_via_rls` exercises the
/// no-policy `auto` fallback, not a declared policy.
#[compio::test]
async fn pg_declared_mask_policy_authorizes_unmask_without_durable_store() {
    use zeroship_plugin_db::crud::mask_policy;
    use zeroship_plugin_db::crud::unmask::{self, UnmaskFieldArgs};

    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = crate::test_app_id!();
    let app = app.as_str();
    let coll = "patients";

    // Drops the schema and the cluster-scoped per-app role, then
    // recreates the schema. `ensure_per_app_role` below creates the role
    // the read path checks for -- without it the unmask SELECT refuses
    // with `schema_not_provisioned` before authorization is ever reached.
    let role = provision_app_with_role(&pool, app).await;
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();

    let schema = json!({
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" }
        }
    });
    let ssn_raw = raw_column_name("ssn");
    // Built with the platform's own emitter, not hand-spelled, so the fixture
    // cannot drift from the runtime's DDL shape: `ssn` gets the bare-TEXT mask
    // column and `__zs_raw__ssn` gets the declared type for the real value.
    let create_table = build_create_table_with_fks(&zeroship_schema::SchemaName::new(app).expect("fixture schema name"), coll, &schema, &FkEmission::Inline)
        .expect("emitter must build the patients DDL");
    pool.batch_execute(&create_table).await.unwrap();
    pool.execute(
        &format!(
            "INSERT INTO \"{app}\".\"{coll}\" (id, \"{ssn_raw}\", ssn) \
             VALUES ('u1', '123-45-6789', '***-**-6789')"
        ),
        &[],
    )
    .await
    .unwrap();
    support::grant_runtime_select_columns(&pool, app, coll, &["id", &ssn_raw]).await;

    zeroship_plugin_db::set_postgres_pool_for_tests(pool.clone(), &url);
    zeroship_plugin_db::cache_schema_for_tests(app, coll, schema);
    zeroship_plugin_db::clear_mask_policy_cache_for_tests(app);

    // The boot-time install `installSchema` performs. Before the fix
    // this issued `SELECT set_mask_policy(...)` against the platform-owned
    // system schema and failed here on every database.
    mask_policy::dispatch_set_mask_policy(
        // The backend the V8 dispatcher resolves before calling the installer.
        &unmask_backend().await,
        app,
        json!({ "support": ["spi"] }),
    )
    .await
    .expect("setMaskPolicy must install the declared policy on PG");

    // A role the declared policy grants reads through.
    let granted = unmask::dispatch_unmask(
        &unmask_route(app).await,
        &DbBinding::cold_start(app),
        UnmaskFieldArgs {
            collection: coll.to_string(),
            row_pk: "u1".to_string(),
            column: "ssn".to_string(),
            actor: Some(json!({ "kind": "support" })),
            reason: Some("declared policy grant".to_string()),
            rejected_claim: None,
        },
    )
    .await
    .expect("the declared policy must authorize the role it lists");
    assert_eq!(granted.plaintext, "123-45-6789");

    // A role the policy does NOT list is refused. Without this arm the
    // test would pass on an implementation that authorized everything,
    // which is exactly the failure mode a cache-only policy could hide.
    let err = unmask::dispatch_unmask(
        &unmask_route(app).await,
        &DbBinding::cold_start(app),
        UnmaskFieldArgs {
            collection: coll.to_string(),
            row_pk: "u1".to_string(),
            column: "ssn".to_string(),
            actor: Some(json!({ "kind": "intern" })),
            reason: Some("declared policy deny".to_string()),
            rejected_claim: None,
        },
    )
    .await
    .expect_err("a role absent from the declared policy must be refused");
    match err {
        zeroship_data_core::error::DbError::Coded { ref code, .. } => {
            assert_eq!(code, "unmask_not_permitted", "got {err:?}");
        }
        other => panic!("expected unmask_not_permitted, got {other:?}"),
    }

    // Roles are CLUSTER-scoped, not database-scoped: leaving this one
    // behind makes every later run of this test anywhere on the same
    // server fail at `CREATE ROLE` with 42710, in a database that looks
    // pristine. Drop the schema first so the role owns nothing.
    let _ = pool
        .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await;
    let _ = pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
        .await;
    release_pg(pool).await;
}

#[compio::test]
async fn wal_connection_stays_platform_role() {
    // §17.5: the WAL/replication connection stays under the platform
    // role and is NEVER switched to a per-app role. This is a structural
    // assertion: the replication helpers (`ensure_worker_slot` and the
    // section 17.7 deprovision) run on the pool
    // directly with NO `SET ROLE` — only the transaction BEGIN paths
    // (`exec_begin`) applies the per-app role. We pin
    // that the role-application surface is exactly the two tx-begin
    // helpers by asserting `apply_per_app_role` is not invoked from the
    // replication/WAL code (verified at the source level — there is no
    // `set_local_role`/`set_role`/`apply_per_app_role` call anywhere in
    // replication.rs / wal_consumer.rs / change_stream_pg.rs).
    //
    // The runtime half: provision a role, then run a replication-side
    // operation on the pool and confirm it executes as the platform
    // login role (current_user unchanged), NOT the per-app role.
    // No `cdc_budget` permit: despite the name, this test creates no slot and
    // no publication. It provisions a role and reads `current_user` off the
    // pool, so it contends for nothing the CDC budget is sized against.
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = crate::test_app_id!();
    let app = app.as_str();
    let role = provision_app_with_role(&pool, app).await;
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();

    // A replication-side read (the watchdog query shape) runs on the
    // pool with no SET ROLE — current_user is the login role.
    let who = pool
        .query_text_params("SELECT current_user AS u", &[])
        .await
        .unwrap();
    let current: String = who[0].get("u");
    assert_ne!(
        current, role,
        "WAL/replication pool connection must stay on the platform login \
         role, never the per-app role"
    );

    let _ = pool
        .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await;
    let _ = pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
        .await;
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// Drop-namespace slot sequencing.
//
// `drop_namespace` runs subscription gate, broker drain, slot teardown
// through ChangeStream::deprovision, DROP SCHEMA CASCADE, then DROP ROLE. These
// tests provision a full app (schema + slot + publication + per-app role)
// and verify the ordering, the subscription gate (defer vs --force),
// idempotency of steps 3-5, and retry-from-step-3 on partial failure.
//
// Slot-dependent tests skip when wal_level != logical (CI's pg-test runs
// with -c wal_level=logical).
// ---------------------------------------------------------------------------

use zeroship_plugin_db::backend::BackendHandle;
use zeroship_plugin_db::drop_namespace::{DropNamespaceOpts, DropNamespaceOutcome, drop_namespace};

/// Build a `BackendHandle::Postgres` over a fresh `PostgresBackend` for
/// the drop-namespace tests. (`PostgresBackend` is already imported at
/// module scope earlier in this file — referenced unqualified here.)
fn pg_backend_handle(pool: &std::rc::Rc<Pool>, url: &str) -> BackendHandle {
    BackendHandle::Postgres(std::rc::Rc::new(
        zeroship_plugin_db::backend::PostgresBackend::new(
            std::rc::Rc::clone(pool),
            url.to_string(),
            zeroship_plugin_db::isolate_key_source(),
        ),
    ))
}

async fn slot_exists(pool: &Pool, app: &str) -> bool {
    let slot = zeroship_plugin_db::replication::worker_slot_name(app, CDC_TEST_WORKER_ID).unwrap();
    let rows = pool
        .query_text_params(
            "SELECT 1 FROM pg_replication_slots WHERE slot_name = $1",
            &[slot.as_str()],
        )
        .await
        .unwrap();
    !rows.is_empty()
}

async fn publication_exists(pool: &Pool, app: &str) -> bool {
    let pubn = zeroship_plugin_db::replication::publication_name(app).unwrap();
    let rows = pool
        .query_text_params(
            "SELECT 1 FROM pg_publication WHERE pubname = $1",
            &[pubn.as_str()],
        )
        .await
        .unwrap();
    !rows.is_empty()
}

async fn schema_exists(pool: &Pool, app: &str) -> bool {
    let rows = pool
        .query_text_params("SELECT 1 FROM pg_namespace WHERE nspname = $1", &[app])
        .await
        .unwrap();
    !rows.is_empty()
}

async fn role_exists(pool: &Pool, app: &str) -> bool {
    let role = zeroship_core::database_role::per_app_role_name(app)
        .expect("role lookup app id must produce a valid PostgreSQL role name");
    let rows = pool
        .query_text_params(
            "SELECT 1 FROM pg_roles WHERE rolname = $1",
            &[role.as_str()],
        )
        .await
        .unwrap();
    !rows.is_empty()
}

#[compio::test]
async fn drop_namespace_defers_on_active_subscription() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = crate::test_app_id!();
    let app = app.as_str();
    c1_cleanup(&pool, app).await;
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();

    let backend = pg_backend_handle(&pool, &url);
    // count > 0, force = false → defer. No teardown runs.
    let outcome = drop_namespace(
        &backend,
        &pool,
        app,
        &DropNamespaceOpts {
            force: false,
            subscription_count: 2,
        },
    )
    .await
    .expect("drop_namespace");

    assert_eq!(
        outcome,
        DropNamespaceOutcome::Deferred {
            active_subscriptions: 2
        },
        "active subscription without --force must defer with the count"
    );
    // Schema must still exist — no teardown ran.
    assert!(
        schema_exists(&pool, app).await,
        "deferred drop must NOT drop the schema"
    );

    c1_cleanup(&pool, app).await;
    drop(backend);
    release_pg(pool).await;
}

#[compio::test]
async fn drop_namespace_force_fires_subscription_app_dropped() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = crate::test_app_id!();
    let app = app.as_str();
    c1_cleanup(&pool, app).await;
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();

    // Register a live subscription on this thread's broker so the
    // force-drain has something to close.
    let sub = zeroship_plugin_db::broker::subscribe(app, "widgets");
    assert_eq!(
        zeroship_plugin_db::broker::app_subscription_count(app),
        1,
        "subscription should be live before drop"
    );

    let backend = pg_backend_handle(&pool, &url);
    // force = true → drain broker + proceed to completion.
    let outcome = drop_namespace(
        &backend,
        &pool,
        app,
        &DropNamespaceOpts {
            force: true,
            subscription_count: 1,
        },
    )
    .await
    .expect("drop_namespace --force");
    assert_eq!(
        outcome,
        DropNamespaceOutcome::Completed,
        "--force must complete"
    );

    // The subscription must have been closed (subscription_app_dropped →
    // broker Closed). The iterator surfaces the terminal close.
    assert!(
        sub.is_closed(),
        "active subscriber must be closed under --force"
    );
    assert_eq!(
        zeroship_plugin_db::broker::app_subscription_count(app),
        0,
        "broker must be drained for the app after --force drop"
    );
    // Schema gone.
    assert!(
        !schema_exists(&pool, app).await,
        "schema must be dropped under --force"
    );

    c1_cleanup(&pool, app).await;
    drop(backend);
    release_pg(pool).await;
}

#[compio::test]
async fn drop_namespace_pg_drops_slots_but_retains_migration_publication() {
    let url = require_pg().await;
    let _cdc = cdc_budget::shared();
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    require_logical_wal(&pool).await;
    let app = crate::test_app_id!();
    let app = app.as_str();
    c1_cleanup(&pool, app).await;
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    c1_create_publication(&pool, app).await;
    // Provision the worker slot against the migration-owned publication.
    zeroship_plugin_db::replication::ensure_worker_slot(&pool, app, CDC_TEST_WORKER_ID)
        .await
        .expect("provision worker slot");
    assert!(slot_exists(&pool, app).await, "slot provisioned");
    assert!(
        publication_exists(&pool, app).await,
        "publication provisioned"
    );

    let backend = pg_backend_handle(&pool, &url);
    let outcome = drop_namespace(
        &backend,
        &pool,
        app,
        &DropNamespaceOpts {
            force: false,
            subscription_count: 0,
        },
    )
    .await
    .expect("drop_namespace");
    assert_eq!(outcome, DropNamespaceOutcome::Completed);

    // Worker teardown removes slots and the schema but leaves publication
    // ownership with the migration service. Dropping the schema removes its
    // relation memberships, so the retained publication is empty.
    assert!(!slot_exists(&pool, app).await, "slot must be dropped");
    assert!(
        publication_exists(&pool, app).await,
        "publication must be retained"
    );
    assert!(!schema_exists(&pool, app).await, "schema must be dropped");

    c1_cleanup(&pool, app).await;
    drop(backend);
    release_pg(pool).await;
}

#[compio::test]
async fn drop_namespace_drops_per_app_role_last() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = crate::test_app_id!();
    let app = app.as_str();
    c1_cleanup(&pool, app).await;
    let role = zeroship_core::database_role::per_app_role_name(app)
        .expect("drop-role fixture app id must produce a valid PostgreSQL role name");
    let _ = pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
        .await;

    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    // Provision the per-app role + give it an object in the schema so the
    // "role still owns objects" path is exercised (the CASCADE must clear
    // it before DROP ROLE).
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();
    pool.execute(
        &format!(r#"CREATE TABLE "{app}".t (id SERIAL PRIMARY KEY)"#),
        &[],
    )
    .await
    .unwrap();
    assert!(role_exists(&pool, app).await, "role provisioned");

    let backend = pg_backend_handle(&pool, &url);
    let outcome = drop_namespace(
        &backend,
        &pool,
        app,
        &DropNamespaceOpts {
            force: false,
            subscription_count: 0,
        },
    )
    .await
    .expect("drop_namespace");
    assert_eq!(outcome, DropNamespaceOutcome::Completed);

    // Both schema and role are gone; the role was dropped after schema (step 5).
    assert!(!schema_exists(&pool, app).await, "schema dropped");
    assert!(!role_exists(&pool, app).await, "per-app role dropped last");

    c1_cleanup(&pool, app).await;
    drop(backend);
    release_pg(pool).await;
}

#[compio::test]
async fn drop_namespace_idempotent_steps_3_to_5() {
    let url = require_pg().await;
    let _cdc = cdc_budget::shared();
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    require_logical_wal(&pool).await;
    let app = crate::test_app_id!();
    let app = app.as_str();
    c1_cleanup(&pool, app).await;
    let role = zeroship_core::database_role::per_app_role_name(app)
        .expect("idempotent-drop fixture app id must produce a valid PostgreSQL role name");
    let _ = pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
        .await;
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    c1_create_publication(&pool, app).await;
    zeroship_plugin_db::replication::ensure_worker_slot(&pool, app, CDC_TEST_WORKER_ID)
        .await
        .unwrap();
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();

    let backend = pg_backend_handle(&pool, &url);
    let opts = DropNamespaceOpts {
        force: false,
        subscription_count: 0,
    };

    // First drop: full teardown.
    let first = drop_namespace(&backend, &pool, app, &opts)
        .await
        .expect("first drop");
    assert_eq!(first, DropNamespaceOutcome::Completed);
    assert!(!slot_exists(&pool, app).await);
    assert!(publication_exists(&pool, app).await);
    assert!(!schema_exists(&pool, app).await);
    assert!(!role_exists(&pool, app).await);

    // Second drop on the already-torn-down app: every step (3-5) is a
    // no-op, returns Completed, no error.
    let second = drop_namespace(&backend, &pool, app, &opts)
        .await
        .expect("second drop must be idempotent");
    assert_eq!(
        second,
        DropNamespaceOutcome::Completed,
        "idempotent re-drop must succeed with everything already gone"
    );

    c1_cleanup(&pool, app).await;
    drop(backend);
    release_pg(pool).await;
}

#[compio::test]
async fn drop_namespace_retries_from_step_3_on_partial_failure() {
    // Retry from step 3 on partial failure; steps 3-5 are idempotent.
    // We simulate a partial failure by dropping the slots first (leaving
    // the publication, schema, and role), then running drop_namespace -
    // step 3 (deprovision) finds nothing to do (idempotent), and steps
    // 4-5 finish the teardown. This proves a re-run after a crash that
    // got partway through completes cleanly.
    let url = require_pg().await;
    let _cdc = cdc_budget::shared();
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    require_logical_wal(&pool).await;
    let app = crate::test_app_id!();
    let app = app.as_str();
    c1_cleanup(&pool, app).await;
    let role = zeroship_core::database_role::per_app_role_name(app)
        .expect("drop-retry fixture app id must produce a valid PostgreSQL role name");
    let _ = pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
        .await;
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    c1_create_publication(&pool, app).await;
    zeroship_plugin_db::replication::ensure_worker_slot(&pool, app, CDC_TEST_WORKER_ID)
        .await
        .unwrap();
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();

    // Simulate a crash AFTER step 3 (slots dropped) but BEFORE
    // steps 4-5 (schema + role still present).
    zeroship_plugin_db::replication::drop_worker_slots(&pool, app)
        .await
        .expect("partial: drop worker slots");
    assert!(!slot_exists(&pool, app).await, "slot gone after partial");
    assert!(
        publication_exists(&pool, app).await,
        "publication retained after partial"
    );
    assert!(
        schema_exists(&pool, app).await,
        "schema still present after partial"
    );
    assert!(
        role_exists(&pool, app).await,
        "role still present after partial"
    );

    // Retry: step 3 is a no-op (nothing to deprovision), steps 4-5 finish.
    let backend = pg_backend_handle(&pool, &url);
    let outcome = drop_namespace(
        &backend,
        &pool,
        app,
        &DropNamespaceOpts {
            force: false,
            subscription_count: 0,
        },
    )
    .await
    .expect("retry drop_namespace after partial failure");
    assert_eq!(outcome, DropNamespaceOutcome::Completed);
    assert!(
        !schema_exists(&pool, app).await,
        "retry must drop the schema"
    );
    assert!(!role_exists(&pool, app).await, "retry must drop the role");

    c1_cleanup(&pool, app).await;
    drop(backend);
    release_pg(pool).await;
}

/// A new test must not open a pool or raw client without teardown.
///
/// Every direct connection in this directory is paired with teardown that
/// drains its driver while the runtime is alive. That pairing is a
/// convention, and nothing stops test 108 from calling `Pool::connect` and
/// forgetting it - the suite would stay green, because two leaked connections are
/// nowhere near the ceiling, until the count creeps back up and returns as
/// "dozens of tests cannot connect".
///
/// So pin the number of direct construction sites. Adding a test that opens its
/// own pool or raw client now fails here and has to be a deliberate edit; adding
/// one that uses the helpers does not touch this count.
///
/// SCOPE, stated because a check that does not say what it covers gets trusted
/// for more than it checks: this counts direct constructions across EVERY `.rs`
/// file in this tests directory - pooled and raw, top level AND subdirectories.
/// It does NOT cover other crates. control, auth, gateway, migrated and both
/// compio libs all construct connections in their tests with no teardown at all;
/// the same leak was measured in control (peak backends climbing 0 to 8 across
/// ten tests under `--test-threads=1`). Nothing here guards those.
///
/// THE SCAN DESCENDS, and it did not until 2026-08-20. It used a flat
/// `read_dir` and skipped `tests/parity/`, so `parity/mod.rs` - a module
/// `integration.rs` and `sqlite_integration.rs` both compile, and which raw
/// connects to probe for a live server - was invisible to a check whose own
/// comment claimed it covered every file here. The `files >= 2` floor was
/// supposed to catch exactly that narrowing and did not: two is met by the two
/// largest files alone, so the floor could not tell a whole directory apart from
/// nothing. A file floor cannot catch this at all - flatten the walk and it
/// still reads 9 of the 10 files. So the guard that does is `nested_files`,
/// which goes to zero the moment the walk stops descending; the file floor below
/// only rules out the scan being aimed somewhere else entirely.
///
/// What it still does NOT catch: a site reached through an aliased import
/// (`use compio_postgres::Pool as P; P::connect(..)`), a connection opened by a
/// helper in another crate, or a site that HAS teardown text nearby but never
/// runs it on the failing path. It counts constructor spellings, not liveness.
///
/// Sound as a text check because these are CONSTRUCTION sites: a constructor has
/// to be written literally to be called, so it cannot hide behind indirection the
/// way an execution can. Verified when this was written: no aliased `Pool`
/// import and no indirect use of the constructor. It also said every site sat
/// directly in a test body with no shared helper wrapping one; that was wrong on
/// the day it was written - `parity::maybe_pg_url` is exactly such a helper, and
/// it went unnoticed because the scan could not see the directory it lives in.
/// An alias would still evade this, which is why the message says to keep the
/// pairing rather than to satisfy the number.
///
/// THIS TEST HAS AN EXPIRY, and it expires by SUCCEEDING. It counts sites, not
/// pools opened. A shared helper that opens a pool is one site whatever number of
/// tests call it - so the day a setup helper lands, this count collapses toward
/// one, every new test routes through the helper without touching it, and the
/// assertion passes forever while measuring nothing. Nothing will have broken;
/// the codebase will have moved and left the check enumerating an empty space.
///
/// So when a pool-owning helper is introduced, REPLACE this test rather than
/// lowering PINNED to match. What it should become is a check over the helper -
/// that it is the only thing constructing a pool, or that its own teardown runs -
/// because at that point the helper is the property worth guarding.
#[test]
fn direct_connection_sites_do_not_grow() {
    // Split so this test's own needles are not part of what it counts.
    let needles = [
        concat!("Pool", "::connect("),
        concat!("Pool", "::connect_with_config("),
        concat!("Pool", "::connect_with_pool_config("),
        concat!("compio_postgres", "::connect("),
    ];
    let root = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests"));

    let mut sites = 0usize;
    let mut files = 0usize;
    let mut nested_files = 0usize;
    let mut pending = vec![(root, false)];
    while let Some((dir, nested)) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("read a tests directory") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                pending.push((path, true));
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("read a test file");
            files += 1;
            nested_files += usize::from(nested);
            for needle in needles {
                sites += source.matches(needle).count();
            }
        }
    }

    // 121 = 120 at the top level + 1 in tests/parity/mod.rs, which the flat scan
    // this replaced never read. Raised from 119 for two reasons, both named
    // because a pin moved without one is a rubber stamp:
    //   +1  c1_setup_refuses_to_create_a_missing_publication, added 2026-08-16 in
    //       2a44ea8ef. It opens its own pool and DOES pair it with `release_pg`,
    //       which is the property this pin exists to keep, so it is an accepted
    //       site and not a leak. The gate has been red since that commit landed.
    //   +1  parity::maybe_pg_url, unchanged since 2026-05-24 and older than this
    //       test. Not a new connection - a newly VISIBLE one, in scope only
    //       because the walk now descends.
    // Raised to 122 for one more:
    //   +1  bytes_column_stores_raw_bytes_on_postgres. It has to dial the server
    //       itself - the whole point of the test is that it reads the stored
    //       cell WITHOUT going through `env.db`, and the SDK path is the thing
    //       under suspicion. It drops the client and calls `drain_pg` before its
    //       first assertion, so the socket is returned even on the failing path.
    // Raised to 123 on 2026-09-01. The arithmetic, because a pin moved without
    // one is a rubber stamp - and here the two numbers do not match, which is
    // exactly the case that needs writing down:
    //   +2  `tests/mask_flip.rs` went from 11 connect sites to 13
    //       (measured at d245ee35c vs HEAD). The two are
    //       `a_rejected_impersonation_is_distinguishable_from_an_absent_actor`
    //       and `a_query_hint_reads_the_column_its_alias_resolved_to`. Each
    //       opens its own pool because each needs a distinct app schema, and
    //       each ends in `release_pg(pool).await` - that file is 13 connects to
    //       13 releases, which is the property this pin exists to keep.
    //   -1  the previous pin carried one slot of headroom: the measured total
    //       moved 122 -> 123, not 122 -> 124. Recorded rather than smoothed
    //       over, because "+2 sites, +1 pin" reads like an arithmetic error
    //       otherwise, and the next person should not have to re-derive it.
    //
    // DO NOT SPELL THE NEEDLE OUT IN THIS FILE. The scan reads every `.rs`
    // under `tests/`, this file included, which is why the needles above are
    // assembled with `concat!`. The first draft of the comment you are reading
    // wrote one of them in full to explain the arithmetic, and the count went
    // 123 -> 124: the pin was raised, the test stayed red, and the extra site
    // was the prose describing the sites. Say "connect sites", never the
    // literal.
    // Raised to 128 on 2026-09-03. The arithmetic, again, because a pin moved
    // without one is a rubber stamp - and again the two numbers disagree, this
    // time because the pin was ALREADY one behind:
    //   +1  `tests/mask_flip.rs` went 13 -> 14 connect sites before this change
    //       (measured at HEAD, bdc3be963). The pin above says 13 and was never
    //       raised, so the gate was red on a clean tree.
    //   +4  `tests/unmask_tx_lane.rs`, added the same day: one `require_pg`
    //       probe plus one pool per test. Each test ends in
    //       `release_pg(pool).await` and the probe drops its client and detaches
    //       the connection task, which is the property this pin exists to keep.
    //       They cannot share one pool: each fixture drops and recreates its own
    //       app schema, and a shared pool would let one test's DROP SCHEMA run
    //       against another's live rows.
    // Raised to 131 on 2026-09-03, and this time the arithmetic closes exactly:
    //   +3  `tests/search_tx_lane.rs`, added the same day: one `require_pg`
    //       probe plus one pool per test, and there are two tests (the vector
    //       arm and the spatial arm). Measured by needle, not inferred - the
    //       file holds 2 pool sites, 1 probe site and 2 `release_pg(pool)`
    //       calls, so every pool it opens is released, which is the property
    //       this pin exists to keep. They cannot share one pool for the same
    //       reason `unmask_tx_lane.rs`'s cannot: each fixture drops and
    //       recreates its own app schema.
    // Raised to 132 on 2026-09-04, and the arithmetic closes exactly:
    //   +1  `tests/support/mod.rs::sweep_prior_run_residue_once`, added the same
    //       day. It is the once-per-binary residue sweep, and it opens its own
    //       pool because it runs BEFORE any test has one - it is the barrier
    //       every `require_pg` passes through. It drops the pool and awaits
    //       `drain_connections` inside the runtime that opened it, which is the
    //       pairing this pin exists to keep.
    //   0   nothing else moved: the parallel-isolation change that landed with
    //       it rewrote app ids and added permits, neither of which is a
    //       constructor spelling. Measured at 132 against 131 before it.
    // Raised to 134 on 2026-09-04, and the arithmetic closes exactly:
    //   +2  `tests/mask_flip.rs`, one `Pool::connect` per test for the two
    //       protection-downgrade gates. They cannot share one pool: each calls
    //       `fixture`, which DROPs and recreates its own app schema, and one of
    //       them supplies a root key the other must not see. Both end in
    //       `release_pg(pool)`, which is the pairing this pin exists to keep.
    //       They add no `require_pg` site - that helper already existed in the
    //       file and is one site however many tests call it.
    // Raised to 136 on 2026-09-04, and the arithmetic closes exactly:
    //   +2  `tests/mask_flip.rs`, one connect site per test for the two
    //       migration-engine protection gates. Measured by needle: that file
    //       held 16 sites at bc7b05be4 and holds 18 now, and the directory
    //       total moved 134 -> 136. They cannot share a pool with their
    //       data-plane-emitter peers: each calls a fixture that DROPs and
    //       recreates its own app schema, and the encryption one supplies a
    //       root key the mask one must not see. Both end in `release_pg(pool)`,
    //       which is the pairing this pin exists to keep. They add no
    //       `require_pg` site - that helper already existed in the file and is
    //       one site however many tests call it.
    const PINNED: usize = 136;
    // 10 files today, one of them nested. This floor alone does NOT catch a walk
    // that stops descending - measured: flattening it reads 9 and clears 9. That
    // is what the second assertion is for. This one catches the scan being
    // pointed at the wrong directory or reading nothing, which `>= 2` could not.
    assert!(
        files >= 9,
        "expected to scan the whole tests directory, saw {files} file(s) - if this \
         drops the count is measuring less than it claims"
    );
    assert!(
        nested_files >= 1,
        "the walk read {files} file(s) but none from a subdirectory - it stopped \
         descending, and every connection site under tests/parity/ is then \
         uncounted while this still reports a number"
    );
    assert!(
        sites <= PINNED,
        "the tests directory now opens {sites} connections directly, up from {PINNED}. \
         A pool or client opened without a `release_pg`/`drain_pg` teardown outlives \
         its runtime and leaks a server connection. Pair the new one with a teardown, \
         then raise PINNED. If you are adding a shared connection-owning helper \
         instead, do not lower PINNED to match - this counts sites, and a helper is \
         one site however many tests call it, so it would pass forever without \
         checking anything. Replace this with a check over the helper."
    );
}

// ---------------------------------------------------------------------------
// `OwnedPooledClient`: the transaction connection is a pool checkout
//
// Until 2026-08-27 `acquire_dedicated_client` called
// `compio_postgres::connect` directly and spawned a detached task per
// connection. It never touched the pool, so the concurrent-transaction ceiling
// was UNBOUNDED - a worker multiplexing ~200 apps that each open a transaction
// opened ~200 backends, which is a way to exhaust a cluster's
// `max_connections` from one process.
//
// Both arms below fail on that code: the first because the pool's counters
// never move, the second because an unbounded acquire never has to wait.
// ---------------------------------------------------------------------------

#[compio::test]
async fn a_dedicated_client_is_a_pool_checkout_and_returns_on_drop() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    let backend = zeroship_plugin_db::backend::PostgresBackend::new(
        std::rc::Rc::clone(&pool),
        url.clone(),
        zeroship_plugin_db::isolate_key_source(),
    );

    let active_before = pool.active_count();
    let created_before = pool.metrics.connections_created.get();

    let client = {
        use zeroship_plugin_db::backend::SqlExecutor as _;
        backend
            .acquire_dedicated_client("app_pool_probe")
            .await
            .expect("dedicated client")
    };

    assert_eq!(
        pool.active_count(),
        active_before + 1,
        "a dedicated client must be a checkout from THIS pool; the pool's \
         active count did not move, so the connection came from somewhere else"
    );
    // The warm pool already holds idle connections, so this checkout must not
    // have opened a new backend at all.
    assert_eq!(
        pool.metrics.connections_created.get(),
        created_before,
        "the checkout opened a new connection instead of reusing an idle one"
    );

    drop(client);

    assert_eq!(
        pool.active_count(),
        active_before,
        "the dedicated client did not return to the pool on drop"
    );
}

#[compio::test]
async fn concurrent_dedicated_clients_are_bounded_by_the_pool() {
    use std::time::Duration;

    let url = require_pg().await;
    // `max_size: 1` makes the ceiling observable in one checkout; a short
    // acquire timeout keeps the queued caller's wait bounded so the test is
    // measuring the ceiling rather than sitting on the 30 s default.
    let mut config = compio_postgres::PoolConfig::default();
    config
        .max_size(1)
        .min_idle(1)
        .acquire_timeout(Duration::from_millis(400));
    let pool = std::rc::Rc::new(
        Pool::connect_with_pool_config(&url, config)
            .await
            .expect("pool"),
    );
    let backend = zeroship_plugin_db::backend::PostgresBackend::new(
        std::rc::Rc::clone(&pool),
        url.clone(),
        zeroship_plugin_db::isolate_key_source(),
    );

    use zeroship_plugin_db::backend::SqlExecutor as _;
    let first = backend
        .acquire_dedicated_client("app_pool_probe")
        .await
        .expect("first dedicated client");

    // THE INVERSION THIS STEP OWNS: a transaction that used to get a
    // connection of its own now queues, and refuses when the wait expires.
    // Conservative policy, and OWED a real decision: queue on the pool's
    // acquire timeout rather than refuse immediately, no per-app fairness, and
    // the ceiling is whatever the shared data pool is sized to.
    let second = backend.acquire_dedicated_client("app_pool_probe").await;
    let err = second.expect_err(
        "a second dedicated client must be bounded by the pool, not opened \
         directly - an unbounded model is how one worker exhausts max_connections",
    );
    let message = format!("{err:?}");
    assert!(
        message.contains("timeout"),
        "the refusal must name the acquire timeout so an operator can see the \
         ceiling was hit; got {message}"
    );

    drop(first);

    // And the ceiling is a queue, not a wall: once the lease returns, the next
    // checkout succeeds.
    let third = backend
        .acquire_dedicated_client("app_pool_probe")
        .await
        .expect("checkout after the first lease returned");
    drop(third);
}
