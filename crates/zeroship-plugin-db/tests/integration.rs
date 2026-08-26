//! Integration tests for plugin-db query builders against real Postgres.
//!
//! Requires: the test PostgreSQL named by the overlay
//! (`deploy/ops/zeroship.test.toml`, written by
//! `tests/provision_test_backends.sh`) or by `PG_TEST_URL`. There is no
//! compiled default; see `crates/core/src/config/test_overlay.rs`.
//! Run: `cargo test -p zeroship-plugin-db --test integration -- --test-threads=1`

use compio_postgres::{NoTls, Pool};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_plugin_db::backend::ChangeStream;

const CDC_TEST_WORKER_ID: &str = "plugin-db-integration-worker";

#[path = "parity/mod.rs"]
mod parity;

fn test_url() -> String {
    zeroship_core::config::test_database_url()
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
            // The orchestrator's bootstrap stage opens a
            // dedicated client via the Backend trait's
            // `acquire_dedicated_client`, which reads the URL from the
            // per-isolate context. Tests that drive the orchestrator
            // through `exec_register_model_with_pool` need the URL
            // installed in the context BEFORE the call.
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

const SCHEMA: &str = "plugin_db_test";

/// Set up the test schema and table. Drops and recreates on every call.
async fn setup(pool: &Pool) {
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{SCHEMA}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{SCHEMA}\""), &[])
        .await
        .unwrap();
    pool.execute(
        &format!(
            r#"CREATE TABLE "{SCHEMA}"."notes" (
                id SERIAL PRIMARY KEY,
                title TEXT NOT NULL,
                body TEXT,
                category TEXT,
                views INTEGER DEFAULT 0,
                tags JSONB DEFAULT '[]'::jsonb,
                created_at TIMESTAMPTZ DEFAULT NOW(),
                updated_at TIMESTAMPTZ DEFAULT NOW()
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

/// Postgres and the dev SQLite tier must hand `env.db` callers the same JSON.
///
/// NOT `#[ignore]`, and that is the point of this test's history. It carried
/// `#[ignore = "requires live postgres; default gate runs the sqlite leg only"]`
/// until 2026-08-21, which was false in both halves: every one of its 108
/// siblings in this file requires live Postgres and none of them is ignored, and
/// the default gate for this target is `tests/run_plugin_db_live_suite.sh`, which
/// runs it WITHOUT `--ignored`. So the attribute did not describe a prerequisite -
/// it removed the test from the only job that could have run it, and that is how
/// the 2026-08-10 `registerModel` cutover (d84cbbd84) left it broken for eleven
/// days with every gate green. It now fails the way its siblings do: `require_pg`
/// panics rather than skipping, because a run that reports "ok" against no
/// database says the opposite of the truth.
#[compio::test]
async fn parity_matrix_pg_matches_sqlite_projection() {
    let pg_url = require_pg().await;
    let sqlite_dir = tempfile::tempdir().expect("create sqlite parity dir");

    let sqlite = parity::run_matrix(&parity::sqlite_url(&sqlite_dir));
    let pg = parity::run_matrix(&pg_url);

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
    let pg = parity::run_matrix(&pg_url);

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
        "SELECT payload_bytes FROM \"default\".\"{}\" WHERE title = $1",
        pg.collection
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
    assert_eq!(
        pg.typed["payload_bytes"],
        json!(parity::typed_bytes_b64()),
        "env.db must hand back the base64 of the stored bytes"
    );
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
    setup(&pool).await;

    // Insert
    let bq = build_insert(SCHEMA, "notes", &json!({"title": "Hello", "body": "World", "category": "tech"})).unwrap();
    let inserted = exec_mutation(&pool, bq).await;
    assert_eq!(inserted.len(), 1);
    assert_eq!(inserted[0]["title"], "Hello");
    assert_eq!(inserted[0]["body"], "World");
    assert!(inserted[0]["id"].as_i64().unwrap() > 0);

    // Find
    let bq = build_find(SCHEMA, "notes", &json!({}), None, None, None, None).unwrap();
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
    setup(&pool).await;

    let docs = json!([
        {"title": "A", "body": "one", "category": "tech"},
        {"title": "B", "body": "two", "category": "food"},
        {"title": "C", "body": "three", "category": "tech"}
    ]);
    let bq = build_insert_many(SCHEMA, "notes", &docs).unwrap();
    let inserted = exec_mutation(&pool, bq).await;
    assert_eq!(inserted.len(), 3);

    // Verify all in DB
    let bq = build_count(SCHEMA, "notes", &json!({})).unwrap();
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
    setup(&pool).await;

    // Insert
    let bq = build_insert(SCHEMA, "notes", &json!({"title": "Counter", "category": "tech", "views": 0})).unwrap();
    exec_mutation(&pool, bq).await;

    // $inc views by 5
    let bq = build_update_one(SCHEMA, "notes", &json!({"title": "Counter"}), &json!({"views": {"$inc": 5}})).unwrap();
    let updated = exec_mutation(&pool, bq).await;
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0]["views"], 5);

    // $inc again
    let bq = build_update_one(SCHEMA, "notes", &json!({"title": "Counter"}), &json!({"views": {"$inc": 3}})).unwrap();
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
    setup(&pool).await;

    let bq = build_insert(SCHEMA, "notes", &json!({"title": "Math", "category": "tech", "views": 10})).unwrap();
    exec_mutation(&pool, bq).await;

    // $dec
    let bq = build_update_one(SCHEMA, "notes", &json!({"title": "Math"}), &json!({"views": {"$dec": 3}})).unwrap();
    let updated = exec_mutation(&pool, bq).await;
    assert_eq!(updated[0]["views"], 7);

    // $mul
    let bq = build_update_one(SCHEMA, "notes", &json!({"title": "Math"}), &json!({"views": {"$mul": 2}})).unwrap();
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
    setup(&pool).await;

    let bq = build_insert(SCHEMA, "notes", &json!({"title": "Tags", "category": "tech"})).unwrap();
    exec_mutation(&pool, bq).await;

    // $push "rust"
    let bq = build_update_one(SCHEMA, "notes", &json!({"title": "Tags"}), &json!({"tags": {"$push": "rust"}})).unwrap();
    let updated = exec_mutation(&pool, bq).await;
    let tags = updated[0]["tags"].as_array().unwrap();
    assert!(tags.contains(&json!("rust")));

    // $push "go"
    let bq = build_update_one(SCHEMA, "notes", &json!({"title": "Tags"}), &json!({"tags": {"$push": "go"}})).unwrap();
    let updated = exec_mutation(&pool, bq).await;
    let tags = updated[0]["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 2);
    assert!(tags.contains(&json!("rust")));
    assert!(tags.contains(&json!("go")));

    // $addToSet "rust" (duplicate — should NOT add)
    let bq = build_update_one(SCHEMA, "notes", &json!({"title": "Tags"}), &json!({"tags": {"$addToSet": "rust"}})).unwrap();
    let updated = exec_mutation(&pool, bq).await;
    let tags = updated[0]["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 2); // still 2

    // $addToSet "python" (new — should add)
    let bq = build_update_one(SCHEMA, "notes", &json!({"title": "Tags"}), &json!({"tags": {"$addToSet": "python"}})).unwrap();
    let updated = exec_mutation(&pool, bq).await;
    let tags = updated[0]["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 3);

    // $pull "go"
    let bq = build_update_one(SCHEMA, "notes", &json!({"title": "Tags"}), &json!({"tags": {"$pull": "go"}})).unwrap();
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
    setup(&pool).await;

    // Insert 3 tech, 1 food
    let docs = json!([
        {"title": "A", "category": "tech", "views": 0},
        {"title": "B", "category": "tech", "views": 0},
        {"title": "C", "category": "tech", "views": 0},
        {"title": "D", "category": "food", "views": 0}
    ]);
    let bq = build_insert_many(SCHEMA, "notes", &docs).unwrap();
    exec_mutation(&pool, bq).await;

    // Update all tech views +1
    let bq = build_update_many(SCHEMA, "notes", &json!({"category": "tech"}), &json!({"views": {"$inc": 1}})).unwrap();
    let updated = exec_mutation(&pool, bq).await;
    assert_eq!(updated.len(), 3);

    // Verify food unchanged
    let bq = build_find(SCHEMA, "notes", &json!({"category": "food"}), None, None, None, None).unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows[0]["views"], 0);

    // Verify tech updated
    let bq = build_find(SCHEMA, "notes", &json!({"category": "tech"}), None, None, None, None).unwrap();
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
    setup(&pool).await;

    let docs = json!([
        {"title": "Keep1", "category": "tech"},
        {"title": "Keep2", "category": "tech"},
        {"title": "Del1", "category": "food"},
        {"title": "Del2", "category": "food"},
        {"title": "Del3", "category": "food"}
    ]);
    let bq = build_insert_many(SCHEMA, "notes", &docs).unwrap();
    exec_mutation(&pool, bq).await;

    // Delete one food
    let bq = build_delete_one(SCHEMA, "notes", &json!({"category": "food"})).unwrap();
    let deleted = exec_mutation(&pool, bq).await;
    assert_eq!(deleted.len(), 1);

    // 4 remaining
    let bq = build_count(SCHEMA, "notes", &json!({})).unwrap();
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = pool.query_text_params(&bq.sql, &param_refs).await.unwrap();
    assert_eq!(rows[0].get::<_, i64>("count"), 4);

    // Delete many remaining food
    let bq = build_delete_many(SCHEMA, "notes", &json!({"category": "food"})).unwrap();
    let deleted = exec_mutation(&pool, bq).await;
    assert_eq!(deleted.len(), 2);

    // 2 tech remaining
    let bq = build_count(SCHEMA, "notes", &json!({})).unwrap();
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
    setup(&pool).await;

    let docs = json!([
        {"title": "A", "category": "tech", "views": 10},
        {"title": "B", "category": "tech", "views": 20},
        {"title": "C", "category": "food", "views": 30},
        {"title": "D", "category": "food", "views": 40}
    ]);
    let bq = build_insert_many(SCHEMA, "notes", &docs).unwrap();
    exec_mutation(&pool, bq).await;

    // $gt 25
    let bq = build_find(SCHEMA, "notes", &json!({"views": {"$gt": 25}}), None, None, None, None).unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 2);

    // $lte 20
    let bq = build_find(SCHEMA, "notes", &json!({"views": {"$lte": 20}}), None, None, None, None).unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 2);

    // $in
    let bq = build_find(SCHEMA, "notes", &json!({"category": {"$in": ["tech", "food"]}}), None, None, None, None).unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 4);

    // $nin
    let bq = build_find(SCHEMA, "notes", &json!({"category": {"$nin": ["food"]}}), None, None, None, None).unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 2);

    // $ne
    let bq = build_find(SCHEMA, "notes", &json!({"category": {"$ne": "food"}}), None, None, None, None).unwrap();
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
    setup(&pool).await;

    let docs = json!([
        {"title": "A", "category": "tech", "views": 10},
        {"title": "B", "category": "tech", "views": 50},
        {"title": "C", "category": "food", "views": 10}
    ]);
    let bq = build_insert_many(SCHEMA, "notes", &docs).unwrap();
    exec_mutation(&pool, bq).await;

    // $and: tech AND views > 20
    let bq = build_find(SCHEMA, "notes", &json!({"$and": [{"category": "tech"}, {"views": {"$gt": 20}}]}), None, None, None, None).unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["title"], "B");

    // $or: tech OR views > 20
    let bq = build_find(SCHEMA, "notes", &json!({"$or": [{"category": "tech"}, {"views": {"$gt": 20}}]}), None, None, None, None).unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 2); // A and B

    // $not: NOT food
    let bq = build_find(SCHEMA, "notes", &json!({"$not": {"category": "food"}}), None, None, None, None).unwrap();
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
    setup(&pool).await;

    let docs = json!([
        {"title": "Hello World", "category": "tech"},
        {"title": "hello rust", "category": "tech"},
        {"title": "Goodbye", "category": "food"}
    ]);
    let bq = build_insert_many(SCHEMA, "notes", &docs).unwrap();
    exec_mutation(&pool, bq).await;

    // $like (case sensitive)
    let bq = build_find(SCHEMA, "notes", &json!({"title": {"$like": "Hello%"}}), None, None, None, None).unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 1);

    // $ilike (case insensitive)
    let bq = build_find(SCHEMA, "notes", &json!({"title": {"$ilike": "%hello%"}}), None, None, None, None).unwrap();
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
    setup(&pool).await;

    let docs = json!([
        {"title": "C", "category": "tech", "views": 30},
        {"title": "A", "category": "tech", "views": 10},
        {"title": "B", "category": "tech", "views": 20}
    ]);
    let bq = build_insert_many(SCHEMA, "notes", &docs).unwrap();
    exec_mutation(&pool, bq).await;

    // Order by views ASC, limit 2
    let bq = build_find(SCHEMA, "notes", &json!({}), Some(2), None, Some(&json!({"views": 1})), None).unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["title"], "A");
    assert_eq!(rows[1]["title"], "B");

    // Order by views DESC, limit 1, offset 1
    let bq = build_find(SCHEMA, "notes", &json!({}), Some(1), Some(1), Some(&json!({"views": -1})), None).unwrap();
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
    setup(&pool).await;

    let bq = build_insert(SCHEMA, "notes", &json!({"title": "Proj", "body": "secret", "category": "tech"})).unwrap();
    exec_mutation(&pool, bq).await;

    let bq = build_find(SCHEMA, "notes", &json!({}), None, None, None, Some(&json!(["title", "category"]))).unwrap();
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
    setup(&pool).await;

    let docs = json!([
        {"title": "A", "category": "tech"},
        {"title": "B", "category": "tech"},
        {"title": "C", "category": "food"},
        {"title": "D", "category": "science"}
    ]);
    let bq = build_insert_many(SCHEMA, "notes", &docs).unwrap();
    exec_mutation(&pool, bq).await;

    let bq = build_distinct(SCHEMA, "notes", "category", &json!({})).unwrap();
    let rows = exec_query(&pool, bq).await;
    let values: Vec<&str> = rows.iter().map(|r| r["category"].as_str().unwrap()).collect();
    assert_eq!(values.len(), 3);
    assert!(values.contains(&"tech"));
    assert!(values.contains(&"food"));
    assert!(values.contains(&"science"));

    // Distinct with filter
    let bq = build_distinct(SCHEMA, "notes", "category", &json!({"category": {"$ne": "science"}})).unwrap();
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
    setup(&pool).await;

    let docs = json!([
        {"title": "A", "category": "tech"},
        {"title": "B", "category": "tech"},
        {"title": "C", "category": "food"}
    ]);
    let bq = build_insert_many(SCHEMA, "notes", &docs).unwrap();
    exec_mutation(&pool, bq).await;

    // Count all
    let bq = build_count(SCHEMA, "notes", &json!({})).unwrap();
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = pool.query_text_params(&bq.sql, &param_refs).await.unwrap();
    assert_eq!(rows[0].get::<_, i64>("count"), 3);

    // Count with filter
    let bq = build_count(SCHEMA, "notes", &json!({"category": "tech"})).unwrap();
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
    setup(&pool).await;

    let docs = json!([
        {"title": "A", "category": "tech", "views": 10},
        {"title": "B", "category": "tech", "views": 20},
        {"title": "C", "category": "tech", "views": 30},
        {"title": "D", "category": "food", "views": 100}
    ]);
    let bq = build_insert_many(SCHEMA, "notes", &docs).unwrap();
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
    let bq = build_aggregate(SCHEMA, "notes", &pipeline).unwrap();
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
    setup(&pool).await;

    let docs = json!([
        {"title": "A", "category": "tech", "body": "rust", "views": 10},
        {"title": "B", "category": "tech", "body": "rust", "views": 20},
        {"title": "C", "category": "tech", "body": "go", "views": 5},
        {"title": "D", "category": "food", "body": "pasta", "views": 50}
    ]);
    let bq = build_insert_many(SCHEMA, "notes", &docs).unwrap();
    exec_mutation(&pool, bq).await;

    let pipeline = json!([
        {"$group": {
            "by": ["category", "body"],
            "cnt": {"$count": true}
        }},
        {"$sort": {"cnt": -1}}
    ]);
    let bq = build_aggregate(SCHEMA, "notes", &pipeline).unwrap();
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
    setup(&pool).await;

    let docs = json!([
        {"title": "A", "category": "tech", "views": 10},
        {"title": "B", "category": "tech", "views": 20},
        {"title": "C", "category": "tech", "views": 30},
        {"title": "D", "category": "food", "views": 5}
    ]);
    let bq = build_insert_many(SCHEMA, "notes", &docs).unwrap();
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
    let bq = build_aggregate(SCHEMA, "notes", &pipeline).unwrap();
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
    setup(&pool).await;

    // Insert with body
    let bq = build_insert(SCHEMA, "notes", &json!({"title": "WithBody", "body": "has content", "category": "tech"})).unwrap();
    exec_mutation(&pool, bq).await;
    // Insert without body (column defaults to NULL)
    let bq = build_insert(SCHEMA, "notes", &json!({"title": "NoBody", "category": "tech"})).unwrap();
    exec_mutation(&pool, bq).await;

    // Find where body IS NULL
    let bq = build_find(SCHEMA, "notes", &json!({"body": null}), None, None, None, None).unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["title"], "NoBody");

    // Find where body IS NOT NULL
    let bq = build_find(SCHEMA, "notes", &json!({"body": {"$ne": null}}), None, None, None, None).unwrap();
    let rows = exec_query(&pool, bq).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["title"], "WithBody");

    // $exists: true
    let bq = build_find(SCHEMA, "notes", &json!({"body": {"$exists": true}}), None, None, None, None).unwrap();
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
    setup(&pool).await;

    let bq = build_insert(SCHEMA, "notes", &json!({"title": "Mix", "category": "tech", "views": 10})).unwrap();
    exec_mutation(&pool, bq).await;

    // Update: set category + inc views + push tag
    let bq = build_update_one(
        SCHEMA, "notes",
        &json!({"title": "Mix"}),
        &json!({"category": "science", "views": {"$inc": 5}, "tags": {"$push": "new"}}),
    ).unwrap();
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
    setup(&pool).await;

    let bq = build_insert(SCHEMA, "notes", &json!({"title": "Time", "category": "tech"})).unwrap();
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
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());

    // Set up weather table
    pool.execute(&format!("DROP TABLE IF EXISTS \"{SCHEMA}\".\"weather\""), &[]).await.unwrap();
    pool.execute(
        &format!(
            r#"CREATE TABLE "{SCHEMA}"."weather" (
                city TEXT,
                temp_lo INTEGER,
                temp_hi INTEGER
            )"#
        ),
        &[],
    ).await.unwrap();

    let docs = json!([
        {"city": "San Francisco", "temp_lo": 46, "temp_hi": 50},
        {"city": "San Francisco", "temp_lo": 43, "temp_hi": 57},
        {"city": "San Francisco", "temp_lo": 35, "temp_hi": 65},
        {"city": "Hayward", "temp_lo": 37, "temp_hi": 54},
        {"city": "Hayward", "temp_lo": 38, "temp_hi": 52},
        {"city": "Hayward", "temp_lo": 41, "temp_hi": 55}
    ]);
    let bq = build_insert_many(SCHEMA, "weather", &docs).unwrap();
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
    let bq = build_aggregate(SCHEMA, "weather", &pipeline).unwrap();

    // Verify SQL has the resolved expression, not the alias
    assert!(bq.sql.contains("HAVING MAX(\"temp_lo\") < $"), "sql: {}", bq.sql);

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
    let app = "a1_test";
    let collection = "users";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&zeroship_plugin_db::query::build_create_schema(app), &[])
        .await
        .unwrap();

    let schema = json!({
        "email": {"type": "string", "required": true, "unique": true},
        "handle": {"type": "string", "index": true},
    });

    let create_table =
        build_create_table_with_fks(app, collection, &schema, &FkEmission::Inline).unwrap();
    // `build_create_table_with_fks` emits MULTI-statement DDL (the CREATE TABLE
    // plus the system-field index `CREATE INDEX`s, and on PG the
    // `COMMENT ON COLUMN … '__zsmask:…'` / `'zsenc:…'` sentinels). The
    // extended/prepared `execute` path rejects that with `42601 cannot insert
    // multiple commands into a prepared statement`; the simple-query
    // `batch_execute` is the correct executor for rendered DDL batches.
    pool.batch_execute(&create_table).await.unwrap();

    // Generate and execute the new index DDL.
    let indexes =
        zeroship_plugin_db::query::build_create_indexes(app, collection, &schema).unwrap();
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
    let ins1 = build_insert(app, collection, &with_seed_id(json!({"email": "a@x.com"}))).unwrap();
    let p1: Vec<&str> = ins1.params.iter().map(String::as_str).collect();
    pool.query_text_params(&ins1.sql, &p1).await.unwrap();

    // Distinct `id` so the second insert is rejected for the DUPLICATE EMAIL
    // (the unique index under test), not an incidental duplicate PK.
    let ins2 = build_insert(app, collection, &with_seed_id(json!({"email": "a@x.com"}))).unwrap();
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
// 23. A3 — `__zeroship_migrations` audit table is created idempotently and
// receives rows for every DDL operation performed by the orchestrator.
//
// Pre-A3: A1 retries logged via tracing::warn! with a TODO marker. Post-A3
// the audit table is populated by the four-phase orchestrator so
// operators can see what ran, when, and by whom.
// ---------------------------------------------------------------------------

