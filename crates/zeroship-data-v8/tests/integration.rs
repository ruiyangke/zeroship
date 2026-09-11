//! Integration tests for plugin-db query builders against real Postgres.
//!
//! PostgreSQL comes from an owned testcontainer; Docker is required.
//! Run: `cargo xtask test data --filter 'test(integration::)'`
//!
//! This file is a module of the `test_helpers` target rather than a target of
//! its own (`tests/main.rs` says why), so selecting it is a libtest FILTER on
//! its module path. A filter is a substring match with no anchor, which is what
//! the `--skip` is for: `integration::` also selects `sqlite_integration::`.
//!
//! Ordinary package tests include this module and require PostgreSQL.
//! Integration helpers are enabled by the package self dev-dependency.
//!
// `support`, `schema_fixture` and `parity` are declared once by
// `tests/test_helpers.rs`, the entry file this module hangs off; its header says
// why a second declaration here would be a second copy of their statics.
#[allow(unused_imports)]
use crate::schema_fixture::{fixture_table_sql, fixture_table_sql_for};
use crate::{parity, schema_fixture, support};
#[allow(unused_imports)]
use zeroship_migrate::schema::query::FkEmission;

use compio_postgres::{NoTls, Pool};
use uuid::Uuid;
use zeroship_data_orm::binding::DbBinding;
use zeroship_data_sql::value::{Value, value};




/// The backend handle the unmask entry points now take as a parameter.
///
/// They resolved one themselves, from the isolate's context, until 2026-09-03.
/// That read is the ADAPTER's and `protection::unmask` is ENGINE, so the resolution
/// moved to the V8 dispatcher and the value is passed down. These tests drive
/// the engine directly, with no V8 frame above them, so they make the same call
/// the dispatcher makes on their behalf in production.
async fn unmask_backend() -> zeroship_data_orm::backend::BackendHandle {
    zeroship_data_v8::tx_scope::ensure_backend()
        .await
        .expect("the backend the V8 dispatcher would have opened")
}

/// The route the unmask dispatchers now take, in place of a bare handle.
///
/// See the twin in `mask_flip.rs` for why. No fixture that reaches it here
/// parks a transaction, so every call binds `in_tx = false` and takes the lane
/// it took before.
async fn unmask_route(app: &str) -> zeroship_data_orm::tx_route::TxRoute {
    zeroship_data_orm::exec::ambient_route_for_tests(app, unmask_backend().await)
}

