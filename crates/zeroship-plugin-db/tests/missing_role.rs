//! The anchor regression, against a REAL PostgreSQL server.
//!
//! A creator deploys an `env.db` app and skips `zeroship migrate`. The
//! per-app role was never created, so `SET LOCAL ROLE "app_<id>_role"` in
//! the autocommit session setup (`plugin_db::exec`) refuses before any
//! creator SQL runs. The contextual session-setup classifier turns that
//! measured failure into a creator-facing provisioning error; generic
//! PostgreSQL errors remain on `pg_error::classify`.
//!
//! This test exists because the discriminator's unit tests in
//! `backend/pg_error.rs` cannot construct a `compio_postgres::Error`: the
//! driver exposes no public constructor, so only a live server can synthesise
//! the measured failure. That also makes this the test that fails if the
//! contextual session-setup arm is deleted.
//!
//! Requires: the test PostgreSQL named by the overlay
//! (`deploy/ops/zeroship.test.toml`, written by
//! `tests/provision_test_backends.sh`) or by `PG_TEST_URL`. There is no
//! compiled default: this file used to fall back to `localhost:5434`, a
//! DIFFERENT server with different credentials, so a run with no overlay
//! silently measured whatever happened to be listening there.
//! Run: `cargo test -p zeroship-plugin-db --test missing_role \
//!       --features test-helpers -- --test-threads=1`
//!
//! WHAT THIS TEST DOES NOT CATCH:
//!   - The HTTP boundary. It asserts the classification and the message
//!     plugin-db produces, not that the runtime rail lets it through --
//!     that is `crates/runtime/src/core/dispatch.rs`'s
//!     `schema_not_provisioned_survives_the_5xx_rail_in_both_spellings`
//!     and its one-variable control.
//!   - A non-English server. The classifier matches PostgreSQL's
//!     English primary message; under a translated `lc_messages` this
//!     test's case would classify `internal` again, and so would
//!     production. The failure mode is a false NEGATIVE (today's
//!     behaviour), never a false positive.
//!   - Any role-missing path that does not go through `SET LOCAL ROLE`.
//!     Those paths deliberately stay on the generic classifier.

use compio_postgres::NoTls;
use zeroship_plugin_db::backend::pg_error;
use zeroship_plugin_db::error::DbError;

fn test_url() -> String {
    zeroship_core::config::test_database_url()
}

async fn connect_test_client() -> compio_postgres::Client {
    let url = test_url();
    let (client, connection) = compio_postgres::connect(&url, NoTls)
        .await
        .unwrap_or_else(|e| panic!("live-Postgres test requires a server at PG_TEST_URL: {e}"));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

fn read_startup_packet(stream: &mut std::net::TcpStream) {
    use std::io::Read;

    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .expect("set fake Postgres read timeout");
    let mut length = [0_u8; 4];
    stream
        .read_exact(&mut length)
        .expect("read startup packet length");
    let length = u32::from_be_bytes(length) as usize;
    assert!(
        length >= 4,
        "startup packet length includes its four-byte header"
    );
    let mut payload = vec![0_u8; length - 4];
    stream
        .read_exact(&mut payload)
        .expect("read startup packet payload");
}

fn push_backend_message(out: &mut Vec<u8>, tag: u8, payload: &[u8]) {
    out.push(tag);
    out.extend_from_slice(&u32::try_from(payload.len() + 4).unwrap().to_be_bytes());
    out.extend_from_slice(payload);
}

fn accept_fake_client(listener: &std::net::TcpListener) -> std::net::TcpStream {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for fake Postgres client"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(err) => panic!("accept fake Postgres client: {err}"),
        }
    }
}

fn spawn_pool_reconnect_server(role: &str) -> (String, std::thread::JoinHandle<()>) {
    use std::io::Write;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake Postgres listener");
    listener
        .set_nonblocking(true)
        .expect("make fake Postgres listener nonblocking");
    let address = listener.local_addr().expect("read fake Postgres address");
    let server_role = role.to_string();

    let server = std::thread::spawn(move || {
        let mut warm = accept_fake_client(&listener);
        read_startup_packet(&mut warm);
        let mut success = Vec::new();
        push_backend_message(&mut success, b'R', &0_u32.to_be_bytes());
        push_backend_message(&mut success, b'Z', b"I");
        warm.write_all(&success)
            .expect("complete warm Postgres handshake");

        let mut reconnect = accept_fake_client(&listener);
        read_startup_packet(&mut reconnect);
        let message = format!("role \"{server_role}\" does not exist");
        let mut fields = Vec::new();
        for (tag, value) in [(b'S', "FATAL"), (b'V', "FATAL"), (b'C', "28000")] {
            fields.push(tag);
            fields.extend_from_slice(value.as_bytes());
            fields.push(0);
        }
        fields.push(b'M');
        fields.extend_from_slice(message.as_bytes());
        fields.push(0);
        fields.push(0);

        let mut refusal = Vec::new();
        push_backend_message(&mut refusal, b'E', &fields);
        reconnect
            .write_all(&refusal)
            .expect("write FATAL role-missing response");
    });

    (
        format!("postgres://{role}:unused@{address}/postgres?sslmode=disable"),
        server,
    )
}

