//! The PostgreSQL server the live suites share is the one they target, it is
//! removed once the last process holding it has ended, it refuses to serve when
//! its watchdog cannot see the lease, and each test's database goes with its
//! guard.
//!
//! `crate::support::server` joins the worktree's shared server through
//! `zeroship_shared_server`. Its lifetime is measured here through
//! `zeroship_shared_server::lifetime` against real child processes and a real daemon:
//! a child that joins the suite's own recipe at a throwaway scope and exits
//! normally, and one `SIGKILL`ed while that server is still starting.

use crate::support::server::{self, SharedServer};
use zeroship_shared_server::{self as shared, lifetime};

/// The child the lifetime measurements run, by its full path in this binary.
const CHILD_TEST: &str =
    "integration::pg_engine::owned_server::child_joins_the_suites_server_at_a_throwaway_scope";

#[test]
fn the_shared_postgres_server_is_the_one_the_suites_target() {
    let db = crate::support::pg_database();
    let mut client = postgres::Client::connect(&db, postgres::NoTls)
        .expect("connect to the test's own database");
    let row = client
        .query_one(
            "SELECT current_setting('server_version_num')::int AS version, \
                    current_database() AS database",
            &[],
        )
        .expect("read the server version");
    let version: i32 = row.get("version");
    let database: String = row.get("database");
    assert!(
        version >= 180_000,
        "the live suites read catalog shapes and UUIDv7 generation from PostgreSQL 18; \
         the shared server reports {version}"
    );
    assert_eq!(
        database,
        db.name(),
        "the DSN a test is handed must reach the database made for it"
    );
}

/// Child side of the lifetime measurements: join the suite's recipe at the
/// throwaway scope on standard input and report the container.
#[test]
#[ignore = "spawned by a lifetime measurement as a child process, with its scope on stdin"]
fn child_joins_the_suites_server_at_a_throwaway_scope() {
    let server = SharedServer::join(&lifetime::child_scope()).expect("join the suite's server");
    let mut client = postgres::Client::connect(&server.postgres_admin_url(), postgres::NoTls)
        .expect("connect to the throwaway server");
    let version: i32 = client
        .query_one("SELECT current_setting('server_version_num')::int", &[])
        .expect("read the server version")
        .get(0);
    assert!(version >= 180_000, "the suite's recipe runs PostgreSQL 18, not {version}");
    lifetime::report_container(server.container_id());
}

#[test]
fn the_suites_server_is_removed_when_its_last_process_ends() {
    lifetime::assert_removed_after_the_child_exits(CHILD_TEST);
}

#[test]
fn the_suites_server_whose_process_is_killed_while_it_starts_is_removed() {
    lifetime::assert_removed_after_a_kill_during_startup(CHILD_TEST);
}

#[test]
fn a_server_container_that_cannot_see_the_lease_is_refused_and_removed() {
    // The positive control: this process's own server reads as running, so the
    // absence asserted below is read through a query that can see a container
    // that is there.
    let own = server::postgres().container_id().to_owned();
    assert_eq!(
        shared::container_status(&own).as_deref(),
        Some("running"),
        "this process's own server must read as running"
    );

    // The suite's image with no lease directory mounted: its watchdog could never
    // see a host lease, so the boot must refuse it rather than serve from a
    // container nothing would remove.
    let base = zeroship_shared_server::images::POSTGRES_18.reference();
    let image = zeroship_shared_server::image::with_watchdog(&base).expect("build the suite's image");
    let name = format!("zeroship-migrate-blind-{}", std::process::id());
    let blind = shared::run_detached(&image, &[], &[], &name).expect("run the blind container");
    let refusal = shared::refuse_a_blind_container(&blind)
        .expect_err("a container that cannot see the lease must be refused");
    assert!(refusal.contains("cannot see"), "{refusal}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while let Some(state) = shared::container_status(&blind) {
        if shared::removal_issued(Some(&state)) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the refused container {blind} is still listed ({state})"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
fn a_role_a_private_server_carries_is_invisible_to_the_shared_servers_cases() {
    let role = format!("zm_private_probe_{}", uuid::Uuid::now_v7().simple());
    let count = |url: &str| -> i64 {
        postgres::Client::connect(url, postgres::NoTls)
            .expect("connect to read the role catalog")
            .query_one("SELECT count(*) FROM pg_roles WHERE rolname = $1", &[&role])
            .expect("read pg_roles")
            .get(0)
    };

    let private = crate::support::private_pg_database();
    let shared = crate::support::pg_database();
    postgres::Client::connect(&private, postgres::NoTls)
        .expect("connect to the private server")
        .batch_execute(&format!("CREATE ROLE \"{role}\""))
        .expect("create the probe role");

    // The control: the role is there on the server that made it.
    assert_eq!(count(&private), 1, "the private server must carry its own role");
    assert_eq!(
        count(&shared),
        0,
        "a role-writing case's role reached the shared server's catalog, so every \
         other case's snapshot would read it"
    );
}

#[test]
fn a_test_database_is_gone_once_its_guard_drops_even_with_a_session_open() {
    let mut admin =
        postgres::Client::connect(&server::postgres().postgres_admin_url(), postgres::NoTls)
            .expect("connect to the admin database");
    let mut count = |name: &str| -> i64 {
        admin
            .query_one(
                "SELECT count(*) FROM pg_database WHERE datname = $1",
                &[&name],
            )
            .expect("read pg_database")
            .get(0)
    };

    let db = crate::support::pg_database();
    let name = db.name().to_string();
    assert_eq!(
        count(&name),
        1,
        "the guard's database exists while it lives"
    );
    let session =
        postgres::Client::connect(&db, postgres::NoTls).expect("a session the guard has to end");
    drop(db);
    assert_eq!(
        count(&name),
        0,
        "the database must be gone once its guard drops, open session or not"
    );
    drop(session);
}
