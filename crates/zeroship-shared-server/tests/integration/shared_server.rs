//! The shared-server protocol: one server for every test process of a
//! worktree, elected by `flock`, leased for the life of a process and removed by
//! the container's watchdog once no lease is held.
//!
//! The cases drive a throwaway `PostgreSQL` server built from the stock image
//! plus the watchdog ([`shared::image::with_watchdog`]), because the protocol
//! is the subject and any server with a readiness probe exercises it. The
//! servers the workspace's fixtures run on top of it are contracted in
//! `zeroship-testkit`.
//!
//! The child tests are spawned from this binary as separate processes, with the
//! scope directory on standard input, never in the environment. They are
//! `#[ignore]`d so a normal run does not boot a throwaway server for them.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Lines, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use zeroship_shared_server::{self as shared, Scope};

/// The idle grace a throwaway scope is started with.
const GRACE: Duration = Duration::from_secs(2);

/// How long a container may take to disappear after its last lease is released.
const REMOVAL_BOUND: Duration = Duration::from_secs(42);

/// How long a child gets to reach a barrier.
const WAIT_BOUND: Duration = Duration::from_secs(120);

/// The mount point the lease directory is bound to in the container.
const CONTAINER_DIR: &str = "/run/zeroship-testkit";

/// The server the cases boot: a stock `PostgreSQL` with the watchdog on top.
const POSTGRES_IMAGE: &str = "postgres:16";

const CHILD_REPORT: &str = "integration::shared_server::child_join_report";
const CHILD_JOURNAL: &str = "integration::shared_server::child_join_journal";
const CHILD_BLOCK: &str = "integration::shared_server::child_join_block";
const CHILD_HOLD: &str = "integration::shared_server::child_join_hold";
const CHILD_LIFETIME: &str = "integration::shared_server::child_lifetime_join";
const CHILD_LIFETIME_LEAK: &str = "integration::shared_server::child_lifetime_leak";

