//! PostgreSQL fixtures used by this test module.

use crate::tests::fixtures::Host;

use compio_postgres::{NoTls, Pool};

use crate::value::{Value, value};

pub(super) async fn require_pg(
    host: &Host,
) -> (crate::tests::fixtures::postgres::Postgres, String) {
    let postgres = crate::tests::fixtures::postgres::Postgres::start();
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
            host.set_database_url(&url);
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
            panic!("PostgreSQL tests could not connect to its PostgreSQL testcontainer: {e}");
        }
    }
}

/// Set up a test's own schema and `notes` table. Drops and recreates on every
/// call, which is what makes a rerun idempotent.
///
/// `schema` is the caller's per-test app id (`test_app_id!()`). It was one
/// shared `const SCHEMA = "plugin_db_test"` until 2026-09-04, and the
/// `DROP ... CASCADE` below is why twenty tests then had to run serially.
pub(super) async fn setup(pool: &Pool, schema: &str) {
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{schema}\""), &[])
        .await
        .unwrap();
    pool.execute(
        &format!(
            // The descriptor below declares the same fields and defaults.
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
pub(super) async fn exec_query(
    pool: &Pool,
    bq: crate::sql::compiler::CompiledQuery,
) -> Vec<Value> {
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
pub(super) async fn exec_mutation(
    pool: &Pool,
    bq: crate::sql::compiler::CompiledQuery,
) -> Vec<Value> {
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

/// Simplified row → JSON (just text columns for testing).
pub(super) fn row_to_value(row: &compio_postgres::Row) -> Value {
    let mut obj = crate::value::Map::new();
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
pub(super) async fn release_pg(host: &Host, pool: std::rc::Rc<Pool>) {
    drop(pool);
    drain_pg(host).await;
}

/// The half of [`release_pg`] that owns no pool, for tests whose handles have
/// already gone out of scope. Every handle must be dropped first: a live one
/// keeps its connection counted and makes this wait out its whole budget.
pub(super) async fn drain_pg(host: &Host) {
    // The context can hold its own pool handle and a parked transaction
    // client; those keep connections counted, so clear it before waiting.
    host.reset();
    if !compio_postgres::drain_connections(std::time::Duration::from_secs(2)).await {
        eprintln!(
            "DRAIN-TIMEOUT: {} connection(s) still live",
            compio_postgres::live_connections()
        );
    }
}

/// Descriptor for the table created by `setup`.
pub(super) fn notes_schema() -> Value {
    crate::tests::fixtures::schema::generated_fields(value!({
        "id": { "type": "integer", "assign": {"by":"identity", "on":"insert"} },
        "title": { "type": "string" },
        "body": { "type": "string" },
        "category": { "type": "string" },
        "views": { "type": "int" },
        "tags": { "type": "json" },
    }))
}

/// The three system indexes every confined table carries.
pub(super) fn pg_system_indexes(app: &str, coll: &str) -> String {
    format!(
        r#"
CREATE INDEX IF NOT EXISTS "{coll}_deleted_at_idx" ON "{app}"."{coll}" ("deleted_at");
CREATE INDEX IF NOT EXISTS "{coll}_updated_at_idx" ON "{app}"."{coll}" ("updated_at");
CREATE INDEX IF NOT EXISTS "{coll}_created_by_idx" ON "{app}"."{coll}" ("created_by");
"#
    )
}

/// Install `pgvector` into the test database, or refuse the run.
///
/// # Panics
///
/// When the extension is not available on the server, with the image that
/// carries it. It used to return `false` and the callers announced a skip - so
/// a `--ignored` run on a stock `postgres:16` printed the same green as a run
/// that had exercised a single vector query.
pub(super) async fn require_pgvector(pool: &Pool) {
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

/// Provision a schema + its per-app role for a test. Returns the role
/// name. Idempotent re-runs are exercised by `per_app_role_created_at_provision`.
pub(super) async fn provision_app_with_role(pool: &std::rc::Rc<Pool>, app: &str) -> String {
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

pub(super) async fn install_role_bound_select_policy(
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

pub(super) fn login_role_test_url(base_url: &str, role: &str, password: &str) -> String {
    let (scheme, rest) = base_url.split_once("://").unwrap_or(("postgres", base_url));
    let host = rest
        .split_once('@')
        .map(|(_, suffix)| suffix)
        .unwrap_or(rest);
    format!("{scheme}://{role}:{password}@{host}")
}

pub(super) async fn provision_platform_login_pool(
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
pub(super) async fn require_postgis(pool: &Pool) {
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

/// The seven platform system columns, PostgreSQL spelling.
///
/// Hand-written, not rendered. plugin-db does not own DDL, so a test that needs
/// a table spells it; a fixture rendered by the layer under test cannot detect
/// that layer being wrong. Same argument as `tests/fixtures/data/sqlite.rs` on the
/// SQLite side.
pub(super) const PG_SYSTEM_COLUMNS: &str = r#"
  id TEXT PRIMARY KEY,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  created_by TEXT NULL,
  updated_by TEXT NULL,
  version INTEGER NOT NULL DEFAULT 1,
  deleted_at TIMESTAMPTZ NULL"#;