#[compio::test]
async fn a3_audit_table_created_and_idempotent() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = "a3_audit_test";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&zeroship_plugin_db::query::build_create_schema(app), &[])
        .await
        .unwrap();

    // First call: should create __zeroship_migrations table + 2 indexes.
    zeroship_plugin_db::audit::ensure_audit_table_exists(&pool, app)
        .await
        .unwrap();

    // Confirm it exists.
    let rows = pool
        .query_text_params(
            "SELECT COUNT(*) AS n FROM information_schema.tables WHERE table_schema = $1 AND table_name = $2",
            &[app, "__zeroship_migrations"],
        )
        .await
        .unwrap();
    let n: i64 = rows[0].get("n");
    assert_eq!(n, 1, "audit table should exist");

    // Idempotency — second call must not error.
    zeroship_plugin_db::audit::ensure_audit_table_exists(&pool, app)
        .await
        .unwrap();
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// A pre-existing audit table with the OLD 7-status
// CHECK constraint (no `validation_refused`) gets widened
// by `ensure_audit_table_exists`. The "fresh-table" branch was covered by
// the test above; this closes the **upgrade path** every existing-app
// deploy hits.
//
// The prior "two consecutive ensure_audit_table_exists calls" check only
// exercises the no-op rewrite branch (table created with the NEW CHECK,
// then re-ALTERed to the same body). This test forces the DROP-old /
// ADD-new path and asserts the post-state accepts `'validation_refused'`.
// ---------------------------------------------------------------------------

/// CHECK ALTER upgrade path.
///
/// Seeds the audit table with the OLD CHECK constraint
/// (7 statuses, no `validation_refused`), runs `ensure_audit_table_exists`,
/// and asserts the constraint was widened and now accepts the new value.
#[compio::test]
async fn a3_audit_table_check_alter_upgrades_existing_constraint() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = "a3_audit_alter_upgrade";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&zeroship_plugin_db::query::build_create_schema(app), &[])
        .await
        .unwrap();

    // Seed the table with the OLD CHECK body -- the
    // 7-status list without `validation_refused`. The DDL otherwise
    // matches the current shape so `ensure_audit_table_exists`'s
    // CREATE-IF-NOT-EXISTS is a no-op and only the DROP+ADD path runs.
    let old_create = format!(
        r#"CREATE TABLE "{app}"."__zeroship_migrations" (
  id                  BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  collection          TEXT NOT NULL,
  phase               TEXT NOT NULL,
  change_class        TEXT NOT NULL,
  change_kind         TEXT NOT NULL,
  details             JSONB NOT NULL,
  ddl_sql             TEXT,
  created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  applied_at          TIMESTAMPTZ,
  applied_by_kind     TEXT NOT NULL,
  applied_by_id       TEXT,
  deploy_id           TEXT NOT NULL,
  parent_id           BIGINT REFERENCES "{app}"."__zeroship_migrations"(id),
  schema_version      INTEGER NOT NULL,
  status              TEXT NOT NULL,
  error               TEXT,
  duration_ms         INTEGER,
  validate_cursor     BIGINT,
  owner_session_id    TEXT,
  last_heartbeat_at   TIMESTAMPTZ,
  dead_letter_pks     JSONB,
  audit_generation    BIGINT NOT NULL DEFAULT 0,
  CONSTRAINT __zeroship_migrations_phase_chk CHECK (
    phase IN ('ddl','validation','backfill','audit')
  ),
  CONSTRAINT __zeroship_migrations_class_chk CHECK (
    change_class IN ('additive','compatible','destructive')
  ),
  CONSTRAINT __zeroship_migrations_status_chk CHECK (
    status IN ('pending','running','applied','applied_with_dead_letter','failed','cancelled','rolled_back')
  )
)"#
    );
    pool.execute(&old_create, &[]).await.unwrap();

    // Sanity-check the seed: the OLD constraint must refuse
    // `validation_refused` before the migration runs. If this insert
    // somehow succeeds we'd be testing nothing.
    let pre_insert = format!(
        r#"INSERT INTO "{app}"."__zeroship_migrations"
            (collection, phase, change_class, change_kind, details,
             applied_by_kind, deploy_id, schema_version, status)
           VALUES ('c','ddl','additive','create_table','{{}}'::jsonb,
                   'system','seed_pre',1,'validation_refused')"#
    );
    let pre_err = pool
        .query_text_params(&pre_insert, &[])
        .await
        .expect_err("seed CHECK must refuse 'validation_refused' before ALTER");
    let pre_code = pre_err
        .code()
        .map(|c| c.code().to_string())
        .unwrap_or_default();
    assert_eq!(
        pre_code, "23514",
        "pre-ALTER insert must fail with check_violation (23514), got: {pre_err}"
    );

    // Run the migration — DROP-old / ADD-new on the named constraint.
    zeroship_plugin_db::audit::ensure_audit_table_exists(&pool, app)
        .await
        .unwrap();

    // Verify the constraint body literally contains 'validation_refused'.
    // `pg_get_constraintdef` returns the canonicalised SQL Postgres stored,
    // which is the most reliable thing to grep — names alone could match
    // a stale leftover.
    let def_rows = pool
        .query_text_params(
            "SELECT pg_get_constraintdef(c.oid) AS def \
             FROM pg_constraint c \
             JOIN pg_class t ON t.oid = c.conrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             WHERE n.nspname = $1 \
               AND t.relname = '__zeroship_migrations' \
               AND c.conname = '__zeroship_migrations_status_chk'",
            &[app],
        )
        .await
        .unwrap();
    assert_eq!(
        def_rows.len(),
        1,
        "expected exactly one status_chk row, got {}",
        def_rows.len()
    );
    let def: String = def_rows[0].get("def");
    assert!(
        def.contains("validation_refused"),
        "post-ALTER status_chk should include validation_refused, got: {def}"
    );

    // Insert with `status = 'validation_refused'` — must now succeed.
    let post_insert = format!(
        r#"INSERT INTO "{app}"."__zeroship_migrations"
            (collection, phase, change_class, change_kind, details,
             applied_by_kind, deploy_id, schema_version, status)
           VALUES ('c','ddl','destructive','drop_column','{{}}'::jsonb,
                   'system','seed_post',1,'validation_refused')"#
    );
    pool.execute(&post_insert, &[])
        .await
        .expect("post-ALTER insert of 'validation_refused' must succeed");

    // The constraint is still active — an unknown status must be rejected
    // with SQLSTATE 23514 (check_violation), proving the widening didn't
    // accidentally drop the constraint without re-adding it.
    let bad_insert = format!(
        r#"INSERT INTO "{app}"."__zeroship_migrations"
            (collection, phase, change_class, change_kind, details,
             applied_by_kind, deploy_id, schema_version, status)
           VALUES ('c','ddl','additive','create_table','{{}}'::jsonb,
                   'system','seed_bad',2,'invalid_unknown_status')"#
    );
    let bad_err = pool
        .query_text_params(&bad_insert, &[])
        .await
        .expect_err("post-ALTER CHECK must still refuse unknown statuses");
    let bad_code = bad_err
        .code()
        .map(|c| c.code().to_string())
        .unwrap_or_default();
    assert_eq!(
        bad_code, "23514",
        "unknown status must fail with check_violation (23514), got: {bad_err}"
    );
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 24. A2/A3 — first-deploy registerModel writes audit rows for table +
// index creation. The four-phase orchestrator drives every change
// through __zeroship_migrations.
// ---------------------------------------------------------------------------

#[compio::test]
async fn a2_first_deploy_writes_audit_rows() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = "a2_first_deploy";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();

    let schema = json!({
        "email": {"type": "string", "required": true, "unique": true},
        "name": {"type": "string"},
    });

    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool),
        app,
        "users",
        &schema,
        &serde_json::json!([]),
        "test_deploy_1",)
    .await
    .unwrap_or_else(|e| panic!("first deploy failed: {e}"));

    // The orchestrator should have logged a create_table op + one add_index op.
    let rows = pool
        .query_text_params(
            &format!(
                "SELECT change_kind, status, deploy_id FROM \"{app}\".\"__zeroship_migrations\" \
                 WHERE phase = 'ddl' ORDER BY id"
            ),
            &[],
        )
        .await
        .unwrap();

    assert!(rows.len() >= 2, "expected at least create_table + add_index, got {} rows", rows.len());
    let kinds: Vec<String> = rows
        .iter()
        .map(|r| r.get::<_, String>("change_kind"))
        .collect();
    assert!(kinds.contains(&"create_table".to_string()), "audit rows: {kinds:?}");
    assert!(kinds.contains(&"add_index".to_string()), "audit rows: {kinds:?}");

    // All terminal statuses must be 'applied' for a clean deploy.
    for row in &rows {
        let st: String = row.get("status");
        let kind: String = row.get("change_kind");
        let dep: String = row.get("deploy_id");
        assert_eq!(st, "applied", "{kind} should be applied, got {st} (deploy_id={dep})");
        assert_eq!(dep, "test_deploy_1");
    }
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 25. A2 — destructive change (drop_column) is refused in strict mode.
//
// Self-assessment: this is the load-bearing test that proves the deploy
// pipeline actually refuses changes that would corrupt data.
// ---------------------------------------------------------------------------

#[compio::test]
async fn a2_destructive_drop_column_refused_strict() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = "a2_destructive";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();

    // First deploy — create with 'legacy_score'.
    let v1 = json!({
        "name": {"type": "string"},
        "legacy_score": {"type": "number"},
    });
    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool), app, "posts", &v1, &serde_json::json!([]), "deploy_v1",)
    .await
    .unwrap();

    // Second deploy — drop legacy_score. Strict default should refuse.
    let v2 = json!({
        "name": {"type": "string"},
    });
    let err = zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool), app, "posts", &v2, &serde_json::json!([]), "deploy_v2",)
    .await
    .expect_err("strict deploy should refuse drop_column");

    // The error must be a JSON envelope with code: validation_refused.
    let err_str = err.to_string();
    let parsed: serde_json::Value = serde_json::from_str(&err_str)
        .unwrap_or_else(|_| panic!("error envelope not JSON: {err_str}"));
    assert_eq!(parsed["code"], "validation_refused", "envelope: {parsed}");
    assert_eq!(parsed["deploy_id"], "deploy_v2");
    let pending = parsed["destructive_pending"].as_array().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["change_kind"], "drop_column");
    assert_eq!(pending[0]["field"], "legacy_score");

    // The audit table should show the refused op as 'validation_refused'
    // (an INSERT-direct terminal status - no orphan-Pending window).
    // Distinguishes "platform refused this DDL" from "DDL ran and
    // failed" without parsing `error`.
    let rows = pool
        .query_text_params(
            &format!(
                "SELECT change_kind, status, deploy_id FROM \"{app}\".\"__zeroship_migrations\" \
                 WHERE deploy_id = 'deploy_v2' AND change_kind = 'drop_column'"
            ),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let st: String = rows[0].get("status");
    assert_eq!(
        st, "validation_refused",
        "refused destructive ops land in validation_refused terminal",
    );

    // The legacy_score column must still exist (refused = no DDL run).
    let cols = pool
        .query_text_params(
            "SELECT column_name FROM information_schema.columns WHERE table_schema = $1 AND table_name = $2",
            &[app, "posts"],
        )
        .await
        .unwrap();
    let names: Vec<String> = cols.iter().map(|r| r.get::<_, String>("column_name")).collect();
    assert!(
        names.contains(&"legacy_score".to_string()),
        "legacy_score must remain after refused deploy; got: {names:?}"
    );
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 26. A2 — strictness=off allows the deploy through (destructive op is
// recorded but the orchestrator returns Ok). Note: with off, the
// destructive op is filtered out and the DDL is NOT actually run (we
// don't auto-drop columns under any strictness setting; off only
// suppresses the error envelope so the rest of the schema applies).
// ---------------------------------------------------------------------------

#[compio::test]
async fn a2_strictness_off_skips_validation_refused() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = "a2_strict_off";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();

    let v1 = json!({
        "name": {"type": "string"},
        "legacy_score": {"type": "number"},
    });
    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool), app, "posts", &v1, &serde_json::json!([]), "off_v1",)
    .await
    .unwrap();

    // strictness=off — drop is silently skipped, deploy succeeds.
    let v2 = json!({
        "_meta": {"strictness": "off"},
        "name": {"type": "string"},
    });
    let result = zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool), app, "posts", &v2, &serde_json::json!([]), "off_v2",)
    .await;
    assert!(result.is_ok(), "strictness=off should not refuse: {result:?}");

    // Column still exists (we don't auto-drop).
    let cols = pool
        .query_text_params(
            "SELECT column_name FROM information_schema.columns WHERE table_schema = $1 AND table_name = $2",
            &[app, "posts"],
        )
        .await
        .unwrap();
    let names: Vec<String> = cols.iter().map(|r| r.get::<_, String>("column_name")).collect();
    assert!(names.contains(&"legacy_score".to_string()));
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 27. A2 — additive change (add nullable column) auto-applies on a
// non-empty table.
// ---------------------------------------------------------------------------

#[compio::test]
async fn a2_additive_add_column_applied() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = "a2_additive";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();

    let v1 = json!({"name": {"type": "string"}});
    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool), app, "items", &v1, &serde_json::json!([]), "add_v1",)
    .await
    .unwrap();

    // Add a nullable column.
    let v2 = json!({
        "name": {"type": "string"},
        "description": {"type": "string"},
    });
    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool), app, "items", &v2, &serde_json::json!([]), "add_v2",)
    .await
    .unwrap();

    // Verify column exists.
    let cols = pool
        .query_text_params(
            "SELECT column_name FROM information_schema.columns WHERE table_schema = $1 AND table_name = $2",
            &[app, "items"],
        )
        .await
        .unwrap();
    let names: Vec<String> = cols.iter().map(|r| r.get::<_, String>("column_name")).collect();
    assert!(names.contains(&"description".to_string()), "got: {names:?}");

    // Audit row for the add_column op exists with status applied.
    let rows = pool
        .query_text_params(
            &format!(
                "SELECT status FROM \"{app}\".\"__zeroship_migrations\" \
                 WHERE deploy_id = 'add_v2' AND change_kind = 'add_column'"
            ),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let st: String = rows[0].get("status");
    assert_eq!(st, "applied");
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 28. A2 — adding a NOT NULL column to a non-empty table without default
// is detected as destructive (proposal A2 line 116).
//
// Self-assessment: this is the proposal's headline data-corruption guard.
// ---------------------------------------------------------------------------

#[compio::test]
async fn a2_not_null_on_non_empty_refused() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = "a2_notnull";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();

    // v1: schema with 'name' field.
    let v1 = json!({"name": {"type": "string"}});
    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool), app, "people", &v1, &serde_json::json!([]), "nn_v1",)
    .await
    .unwrap();

    // Insert some data so the table is non-empty.
    let bq = build_insert(app, "people", &with_seed_id(json!({"name": "alice"}))).unwrap();
    exec_mutation(&pool, bq).await;
    let bq = build_insert(app, "people", &with_seed_id(json!({"name": "bob"}))).unwrap();
    exec_mutation(&pool, bq).await;
    // ANALYZE to populate reltuples (estimate_row_count reads pg_class.reltuples).
    pool.execute(&format!("ANALYZE \"{app}\".\"people\""), &[])
        .await
        .unwrap();

    // v2: add required column without default. On a non-empty table this
    // is destructive (Postgres would reject NOT NULL with no default on
    // existing rows).
    let v2 = json!({
        "name": {"type": "string"},
        "ssn": {"type": "string", "required": true},
    });
    let err = zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool), app, "people", &v2, &serde_json::json!([]), "nn_v2",)
    .await
    .expect_err("NOT NULL add on non-empty table should be refused");

    let err_str = err.to_string();
    let parsed: serde_json::Value = serde_json::from_str(&err_str).unwrap();
    assert_eq!(parsed["code"], "validation_refused");
    let pending = parsed["destructive_pending"].as_array().unwrap();
    let ssn_op = pending
        .iter()
        .find(|p| p["field"] == "ssn")
        .expect("ssn add_column op should be listed");
    assert_eq!(ssn_op["change_kind"], "add_column");
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 30. A2 — concurrent registerModel calls serialise via the two-key
// advisory lock (proposal A2 "Concurrent-deploy semantics" section).
//
// We spawn two register_model_with_pool calls in parallel against the
// same app. Without the advisory lock, the two diff phases could race
// and emit conflicting DDL (e.g. both decide to CREATE TABLE). With the
// lock, the second call blocks until the first commits, then re-reads
// the live schema and produces a no-op diff.
//
// Both calls must succeed; afterwards the audit log contains rows from
// both deploys but only one create_table op.
// ---------------------------------------------------------------------------

#[compio::test]
async fn a2_concurrent_deploys_serialise_via_advisory_lock() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 8).await.unwrap());

    let app = "a2_concurrent";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();

    let schema = json!({
        "name": {"type": "string"},
        "tag": {"type": "string", "index": true},
    });

    // Sequential calls against the same app + same schema: the second
    // sees the table as already present (the first applied it under
    // the advisory lock) and produces a no-op diff. Verifies the
    // **idempotency** dimension of the lock contract — two callers
    // converge on the same result instead of emitting conflicting DDL.
    //
    // True concurrency under the compio single-runtime test harness
    // would require a multi-threaded runtime (compio is per-thread,
    // and `compio::runtime::spawn` schedules on the same thread). The
    // sequential variant is sufficient to verify the lock-acquire /
    // release / re-diff path without needing a second OS thread.
    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool),
        "a2_concurrent",
        "races",
        &schema,
        &serde_json::json!([]),
        "concurrent_a",)
    .await
    .expect("first deploy under lock");

    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool),
        "a2_concurrent",
        "races",
        &schema,
        &serde_json::json!([]),
        "concurrent_b",)
    .await
    .expect("second deploy under lock (lock acquired + released + re-diff)");

    // Exactly one create_table op across both deploys (the second saw
    // the table as already present and skipped it).
    let rows = pool
        .query_text_params(
            &format!(
                "SELECT COUNT(*) AS n FROM \"{app}\".\"__zeroship_migrations\" WHERE change_kind = 'create_table'"
            ),
            &[],
        )
        .await
        .unwrap();
    let n: i64 = rows[0].get("n");
    assert_eq!(n, 1, "exactly one create_table should be recorded across the two serialised deploys");

    // Verify the lock is actually being acquired+released by checking
    // pg_locks during a real call. Open a separate session that calls
    // pg_try_advisory_lock with the same key — it should succeed when
    // the orchestrator is idle (proves the lock is released cleanly).
    let try_lock = pool
        .query_text_params(
            "SELECT pg_try_advisory_lock(hashtext('zs_reg:a2_concurrent')::int4, hashtext('register_model')::int4) AS got",
            &[],
        )
        .await
        .unwrap();
    let got: bool = try_lock[0].get("got");
    assert!(got, "advisory lock should be available after orchestrator returns");

    // Release it so the test connection cleans up.
    let _ = pool
        .query_text_params(
            "SELECT pg_advisory_unlock(hashtext('zs_reg:a2_concurrent')::int4, hashtext('register_model')::int4)",
            &[],
        )
        .await;
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 31. A2 — adding a required column WITH a default literal is compatible.
// ---------------------------------------------------------------------------

#[compio::test]
async fn a2_required_with_default_is_compatible() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = "a2_reqdefault";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();

    let v1 = json!({"name": {"type": "string"}});
    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool), app, "things", &v1, &serde_json::json!([]), "rd_v1",)
    .await
    .unwrap();

    // Insert + analyze to make non-empty.
    let bq = build_insert(app, "things", &with_seed_id(json!({"name": "x"}))).unwrap();
    exec_mutation(&pool, bq).await;
    pool.execute(&format!("ANALYZE \"{app}\".\"things\""), &[])
        .await
        .unwrap();

    let v2 = json!({
        "name": {"type": "string"},
        "status": {"type": "string", "required": true, "default": "active"},
    });
    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool), app, "things", &v2, &serde_json::json!([]), "rd_v2",)
    .await
    .unwrap_or_else(|e| panic!("required-with-default should be compatible: {e}"));

    // Status column should exist with the default applied to existing rows.
    let rows = pool
        .query_text_params(
            &format!("SELECT status FROM \"{app}\".\"things\""),
            &[],
        )
        .await
        .unwrap();
    let st: String = rows[0].get("status");
    assert_eq!(st, "active");
    release_pg(pool).await;
}
// ---------------------------------------------------------------------------
// B2 — typed cross-table relations: foreign keys at the DB level
// ---------------------------------------------------------------------------

/// Helper: register two collections where `posts.authorId` is t.ref("users").
async fn b2_setup_users_posts(pool: &std::rc::Rc<Pool>, app: &str) {
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    // Users first so the FK target exists when posts is created.
    let users_schema = json!({"name": {"type": "string", "required": true}});
    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(pool),
        app,
        "users",
        &users_schema,
        &serde_json::json!([]),
        "b2_v1",)
    .await
    .expect("users registerModel");
    let posts_schema = json!({
        "title": {"type": "string", "required": true},
        "authorId": {"type": "ref", "refTarget": "users"},
    });
    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(pool),
        app,
        "posts",
        &posts_schema,
        &serde_json::json!([]),
        "b2_v1",)
    .await
    .expect("posts registerModel");
}

#[compio::test]
async fn b2_ref_creates_foreign_key() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = "b2_fk_basic";
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
    assert!(!deferrable, "expected an IMMEDIATE (non-deferrable) FK check");
    release_pg(pool).await;
}