/// Wait for a dropped direct client to finish closing its socket while this
/// test's compio runtime is still alive.
async fn drain_pg() {
    assert!(
        compio_postgres::drain_connections(std::time::Duration::from_secs(2)).await,
        "direct Postgres client did not close; {} connection(s) remain",
        compio_postgres::live_connections()
    );
}

/// Drive a real `SET LOCAL ROLE` against a role that does not exist and
/// hand the resulting server error to the classifier.
async fn classify_missing_role(app_id: &str) -> DbError {
    let client = connect_test_client().await;
    let role = zeroship_core::database_role::per_app_role_name(app_id)
        .expect("missing-role fixture app id must produce a valid PostgreSQL role name");

    // Same shape as `exec::query_postgres_pool_with_autocommit_role`: the
    // SET LOCAL runs inside an explicit transaction, so the failure is the
    // one a creator's first `env.db` call actually hits.
    let err = client
        .simple_query(&format!("BEGIN; SET LOCAL ROLE \"{role}\""))
        .await
        .expect_err("SET LOCAL ROLE to a nonexistent role must fail");

    assert_eq!(
        err.code().map(|code| code.code()),
        Some("22023"),
        "missing-role SET LOCAL ROLE must report the measured SQLSTATE"
    );

    let classified = pg_error::classify_pg_per_app_session_setup_for_tests(&err, app_id);
    drop(client);
    drain_pg().await;
    classified
}

#[compio::test]
async fn missing_per_app_role_is_creator_facing_not_internal() {
    // A name no cluster will have. Shaped like `per_app_role_name` output
    // so the case is the production one.
    let app_id = uuid::Uuid::new_v4().simple().to_string();
    let role = zeroship_core::database_role::per_app_role_name(&app_id)
        .expect("missing-role fixture app id must produce a valid PostgreSQL role name");
    let classified = classify_missing_role(&app_id).await;

    let op = classified.to_op_error();
    let code = match &op.kind {
        zeroship_runtime::state::OpErrorKind::CodedError { code, .. } => code.clone(),
        other => panic!("expected CodedError, got {other:?}"),
    };

    assert_eq!(
        code, "schema_not_provisioned",
        "a missing per-app role is a creator CONFIGURATION state with a documented \
         one-command fix, not an internal fault. Got {code:?} with message {:?}",
        op.message
    );

    // THE ACCEPTANCE BAR: the response names the command.
    assert!(
        op.message.contains("zeroship migrate"),
        "the creator must learn what to run from this message: {:?}",
        op.message
    );

    // And it names nothing else. The message is the platform CONSTANT --
    // not a composition -- so nothing from the server can appear in it.
    // Asserting equality rather than absence is the stronger form: an
    // absence list can only rule out the leaks someone thought of.
    assert_eq!(
        op.message,
        zeroship_plugin_db::error::MISSING_ROLE_MESSAGE,
        "the wire message must be the fixed platform constant"
    );

    // The three specific things that WOULD have ridden out on the old
    // `Internal` path, spelled out so a future edit that starts composing
    // the message fails here with a readable reason rather than only on
    // the equality above. The role name embeds the app id; `ERROR:` is
    // `compio_postgres::DbError`'s severity prefix; `caused by` is
    // `walk_pg_chain`'s source-chain joiner.
    assert!(
        !op.message.contains(&role),
        "the role name (which embeds the app id) must not ride out: {:?}",
        op.message
    );
    assert!(
        !op.message.contains("ERROR:"),
        "server severity prefix must not ride out: {:?}",
        op.message
    );
    assert!(
        !op.message.contains("caused by"),
        "the walked source chain must not ride out: {:?}",
        op.message
    );
    assert!(op.message.is_ascii(), "ASCII only: {:?}", op.message);
}

