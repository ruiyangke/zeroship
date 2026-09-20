//! PostgreSQL fixtures used by this test module.

use crate::tests::fixtures::Host;

use compio_postgres::{NoTls, Pool};

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

/// The three system indexes every confined table carries.
pub(super) fn pg_system_indexes(app: &str, coll: &str) -> String {
    let alias = crate::tests::fixtures::harness_alias(app);
    format!(
        r#"
CREATE INDEX IF NOT EXISTS "{coll}_deleted_at_idx" ON "{alias}"."{coll}" ("deleted_at");
CREATE INDEX IF NOT EXISTS "{coll}_updated_at_idx" ON "{alias}"."{coll}" ("updated_at");
CREATE INDEX IF NOT EXISTS "{coll}_created_by_idx" ON "{alias}"."{coll}" ("created_by");
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

/// Reset the physical schema a binding addresses and create the audit table
/// its unmask writes land in. Returns the binding role a session narrows to.
///
/// The roles themselves belong to
/// `crate::tests::fixtures::roles::ensure_binding_ladder`, which every caller
/// runs next; the schema is created here because the audit table needs a
/// namespace to land in, and the ladder's `CREATE SCHEMA IF NOT EXISTS` takes
/// it as it finds it.
pub(super) async fn provision_binding_schema(pool: &std::rc::Rc<Pool>, app: &str) -> String {
    let binding = crate::tests::fixtures::harness_binding(app);
    let alias = binding.schema().as_str().to_owned();
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{alias}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{alias}\""), &[])
        .await
        .unwrap();
    // Stand in for the migration service by creating the audit table before the
    // ladder issues its schema-wide data grants.
    //
    // `batch_execute`, not `execute`: this is multi-statement DDL and the
    // extended protocol refuses it with "cannot insert multiple commands into a
    // prepared statement".
    pool.batch_execute(&zeroship_migrate_server::provisioning::audit_unmask_table_sql(&alias))
        .await
        .unwrap();
    binding
        .session_role()
        .expect("a harness binding names the role its sessions narrow to")
        .to_owned()
}

pub(super) async fn install_role_bound_select_policy(
    pool: &std::rc::Rc<Pool>,
    app: &str,
    collection: &str,
    role: &str,
) {
    let alias = crate::tests::fixtures::harness_alias(app);
    pool.execute(
        &format!("ALTER TABLE \"{alias}\".\"{collection}\" ENABLE ROW LEVEL SECURITY"),
        &[],
    )
    .await
    .unwrap();
    pool.execute(
        &format!("ALTER TABLE \"{alias}\".\"{collection}\" FORCE ROW LEVEL SECURITY"),
        &[],
    )
    .await
    .unwrap();
    pool.execute(
        &format!("DROP POLICY IF EXISTS role_gate ON \"{alias}\".\"{collection}\""),
        &[],
    )
    .await
    .unwrap();
    pool.execute(
        &format!(
            "CREATE POLICY role_gate ON \"{alias}\".\"{collection}\" \
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
            &format!(
                "GRANT USAGE ON SCHEMA \"{}\" TO \"{login_role}\"",
                crate::tests::fixtures::harness_alias(app)
            ),
            &[],
        )
        .await
        .unwrap();
    // The membership edge reaches the column grants the capability role holds,
    // through the binding role the login is a member of. Giving the login a
    // table-level SELECT would bypass that column fence and make this RLS
    // control unlike the production login.
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

/// Common schema-declared columns used by PostgreSQL fixtures.
///
/// Hand-written, not rendered. The ORM does not own DDL, so a test that needs
/// a table spells it; a fixture rendered by the layer under test cannot detect
/// that layer being wrong. Same argument as `tests/fixtures/data/sqlite.rs` on the
/// SQLite side.
pub(super) const PG_COMMON_FIXTURE_COLUMNS: &str = r#"
  id TEXT PRIMARY KEY,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  created_by TEXT NULL,
  updated_by TEXT NULL,
  version INTEGER NOT NULL DEFAULT 1,
  deleted_at TIMESTAMPTZ NULL"#;