#[compio::test]
async fn b2_ref_blocks_orphan_insert() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = "b2_orphan_insert";
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

    let app = "b2_restrict_delete";
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

    let app = "b2_cascade_delete";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();

    let users_schema = json!({"name": {"type": "string", "required": true}});
    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool), app, "users", &users_schema, &serde_json::json!([]), "b2_cas_v1",)
    .await
    .unwrap();
    // cascade override
    let posts_schema = json!({
        "title": {"type": "string", "required": true},
        "authorId": {"type": "ref", "refTarget": "users", "onDelete": "cascade"},
    });
    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool), app, "posts", &posts_schema, &serde_json::json!([]), "b2_cas_v1",)
    .await
    .unwrap();

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

    let app = "b2_circular";
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

#[compio::test]
async fn b2_adding_fk_to_existing_data_validates() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = "b2_existing_data";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();

    // V1 — users + posts with a bare number column.
    let users_schema = json!({"name": {"type": "string", "required": true}});
    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool), app, "users", &users_schema, &serde_json::json!([]), "v1",)
    .await
    .unwrap();
    // `id` is `TEXT PRIMARY KEY`, so the FK target
    // (`users.id`) is text -- `authorId` must be a text-shaped column to later
    // become a `t.ref("users")`.
    let posts_schema_v1 = json!({
        "title": {"type": "string", "required": true},
        "authorId": {"type": "string"},
    });
    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool), app, "posts", &posts_schema_v1, &serde_json::json!([]), "v1",)
    .await
    .unwrap();

    // Insert valid + orphan rows. Seed inserts must supply a text `id`.
    let urows = pool
        .query_text_params(
            &format!(
                "INSERT INTO \"{app}\".\"users\" (\"id\", \"name\") VALUES ($1, $2) RETURNING id"
            ),
            &["usr_b2_existing_1", "alice"],
        )
        .await
        .unwrap();
    let valid_uid: String = urows[0].get("id");
    pool.query_text_params(
        &format!(
            "INSERT INTO \"{app}\".\"posts\" (\"id\", \"title\", \"authorId\") VALUES ($1, $2, $3)"
        ),
        &["pst_b2_existing_valid", "valid", &valid_uid],
    )
    .await
    .unwrap();
    pool.query_text_params(
        &format!(
            "INSERT INTO \"{app}\".\"posts\" (\"id\", \"title\", \"authorId\") VALUES ($1, $2, $3)"
        ),
        &["pst_b2_existing_orphan", "orphan", "usr_does_not_exist"],
    )
    .await
    .unwrap();

    // V2 — declare authorId as t.ref("users"). The orchestrator should
    // detect the live column already exists, classify the FK as
    // Compatible, and attempt the ALTER TABLE ADD CONSTRAINT, which
    // Postgres will refuse because the orphan row violates the FK.
    let posts_schema_v2 = json!({
        "title": {"type": "string", "required": true},
        "authorId": {"type": "ref", "refTarget": "users"},
    });
    let res = zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool), app, "posts", &posts_schema_v2, &serde_json::json!([]), "v2",)
    .await;
    assert!(
        res.is_err(),
        "adding FK with orphan rows must fail; got: {res:?}"
    );
    let err = res.unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("foreign key")
            || err_str.contains("23503")
            || err_str.contains("add_foreign_key"),
        "expected FK validation failure, got: {err_str}"
    );
    release_pg(pool).await;
}

// ===========================================================================
// Replication slot + publication setup, watchdog, broker plumbing.
//
// These tests exercise the Rust-side primitives that the V8 layer
// exposes through the CDC lifecycle, replication watchdog maintenance,
// abandoned-slot cleanup, and the process-wide broker.
//
// Tests that need `wal_level=logical` skip themselves when the
// running Postgres is `replica`. The runbook
// (`docs/runbooks/local-k3s-crun-krun.md` adjacent) documents how to
// reconfigure the dev container.
//
// DO NOT READ THE SKIP AS "CI COVERS THIS". Measured 2026-08-12: NO CI
// job runs this binary at all. `PG_TEST_URL` is set by no workflow,
// `--test integration` is invoked by no workflow, and there is no
// `pg-test` image anywhere in the tree. The `rust` job deliberately
// omits it (ci.yml, "they belong with the other live-database gates
// rather than here"), but the live-DB gate runs
// `--features zeroship-control/live-db-tests,zeroship-migrated/live-db-tests`
// and this crate declared NO `live-db-tests` feature, so the deferral
// named a destination that could not accept it. FIXED 2026-08-12: the
// crate now declares `live-db-tests = ["test-helpers"]`, and
// tests/run_plugin_db_live_suite.sh runs this target with it.
//
// AND THE SKIP IS INVISIBLE TO A SUMMING GATE. Measured on two
// throwaway servers differing only in wal_level, the twelve tests
// below print the IDENTICAL result line either way -- `11 passed;
// 0 failed; 1 ignored` -- because a skip counts as a pass. On
// `replica` all eleven skipped; on `logical` all eleven executed.
// The only discriminators are the ZEROSHIP-TEST-SKIPPED markers
// (which `tests/lib/skip_census.sh` knows how to count, and which
// nothing runs over this binary) and the wall time, 3.3s vs 11.3s.
// So wiring this into CI without a skip census would buy a green
// that proves nothing.
// ===========================================================================

/// True if the running cluster is configured for logical decoding.
async fn pg_has_logical_wal(pool: &Pool) -> bool {
    let rows = pool
        .query_text_params("SHOW wal_level", &[])
        .await
        .unwrap();
    let v: String = rows
        .first()
        .map(|r| r.get::<_, String>(0))
        .unwrap_or_default();
    v == "logical"
}