#[compio::test]
async fn a_real_internal_pg_failure_is_still_internal() {
    // ONE-VARIABLE CONTROL, run against the SAME live server through the
    // SAME classifier: a statement that fails for a reason the creator
    // cannot fix with `zeroship migrate`. If the new arm had widened into
    // "any configuration-shaped SQLSTATE is creator-facing", this would
    // come back `schema_not_provisioned` too.
    //
    // 22023 deliberately -- the SAME SQLSTATE the missing-role case
    // reports. The only thing separating the two is the server's primary
    // message, so this proves the discriminator narrows rather than
    // rubber-stamping the SQLSTATE.
    let url = test_url();
    let (client, connection) = compio_postgres::connect(&url, NoTls)
        .await
        .unwrap_or_else(|e| panic!("live-Postgres test requires a server at PG_TEST_URL: {e}"));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    let err = client
        .simple_query("BEGIN; SET LOCAL statement_timeout = 'not-a-duration'")
        .await
        .expect_err("a bad GUC value must fail");

    // Confirm the control really is the same SQLSTATE, so a future server
    // version that changes it turns this into a visible failure rather
    // than a silently weaker control.
    assert_eq!(
        err.code().map(|c| c.code().to_string()).as_deref(),
        Some("22023"),
        "control must share the SQLSTATE of the case it controls for"
    );

    let op = pg_error::classify(&err).to_op_error();
    drop(client);
    drain_pg().await;
    match &op.kind {
        zeroship_runtime::state::OpErrorKind::CodedError { code, .. } => {
            assert_eq!(
                code, "internal",
                "a genuinely-internal 22023 must stay `internal` and be blanked at \
                 the rail; got {code:?}"
            );
        }
        other => panic!("expected CodedError, got {other:?}"),
    }
}

#[compio::test]
async fn pool_reconnect_missing_app_shaped_login_role_stays_internal() {
    let app_id = uuid::Uuid::new_v4().simple().to_string();
    let role = zeroship_core::database_role::per_app_role_name(&app_id)
        .expect("pool-reconnect fixture app id must produce a valid PostgreSQL role name");
    let (url, server) = spawn_pool_reconnect_server(&role);

    // A SHORT lifetime the warm entry then outlives, not `Duration::ZERO`.
    //
    // Zero used to work and stopped: `is_expired` is `Instant::now() >=
    // expiry`, so a zero lifetime expires the entry at the instant it is
    // created, and the pool's warm-up eligibility recheck then refuses to
    // publish a pool at all - `warm-up connection 1 became unusable before pool
    // publication`. That recheck is correct and deliberately covers lifetime:
    // an entry that sat across later connects/hooks may genuinely have aged
    // out. What was wrong was this fixture asking for a pool that can hold
    // nothing in order to force one reconnect.
    //
    // The sleep is a one-directional wait past a deadline, not a race: the
    // entry is eligible when `connect_with_pool_config` publishes it
    // microseconds later, and unambiguously expired 50ms after a 5ms lifetime.
    let mut pool_config = compio_postgres::PoolConfig::new();
    pool_config
        .max_size(1)
        .min_idle(0)
        .max_lifetime(std::time::Duration::from_millis(5));
    let pool = compio_postgres::Pool::connect_with_pool_config(&url, pool_config)
        .await
        .expect("warm pool as the temporary login role");
    compio::time::sleep(std::time::Duration::from_millis(50)).await;

    let err = match pool.get().await {
        Ok(_) => panic!("expired pool entry must reconnect after its login role is dropped"),
        Err(err) => err,
    };
    server.join().expect("fake Postgres server must finish");
    assert_eq!(
        err.code().map(|code| code.code()),
        Some("28000"),
        "pool reconnect must exercise the server-side FATAL role error"
    );
    let expected_message = format!("role \"{role}\" does not exist");
    assert_eq!(
        err.as_db_error().map(|db| db.message()),
        Some(expected_message.as_str()),
        "the reconnect error must have the same app-shaped message as session setup"
    );

    let op = pg_error::classify(&err).to_op_error();
    let code = match &op.kind {
        zeroship_runtime::state::OpErrorKind::CodedError { code, .. } => code.clone(),
        other => panic!("expected CodedError, got {other:?}"),
    };

    drop(pool);
    drain_pg().await;

    assert_eq!(
        code, "internal",
        "a missing pool login role is an operator DSN failure, not an app schema state"
    );
}
