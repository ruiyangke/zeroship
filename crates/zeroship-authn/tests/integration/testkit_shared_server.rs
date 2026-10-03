//! The shared-server contract: one migrated server for every test process of a
//! worktree, elected by `flock`, leased for the life of a process and removed by
//! the container's watchdog once no lease is held.
//!
//! The child tests are spawned from this binary (the `lifetime.rs` pattern) with
//! the scope directory on standard input, never in the environment. They are
//! `#[ignore]`d so a normal run does not boot a throwaway server for them.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Lines, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use compio_postgres::{Client, NoTls};

use zeroship_testkit::fingerprint;
use zeroship_testkit::postgres::server_inputs;
use zeroship_testkit::shared::{self, Scope};
use zeroship_testkit::DockerCli;

/// The idle grace a throwaway scope is started with.
const GRACE: Duration = Duration::from_secs(2);

/// How long a container may take to disappear after its last lease is released.
const REMOVAL_BOUND: Duration = Duration::from_secs(42);

/// How long a child gets to reach a barrier.
const WAIT_BOUND: Duration = Duration::from_secs(120);

/// The mount point the lease directory is bound to in the container.
const CONTAINER_DIR: &str = "/run/zeroship-testkit";

const CHILD_REPORT: &str = "integration::testkit_shared_server::child_join_report";
const CHILD_JOURNAL: &str = "integration::testkit_shared_server::child_join_journal";
const CHILD_BLOCK: &str = "integration::testkit_shared_server::child_join_block";