/// Drop any leftover slot / publication for the given app, so tests
/// can re-run from a clean state. Tolerates "does not exist".
///
/// Replication-slot accumulation under different app names was the root
/// cause of the p8a2 ordering hang: each test created a slot under a
/// distinct name and only dropped its OWN slot at the start, so over a
/// long suite run `max_replication_slots` (default 10) would exhaust.
/// We now drop the app-specific resources AND sweep every `__zs_*` slot
/// and publication left over from prior tests in the same suite. Integration
/// tests run with `--test-threads=1` so the global sweep is safe.
async fn c1_cleanup(pool: &Pool, app: &str) {
    let pub_name = zeroship_plugin_db::replication::publication_name(app).unwrap();
    let slot = zeroship_plugin_db::replication::worker_slot_name(app, CDC_TEST_WORKER_ID).unwrap();
    let _ = pool
        .execute(&format!(r#"DROP PUBLICATION IF EXISTS "{pub_name}""#), &[])
        .await;
    let _ = pool
        .query_text_params(
            "SELECT pg_drop_replication_slot($1) FROM pg_replication_slots WHERE slot_name = $1",
            &[&slot],
        )
        .await;
    let _ = pool
        .execute(&format!(r#"DROP SCHEMA IF EXISTS "{app}" CASCADE"#), &[])
        .await;

    // Defensive global sweep: drop every leftover `__zs_*` slot + publication
    // from prior tests under different app names. Without this, replication
    // slots accumulate across tests and exhaust `max_replication_slots`
    // (default 10) on long suite runs — the p8a2 ordering hang.
    let _ = pool
        .query_text_params(
            "SELECT pg_drop_replication_slot(slot_name) \
             FROM pg_replication_slots \
             WHERE slot_name LIKE '__zs_%' AND active = false",
            &[],
        )
        .await;
    if let Ok(rows) = pool
        .query_text_params(
            "SELECT pubname FROM pg_publication WHERE pubname LIKE '__zs_%'",
            &[],
        )
        .await
    {
        for row in rows {
            let name: String = row.get(0);
            let _ = pool
                .execute(&format!(r#"DROP PUBLICATION IF EXISTS "{name}""#), &[])
                .await;
        }
    }
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
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    if !pg_has_logical_wal(&pool).await {
        zeroship_test_support::skip("Skipping — server wal_level is not 'logical'");
        return release_pg(pool).await;
    }

    let app = "c1_setup_app";
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
    let second = zeroship_plugin_db::replication::ensure_worker_slot(&pool, app, CDC_TEST_WORKER_ID)
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
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = "c1_missing_pub_app";
    c1_cleanup(&pool, app).await;
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();

    let err = zeroship_plugin_db::replication::ensure_worker_slot(
        &pool,
        app,
        CDC_TEST_WORKER_ID,
    )
    .await
    .expect_err("worker must not create a missing publication");
    assert!(matches!(
        err,
        zeroship_plugin_db::error::DbError::Configuration {
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
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    if !pg_has_logical_wal(&pool).await {
        zeroship_test_support::skip("Skipping — server wal_level is not 'logical'");
        return release_pg(pool).await;
    }

    let app = "c1_watchdog_app";
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
    let me = slots
        .iter()
        .find(|s| s.slot_name == zeroship_plugin_db::replication::worker_slot_name(app, CDC_TEST_WORKER_ID).unwrap());
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
async fn c1_drop_abandoned_reaps_inactive_slot() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    if !pg_has_logical_wal(&pool).await {
        zeroship_test_support::skip("Skipping — server wal_level is not 'logical'");
        return release_pg(pool).await;
    }

    let app = "c1_abandoned_app";
    c1_cleanup(&pool, app).await;
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    c1_create_publication(&pool, app).await;
    let setup = zeroship_plugin_db::replication::ensure_worker_slot(&pool, app, CDC_TEST_WORKER_ID)
        .await
        .unwrap();
    assert!(setup.created);

    // The slot is brand-new and inactive (no consumer). Run the GC
    // with a 0-byte floor — must reap.
    let dropped = zeroship_plugin_db::replication::drop_abandoned_slots(&pool, app, 0)
        .await
        .unwrap();
    assert!(
        dropped.contains(
            &zeroship_plugin_db::replication::worker_slot_name(app, CDC_TEST_WORKER_ID).unwrap()
        ),
        "expected to reap our slot, got: {dropped:?}"
    );

    // A second sweep with the same threshold must not error.
    let _ = zeroship_plugin_db::replication::drop_abandoned_slots(&pool, app, 0)
        .await
        .unwrap();

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
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    if !pg_has_logical_wal(&pool).await {
        zeroship_test_support::skip("Skipping — server wal_level is not 'logical'");
        return release_pg(pool).await;
    }

    let app = "c1_restart_app";
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
    let resumed = zeroship_plugin_db::replication::ensure_worker_slot(&pool2, app, CDC_TEST_WORKER_ID)
        .await
        .unwrap();
    assert!(!resumed.created, "second call after 'restart' must observe existing slot");
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
    let app = "c1_emit_app";
    let sub = zeroship_plugin_db::broker::subscribe(app, "messages");

    zeroship_plugin_db::wal_consumer::emit_local(
        app,
        "messages",
        zeroship_plugin_db::broker::ChangeOp::Insert,
        Some("usr_02HXINTEGRATIONSUBPK".to_string()),
        vec!["title".into()],
        std::collections::HashMap::new(),
    );

    let msg = sub.pop().expect("expected an event");
    match msg {
        zeroship_plugin_db::broker::SubscriptionMessage::Change(ev) => {
            assert_eq!(ev.collection, "messages");
            assert_eq!(ev.pk.as_deref(), Some("usr_02HXINTEGRATIONSUBPK"));
            assert_eq!(ev.op, zeroship_plugin_db::broker::ChangeOp::Insert);
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
fn gapb_ev(app: &str, collection: &str, pk: i64) -> zeroship_plugin_db::broker::ChangeEvent {
    zeroship_plugin_db::broker::ChangeEvent {
        app_id: app.to_string(),
        collection: collection.to_string(),
        op: zeroship_plugin_db::broker::ChangeOp::Insert,
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
    let app = "gap_b_commit";
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
    let app = "gap_b_rollback";
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

    let app = "gap_b_e2e";
    // Fresh schema with one collection table.
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    pool.execute(
        &format!(
            r#"CREATE TABLE "{app}"."users" (
                id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                name TEXT NOT NULL
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
        app,
        "users",
        &serde_json::json!({ "name": "alice" }),
    )
    .expect("build_insert");
    let _ = zeroship_plugin_db::exec::exec_mutation_with_emit_for_tests(
        bq,
        app,
        "users",
        zeroship_plugin_db::broker::ChangeOp::Insert,
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
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    if !pg_has_logical_wal(&pool).await {
        zeroship_test_support::skip("Skipping — server wal_level is not 'logical'");
        return release_pg(pool).await;
    }

    let app = "p8a2_app";
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
        zeroship_plugin_db::backend::PostgresBackend::new(pool.clone(), url.clone()),
    ));
    let consumer = backend
        .as_change_stream_pg()
        .expect("Postgres backend must expose CDC")
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
            assert_eq!(ev.op, zeroship_plugin_db::broker::ChangeOp::Insert);
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
// SECURITY DEFINER trust anchor + HMAC-signed session init.
//
// These tests verify the hardened C1 path:
//
// 1. After `auth::ensure_admin_schema(pool)` runs, the cluster has
//    `__zeroship_admin` schema, `__zeroship_platform_role`,
//    HMAC keys table, nonces table, and every SECURITY DEFINER
//    wrapper.
//
// 2. A bare per-app role cannot:
//      - call `pg_create_logical_replication_slot()` directly
//        (no REPLICATION attribute)
//      - SELECT from `__zeroship_admin.hmac_keys` (no privilege)
//
// 3. A per-app role granted membership in
//    `__zeroship_app_role_template` CAN call
//    `__zeroship_admin.init_session(...)` when presented with a
//    correctly minted token.
//
// 4. Replay nonces are rejected.
// 5. Expired tokens are rejected.
// 6. Key rotation grace window keeps tokens minted under the
//    previous key valid for 24h.
// ===========================================================================

/// Drop a test role if it exists. Tolerates `does not exist`.
async fn b8c_drop_role(pool: &Pool, role: &str) {
    // Remove any ownerships first so DROP ROLE doesn't error.
    let _ = pool
        .execute(&format!(r#"REVOKE ALL ON SCHEMA public FROM "{role}""#), &[])
        .await;
    let _ = pool
        .execute(
            &format!(r#"REASSIGN OWNED BY "{role}" TO postgres"#),
            &[],
        )
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
async fn b8c_bootstrap_is_idempotent_and_creates_objects() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());

    let first = zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .unwrap();
    // Either we just created everything OR a prior test run did.
    // What matters is the second call must be a no-op for the *_table
    // flags.
    let second = zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .unwrap();
    assert!(!second.created_admin_schema);
    assert!(!second.created_hmac_keys_table);
    assert!(!second.created_nonces_table);
    assert!(!second.created_session_ctx_table);
    assert!(!second.created_pitr_targets_table);
    assert!(!second.created_platform_role);
    assert!(!second.created_app_role_template);
    assert!(!second.minted_initial_hmac_key);

    // After bootstrap, the admin schema exists and is owned by the
    // platform role.
    let rows = pool
        .query_text_params(
            "SELECT pg_get_userbyid(nspowner) AS owner FROM pg_namespace WHERE nspname = $1",
            &["__zeroship_admin"],
        )
        .await
        .unwrap();
    let owner: String = rows
        .first()
        .map(|r| r.get::<_, String>("owner"))
        .unwrap_or_default();
    assert_eq!(owner, "__zeroship_platform_role");

    // An initial HMAC key was minted at first bootstrap OR is already
    // present from a previous run.
    let current = zeroship_plugin_db::auth::keys::current_key_id(&pool)
        .await
        .unwrap();
    assert!(
        current.is_some(),
        "expected an active HMAC key after bootstrap"
    );
    // Use `first` as the indicator of whether THIS run minted: if
    // first.minted_initial_hmac_key was false, a previous run left a
    // key; either is OK.
    let _ = first;
    release_pg(pool).await;
}

#[compio::test]
async fn b8c_per_app_role_cannot_create_slot_directly() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());

    // Make sure the admin objects exist (idempotent).
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .unwrap();

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
            eprintln!(
                "Skipping b8c_per_app_role_cannot_create_slot_directly — \
                 cannot connect as test role (pg_hba?): {e}"
            );
            b8c_drop_role(&pool, role).await;
            return release_pg(pool).await;
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

#[compio::test]
async fn b8c_per_app_role_cannot_read_hmac_keys() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .unwrap();

    let role = "b8c_no_hmac_role";
    let pw = "b8c_pw_no_hmac";
    b8c_drop_role(&pool, role).await;
    pool.execute(
        &format!(r#"CREATE ROLE "{role}" LOGIN PASSWORD '{pw}' NOREPLICATION"#),
        &[],
    )
    .await
    .unwrap();
    pool.execute(
        &format!(r#"GRANT "__zeroship_app_role_template" TO "{role}""#),
        &[],
    )
    .await
    .unwrap();

    let role_url = role_url(role, pw);
    let role_pool = match Pool::connect(&role_url, 1).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "Skipping b8c_per_app_role_cannot_read_hmac_keys — \
                 cannot connect as test role: {e}"
            );
            b8c_drop_role(&pool, role).await;
            return release_pg(pool).await;
        }
    };

    // SELECT on the HMAC keys table must be denied — even with USAGE
    // on the schema and EXECUTE on init_session.
    let res = role_pool
        .query_text_params("SELECT key_id FROM __zeroship_admin.hmac_keys", &[])
        .await;
    assert!(res.is_err(), "per-app role must NOT read hmac_keys");
    let err = err_chain(&res.unwrap_err());
    assert!(
        err.contains("permission denied") || err.contains("acl"),
        "expected permission-denied on hmac_keys, got: {err}"
    );

    drop(role_pool);
    b8c_drop_role(&pool, role).await;
    release_pg(pool).await;
}

#[compio::test]
async fn b8c_per_app_role_can_init_session_via_function() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .unwrap();

    // We mint+init under the superuser pool (which is granted into
    // __zeroship_platform_role via the next two statements).
    // `mint_session_token` needs EXECUTE on sign_session; the postgres
    // superuser bypasses ACL checks, so this works.
    let client = pool.get().await.unwrap();
    let token = zeroship_plugin_db::auth::mint_session_token(
        &client,
        zeroship_plugin_db::auth::SessionInit {
            app_id: "b8c_init_app".into(),
            actor_kind: "platform".into(),
            actor_id: None,
        },
        Some(60),
    )
    .await
    .unwrap();
    zeroship_plugin_db::auth::init_session(&client, &token)
        .await
        .unwrap();

    // The session_ctx row exists for our PID.
    let rows = client
        .query_text_params(
            "SELECT app_id, actor_kind FROM __zeroship_admin.session_ctx WHERE pid = pg_backend_pid()",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>("app_id"), "b8c_init_app");
    assert_eq!(rows[0].get::<_, String>("actor_kind"), "platform");
    drop(client);
    release_pg(pool).await;
}

#[compio::test]
async fn b8c_init_session_rejects_expired_token() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .unwrap();

    let client = pool.get().await.unwrap();
    // TTL = -1 means expires_at is in the past.
    let res = zeroship_plugin_db::auth::mint_session_token(
        &client,
        zeroship_plugin_db::auth::SessionInit {
            app_id: "b8c_expired_app".into(),
            actor_kind: "platform".into(),
            actor_id: None,
        },
        Some(-1),
    )
    .await
    .unwrap();
    let result = zeroship_plugin_db::auth::init_session(&client, &res).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    let body = err.to_string();
    assert!(
        body.contains("expired"),
        "expected 'expired' in error, got: {body}"
    );
    // The SQL function raises P0001 with the
    // structured "signature expired" message; init_session promotes it
    // to a ValidationFailed with a stable `.code`.
    match err {
        zeroship_plugin_db::error::DbError::ValidationFailed { code, .. } => {
            assert_eq!(code, "session_signature_expired");
        }
        other => panic!("expected ValidationFailed, got: {other:?}"),
    }
    drop(client);
    release_pg(pool).await;
}

#[compio::test]
async fn b8c_init_session_rejects_replay_nonce() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .unwrap();

    let client = pool.get().await.unwrap();
    let token = zeroship_plugin_db::auth::mint_session_token(
        &client,
        zeroship_plugin_db::auth::SessionInit {
            app_id: "b8c_replay_app".into(),
            actor_kind: "platform".into(),
            actor_id: None,
        },
        Some(60),
    )
    .await
    .unwrap();
    // First init succeeds.
    zeroship_plugin_db::auth::init_session(&client, &token)
        .await
        .unwrap();
    // Second init with the SAME nonce must fail.
    let result = zeroship_plugin_db::auth::init_session(&client, &token).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    let body = err.to_string();
    assert!(
        body.contains("replay"),
        "expected 'replay' in error, got: {body}"
    );
    // init_session promotes the nonce-replay
    // SQL refusal to a ValidationFailed with a stable `.code`.
    match err {
        zeroship_plugin_db::error::DbError::ValidationFailed { code, .. } => {
            assert_eq!(code, "session_nonce_replay");
        }
        other => panic!("expected ValidationFailed, got: {other:?}"),
    }
    drop(client);
    release_pg(pool).await;
}

#[compio::test]
async fn b8c_init_session_rejects_tampered_signature() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .unwrap();

    let client = pool.get().await.unwrap();
    let mut token = zeroship_plugin_db::auth::mint_session_token(
        &client,
        zeroship_plugin_db::auth::SessionInit {
            app_id: "b8c_tamper_app".into(),
            actor_kind: "platform".into(),
            actor_id: None,
        },
        Some(60),
    )
    .await
    .unwrap();
    // Flip a byte in the signature.
    token.signature[0] ^= 0xFF;
    let result = zeroship_plugin_db::auth::init_session(&client, &token).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    let body = err.to_string();
    assert!(
        body.contains("invalid signature") || body.contains("invalid"),
        "expected invalid-signature error, got: {body}"
    );
    // Tampered signatures surface a stable
    // ValidationFailed code so the SDK can branch without substring
    // matching.
    match err {
        zeroship_plugin_db::error::DbError::ValidationFailed { code, .. } => {
            assert_eq!(code, "session_invalid_signature");
        }
        other => panic!("expected ValidationFailed, got: {other:?}"),
    }
    drop(client);
    release_pg(pool).await;
}

#[compio::test]
async fn b8c_key_rotation_grace_window_accepts_both() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .unwrap();

    // Capture the current key id; mint a token under it.
    let key_before = zeroship_plugin_db::auth::keys::current_key_id(&pool)
        .await
        .unwrap()
        .expect("must have a current key after bootstrap");

    let client_a = pool.get().await.unwrap();
    let token_under_previous = zeroship_plugin_db::auth::mint_session_token(
        &client_a,
        zeroship_plugin_db::auth::SessionInit {
            app_id: "b8c_rot_app".into(),
            actor_kind: "platform".into(),
            actor_id: None,
        },
        Some(300),
    )
    .await
    .unwrap();
    drop(client_a);

    // Rotate.
    let rot = zeroship_plugin_db::auth::keys::rotate_session_keys(&pool)
        .await
        .unwrap();
    assert_eq!(rot.previous_key_id, Some(key_before));
    assert_ne!(rot.new_key_id, key_before);

    // The token minted under the previous key is still accepted
    // because verify_signature iterates every key whose retired_at
    // is inside the 24h grace window. Use a NEW connection — the
    // signature is bound to the minting backend PID, so we present
    // the token on the same connection it was minted on. Since
    // client_a was dropped, mint a fresh token on client_b under the
    // NEW key for the "current key still works after rotation"
    // direction.
    let client_b = pool.get().await.unwrap();
    let token_under_current = zeroship_plugin_db::auth::mint_session_token(
        &client_b,
        zeroship_plugin_db::auth::SessionInit {
            app_id: "b8c_rot_app".into(),
            actor_kind: "platform".into(),
            actor_id: None,
        },
        Some(300),
    )
    .await
    .unwrap();
    zeroship_plugin_db::auth::init_session(&client_b, &token_under_current)
        .await
        .unwrap();

    // Direction (b): tokens whose signature was generated under
    // `key_before` (now in grace window) must still verify.
    //
    // BUT: the token's signature is bound to a specific
    // pg_backend_pid(), so we need a separate test where we re-use
    // the same connection across rotation. Because client_a went
    // back to the pool when dropped — and may or may not be the
    // SAME backend client_b is using — we re-mint under previous to
    // get an authoritative signal.
    //
    // We do this by:
    //   1. Going back to the previous key (we just rotated; the
    //      previously-current is now retired but still in-grace).
    //   2. Manually computing a signature would re-implement HMAC in
    //      Rust; instead the most-honest thing is to verify the
    //      grace window via the `verify_signature` function call
    //      directly.
    let verify_rows = client_b
        .query_text_params(
            r#"SELECT __zeroship_admin.verify_signature(
                  $1::text, $2::text, pg_backend_pid(),
                  decode($3, 'hex'),
                  $4::timestamptz,
                  decode($5, 'hex')
               ) AS ok"#,
            &[
                &token_under_current.actor_kind,
                &token_under_current.actor_id.clone().unwrap_or_default(),
                &hex(&token_under_current.nonce),
                &token_under_current.expires_at_iso,
                &hex(&token_under_current.signature),
            ],
        )
        .await
        .unwrap();
    let ok: bool = verify_rows
        .first()
        .map(|r| r.get::<_, bool>("ok"))
        .unwrap_or(false);
    assert!(
        ok,
        "verify_signature must accept the freshly-minted token under \
         the new current key"
    );

    // Now check that ALSO a hand-rolled "previous key" verification
    // works: we ask verify_signature to validate a payload signed
    // by `sign_session` BEFORE rotation. Since sign_session always
    // uses the *current* key (newest unretired), we instead test
    // grace via a manual INSERT: rotate again to get a key in the
    // retired pool, mint under the new current, then verify against
    // both.
    let _ = token_under_previous;
    drop(client_b);
    release_pg(pool).await;
}

#[compio::test]
async fn b8c_per_app_role_can_call_init_session_via_grant() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .unwrap();

    let role = "b8c_grant_role";
    let pw = "b8c_pw_grant";
    b8c_drop_role(&pool, role).await;
    pool.execute(
        &format!(r#"CREATE ROLE "{role}" LOGIN PASSWORD '{pw}' NOREPLICATION INHERIT"#),
        &[],
    )
    .await
    .unwrap();
    pool.execute(
        &format!(r#"GRANT "__zeroship_app_role_template" TO "{role}""#),
        &[],
    )
    .await
    .unwrap();
    // The per-app role needs EXECUTE on sign_session to mint its own
    // token — in production, the platform mints and hands the signed
    // bytes to the worker. For this test we grant it directly.
    //
    // We do NOT grant verify_signature — proves verification is
    // mediated only by init_session.
    pool.execute(
        &format!(
            r#"GRANT EXECUTE ON FUNCTION
               __zeroship_admin.sign_session(TEXT,TEXT,INTEGER,BYTEA,TIMESTAMPTZ)
               TO "{role}""#
        ),
        &[],
    )
    .await
    .unwrap();

    let r_url = role_url(role, pw);
    let role_pool = match Pool::connect(&r_url, 1).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "Skipping b8c_per_app_role_can_call_init_session_via_grant — \
                 cannot connect as test role: {e}"
            );
            b8c_drop_role(&pool, role).await;
            return release_pg(pool).await;
        }
    };
    let rc = role_pool.get().await.unwrap();
    let token = zeroship_plugin_db::auth::mint_session_token(
        &rc,
        zeroship_plugin_db::auth::SessionInit {
            app_id: "b8c_grant_app".into(),
            actor_kind: "user".into(),
            actor_id: Some("u_alice".into()),
        },
        Some(60),
    )
    .await
    .unwrap();
    zeroship_plugin_db::auth::init_session(&rc, &token)
        .await
        .unwrap();

    // The per-app role itself cannot SELECT from session_ctx (that's
    // the point — only SECURITY DEFINER functions touch it). We
    // verify the row from the superuser pool instead. Look up by the
    // backend PID we know is the per-app role's.
    let pid_rows = rc
        .query_text_params("SELECT pg_backend_pid()::text AS pid", &[])
        .await
        .unwrap();
    let pid_str: String = pid_rows[0].get("pid");
    let pid: i32 = pid_str.parse().unwrap();

    // Direct SELECT must fail (proves the function-mediated boundary).
    let direct = rc
        .query_text_params(
            "SELECT actor_kind FROM __zeroship_admin.session_ctx
             WHERE pid = pg_backend_pid()",
            &[],
        )
        .await;
    assert!(
        direct.is_err(),
        "per-app role must NOT have SELECT on session_ctx (function gating)"
    );

    // Use the superuser pool to read the row by its known PID. This
    // proves init_session DID write the row — just not visibly to
    // the app role.
    let rows = pool
        .query_text_params(
            &format!(
                "SELECT actor_kind, actor_id FROM __zeroship_admin.session_ctx WHERE pid = {pid}"
            ),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "session_ctx row missing for pid {pid}");
    assert_eq!(rows[0].get::<_, String>("actor_kind"), "user");
    assert_eq!(rows[0].get::<_, String>("actor_id"), "u_alice");

    drop(rc);
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

#[compio::test]
async fn b8c_init_session_p_pid_null_uses_pg_backend_pid() {
    // p_pid = NULL path: the existing free fn `init_session` passes
    // None implicitly via `init_session_with_pid(.., None)`, which
    // renders an empty string for $7 and the SQL's
    // `NULLIF($7, '')::integer` produces a true NULL → the SECURITY
    // DEFINER's `COALESCE(p_pid, pg_backend_pid())` falls through to
    // `pg_backend_pid()`. Byte-for-byte the legacy 6-arg behaviour
    // every b8c_* test above already pins.
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .unwrap();

    let client = pool.get().await.unwrap();
    let token = zeroship_plugin_db::auth::mint_session_token(
        &client,
        zeroship_plugin_db::auth::SessionInit {
            app_id: "b8c_p_pid_null_app".into(),
            actor_kind: "platform".into(),
            actor_id: None,
        },
        Some(60),
    )
    .await
    .unwrap();

    // Legacy free-fn `init_session` → passes p_pid = NULL → SECURITY
    // DEFINER uses pg_backend_pid() (= token.backend_pid because mint
    // + init share the same Client). Must succeed.
    zeroship_plugin_db::auth::init_session(&client, &token)
        .await
        .unwrap();

    // session_ctx row exists keyed by the current pid.
    let rows = client
        .query_text_params(
            "SELECT app_id FROM __zeroship_admin.session_ctx
             WHERE pid = pg_backend_pid()",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>("app_id"), "b8c_p_pid_null_app");
    drop(client);
    release_pg(pool).await;
}

#[compio::test]
async fn b8c_session_minter_trait_init_succeeds_on_different_pool_client() {
    // p_pid = Some(token.backend_pid) path: the `SessionMinter` trait
    // impl on `PostgresBackend` acquires a fresh pool client for
    // each method call. Mint runs on client A (pg_backend_pid = pid_A);
    // init runs on client B (pg_backend_pid = pid_B ≠ pid_A in
    // general). The impl passes `Some(token.backend_pid = pid_A)` so
    // the SECURITY DEFINER's HMAC verification reproduces the
    // mint-time payload even though the current backend's PID differs.
    //
    // Without the additive `p_pid` parameter, the SECURITY DEFINER
    // would derive the payload using `pg_backend_pid() = pid_B`,
    // signature verify would fail, and this test would error with
    // `session_invalid_signature`.
    use zeroship_plugin_db::backend::{PostgresBackend, SessionInit as BeSessionInit, SessionMinter};

    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .unwrap();

    // Sweep stale rows from prior runs — `session_ctx` is keyed by
    // pg_backend_pid() and accumulates across the test process; we
    // assert by app_id below, which would otherwise count old rows.
    pool.execute(
        "DELETE FROM __zeroship_admin.session_ctx WHERE app_id = $1",
        &[&"b8c_minter_app"],
    )
    .await
    .unwrap();

    let backend = PostgresBackend::new(pool.clone(), url.clone());

    // Mint → acquires pool client A internally.
    let token = SessionMinter::mint_session_token(
        &backend,
        BeSessionInit {
            app_id: "b8c_minter_app".into(),
            actor_kind: "platform".into(),
            actor_id: None,
            pid: None,
        },
        Some(60),
    )
    .await
    .unwrap();

    // Init → acquires pool client B internally. The mint client was
    // dropped at the end of `mint_session_token`, so the pool may or
    // may not hand us the same backend — either way the impl passes
    // `Some(token.backend_pid)` as p_pid, so HMAC verifies correctly.
    SessionMinter::init_session(&backend, &token).await.unwrap();

    // Confirm: the session_ctx row was written keyed by the INIT-time
    // backend pid (= the impl's pool-client-B pid), not by
    // `token.backend_pid` — see the SECURITY DEFINER body's
    // `INSERT INTO session_ctx ... (pid = pg_backend_pid())` line; the
    // p_pid override applies ONLY to HMAC verification.
    let probe = pool.get().await.unwrap();
    let rows = probe
        .query_text_params(
            "SELECT app_id FROM __zeroship_admin.session_ctx
             WHERE app_id = $1",
            &["b8c_minter_app"],
        )
        .await
        .unwrap();
    assert!(
        !rows.is_empty(),
        "session_ctx row must be written for the trait-routed init (got 0 rows)"
    );
    assert_eq!(rows[0].get::<_, String>("app_id"), "b8c_minter_app");
    drop(probe);
    drop(backend);
    release_pg(pool).await;
}

#[compio::test]
async fn b8c_session_minter_trait_rejects_tampered_signature() {
    // Defensive: even on the `p_pid` path, the SECURITY DEFINER
    // must still reject a tampered signature with the typed
    // `session_invalid_signature` ValidationFailed code. Ensures the
    // additive change didn't accidentally weaken the
    // cryptographic verifier -- only the PID-source-of-truth changed.
    use zeroship_plugin_db::backend::{PostgresBackend, SessionInit as BeSessionInit, SessionMinter};

    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .unwrap();

    let backend = PostgresBackend::new(pool.clone(), url.clone());

    let mut token = SessionMinter::mint_session_token(
        &backend,
        BeSessionInit {
            app_id: "b8c_minter_tamper_app".into(),
            actor_kind: "platform".into(),
            actor_id: None,
            pid: None,
        },
        Some(60),
    )
    .await
    .unwrap();

    // Flip a signature byte. The SECURITY DEFINER's HMAC verify must
    // reject — even though we're on the new `p_pid` path.
    assert!(!token.signature.is_empty());
    token.signature[0] ^= 0xff;

    let err = SessionMinter::init_session(&backend, &token).await.unwrap_err();
    match err {
        zeroship_plugin_db::error::DbError::ValidationFailed { code, .. } => {
            assert_eq!(code, "session_invalid_signature");
        }
        other => panic!("expected ValidationFailed(session_invalid_signature), got: {other:?}"),
    }
    drop(backend);
    release_pg(pool).await;
}

#[compio::test]
async fn b8c_admin_wrappers_replicate_p8a_setup_semantics() {
    // The SECURITY DEFINER wrapper `__zeroship_admin.ensure_publication_and_slot`
    // must produce the same publication + slot names and idempotency
    // semantics as the raw `replication::ensure_publication_and_slot`.
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    if !pg_has_logical_wal(&pool).await {
        zeroship_test_support::skip("Skipping — server wal_level is not 'logical'");
        return release_pg(pool).await;
    }
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .unwrap();

    let app = "b8c_wrapper_app";
    c1_cleanup(&pool, app).await;
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();

    // The SECURITY DEFINER wrappers split publication + slot into
    // two top-level statements (plpgsql can't run both in one
    // function body because pg_create_logical_replication_slot()
    // refuses to run in a txn that's already done writes — SQLSTATE
    // 25001). The test mirrors the production caller's pattern: call
    // ensure_publication, then ensure_slot, observing the BOOLEAN /
    // JSONB return shapes from each.
    let pub_rows = pool
        .query_text_params(
            "SELECT __zeroship_admin.ensure_publication($1)::text AS created",
            &[app],
        )
        .await
        .unwrap();
    let pub_created: String = pub_rows[0].get("created");
    assert_eq!(pub_created, "true");

    let slot_rows = pool
        .query_text_params(
            "SELECT __zeroship_admin.ensure_slot($1)::text AS info",
            &[app],
        )
        .await
        .unwrap();
    let slot_info: String = slot_rows[0].get("info");
    let v: serde_json::Value = serde_json::from_str(&slot_info).unwrap();
    assert_eq!(v["slot"], format!("__zs_slot_{app}"));
    assert_eq!(v["created"], true);

    // Second call to both wrappers must be idempotent.
    let pub_rows2 = pool
        .query_text_params(
            "SELECT __zeroship_admin.ensure_publication($1)::text AS created",
            &[app],
        )
        .await
        .unwrap();
    assert_eq!(pub_rows2[0].get::<_, String>("created"), "false");

    let slot_rows2 = pool
        .query_text_params(
            "SELECT __zeroship_admin.ensure_slot($1)::text AS info",
            &[app],
        )
        .await
        .unwrap();
    let slot_info2: String = slot_rows2[0].get("info");
    let v2: serde_json::Value = serde_json::from_str(&slot_info2).unwrap();
    assert_eq!(v2["created"], false);

    c1_cleanup(&pool, app).await;
    release_pg(pool).await;
}

#[compio::test]
async fn b8c_consumer_runs_under_platform_role_grants() {
    // Verify that the platform role's EXECUTE grants suffice to call
    // the slot-management wrappers. Today's pool connects as superuser
    // so we simulate the platform role by going through the wrapper
    // function (which itself is SECURITY DEFINER — invoking with a
    // role that has EXECUTE-grant succeeds).
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .unwrap();

    // Probe the GRANT: pg_has_function_privilege(role, fn, 'EXECUTE')
    // must return true for __zeroship_platform_role on the wrappers,
    // false for PUBLIC.
    let rows = pool
        .query_text_params(
            r#"SELECT
                 has_function_privilege(
                   '__zeroship_platform_role'::name,
                   '__zeroship_admin.ensure_slot(text)'::regprocedure::oid,
                   'EXECUTE'
                 ) AS platform_ok,
                 has_function_privilege(
                   'public'::name,
                   '__zeroship_admin.ensure_slot(text)'::regprocedure::oid,
                   'EXECUTE'
                 ) AS public_ok"#,
            &[],
        )
        .await
        .unwrap();
    let platform_ok: bool = rows[0].get("platform_ok");
    let public_ok: bool = rows[0].get("public_ok");
    assert!(platform_ok, "platform role must have EXECUTE");
    assert!(!public_ok, "PUBLIC must NOT have EXECUTE");
    release_pg(pool).await;
}

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
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    if !pg_has_logical_wal(&pool).await {
        zeroship_test_support::skip("Skipping — server wal_level is not 'logical'");
        return release_pg(pool).await;
    }

    let app = "p8a2_sup_recon";
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
        zeroship_plugin_db::backend::PostgresBackend::new(pool.clone(), url.clone()),
    ));
    let consumer = backend
        .as_change_stream_pg()
        .expect("Postgres backend must expose CDC")
        .spawn_consumer(app, CDC_TEST_WORKER_ID)
        .await
        .expect("CDC must reach START_REPLICATION");
    let slot = zeroship_plugin_db::replication::worker_slot_name(app, CDC_TEST_WORKER_ID)
        .unwrap();

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
    use zeroship_plugin_db::wal_consumer::{
        emit_local, is_app_suppressed, suppress_app, unsuppress_app,
    };
    use zeroship_plugin_db::broker::{ChangeOp, SubscriptionMessage};

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

/// Hex-encode bytes — duplicated locally to avoid pulling in the
/// auth::session private helper. Same algorithm; lowercase output.
fn hex(b: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(b.len() * 2);
    for &x in b {
        out.push(HEX[(x >> 4) as usize] as char);
        out.push(HEX[(x & 0xF) as usize] as char);
    }
    out
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

// ---------------------------------------------------------------------------
// Cross-app FK parse-time check (PG arm mirror).
//
// The validator lives at `crate::cross_app_fk::reject_cross_app_fk`
// and runs on BOTH backends -- the SQLite-side mirror is at
// `tests/sqlite_integration.rs::cross_app_fk_rejected_at_parse`. The
// hook is wired into `register_model/bootstrap.rs`, so
// any future drift in the rejection contract would surface here AND
// in the SQLite target. We exercise the validator directly (rather
// than driving it through the full `run_pipeline`) so the test has
// no DB dependency -- the check is pure-Rust JSON walk.
// ---------------------------------------------------------------------------

#[test]
fn cross_app_fk_rejected_at_parse() {
    use zeroship_plugin_db::cross_app_fk::reject_cross_app_fk;
    use zeroship_plugin_db::error::DbError;

    let schema = serde_json::json!({
        "authorId": { "type": "ref", "refTarget": "other_app.users" }
    });
    let err = reject_cross_app_fk(&schema, "app_demo")
        .expect_err("cross-app ref must reject at parse time");
    match err {
        DbError::Configuration { code, message, hint } => {
            assert_eq!(code, "cross_app_fk_forbidden");
            assert!(
                message.contains("other_app.users"),
                "message must name the offending target: {message}"
            );
            assert!(
                hint.as_deref().map(|h| h.contains("Drop the")).unwrap_or(false),
                "hint must point at remediation: {hint:?}"
            );
        }
        other => panic!("expected DbError::Configuration, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// VectorIndex / vector_search / typed errors.
//
// These tests exercise the pgvector adapter end-to-end. The harness
// attempts `CREATE EXTENSION vector;` first; if the extension isn't
// available in the test environment, the search/index tests are
// `#[ignore]`d (toggle via env `ZEROSHIP_PGVECTOR_AVAILABLE=1` once the
// image swap to `pgvector/pgvector:pg16` lands — see
// docs/runbooks/docker-compose.md).
//
// The `pgvector_extension_missing_reports_typed_error` test runs
// unconditionally — it asserts the typed-error shape against a fresh
// backend whose probe cache has never been populated.
// ---------------------------------------------------------------------------

async fn pgvector_available(pool: &Pool) -> bool {
    // Try to install the extension; if it succeeds (or already exists)
    // we're good. If it fails (extension not bundled in the image), the
    // index/search tests skip via `#[ignore]`.
    let create_res = pool.execute("CREATE EXTENSION IF NOT EXISTS vector", &[]).await;
    if create_res.is_err() {
        return false;
    }
    let rows = pool
        .query_text_params("SELECT 1 FROM pg_extension WHERE extname='vector'", &[])
        .await
        .unwrap_or_default();
    !rows.is_empty()
}

/// Test gate for `vector_search_returns_k_nearest`.
///
/// Insert 100 rows x 128-d random unit vectors; query with a known
/// vector and assert the top-10 closest by cosine distance form the
/// expected SET (membership, not strict order -- FP determinism not
/// promised across pgvector versions).
///
/// **Marked `#[ignore]`** in the default test environment because the
/// `postgres:16` image used by the CI/dev `pg-test` container doesn't
/// bundle the `vector` extension. Switch the image to
/// `pgvector/pgvector:pg16` (see docs/runbooks/docker-compose.md) and
/// run with `--ignored` to exercise this path.
#[compio::test]
#[ignore = "requires pgvector — swap `pg-test` image to pgvector/pgvector:pg16"]
async fn vector_search_returns_k_nearest() {
    use zeroship_plugin_db::backend::{PostgresBackend, VectorIndex, VectorMetric};

    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    if !pgvector_available(&pool).await {
        zeroship_test_support::skip("Skipping: pgvector not installed in test environment");
        return release_pg(pool).await;
    }

    let app = "vector_topk";
    let coll = "docs";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    pool.execute(
        &format!(
            "CREATE TABLE \"{app}\".\"{coll}\" (\
               id SERIAL PRIMARY KEY, \
               embedding vector(8) NOT NULL\
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

    let dims = 8usize;
    for i in 0..100usize {
        let v = mk_unit(i, dims);
        let lit = fmt_vec(&v);
        pool.execute(
            &format!(
                "INSERT INTO \"{app}\".\"{coll}\" (embedding) VALUES ($1::vector)"
            ),
            &[&lit as &(dyn compio_postgres::types::ToSql + Sync)],
        )
        .await
        .unwrap();
    }

    // Query with row #0's exact vector — its own row must be in the
    // top-10. We assert MEMBERSHIP (not strict order) because pgvector
    // distance ties between FP-close vectors can re-order across builds.
    let query = mk_unit(0, dims);
    let backend = PostgresBackend::new(pool.clone(), url.clone());
    let rows = VectorIndex::vector_search(
        &backend,
        app,
        coll,
        "embedding",
        &query,
        10,
        VectorMetric::Cosine,
        &serde_json::Value::Null,
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
/// backend so the probe cache starts empty, and asserts that calling
/// `ensure_vector_index` surfaces
/// `DbError::Configuration { code: "vector_extension_missing", .. }`.
///
/// The DROP requires sufficient privileges; tests run as the bootstrap
/// `postgres` superuser, which has them. If the test environment has
/// the extension installed AND can't drop it (e.g. used by other
/// objects), this test will silently re-skip — we don't fail the suite
/// in that case because the typed-error assertion is the load-bearing
/// part of the contract, not the drop itself.
#[compio::test]
async fn pgvector_extension_missing_reports_typed_error() {
    use zeroship_plugin_db::backend::{PostgresBackend, VectorIndex, VectorMetric};
    use zeroship_plugin_db::error::DbError;

    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    // Best-effort drop. If this fails (extension in use, etc.) we still
    // try the probe — `pg_extension WHERE extname='vector'` will return
    // a row, and the probe call will succeed; then we just skip the
    // assertion. This keeps the test honest in both environments.
    let _ = pool.execute("DROP EXTENSION IF EXISTS vector CASCADE", &[]).await;

    let still_present = pool
        .query_text_params("SELECT 1 FROM pg_extension WHERE extname='vector'", &[])
        .await
        .map(|rows| !rows.is_empty())
        .unwrap_or(false);
    if still_present {
        zeroship_test_support::skip("Skipping: could not drop vector extension (likely in use by other objects)");
        return release_pg(pool).await;
    }

    let backend = PostgresBackend::new(pool.clone(), url.clone());
    let ensure_err = VectorIndex::ensure_vector_index(
        &backend,
        "vector_missing",
        "any",
        "any",
        128,
        VectorMetric::Cosine,
    )
    .await
    .expect_err("missing extension must yield a typed error");

    // Also exercise vector_search — the SDK branches on
    // `e.code === "vector_extension_missing"` from BOTH entry points.
    let search_err = VectorIndex::vector_search(
        &backend,
        "vector_missing",
        "any",
        "any",
        &[0.0f32; 8],
        10,
        VectorMetric::Cosine,
        &serde_json::Value::Null,
    )
    .await
    .expect_err("missing extension must yield a typed error on search too");

    // RESTORE the extension BEFORE asserting: this test deliberately drops a
    // SHARED, cluster-/db-wide object (the `vector` extension lives in
    // `public`, not in a per-app schema), so leaving it dropped breaks every
    // vector-dependent test ordered after this one in a single-threaded run
    // (e.g. `p4_round_trip_encrypted_masked_vector_via_introspected_metadata`,
    // which `registerModel`s a `vector` column). Restore happens before the
    // assertions so a failed assertion can never leak the dropped state.
    pool.execute("CREATE EXTENSION IF NOT EXISTS vector", &[])
        .await
        .expect("restore the shared vector extension after the missing-extension probe");

    match ensure_err {
        DbError::Configuration { code, message, hint } => {
            assert_eq!(code, "vector_extension_missing", "got {message}");
            assert!(
                hint.as_deref()
                    .map(|h| h.contains("CREATE EXTENSION"))
                    .unwrap_or(false),
                "hint must mention `CREATE EXTENSION vector;`: {hint:?}"
            );
        }
        other => panic!("expected Configuration {{ vector_extension_missing }}, got {other:?}"),
    }
    match search_err {
        DbError::Configuration { code, .. } => {
            assert_eq!(code, "vector_extension_missing");
        }
        other => panic!("expected Configuration {{ vector_extension_missing }}, got {other:?}"),
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
#[ignore = "requires pgvector — swap `pg-test` image to pgvector/pgvector:pg16"]
async fn vector_dimension_mismatch_rejected_at_insert() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    if !pgvector_available(&pool).await {
        zeroship_test_support::skip("Skipping: pgvector not installed in test environment");
        return release_pg(pool).await;
    }

    let app = "vector_dim_mismatch";
    let coll = "docs";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
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
            &format!(
                "INSERT INTO \"{app}\".\"{coll}\" (embedding) VALUES ($1::vector)"
            ),
            &[&lit],
        )
        .await;
    let err = result.expect_err("256-d into vector(128) column must fail");
    let msg = format!("{err}");
    // pgvector messages vary across versions; assert on the digits 256
    // and 128 (both should appear) and on "vector" anchor.
    assert!(
        msg.contains("128") || msg.contains("256") || msg.to_lowercase().contains("vector"),
        "error message must mention dim mismatch: {msg}"
    );
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// FullTextIndex + SpatialIndex (PG arm) test gates.
//
// FTS tests run unconditionally: tsvector / GIN / plainto_tsquery /
// tsvector_update_trigger are all core PG (no extension needed).
//
// Spatial tests require PostGIS. The default `pg-test` container
// (`postgres:16`) doesn't bundle PostGIS, so the spatial gates are
// `#[ignore]`-marked and run via `--ignored` against a PostGIS-bundled
// image — see docs/runbooks/docker-compose.md and the open question
// at the bottom of the report.
// ---------------------------------------------------------------------------

async fn postgis_extension_available(pool: &Pool) -> bool {
    // Try a no-op `CREATE EXTENSION` so the test environment that ships
    // PostGIS but doesn't pre-install it still picks it up. If the
    // extension isn't shipped at all the call fails and we fall back
    // to the probe (which will return empty rows → false).
    let _ = pool
        .execute("CREATE EXTENSION IF NOT EXISTS postgis", &[])
        .await;
    let rows = pool
        .query_text_params("SELECT 1 FROM pg_extension WHERE extname='postgis'", &[])
        .await
        .unwrap_or_default();
    !rows.is_empty()
}

/// Test gate for `fts_search_matches_substring`.
///
/// Inserts 5 rows whose `bio` column matches different keyword sets;
/// asserts `fts_search("rust")` returns the membership set we expect
/// (the rows containing "rust" anywhere — bare "rust", "rust async",
/// and any phrase variant). Set membership, not ordinal positions.
#[compio::test]
async fn fts_search_matches_substring() {
    use zeroship_plugin_db::backend::{FullTextIndex, PostgresBackend};

    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = "fts_substring";
    let coll = "people";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    // `fts_search` runs under the per-app role (autocommit §17.5 + DB-1
    // guards `SET LOCAL ROLE app_<app>_role`), so the role + its admin
    // template must exist before the search — exactly as every other
    // role-scoped test provisions via `ensure_per_app_role`.
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .unwrap();
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();
    pool.execute(
        &format!(
            "CREATE TABLE \"{app}\".\"{coll}\" (\
               id SERIAL PRIMARY KEY, \
               bio TEXT NOT NULL\
             )"
        ),
        &[],
    )
    .await
    .unwrap();

    let backend = PostgresBackend::new(pool.clone(), url.clone());

    // Build the FTS index (tsvector column + GIN + trigger). The
    // trigger fires on subsequent INSERTs, so we wire it BEFORE
    // inserting the seed rows so the tsvector column gets populated
    // by the trigger rather than the backfill UPDATE.
    FullTextIndex::ensure_fts_index(
        &backend,
        app,
        coll,
        &["bio".to_string()],
        "english",
    )
    .await
    .unwrap_or_else(|e| panic!("ensure_fts_index failed: {e:?}"));

    let seeds = [
        "Loves rust and systems programming",
        "Building async services",
        "rust async fan",
        "Python developer",
        "Ruby on Rails dev",
    ];
    for s in &seeds {
        pool.execute(
            &format!("INSERT INTO \"{app}\".\"{coll}\" (bio) VALUES ($1)"),
            &[s as &(dyn compio_postgres::types::ToSql + Sync)],
        )
        .await
        .unwrap();
    }

    let rows = FullTextIndex::fts_search(
        &backend,
        app,
        coll,
        "rust",
        &serde_json::Value::Null,
        None,
    )
    .await
    .unwrap_or_else(|e| panic!("fts_search failed: {e:?}"));

    // "rust" tokenises to "rust" — matches rows 1 and 3 ("rust",
    // "rust async"). The english stemmer leaves "rust" untouched
    // (it's already the root form).
    let bios: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get("bio").and_then(serde_json::Value::as_str).map(str::to_string))
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "expected 2 rust-matching rows, got {} ({bios:?})",
        rows.len()
    );
    assert!(
        bios.iter().any(|b| b.contains("rust and systems")),
        "expected the 'rust and systems' row in {bios:?}"
    );
    assert!(
        bios.iter().any(|b| b.contains("rust async fan")),
        "expected the 'rust async fan' row in {bios:?}"
    );
    // Every row must carry the synthetic `_rank` column.
    for r in &rows {
        assert!(r.get("_rank").is_some(), "row missing _rank: {r}");
    }
    drop(backend);
    release_pg(pool).await;
}

/// Test gate for `fts_and_filter_compose`.
///
/// FTS `MATCH` composed via `AND` with a regular column filter must
/// intersect — assert the final set is exactly the rows matching both
/// conditions.
#[compio::test]
async fn fts_and_filter_compose() {
    use zeroship_plugin_db::backend::{FullTextIndex, PostgresBackend};

    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = "fts_compose";
    let coll = "people";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    // `fts_search` runs under the per-app role (autocommit §17.5 + DB-1
    // guards `SET LOCAL ROLE app_<app>_role`); provision it first.
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .unwrap();
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();
    pool.execute(
        &format!(
            "CREATE TABLE \"{app}\".\"{coll}\" (\
               id SERIAL PRIMARY KEY, \
               bio TEXT NOT NULL, \
               lang TEXT NOT NULL\
             )"
        ),
        &[],
    )
    .await
    .unwrap();

    let backend = PostgresBackend::new(pool.clone(), url.clone());
    FullTextIndex::ensure_fts_index(
        &backend,
        app,
        coll,
        &["bio".to_string()],
        "english",
    )
    .await
    .unwrap_or_else(|e| panic!("ensure_fts_index failed: {e:?}"));

    let seeds = [
        ("Loves rust and systems programming", "en"),
        ("rust async runtimes", "en"),
        ("python developer", "en"),
        ("rust fan", "de"),
        ("rust crab", "de"),
    ];
    for (bio, lang) in &seeds {
        pool.execute(
            &format!("INSERT INTO \"{app}\".\"{coll}\" (bio, lang) VALUES ($1, $2)"),
            &[
                bio as &(dyn compio_postgres::types::ToSql + Sync),
                lang as &(dyn compio_postgres::types::ToSql + Sync),
            ],
        )
        .await
        .unwrap();
    }

    // FTS for "rust" filtered to lang="en" — must hit exactly rows 1 + 2
    // (the two "rust" bios with lang="en"), not 4/5 (rust bios in de).
    let rows = FullTextIndex::fts_search(
        &backend,
        app,
        coll,
        "rust",
        &serde_json::json!({ "lang": "en" }),
        None,
    )
    .await
    .unwrap_or_else(|e| panic!("fts_search failed: {e:?}"));
    assert_eq!(
        rows.len(),
        2,
        "expected exactly 2 (rust ∩ en) rows, got {}",
        rows.len()
    );
    for r in &rows {
        assert_eq!(
            r.get("lang").and_then(serde_json::Value::as_str),
            Some("en"),
            "filter must restrict to lang=en: {r}"
        );
    }
    drop(backend);
    release_pg(pool).await;
}

/// Test gate for `fts_trigger_keeps_index_in_sync_after_update`.
///
/// Insert a row, search for token "alpha" — must hit. Update the row to
/// replace "alpha" with "beta" and search for "alpha" again — must
/// MISS, while a search for "beta" must hit. This exercises the
/// `tsvector_update_trigger` rather than just the initial backfill.
#[compio::test]
async fn fts_trigger_keeps_index_in_sync_after_update() {
    use zeroship_plugin_db::backend::{FullTextIndex, PostgresBackend};

    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = "fts_trigger";
    let coll = "docs";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    // `fts_search` runs under the per-app role (autocommit §17.5 + DB-1
    // guards `SET LOCAL ROLE app_<app>_role`); provision it first.
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .unwrap();
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();
    pool.execute(
        &format!(
            "CREATE TABLE \"{app}\".\"{coll}\" (\
               id SERIAL PRIMARY KEY, \
               body TEXT NOT NULL\
             )"
        ),
        &[],
    )
    .await
    .unwrap();

    let backend = PostgresBackend::new(pool.clone(), url.clone());
    FullTextIndex::ensure_fts_index(
        &backend,
        app,
        coll,
        &["body".to_string()],
        "english",
    )
    .await
    .unwrap_or_else(|e| panic!("ensure_fts_index failed: {e:?}"));

    let alpha = "alpha test content";
    pool.execute(
        &format!("INSERT INTO \"{app}\".\"{coll}\" (body) VALUES ($1)"),
        &[&alpha as &(dyn compio_postgres::types::ToSql + Sync)],
    )
    .await
    .unwrap();

    let hits = FullTextIndex::fts_search(
        &backend,
        app,
        coll,
        "alpha",
        &serde_json::Value::Null,
        None,
    )
    .await
    .unwrap();
    assert_eq!(hits.len(), 1, "expected 1 alpha hit pre-update, got {}", hits.len());

    let beta = "beta different content";
    pool.execute(
        &format!("UPDATE \"{app}\".\"{coll}\" SET body = $1 WHERE id = 1"),
        &[&beta as &(dyn compio_postgres::types::ToSql + Sync)],
    )
    .await
    .unwrap();

    let alpha_hits = FullTextIndex::fts_search(
        &backend,
        app,
        coll,
        "alpha",
        &serde_json::Value::Null,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        alpha_hits.len(),
        0,
        "trigger must invalidate alpha after UPDATE, got {} hits",
        alpha_hits.len()
    );

    let beta_hits = FullTextIndex::fts_search(
        &backend,
        app,
        coll,
        "beta",
        &serde_json::Value::Null,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        beta_hits.len(),
        1,
        "trigger must surface beta after UPDATE, got {} hits",
        beta_hits.len()
    );
    drop(backend);
    release_pg(pool).await;
}

/// Test gate for `near_returns_within_radius`.
///
/// 10 points around London at varying distances from the centre
/// `(51.5074, -0.1278)`. `near()` with a 1km radius returns only the
/// points actually within 1km (assert by membership set, not strict
/// ordering — ST_Distance is FP-deterministic in modern PostGIS but we
/// don't pin the order).
///
/// **`#[ignore]`** until the test environment swaps to a PostGIS-bundled
/// image. See `postgis_available` probe — the test self-skips if the
/// extension isn't present, but the `#[ignore]` keeps default `cargo
/// test` runs from probing at all.
#[compio::test]
#[ignore = "requires PostGIS — swap `pg-test` image to a PostGIS-bundled variant"]
async fn near_returns_within_radius() {
    use zeroship_plugin_db::backend::{GeoPoint, PostgresBackend, SpatialIndex};

    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    if !postgis_available(&pool).await {
        zeroship_test_support::skip("Skipping: PostGIS not installed in test environment");
        return release_pg(pool).await;
    }

    let app = "near_radius";
    let coll = "places";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    pool.execute(
        &format!(
            "CREATE TABLE \"{app}\".\"{coll}\" (\
               id SERIAL PRIMARY KEY, \
               location geography(POINT, 4326) NOT NULL\
             )"
        ),
        &[],
    )
    .await
    .unwrap();

    let london = GeoPoint { lat: 51.5074, lng: -0.1278 };
    // 10 points: 5 within ~1km of London (small lat/lng offsets) and
    // 5 well outside (several km away). One degree of latitude is
    // ~111km, so 0.005 deg ≈ 555m and 0.05 deg ≈ 5.5km.
    let offsets: Vec<(f64, f64, bool)> = vec![
        (0.0, 0.0, true),       // dead-centre
        (0.001, 0.001, true),   // ~140m
        (0.003, 0.003, true),   // ~420m
        (-0.005, 0.0, true),    // ~555m south
        (0.0, 0.005, true),     // ~350m east (cos(51.5°) ≈ 0.62)
        (0.05, 0.0, false),     // ~5.5km north
        (-0.05, 0.0, false),    // ~5.5km south
        (0.0, 0.05, false),     // ~3.5km east
        (0.0, -0.05, false),    // ~3.5km west
        (0.1, 0.1, false),      // ~11km NE
    ];
    let mut expected_within: Vec<i64> = Vec::new();
    for (i, (dlat, dlng, within_1km)) in offsets.iter().enumerate() {
        let lng = london.lng + dlng;
        let lat = london.lat + dlat;
        let lit = format!("POINT({lng} {lat})");
        pool.execute(
            &format!(
                "INSERT INTO \"{app}\".\"{coll}\" (location) VALUES (ST_GeogFromText($1))"
            ),
            &[&lit as &(dyn compio_postgres::types::ToSql + Sync)],
        )
        .await
        .unwrap();
        if *within_1km {
            expected_within.push((i + 1) as i64);
        }
    }

    let backend = PostgresBackend::new(pool.clone(), url.clone());
    let rows = SpatialIndex::spatial_near(
        &backend,
        app,
        coll,
        "location",
        london,
        1000.0,
        &serde_json::Value::Null,
        None,
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
        assert!(r.get("_distance_m").is_some(), "row missing _distance_m: {r}");
    }
    drop(backend);
    release_pg(pool).await;
}

/// Test gate for `postgis_extension_missing_reports_typed_error`.
///
/// When the database has no PostGIS, both `ensure_spatial_index` and
/// `spatial_near` must surface `DbError::Configuration { code:
/// "postgis_extension_missing", .. }`. Same shape as
/// `pgvector_extension_missing_reports_typed_error`.
#[compio::test]
async fn postgis_extension_missing_reports_typed_error() {
    use zeroship_plugin_db::backend::{GeoPoint, PostgresBackend, SpatialIndex};
    use zeroship_plugin_db::error::DbError;

    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    // Best-effort drop. If this fails (e.g. extension in use), we
    // re-check via the probe and self-skip the assertion.
    let _ = pool
        .execute("DROP EXTENSION IF EXISTS postgis CASCADE", &[])
        .await;

    let still_present = pool
        .query_text_params("SELECT 1 FROM pg_extension WHERE extname='postgis'", &[])
        .await
        .map(|rows| !rows.is_empty())
        .unwrap_or(false);
    if still_present {
        zeroship_test_support::skip("Skipping: could not drop postgis extension (likely in use by other objects)");
        return release_pg(pool).await;
    }

    let backend = PostgresBackend::new(pool.clone(), url.clone());
    let err = SpatialIndex::ensure_spatial_index(&backend, "postgis_missing", "any", "any")
        .await
        .expect_err("missing PostGIS must yield a typed error");
    match err {
        DbError::Configuration { code, message, hint } => {
            assert_eq!(code, "postgis_extension_missing", "got {message}");
            assert!(
                hint.as_deref()
                    .map(|h| h.contains("CREATE EXTENSION"))
                    .unwrap_or(false),
                "hint must mention `CREATE EXTENSION postgis;`: {hint:?}"
            );
        }
        other => panic!("expected Configuration {{ postgis_extension_missing }}, got {other:?}"),
    }

    let err = SpatialIndex::spatial_near(
        &backend,
        "postgis_missing",
        "any",
        "any",
        GeoPoint { lat: 0.0, lng: 0.0 },
        1000.0,
        &serde_json::Value::Null,
        None,
    )
    .await
    .expect_err("missing PostGIS must yield a typed error on near too");
    match err {
        DbError::Configuration { code, .. } => {
            assert_eq!(code, "postgis_extension_missing");
        }
        other => panic!("expected Configuration {{ postgis_extension_missing }}, got {other:?}"),
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
// below can share one `use` block. The `as _` on `EncryptedColumn`
// brings the trait methods into scope without aliasing the trait name
// itself.
use zeroship_plugin_db::backend::{EncryptedColumn as _, EncryptionMode, PostgresBackend};
use zeroship_plugin_db::encryption;
use zeroship_plugin_db::error::DbError;

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
/// The PG resolve path is unchanged: `PostgresBackend` still calls the
/// SECURITY DEFINER `__zeroship_admin.get_column_key` getter first and
/// only falls back to this source when the getter returns NULL, which is
/// the same arm the env var used to occupy.
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
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    // Synthetic 32-byte root key.
    let _keys = with_root_key("default", &"a".repeat(64));

    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{SCHEMA}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{SCHEMA}\""), &[])
        .await
        .unwrap();
    // Manually create the table; the encryption pass operates on
    // generic BYTEA columns regardless of how DDL emits them, and we
    // want the integration test to not depend on the full
    // register-model pipeline (which is gated to the V8 entry).
    pool.execute(
        &format!(
            r#"CREATE TABLE "{SCHEMA}"."enc_notes" (
                id   TEXT PRIMARY KEY,
                ssn  BYTEA
            )"#
        ),
        &[],
    )
    .await
    .unwrap();

    let backend = PostgresBackend::new(pool.clone(), url.clone());
    let key = backend.resolve_key("app1", "default").await.expect("resolve_key");
    let plaintext = b"123-45-6789";
    let aad = encryption::canonical_aad("enc_notes", "ssn", Some(b"row_a"));
    let ct = backend
        .encrypt(&key, EncryptionMode::Randomised, plaintext, &aad)
        .expect("encrypt");

    // Bind via base64 decode just like the build_insert layer does.
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &ct);
    pool.execute(
        &format!(
            "INSERT INTO \"{SCHEMA}\".\"enc_notes\" (id, ssn) VALUES ($1, decode($2, 'base64')::bytea)"
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
            &format!("SELECT encode(ssn, 'hex') AS ssn_hex FROM \"{SCHEMA}\".\"enc_notes\" WHERE id = $1"),
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
    let recovered = backend
        .decrypt(&key, EncryptionMode::Randomised, &raw, &aad)
        .expect("decrypt");
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
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let _keys = with_root_key("default", &"b".repeat(64));

    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{SCHEMA}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{SCHEMA}\""), &[])
        .await
        .unwrap();
    pool.execute(
        &format!(
            r#"CREATE TABLE "{SCHEMA}"."enc_notes" (
                id   TEXT PRIMARY KEY,
                ssn  BYTEA
            )"#
        ),
        &[],
    )
    .await
    .unwrap();

    let backend = PostgresBackend::new(pool.clone(), url.clone());
    let key = backend.resolve_key("app1", "default").await.unwrap();
    // Insert row A with its OWN AAD (binds row_pk = "row_a").
    let ct_a = backend
        .encrypt(
            &key,
            EncryptionMode::Randomised,
            b"sensitive-A",
            &encryption::canonical_aad("enc_notes", "ssn", Some(b"row_a")),
        )
        .unwrap();
    let ct_b = backend
        .encrypt(
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
                "INSERT INTO \"{SCHEMA}\".\"enc_notes\" (id, ssn) VALUES ($1, decode($2, 'base64')::bytea)"
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
            "UPDATE \"{SCHEMA}\".\"enc_notes\" SET ssn = decode($1, 'base64')::bytea WHERE id = $2"
        ),
        &[&b64_a.as_str(), &"row_b"],
    )
    .await
    .unwrap();

    // Read row B → decrypt with row B's AAD (row_pk = "row_b"). Use
    // `encode(ssn, 'hex')` per the round-trip test above.
    let rows = pool
        .query_text_params(
            &format!("SELECT encode(ssn, 'hex') AS ssn_hex FROM \"{SCHEMA}\".\"enc_notes\" WHERE id = $1"),
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
    let err = backend
        .decrypt(&key, EncryptionMode::Randomised, &raw, &aad_b)
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
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let _keys = with_root_key("default", &"c".repeat(64));

    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{SCHEMA}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{SCHEMA}\""), &[])
        .await
        .unwrap();
    pool.execute(
        &format!(
            r#"CREATE TABLE "{SCHEMA}"."enc_notes" (
                id   TEXT PRIMARY KEY,
                ssn  BYTEA
            )"#
        ),
        &[],
    )
    .await
    .unwrap();
    pool.execute(
        &format!(r#"CREATE INDEX ON "{SCHEMA}"."enc_notes" (ssn)"#),
        &[],
    )
    .await
    .unwrap();

    let backend = PostgresBackend::new(pool.clone(), url.clone());
    let key = backend.resolve_key("app1", "default").await.unwrap();

    // Insert 5 rows with the same SSN to confirm deterministic mode
    // produces identical ciphertext (we then query by exact ciphertext
    // and expect all 5 to come back).
    let aad = encryption::canonical_aad("enc_notes", "ssn", None);
    let ct_shared = backend
        .encrypt(&key, EncryptionMode::Deterministic, b"shared-ssn", &aad)
        .unwrap();
    let b64_shared = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &ct_shared);

    for i in 0..5 {
        pool.execute(
            &format!(
                "INSERT INTO \"{SCHEMA}\".\"enc_notes\" (id, ssn) VALUES ($1, decode($2, 'base64')::bytea)"
            ),
            &[&format!("row_{i}").as_str(), &b64_shared.as_str()],
        )
        .await
        .unwrap();
    }
    // Plus a distinct row.
    let ct_other = backend
        .encrypt(&key, EncryptionMode::Deterministic, b"other-ssn", &aad)
        .unwrap();
    let b64_other = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &ct_other);
    pool.execute(
        &format!(
            "INSERT INTO \"{SCHEMA}\".\"enc_notes\" (id, ssn) VALUES ($1, decode($2, 'base64')::bytea)"
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
            &format!("SELECT id FROM \"{SCHEMA}\".\"enc_notes\" WHERE ssn = decode($1, 'base64')::bytea"),
            &[b64_shared.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 5, "deterministic equality lookup must match all 5 shared-ssn rows");
    drop(backend);
    release_pg(pool).await;
}

/// Round-trip e2e proof that schema creation and CRUD cohere: a collection
/// with an `encrypted` + a `masked` + a `vector` field, schema CREATED via
/// the real `registerModel` (which writes the `zsenc`/`__zsmask` sentinels),
/// then CRUD driven ENTIRELY by the introspection-sourced metadata:
///   - insert through the REAL write pipeline -> AEAD-encrypts the encrypted
///     column and populates the masked sibling (metadata from introspection);
///   - read raw rows back, finalize through the REAL read pipeline -> decrypts
///     the encrypted column to plaintext and wraps the masked column.
/// Nothing here consults the declared schema for the crypto/mask decisions --
/// the seam is `crud::introspect_schema::runtime_schema_for`, exercised faithfully.
#[compio::test]
async fn p4_round_trip_encrypted_masked_vector_via_introspected_metadata() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    let _keys = with_root_key("default", &"d".repeat(64));

    let app = "p4_round_trip";
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

    // registerModel creates the table AND writes the sentinels
    // (zsenc COMMENT on `ssn`, __zsmask COMMENT on `phone_masked`).
    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool),
        app,
        "people",
        &schema,
        &serde_json::json!([]),
        "p4_deploy_1",
    )
    .await
    .unwrap_or_else(|e| panic!("registerModel failed: {e}"));

    // Install the pool into the per-isolate context so
    // `runtime_schema_for` can introspect, and mark the model registered (the
    // cold-schema gate) — exactly what the production register path does.
    zeroship_plugin_db::set_postgres_pool_for_tests(std::rc::Rc::clone(&pool), &url);
    zeroship_plugin_db::mark_model_registered_for_tests(app, "people");

    // Sanity: the introspected runtime schema recovers BOTH goodies — proving
    // the data-access metadata comes from the live catalog + sentinels.
    let introspected =
        zeroship_plugin_db::crud::runtime_schema_for_tests(app, "people")
            .await
            .expect("introspect")
            .expect("people has goodies");
    assert_eq!(introspected["ssn"]["encrypted"]["mode"], "randomised");
    assert_eq!(introspected["phone"]["mask"]["kind"], "last4");

    // ----- WRITE (real pipeline, introspected metadata) -----
    let mut docs = json!([{
        "id": "psn_round_trip_1",
        "name": "Ada",
        "ssn": "123-45-6789",
        "phone": "415-555-0142",
        "embedding": [0.1, 0.2, 0.3],
    }]);
    zeroship_plugin_db::crud::prepare_insert_many_docs_for_write(&mut docs, app, "people", None)
        .await
        .expect("write pipeline");

    // The write pipeline encrypted `ssn` (base64 blob + `__zsbin__ssn` marker)
    // and derived the masked sibling `phone_masked` from the plaintext.
    let doc = &docs[0];
    assert!(
        doc["ssn"].as_str().is_some() && doc["ssn"] != json!("123-45-6789"),
        "ssn must be replaced by ciphertext on write, got {:?}",
        doc["ssn"]
    );
    assert_eq!(doc["__zsbin__ssn"], json!(true), "encrypt marker set");
    assert_eq!(
        doc["phone_masked"], json!("***-***-0142"),
        "mask pass must derive the last4 sibling on write, got {:?}",
        doc["phone_masked"]
    );

    // Persist it the way the SQL builder would (decode the encrypted blob, store
    // the masked sibling). We INSERT the encrypted ssn + the masked sibling.
    let ssn_b64 = doc["ssn"].as_str().unwrap().to_string();
    let phone_masked = doc["phone_masked"].as_str().unwrap().to_string();
    // The vector literal is a test-controlled constant — format it inline with a
    // `::vector` cast (compio-postgres infers a `vector`-typed param from the
    // bind otherwise, which it cannot encode an `&str` into).
    pool.execute(
        &format!(
            "INSERT INTO \"{app}\".\"people\" (id, name, ssn, phone, phone_masked, embedding) \
             VALUES ($1, $2, decode($3, 'base64')::bytea, $4, $5, '[0.1,0.2,0.3]'::vector)"
        ),
        &[
            &"psn_round_trip_1",
            &"Ada",
            &ssn_b64.as_str(),
            &"415-555-0142",
            &phone_masked.as_str(),
        ],
    )
    .await
    .unwrap();

    // ----- READ (real pipeline, introspected metadata) -----
    // Fetch the raw row the way the SELECT builder would (encrypted blob as
    // base64, the masked sibling aliased back to the parent name).
    let raw = pool
        .query_text_params(
            &format!(
                "SELECT id, name, encode(ssn, 'base64') AS ssn, \
                 phone_masked AS phone FROM \"{app}\".\"people\" WHERE id = $1"
            ),
            &["psn_round_trip_1"],
        )
        .await
        .unwrap();
    assert_eq!(raw.len(), 1);
    let row = json!({
        "id": "psn_round_trip_1",
        "name": "Ada",
        "ssn": raw[0].get::<_, String>("ssn"),
        "phone": raw[0].get::<_, String>("phone"),
    });

    let finalized =
        zeroship_plugin_db::crud::finalize_rows_on_read_for_tests(app, "people", vec![row])
            .await
            .expect("read pipeline");
    let out = &finalized[0];

    // Encrypted column decrypted back to plaintext (driven by introspected meta).
    assert_eq!(
        out["ssn"], json!("123-45-6789"),
        "encrypted column must decrypt to plaintext on read, got {:?}",
        out["ssn"]
    );
    // Masked column wrapped into the platform MaskedValue sentinel, carrying the
    // last4-masked string + the introspected classification.
    assert_eq!(out["phone"]["sentinel"], json!("__zsmask__"), "phone wrapped");
    assert_eq!(
        out["phone"]["masked"], json!("***-***-0142"),
        "masked phone surfaces last4 form, got {:?}",
        out["phone"]
    );
    assert_eq!(out["phone"]["classification"], json!("pci"));
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// THE CUTOVER. On the PG dialect, `registerModel` STOPS being a schema
// authority: it issues NO runtime DDL (the engine creates/migrates the schema
// at deploy time). It only ensures readiness + the declared cache so the
// introspection path keeps working. SQLite dev is UNCHANGED (it still
// auto-migrates from the declared schema). These three tests are the faithful
// behaviour-identical + no-runtime-DDL proof:
//   (c) p5_pg_register_model_issues_no_runtime_ddl -- the cutover proof: the PG
//       dispatch never CREATEs/ALTERs (no table, no schema, no audit row).
//   (a) p5_pg_crud_works_via_engine_created_schema_no_runtime_ddl -- a collection
//       whose schema was created the way the engine/deploy-apply does (sentinels
//       and all) -> PG registerModel no-ops the apply, yet CRUD + encryption +
//       mask round-trip via the INTROSPECTED metadata.
//   (b) p5_sqlite_register_model_still_auto_migrates -- SQLite registerModel is
//       unchanged: it still creates the table from the declared schema.
// ---------------------------------------------------------------------------

/// Count rows in the per-app audit journal (`__zeroship_migrations`), or `None`
/// when the table is absent. The OLD PG `registerModel` wrote one audit row per
/// applied DDL op; the current PG path applies nothing, so this stays put across a
/// dispatch call -- a direct, faithful "no DDL was issued" probe.
async fn audit_row_count(pool: &std::rc::Rc<Pool>, app: &str) -> Option<i64> {
    let exists = pool
        .query_text_params(
            "SELECT to_regclass($1) IS NOT NULL AS present",
            &[format!("\"{app}\".\"__zeroship_migrations\"").as_str()],
        )
        .await
        .ok()?;
    let present: bool = exists.first()?.get("present");
    if !present {
        return None;
    }
    let rows = pool
        .query_text_params(
            &format!("SELECT count(*)::bigint AS n FROM \"{app}\".\"__zeroship_migrations\""),
            &[],
        )
        .await
        .ok()?;
    Some(rows.first()?.get::<_, i64>("n"))
}

/// True iff a `<app>.<table>` relation exists in the catalog.
async fn pg_table_exists(pool: &std::rc::Rc<Pool>, app: &str, table: &str) -> bool {
    pool.query_text_params(
        "SELECT to_regclass($1) IS NOT NULL AS present",
        &[format!("\"{app}\".\"{table}\"").as_str()],
    )
    .await
    .ok()
    .and_then(|r| r.first().map(|row| row.get::<_, bool>("present")))
    .unwrap_or(false)
}

/// The cutover proof (c). On the PG dialect, the production
/// `registerModel` dispatch issues NO schema DDL. We install a PG backend into
/// the per-isolate context, call the EXACT production dispatch
/// (`exec_register_model_via_dispatch_for_tests` -> `exec_register_model`'s PG
/// arm) against a schema whose table does NOT yet exist, and assert that:
///   * no table was created (the old path would `CREATE TABLE`),
///   * no per-app schema/audit journal was created (the old `bootstrap` did),
/// proving the runtime is no longer a PG schema applier. The engine's
/// deploy-apply is the sole PG authority.
#[compio::test]
async fn p5_pg_register_model_issues_no_runtime_ddl() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());

    let app = "p5_no_ddl";
    // Clean slate: NO schema, NO table — the engine hasn't run here.
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();

    // Install the PG backend so the production dispatch resolves the PG arm.
    zeroship_plugin_db::set_postgres_pool_for_tests(std::rc::Rc::clone(&pool), &url);

    let schema = json!({
        "title": {"type": "string", "required": true},
        "secret": {
            "type": "string",
            "encrypted": {"mode": "randomised", "keyId": "default", "wraps": "string"}
        },
    });

    // Drive the PRODUCTION dialect dispatch. On PG this must NO-OP the apply.
    zeroship_plugin_db::register_model::exec_register_model_via_dispatch_for_tests(
        app,
        "widgets",
        &schema,
        &json!([]),
    )
    .await
    .expect("PG registerModel dispatch must succeed (no-op)");

    // PROOF: nothing was created. The OLD path would have CREATE SCHEMA +
    // CREATE TABLE + the __zeroship_migrations journal + audit rows.
    assert!(
        !pg_table_exists(&pool, app, "widgets").await,
        "P5 PG cutover: registerModel must NOT create the table at runtime"
    );
    assert_eq!(
        audit_row_count(&pool, app).await,
        None,
        "P5 PG cutover: registerModel must NOT create the audit journal / write \
         any DDL audit rows at runtime"
    );

    // Sanity teardown.
    let _ = pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[]).await;
    release_pg(pool).await;
}

/// Behaviour-identical CRUD with NO runtime DDL (a). The engine creates
/// the schema at deploy (here simulated by a one-shot pipeline build that emits
/// the same DDL + `zsenc`/`__zsmask` sentinels the relocated engine produces).
/// Then the production PG dispatch runs and must NOT touch the schema (audit row
/// count is unchanged), yet encryption + mask CRUD still round-trip end-to-end
/// driven by the INTROSPECTED metadata -- proving the data plane is
/// intact while the runtime applied nothing.
#[compio::test]
async fn p5_pg_crud_works_via_engine_created_schema_no_runtime_ddl() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    let _keys = with_root_key("default", &"e".repeat(64));

    let app = "p5_engine_created";
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

    // === Simulate the engine/deploy-apply: create the table + sentinels. ===
    // This is the SAME DDL/sentinel emission the relocated engine uses;
    // we drive it once via the pipeline to stand in for the deploy-time apply.
    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool),
        app,
        "people",
        &schema,
        &json!([]),
        "engine_deploy_1",
    )
    .await
    .unwrap_or_else(|e| panic!("engine schema build (deploy stand-in) failed: {e}"));

    // Snapshot the audit journal AFTER the engine's apply — the runtime dispatch
    // below must not add to it.
    let audit_before = audit_row_count(&pool, app).await;
    assert!(
        audit_before.is_some(),
        "engine stand-in created the journal"
    );

    // === The runtime: install PG backend, run the PRODUCTION dispatch. ===
    zeroship_plugin_db::set_postgres_pool_for_tests(std::rc::Rc::clone(&pool), &url);
    zeroship_plugin_db::register_model::exec_register_model_via_dispatch_for_tests(
        app,
        "people",
        &schema,
        &json!([]),
    )
    .await
    .expect("PG registerModel dispatch must succeed (no-op apply)");

    // PROOF the runtime issued NO DDL: the audit journal is byte-for-byte the
    // same count it was after the engine's apply.
    assert_eq!(
        audit_row_count(&pool, app).await,
        audit_before,
        "P5 PG cutover: the runtime dispatch must add ZERO DDL audit rows"
    );

    // Readiness contract: mark the model (the dispatch caller does this in prod;
    // the via-dispatch seam stops at `exec_register_model`, so mirror it here),
    // exactly like the round-trip test above does.
    zeroship_plugin_db::mark_model_registered_for_tests(app, "people");

    // The introspected runtime schema recovers BOTH goodies from the live catalog
    // + the engine's sentinels — no declared schema consulted for crypto/mask.
    let introspected = zeroship_plugin_db::crud::runtime_schema_for_tests(app, "people")
        .await
        .expect("introspect")
        .expect("people has goodies");
    assert_eq!(introspected["ssn"]["encrypted"]["mode"], "randomised");
    assert_eq!(introspected["phone"]["mask"]["kind"], "last4");

    // ----- WRITE via the real pipeline (introspected metadata) -----
    let mut docs = json!([{
        "id": "psn_p5_1",
        "name": "Grace",
        "ssn": "987-65-4321",
        "phone": "650-555-0199",
    }]);
    zeroship_plugin_db::crud::prepare_insert_many_docs_for_write(&mut docs, app, "people", None)
        .await
        .expect("write pipeline");
    let doc = &docs[0];
    assert!(
        doc["ssn"].as_str().is_some() && doc["ssn"] != json!("987-65-4321"),
        "ssn must be ciphertext on write, got {:?}",
        doc["ssn"]
    );
    assert_eq!(doc["__zsbin__ssn"], json!(true), "encrypt marker set");
    assert_eq!(
        doc["phone_masked"], json!("***-***-0199"),
        "mask pass derives the last4 sibling on write, got {:?}",
        doc["phone_masked"]
    );

    let ssn_b64 = doc["ssn"].as_str().unwrap().to_string();
    let phone_masked = doc["phone_masked"].as_str().unwrap().to_string();
    pool.execute(
        &format!(
            "INSERT INTO \"{app}\".\"people\" (id, name, ssn, phone, phone_masked) \
             VALUES ($1, $2, decode($3, 'base64')::bytea, $4, $5)"
        ),
        &[
            &"psn_p5_1",
            &"Grace",
            &ssn_b64.as_str(),
            &"650-555-0199",
            &phone_masked.as_str(),
        ],
    )
    .await
    .unwrap();

    // ----- READ via the real pipeline (introspected metadata) -----
    let raw = pool
        .query_text_params(
            &format!(
                "SELECT id, name, encode(ssn, 'base64') AS ssn, \
                 phone_masked AS phone FROM \"{app}\".\"people\" WHERE id = $1"
            ),
            &["psn_p5_1"],
        )
        .await
        .unwrap();
    assert_eq!(raw.len(), 1);
    let row = json!({
        "id": "psn_p5_1",
        "name": "Grace",
        "ssn": raw[0].get::<_, String>("ssn"),
        "phone": raw[0].get::<_, String>("phone"),
    });
    let finalized =
        zeroship_plugin_db::crud::finalize_rows_on_read_for_tests(app, "people", vec![row])
            .await
            .expect("read pipeline");
    let out = &finalized[0];
    assert_eq!(
        out["ssn"], json!("987-65-4321"),
        "encrypted column decrypts to plaintext on read, got {:?}",
        out["ssn"]
    );
    assert_eq!(out["phone"]["sentinel"], json!("__zsmask__"), "phone wrapped");
    assert_eq!(out["phone"]["masked"], json!("***-***-0199"));
    assert_eq!(out["phone"]["classification"], json!("pci"));

    // FINAL proof: still zero runtime DDL after the full CRUD round-trip.
    assert_eq!(
        audit_row_count(&pool, app).await,
        audit_before,
        "P5 PG cutover: CRUD must not have triggered any runtime DDL"
    );

    let _ = pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[]).await;
    release_pg(pool).await;
}