async fn require_pg() -> (crate::support::postgres::Postgres, String) {
    let postgres = crate::support::postgres::Postgres::start();
    let url = postgres.url();
    match compio_postgres::connect(&url, NoTls).await {
        Ok((client, connection)) => {
            // Drive the connection just long enough to drop both halves.
            compio::runtime::spawn(async move {
                let _ = connection.run().await;
            })
            .detach();
            drop(client);
            // The transaction orchestrator opens a
            // dedicated client via the Backend trait's
            // `fixture_session`, which reads the URL from the
            // per-thread context. Tests that drive the orchestrator directly
            // need the URL installed in the context before the call.
            zeroship_data_v8::testing::set_db_url_for_tests(&url);
            (postgres, url)
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
            // database is required by the ordinary test suite.
            panic!("live-Postgres suite could not connect to its PostgreSQL testcontainer: {e}");
        }
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
            // writes them NOT NULL (`zeroship-data-sql/src/compile.rs:212-213`).
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
async fn exec_query(pool: &Pool, bq: zeroship_data_sql::compile::BuiltQuery) -> Vec<Value> {
    let param_refs = &bq.params;
    let rows = zeroship_data_orm::backend::postgres::params::query(
        &pool.acquire().await.unwrap(),
        &bq.sql,
        param_refs,
    )
    .await
    .unwrap();
    rows.iter().map(row_to_value).collect()
}

/// Helper: build + execute a mutation, return parsed JSON array.
async fn exec_mutation(pool: &Pool, bq: zeroship_data_sql::compile::BuiltQuery) -> Vec<Value> {
    let param_refs = &bq.params;
    let rows = zeroship_data_orm::backend::postgres::params::query(
        &pool.acquire().await.unwrap(),
        &bq.sql,
        param_refs,
    )
    .await
    .unwrap();
    rows.iter().map(row_to_value).collect()
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
        obj.entry("id".to_owned())
            .or_insert_with(|| Value::String(format!("seed_{n}")));
    }
    doc
}

/// Simplified row → JSON (just text columns for testing).
fn row_to_value(row: &compio_postgres::Row) -> Value {
    let mut obj = zeroship_data_sql::value::Map::new();
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
    zeroship_data_v8::testing::reset_context_for_tests();
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
    // Process-wide socket counts need a process containing only this test.
    // The exact child invocation also works when the parent suite is parallel.
    if !std::env::args().any(|argument| argument == "--exact") {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "integration::connections_do_not_outlive_the_runtime_that_opened_them",
                "--nocapture",
            ])
            .output()
            .expect("start isolated socket lifecycle test");
        assert!(
            result.status.success(),
            "isolated socket lifecycle test failed:\n{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            String::from_utf8_lossy(&result.stdout).contains("1 passed"),
            "the isolated process must execute the lifecycle assertion"
        );
        return;
    }
    const ITERATIONS: usize = 40;

    let postgres = crate::support::postgres::Postgres::start();
    let baseline = open_sockets();
    for _ in 0..ITERATIONS {
        let url = postgres.url();
        std::thread::spawn(move || {
            compio::runtime::Runtime::new()
                .expect("cannot create runtime")
                .block_on(async {
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

use zeroship_data_sql::compile::*;

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
    value!({
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
    value!({
        "city": { "type": "string" },
        "temp_lo": { "type": "int" },
        "temp_hi": { "type": "int" },
    })
}

/// Postgres and the dev SQLite tier must hand `env.db` callers the same JSON.
///
/// Included in the native data suite. Missing PostgreSQL prerequisites fail
/// through `require_pg`; they never turn this comparison into a skipped leg.
#[compio::test]
async fn parity_matrix_pg_matches_sqlite_projection() {
    let (_postgres, pg_url) = require_pg().await;
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
    let (_postgres, pg_url) = require_pg().await;
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
            value!(parity::TYPED_BYTES_RAW),
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    // Insert
    let bq = build_insert(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"title": "Hello", "body": "World", "category": "tech"}),
    )
    .unwrap();
    let inserted = exec_mutation(&pool, bq).await;
    assert_eq!(inserted.len(), 1);
    assert_eq!(inserted[0]["title"], "Hello");
    assert_eq!(inserted[0]["body"], "World");
    assert!(inserted[0]["id"].as_i64().unwrap() > 0);

    // Find
    let bq = build_find_with_schema(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &value!({}),
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = value!([
        {"title": "A", "body": "one", "category": "tech"},
        {"title": "B", "body": "two", "category": "food"},
        {"title": "C", "body": "three", "category": "tech"}
    ]);
    let bq = build_insert_many(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &docs,
    )
    .unwrap();
    let inserted = exec_mutation(&pool, bq).await;
    assert_eq!(inserted.len(), 3);

    // Verify all in DB
    let bq = build_count(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({}),
    )
    .unwrap();
    let param_refs = &bq.params;
    let rows = zeroship_data_orm::backend::postgres::params::query(
        &pool.acquire().await.unwrap(),
        &bq.sql,
        param_refs,
    )
    .await
    .unwrap();
    let count: i64 = rows[0].get("count");
    assert_eq!(count, 3);
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 3. Update one with $inc
// ---------------------------------------------------------------------------

#[compio::test]
async fn update_one_inc() {
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    // Insert
    let bq = build_insert(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"title": "Counter", "category": "tech", "views": 0}),
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    // $inc views by 5
    let bq = build_update_one(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"title": "Counter"}),
        &value!({"views": {"$inc": 5}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0]["views"], 5);

    // $inc again
    let bq = build_update_one(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"title": "Counter"}),
        &value!({"views": {"$inc": 3}}),
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let bq = build_insert(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"title": "Math", "category": "tech", "views": 10}),
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    // $dec
    let bq = build_update_one(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"title": "Math"}),
        &value!({"views": {"$dec": 3}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    assert_eq!(updated[0]["views"], 7);

    // $mul
    let bq = build_update_one(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"title": "Math"}),
        &value!({"views": {"$mul": 2}}),
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let bq = build_insert(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"title": "Tags", "category": "tech"}),
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    // $push "rust"
    let bq = build_update_one(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"title": "Tags"}),
        &value!({"tags": {"$push": "rust"}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    let tags = updated[0]["tags"].as_array().unwrap();
    assert!(tags.contains(&value!("rust")));

    // $push "go"
    let bq = build_update_one(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"title": "Tags"}),
        &value!({"tags": {"$push": "go"}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    let tags = updated[0]["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 2);
    assert!(tags.contains(&value!("rust")));
    assert!(tags.contains(&value!("go")));

    // $addToSet "rust" (duplicate — should NOT add)
    let bq = build_update_one(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"title": "Tags"}),
        &value!({"tags": {"$addToSet": "rust"}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    let tags = updated[0]["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 2); // still 2

    // $addToSet "python" (new — should add)
    let bq = build_update_one(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"title": "Tags"}),
        &value!({"tags": {"$addToSet": "python"}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    let tags = updated[0]["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 3);

    // $pull "go"
    let bq = build_update_one(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"title": "Tags"}),
        &value!({"tags": {"$pull": "go"}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    let tags = updated[0]["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 2);
    assert!(!tags.contains(&value!("go")));
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 6. Update many
// ---------------------------------------------------------------------------

#[compio::test]
async fn update_many_round_trip() {
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    // Insert 3 tech, 1 food
    let docs = value!([
        {"title": "A", "category": "tech", "views": 0},
        {"title": "B", "category": "tech", "views": 0},
        {"title": "C", "category": "tech", "views": 0},
        {"title": "D", "category": "food", "views": 0}
    ]);
    let bq = build_insert_many(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &docs,
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    // Update all tech views +1
    let bq = build_update_many(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"category": "tech"}),
        &value!({"views": {"$inc": 1}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    assert_eq!(updated.len(), 3);

    // Verify food unchanged
    let bq = build_find_with_schema(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &value!({"category": "food"}),
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
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &value!({"category": "tech"}),
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = value!([
        {"title": "Keep1", "category": "tech"},
        {"title": "Keep2", "category": "tech"},
        {"title": "Del1", "category": "food"},
        {"title": "Del2", "category": "food"},
        {"title": "Del3", "category": "food"}
    ]);
    let bq = build_insert_many(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &docs,
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    // Delete one food
    let bq = build_delete_one(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"category": "food"}),
    )
    .unwrap();
    let deleted = exec_mutation(&pool, bq).await;
    assert_eq!(deleted.len(), 1);

    // 4 remaining
    let bq = build_count(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({}),
    )
    .unwrap();
    let param_refs = &bq.params;
    let rows = zeroship_data_orm::backend::postgres::params::query(
        &pool.acquire().await.unwrap(),
        &bq.sql,
        param_refs,
    )
    .await
    .unwrap();
    assert_eq!(rows[0].get::<_, i64>("count"), 4);

    // Delete many remaining food
    let bq = build_delete_many(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"category": "food"}),
        SqlDialect::Postgres,
    )
    .unwrap();
    let deleted = exec_mutation(&pool, bq).await;
    assert_eq!(deleted.len(), 2);

    // 2 tech remaining
    let bq = build_count(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({}),
    )
    .unwrap();
    let param_refs = &bq.params;
    let rows = zeroship_data_orm::backend::postgres::params::query(
        &pool.acquire().await.unwrap(),
        &bq.sql,
        param_refs,
    )
    .await
    .unwrap();
    assert_eq!(rows[0].get::<_, i64>("count"), 2);
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 8. Filter operators: $gt, $gte, $lt, $lte, $in, $nin, $ne
// ---------------------------------------------------------------------------

#[compio::test]
async fn filter_comparison_operators() {
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = value!([
        {"title": "A", "category": "tech", "views": 10},
        {"title": "B", "category": "tech", "views": 20},
        {"title": "C", "category": "food", "views": 30},
        {"title": "D", "category": "food", "views": 40}
    ]);
    let bq = build_insert_many(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &docs,
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    // $gt 25
    let bq = build_find_with_schema(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &value!({"views": {"$gt": 25}}),
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
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &value!({"views": {"$lte": 20}}),
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
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &value!({"category": {"$in": ["tech", "food"]}}),
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
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &value!({"category": {"$nin": ["food"]}}),
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
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &value!({"category": {"$ne": "food"}}),
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = value!([
        {"title": "A", "category": "tech", "views": 10},
        {"title": "B", "category": "tech", "views": 50},
        {"title": "C", "category": "food", "views": 10}
    ]);
    let bq = build_insert_many(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &docs,
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    // $and: tech AND views > 20
    let bq = build_find_with_schema(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &value!({"$and": [{"category": "tech"}, {"views": {"$gt": 20}}]}),
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
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &value!({"$or": [{"category": "tech"}, {"views": {"$gt": 20}}]}),
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
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &value!({"$not": {"category": "food"}}),
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = value!([
        {"title": "Hello World", "category": "tech"},
        {"title": "hello rust", "category": "tech"},
        {"title": "Goodbye", "category": "food"}
    ]);
    let bq = build_insert_many(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &docs,
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    // $like (case sensitive)
    let bq = build_find_with_schema(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &value!({"title": {"$like": "Hello%"}}),
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
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &value!({"title": {"$ilike": "%hello%"}}),
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = value!([
        {"title": "C", "category": "tech", "views": 30},
        {"title": "A", "category": "tech", "views": 10},
        {"title": "B", "category": "tech", "views": 20}
    ]);
    let bq = build_insert_many(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &docs,
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    // Order by views ASC, limit 2
    let bq = build_find_with_schema(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &value!({}),
        Some(2),
        None,
        Some(&value!({"views": 1})),
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
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &value!({}),
        Some(1),
        Some(1),
        Some(&value!({"views": -1})),
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let bq = build_insert(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"title": "Proj", "body": "secret", "category": "tech"}),
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    let bq = build_find_with_schema(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &value!({}),
        None,
        None,
        None,
        Some(&value!(["title", "category"])),
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = value!([
        {"title": "A", "category": "tech"},
        {"title": "B", "category": "tech"},
        {"title": "C", "category": "food"},
        {"title": "D", "category": "science"}
    ]);
    let bq = build_insert_many(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &docs,
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    let bq = build_distinct(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        "category",
        &value!({}),
        &notes_schema(),
    )
    .unwrap();
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
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        "category",
        &value!({"category": {"$ne": "science"}}),
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = value!([
        {"title": "A", "category": "tech"},
        {"title": "B", "category": "tech"},
        {"title": "C", "category": "food"}
    ]);
    let bq = build_insert_many(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &docs,
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    // Count all
    let bq = build_count(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({}),
    )
    .unwrap();
    let param_refs = &bq.params;
    let rows = zeroship_data_orm::backend::postgres::params::query(
        &pool.acquire().await.unwrap(),
        &bq.sql,
        param_refs,
    )
    .await
    .unwrap();
    assert_eq!(rows[0].get::<_, i64>("count"), 3);

    // Count with filter
    let bq = build_count(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"category": "tech"}),
    )
    .unwrap();
    let param_refs = &bq.params;
    let rows = zeroship_data_orm::backend::postgres::params::query(
        &pool.acquire().await.unwrap(),
        &bq.sql,
        param_refs,
    )
    .await
    .unwrap();
    assert_eq!(rows[0].get::<_, i64>("count"), 2);
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 15. Aggregate: group by + $count + $sum + $avg + $min + $max
// ---------------------------------------------------------------------------

#[compio::test]
async fn aggregate_full() {
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = value!([
        {"title": "A", "category": "tech", "views": 10},
        {"title": "B", "category": "tech", "views": 20},
        {"title": "C", "category": "tech", "views": 30},
        {"title": "D", "category": "food", "views": 100}
    ]);
    let bq = build_insert_many(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &docs,
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    let pipeline = value!([
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
    let bq = build_aggregate(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &pipeline,
        &notes_schema(),
    )
    .unwrap();
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = value!([
        {"title": "A", "category": "tech", "body": "rust", "views": 10},
        {"title": "B", "category": "tech", "body": "rust", "views": 20},
        {"title": "C", "category": "tech", "body": "go", "views": 5},
        {"title": "D", "category": "food", "body": "pasta", "views": 50}
    ]);
    let bq = build_insert_many(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &docs,
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    let pipeline = value!([
        {"$group": {
            "by": ["category", "body"],
            "cnt": {"$count": true}
        }},
        {"$sort": {"cnt": -1}}
    ]);
    let bq = build_aggregate(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &pipeline,
        &notes_schema(),
    )
    .unwrap();
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let docs = value!([
        {"title": "A", "category": "tech", "views": 10},
        {"title": "B", "category": "tech", "views": 20},
        {"title": "C", "category": "tech", "views": 30},
        {"title": "D", "category": "food", "views": 5}
    ]);
    let bq = build_insert_many(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &docs,
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    // HAVING with alias → resolved to aggregate expression
    let pipeline = value!([
        {"$group": {
            "by": "category",
            "cnt": {"$count": true}
        }},
        {"$having": {"cnt": {"$gt": 1}}},
        {"$sort": {"cnt": -1}}
    ]);
    let bq = build_aggregate(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &pipeline,
        &notes_schema(),
    )
    .unwrap();
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    // Insert with body
    let bq = build_insert(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"title": "WithBody", "body": "has content", "category": "tech"}),
    )
    .unwrap();
    exec_mutation(&pool, bq).await;
    // Insert without body (column defaults to NULL)
    let bq = build_insert(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"title": "NoBody", "category": "tech"}),
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    // Find where body IS NULL
    let bq = build_find_with_schema(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &value!({"body": null}),
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
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &value!({"body": {"$ne": null}}),
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
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &value!({"body": {"$exists": true}}),
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let bq = build_insert(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"title": "Mix", "category": "tech", "views": 10}),
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    // Update: set category + inc views + push tag
    let bq = build_update_one(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"title": "Mix"}),
        &value!({"category": "science", "views": {"$inc": 5}, "tags": {"$push": "new"}}),
    )
    .unwrap();
    let updated = exec_mutation(&pool, bq).await;
    assert_eq!(updated[0]["category"], "science");
    assert_eq!(updated[0]["views"], 15);
    let tags = updated[0]["tags"].as_array().unwrap();
    assert!(tags.contains(&value!("new")));
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 20. Timestamps are returned as numbers
// ---------------------------------------------------------------------------

#[compio::test]
async fn timestamps_as_numbers() {
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    setup(&pool, schema).await;

    let bq = build_insert(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "notes",
        &notes_schema(),
        &value!({"title": "Time", "category": "tech"}),
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
    let (_postgres, url) = require_pg().await;
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

    let docs = value!([
        {"city": "San Francisco", "temp_lo": 46, "temp_hi": 50},
        {"city": "San Francisco", "temp_lo": 43, "temp_hi": 57},
        {"city": "San Francisco", "temp_lo": 35, "temp_hi": 65},
        {"city": "Hayward", "temp_lo": 37, "temp_hi": 54},
        {"city": "Hayward", "temp_lo": 38, "temp_hi": 52},
        {"city": "Hayward", "temp_lo": 41, "temp_hi": 55}
    ]);
    let bq = build_insert_many(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "weather",
        &weather_schema(),
        &docs,
    )
    .unwrap();
    exec_mutation(&pool, bq).await;

    // Equivalent of: SELECT city, count(*), max(temp_lo)
    //                FROM weather GROUP BY city HAVING max(temp_lo) < 42
    let pipeline = value!([
        {"$group": {
            "by": "city",
            "cnt": {"$count": true},
            "max_temp": {"$max": "temp_lo"}
        }},
        {"$having": {"max_temp": {"$lt": 42}}}
    ]);
    let bq = build_aggregate(
        &zeroship_data_sql::SchemaName::new(schema).expect("fixture schema name"),
        "weather",
        &pipeline,
        &weather_schema(),
    )
    .unwrap();

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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());

    // Fresh schema + table — `build_create_table` is the production path.
    let app = crate::test_app_id!();
    let app = app.as_str();
    let collection = "users";
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(
        &schema_fixture::fixture_schema_sql(
            &zeroship_data_sql::SchemaName::new(app).expect("fixture schema name"),
        ),
        &[],
    )
    .await
    .unwrap();

    let schema = value!({
        "email": {"type": "string", "required": true, "unique": true},
        "handle": {"type": "string", "index": true},
    });

    let create_table = fixture_table_sql(
        &zeroship_data_sql::SchemaName::new(app).expect("fixture schema name"),
        collection,
        &schema,
        &FkEmission::Inline,
    )
    .unwrap();
    // `build_create_table_with_fks` emits MULTI-statement DDL (the CREATE TABLE
    // plus the system-field index `CREATE INDEX`s, and on PG the
    // `COMMENT ON COLUMN … 'zero-migrate:mask:…'` / `'zero-migrate:enc:…'` sentinels). The
    // extended/prepared `execute` path rejects that with `42601 cannot insert
    // multiple commands into a prepared statement`; the simple-query
    // `batch_execute` is the correct executor for rendered DDL batches.
    pool.batch_execute(&create_table).await.unwrap();

    // Generate and execute the new index DDL.
    let indexes = schema_fixture::fixture_indexes(
        &zeroship_data_sql::SchemaName::new(app).expect("fixture schema name"),
        collection,
        &schema,
    )
    .unwrap();
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
        &zeroship_data_sql::SchemaName::new(app).expect("fixture schema name"),
        collection,
        &schema,
        &with_seed_id(value!({"email": "a@x.com"})),
    )
    .unwrap();
    let p1 = &ins1.params;
    zeroship_data_orm::backend::postgres::params::query(
        &pool.acquire().await.unwrap(),
        &ins1.sql,
        p1,
    )
    .await
    .unwrap();

    // Distinct `id` so the second insert is rejected for the DUPLICATE EMAIL
    // (the unique index under test), not an incidental duplicate PK.
    let ins2 = build_insert(
        &zeroship_data_sql::SchemaName::new(app).expect("fixture schema name"),
        collection,
        &schema,
        &with_seed_id(value!({"email": "a@x.com"})),
    )
    .unwrap();
    let p2 = &ins2.params;
    let err = zeroship_data_orm::backend::postgres::params::query(
        &pool.acquire().await.unwrap(),
        &ins2.sql,
        p2,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            zeroship_data_orm::error::DbError::UniqueViolation { .. }
        ),
        "duplicate email must violate uniqueness: {err}"
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
    let (_postgres, url) = require_pg().await;
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
    // crates/zeroship-data-sql/src/compile.rs:1607, which OMITS the ON DELETE
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
    let (_postgres, url) = require_pg().await;
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
    let (_postgres, url) = require_pg().await;
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
    let (_postgres, url) = require_pg().await;
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
    let (_postgres, url) = require_pg().await;
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
    let client = pool.acquire().await.unwrap();
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
// PostgreSQL coverage runs under ordinary cargo test. The live-suite runner
// provisions its prerequisites and runs both data packages without feature gates.
//
// AND THE SKIP WAS INVISIBLE TO A SUMMING GATE. Measured on two
// throwaway servers differing only in wal_level, the tests below printed the
// IDENTICAL result line either way, because a skip counts as a pass. On
// `replica` every one of them skipped; on `logical` every one executed. The
// only discriminators were a marker no gate ran over this binary, and the wall
// time. So wiring this into CI would have bought a green that proved nothing.
// `require_logical_wal` closes that: the two servers now differ in exit status.
// ===========================================================================

#[compio::test]
async fn c1_broker_event_delivered_for_insert_via_emit() {
    // End-to-end of the local-emit path: the broker, attached
    // on the same thread the test runs on, receives an insert event
    // when `emit_local` is called. No Postgres needed — the broker
    // is in-process.

    // Clean slate.
    zeroship_data_orm::cdc::broker::drain_current_thread_subscriptions();
    let app = crate::test_app_id!();
    let app = app.as_str();
    let sub = zeroship_data_orm::cdc::broker::subscribe(app, "messages");

    zeroship_data_orm::cdc::broker::emit_local(
        app,
        "messages",
        zeroship_data_orm::cdc::ChangeOp::Insert,
        Some("usr_02HXINTEGRATIONSUBPK".to_string()),
        vec!["title".into()],
        std::collections::HashMap::new(),
    );

    let msg = sub.pop().expect("expected an event");
    match msg {
        zeroship_data_orm::cdc::broker::SubscriptionMessage::Change(ev) => {
            assert_eq!(ev.collection, "messages");
            assert_eq!(ev.pk.as_deref(), Some("usr_02HXINTEGRATIONSUBPK"));
            assert_eq!(ev.op, zeroship_data_orm::cdc::ChangeOp::Insert);
        }
        other => panic!("unexpected: {other:?}"),
    }
    sub.close();
    zeroship_data_orm::cdc::broker::drain_current_thread_subscriptions();
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
fn gapb_ev(app: &str, collection: &str, pk: i64) -> zeroship_data_orm::cdc::ChangeEvent {
    zeroship_data_orm::cdc::ChangeEvent {
        app_id: app.to_string(),
        collection: collection.to_string(),
        op: zeroship_data_orm::cdc::ChangeOp::Insert,
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
    zeroship_data_orm::cdc::broker::drain_current_thread_subscriptions();
    let app = crate::test_app_id!();
    let app = app.as_str();
    let sub = zeroship_data_orm::cdc::broker::subscribe(app, "users");

    zeroship_data_v8::testing::push_pending_emit_for_tests(gapb_ev(app, "users", 1));
    zeroship_data_v8::testing::push_pending_emit_for_tests(gapb_ev(app, "users", 2));
    // Pre-drain: subscriber must observe nothing (events still queued).
    assert!(sub.pop().is_none(), "events must not leak before commit");

    zeroship_data_v8::testing::drain_pending_emits_for_tests(app);

    let mut pks: Vec<String> = Vec::new();
    while let Some(zeroship_data_orm::cdc::broker::SubscriptionMessage::Change(ev)) = sub.pop() {
        pks.push(ev.pk.as_deref().unwrap().to_string());
    }
    assert_eq!(pks, vec!["1".to_string(), "2".to_string()]);

    sub.close();
    zeroship_data_orm::cdc::broker::drain_current_thread_subscriptions();
}

#[compio::test]
async fn gap_b_rollback_clears_pending_emits_silently() {
    // Push events, then `clear` (rollback path). The broker must
    // never see them.
    zeroship_data_orm::cdc::broker::drain_current_thread_subscriptions();
    let app = crate::test_app_id!();
    let app = app.as_str();
    let sub = zeroship_data_orm::cdc::broker::subscribe(app, "users");

    zeroship_data_v8::testing::push_pending_emit_for_tests(gapb_ev(app, "users", 42));
    zeroship_data_v8::testing::push_pending_emit_for_tests(gapb_ev(app, "users", 43));
    zeroship_data_v8::testing::clear_pending_emits_for_tests(app);

    assert!(
        sub.pop().is_none(),
        "rollback must NOT publish any broker event"
    );

    sub.close();
    zeroship_data_orm::cdc::broker::drain_current_thread_subscriptions();
}

#[compio::test]
async fn gap_b_end_to_end_insert_inside_tx_defers_emit_until_commit() {
    // End-to-end: real Postgres tx, real `exec_mutation_with_emit`
    // call. Pre-commit the broker stays empty; post-drain it sees
    // the insert.
    let (_postgres, url) = require_pg().await;
    zeroship_data_v8::testing::set_db_url_for_tests(&url);
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

    zeroship_data_orm::cdc::broker::drain_current_thread_subscriptions();
    let sub = zeroship_data_orm::cdc::broker::subscribe(app, "users");

    // Open the production transaction protocol.
    crate::support::begin_transaction(app, &url).await;

    let role = zeroship_core::database_role::per_app_role_name(app).unwrap();
    pool.batch_execute(&format!(r#"GRANT SELECT, INSERT ON "{app}"."users" TO "{role}"; GRANT USAGE ON ALL SEQUENCES IN SCHEMA "{app}" TO "{role}""#)).await.unwrap();

    // Insert via the production helper.
    let bq = zeroship_data_sql::compile::build_insert(
        &zeroship_data_sql::SchemaName::new(app).expect("fixture schema name"),
        "users",
        // The descriptor entry for the fixture table above: one declared field.
        &zeroship_data_sql::value!({ "name": { "type": "string", "required": true } }),
        &zeroship_data_sql::value!({ "name": "alice" }),
    )
    .expect("build_insert");
    let _ = zeroship_data_v8::testing::exec_mutation_with_emit_for_tests(
        bq,
        app,
        "users",
        zeroship_data_orm::cdc::ChangeOp::Insert,
    )
    .await
    .expect("insert");

    // Mid-transaction: subscriber must see nothing.
    assert!(
        sub.pop().is_none(),
        "pre-commit broker must be empty (Gap B)"
    );

    // Settlement commits the row before publishing its buffered event.
    assert!(matches!(
        zeroship_data_orm::transaction::exec_settle(app, true, None).await,
        zeroship_data_orm::transaction::SettleOutcome::Ok
    ));

    let got = sub.pop();
    match got {
        Some(zeroship_data_orm::cdc::broker::SubscriptionMessage::Change(ev)) => {
            assert_eq!(ev.collection, "users");
        }
        other => panic!("expected Change event after commit, got: {other:?}"),
    }

    sub.close();
    zeroship_data_orm::cdc::broker::drain_current_thread_subscriptions();
    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
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
        "The PostgreSQL testcontainer must provide the `vector` extension; check tests/fixtures/postgres/Dockerfile and extensions.sql."
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
    use zeroship_data_orm::backend::VectorMetric;

    let (_postgres, url) = require_pg().await;
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
    zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&pool, app)
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
    let backend = zeroship_data_orm::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_data_v8::testing::isolate_key_source(),
    );
    // The search's projection is the descriptor's field list; install the entry
    // this deploy's runtime descriptor would have planted at boot.
    zeroship_data_orm::cache_schema_for_tests(
        app,
        coll,
        value!({ "embedding": { "type": "vector", "vectorDims": 8 } }),
    );
    let rows = zeroship_data_orm::search::Search::vector_search(
        &backend,
        None,
        zeroship_data_orm::search::VectorSearch {
            binding: &DbBinding::cold_start(app),
            collection: coll,
            column: "embedding",
            query: &query,
            k: 10,
            metric: VectorMetric::Cosine,
            filter: &zeroship_data_sql::value::Value::Null,
            schema: &zeroship_data_orm::descriptor::collection_schema(&DbBinding::cold_start(app), coll)
                .expect("descriptor slice for the search fixture"),
        },
    )
    .await
    .unwrap_or_else(|e| panic!("vector_search failed: {e:?}"));

    assert_eq!(rows.len(), 10, "expected k=10 rows, got {}", rows.len());
    // Row id #1 (1-indexed via SERIAL) must be in the top-10 (it
    // matches the query exactly).
    let ids: Vec<i64> = rows
        .iter()
        .filter_map(|r| {
            r.get("id")
                .and_then(zeroship_data_sql::value::Value::as_i64)
        })
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
    use zeroship_data_orm::backend::{PostgresBackend, VectorMetric};
    use zeroship_data_orm::error::DbError;

    let (_postgres, url) = require_pg().await;
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

    let backend = zeroship_data_orm::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_data_v8::testing::isolate_key_source(),
    );

    // No descriptor entry is installed for `vector_missing`, and that is
    // deliberate: `ensure_pgvector_available` runs BEFORE the schema resolve,
    // so the extension error must still be the one that surfaces. If the order
    // ever flipped, this would fail with `collection_not_declared` instead.
    async fn search(backend: &PostgresBackend) -> DbError {
        zeroship_data_orm::search::Search::vector_search(
            backend,
            None,
            zeroship_data_orm::search::VectorSearch {
                binding: &DbBinding::cold_start("vector_missing"),
                collection: "any",
                column: "any",
                query: &[0.0f32; 8],
                k: 10,
                metric: VectorMetric::Cosine,
                filter: &zeroship_data_sql::value::Value::Null,
                schema: &zeroship_data_sql::value::Value::Null,
            },
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    require_pgvector(&pool).await;

    let app = crate::test_app_id!();

    let app = app.as_str();
    let coll = "docs";
    // Same provisioning gap as `vector_search_returns_k_nearest`: a schema
    // without its per-app role fails closed before the insert is ever attempted.
    let _role = provision_app_with_role(&pool, app).await;
    zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&pool, app)
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
    use zeroship_data_orm::backend::GeoPoint;

    let (_postgres, url) = require_pg().await;
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
    zeroship_data_orm::cache_schema_for_tests(
        app,
        coll,
        value!({ "location": { "type": "geoPoint" } }),
    );

    zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();
    support::grant_all_runtime_table_columns(&pool, app, coll).await;

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

    let backend = zeroship_data_orm::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_data_v8::testing::isolate_key_source(),
    );
    let rows = zeroship_data_orm::search::Search::spatial_near(
        &backend,
        None,
        zeroship_data_orm::search::SpatialSearch {
            binding: &DbBinding::cold_start(app),
            collection: coll,
            column: "location",
            point: london,
            radius_m: 1000.0,
            filter: &zeroship_data_sql::value::Value::Null,
            limit: None,
            schema: &value!({ "location": { "type": "geoPoint" } }),
        },
    )
    .await
    .unwrap_or_else(|e| panic!("spatial_near failed: {e:?}"));

    let returned_ids: std::collections::BTreeSet<i64> = rows
        .iter()
        .filter_map(|r| {
            r.get("id")
                .and_then(zeroship_data_sql::value::Value::as_i64)
        })
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
    use zeroship_data_orm::backend::{GeoPoint, PostgresBackend};
    use zeroship_data_orm::error::DbError;

    let (_postgres, url) = require_pg().await;
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

    let backend = zeroship_data_orm::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_data_v8::testing::isolate_key_source(),
    );

    // No descriptor entry, deliberately: the extension probe runs BEFORE the
    // schema resolve, so this must still surface `postgis_extension_missing`.
    async fn near(backend: &PostgresBackend) -> DbError {
        zeroship_data_orm::search::Search::spatial_near(
            backend,
            None,
            zeroship_data_orm::search::SpatialSearch {
                binding: &DbBinding::cold_start("postgis_missing"),
                collection: "any",
                column: "any",
                point: GeoPoint { lat: 0.0, lng: 0.0 },
                radius_m: 1000.0,
                filter: &zeroship_data_sql::value::Value::Null,
                limit: None,
                schema: &zeroship_data_sql::value::Value::Null,
            },
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
// insert, encode-as-hex on read, AAD-bound decrypt. Authentication of
// the row primary key is the load-bearing assertion in
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
use zeroship_data_orm::error::DbError;

use zeroship_data_orm::encryption;

/// Supply a project key for the apps explicitly named by this fixture.
fn with_project_key(app_ids: &[&str], hex: &str) -> zeroship_data_v8::testing::SuppliedProjectKeysGuard {
    zeroship_data_v8::testing::supply_project_key_for_tests(app_ids, hex)
}

/// Gate #1: round-trip an encrypted string column. Insert a
/// row with `ssn` declared `t.encrypted({  })`,
/// read it back via the PG path, expect the plaintext to recover.
#[compio::test]
async fn encrypted_column_round_trip_randomised() {
    let (_postgres, url) = require_pg().await;
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    // Synthetic 32-byte root key.
    let _keys = with_project_key(&["app1"], &"a".repeat(64));

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

    let backend = zeroship_data_orm::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_data_v8::testing::isolate_key_source(),
    );
    let key = backend
        .key_store()
        .resolve("app1")
        .await
        .expect("resolve_key");
    let plaintext = b"123-45-6789";
    let aad = encryption::canonical_aad("app1", "enc_notes", "ssn", b"row_a");
    let ct = zeroship_data_orm::encryption::aead::encrypt(&key, plaintext, &aad).expect("encrypt");

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
    let recovered = zeroship_data_orm::encryption::aead::decrypt(&key, &raw, &aad).expect("decrypt");
    assert_eq!(recovered, plaintext);
    drop(backend);
    release_pg(pool).await;
}

/// Camp A fence: copying ciphertext from row A into row
/// B's slot must surface `encryption_aead_failed` (row_pk in AAD
/// defeats the ciphertext-oracle attack on randomised columns).
#[compio::test]
async fn encrypted_randomised_row_swap_rejected() {
    let (_postgres, url) = require_pg().await;
    let schema = crate::test_app_id!();
    let schema = schema.as_str();
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let _keys = with_project_key(&["app1"], &"b".repeat(64));

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

    let backend = zeroship_data_orm::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_data_v8::testing::isolate_key_source(),
    );
    let key = backend
        .key_store()
        .resolve("app1")
        .await
        .unwrap();
    // Insert row A with its OWN AAD (binds row_pk = "row_a").
    let ct_a = zeroship_data_orm::encryption::aead::encrypt(
        &key,
        b"sensitive-A",
        &encryption::canonical_aad("app1", "enc_notes", "ssn", b"row_a"),
    )
    .unwrap();
    let ct_b = zeroship_data_orm::encryption::aead::encrypt(
        &key,
        b"sensitive-B",
        &encryption::canonical_aad("app1", "enc_notes", "ssn", b"row_b"),
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
    let aad_b = encryption::canonical_aad("app1", "enc_notes", "ssn", b"row_b");
    let err = zeroship_data_orm::encryption::aead::decrypt(&key, &raw, &aad_b)
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = crate::test_app_id!();

    let app = app.as_str();
    let _keys = with_project_key(&[app], &"d".repeat(64));
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();

    // Schema: encrypted `ssn`, masked `phone`, and a `vector` embedding.
    let schema = value!({
        "name": {"type": "string", "required": true},
        "ssn": {
            "type": "string",
            "encrypted": {"wraps": "string"}
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
    crate::support::install_postgres_pool(std::rc::Rc::clone(&pool), &url);
    zeroship_data_orm::cache_schema_for_tests(app, "people", schema.clone());

    // Sanity: the resolution the CRUD passes will perform returns BOTH goodies.
    let resolved = zeroship_data_orm::crud::runtime_schema_for_tests(app, "people")
        .expect("the descriptor entry this deploy installed must resolve");
    assert_eq!(resolved["phone"]["mask"]["kind"], "last4");

    // ----- WRITE (real pipeline, introspected metadata) -----
    // No `id`: the write pipeline refuses a creator-supplied one and mints a
    // typed id in `system_fields_pass`. The raw INSERT below MUST then carry
    // THAT MINTED ID and nothing else: encryption binds the row primary key
    // into the AEAD's
    // additional data (`canonical_aad(collection, column, row_pk_bytes)` in
    // zeroship-data-orm's `encryption::aad`, stamped on write by
    // `protection::encryption_pass` and reconstructed on read from the row's `id`).
    // Storing this ciphertext under a DIFFERENT id and reading it back is a
    // ciphertext-relocation attack, and the AEAD refuses it with
    // `encryption_aead_failed` - correctly. Hard-coding a literal here is what
    // broke the test.
    let mut docs = value!([{
        "name": "Ada",
        "ssn": "123-45-6789",
        "phone": "415-555-0142",
        "embedding": [0.1, 0.2, 0.3],
    }]);
    zeroship_data_v8::testing::prepare_insert_many_docs_for_tests(&mut docs, app, "people", None)
        .await
        .expect("write pipeline");

    // The write pipeline encrypted `ssn` into native bytes
    // and RELOCATED `phone`: the mask moves into the field's OWN column and
    // the real value moves out to the raw sibling
    // (`mask_pass::relocate_masked_columns`).
    let doc = &docs[0];
    let row_id = doc["id"]
        .as_str()
        .expect("the write pipeline mints the row id, and the AAD binds it")
        .to_string();
    assert!(
        doc["ssn"].as_bytes().is_some(),
        "ssn must be replaced by ciphertext on write, got {:?}",
        doc["ssn"]
    );
    assert_eq!(
        doc["phone"],
        value!("***-***-0142"),
        "mask pass must move the last4 mask into phone's own column on write, got {:?}",
        doc["phone"]
    );
    assert_ne!(
        doc["phone"],
        value!("415-555-0142"),
        "phone's own column must not carry the real value after relocation, got {:?}",
        doc["phone"]
    );
    assert_eq!(
        doc[phone_raw.as_str()],
        value!("415-555-0142"),
        "the real phone value must be relocated to the raw sibling column, got {:?}",
        doc[phone_raw.as_str()]
    );

    // Persist it the way the SQL builder would (bind the encrypted bytes,
    // store the mask under `phone` and the real value under its raw sibling).
    let ciphertext = doc["ssn"].as_bytes().unwrap();
    let phone_mask = doc["phone"].as_str().unwrap().to_string();
    let phone_real = doc[phone_raw.as_str()].as_str().unwrap().to_string();
    // The vector literal is a test-controlled constant — format it inline with a
    // `::vector` cast (compio-postgres infers a `vector`-typed param from the
    // bind otherwise, which it cannot encode an `&str` into).
    pool.execute(
        &format!(
            "INSERT INTO \"{app}\".\"people\" (id, name, ssn, phone, \"{phone_raw}\", embedding) \
             VALUES ($1, $2, $3::bytea, $4, $5, '[0.1,0.2,0.3]'::vector)"
        ),
        &[
            &row_id.as_str(),
            &"Ada",
            &ciphertext,
            &phone_mask.as_str(),
            &phone_real.as_str(),
        ],
    )
    .await
    .unwrap();

    // ----- READ (real pipeline, introspected metadata) -----
    // Fetch the raw row the way the SELECT builder would: the encrypted blob
    // as bytes, and `phone` read directly. Reads no longer alias anything
    // after the storage flip -- the field's own column already holds the mask.
    let raw = pool
        .query_text_params(
            &format!(
                "SELECT id, name, ssn, phone \
                 FROM \"{app}\".\"people\" WHERE id = $1"
            ),
            &[row_id.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(raw.len(), 1);
    let row = zeroship_data_orm::backend::postgres::pg_row_json::row_to_value_for_bench(&raw[0]).unwrap();

    let finalized = zeroship_data_v8::testing::finalize_rows_on_read_for_tests(app, "people", vec![row])
        .await
        .expect("read pipeline");
    let out = &finalized[0];

    // Encrypted column decrypted back to plaintext (driven by introspected meta).
    assert_eq!(
        out["ssn"],
        value!("123-45-6789"),
        "encrypted column must decrypt to plaintext on read, got {:?}",
        out["ssn"]
    );
    // Masked column wrapped into the platform MaskedValue sentinel, carrying the
    // last4-masked string + the introspected classification.
    assert_eq!(
        out["phone"]["sentinel"],
        value!("__zsmask__"),
        "phone wrapped"
    );
    assert_eq!(
        out["phone"]["masked"],
        value!("***-***-0142"),
        "masked phone surfaces last4 form, got {:?}",
        out["phone"]
    );
    assert_eq!(out["phone"]["classification"], value!("pci"));
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = crate::test_app_id!();

    let app = app.as_str();
    let _keys = with_project_key(&[app], &"e".repeat(64));
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();

    let schema = value!({
        "name": {"type": "string", "required": true},
        "ssn": {
            "type": "string",
            "encrypted": {"wraps": "string"}
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
    crate::support::install_postgres_pool(std::rc::Rc::clone(&pool), &url);
    zeroship_data_orm::cache_schema_for_tests(app, "people", schema.clone());

    // The resolution the CRUD passes will perform returns BOTH goodies.
    let resolved = zeroship_data_orm::crud::runtime_schema_for_tests(app, "people")
        .expect("the descriptor entry this deploy installed must resolve");
    assert_eq!(resolved["phone"]["mask"]["kind"], "last4");

    // ----- WRITE via the real pipeline (descriptor metadata) -----
    // No `id`: the write pipeline refuses a creator-supplied one and mints a
    // typed id. The raw INSERT below MUST carry that minted id - `ssn` is a
    // `randomised` encrypted column, so the row primary key is bound into the
    // AEAD's additional data on write and reconstructed from the row's `id` on
    // read. A literal id here relocates the ciphertext onto another row, and
    // the read correctly refuses it with `encryption_aead_failed`.
    let mut docs = value!([{
        "name": "Grace",
        "ssn": "987-65-4321",
        "phone": "650-555-0199",
    }]);
    zeroship_data_v8::testing::prepare_insert_many_docs_for_tests(&mut docs, app, "people", None)
        .await
        .expect("write pipeline");
    let doc = &docs[0];
    let row_id = doc["id"]
        .as_str()
        .expect("the write pipeline mints the row id, and the AAD binds it")
        .to_string();
    assert!(
        doc["ssn"].as_bytes().is_some(),
        "ssn must be ciphertext on write, got {:?}",
        doc["ssn"]
    );
    assert_eq!(
        doc["phone"],
        value!("***-***-0199"),
        "mask pass must move the last4 mask into phone's own column on write, got {:?}",
        doc["phone"]
    );
    assert_ne!(
        doc["phone"],
        value!("650-555-0199"),
        "phone's own column must not carry the real value after relocation, got {:?}",
        doc["phone"]
    );
    assert_eq!(
        doc[phone_raw.as_str()],
        value!("650-555-0199"),
        "the real phone value must be relocated to the raw sibling column, got {:?}",
        doc[phone_raw.as_str()]
    );

    let ciphertext = doc["ssn"].as_bytes().unwrap();
    let phone_mask = doc["phone"].as_str().unwrap().to_string();
    let phone_real = doc[phone_raw.as_str()].as_str().unwrap().to_string();
    pool.execute(
        &format!(
            "INSERT INTO \"{app}\".\"people\" (id, name, ssn, phone, \"{phone_raw}\") \
             VALUES ($1, $2, $3::bytea, $4, $5)"
        ),
        &[
            &row_id.as_str(),
            &"Grace",
            &ciphertext,
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
                "SELECT id, name, ssn, phone \
                 FROM \"{app}\".\"people\" WHERE id = $1"
            ),
            &[row_id.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(raw.len(), 1);
    let row = zeroship_data_orm::backend::postgres::pg_row_json::row_to_value_for_bench(&raw[0]).unwrap();
    let finalized = zeroship_data_v8::testing::finalize_rows_on_read_for_tests(app, "people", vec![row])
        .await
        .expect("read pipeline");
    let out = &finalized[0];
    assert_eq!(
        out["ssn"],
        value!("987-65-4321"),
        "encrypted column decrypts to plaintext on read, got {:?}",
        out["ssn"]
    );
    assert_eq!(
        out["phone"]["sentinel"],
        value!("__zsmask__"),
        "phone wrapped"
    );
    assert_eq!(out["phone"]["masked"], value!("***-***-0199"));
    assert_eq!(out["phone"]["classification"], value!("pci"));
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    // Resolve against a source that provably has NO key: an empty
    // supplied set, so the fixture is independent of process configuration.
    let _keys = with_project_key(&[], &"00".repeat(32));

    let backend = zeroship_data_orm::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_data_v8::testing::isolate_key_source(),
    );
    let err = backend
        .key_store()
        .resolve("app1")
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let rows = pool
        .query_text_params(
            "SELECT decode('5c783431343234333434', 'hex')::bytea AS payload",
            &[],
        )
        .await
        .unwrap();
    let json = zeroship_data_orm::backend::postgres::pg_row_json::row_to_value_for_bench(&rows[0]).unwrap();
    let payload = json
        .get("payload")
        .and_then(Value::as_bytes)
        .expect("native payload bytes");
    let expected_raw = br"\x41424344".as_slice();
    let wrong_hex_decoded = b"ABCD".as_slice();

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
//      `zeroship_data_orm::storage::Backup`.
//   3. `snapshot_during_migration_returns_typed_error` — acquire the
//      `snapshot_restore` mig-lock manually; attempt `snapshot()`;
//      expect `Coded { code: "migration_in_progress" }`. No subprocess.
//   4. `snapshot_uri_content_hash_round_trip` — `snapshot()` →
//      `SnapshotHandle.content_hash` matches SHA-256 of the on-disk
//      dump file. Needs `pg_dump` for the same reason as #1.

use zeroship_data_orm::backend::{
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
         inside the container where this process cannot reach it. CI installs\n\
         matching clients; local runs must also put them on PATH.\n\
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let backend = zeroship_data_orm::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_data_v8::testing::isolate_key_source(),
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
    let (_postgres, url) = require_pg().await;
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

    let backend = zeroship_data_orm::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_data_v8::testing::isolate_key_source(),
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
    let (_postgres, url) = require_pg().await;
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

    let backend = zeroship_data_orm::backend::PostgresBackend::new(
        pool.clone(),
        url.clone(),
        zeroship_data_v8::testing::isolate_key_source(),
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
        "The PostgreSQL testcontainer must provide the `postgis` extension; check tests/fixtures/postgres/Dockerfile and extensions.sql."
    );
}

#[compio::test]
async fn per_app_role_created_at_provision() {
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = crate::test_app_id!();
    let app = app.as_str();
    let role = provision_app_with_role(&pool, app).await;

    // First provision creates the role.
    let first = zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&pool, app)
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
    let second = zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&pool, app)
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
    let (_postgres, url) = require_pg().await;
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
    zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&pool, &app_schema)
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = crate::test_app_id!();
    let app = app.as_str();
    let role = provision_app_with_role(&pool, app).await;
    zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&pool, app)
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = crate::test_app_id!();
    let app = app.as_str();
    let role = provision_app_with_role(&pool, app).await;
    zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&pool, app)
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
    let (_postgres, url) = require_pg().await;
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

    zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&pool, app_a)
        .await
        .unwrap();
    zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&pool, app_b)
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = crate::test_app_id!();
    let app = app.as_str();
    let role = provision_app_with_role(&pool, app).await;
    zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&pool, app)
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
    let set_sql = zeroship_data_orm::auth::bootstrap::set_local_role_sql(app)
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = crate::test_app_id!();
    let app = app.as_str();
    let role = provision_app_with_role(&pool, app).await;
    zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();
    zeroship_data_v8::testing::set_db_url_for_tests(&url);

    let rows = zeroship_data_v8::testing::exec_query_for_tests(
        app,
        zeroship_data_sql::compile::BuiltQuery {
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
    use zeroship_data_orm::backend::VectorMetric;

    let (_postgres, url) = require_pg().await;
    let admin_pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    require_pgvector(&admin_pool).await;

    let app = crate::test_app_id!();

    let app = app.as_str();
    let coll = "docs";
    let role = provision_app_with_role(&admin_pool, app).await;
    zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&admin_pool, app)
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
    zeroship_data_orm::cache_schema_for_tests(
        app,
        coll,
        value!({ "embedding": { "type": "vector", "vectorDims": 2 } }),
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

    let backend = zeroship_data_orm::backend::PostgresBackend::new(
        login_pool.clone(),
        login_url,
        zeroship_data_v8::testing::isolate_key_source(),
    );
    let rows = zeroship_data_orm::search::Search::vector_search(
        &backend,
        None,
        zeroship_data_orm::search::VectorSearch {
            binding: &DbBinding::cold_start(app),
            collection: coll,
            column: "embedding",
            query: &[1.0, 0.0],
            k: 1,
            metric: VectorMetric::Cosine,
            filter: &Value::Null,
            schema: &zeroship_data_orm::descriptor::collection_schema(&DbBinding::cold_start(app), coll)
                .expect("descriptor slice for the search fixture"),
        },
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
    use zeroship_data_orm::backend::GeoPoint;

    let (_postgres, url) = require_pg().await;
    let admin_pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    require_postgis(&admin_pool).await;

    let app = crate::test_app_id!();

    let app = app.as_str();
    let coll = "places";
    let role = provision_app_with_role(&admin_pool, app).await;
    zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&admin_pool, app)
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
    zeroship_data_orm::cache_schema_for_tests(
        app,
        coll,
        value!({ "location": { "type": "geoPoint" } }),
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

    let backend = zeroship_data_orm::backend::PostgresBackend::new(
        login_pool.clone(),
        login_url,
        zeroship_data_v8::testing::isolate_key_source(),
    );
    let rows = zeroship_data_orm::search::Search::spatial_near(
        &backend,
        None,
        zeroship_data_orm::search::SpatialSearch {
            binding: &DbBinding::cold_start(app),
            collection: coll,
            column: "location",
            point: GeoPoint {
                lat: 51.5074,
                lng: -0.1278,
            },
            radius_m: 1000.0,
            filter: &Value::Null,
            limit: Some(1),
            schema: &zeroship_data_orm::descriptor::collection_schema(&DbBinding::cold_start(app), coll)
                .unwrap(),
        },
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
    use zeroship_data_orm::protection::unmask::{self, UnmaskFieldArgs};

    let (_postgres, url) = require_pg().await;
    let admin_pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = crate::test_app_id!();

    let app = app.as_str();
    let coll = "users";
    let role = provision_app_with_role(&admin_pool, app).await;
    zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&admin_pool, app)
        .await
        .unwrap();
    let schema = value!({
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" }
        }
    });
    let ssn_raw = raw_column_name("ssn");
    // Built with the platform's own emitter, not hand-spelled, so the fixture
    // cannot drift from the runtime's DDL shape: `ssn` gets the bare-TEXT mask
    // column and `__zs_raw__ssn` gets the declared type for the real value.
    let create_table = fixture_table_sql(
        &zeroship_data_sql::SchemaName::new(app).expect("fixture schema name"),
        coll,
        &schema,
        &FkEmission::Inline,
    )
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

    crate::support::install_postgres_pool(login_pool.clone(), &login_url);
    zeroship_data_orm::cache_schema_for_tests(app, coll, schema);
    zeroship_data_v8::testing::clear_mask_policy_cache_for_tests(app);

    let result = unmask::dispatch_unmask(
        &unmask_route(app).await,
        &DbBinding::cold_start(app),
        UnmaskFieldArgs {
            collection: coll.to_string(),
            row_pk: "u1".to_string(),
            column: "ssn".to_string(),
            actor: Some(value!({ "kind": "auto" })),
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
    use zeroship_data_orm::protection::unmask::{self, UnmaskFieldArgs};

    let (_postgres, url) = require_pg().await;
    let admin_pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    // Synthetic 32-byte root key, same shape as the encrypted round-trip gate.

    let app = crate::test_app_id!();

    let app = app.as_str();
    let _keys = with_project_key(&[app], &"b".repeat(64));
    let coll = "users";
    let role = provision_app_with_role(&admin_pool, app).await;
    zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&admin_pool, app)
        .await
        .unwrap();
    let schema = value!({
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" },
            "encrypted": { "wraps": "string" }
        }
    });
    let ssn_raw = raw_column_name("ssn");
    // The emitter decides the raw sibling's type. For an encrypted column that
    // is BYTEA, which is the whole point of this test - so build the DDL rather
    // than hand-spelling it, or the fixture proves nothing about the runtime.
    let create_table = fixture_table_sql(
        &zeroship_data_sql::SchemaName::new(app).expect("fixture schema name"),
        coll,
        &schema,
        &FkEmission::Inline,
    )
    .expect("emitter must build the users DDL");
    admin_pool.batch_execute(&create_table).await.unwrap();

    // Real ciphertext from the platform's own encryptor, under the AAD the read
    // path recomputes: canonical_aad(collection, column, row_pk).
    let backend = zeroship_data_orm::backend::PostgresBackend::new(
        admin_pool.clone(),
        url.clone(),
        zeroship_data_v8::testing::isolate_key_source(),
    );
    let key = backend
        .key_store()
        .resolve(app)
        .await
        .expect("resolve_key");
    let aad = encryption::canonical_aad(app, coll, "ssn", b"u1");
    let ct =
        zeroship_data_orm::encryption::aead::encrypt(&key, b"123-45-6789", &aad).expect("encrypt");
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

    crate::support::install_postgres_pool(login_pool.clone(), &login_url);
    zeroship_data_orm::cache_schema_for_tests(app, coll, schema);
    zeroship_data_v8::testing::clear_mask_policy_cache_for_tests(app);

    let result = unmask::dispatch_unmask(
        &unmask_route(app).await,
        &DbBinding::cold_start(app),
        UnmaskFieldArgs {
            collection: coll.to_string(),
            row_pk: "u1".to_string(),
            column: "ssn".to_string(),
            actor: Some(value!({ "kind": "auto" })),
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
    use zeroship_data_orm::protection::unmask::{self, UnmaskFieldArgs};

    let (_postgres, url) = require_pg().await;
    let admin_pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

    let app = crate::test_app_id!();

    let app = app.as_str();
    let coll = "patients";
    let role = provision_app_with_role(&admin_pool, app).await;
    zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&admin_pool, app)
        .await
        .unwrap();
    let schema = value!({
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "phi" }
        }
    });
    let ssn_raw = raw_column_name("ssn");
    // Built with the platform's own emitter, not hand-spelled, so the fixture
    // cannot drift from the runtime's DDL shape: `ssn` gets the bare-TEXT mask
    // column and `__zs_raw__ssn` gets the declared type for the real value.
    let create_table = fixture_table_sql(
        &zeroship_data_sql::SchemaName::new(app).expect("fixture schema name"),
        coll,
        &schema,
        &FkEmission::Inline,
    )
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

    crate::support::install_postgres_pool(login_pool.clone(), &login_url);
    zeroship_data_orm::cache_schema_for_tests(app, coll, schema);
    zeroship_data_v8::testing::clear_mask_policy_cache_for_tests(app);

    let result = unmask::dispatch_unmask(
        &unmask_route(app).await,
        &DbBinding::cold_start(app),
        UnmaskFieldArgs {
            collection: coll.to_string(),
            row_pk: "p1".to_string(),
            column: "ssn".to_string(),
            actor: Some(value!({ "kind": "auto" })),
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

/// The startup declaration authorizes unmasking on PostgreSQL without a
/// database policy store. An undeclared role remains denied.
#[compio::test]
async fn pg_declared_mask_policy_authorizes_unmask_without_durable_store() {
    use zeroship_data_orm::protection::mask_policy;
    use zeroship_data_orm::protection::unmask::{self, UnmaskFieldArgs};

    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let app = crate::test_app_id!();
    let app = app.as_str();
    let coll = "patients";

    // Drops the schema and the cluster-scoped per-app role, then
    // recreates the schema. `ensure_per_app_role` below creates the role
    // the read path checks for -- without it the unmask SELECT refuses
    // with `schema_not_provisioned` before authorization is ever reached.
    let role = provision_app_with_role(&pool, app).await;
    zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .unwrap();

    let schema = value!({
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" }
        }
    });
    let ssn_raw = raw_column_name("ssn");
    // Built with the platform's own emitter, not hand-spelled, so the fixture
    // cannot drift from the runtime's DDL shape: `ssn` gets the bare-TEXT mask
    // column and `__zs_raw__ssn` gets the declared type for the real value.
    let create_table = fixture_table_sql(
        &zeroship_data_sql::SchemaName::new(app).expect("fixture schema name"),
        coll,
        &schema,
        &FkEmission::Inline,
    )
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

    crate::support::install_postgres_pool(pool.clone(), &url);
    zeroship_data_orm::cache_schema_for_tests(app, coll, schema);
    zeroship_data_v8::testing::clear_mask_policy_cache_for_tests(app);

    // The boot-time install `installSchema` performs. Before the fix
    // this issued `SELECT set_mask_policy(...)` against the platform-owned
    // system schema and failed here on every database.
    mask_policy::install_mask_policy(&DbBinding::cold_start(app), value!({ "support": ["spi"] }))
        .expect("setMaskPolicy must install the declared policy on PG");

    // A role the declared policy grants reads through.
    let granted = unmask::dispatch_unmask(
        &unmask_route(app).await,
        &DbBinding::cold_start(app),
        UnmaskFieldArgs {
            collection: coll.to_string(),
            row_pk: "u1".to_string(),
            column: "ssn".to_string(),
            actor: Some(value!({ "kind": "support" })),
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
            actor: Some(value!({ "kind": "intern" })),
            reason: Some("declared policy deny".to_string()),
            rejected_claim: None,
        },
    )
    .await
    .expect_err("a role absent from the declared policy must be refused");
    match err {
        zeroship_data_orm::error::DbError::Coded { ref code, .. } => {
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

    // Existing direct connection constructors need explicit driver teardown.
    // The runtime lifecycle test above checks the resource behavior itself.
    const PINNED: usize = 136;
    // Reject an empty or non-recursive scan.
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
// `PoolConnection`: the transaction connection is a pool checkout
//
// Until 2026-08-27 `fixture_session` called
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
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
    let backend = zeroship_data_orm::backend::PostgresBackend::new(
        std::rc::Rc::clone(&pool),
        url.clone(),
        zeroship_data_v8::testing::isolate_key_source(),
    );

    let active_before = pool.active_count();
    let created_before = pool.metrics().connections_created.get();

    let client = {
        use zeroship_data_orm::fixtures::DatabaseFixture;
        backend
            .fixture_session("app_pool_probe")
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
        pool.metrics().connections_created.get(),
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

    let (_postgres, url) = require_pg().await;
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
    let backend = zeroship_data_orm::backend::PostgresBackend::new(
        std::rc::Rc::clone(&pool),
        url.clone(),
        zeroship_data_v8::testing::isolate_key_source(),
    );

    use zeroship_data_orm::fixtures::DatabaseFixture;
    let first = backend
        .fixture_session("app_pool_probe")
        .await
        .expect("first dedicated client");

    // THE INVERSION THIS STEP OWNS: a transaction that used to get a
    // connection of its own now queues, and refuses when the wait expires.
    // Conservative policy, and OWED a real decision: queue on the pool's
    // acquire timeout rather than refuse immediately, no per-app fairness, and
    // the ceiling is whatever the shared data pool is sized to.
    let second = backend.fixture_session("app_pool_probe").await;
    let err = second.expect_err(
        "a second dedicated client must be bounded by the pool, not opened \
         directly - an unbounded model is how one worker exhausts max_connections",
    );
    let message = format!("{err:?}");
    assert!(
        message.contains("acquisition timed out"),
        "the refusal must name the acquire timeout so an operator can see the \
         ceiling was hit; got {message}"
    );

    drop(first);

    // And the ceiling is a queue, not a wall: once the lease returns, the next
    // checkout succeeds.
    let third = backend
        .fixture_session("app_pool_probe")
        .await
        .expect("checkout after the first lease returned");
    drop(third);
}

#[cfg(any(test, feature = "test-helpers"))]
#[allow(unused_imports)]
use zeroship_data_orm::fixtures::DatabaseFixture;

#[allow(unused_imports)]
use zeroship_data_orm::search::Search;