/// A throwaway lease directory under the worktree's `target`, canonical so its
/// path is the one the container labels carry.
fn scratch(name: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("authn lives under crates/");
    let dir = root
        .join("target/zeroship-testkit-tests")
        .join(format!("{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create a scratch lease directory");
    fs::canonicalize(&dir).expect("canonicalize the scratch directory")
}

fn cleanup(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

/// The image must exist before any process runs a container from it.
fn ensure_image() {
    let _ = zeroship_testkit::postgres::build().expect("build the shared PostgreSQL image");
}

/// A throwaway server: the shared image, a database that always exists, and no
/// migration.
fn test_spec(inputs: &str) -> shared::Spec {
    shared::Spec {
        inputs: inputs.to_owned(),
        image: zeroship_testkit::postgres::image_ref(),
        database: "postgres".to_owned(),
        port: 5432,
        environment: vec![
            ("POSTGRES_PASSWORD".to_owned(), "fixture".to_owned()),
            ("POSTGRES_DB".to_owned(), "postgres".to_owned()),
        ],
        watchdog: "/usr/local/bin/zeroship-watchdog".to_owned(),
        postgres_args: vec!["-c".to_owned(), "max_connections=100".to_owned()],
    }
}

/// A throwaway server whose database a case may drop to make the live server
/// stop answering.
fn victim_spec(inputs: &str) -> shared::Spec {
    let mut spec = test_spec(inputs);
    spec.database = "victim".to_owned();
    spec.environment = vec![
        ("POSTGRES_PASSWORD".to_owned(), "fixture".to_owned()),
        ("POSTGRES_DB".to_owned(), "victim".to_owned()),
    ];
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
    while let Some(state) = zeroship_testkit::lifetime::container_status(id) {
        assert!(
            Instant::now() < deadline,
            "container {id} is still listed as {state} after {bound:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + WAIT_BOUND;
    while !path.exists() {
        assert!(Instant::now() < deadline, "{} never appeared", path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
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
    DockerCli::system()
        .labelled(shared::DIR_LABEL, dir.to_str().expect("utf-8 directory"))
        .expect("list containers by directory")
}

/// Start `image` detached with `mounts`, for a case that drives the lease
/// refusal directly.
fn run_detached(image: &str, mounts: &[String], label: &str) -> String {
    let name = format!(
        "zeroship-testkit-refuse-{label}-{}",
        std::process::id()
    );
    let mut command = Command::new("docker");
    command
        .args(["run", "--detach", "--rm", "--name", &name])
        .stdin(Stdio::null());
    for mount in mounts {
        command.arg("--mount").arg(mount);
    }
    let output = command
        .arg(image)
        .args(["sleep", "600"])
        .output()
        .expect("run a detached container");
    assert!(
        output.status.success(),
        "docker run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
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

// --- identity ---------------------------------------------------------------------

#[test]
fn the_migration_fingerprint_is_keyed_to_the_corpus_not_the_path() {
    let migrations = [("20260101_one.ts", "one"), ("20260202_two.ts", "two")];
    let a = scratch("fingerprint-a");
    let b = scratch("fingerprint-b");
    write_migrations(&a, &migrations);
    write_migrations(&b, &migrations);

    let from_a = fingerprint::of_dir(&a).expect("a corpus");
    assert_eq!(from_a, fingerprint::of_dir(&b).expect("the same corpus"));
    assert_eq!(from_a.len(), 12);

    let edited = scratch("fingerprint-edited");
    write_migrations(
        &edited,
        &[("20260101_one.ts", "one, but different"), ("20260202_two.ts", "two")],
    );
    assert_ne!(from_a, fingerprint::of_dir(&edited).expect("an edited corpus"));

    cleanup(&a);
    cleanup(&b);
    cleanup(&edited);
}

fn write_migrations(dir: &Path, files: &[(&str, &str)]) {
    let migrations = dir.join(fingerprint::MIGRATIONS_DIR);
    fs::create_dir_all(&migrations).expect("create the migration directory");
    for (name, body) in files {
        fs::write(migrations.join(name), body).expect("write a migration");
    }
}

/// A root with every input `server_inputs` hashes.
fn input_root(name: &str) -> PathBuf {
    let root = scratch(name);
    write_migrations(&root, &[("20260101_one.ts", "one")]);
    fs::create_dir_all(root.join("packages/zero-migrate-cli/dist")).unwrap();
    fs::write(root.join("packages/zero-migrate-cli/dist/cli-bin.js"), "cli").unwrap();
    fs::create_dir_all(root.join("packages/zero-migrate/dist")).unwrap();
    fs::write(root.join("packages/zero-migrate/dist/index.js"), "migrate").unwrap();
    fs::create_dir_all(root.join("crates/zeroship-migrate-node")).unwrap();
    fs::write(root.join("crates/zeroship-migrate-node/addon.node"), "addon").unwrap();
    fs::create_dir_all(root.join("policies")).unwrap();
    fs::write(root.join("policies/platform-table-owners.json"), "{}").unwrap();
    fs::write(root.join("policies/platform.policy.toml"), "policy").unwrap();
    root
}

fn test_environment() -> Vec<(String, String)> {
    vec![
        ("POSTGRES_PASSWORD".to_owned(), "fixture".to_owned()),
        ("POSTGRES_DB".to_owned(), "db".to_owned()),
    ]
}

fn test_args() -> Vec<String> {
    ["-c", "fsync=off"].iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn changing_any_server_input_moves_the_identity() {
    let root = input_root("inputs");
    let identity = |root: &Path| {
        server_inputs(root, "image:tag", &test_environment(), &test_args()).expect("an identity")
    };
    let base = identity(&root);

    let migration = root.join(fingerprint::MIGRATIONS_DIR).join("20260101_one.ts");
    fs::write(&migration, "changed").unwrap();
    assert_ne!(base, identity(&root), "a migration edit must move the identity");
    fs::write(&migration, "one").unwrap();
    assert_eq!(base, identity(&root), "restoring the migration must restore the identity");

    let cases: [(&str, &[u8]); 4] = [
        ("packages/zero-migrate-cli/dist/cli-bin.js", b"cli changed"),
        ("packages/zero-migrate/dist/index.js", b"migrate changed"),
        ("crates/zeroship-migrate-node/addon.node", b"addon changed"),
        ("policies/platform-table-owners.json", b"{\"changed\":true}"),
    ];
    for (relative, changed) in cases {
        let path = root.join(relative);
        let original = fs::read(&path).unwrap();
        fs::write(&path, changed).unwrap();
        assert_ne!(
            base,
            identity(&root),
            "changing {relative} must move the identity"
        );
        fs::write(&path, original).unwrap();
        assert_eq!(
            base,
            identity(&root),
            "restoring {relative} must restore the identity"
        );
    }

    let policy = root.join("policies/platform.policy.toml");
    fs::write(&policy, "changed policy").unwrap();
    assert_ne!(base, identity(&root), "a policy edit must move the identity");
    fs::write(&policy, "policy").unwrap();
    assert_eq!(base, identity(&root), "restoring the policy must restore the identity");

    // The image, the environment and the `postgres` arguments are hashed too.
    assert_ne!(
        base,
        server_inputs(&root, "image:other", &test_environment(), &test_args()).unwrap(),
        "a different image must move the identity"
    );
    let mut environment = test_environment();
    environment[1].1 = "other".to_owned();
    assert_ne!(
        base,
        server_inputs(&root, "image:tag", &environment, &test_args()).unwrap(),
        "a different environment must move the identity"
    );
    let mut arguments = test_args();
    arguments[1] = "synchronous_commit=off".to_owned();
    assert_ne!(
        base,
        server_inputs(&root, "image:tag", &test_environment(), &arguments).unwrap(),
        "different postgres arguments must move the identity"
    );

    cleanup(&root);
}

#[test]
fn a_missing_compiled_input_names_the_build_step() {
    let root = scratch("inputs-missing");
    write_migrations(&root, &[("20260101_one.ts", "one")]);
    fs::create_dir_all(root.join("policies")).unwrap();
    fs::write(root.join("policies/platform-table-owners.json"), "{}").unwrap();
    fs::write(root.join("policies/platform.policy.toml"), "policy").unwrap();

    let error = server_inputs(&root, "image:tag", &[], &[])
        .expect_err("a missing compiled input must fail");
    assert!(error.contains("pnpm build"), "{error}");
    cleanup(&root);
}

#[test]
#[should_panic(expected = "an idle grace under one second")]
fn a_sub_second_idle_grace_is_rejected() {
    let _ = Scope::at("irrelevant", Duration::from_millis(500));
}

// --- the lease --------------------------------------------------------------------

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
    let docker = DockerCli::system();
    let held = docker
        .command()
        .args([
            "exec",
            &lease.container_id,
            "flock",
            "-n",
            "-x",
            "/run/zeroship-testkit/session.lock",
            "true",
        ])
        .output()
        .expect("run flock in the container");
    assert!(
        !held.status.success(),
        "the container must contend with the host's lease"
    );

    let control = docker
        .command()
        .args([
            "exec",
            &lease.container_id,
            "flock",
            "-n",
            "-x",
            "/run/zeroship-testkit/control.lock",
            "true",
        ])
        .output()
        .expect("run flock in the container");
    assert!(
        control.status.success(),
        "the probe must succeed on an unheld file: {}",
        String::from_utf8_lossy(&control.stderr)
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
        &zeroship_testkit::postgres::image_ref(),
        &[format!(
            "type=bind,src={},dst=/usr/bin/flock,readonly",
            masked.display()
        )],
        "no-flock",
    );
    let error = shared::refuse_a_blind_container(&DockerCli::system(), &id)
        .expect_err("a container without flock must be refused");
    assert!(error.contains("has no flock"), "{error}");
    wait_removed(&id, REMOVAL_BOUND);
    cleanup(&dir);
}

#[test]
fn a_container_that_cannot_see_the_lease_is_refused() {
    ensure_image();
    let id = run_detached(
        &zeroship_testkit::postgres::image_ref(),
        &[],
        "blind",
    );
    let error = shared::refuse_a_blind_container(&DockerCli::system(), &id)
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
        &zeroship_testkit::postgres::image_ref(),
        &[format!("type=bind,src={},dst={CONTAINER_DIR}", other.display())],
        "takes-lease",
    );
    let error = shared::refuse_a_blind_container(&DockerCli::system(), &id)
        .expect_err("a container that can take the lease must be refused");
    assert!(error.contains("took the lease"), "{error}");
    wait_removed(&id, REMOVAL_BOUND);

    drop(lease);
    wait_removed(&server, REMOVAL_BOUND);
    cleanup(&dir);
    cleanup(&other);
}

// --- joining, racing and lifetime -------------------------------------------------

#[test]
fn racing_processes_share_one_server_and_boot_once() {
    ensure_image();
    let dir = scratch("racing");
    let mut children: Vec<(Child, Lines<BufReader<ChildStdout>>)> = (0..8)
        .map(|_| spawn_child(CHILD_JOURNAL, &dir))
        .collect();

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
        assert!(status.success(), "a racing child failed:\n{}", all.join("\n"));
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
    let first = shared::join(&Scope::at(&a, GRACE), &test_spec("two-a"), |_| Ok(())).expect("boot a");
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
    let lease = shared::join(&Scope::at(&dir, GRACE), &test_spec("held"), |_| Ok(())).expect("boot");
    let id = lease.container_id.clone();

    let deadline = Instant::now() + 3 * GRACE;
    while Instant::now() < deadline {
        assert!(
            zeroship_testkit::lifetime::container_status(&id).is_some(),
            "a held lease must keep the server alive"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    drop(lease);
    wait_removed(&id, REMOVAL_BOUND);
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

    let lease =
        shared::join(&Scope::at(&dir, GRACE), &test_spec("takeover"), |_| Ok(())).expect("take over");
    assert!(lease.booted, "the second joiner must boot after the killed one");
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
    shared::remove_by_id(&DockerCli::system(), &dead);

    let second = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("reboot");
    assert!(second.booted, "a ready state naming a dead container must reboot");
    let live = second.container_id.clone();
    assert_ne!(live, dead);
    drop(first);
    drop(second);
    wait_removed(&dead, REMOVAL_BOUND);
    wait_removed(&live, REMOVAL_BOUND);
    cleanup(&dir);
}

// --- failure ----------------------------------------------------------------------

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
    shared::psql(
        &DockerCli::system(),
        &id,
        "postgres",
        "DROP DATABASE victim WITH (FORCE)",
    )
    .expect("drop the served database");

    let error = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(()))
        .expect_err("a live server that does not answer must be reported");
    assert!(error.contains("not replacing a live server"), "{error}");
    assert_eq!(
        zeroship_testkit::lifetime::container_status(&id).as_deref(),
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
    let status = DockerCli::system()
        .command()
        .args(["stop", &stopped])
        .output()
        .expect("stop the server");
    assert!(status.status.success(), "docker stop failed");

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
    let lease =
        shared::join(&Scope::at(&dir, GRACE), &test_spec("stubborn-client"), |_| Ok(())).expect("boot");
    let id = lease.container_id.clone();

    // A backend that holds a connection without any lease. A smart shutdown
    // would wait out the whole sleep; the watchdog's fast shutdown must not.
    let mut client = Command::new("docker")
        .args([
            "exec",
            &id,
            "psql",
            "-U",
            "postgres",
            "-d",
            "postgres",
            "-c",
            "SELECT pg_sleep(600)",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start a stray client");
    let deadline = Instant::now() + WAIT_BOUND;
    loop {
        let running = Command::new("docker")
            .args([
                "exec",
                &id,
                "psql",
                "-U",
                "postgres",
                "-d",
                "postgres",
                "-tAc",
                "SELECT count(*) FROM pg_stat_activity WHERE query LIKE '%pg_sleep%'",
            ])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .is_some_and(|count| count != "0");
        if running {
            break;
        }
        assert!(Instant::now() < deadline, "the stray client never connected");
        std::thread::sleep(Duration::from_millis(50));
    }

    drop(lease);
    wait_removed(&id, REMOVAL_BOUND);

    let _ = client.kill();
    let _ = client.wait();
    cleanup(&dir);
}

#[test]
fn a_reader_does_not_extend_a_failed_state() {
    ensure_image();
    let dir = scratch("failed-not-extended");
    let failed_at = now_millis() - 1_000;
    write_state(&dir, "failed", "a recorded failure", failed_at);

    let error = shared::join(&Scope::at(&dir, GRACE), &test_spec("failed-not-extended"), |_| Ok(()))
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
    write_state(&dir, "failed", "an old recorded failure", now_millis() - 10_000);
    let lease =
        shared::join(&Scope::at(&dir, GRACE), &test_spec("stale-failed-old"), |_| Ok(())).expect("retry");
    assert!(lease.booted, "a failure older than the grace must be retried");
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
    let held = shared::join(&Scope::at(&control, GRACE), &test_spec("stale-failed-control"), |_| Ok(()))
        .expect("boot the control");
    assert!(
        !containers_in(&control).is_empty(),
        "the directory query must see a container that is there"
    );

    write_state(&dir, "failed", "a fresh recorded failure", now_millis());
    let error = shared::join(&Scope::at(&dir, GRACE), &test_spec("stale-failed-young"), |_| Ok(()))
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

// --- fresh database --------------------------------------------------------------

async fn connect(
    url: &str,
) -> (
    Client,
    compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>,
) {
    let config: compio_postgres::Config = url.parse().expect("fixture database URL");
    let (client, connection) = config.connect(NoTls).await.expect("connect fixture database");
    (
        client,
        compio::runtime::spawn(async move { connection.run().await }),
    )
}

fn database_name(fresh: &zeroship_testkit::postgres::FreshDatabase) -> String {
    fresh.admin_url().path().trim_start_matches('/').to_owned()
}

fn docker_output(arguments: &[&str]) -> String {
    let output = Command::new("docker")
        .args(arguments)
        .output()
        .expect("run the docker CLI");
    assert!(
        output.status.success(),
        "docker {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn schema_dump(container_id: &str, database: &str) -> String {
    // `pg_dump` brackets its output with `\restrict`/`\unrestrict` carrying a
    // random key, so two dumps of identical catalogs differ in those lines.
    docker_output(&[
        "exec",
        container_id,
        "pg_dump",
        "-U",
        "postgres",
        "--schema-only",
        database,
    ])
    .lines()
    .filter(|line| !line.starts_with('\\'))
    .collect::<Vec<_>>()
    .join("\n")
}

#[compio::test]
async fn a_fresh_database_clones_the_migrated_template() {
    let platform = zeroship_testkit::postgres::platform();
    let first = platform.fresh_database();
    let second = platform.fresh_database();

    let first_dump = schema_dump(platform.container_id(), &database_name(&first));
    let second_dump = schema_dump(platform.container_id(), &database_name(&second));
    assert_eq!(
        first_dump, second_dump,
        "two clones of the template must have the same catalog"
    );
    assert!(
        first_dump.contains("CREATE TABLE zeroship.plans"),
        "the clone must carry the migrated platform schema"
    );
}

#[compio::test]
async fn a_fresh_database_isolates_rows() {
    let platform = zeroship_testkit::postgres::platform();
    let first = platform.fresh_database();
    let second = platform.fresh_database();

    let (writer, writer_driver) = connect(first.admin_url().as_str()).await;
    writer
        .batch_execute(
            "INSERT INTO zeroship.plans (id, name, runtime_limits_json) \
             VALUES ('fresh_only', 'Fresh Only', '{}')",
        )
        .await
        .expect("insert into the clone");

    let (reader, reader_driver) = connect(second.admin_url().as_str()).await;
    let seen: i64 = reader
        .query_one(
            "SELECT count(*) FROM zeroship.plans WHERE id = 'fresh_only'",
            &[],
        )
        .await
        .expect("count in the other clone")
        .get(0);
    assert_eq!(seen, 0, "a row in one clone must not appear in another");

    drop(writer);
    drop(reader);
    writer_driver.await.expect("writer driver").expect("writer connection");
    reader_driver.await.expect("reader driver").expect("reader connection");
}

#[compio::test]
async fn a_fresh_database_isolates_advisory_locks() {
    let platform = zeroship_testkit::postgres::platform();
    let first = platform.fresh_database();
    let second = platform.fresh_database();

    let (holder, holder_driver) = connect(first.admin_url().as_str()).await;
    let taken: bool = holder
        .query_one("SELECT pg_try_advisory_lock(4242)", &[])
        .await
        .expect("take the lock in the first clone")
        .get(0);
    assert!(taken, "the first clone must take its own advisory lock");

    let (other, other_driver) = connect(second.admin_url().as_str()).await;
    let also_taken: bool = other
        .query_one("SELECT pg_try_advisory_lock(4242)", &[])
        .await
        .expect("take the lock in the second clone")
        .get(0);
    assert!(
        also_taken,
        "an advisory lock in one clone must not block another database"
    );

    drop(holder);
    drop(other);
    holder_driver.await.expect("holder driver").expect("holder connection");
    other_driver.await.expect("other driver").expect("other connection");
}