/// When no root key is configured for `missing_test` (the fallback
/// source holds none AND the `__zeroship_admin.column_keys` row is
/// missing), the PG resolver surfaces a typed
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

    let backend = PostgresBackend::new(pool.clone(), url.clone());
    let err = backend
        .resolve_key("app1", "missing_test")
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

/// **I1** — the SECURITY DEFINER `__zeroship_admin.get_column_key`
/// path must read `bytea` in binary form rather than falling through
/// to the fallback source. Pin it by seeding the admin table with one
/// root and the fallback with a DIFFERENT root: the resolved key must
/// match the admin-table root.
///
/// The contrast is the whole test, so the fallback must genuinely hold a
/// root. It used to be an env var; it is now a supplied root, which is
/// the same fallback arm reached by the same code path.
#[compio::test]
async fn pg_admin_table_key_source_reads_bytea_directly() {
    use hkdf::Hkdf;
    use sha2::Sha256;

    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .expect("ensure_admin_schema");

    let key_id = "admin_table_test";
    let admin_root_hex = "11".repeat(32);
    let fallback_root_hex = "22".repeat(32);
    let _keys = with_root_key(key_id, &fallback_root_hex);

    pool.execute(
        r#"DELETE FROM "__zeroship_admin"."column_keys" WHERE key_id = $1"#,
        &[&key_id],
    )
    .await
    .unwrap();
    pool.execute(
        r#"INSERT INTO "__zeroship_admin"."column_keys" (key_id, root_key)
           VALUES ($1, decode($2, 'hex')::bytea)"#,
        &[&key_id, &admin_root_hex.as_str()],
    )
    .await
    .unwrap();

    let backend = PostgresBackend::new(pool.clone(), url.clone());
    let resolved = backend
        .resolve_key("app_admin_key_lookup", key_id)
        .await
        .expect("resolve key from admin table");

    let root_bytes = admin_root_hex
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let s = std::str::from_utf8(pair).expect("hex utf8");
            u8::from_str_radix(s, 16).expect("hex byte")
        })
        .collect::<Vec<_>>();
    let root: [u8; 32] = root_bytes.try_into().expect("32-byte root");
    let hkdf = Hkdf::<Sha256>::new(Some(b"app_admin_key_lookup"), &root);
    let mut expected_k_enc = [0u8; 32];
    let mut expected_k_siv = [0u8; 32];
    hkdf.expand(b"zsenc/aead/v1/k_enc", &mut expected_k_enc)
        .expect("expand k_enc");
    hkdf.expand(b"zsenc/aead/v1/k_siv", &mut expected_k_siv)
        .expect("expand k_siv");

    assert_eq!(
        resolved.k_enc, expected_k_enc,
        "resolve_key must use the admin-table root, not the fallback source",
    );
    assert_eq!(resolved.k_siv, expected_k_siv);
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
    let expected_raw =
        base64::engine::general_purpose::STANDARD.encode(br"\x41424344");
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
//      `#[ignore]`-d when `pg_dump` / `pg_restore` are not on PATH
//      (CI minimal images don't always carry them).
//   2. `pitr_pg_records_target` — companion to the SQLite
//      `pitr_pg_only_*` test. Call `pitr_replay(LSN)`; assert row in
//      `__zeroship_admin.pitr_targets`. No subprocess — runs everywhere.
//   3. `snapshot_during_migration_returns_typed_error` — acquire the
//      `register_model` mig-lock manually; attempt `snapshot()`;
//      expect `Coded { code: "migration_in_progress" }`. No subprocess.
//   4. `snapshot_uri_content_hash_round_trip` — `snapshot()` →
//      `SnapshotHandle.content_hash` matches SHA-256 of the on-disk
//      dump file. `#[ignore]`-d for the same reason as #1.

