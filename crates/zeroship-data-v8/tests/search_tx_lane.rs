//! Which CONNECTION the SEARCH FAMILY uses, inside `db.transaction(fn)`.
//!
//! # The claim these tests exist to rule on
//!
//! `run_search` and `run_near` take a `TxRoute` and read its backend once, so
//! the scan and the read pipeline name the same `BackendHandle`. A handle is
//! not a connection. `exec_query` honours `route.in_tx()` and issues on the
//! app's parked transaction client; `VectorIndex::vector_search` and
//! `SpatialIndex::spatial_near` used to call the handle directly, which lowers
//! to `pg_autocommit::roled_json` - its own `pool.acquire()` + `BEGIN` + `COMMIT`.
//!
//! The consequence, one per family member and neither implying the other: a
//! vector `search` or a spatial `near` issued inside `db.transaction(fn)`
//! cannot see rows that same transaction has written.
//!
//! This is the search half of what `unmask_tx_lane.rs` rules on for the unmask
//! fetch. The two are separate targets because they reach the pool through
//! different functions, and a fix to one is not evidence about the other.
//!
//! # Why each test carries a control that differs in ONE variable
//!
//! "The search returned nothing" and "the fixture wrote nothing" are the same
//! observation from the outside. Every arm therefore runs THREE reads over the
//! same row at the same instant:
//!
//! 1. a plain `find` on the transaction route - proves the row is there and the
//!    transaction lane reaches it;
//! 2. the SAME search on a POOLED route - proves the row really is uncommitted,
//!    so the subject arm is a question about lanes and not about visibility;
//! 3. the SUBJECT: the search on the transaction route.
//!
//! Arms 2 and 3 differ in exactly one token: `route.in_tx()`.
//!
//! # Run it
//!
//! Needs a live PostgreSQL with **both** `vector` and `postgis`, named by
//! `PG_TEST_URL` or by the overlay (`deploy/ops/zeroship.test.toml`) - the same
//! server `search_ir_live` documents. It FAILS rather than skips when the
//! server is unreachable or an extension cannot be created: a skipping run of a
//! lane suite is indistinguishable from a passing one.
//!
//! ```text
//! PG_TEST_URL=postgres://postgres:postgres@127.0.0.1:5478/postgres \
//!   cargo test -p zeroship-data-v8 --features test-helpers \
//!     --test test_helpers -- --test-threads=1 search_tx_lane::
//! ```

// `support` and `schema_fixture` are declared once by `tests/test_helpers.rs`,
// the entry file this module hangs off; its header says why a second declaration
// here would be a second copy of their statics.
#[allow(unused_imports)]
use crate::schema_fixture::{fixture_table_sql, fixture_table_sql_for};
use crate::support;
#[allow(unused_imports)]
use zeroship_migrate::schema::query::FkEmission;

use std::rc::Rc;

use compio_postgres::{NoTls, Pool};
use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::error::DbError;
use zeroship_data_sql::value::{Value, value};
use zeroship_data_sql::compile::SqlDialect;
use zeroship_data_orm::tx_route::{CapturedRoute, TxRoute};

fn test_url() -> String {
    zeroship_core::config::test_database_url()
}

/// Connect, or fail the test. Deliberately NOT a skip, for the reason in the
/// module header.
async fn require_pg() -> String {
    let url = test_url();
    match compio_postgres::connect(&url, NoTls).await {
        Ok((client, connection)) => {
            compio::runtime::spawn(async move {
                let _ = connection.run().await;
            })
            .detach();
            drop(client);
            url
        }
        Err(e) => {
            panic!("the search-tx-lane suite requires a reachable server at PG_TEST_URL: {e}")
        }
    }
}

async fn release_pg(pool: Rc<Pool>) {
    drop(pool);
    zeroship_data_v8::reset_context_for_tests();
    let _ = compio_postgres::drain_connections(std::time::Duration::from_secs(2)).await;
}

/// Create the extension the arm needs, naming it if the server refuses.
///
/// Created rather than probed-and-skipped, exactly as `search_ir_live` does: a
/// skip turns "this server has no pgvector" into a pass.
async fn require_extension(pool: &Rc<Pool>, extension: &str) {
    pool.execute(&format!("CREATE EXTENSION IF NOT EXISTS {extension}"), &[])
        .await
        .unwrap_or_else(|e| {
            panic!(
                "the search-tx-lane suite needs the `{extension}` extension and the server \
                 refused to create it: {e}. Point PG_TEST_URL at a server that has both \
                 `vector` and `postgis` (e.g. pgvector/pgvector:pg17 with postgis available)."
            )
        });
}

