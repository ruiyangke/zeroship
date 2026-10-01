//! The PostgreSQL server a live suite binary owns is the one the suites target, it is
//! removed once the binary's process has ended, and each test's database goes with
//! its guard.
//!
//! `support::server` keeps the server in a `static`, which libtest never drops, and
//! leaves its removal to the shared `container_reaper`. Three of that module's paths
//! are measured here against real processes and a real daemon: a child test process
//! that exits normally, a child SIGKILLed while its server is still starting, and a
//! `docker` program that cannot see the container the daemon started.

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::support::container_reaper::{
    process_owner, start_owned, DockerCli, Ownership, OWNER_LABEL, REAPER_LABEL,
};
use crate::support::server;

/// The test the child processes run, by its full path in this binary.
const CHILD_TEST: &str = "owned_server::the_owned_postgres_server_is_the_one_the_suites_target";

/// The line the child prints its owner label on, before it asks for a server.
const OWNER_LINE: &str = "OWNED_POSTGRES_OWNER=";

/// The line the child prints its container id on, once its server is up.
const CONTAINER_LINE: &str = "OWNED_POSTGRES_CONTAINER=";

/// How long the reaper gets to remove a container after its process has ended.
const REMOVAL_BOUND: Duration = Duration::from_secs(90);

/// Docker's view of one container: `Some(status)` while it exists, `None` once it is
/// gone.
fn container_status(id: &str) -> Option<String> {
    let output = Command::new("docker")
        .args(["container", "inspect", "--format", "{{.State.Status}}", id])
        .output()
        .expect("run the docker CLI");
    if output.status.success() {
        return Some(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("No such container") || stderr.contains("No such object"),
        "docker could not answer whether {id} exists: {stderr}"
    );
    None
}

/// Wait for Docker to stop listing `id`, or fail naming `why` it should have.
fn wait_until_removed(id: &str, why: &str) {
    let deadline = Instant::now() + REMOVAL_BOUND;
    while let Some(status) = container_status(id) {
        assert!(
            Instant::now() < deadline,
            "container {id} is still listed ({status}) {REMOVAL_BOUND:?} after {why}; \
             its reaper did not remove it"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// This process's own server reads as running: the instrument's positive control,
/// without which an absence measured by [`container_status`] proves nothing.
fn own_server_is_running() -> String {
    let own = server::postgres().container_id().to_string();
    assert_eq!(
        container_status(&own).as_deref(),
        Some("running"),
        "this binary's own server must read as running"
    );
    own
}

/// A child process running [`CHILD_TEST`] alone, its stdout piped.
fn child_test() -> Command {
    let mut command = Command::new(std::env::current_exe().expect("locate this test binary"));
    command.args([CHILD_TEST, "--exact", "--nocapture", "--test-threads=1"]);
    command
}

#[test]
fn the_owned_postgres_server_is_the_one_the_suites_target() {
    println!("{OWNER_LINE}{}", process_owner());
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
         the owned server reports {version}"
    );
    assert_eq!(
        database,
        db.name(),
        "the DSN a test is handed must reach the database made for it"
    );
    println!("{CONTAINER_LINE}{}", server::postgres().container_id());
}

#[test]
fn an_owned_server_is_removed_when_its_process_ends() {
    let own = own_server_is_running();

    let output = child_test()
        .output()
        .expect("run one test of this binary in a child process");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the child test failed ({}):\n{stdout}\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains("running 1 test"),
        "the child must have run exactly the one test it was pointed at:\n{stdout}"
    );
    let child = stdout
        .lines()
        .find_map(|line| line.split_once(CONTAINER_LINE).map(|(_, id)| id))
        .unwrap_or_else(|| panic!("the child did not report its container:\n{stdout}"))
        .trim()
        .to_string();
    assert_ne!(
        child, own,
        "the child process has to have started a server of its own"
    );
    wait_until_removed(&child, "the child exited");
}

#[test]
fn an_owned_server_whose_process_is_killed_while_it_starts_is_removed() {
    own_server_is_running();

    let mut child = child_test()
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("run one test of this binary in a child process");
    let mut lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    let owner = lines
        .by_ref()
        .map_while(Result::ok)
        .find_map(|line| {
            line.split_once(OWNER_LINE)
                .map(|(_, owner)| owner.trim().to_string())
        })
        .expect("the child reports its owner label before it asks for a server");

    // The container exists from the moment it is created, seconds before PostgreSQL
    // has initialized and the start returns. Kill the child inside that window.
    let real = DockerCli::system();
    let deadline = Instant::now() + REMOVAL_BOUND;
    let id = loop {
        let ids = real
            .labelled(OWNER_LABEL, &owner)
            .expect("the docker CLI lists containers by label");
        if let Some(id) = ids.into_iter().next() {
            break id;
        }
        assert!(
            Instant::now() < deadline,
            "the child never created its server container"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    child.kill().expect("SIGKILL the child");
    child.wait().expect("reap the child");

    let rest: Vec<String> = lines.map_while(Result::ok).collect();
    assert!(
        !rest.iter().any(|line| line.contains(CONTAINER_LINE)),
        "the child's server finished starting before the kill landed, so this run did \
         not measure a kill during startup:\n{}",
        rest.join("\n")
    );
    wait_until_removed(&id, "the child was killed while its server was starting");
}

#[test]
fn a_docker_cli_that_cannot_see_the_container_refuses_the_start_and_removes_it() {
    // The real CLI sees this process's own server by the owner label: the query the
    // absence below is read through can find a container that is there.
    let own = own_server_is_running();
    let real = DockerCli::system();
    assert!(
        real.labelled(OWNER_LABEL, process_owner())
            .expect("the real docker CLI answers")
            .contains(&own),
        "the real CLI must list this process's own server by its owner label"
    );

    let fakes = [
        (
            "a CLI that cannot reach the daemon",
            "echo 'Cannot connect to the Docker daemon at unix:///fake/docker.sock' >&2\nexit 1",
            "Cannot connect to the Docker daemon",
        ),
        (
            "a CLI that reaches a different daemon",
            "exit 0",
            "it listed []",
        ),
    ];
    let dir = tempfile::tempdir().expect("a directory for the fake docker programs");
    for (index, (what, body, expected)) in fakes.into_iter().enumerate() {
        let program = dir.path().join(format!("docker-{index}"));
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\ncase \"$1\" in --version) echo 'Docker version 0.0.0, fake'; \
                 exit 0 ;; esac\n{body}\n"
            ),
        )
        .expect("write the fake docker program");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("make the fake docker program executable");

        let ownership = Ownership::mint();
        let refusal = start_owned(
            &DockerCli::at(&program),
            &ownership,
            server::postgres_request(),
        )
        .err()
        .unwrap_or_else(|| panic!("{what}: the start must be refused"));
        assert!(refusal.contains(expected), "{what}: {refusal}");
        let id = refusal
            .split_once("removes container ")
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .unwrap_or_else(|| panic!("{what}: the refusal must name the container: {refusal}"))
            .to_string();
        assert!(
            id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "{what}: the daemon started a container and the refusal names it: {refusal}"
        );
        assert_eq!(
            container_status(&id),
            None,
            "{what}: a refused start must not leave its container behind"
        );
        assert_eq!(
            real.labelled(REAPER_LABEL, ownership.reaper())
                .expect("the real docker CLI answers"),
            Vec::<String>::new(),
            "{what}: nothing may carry the refused start's reaper label"
        );
    }
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