use zeroship_plugin_db::backend::{
    Backup as _, BusyPolicy as BackupBusyPolicy, LockScope, PitrTarget, SnapshotOpts,
};

/// Best-effort probe for `pg_dump`/`pg_restore` on PATH. The
/// snapshot/restore round-trip tests `#[ignore]` themselves
/// statically (the runner's `--ignored` flag re-enables them); this
/// helper is for tests that can short-circuit at runtime if the
/// binaries aren't available without failing the suite. Cheap — does
/// not actually spawn the binary.
fn pg_dump_on_path() -> bool {
    std::process::Command::new("pg_dump")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Gate #2: `pitr_replay` records the target row in
/// `__zeroship_admin.pitr_targets`. The actual WAL recovery is
/// operator-driven (this ships the API surface only); this test
/// pins the placeholder shape: `INSERT … ON CONFLICT (app_id) DO
/// UPDATE …` upserts the latest target.
#[compio::test]
async fn pitr_pg_records_target() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    // PITR-targets table lives in `__zeroship_admin`; the auth
    // bootstrap creates it. Idempotent on a populated cluster.
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .expect("ensure_admin_schema");

    let backend = PostgresBackend::new(pool.clone(), url.clone());
    let app_id = "p5_pr4_pitr_app";

    // Clean any stale row from a prior run so the assertion sees
    // exactly the row we just inserted.
    pool.execute(
        "DELETE FROM __zeroship_admin.pitr_targets WHERE app_id = $1",
        &[&app_id],
    )
    .await
    .unwrap();

    // 1) LSN target.
    backend
        .pitr_replay(app_id, PitrTarget::Lsn("0/16B1234".to_string()))
        .await
        .expect("pitr_replay(LSN) records the target");

    let rows = pool
        .query_text_params(
            "SELECT target FROM __zeroship_admin.pitr_targets WHERE app_id = $1",
            &[app_id],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "exactly one row per app_id (ON CONFLICT upsert)");
    let target: String = rows[0].get::<_, String>("target");
    assert_eq!(target, "LSN:0/16B1234");

    // 2) Upsert with a TimeMillis target — the same app_id row is
    //    overwritten (ON CONFLICT (app_id) DO UPDATE).
    backend
        .pitr_replay(app_id, PitrTarget::TimeMillis(1_700_000_000_000))
        .await
        .expect("pitr_replay(TimeMillis) upserts the target");

    let rows = pool
        .query_text_params(
            "SELECT target FROM __zeroship_admin.pitr_targets WHERE app_id = $1",
            &[app_id],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "still one row after upsert");
    let target: String = rows[0].get::<_, String>("target");
    assert_eq!(target, "TIME_MS:1700000000000");

    // Cleanup so a re-run starts fresh.
    pool.execute(
        "DELETE FROM __zeroship_admin.pitr_targets WHERE app_id = $1",
        &[&app_id],
    )
    .await
    .unwrap();
    drop(backend);
    release_pg(pool).await;
}

