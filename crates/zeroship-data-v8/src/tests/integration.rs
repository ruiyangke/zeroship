//! Adapter contracts exercised through JavaScript dispatch.

#[allow(unused_imports)]
use crate::tests::fixtures::schema::{fixture_table_sql, fixture_table_sql_for};

use crate::tests::parity;

#[allow(unused_imports)]
use zeroship_migrate::schema::query::FkEmission;

use compio_postgres::{NoTls, Pool};

use uuid::Uuid;

use zeroship_data_sql::value::value;

async fn require_pg() -> (crate::tests::fixtures::postgres::Postgres, String) {
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
            crate::tests::fixtures::set_database_url(&url);
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

/// The half of [`release_pg`] that owns no pool, for tests whose handles have
/// already gone out of scope. Every handle must be dropped first: a live one
/// keeps its connection counted and makes this wait out its whole budget.
async fn drain_pg() {
    // The context can hold its own pool handle and a parked transaction
    // client; those keep connections counted, so clear it before waiting.
    crate::tests::fixtures::reset_context();
    if !compio_postgres::drain_connections(std::time::Duration::from_secs(2)).await {
        eprintln!(
            "DRAIN-TIMEOUT: {} connection(s) still live",
            compio_postgres::live_connections()
        );
    }
}

/// Postgres and the dev SQLite tier must hand `env.db` callers the same JSON.
///
/// Included in the native data suite. Missing PostgreSQL prerequisites fail
/// through `require_pg`; they never turn this comparison into a skipped leg.
#[compio::test]
async fn parity_matrix_pg_matches_sqlite_projection() {
    let (_postgres, pg_url) = require_pg().await;
    let sqlite_dir = tempfile::tempdir().expect("create sqlite parity dir");

    let app = crate::tests::fixtures::test_app_id!();

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
    let app = crate::tests::fixtures::test_app_id!();
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

#[cfg(test)]
#[allow(unused_imports)]
use zeroship_data_orm::search::Search;

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
    let app_schema = zeroship_workflow::store::pg::app_schema_for(&app_id);
    let tables = zeroship_workflow::store::pg::WorkflowTables::for_app_id(&app_id);
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
    zeroship_workflow::store::pg::PgStore::provision(&client, &app_id)
        .await
        .expect("provision app-local workflow journal");
    crate::tests::fixtures::roles::ensure_per_app_role(&pool, &app_schema)
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