/// Build the app schema from the PLATFORM's own DDL emitter, provision the
/// per-app role the data plane runs under, and install the descriptor entry a
/// deploy would have installed.
async fn fixture(pool: &Rc<Pool>, url: &str, app: &str, collection: &str, schema: Value) {
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    let ddl = fixture_table_sql(
        &zeroship_data_sql::SchemaName::new(app).expect("fixture schema name"),
        collection,
        &schema,
        &FkEmission::Inline,
    )
    .expect("the platform's own CREATE TABLE emitter");
    pool.batch_execute(&ddl)
        .await
        .unwrap_or_else(|e| panic!("emitted DDL must apply: {e}\n{ddl}"));

    zeroship_data_orm::auth::bootstrap::ensure_per_app_role(pool, app)
        .await
        .expect("per-app role, as the deploy would provision it");
    support::grant_all_runtime_table_columns(pool, app, collection).await;

    zeroship_data_v8::set_postgres_pool_for_tests(Rc::clone(pool), url);
    zeroship_data_orm::cache_schema_for_tests(app, collection, schema);
}

/// The backend handle the V8 dispatcher would have bound for this dispatch.
async fn backend() -> zeroship_data_orm::backend::BackendHandle {
    zeroship_data_v8::tx_scope::ensure_backend()
        .await
        .expect("the backend the V8 dispatcher would have opened")
}

/// A route that claims the app's open transaction — what `CapturedRoute::capture`
/// produces for a dispatch issued inside `db.transaction(fn)`.
async fn tx_route(app: &str) -> TxRoute {
    CapturedRoute::tx_for_tests(app, SqlDialect::Postgres).bind(backend().await)
}

/// A route outside any transaction.
async fn pool_route(app: &str) -> TxRoute {
    CapturedRoute::pool_for_tests(app, SqlDialect::Postgres).bind(backend().await)
}

/// Run the real `plan_find` + `run_find` pair on `route`.
async fn find_on(
    route: TxRoute,
    app: &str,
    collection: &str,
    filter: Value,
) -> Result<Vec<Value>, DbError> {
    let binding = DbBinding::cold_start(app);
    let plan = zeroship_data_orm::crud::plan_find(&binding, collection, &filter, &value!({}));
    zeroship_data_orm::crud::run_find(binding, collection.to_string(), route, filter, plan)
        .await
        .map(|r| r.rows)
}

/// Run the real `plan_search` + `run_search` pair on `route`.
async fn search_on(
    route: TxRoute,
    app: &str,
    collection: &str,
    args: Value,
) -> Result<Vec<Value>, DbError> {
    let binding = DbBinding::cold_start(app);
    let plan =
        zeroship_data_orm::crud::plan_search(&binding, SqlDialect::Postgres, collection, &args)?;
    zeroship_data_orm::crud::run_search(&route, binding, collection.to_string(), plan)
        .await
        .map(|r| r.rows)
}

/// Run the real `plan_near` + `run_near` pair on `route`.
async fn near_on(
    route: TxRoute,
    app: &str,
    collection: &str,
    args: Value,
) -> Result<Vec<Value>, DbError> {
    let binding = DbBinding::cold_start(app);
    let plan =
        zeroship_data_orm::crud::plan_near(&binding, SqlDialect::Postgres, collection, &args)?;
    zeroship_data_orm::crud::run_near(&route, binding, collection.to_string(), plan)
        .await
        .map(|r| r.rows)
}