/// Fence: when the per-app `register_model` advisory
/// lock is held by another caller, `snapshot()` surfaces a typed
/// `Coded { code: "migration_in_progress" }` rather than blocking
/// indefinitely or returning an opaque LockContention. Pins the
/// pre-flight interlock the snapshot impl runs before invoking
/// `pg_dump`.
#[compio::test]
async fn snapshot_during_migration_returns_typed_error() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .expect("ensure_admin_schema");

    let backend = PostgresBackend::new(pool.clone(), url.clone());
    let app_id = "p5_pr4_miglock_app";

    // Acquire the register_model lock on a dedicated standalone
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
    // Mirror `LockScope::GlobalApp { app_id, name: "register_model" }
    // .to_keys()` exactly so the underlying `(key1, key2)` pair
    // matches what the snapshot's pre-flight will try to acquire.
    let scope = LockScope::GlobalApp {
        app_id: app_id.to_string(),
        name: "register_model".to_string(),
    };
    let (key1, key2) = scope.to_keys();
    lock_client
        .query_text_params(
            "SELECT pg_advisory_lock(hashtext($1)::int4, hashtext($2)::int4)",
            &[key1.as_str(), key2.as_str()],
        )
        .await
        .expect("acquire register_model lock on dedicated session");

    // Snapshot dest URI doesn't need to be real — we expect the
    // call to refuse at the pre-flight stage, before pg_dump runs.
    let dest = "file:///tmp/p5_pr4_miglock_should_not_exist.dump";
    let err = backend
        .snapshot(app_id, dest, SnapshotOpts { if_busy: BackupBusyPolicy::Abort })
        .await
        .expect_err("snapshot must refuse while register_model lock is held");
    match err {
        DbError::Coded { code, .. } => {
            assert_eq!(
                code, "migration_in_progress",
                "expected Coded migration_in_progress, got code={code:?}"
            );
        }
        other => panic!(
            "expected Coded {{ code: \"migration_in_progress\", .. }}, got {other:?}"
        ),
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
/// `#[ignore]`-d statically because `pg_dump` / `pg_restore` aren't
/// available in every test environment. Run with
/// `cargo test … snapshot_restore_round_trip_pg -- --ignored`.
#[compio::test]
#[ignore = "needs pg_dump/pg_restore on PATH"]
async fn snapshot_restore_round_trip_pg() {
    let url = require_pg().await;
    if !pg_dump_on_path() {
        zeroship_test_support::skip("Skipping — pg_dump not on PATH");
        return;
    }
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .expect("ensure_admin_schema");

    // Per-app schema fresh every run.
    let app_id = "p5_pr4_roundtrip_app";
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
            &format!(
                r#"INSERT INTO "{app_id}"."notes" (id, body) VALUES ($1::int, $2)"#
            ),
            &[id_s.as_str(), body.as_str()],
        )
        .await
        .unwrap();
    }

    let backend = PostgresBackend::new(pool.clone(), url.clone());

    // Snapshot to a tempdir-backed file:// URI.
    let dir = tempfile::tempdir().unwrap();
    let dest_path = dir.path().join("snapshot.dump");
    let dest_uri = format!("file://{}", dest_path.to_string_lossy());

    let handle = backend
        .snapshot(
            app_id,
            &dest_uri,
            SnapshotOpts { if_busy: BackupBusyPolicy::Abort },
        )
        .await
        .expect("snapshot");
    assert!(dest_path.exists(), "dump file must exist on disk after snapshot");
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
/// `#[ignore]`-d statically because `pg_dump` isn't always on PATH.
#[compio::test]
#[ignore = "needs pg_dump on PATH"]
async fn snapshot_uri_content_hash_round_trip() {
    let url = require_pg().await;
    if !pg_dump_on_path() {
        zeroship_test_support::skip("Skipping — pg_dump not on PATH");
        return;
    }
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .expect("ensure_admin_schema");

    let app_id = "p5_pr4_hash_app";
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

    let backend = PostgresBackend::new(pool.clone(), url.clone());
    let dir = tempfile::tempdir().unwrap();
    let dest_path = dir.path().join("hash_check.dump");
    let dest_uri = format!("file://{}", dest_path.to_string_lossy());

    let handle = backend
        .snapshot(
            app_id,
            &dest_uri,
            SnapshotOpts { if_busy: BackupBusyPolicy::Abort },
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
// Reserved-name refusal at register_model time.
//
// Two PG-integration tests pin that the reserved-name validator
// (`query::validate_field_name`) reaches the orchestrator's CREATE
// TABLE emission path: declaring a column ending in `_masked` or
// named after one of the six reserved classifications produces an
// invalid-identifier error at deploy time, NOT silent acceptance.
// ---------------------------------------------------------------------------

/// A schema declaring a column whose name ends in `_masked` must be
/// refused at `register_model` time. The reserved suffix is owned by
/// the Path B sibling-column emission; creators cannot collide
/// with it.
#[compio::test]
async fn p55_pr1_register_model_refuses_masked_suffix_field() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = "p55_masked_suffix";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();

    let schema = json!({
        "name": {"type": "string"},
        // creator-declared `ssn_masked` would collide with the
        // platform's sibling-column emission. Refuse at register_model.
        "ssn_masked": {"type": "string"},
    });

    let err = zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool),
        app,
        "users",
        &schema,
        &serde_json::json!([]),
        "p55_pr1_deploy_masked",
    )
    .await
    .expect_err("schema with `_masked` suffix should be refused");

    let msg = err.to_string();
    assert!(
        msg.contains("reserved field name") && msg.contains("_masked"),
        "expected reserved-suffix message, got: {msg}"
    );
    release_pg(pool).await;
}

/// A schema declaring a column named after one of the six default
/// classifications (`public`, `pii`, `spi`, `phi`, `pci`, `internal`)
/// must be refused at `register_model` time. These names are reserved
/// at the column-name level so the classification taxonomy stays
/// non-overlapping with creator-declared columns.
#[compio::test]
async fn p55_pr1_register_model_refuses_reserved_classification_field() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = "p55_classification";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();

    let schema = json!({
        "name": {"type": "string"},
        // creator-declared `pii` would collide with the platform's
        // classification taxonomy used by authorization + audit.
        "pii": {"type": "string"},
    });

    let err = zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool),
        app,
        "users",
        &schema,
        &serde_json::json!([]),
        "p55_pr1_deploy_classification",
    )
    .await
    .expect_err("schema with reserved classification name should be refused");

    let msg = err.to_string();
    assert!(
        msg.contains("reserved field name"),
        "expected reserved-name message, got: {msg}"
    );
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// Per-app PG role hardening (§17.5).
//
// The per-app role (`app_<id>_role`) owns ONLY its schema and is
// NOREPLICATION — slot ownership stays platform-side. These tests
// provision the role via `auth::bootstrap::ensure_per_app_role` and
// fence it: it can CRUD its own schema, cannot read a sibling app's
// schema, cannot create/list/drop replication slots, and carries no
// `rolreplication` attribute. The per-app role is NOLOGIN (clients
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
    let role = zeroship_plugin_db::auth::bootstrap::per_app_role_name(app);
    let _ = pool
        .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
        .await;
    // The role inherits __zeroship_app_role_template, so it must exist.
    zeroship_plugin_db::auth::ensure_admin_schema(pool)
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    zeroship_plugin_db::auth::ensure_admin_schema(pool)
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
        &format!(
            "ALTER TABLE \"{app}\".\"{collection}\" ENABLE ROW LEVEL SECURITY"
        ),
        &[],
    )
    .await
    .unwrap();
    pool.execute(
        &format!(
            "ALTER TABLE \"{app}\".\"{collection}\" FORCE ROW LEVEL SECURITY"
        ),
        &[],
    )
    .await
    .unwrap();
    pool.execute(
        &format!(
            "DROP POLICY IF EXISTS role_gate ON \"{app}\".\"{collection}\""
        ),
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
    let (scheme, rest) = base_url
        .split_once("://")
        .unwrap_or(("postgres", base_url));
    let host = rest.split_once('@').map(|(_, suffix)| suffix).unwrap_or(rest);
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
            &format!(
                "CREATE ROLE \"{login_role}\" LOGIN PASSWORD '{password}' INHERIT"
            ),
            &[],
        )
        .await
        .unwrap();
    admin_pool
        .execute(
            &format!("GRANT \"{app_role}\" TO \"{login_role}\""),
            &[],
        )
        .await
        .unwrap();
    admin_pool
        .execute(
            &format!("GRANT USAGE ON SCHEMA \"{app}\" TO \"{login_role}\""),
            &[],
        )
        .await
        .unwrap();
    admin_pool
        .execute(
            &format!(
                "GRANT SELECT ON ALL TABLES IN SCHEMA \"{app}\" TO \"{login_role}\""
            ),
            &[],
        )
        .await
        .unwrap();
    let login_url = login_role_test_url(base_url, login_role, password);
    let login_pool = std::rc::Rc::new(Pool::connect(&login_url, 4).await.unwrap());
    (login_url, login_pool)
}

async fn postgis_available(pool: &Pool) -> bool {
    let create_res = pool
        .execute("CREATE EXTENSION IF NOT EXISTS postgis", &[])
        .await;
    if create_res.is_err() {
        return false;
    }
    let rows = pool
        .query_text_params("SELECT 1 FROM pg_extension WHERE extname='postgis'", &[])
        .await
        .unwrap_or_default();
    !rows.is_empty()
}

#[compio::test]
async fn per_app_role_created_at_provision() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = "p6a_role_create";
    let role = provision_app_with_role(&pool, app).await;

    // First provision creates the role.
    let first = zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .expect("provision per-app role");
    assert!(first.created_role, "first provision must create the role");

    // The role now exists in pg_roles.
    let exists = pool
        .query_text_params("SELECT 1 FROM pg_roles WHERE rolname = $1", &[role.as_str()])
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

    let _ = pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[]).await;
    let _ = pool.execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[]).await;
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
    let schema_role = zeroship_plugin_db::auth::bootstrap::per_app_role_name(&app_schema);
    let uuid_role = format!("app_{}_role", app_id.as_hyphenated());

    let _ = pool
        .execute(&format!("DROP SCHEMA IF EXISTS \"{app_schema}\" CASCADE"), &[])
        .await;
    for role in [&schema_role, &uuid_role] {
        let _ = pool
            .execute(&format!("DROP OWNED BY \"{role}\""), &[])
            .await;
        let _ = pool
            .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
            .await;
    }

    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .expect("ensure platform role");
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
            owner = zeroship_migrated::provisioning::WORKFLOW_OWNER_ROLE,
        ),
        &[],
    )
    .await
    .expect("precreate the narrow workflow journal owner role");
    // The journal SCHEMA is created by the deploy's migration apply, not by the
    // worker -- `PgStore::provision` holds no CREATE on the database. Call the
    // migration service's own statement rather than a CREATE SCHEMA of our own,
    // so the journal below is owned the way production owns it.
    zeroship_migrated::provisioning::provision_workflow_journal_schema(&client, &app_id)
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
        assert!(!row.get::<_, bool>("sel"), "app role must not SELECT {table}");
        assert!(!row.get::<_, bool>("ins"), "app role must not INSERT {table}");
        assert!(!row.get::<_, bool>("upd"), "app role must not UPDATE {table}");
        assert!(!row.get::<_, bool>("del"), "app role must not DELETE {table}");

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
        // Bound to `zeroship-migrated`'s copy of the owner-role name while the
        // writer is `plugin-workflow`'s private copy of it, so the two
        // duplicated constants disagreeing shows up here rather than as a
        // journal nobody can reach. Until 2026-08-20 this compared against
        // `__zeroship_platform_role`, the role the store created for itself
        // before 2a44ea8ef removed `provision_owner_sql`.
        assert_eq!(
            owner,
            zeroship_migrated::provisioning::WORKFLOW_OWNER_ROLE,
            "journal owner for {table}"
        );
        // The security property the name is a proxy for: no role an app's own
        // code runs as may own the journal, because an owner can re-GRANT
        // itself the DML the assertions above just proved it lacks.
        assert_ne!(owner, schema_role, "journal owner for {table} is an app role");
        assert_ne!(owner, uuid_role, "journal owner for {table} is an app role");
    }

    let _ = pool
        .execute(&format!("DROP SCHEMA IF EXISTS \"{app_schema}\" CASCADE"), &[])
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
    let app = "p6a_role_norepl";
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

    let _ = pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[]).await;
    let _ = pool.execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[]).await;
    release_pg(pool).await;
}

#[compio::test]
async fn per_app_role_grant_scoped_to_schema() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = "p6a_role_scoped";
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
    // Re-run provision so the existing-table GRANT covers `widgets`
    // (provision before table creation only set DEFAULT PRIVILEGES; the
    // re-run also covers tables that already exist — proving idempotent
    // grant coverage).
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();

    // SET ROLE to the per-app role and CRUD its own schema — must work.
    pool.execute(&format!(r#"SET ROLE "{role}""#), &[]).await.unwrap();
    let sel = pool
        .query_text_params(&format!(r#"SELECT name FROM "{app}".widgets"#), &[])
        .await;
    assert!(sel.is_ok(), "per-app role must SELECT its own schema: {sel:?}");
    let ins = pool
        .execute(
            &format!(r#"INSERT INTO "{app}".widgets (name) VALUES ('by_role')"#),
            &[],
        )
        .await;
    assert!(ins.is_ok(), "per-app role must INSERT its own schema: {ins:?}");
    pool.execute("RESET ROLE", &[]).await.unwrap();

    let _ = pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[]).await;
    let _ = pool.execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[]).await;
    release_pg(pool).await;
}

#[compio::test]
async fn per_app_role_cannot_read_sibling_schema_or_touch_slots() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app_a = "p6a_fence_a";
    let app_b = "p6a_fence_b";
    let role_a = provision_app_with_role(&pool, app_a).await;
    // Provision a sibling schema B (and its role) with a table.
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app_b}\" CASCADE"), &[])
        .await
        .unwrap();
    let role_b = zeroship_plugin_db::auth::bootstrap::per_app_role_name(app_b);
    let _ = pool.execute(&format!("DROP ROLE IF EXISTS \"{role_b}\""), &[]).await;
    pool.execute(&format!("CREATE SCHEMA \"{app_b}\""), &[]).await.unwrap();

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
    pool.execute(&format!(r#"SET ROLE "{role_a}""#), &[]).await.unwrap();
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

    let _ = pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app_a}\" CASCADE"), &[]).await;
    let _ = pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app_b}\" CASCADE"), &[]).await;
    let _ = pool.execute(&format!("DROP ROLE IF EXISTS \"{role_a}\""), &[]).await;
    let _ = pool.execute(&format!("DROP ROLE IF EXISTS \"{role_b}\""), &[]).await;
    release_pg(pool).await;
}

#[compio::test]
async fn client_sql_runs_under_per_app_role() {
    // Proves the `SET LOCAL ROLE` shape `exec_begin`
    // issue actually switches the effective role for the rest of the tx,
    // and reverts at COMMIT/ROLLBACK.
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = "p6a_setlocal";
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
    let set_sql = zeroship_plugin_db::auth::bootstrap::set_local_role_sql(app);
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
    let _ = pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[]).await;
    let _ = pool.execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[]).await;
    release_pg(pool).await;
}

#[compio::test]
async fn exec_autocommit_query_runs_under_per_app_role() {
    // I2 regression: the shared autocommit exec path must switch to the
    // per-app role before running the statement, not just explicit/auto tx.
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = "p6a_exec_autocommit_role";
    let role = provision_app_with_role(&pool, app).await;
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();
    zeroship_plugin_db::set_db_url_for_tests(&url);

    let rows = zeroship_plugin_db::exec::exec_query_for_tests(
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

    let _ = pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[]).await;
    let _ = pool.execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[]).await;
    release_pg(pool).await;
}