/// A throwaway lease directory under the worktree's `target`, canonical so its
/// path is the one the container labels carry.
fn scratch(name: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the shared-server crate lives under crates/");
    let dir = root
        .join("target/zeroship-shared-server-tests")
        .join(format!("{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create a scratch lease directory");
    fs::canonicalize(&dir).expect("canonicalize the scratch directory")
}

fn cleanup(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

/// The server image, built before any process runs a container from it.
fn image() -> String {
    shared::image::with_watchdog(POSTGRES_IMAGE).expect("build the throwaway PostgreSQL image")
}

/// The image must exist before any process runs a container from it.
fn ensure_image() {
    let _ = image();
}

/// The readiness of a throwaway `PostgreSQL` server for `database`.
fn readiness(database: &str) -> shared::Readiness {
    let list = |values: &[&str]| values.iter().map(|value| (*value).to_owned()).collect();
    shared::Readiness {
        log_marker: "PostgreSQL init process complete".to_owned(),
        probe: list(&[
            "pg_isready",
            "-U",
            "postgres",
            "-h",
            "127.0.0.1",
            "-p",
            "5432",
        ]),
        answer: list(&["psql", "-U", "postgres", "-d", database, "-tAc", "SELECT 1"]),
    }
}

/// A throwaway server: the stock image, a database that always exists, and no
/// boot step.
fn test_spec(inputs: &str) -> shared::Spec {
    shared::Spec {
        inputs: inputs.to_owned(),
        image: image(),
        environment: vec![
            ("POSTGRES_PASSWORD".to_owned(), "fixture".to_owned()),
            ("POSTGRES_DB".to_owned(), "postgres".to_owned()),
        ],
        ports: vec![shared::Port {
            container: 5432,
            host: shared::HostPort::Assigned,
        }],
        watchdog: shared::image::WATCHDOG.to_owned(),
        entrypoint: shared::Entrypoint::Image,
        args: vec![
            "docker-entrypoint.sh".to_owned(),
            "postgres".to_owned(),
            "-c".to_owned(),
            "max_connections=100".to_owned(),
        ],
        ready: readiness("postgres"),
    }
}

/// A throwaway server whose database a case may drop to make the live server
/// stop answering.
fn victim_spec(inputs: &str) -> shared::Spec {
    let mut spec = test_spec(inputs);
    spec.environment = vec![
        ("POSTGRES_PASSWORD".to_owned(), "fixture".to_owned()),
        ("POSTGRES_DB".to_owned(), "victim".to_owned()),
    ];
    spec.ready = readiness("victim");
    spec
}

fn spawn_child(test_name: &str, dir: &Path) -> (Child, Lines<BufReader<ChildStdout>>) {
    let mut child = Command::new(std::env::current_exe().expect("locate this test binary"))
        .args([
            test_name,
            "--exact",
            "--nocapture",
            "--test-threads=1",
            "--ignored",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("run a child test of this binary");
    {
        let stdin = child.stdin.as_mut().expect("piped stdin");
        writeln!(stdin, "{}", dir.display()).expect("write the scope directory");
    }
    drop(child.stdin.take());
    let lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    (child, lines)
}

fn run_child(test_name: &str, dir: &Path) -> (std::process::ExitStatus, Vec<String>) {
    let (mut child, lines) = spawn_child(test_name, dir);
    let all: Vec<String> = lines.map_while(Result::ok).collect();
    let status = child.wait().expect("wait for the child test");
    (status, all)
}

fn expect_line(lines: &mut Lines<BufReader<ChildStdout>>, prefix: &str) -> String {
    lines
        .by_ref()
        .map_while(Result::ok)
        .find_map(|line| line.split_once(prefix).map(|(_, value)| value.to_owned()))
        .unwrap_or_else(|| panic!("the child never printed a line carrying {prefix}"))
}

fn field(lines: &[String], prefix: &str) -> String {
    lines
        .iter()
        .find_map(|line| line.split_once(prefix).map(|(_, value)| value.to_owned()))
        .unwrap_or_else(|| panic!("no line carrying {prefix} in {lines:?}"))
}

fn wait_removed(id: &str, bound: Duration) {
    let deadline = Instant::now() + bound;
    while let Some(state) = shared::container_status(id) {
        if shared::removal_issued(Some(&state)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "container {id} is still listed as {state} after {bound:?}: its watchdog did not \
             remove it"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + WAIT_BOUND;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// A token no earlier run of this process used, so each run builds and removes a
/// tag of its own.
fn unique_token() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        now_millis(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn write_state(dir: &Path, status: &str, error: &str, failed_at: u64) {
    let state = serde_json::json!({
        "nonce": "stale",
        "status": status,
        "name": "stale",
        "container_id": null,
        "port": null,
        "error": error,
        "failed_at": failed_at,
    });
    fs::write(dir.join("state.json"), state.to_string()).expect("write a stale state file");
}

fn read_state(dir: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(dir.join("state.json")).expect("read the state file"))
        .expect("parse the state file")
}

/// The labelled containers in `dir`, the query a fresh-failure check reads.
fn containers_in(dir: &Path) -> Vec<String> {
    shared::containers_in_dir(dir.to_str().expect("utf-8 directory"))
        .expect("list containers by directory")
}

/// Start `image` detached with `binds`, for a case that drives the lease
/// refusal directly.
fn run_detached(image: &str, binds: &[String], label: &str) -> String {
    let name = format!(
        "zeroship-shared-server-refuse-{label}-{}",
        std::process::id()
    );
    shared::run_detached(image, binds, &[], &name).expect("run a detached container")
}

fn read_scope() -> PathBuf {
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .expect("read the scope directory");
    if line.trim().is_empty() {
        panic!(
            "this test is spawned by a contract test in `{}` and must be given a scope \
             directory on standard input; it is not runnable on its own",
            std::module_path!()
        );
    }
    PathBuf::from(line.trim())
}

// --- child tests -----------------------------------------------------------------

/// Child side of a plain join: join and report what the process saw.
#[test]
#[ignore = "spawned by a contract test as a child process, with its scope on stdin"]
fn child_join_report() {
    let dir = read_scope();
    let scope = Scope::at(&dir, GRACE);
    let spec = test_spec("child-report");
    let lease = shared::join(&scope, &spec, |_| Ok(())).expect("join the shared server");
    println!("CONTAINER={}", lease.container_id);
    println!("NONCE={}", lease.nonce);
    println!("BOOTED={}", lease.booted);
}

/// Child side of the race: wait behind the parent's barrier, join, and journal
/// the boot the closure runs.
#[test]
#[ignore = "spawned by a contract test as a child process, with its scope on stdin"]
fn child_join_journal() {
    let dir = read_scope();
    let scope = Scope::at(&dir, GRACE);
    let spec = test_spec("child-journal");
    println!("WAITING");
    std::io::stdout().flush().expect("flush WAITING");
    wait_for(&dir.join("go"));
    let lease = shared::join(&scope, &spec, |boot| {
        let mut journal = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("journal"))
            .expect("open the boot journal");
        writeln!(journal, "{}", boot.nonce).expect("journal the boot");
        Ok(())
    })
    .expect("join the shared server");
    println!("CONTAINER={}", lease.container_id);
    println!("NONCE={}", lease.nonce);
    println!("BOOTED={}", lease.booted);
}

/// Child side of a boot killed mid-migration: report the container, then block
/// inside the boot closure so the parent can kill it before it writes ready.
#[test]
#[ignore = "spawned by a contract test as a child process, with its scope on stdin"]
fn child_join_block() {
    let dir = read_scope();
    let scope = Scope::at(&dir, GRACE);
    let spec = test_spec("child-block");
    let _ = shared::join(&scope, &spec, |boot| {
        println!("CONTAINER={}", boot.container_id);
        println!("MIGRATING");
        std::io::stdout().flush().expect("flush the boot report");
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

/// Child side of the signal contract: boot the server, report it, and hold the
/// lease until the parent signals this process.
#[test]
#[ignore = "spawned by a contract test as a child process, with its scope on stdin"]
fn child_join_hold() {
    let dir = read_scope();
    let lease = shared::join(
        &Scope::at(&dir, GRACE),
        &test_spec("child-hold"),
        |_| Ok(()),
    )
    .expect("boot");
    println!("CONTAINER={}", lease.container_id);
    std::io::stdout().flush().expect("flush the boot report");
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
#[should_panic(expected = "an idle grace under one second")]
fn a_sub_second_idle_grace_is_rejected() {
    let _ = Scope::at("irrelevant", Duration::from_millis(500));
}

#[test]
fn the_container_sees_the_lease() {
    ensure_image();
    let dir = scratch("visibility");
    let spec = test_spec("visibility");
    let lease = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("boot");
    assert!(lease.booted);

    // A file no process locks is the control: the probe must succeed on it, so a
    // failing probe on the lease is a statement about the lease and not about
    // `flock` being absent or the mount being invisible.
    fs::write(dir.join("control.lock"), b"").expect("write the control lock");
    let held = shared::exec_in_container(
        &lease.container_id,
        &[
            "flock",
            "-n",
            "-x",
            "/run/zeroship-testkit/session.lock",
            "true",
        ],
    )
    .expect("run flock in the container");
    assert!(
        !held.success(),
        "the container must contend with the host's lease"
    );

    let control = shared::exec_in_container(
        &lease.container_id,
        &[
            "flock",
            "-n",
            "-x",
            "/run/zeroship-testkit/control.lock",
            "true",
        ],
    )
    .expect("run flock in the container");
    assert!(
        control.success(),
        "the probe must succeed on an unheld file: {}",
        control.stderr_text()
    );

    let id = lease.container_id.clone();
    drop(lease);
    wait_removed(&id, REMOVAL_BOUND);
    cleanup(&dir);
}

#[test]
fn a_container_without_flock_is_refused() {
    ensure_image();
    let dir = scratch("refuse-no-flock");
    // A non-executable file over the image's `flock` makes `command -v flock`
    // fail while the container stays up, the condition the refusal names.
    let masked = dir.join("not-executable");
    fs::write(&masked, b"").expect("write the mask");
    let id = run_detached(
        &image(),
        &[format!("{}:/usr/bin/flock:ro", masked.display())],
        "no-flock",
    );
    let error = shared::refuse_a_blind_container(&id)
        .expect_err("a container without flock must be refused");
    assert!(error.contains("has no flock"), "{error}");
    wait_removed(&id, REMOVAL_BOUND);
    cleanup(&dir);
}

#[test]
fn a_container_that_cannot_see_the_lease_is_refused() {
    ensure_image();
    let id = run_detached(&image(), &[], "blind");
    let error = shared::refuse_a_blind_container(&id)
        .expect_err("a container that cannot see the lease must be refused");
    assert!(error.contains("cannot see"), "{error}");
    wait_removed(&id, REMOVAL_BOUND);
}

#[test]
fn a_container_that_can_take_the_lease_is_refused() {
    ensure_image();
    let dir = scratch("refuse-probe-holder");
    let spec = test_spec("refuse-probe-holder");
    let lease = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("boot");
    let server = lease.container_id.clone();

    // A second directory whose `session.lock` no host holds, mounted at the
    // container's lease path: the file is visible but does not contend with the
    // host's lock, so the probe succeeds.
    let other = scratch("refuse-probe-other");
    fs::write(other.join("session.lock"), b"").expect("write an unheld lease");
    let id = run_detached(
        &image(),
        &[format!("{}:{CONTAINER_DIR}", other.display())],
        "takes-lease",
    );
    let error = shared::refuse_a_blind_container(&id)
        .expect_err("a container that can take the lease must be refused");
    assert!(error.contains("took the lease"), "{error}");
    wait_removed(&id, REMOVAL_BOUND);

    drop(lease);
    wait_removed(&server, REMOVAL_BOUND);
    cleanup(&dir);
    cleanup(&other);
}

#[test]
fn racing_processes_share_one_server_and_boot_once() {
    ensure_image();
    let dir = scratch("racing");
    let mut children: Vec<(Child, Lines<BufReader<ChildStdout>>)> =
        (0..8).map(|_| spawn_child(CHILD_JOURNAL, &dir)).collect();

    for (_, lines) in &mut children {
        expect_line(lines, "WAITING");
    }
    fs::write(dir.join("go"), b"go").expect("release the barrier");

    let mut ids = Vec::new();
    let mut nonces = Vec::new();
    let mut booted = 0;
    for (mut child, lines) in children {
        let all: Vec<String> = lines.map_while(Result::ok).collect();
        let status = child.wait().expect("wait for a racing child");
        assert!(
            status.success(),
            "a racing child failed:\n{}",
            all.join("\n")
        );
        ids.push(field(&all, "CONTAINER="));
        nonces.push(field(&all, "NONCE="));
        if field(&all, "BOOTED=") == "true" {
            booted += 1;
        }
    }

    assert_eq!(booted, 1, "exactly one process must boot the server");
    assert!(
        ids.windows(2).all(|pair| pair[0] == pair[1]),
        "every process must join the one container: {ids:?}"
    );
    assert!(
        nonces.windows(2).all(|pair| pair[0] == pair[1]),
        "every process must name the one boot: {nonces:?}"
    );
    let journal = fs::read_to_string(dir.join("journal")).expect("read the boot journal");
    assert_eq!(
        journal.lines().count(),
        1,
        "the boot must run exactly once: {journal:?}"
    );

    let id = ids[0].clone();
    wait_removed(&id, REMOVAL_BOUND);
    cleanup(&dir);
}

#[test]
fn two_scopes_boot_two_containers() {
    ensure_image();
    let a = scratch("two-a");
    let b = scratch("two-b");
    let first =
        shared::join(&Scope::at(&a, GRACE), &test_spec("two-a"), |_| Ok(())).expect("boot a");
    let second =
        shared::join(&Scope::at(&b, GRACE), &test_spec("two-b"), |_| Ok(())).expect("boot b");
    assert!(first.booted && second.booted);
    assert_ne!(
        first.container_id, second.container_id,
        "different scopes must not share a container"
    );

    let (id_a, id_b) = (first.container_id.clone(), second.container_id.clone());
    drop(first);
    drop(second);
    wait_removed(&id_a, REMOVAL_BOUND);
    wait_removed(&id_b, REMOVAL_BOUND);
    cleanup(&a);
    cleanup(&b);
}

#[test]
fn the_private_server_is_removed_when_its_child_exits() {
    ensure_image();
    let dir = scratch("idle");
    let (status, lines) = run_child(CHILD_REPORT, &dir);
    assert!(status.success(), "the child failed:\n{}", lines.join("\n"));
    let id = field(&lines, "CONTAINER=");
    wait_removed(&id, REMOVAL_BOUND);
    cleanup(&dir);
}

#[test]
fn a_child_run_without_a_scope_on_stdin_fails_clearly() {
    ensure_image();
    let output = Command::new(std::env::current_exe().expect("locate this test binary"))
        .args([
            CHILD_REPORT,
            "--exact",
            "--nocapture",
            "--test-threads=1",
            "--ignored",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run the child test as an orphan");
    assert!(
        !output.status.success(),
        "an orphan child must fail: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must be given a scope directory on standard input"),
        "the orphan failure must name the missing stdin scope:\n{stderr}"
    );
}

#[test]
fn a_held_lease_keeps_the_server_alive_past_three_graces() {
    ensure_image();
    let dir = scratch("held");
    let lease =
        shared::join(&Scope::at(&dir, GRACE), &test_spec("held"), |_| Ok(())).expect("boot");
    let id = lease.container_id.clone();

    let deadline = Instant::now() + 3 * GRACE;
    while Instant::now() < deadline {
        assert!(
            shared::container_status(&id).is_some(),
            "a held lease must keep the server alive"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    drop(lease);
    wait_removed(&id, REMOVAL_BOUND);
    cleanup(&dir);
}

#[test]
fn a_sigterm_to_a_booting_process_leaves_the_server_for_other_leases() {
    ensure_image();
    let dir = scratch("sigterm-booter");
    let (mut child, mut lines) = spawn_child(CHILD_HOLD, &dir);
    let first = expect_line(&mut lines, "CONTAINER=");

    // The parent leases the same server while the child still holds its lease.
    let lease = shared::join(
        &Scope::at(&dir, GRACE),
        &test_spec("sigterm-booter"),
        |_| Ok(()),
    )
    .expect("join the server the child booted");
    assert!(!lease.booted, "the parent must join, not reboot");
    assert_eq!(lease.container_id, first);

    // SIGTERM the process that booted it. A container created through bollard is
    // not registered with testcontainers' process watchdog, so the signal must
    // not stop a server another process still leases.
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("signal the booter");
    assert!(status.success(), "kill -TERM failed");
    let _ = child.wait();

    assert_eq!(
        shared::container_status(&first).as_deref(),
        Some("running"),
        "the signalled booter must not stop the server another lease holds"
    );
    shared::psql(&first, "postgres", "SELECT 1").expect("the server still answers");

    drop(lease);
    wait_removed(&first, REMOVAL_BOUND);
    cleanup(&dir);
}

#[test]
fn a_join_takes_over_a_boot_killed_mid_migration() {
    ensure_image();
    let dir = scratch("takeover");
    let (mut child, mut lines) = spawn_child(CHILD_BLOCK, &dir);
    let first = expect_line(&mut lines, "CONTAINER=");
    expect_line(&mut lines, "MIGRATING");
    child.kill().expect("SIGKILL the booting child");
    child.wait().expect("reap the booting child");

    let lease = shared::join(&Scope::at(&dir, GRACE), &test_spec("takeover"), |_| Ok(()))
        .expect("take over");
    assert!(
        lease.booted,
        "the second joiner must boot after the killed one"
    );
    let second = lease.container_id.clone();
    assert_ne!(second, first, "the dead container must be replaced");
    wait_removed(&first, REMOVAL_BOUND);
    drop(lease);
    wait_removed(&second, REMOVAL_BOUND);
    cleanup(&dir);
}

#[test]
fn a_ready_state_naming_a_dead_container_reboots() {
    ensure_image();
    let dir = scratch("stale-ready");
    let spec = test_spec("stale-ready");
    let first = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("boot");
    let dead = first.container_id.clone();
    shared::remove_by_id(&dead);

    let second = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("reboot");
    assert!(
        second.booted,
        "a ready state naming a dead container must reboot"
    );
    let live = second.container_id.clone();
    assert_ne!(live, dead);
    drop(first);
    drop(second);
    wait_removed(&dead, REMOVAL_BOUND);
    wait_removed(&live, REMOVAL_BOUND);
    cleanup(&dir);
}

#[test]
fn a_boot_failure_is_recorded_and_its_container_removed() {
    ensure_image();
    let dir = scratch("boot-failure");
    let spec = test_spec("boot-failure");
    let error = shared::join(&Scope::at(&dir, GRACE), &spec, |_| {
        Err("intentional boot failure".to_owned())
    })
    .expect_err("a failing boot must fail");
    assert!(error.contains("intentional boot failure"), "{error}");

    let state = read_state(&dir);
    assert_eq!(state["status"], "failed", "{state}");
    assert_eq!(state["error"], "intentional boot failure", "{state}");
    assert!(
        containers_in(&dir).is_empty(),
        "a failed boot must remove its container"
    );

    // A second joiner inside the grace fails fast and does not boot again.
    let error = shared::join(&Scope::at(&dir, GRACE), &spec, |_| {
        panic!("a fresh failure must not be retried")
    })
    .expect_err("a fresh failure must fail fast");
    assert!(error.contains("intentional boot failure"), "{error}");
    assert!(
        containers_in(&dir).is_empty(),
        "a fresh failure must not start a container"
    );
    cleanup(&dir);
}

#[test]
fn a_panicking_boot_is_recorded_and_its_container_removed() {
    ensure_image();
    let dir = scratch("boot-panic");
    let spec = test_spec("boot-panic");
    let error = shared::join(&Scope::at(&dir, GRACE), &spec, |_| {
        panic!("intentional boot panic")
    })
    .expect_err("a panicking boot must fail");
    assert!(error.contains("intentional boot panic"), "{error}");

    let state = read_state(&dir);
    assert_eq!(state["status"], "failed", "{state}");
    assert!(
        containers_in(&dir).is_empty(),
        "a panicking boot must remove its container"
    );
    cleanup(&dir);
}

#[test]
fn a_live_server_that_does_not_answer_is_not_replaced() {
    ensure_image();
    let dir = scratch("unreachable");
    let spec = victim_spec("unreachable");
    let lease = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("boot");
    let id = lease.container_id.clone();

    // Drop the served database: the container stays up but cannot answer.
    shared::psql(&id, "postgres", "DROP DATABASE victim WITH (FORCE)")
        .expect("drop the served database");

    let error = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(()))
        .expect_err("a live server that does not answer must be reported");
    assert!(error.contains("not replacing a live server"), "{error}");
    assert_eq!(
        shared::container_status(&id).as_deref(),
        Some("running"),
        "the live server must not be replaced"
    );

    drop(lease);
    wait_removed(&id, REMOVAL_BOUND);
    cleanup(&dir);
}

#[test]
fn a_stopped_server_is_replaced() {
    ensure_image();
    let dir = scratch("stopped-control");
    let spec = test_spec("stopped-control");
    let first = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("boot");
    let stopped = first.container_id.clone();
    shared::stop_container(&stopped).expect("stop the server");

    let second = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("replace");
    assert!(second.booted, "a stopped server must be replaced");
    let live = second.container_id.clone();
    assert_ne!(live, stopped);
    drop(first);
    drop(second);
    wait_removed(&stopped, REMOVAL_BOUND);
    wait_removed(&live, REMOVAL_BOUND);
    cleanup(&dir);
}

#[test]
fn a_client_without_a_lease_does_not_hold_up_teardown() {
    ensure_image();
    let dir = scratch("stubborn-client");
    let lease = shared::join(
        &Scope::at(&dir, GRACE),
        &test_spec("stubborn-client"),
        |_| Ok(()),
    )
    .expect("boot");
    let id = lease.container_id.clone();

    // A backend that holds a connection without any lease. A smart shutdown
    // would wait out the whole sleep; the watchdog's fast shutdown must not.
    let stray_id = id.clone();
    let stray = std::thread::spawn(move || {
        let _ = shared::exec_in_container(
            &stray_id,
            &[
                "psql",
                "-U",
                "postgres",
                "-d",
                "postgres",
                "-c",
                "SELECT pg_sleep(600)",
            ],
        );
    });
    let deadline = Instant::now() + WAIT_BOUND;
    loop {
        let running = shared::exec_in_container(
            &id,
            &[
                "psql",
                "-U",
                "postgres",
                "-d",
                "postgres",
                "-tAc",
                "SELECT count(*) FROM pg_stat_activity WHERE query LIKE '%pg_sleep%'",
            ],
        )
        .ok()
        .is_some_and(|output| {
            output.success() && String::from_utf8_lossy(&output.stdout).trim() != "0"
        });
        if running {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the stray client never connected"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    drop(lease);
    wait_removed(&id, REMOVAL_BOUND);

    let _ = stray.join();
    cleanup(&dir);
}

#[test]
fn a_reader_does_not_extend_a_failed_state() {
    ensure_image();
    let dir = scratch("failed-not-extended");
    let failed_at = now_millis() - 1_000;
    write_state(&dir, "failed", "a recorded failure", failed_at);

    let error = shared::join(
        &Scope::at(&dir, GRACE),
        &test_spec("failed-not-extended"),
        |_| Ok(()),
    )
    .expect_err("a fresh failure must be reported to the reader");
    assert!(error.contains("a recorded failure"), "{error}");
    assert_eq!(
        read_state(&dir)["failed_at"].as_u64(),
        Some(failed_at),
        "a reader must not re-stamp failed_at, or the failure would never expire"
    );
    cleanup(&dir);
}

#[test]
fn a_failed_state_older_than_the_grace_is_retried() {
    ensure_image();
    let dir = scratch("stale-failed-old");
    write_state(
        &dir,
        "failed",
        "an old recorded failure",
        now_millis() - 10_000,
    );
    let lease = shared::join(
        &Scope::at(&dir, GRACE),
        &test_spec("stale-failed-old"),
        |_| Ok(()),
    )
    .expect("retry");
    assert!(
        lease.booted,
        "a failure older than the grace must be retried"
    );
    let id = lease.container_id.clone();
    drop(lease);
    wait_removed(&id, REMOVAL_BOUND);
    cleanup(&dir);
}

#[test]
fn a_failed_state_inside_the_grace_is_reported_without_booting() {
    ensure_image();
    let dir = scratch("stale-failed-young");
    // Positive control: the same query finds a container where one exists, so
    // the empty answer below is about this directory and not a blind query.
    let control = scratch("stale-failed-control");
    let held = shared::join(
        &Scope::at(&control, GRACE),
        &test_spec("stale-failed-control"),
        |_| Ok(()),
    )
    .expect("boot the control");
    assert!(
        !containers_in(&control).is_empty(),
        "the directory query must see a container that is there"
    );

    write_state(&dir, "failed", "a fresh recorded failure", now_millis());
    let error = shared::join(
        &Scope::at(&dir, GRACE),
        &test_spec("stale-failed-young"),
        |_| Ok(()),
    )
    .expect_err("a fresh failure must fail fast");
    assert!(error.contains("a fresh recorded failure"), "{error}");

    assert!(
        containers_in(&dir).is_empty(),
        "a fresh failure must not boot a container"
    );

    let control_id = held.container_id.clone();
    drop(held);
    wait_removed(&control_id, REMOVAL_BOUND);
    cleanup(&dir);
    cleanup(&control);
}

#[test]
fn the_watchdog_removes_only_state_naming_its_nonce() {
    ensure_image();
    let dir = scratch("watchdog-owner");
    let spec = test_spec("watchdog-owner");
    let lease = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("boot");
    let id = lease.container_id.clone();

    // A state naming another boot must survive the watchdog.
    let foreign = serde_json::json!({
        "nonce": "someone-else",
        "status": "ready",
        "name": "someone-else",
        "container_id": null,
        "port": null,
        "error": null,
        "failed_at": null,
    });
    fs::write(dir.join("state.json"), foreign.to_string()).expect("write a foreign state");
    drop(lease);
    wait_removed(&id, REMOVAL_BOUND);
    assert!(
        dir.join("state.json").exists(),
        "the watchdog must not remove a state naming another boot"
    );

    // A state naming its own boot is removed.
    let lease = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("reboot");
    let id = lease.container_id.clone();
    drop(lease);
    wait_removed(&id, REMOVAL_BOUND);
    assert!(
        !dir.join("state.json").exists(),
        "the watchdog must remove a state naming its own boot"
    );
    cleanup(&dir);
}

#[test]
fn a_boot_removes_a_created_leftover_of_its_name() {
    ensure_image();
    let dir = scratch("created-leftover");
    // A boot killed between the daemon's create and start leaves this: a
    // container under the scope's name that never ran, which `auto_remove`
    // never removes and no watchdog inside it ever will.
    let leftover = shared::create_unstarted(&image(), &shared::container_name(&dir))
        .expect("create the leftover");
    assert_eq!(
        shared::container_status(&leftover).as_deref(),
        Some("created"),
        "the leftover must be present and never started"
    );

    let lease = shared::join(
        &Scope::at(&dir, GRACE),
        &test_spec("created-leftover"),
        |_| Ok(()),
    )
    .expect("a boot over a created leftover of its name");
    assert!(lease.booted);
    assert_ne!(
        lease.container_id, leftover,
        "the boot runs a container of its own"
    );
    assert_eq!(
        shared::container_status(&leftover),
        None,
        "the boot must remove the leftover of its name"
    );

    let id = lease.container_id.clone();
    drop(lease);
    wait_removed(&id, REMOVAL_BOUND);
    cleanup(&dir);
}

#[test]
fn a_container_that_exits_while_it_boots_fails_the_boot_at_once() {
    ensure_image();
    let dir = scratch("exits-at-boot");
    // The server outlives the refusal checks and then exits without ever
    // printing the readiness marker; the watchdog follows it out and
    // `auto_remove` removes the container.
    let mut spec = test_spec("exits-at-boot");
    spec.args = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        "echo the server is starting; sleep 5; exit 3".to_owned(),
    ];
    let started = Instant::now();
    let error = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(()))
        .expect_err("a server that exits while it boots must fail the boot");
    let elapsed = started.elapsed();
    assert!(error.contains("exited while it booted"), "{error}");
    assert!(
        error.contains("the server is starting"),
        "the failure carries the server's last logs: {error}"
    );
    assert!(
        elapsed < Duration::from_secs(60),
        "the boot must fail once the container is gone, not run out its timeout: {elapsed:?}"
    );
    assert_eq!(read_state(&dir)["status"], "failed");
    assert!(
        containers_in(&dir).is_empty(),
        "a boot whose server exited leaves no container"
    );
    cleanup(&dir);
}

#[test]
fn an_exec_that_never_ends_is_stopped_at_its_bound() {
    ensure_image();
    let dir = scratch("endless-exec");
    let lease = shared::join(&Scope::at(&dir, GRACE), &test_spec("endless-exec"), |_| {
        Ok(())
    })
    .expect("boot");
    let id = lease.container_id.clone();

    // The control: a command that ends inside the bound returns its output.
    let quick = shared::exec_bounded(&id, &["echo", "answered"], Duration::from_secs(5))
        .expect("a command inside its bound returns");
    assert_eq!(String::from_utf8_lossy(&quick.stdout).trim(), "answered");

    // A sentinel argument no other process in the container carries.
    let started = Instant::now();
    let error = shared::exec_bounded(&id, &["sleep", "30.0417"], Duration::from_secs(2))
        .expect_err("a command past its bound must fail");
    let elapsed = started.elapsed();
    assert!(error.contains("timed out"), "{error}");
    assert!(
        elapsed < Duration::from_secs(15),
        "the bound must cover the whole exchange, not only the attach: {elapsed:?}"
    );

    // The stopped command does not linger in the server: no process carries the
    // sentinel. The pattern's brackets keep the probe from matching itself.
    let lingering = shared::exec_in_container(
        &id,
        &[
            "sh",
            "-c",
            "cat /proc/[0-9]*/cmdline 2>/dev/null | tr '\\0' ' ' | grep -c '30[.]0417' || true",
        ],
    )
    .expect("list the container's processes");
    assert_eq!(
        String::from_utf8_lossy(&lingering.stdout).trim(),
        "0",
        "the stopped command must not keep running in the container"
    );

    drop(lease);
    wait_removed(&id, REMOVAL_BOUND);
    cleanup(&dir);
}

#[test]
fn a_daemon_error_is_not_read_as_a_removed_container() {
    // The control: a name the daemon does not know reads as absent.
    let absent = format!("zeroship-testkit-absent-{}", std::process::id());
    assert_eq!(shared::container_status(&absent), None);

    // A path the daemon answers with a redirect rather than a container's state
    // or its "No such container" is a failure to ask, not an absence.
    let outcome = std::panic::catch_unwind(|| {
        shared::container_status("../../zeroship-testkit-not-a-container")
    });
    let panic = outcome.expect_err("a daemon error must not read as a removed container");
    let message = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
        .unwrap_or_default();
    assert!(
        message.contains("could not say whether"),
        "the failure names what the daemon could not answer: {message}"
    );
}

/// The removal waiters succeed on the daemon's own report that the removal has
/// been issued, not on the deletion that follows it: `removing` means the
/// daemon has accepted the removal and stopped the container, and `None` means
/// the deletion finished. A state the daemon was never asked to remove - or one
/// whose removal it has not issued - stays outside that set, so a waiter fails
/// it rather than reading it as removal.
#[test]
fn issued_removal_is_the_removing_state_or_gone() {
    for issued in [None, Some("removing")] {
        assert!(
            shared::removal_issued(issued),
            "{issued:?} is a removal the daemon has issued"
        );
    }
    for present in [Some("running"), Some("created"), Some("exited"), Some("dead")] {
        assert!(
            !shared::removal_issued(present),
            "{present:?} is a state whose removal the daemon has not issued"
        );
    }
}

/// Child side of the lifetime instrument's measurements: join a throwaway server
/// at the scope the instrument hands this process.
#[test]
#[ignore = "spawned by a lifetime measurement as a child process, with its scope on stdin"]
fn child_lifetime_join() {
    let lease = shared::join(
        &shared::lifetime::child_scope(),
        &test_spec("lifetime"),
        |_| Ok(()),
    )
    .expect("join the throwaway server");
    shared::lifetime::report_container(&lease.container_id);
}

/// Child side of the instrument's rejection control: start a container in the
/// scope that no watchdog owns, the shape of a fixture that holds a container
/// nothing removes.
#[test]
#[ignore = "spawned by a lifetime measurement as a child process, with its scope on stdin"]
fn child_lifetime_leak() {
    let scope_dir = read_scope();
    let dir = scope_dir
        .to_str()
        .expect("a UTF-8 scope directory")
        .to_owned();
    let name = format!("zeroship-shared-server-leak-{}", std::process::id());
    let id = shared::run_detached(&image(), &[], &[(shared::DIR_LABEL, &dir)], &name)
        .expect("start the unowned container");
    shared::lifetime::report_container(&id);
}

#[test]
fn the_lifetime_instrument_sees_a_server_removed_after_its_child_exits() {
    ensure_image();
    shared::lifetime::assert_removed_after_the_child_exits(CHILD_LIFETIME);
}

#[test]
fn the_lifetime_instrument_sees_a_server_removed_after_its_child_is_killed() {
    ensure_image();
    shared::lifetime::assert_removed_after_a_kill_during_startup(CHILD_LIFETIME);
}

#[test]
fn the_lifetime_instrument_fails_a_container_that_outlives_its_child() {
    ensure_image();
    let outcome = std::panic::catch_unwind(|| {
        shared::lifetime::assert_removed_after_the_child_exits(CHILD_LIFETIME_LEAK);
    });
    let panic = outcome.expect_err("a container nothing removes must fail the measurement");
    let message = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
        .unwrap_or_default();
    assert!(message.contains("is still listed"), "{message}");
    // The leaked container is this test's own: its child started it in a scope
    // this test made, so it is removed here rather than left to its sleep.
    let id = message
        .split_whitespace()
        .nth(1)
        .expect("the failure names the container")
        .to_owned();
    shared::remove_by_id(&id);
    wait_removed(&id, REMOVAL_BOUND);
}

/// Eight threads that want one fresh tag build it once: the cross-process lock
/// lets one build, and every waiter that found the tag absent re-checks under
/// the lock and returns the one reference.
#[test]
fn one_image_reference_is_built_once_across_threads() {
    ensure_image();
    let token = unique_token();
    // A stage alias carrying the token makes the recipe new each run, so the
    // tag is absent when the race starts and the lock has a build to serialize.
    let base = format!("{POSTGRES_IMAGE} AS s{token}");
    let reference = format!("{}:{}", shared::image::NAME, shared::image::tag(&base));
    assert!(
        !shared::image_exists(&reference),
        "the unique tag must be absent before the race: {reference}"
    );

    let barrier = Arc::new(Barrier::new(8));
    let results: Vec<Result<String, String>> = std::thread::scope(|scope| {
        // Every thread must be spawned before any is joined, or the barrier
        // never sees all eight and the test deadlocks.
        let handles: [_; 8] = std::array::from_fn(|_| {
            let barrier = Arc::clone(&barrier);
            let base = base.clone();
            scope.spawn(move || {
                barrier.wait();
                shared::image::with_watchdog(&base)
            })
        });
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a builder thread"))
            .collect()
    });

    let builds = shared::image::built_images()
        .into_iter()
        .filter(|entry| *entry == reference)
        .count();
    // Cleanup runs before the count assertion so a failure that panics on the
    // count still removes the tag the race built.
    let cleanup = shared::remove_image(&reference);
    assert_eq!(
        builds, 1,
        "exactly one thread must build the reference: {reference} {results:?}"
    );
    for result in &results {
        assert_eq!(
            result.as_ref().map(String::as_str),
            Ok(reference.as_str()),
            "every builder must return the one reference"
        );
    }
    cleanup.expect("remove the throwaway image");
    assert!(
        !shared::image_exists(&reference),
        "the throwaway image must be gone"
    );
}

/// A build that fails releases the lock: the next caller reaches the build
/// again rather than waiting on a lock the failure left held.
#[test]
fn a_failed_build_releases_the_build_lock() {
    let token = unique_token();
    let name = "zeroship-shared-server-build-failure";
    let dockerfile =
        format!("FROM {POSTGRES_IMAGE}\nLABEL zeroship.test.build=\"{token}\"\nRUN exit 1\n");

    let first = shared::image::build(name, &dockerfile, &[])
        .expect_err("a Dockerfile that fails must fail the build");
    assert!(first.contains("could not build"), "{first}");

    let second = shared::image::build(name, &dockerfile, &[])
        .expect_err("the second build must fail, not deadlock on a held lock");
    assert!(
        second.contains("could not build"),
        "the second caller must reach the build, not the lock: {second}"
    );
    assert!(
        !second.contains("image build lock"),
        "a failed build must release the build lock: {second}"
    );
}