fn code_of(err: &DbError) -> String {
    match err {
        DbError::ValidationFailed { code, .. } => (*code).to_string(),
        DbError::Coded { code, .. } => code.clone(),
        other => format!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. Vector search
// ---------------------------------------------------------------------------

/// A `search({ vector })` inside `db.transaction(fn)` must reach a row that
/// same transaction inserted.
///
/// The insert goes through the real write pipeline on the transaction route, so
/// the row exists only on the parked transaction connection. `run_search` calls
/// `VectorIndex::vector_search`, which lowered to `pg_autocommit::roled_json` -
/// a fresh pooled checkout that cannot see it.
#[compio::test]
async fn a_vector_search_inside_a_transaction_sees_the_row_that_transaction_inserted() {
    let url = require_pg().await;
    let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
    require_extension(&pool, "vector").await;

    let app = "search_lane_vector";
    let coll = "docs";
    fixture(
        &pool,
        &url,
        app,
        coll,
        value!({
            "embedding": { "type": "vector", "vectorDims": 4 },
            "title": { "type": "string" },
        }),
    )
    .await;

    zeroship_data_v8::begin_transaction_for_tests(app, &url).await;

    let inserted = zeroship_data_orm::crud::run_insert(
        DbBinding::cold_start(app),
        coll.to_string(),
        tx_route(app).await,
        value!({ "embedding": [1.0, 0.0, 0.0, 0.0], "title": "in the transaction" }),
        None,
    )
    .await
    .expect("the write pipeline + insert builder must apply on the transaction lane");
    let id = inserted.rows[0]["id"]
        .as_str()
        .expect("the write pipeline must mint an id")
        .to_string();

    // ---- CONTROL 1: the transaction lane sees its own uncommitted row.
    let inside_plain = find_on(tx_route(app).await, app, coll, value!({ "id": &id }))
        .await
        .expect("a plain find inside the transaction must be authorised to run");
    assert_eq!(
        inside_plain.len(),
        1,
        "the transaction lane must see its own uncommitted row; without this the \
         subject arm below rules on nothing: {inside_plain:?}",
    );

    assert_eq!(inserted.rows[0]["embedding"], value!([1.0, 0.0, 0.0, 0.0]));
    assert_eq!(inside_plain[0]["embedding"], value!([1.0, 0.0, 0.0, 0.0]));

    let args = value!({ "vector": [1.0, 0.0, 0.0, 0.0], "k": 10 });

    // ---- CONTROL 2: the same search, POOLED. Differs in one token: `in_tx`.
    let outside = search_on(pool_route(app).await, app, coll, args.clone())
        .await
        .expect("a pooled vector search is authorised to run");
    assert!(
        outside.is_empty(),
        "the row must be invisible outside the transaction, or the subject arm \
         below cannot distinguish the two lanes: {outside:?}",
    );

    // ---- SUBJECT: the same search on the transaction's own lane.
    let inside = search_on(tx_route(app).await, app, coll, args).await;

    zeroship_data_v8::rollback_transaction_for_tests(app).await;

    let inside = inside.unwrap_or_else(|e| {
        panic!(
            "a vector search inside a transaction must be authorised to run. Got {}: {e:?}",
            code_of(&e),
        )
    });
    assert_eq!(
        inside.len(),
        1,
        "a vector search inside a transaction must reach the row that transaction \
         inserted. The plain find on the SAME route found it (control 1) and the \
         pooled search did not (control 2), so an empty result here means the \
         scan took the autocommit lane: {inside:?}",
    );
    assert_eq!(inside[0]["id"], value!(id));
    assert_eq!(inside[0]["embedding"], value!([1.0, 0.0, 0.0, 0.0]));
    assert!(
        inside[0].get("_distance").is_some(),
        "the row must carry pgvector's synthetic distance column: {inside:?}",
    );

    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 2. Spatial near
// ---------------------------------------------------------------------------

/// A `near({ field, point, radius })` inside `db.transaction(fn)` must reach a
/// row that same transaction inserted.
///
/// Same shape as the vector arm, through the other trait. The two are
/// independent: `run_search` and `run_near` are separate functions calling
/// separate trait impls, and each reached the pool on its own.
///
#[compio::test]
async fn a_spatial_near_inside_a_transaction_sees_the_row_that_transaction_inserted() {
    let url = require_pg().await;
    let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
    require_extension(&pool, "postgis").await;

    let app = "search_lane_spatial";
    let coll = "places";
    fixture(
        &pool,
        &url,
        app,
        coll,
        value!({
            "location": { "type": "geoPoint" },
            "title": { "type": "string" },
        }),
    )
    .await;

    zeroship_data_v8::begin_transaction_for_tests(app, &url).await;

    let point = value!({"lat": 51.5074, "lng": -0.1278});
    let inserted = zeroship_data_orm::crud::run_insert(
        DbBinding::cold_start(app),
        coll.to_string(),
        tx_route(app).await,
        value!({"location": point.clone(), "title": "in the transaction"}),
        None,
    )
    .await
    .expect("native geographic values must insert through the shared ORM");
    let id = inserted.rows[0]["id"].as_str().unwrap().to_owned();
    assert_eq!(inserted.rows[0]["location"], point);

    // ---- CONTROL 1: the transaction lane sees its own uncommitted row.
    let inside_plain = find_on(tx_route(app).await, app, coll, value!({ "id": &id }))
        .await
        .expect("a plain find inside the transaction must be authorised to run");
    assert_eq!(
        inside_plain.len(),
        1,
        "the transaction lane must see its own uncommitted row; without this the \
         subject arm below rules on nothing: {inside_plain:?}",
    );

    assert_eq!(inside_plain[0]["location"], point);
    let args = value!({
        "field": "location",
        "point": { "lat": 51.5074, "lng": -0.1278 },
        "radius": 1000.0,
        "limit": 10,
    });

    // ---- CONTROL 2: the same near, POOLED. Differs in one token: `in_tx`.
    let outside = near_on(pool_route(app).await, app, coll, args.clone())
        .await
        .expect("a pooled spatial near is authorised to run");
    assert!(
        outside.is_empty(),
        "the row must be invisible outside the transaction, or the subject arm \
         below cannot distinguish the two lanes: {outside:?}",
    );

    // ---- SUBJECT: the same near on the transaction's own lane.
    let inside = near_on(tx_route(app).await, app, coll, args).await;

    zeroship_data_v8::rollback_transaction_for_tests(app).await;

    let inside = inside.unwrap_or_else(|e| {
        panic!(
            "a spatial near inside a transaction must be authorised to run. Got {}: {e:?}",
            code_of(&e),
        )
    });
    assert_eq!(
        inside.len(),
        1,
        "a spatial near inside a transaction must reach the row that transaction \
         inserted. The plain find on the SAME route found it (control 1) and the \
         pooled near did not (control 2), so an empty result here means the scan \
         took the autocommit lane: {inside:?}",
    );
    assert_eq!(inside[0]["id"], value!(id));
    assert_eq!(inside[0]["location"], point);
    assert!(
        inside[0].get("_distance_m").is_some(),
        "the row must carry PostGIS's synthetic distance column: {inside:?}",
    );

    release_pg(pool).await;
}