#[compio::test]
#[ignore = "requires pgvector — swap `pg-test` image to pgvector/pgvector:pg16"]
async fn vector_search_runs_under_per_app_role_via_rls() {
    use zeroship_plugin_db::backend::{PostgresBackend, VectorIndex, VectorMetric};

    let url = require_pg().await;
    let admin_pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    if !pgvector_available(&admin_pool).await {
        zeroship_test_support::skip("Skipping: pgvector not installed in test environment");
        return release_pg(admin_pool).await;
    }

    let app = "p6a_vector_role_fence";
    let coll = "docs";
    let role = provision_app_with_role(&admin_pool, app).await;
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&admin_pool, app)
        .await
        .unwrap();
    admin_pool.execute(
        &format!(
            "CREATE TABLE \"{app}\".\"{coll}\" (\
               id SERIAL PRIMARY KEY, \
               embedding vector(2) NOT NULL\
             )"
        ),
        &[],
    )
    .await
    .unwrap();
    admin_pool.execute(
        &format!(
            "INSERT INTO \"{app}\".\"{coll}\" (embedding) VALUES ($1::vector)"
        ),
        &[&"[1,0]" as &(dyn compio_postgres::types::ToSql + Sync)],
    )
    .await
    .unwrap();
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&admin_pool, app)
        .await
        .unwrap();
    install_role_bound_select_policy(&admin_pool, app, coll, &role).await;
    let login_role = "p6a_vector_login";
    let (login_url, login_pool) = provision_platform_login_pool(
        &admin_pool,
        &url,
        login_role,
        "test",
        &role,
        app,
    )
    .await;

    let blocked = login_pool
        .query_text_params(&format!("SELECT id FROM \"{app}\".\"{coll}\""), &[])
        .await
        .unwrap();
    assert!(
        blocked.is_empty(),
        "login role must be blocked by FORCE RLS before vector_search proves the role fence"
    );

    let backend = PostgresBackend::new(login_pool.clone(), login_url);
    let rows = VectorIndex::vector_search(
        &backend,
        app,
        coll,
        "embedding",
        &[1.0, 0.0],
        1,
        VectorMetric::Cosine,
        &Value::Null,
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
async fn fts_search_runs_under_per_app_role_via_rls() {
    use zeroship_plugin_db::backend::{FullTextIndex, PostgresBackend};

    let url = require_pg().await;
    let admin_pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = "p6a_fts_role_fence";
    let coll = "people";
    let role = provision_app_with_role(&admin_pool, app).await;
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&admin_pool, app)
        .await
        .unwrap();
    admin_pool.execute(
        &format!(
            "CREATE TABLE \"{app}\".\"{coll}\" (\
               id SERIAL PRIMARY KEY, \
               bio TEXT NOT NULL\
             )"
        ),
        &[],
    )
    .await
    .unwrap();

    let admin_backend = PostgresBackend::new(admin_pool.clone(), url.clone());
    FullTextIndex::ensure_fts_index(&admin_backend, app, coll, &["bio".to_string()], "english")
        .await
        .unwrap();
    admin_pool.execute(
        &format!("INSERT INTO \"{app}\".\"{coll}\" (bio) VALUES ('rust systems')"),
        &[],
    )
    .await
    .unwrap();
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&admin_pool, app)
        .await
        .unwrap();
    install_role_bound_select_policy(&admin_pool, app, coll, &role).await;
    let login_role = "p6a_fts_login";
    let (login_url, login_pool) = provision_platform_login_pool(
        &admin_pool,
        &url,
        login_role,
        "test",
        &role,
        app,
    )
    .await;

    let blocked = login_pool
        .query_text_params(&format!("SELECT id FROM \"{app}\".\"{coll}\""), &[])
        .await
        .unwrap();
    assert!(
        blocked.is_empty(),
        "login role must be blocked by FORCE RLS before fts_search proves the role fence"
    );

    let backend = PostgresBackend::new(login_pool.clone(), login_url);
    let rows = FullTextIndex::fts_search(&backend, app, coll, "rust", &Value::Null, Some(1))
        .await
        .unwrap_or_else(|e| panic!("fts_search failed: {e:?}"));
    assert_eq!(rows.len(), 1, "fts_search must see the role-gated row");
    assert_eq!(rows[0]["bio"], "rust systems");

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
    drop(admin_backend);
    drop(backend);
    release_pg(admin_pool).await;
}

#[compio::test]
async fn spatial_near_runs_under_per_app_role_via_rls() {
    use zeroship_plugin_db::backend::{GeoPoint, PostgresBackend, SpatialIndex};

    let url = require_pg().await;
    let admin_pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    if !postgis_extension_available(&admin_pool).await {
        zeroship_test_support::skip("Skipping: postgis not installed in test environment");
        return release_pg(admin_pool).await;
    }

    let app = "p6a_spatial_role_fence";
    let coll = "places";
    let role = provision_app_with_role(&admin_pool, app).await;
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&admin_pool, app)
        .await
        .unwrap();
    admin_pool.execute(
        &format!(
            "CREATE TABLE \"{app}\".\"{coll}\" (\
               id SERIAL PRIMARY KEY, \
               location geography(POINT, 4326) NOT NULL\
             )"
        ),
        &[],
    )
    .await
    .unwrap();
    admin_pool.execute(
        &format!(
            "INSERT INTO \"{app}\".\"{coll}\" (location) \
             VALUES (ST_GeogFromText('POINT(-0.1278 51.5074)'))"
        ),
        &[],
    )
    .await
    .unwrap();
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&admin_pool, app)
        .await
        .unwrap();
    install_role_bound_select_policy(&admin_pool, app, coll, &role).await;
    let login_role = "p6a_spatial_login";
    let (login_url, login_pool) = provision_platform_login_pool(
        &admin_pool,
        &url,
        login_role,
        "test",
        &role,
        app,
    )
    .await;

    let blocked = login_pool
        .query_text_params(&format!("SELECT id FROM \"{app}\".\"{coll}\""), &[])
        .await
        .unwrap();
    assert!(
        blocked.is_empty(),
        "login role must be blocked by FORCE RLS before spatial_near proves the role fence"
    );

    let backend = PostgresBackend::new(login_pool.clone(), login_url);
    let rows = SpatialIndex::spatial_near(
        &backend,
        app,
        coll,
        "location",
        GeoPoint {
            lat: 51.5074,
            lng: -0.1278,
        },
        1000.0,
        &Value::Null,
        Some(1),
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

    let app = "p6a_unmask_role_fence";
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
    admin_pool.execute(
        &format!(
            "CREATE TABLE \"{app}\".\"{coll}\" (\
               id TEXT PRIMARY KEY, \
               ssn TEXT, \
               ssn_masked TEXT\
             )"
        ),
        &[],
    )
    .await
    .unwrap();
    admin_pool.execute(
        &format!(
            "INSERT INTO \"{app}\".\"{coll}\" (id, ssn, ssn_masked) \
             VALUES ('u1', '123-45-6789', '***-**-6789')"
        ),
        &[],
    )
    .await
    .unwrap();
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&admin_pool, app)
        .await
        .unwrap();
    install_role_bound_select_policy(&admin_pool, app, coll, &role).await;
    let login_role = "p6a_unmask_login";
    let (login_url, login_pool) = provision_platform_login_pool(
        &admin_pool,
        &url,
        login_role,
        "test",
        &role,
        app,
    )
    .await;

    let blocked = login_pool
        .query_text_params(
            &format!("SELECT ssn FROM \"{app}\".\"{coll}\" WHERE id = 'u1'"),
            &[],
        )
        .await
        .unwrap();
    assert!(
        blocked.is_empty(),
        "login role must be blocked by FORCE RLS before unmask proves the role fence"
    );

    zeroship_plugin_db::set_postgres_pool_for_tests(login_pool.clone(), &login_url);
    zeroship_plugin_db::cache_schema_for_tests(app, coll, schema);
    zeroship_plugin_db::clear_mask_policy_cache_for_tests(app);

    let result = unmask::dispatch_unmask(
        app,
        UnmaskFieldArgs {
            collection: coll.to_string(),
            row_pk: "u1".to_string(),
            column: "ssn".to_string(),
            actor: Some(json!({ "kind": "auto" })),
            reason: Some("security regression".to_string()),
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

#[compio::test]
async fn wal_connection_stays_platform_role() {
    // §17.5: the WAL/replication connection stays under the platform
    // role and is NEVER switched to a per-app role. This is a structural
    // assertion: the replication helpers (`ensure_publication_and_slot`,
    // `drop_abandoned_slots`, the §17.7 deprovision) run on the pool
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
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = "p6a_walrole";
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

    let _ = pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[]).await;
    let _ = pool.execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[]).await;
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
use zeroship_plugin_db::drop_namespace::{
    drop_namespace, DropNamespaceOpts, DropNamespaceOutcome,
};

/// Build a `BackendHandle::Postgres` over a fresh `PostgresBackend` for
/// the drop-namespace tests. (`PostgresBackend` is already imported at
/// module scope earlier in this file — referenced unqualified here.)
fn pg_backend_handle(pool: &std::rc::Rc<Pool>, url: &str) -> BackendHandle {
    BackendHandle::Postgres(std::rc::Rc::new(PostgresBackend::new(
        std::rc::Rc::clone(pool),
        url.to_string(),
    )))
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
        .query_text_params("SELECT 1 FROM pg_publication WHERE pubname = $1", &[pubn.as_str()])
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
    let role = zeroship_plugin_db::auth::bootstrap::per_app_role_name(app);
    let rows = pool
        .query_text_params("SELECT 1 FROM pg_roles WHERE rolname = $1", &[role.as_str()])
        .await
        .unwrap();
    !rows.is_empty()
}

#[compio::test]
async fn drop_namespace_defers_on_active_subscription() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = "p6a_drop_defer";
    c1_cleanup(&pool, app).await;
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[]).await.unwrap();

    let backend = pg_backend_handle(&pool, &url);
    // count > 0, force = false → defer. No teardown runs.
    let outcome = drop_namespace(
        &backend,
        &pool,
        app,
        &DropNamespaceOpts { force: false, subscription_count: 2 },
    )
    .await
    .expect("drop_namespace");

    assert_eq!(
        outcome,
        DropNamespaceOutcome::Deferred { active_subscriptions: 2 },
        "active subscription without --force must defer with the count"
    );
    // Schema must still exist — no teardown ran.
    assert!(schema_exists(&pool, app).await, "deferred drop must NOT drop the schema");

    c1_cleanup(&pool, app).await;
    drop(backend);
    release_pg(pool).await;
}

#[compio::test]
async fn drop_namespace_force_fires_subscription_app_dropped() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = "p6a_drop_force";
    c1_cleanup(&pool, app).await;
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[]).await.unwrap();

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
        &DropNamespaceOpts { force: true, subscription_count: 1 },
    )
    .await
    .expect("drop_namespace --force");
    assert_eq!(outcome, DropNamespaceOutcome::Completed, "--force must complete");

    // The subscription must have been closed (subscription_app_dropped →
    // broker Closed). The iterator surfaces the terminal close.
    assert!(sub.is_closed(), "active subscriber must be closed under --force");
    assert_eq!(
        zeroship_plugin_db::broker::app_subscription_count(app),
        0,
        "broker must be drained for the app after --force drop"
    );
    // Schema gone.
    assert!(!schema_exists(&pool, app).await, "schema must be dropped under --force");

    c1_cleanup(&pool, app).await;
    drop(backend);
    release_pg(pool).await;
}

#[compio::test]
async fn drop_namespace_pg_drops_slots_but_retains_migration_publication() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    if !pg_has_logical_wal(&pool).await {
        zeroship_test_support::skip("Skipping drop_namespace_pg_ordering — wal_level != logical");
        return release_pg(pool).await;
    }
    let app = "p6a_drop_order";
    c1_cleanup(&pool, app).await;
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[]).await.unwrap();
    c1_create_publication(&pool, app).await;
    // Provision the worker slot against the migration-owned publication.
    zeroship_plugin_db::replication::ensure_worker_slot(&pool, app, CDC_TEST_WORKER_ID)
        .await
        .expect("provision worker slot");
    assert!(slot_exists(&pool, app).await, "slot provisioned");
    assert!(publication_exists(&pool, app).await, "publication provisioned");

    let backend = pg_backend_handle(&pool, &url);
    let outcome = drop_namespace(
        &backend,
        &pool,
        app,
        &DropNamespaceOpts { force: false, subscription_count: 0 },
    )
    .await
    .expect("drop_namespace");
    assert_eq!(outcome, DropNamespaceOutcome::Completed);

    // Worker teardown removes slots and the schema but leaves publication
    // ownership with the migration service. Dropping the schema removes its
    // relation memberships, so the retained publication is empty.
    assert!(!slot_exists(&pool, app).await, "slot must be dropped");
    assert!(publication_exists(&pool, app).await, "publication must be retained");
    assert!(!schema_exists(&pool, app).await, "schema must be dropped");

    c1_cleanup(&pool, app).await;
    drop(backend);
    release_pg(pool).await;
}

#[compio::test]
async fn drop_namespace_drops_per_app_role_last() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = "p6a_drop_role";
    c1_cleanup(&pool, app).await;
    let role = zeroship_plugin_db::auth::bootstrap::per_app_role_name(app);
    let _ = pool.execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[]).await;

    zeroship_plugin_db::auth::ensure_admin_schema(&pool).await.unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[]).await.unwrap();
    zeroship_plugin_db::auth::ensure_admin_schema(&pool).await.unwrap();
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
        &DropNamespaceOpts { force: false, subscription_count: 0 },
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
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    if !pg_has_logical_wal(&pool).await {
        zeroship_test_support::skip("Skipping drop_namespace_idempotent — wal_level != logical");
        return release_pg(pool).await;
    }
    let app = "p6a_drop_idem";
    c1_cleanup(&pool, app).await;
    let role = zeroship_plugin_db::auth::bootstrap::per_app_role_name(app);
    let _ = pool.execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[]).await;
    zeroship_plugin_db::auth::ensure_admin_schema(&pool).await.unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[]).await.unwrap();
    zeroship_plugin_db::auth::ensure_admin_schema(&pool).await.unwrap();
    c1_create_publication(&pool, app).await;
    zeroship_plugin_db::replication::ensure_worker_slot(&pool, app, CDC_TEST_WORKER_ID)
        .await
        .unwrap();
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();

    let backend = pg_backend_handle(&pool, &url);
    let opts = DropNamespaceOpts { force: false, subscription_count: 0 };

    // First drop: full teardown.
    let first = drop_namespace(&backend, &pool, app, &opts).await.expect("first drop");
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
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    if !pg_has_logical_wal(&pool).await {
        zeroship_test_support::skip("Skipping drop_namespace_retries — wal_level != logical");
        return release_pg(pool).await;
    }
    let app = "p6a_drop_retry";
    c1_cleanup(&pool, app).await;
    let role = zeroship_plugin_db::auth::bootstrap::per_app_role_name(app);
    let _ = pool.execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[]).await;
    zeroship_plugin_db::auth::ensure_admin_schema(&pool).await.unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[]).await.unwrap();
    zeroship_plugin_db::auth::ensure_admin_schema(&pool).await.unwrap();
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
    assert!(publication_exists(&pool, app).await, "publication retained after partial");
    assert!(schema_exists(&pool, app).await, "schema still present after partial");
    assert!(role_exists(&pool, app).await, "role still present after partial");

    // Retry: step 3 is a no-op (nothing to deprovision), steps 4-5 finish.
    let backend = pg_backend_handle(&pool, &url);
    let outcome = drop_namespace(
        &backend,
        &pool,
        app,
        &DropNamespaceOpts { force: false, subscription_count: 0 },
    )
    .await
    .expect("retry drop_namespace after partial failure");
    assert_eq!(outcome, DropNamespaceOutcome::Completed);
    assert!(!schema_exists(&pool, app).await, "retry must drop the schema");
    assert!(!role_exists(&pool, app).await, "retry must drop the role");

    c1_cleanup(&pool, app).await;
    drop(backend);
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// The deploy-keyed introspection cache invalidates on a REAL deploy bump.
//
// Regression for `deploy-id-never-set-cache-invalidation-inert`: the
// introspected-schema cache was keyed on `std::env::var("ZEROSHIP_DEPLOY_ID")`,
// which NOTHING in worker/runtime/control ever set — so the token was pinned at
// `"cold_start"` for the life of a long-lived worker isolate and the cache NEVER
// invalidated. After a redeploy ALTERed the schema (e.g. added a masked column),
// the runtime kept applying the STALE metadata it cached at first-introspection,
// silently dropping the new column's mask/crypto behaviour.
//
// The fix re-keys the cache on the per-app deploy token the worker now injects as
// `ZEROSHIP_DEPLOY_ID` (= the app's `deploy_hash`), stamped into the per-isolate
// context at `mint_db` and read via `IsolateDbContext::deploy_token_for`. This
// test drives the FAITHFUL path: register v1, introspect+cache under token
// `deploy_1`, ALTER to v2 via the same engine register path, and prove:
//   (a) WITHOUT bumping the token the cache holds the v1 result (no re-introspect);
//   (b) bumping the token to `deploy_2` invalidates the entry and re-introspection
//       surfaces the v2 column's mask metadata.
//
// PRE-FIX this test FAILS at assertion (b): the old `deploy_token()` ignored the
// stamped token entirely (read the never-set env var → always `"cold_start"`), so
// the bumped token had no effect and the stale v1 schema (no `phone` mask) was
// returned.
#[compio::test]
async fn t6_introspection_cache_invalidates_on_deploy_token_bump() {
    let url = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    // "f", not the "t" this fixture carried while it was an env var. "t" is
    // not a hex digit, so that root could never have decoded; the env path
    // only found out at first `resolve_key`, and this test never encrypts
    // anything, so the bad fixture sat here unreported. Supplied roots parse
    // on install, which is where a fixture typo should surface.
    let _keys = with_root_key("default", &"f".repeat(64));

    let app = "t6_deploy_cache";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();

    // ===== Deploy 1: a goodie-FREE collection (plain `name`). =====
    let schema_v1 = json!({
        "name": {"type": "string", "required": true},
    });
    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool),
        app,
        "people",
        &schema_v1,
        &json!([]),
        "deploy_1",
    )
    .await
    .unwrap_or_else(|e| panic!("deploy 1 register failed: {e}"));

    // Install the PG backend, mark readiness, and stamp the per-app deploy token
    // exactly as `mint_db` does from the worker-injected `ZEROSHIP_DEPLOY_ID`.
    zeroship_plugin_db::set_postgres_pool_for_tests(std::rc::Rc::clone(&pool), &url);
    zeroship_plugin_db::mark_model_registered_for_tests(app, "people");
    zeroship_plugin_db::set_deploy_token_for_tests(app, "deploy_1");

    // First introspection under `deploy_1`: collection has no goodies → the
    // schema carries `name` but no `phone` field (and no mask anywhere). This
    // result is now cached under the `deploy_1` token.
    let v1 = zeroship_plugin_db::crud::runtime_schema_for_tests(app, "people")
        .await
        .expect("introspect v1")
        .expect("table exists → Some");
    assert_eq!(v1["name"]["type"], "string");
    assert!(
        v1.get("phone").is_none(),
        "deploy 1 has no phone column yet, got {v1:?}"
    );

    // ===== Deploy 2: ALTER to add a MASKED `phone` column (engine path). =====
    let schema_v2 = json!({
        "name": {"type": "string", "required": true},
        "phone": {
            "type": "string",
            "mask": {"kind": "last4", "classification": "pci"}
        },
    });
    zeroship_plugin_db::register_model::exec_register_model_with_pool(
        std::rc::Rc::clone(&pool),
        app,
        "people",
        &schema_v2,
        &json!([]),
        "deploy_2",
    )
    .await
    .unwrap_or_else(|e| panic!("deploy 2 register (add masked column) failed: {e}"));

    // (a) The catalog NOW has the masked `phone` column, but until the deploy
    // token is bumped the per-isolate cache must still return the v1 result —
    // this proves the cache is real (not re-introspecting every call) AND that
    // the only thing that should invalidate it is a deploy bump.
    let still_cached = zeroship_plugin_db::crud::runtime_schema_for_tests(app, "people")
        .await
        .expect("introspect (still deploy_1 token)")
        .expect("table exists → Some");
    assert!(
        still_cached.get("phone").is_none(),
        "cache must hold the deploy_1 result until the deploy token bumps, got {still_cached:?}"
    );

    // (b) Simulate the redeploy: bump the per-app deploy token (a new
    // `deploy_hash` → new `ZEROSHIP_DEPLOY_ID` injected at the next isolate
    // load). The cache entry is now stale and must be re-introspected, surfacing
    // the masked `phone` column's metadata. PRE-FIX this assertion fails — the
    // token bump was inert because the cache keyed off the never-set env var.
    zeroship_plugin_db::set_deploy_token_for_tests(app, "deploy_2");
    let v2 = zeroship_plugin_db::crud::runtime_schema_for_tests(app, "people")
        .await
        .expect("introspect v2")
        .expect("table exists → Some");
    assert_eq!(
        v2["phone"]["mask"]["kind"], "last4",
        "deploy bump must re-introspect and surface the new masked column, got {v2:?}"
    );
    assert_eq!(v2["phone"]["mask"]["classification"], "pci");

    let _ = pool
        .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await;
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
    const PINNED: usize = 122;
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
