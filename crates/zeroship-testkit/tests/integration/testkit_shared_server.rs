//! The servers this testkit runs on the shared-server protocol: the platform
//! and bare `PostgreSQL` servers, the Redpanda broker, the `MySQL` server and the
//! Redis servers, each shared by every test process of a worktree. The protocol
//! itself - election, leases, the watchdog, failure states - is contracted in
//! `zeroship-shared-server`'s own tests.
//!
//! The child tests are spawned from this binary as separate processes, with the
//! scope directory on standard input, never in the environment. They are
//! `#[ignore]`d so a normal run does not boot a throwaway server for them.

use std::fs;
use std::io::{BufRead, BufReader, Lines, Write};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use compio_postgres::{Client, NoTls};
use futures::FutureExt;

use zeroship_testkit::fingerprint;
use zeroship_testkit::postgres::server_inputs;
use zeroship_shared_server::{self as shared, Scope};

/// The idle grace a throwaway scope is started with.
const GRACE: Duration = Duration::from_secs(2);

/// How long a container may take to disappear after its last lease is released.
const REMOVAL_BOUND: Duration = Duration::from_secs(42);

const CHILD_BARE: &str = "integration::testkit_shared_server::child_bare_server_report";
const CHILD_REDPANDA: &str = "integration::testkit_shared_server::child_join_redpanda_report";
const CHILD_MYSQL: &str = "integration::testkit_shared_server::child_join_mysql_report";
const CHILD_REDIS: &str = "integration::testkit_shared_server::child_join_redis_report";
const CHILD_DRAGONFLY_CLUSTER: &str =
    "integration::testkit_shared_server::child_join_dragonfly_cluster_report";

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
    let _ = zeroship_testkit::postgres::image().expect("build the shared PostgreSQL image");
}

/// The readiness of a throwaway PostgreSQL server for `database`.
fn readiness(database: &str) -> shared::Readiness {
    let list = |values: &[&str]| values.iter().map(|value| (*value).to_owned()).collect();
    shared::Readiness {
        log_marker: "PostgreSQL init process complete".to_owned(),
        probe: list(&["pg_isready", "-U", "postgres", "-h", "127.0.0.1", "-p", "5432"]),
        answer: list(&["psql", "-U", "postgres", "-d", database, "-tAc", "SELECT 1"]),
    }
}

/// A throwaway server: the shared image, a database that always exists, and no
/// migration.
fn test_spec(inputs: &str) -> shared::Spec {
    shared::Spec {
        inputs: inputs.to_owned(),
        image: zeroship_testkit::postgres::image().expect("the shared PostgreSQL image"),
        environment: vec![
            ("POSTGRES_PASSWORD".to_owned(), "fixture".to_owned()),
            ("POSTGRES_DB".to_owned(), "postgres".to_owned()),
        ],
        ports: vec![shared::Port {
            container: 5432,
            host: shared::HostPort::Assigned,
        }],
        watchdog: "/usr/local/bin/zeroship-watchdog".to_owned(),
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

/// Child side of the bare-server contract: start the fixture the data crates
/// use and report the container it joined.
#[test]
#[ignore = "spawned by a contract test as a child process"]
fn child_bare_server_report() {
    let postgres = zeroship_testkit::postgres::server::Postgres::start();
    println!("CONTAINER={}", postgres.container_id());
}

/// Child side of the broker contract: join a throwaway Redpanda broker and
/// report what this process saw.
#[test]
#[ignore = "spawned by a contract test as a child process, with its scope on stdin"]
fn child_join_redpanda_report() {
    let dir = read_scope();
    let spec = zeroship_testkit::redpanda::spec().expect("the Redpanda broker recipe");
    let lease = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("join the broker");
    println!("CONTAINER={}", lease.container_id);
    println!("NONCE={}", lease.nonce);
    println!("BOOTED={}", lease.booted);
}

/// Child side of the `MySQL` contract: join a throwaway MySQL server and report
/// what this process saw.
#[test]
#[ignore = "spawned by a contract test as a child process, with its scope on stdin"]
fn child_join_mysql_report() {
    let dir = read_scope();
    let spec = zeroship_testkit::mysql::spec().expect("the MySQL server recipe");
    let lease = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("join the server");
    println!("CONTAINER={}", lease.container_id);
    println!("NONCE={}", lease.nonce);
    println!("BOOTED={}", lease.booted);
}

/// Child side of the Redis contract: join a throwaway standalone Redis server
/// and report what this process saw.
#[test]
#[ignore = "spawned by a contract test as a child process, with its scope on stdin"]
fn child_join_redis_report() {
    let dir = read_scope();
    let spec = zeroship_testkit::redis::standalone_spec().expect("the Redis server recipe");
    let lease = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("join the server");
    println!("CONTAINER={}", lease.container_id);
    println!("NONCE={}", lease.nonce);
    println!("BOOTED={}", lease.booted);
}

/// Child side of the Dragonfly cluster contract: join a throwaway cluster
/// container and report what this process saw. The boot closure is a no-op:
/// this contract checks container-identity sharing, not slot configuration.
#[test]
#[ignore = "spawned by a contract test as a child process, with its scope on stdin"]
fn child_join_dragonfly_cluster_report() {
    let dir = read_scope();
    let spec = zeroship_testkit::redis::cluster_spec().expect("the Dragonfly cluster recipe");
    let lease = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("join the cluster");
    println!("CONTAINER={}", lease.container_id);
    println!("NONCE={}", lease.nonce);
    println!("BOOTED={}", lease.booted);
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

// --- the shared servers ---------------------------------------------------------

#[test]
fn two_processes_using_the_bare_server_get_one_container() {
    ensure_image();
    let dir = scratch("bare-shared");

    let (status, lines) = run_child(CHILD_BARE, &dir);
    assert!(
        status.success(),
        "the first bare-server child failed:\n{}",
        lines.join("\n")
    );
    let first = field(&lines, "CONTAINER=");

    let (status, lines) = run_child(CHILD_BARE, &dir);
    assert!(
        status.success(),
        "the second bare-server child failed:\n{}",
        lines.join("\n")
    );
    let second = field(&lines, "CONTAINER=");

    assert_eq!(
        first, second,
        "two processes of a worktree must join its one bare server"
    );
    cleanup(&dir);
}

/// Two processes join one Redpanda broker, and the watchdog removes it once the
/// last lease is released.
///
/// The parent holds its own lease across both sequential children; without it
/// the watchdog would remove the broker after the grace between them and the
/// second child would boot a new one.
#[test]
fn two_processes_share_one_redpanda_broker() {
    let dir = scratch("redpanda-shared");
    let spec = zeroship_testkit::redpanda::spec().expect("the Redpanda broker recipe");
    let held = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("boot the broker");
    let id = held.container_id.clone();

    let (status, lines) = run_child(CHILD_REDPANDA, &dir);
    assert!(
        status.success(),
        "the first broker child failed:\n{}",
        lines.join("\n")
    );
    let first = field(&lines, "CONTAINER=");
    assert_eq!(
        field(&lines, "BOOTED="),
        "false",
        "a child must join the broker the parent booted"
    );

    let (status, lines) = run_child(CHILD_REDPANDA, &dir);
    assert!(
        status.success(),
        "the second broker child failed:\n{}",
        lines.join("\n")
    );
    let second = field(&lines, "CONTAINER=");

    assert_eq!(first, id, "a child must join the broker the parent booted");
    assert_eq!(second, id, "both children must join the one broker");

    drop(held);
    wait_removed(&id, REMOVAL_BOUND);
    cleanup(&dir);
}

/// Two processes join one `MySQL` server, and the watchdog removes it once the
/// last lease is released.
///
/// The parent holds its own lease across both sequential children; without it
/// the watchdog would remove the server after the grace between them and the
/// second child would boot a new one.
#[test]
fn two_processes_share_one_mysql_server() {
    let _ = zeroship_testkit::mysql::image().expect("build the shared MySQL image");
    let dir = scratch("mysql-shared");
    let spec = zeroship_testkit::mysql::spec().expect("the MySQL server recipe");
    let held = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("boot the server");
    let id = held.container_id.clone();

    let (status, lines) = run_child(CHILD_MYSQL, &dir);
    assert!(
        status.success(),
        "the first MySQL child failed:\n{}",
        lines.join("\n")
    );
    let first = field(&lines, "CONTAINER=");
    assert_eq!(
        field(&lines, "BOOTED="),
        "false",
        "a child must join the server the parent booted"
    );

    let (status, lines) = run_child(CHILD_MYSQL, &dir);
    assert!(
        status.success(),
        "the second MySQL child failed:\n{}",
        lines.join("\n")
    );
    let second = field(&lines, "CONTAINER=");

    assert_eq!(first, id, "a child must join the server the parent booted");
    assert_eq!(second, id, "both children must join the one server");

    drop(held);
    wait_removed(&id, REMOVAL_BOUND);
    cleanup(&dir);
}

/// Two processes join one standalone Redis server, and the watchdog removes it
/// once the last lease is released.
///
/// The parent holds its own lease across both sequential children; without it
/// the watchdog would remove the server after the grace between them and the
/// second child would boot a new one.
#[test]
fn two_processes_share_one_redis_server() {
    let dir = scratch("redis-shared");
    let spec = zeroship_testkit::redis::standalone_spec().expect("the Redis server recipe");
    let held = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("boot the server");
    let id = held.container_id.clone();

    let (status, lines) = run_child(CHILD_REDIS, &dir);
    assert!(
        status.success(),
        "the first Redis child failed:\n{}",
        lines.join("\n")
    );
    let first = field(&lines, "CONTAINER=");
    assert_eq!(
        field(&lines, "BOOTED="),
        "false",
        "a child must join the server the parent booted"
    );

    let (status, lines) = run_child(CHILD_REDIS, &dir);
    assert!(
        status.success(),
        "the second Redis child failed:\n{}",
        lines.join("\n")
    );
    let second = field(&lines, "CONTAINER=");

    assert_eq!(first, id, "a child must join the server the parent booted");
    assert_eq!(second, id, "both children must join the one server");

    drop(held);
    wait_removed(&id, REMOVAL_BOUND);
    cleanup(&dir);
}

/// Two processes join one Dragonfly cluster container, and the watchdog
/// removes it once the last lease is released.
///
/// The parent holds its own lease across both sequential children; without it
/// the watchdog would remove the cluster after the grace between them and the
/// second child would boot a new one. This is the regression the run-shared
/// cluster fixture exists to prevent: before it, every test process started
/// its own three-node cluster.
#[test]
fn two_processes_share_one_dragonfly_cluster_server() {
    let dir = scratch("dragonfly-cluster-shared");
    let spec = zeroship_testkit::redis::cluster_spec().expect("the Dragonfly cluster recipe");
    let held = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("boot the cluster");
    let id = held.container_id.clone();

    let (status, lines) = run_child(CHILD_DRAGONFLY_CLUSTER, &dir);
    assert!(
        status.success(),
        "the first cluster child failed:\n{}",
        lines.join("\n")
    );
    let first = field(&lines, "CONTAINER=");
    assert_eq!(
        field(&lines, "BOOTED="),
        "false",
        "a child must join the cluster container the parent booted"
    );

    let (status, lines) = run_child(CHILD_DRAGONFLY_CLUSTER, &dir);
    assert!(
        status.success(),
        "the second cluster child failed:\n{}",
        lines.join("\n")
    );
    let second = field(&lines, "CONTAINER=");

    assert_eq!(first, id, "a child must join the cluster container the parent booted");
    assert_eq!(second, id, "both children must join the one cluster container");

    drop(held);
    wait_removed(&id, REMOVAL_BOUND);
    cleanup(&dir);
}

/// A node inside the shared Dragonfly cluster dying must not leave a
/// container the watchdog still considers healthy but that answers short a
/// slot owner for the rest of the run: the wrapper running all three nodes
/// must exit the moment any one of them does, so the watchdog's own liveness
/// check on its tracked child fails, `auto_remove` takes the container, and
/// the next joiner boots a fresh, fully configured cluster - mirroring how
/// `a_stopped_server_is_replaced` proves the single-process servers recover
/// from the same shape of failure.
#[test]
fn a_cluster_with_a_dead_node_is_replaced() {
    let dir = scratch("cluster-dead-node");
    let spec = zeroship_testkit::redis::cluster_spec().expect("the Dragonfly cluster recipe");
    let first = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("boot the cluster");
    let dead = first.container_id.clone();

    // Shut down one node from the inside - the shape of a crash, not a
    // graceful `docker stop` of the whole container: the other two nodes keep
    // running underneath it. Node 1 listens on port 7001 (`CLUSTER_BASE_PORT
    // + 1` in `src/redis.rs`); `SHUTDOWN` closes the connection without a
    // reply, so the exec's own exit code is not what this test checks.
    let _ = shared::exec_in_container(&dead, &["redis-cli", "-p", "7001", "SHUTDOWN", "NOSAVE"]);

    // `first`'s lease is held throughout this wait, on purpose: the
    // throwaway scope's ordinary idle-grace teardown (`GRACE`, above) would
    // otherwise remove the container on its own a couple of seconds after a
    // dropped lease, regardless of the dead node, and the removal below would
    // prove nothing about the wrapper. Removal here can only come from the
    // watchdog noticing its tracked child - the wrapper - has exited, which is
    // the fix under test: a wrapper that outlives the dead node leaves this
    // waiting out its full bound against a cluster that still looks ready
    // with two of its three nodes alive.
    wait_removed(&dead, REMOVAL_BOUND);
    drop(first);

    let second = shared::join(&Scope::at(&dir, GRACE), &spec, |_| Ok(())).expect("boot a fresh cluster");
    assert!(
        second.booted,
        "a cluster whose node died must be replaced with a fresh boot, not joined"
    );
    assert_ne!(second.container_id, dead, "the cluster with the dead node must not be reused");

    let live = second.container_id.clone();
    drop(second);
    wait_removed(&live, REMOVAL_BOUND);
    cleanup(&dir);
}

/// The shared standalone Redis server and the Dragonfly cluster are each
/// shared by every test process of a run, so two cases that overlap on either
/// one must not see each other's keys. Each "case" here is a concurrent
/// future minting its own prefix with `case_prefix()`, writing a label under
/// it, and reading the label back after a window where a concurrent case's
/// write could have landed on the same key - which is exactly what happens if
/// the prefix is not unique.
#[compio::test]
async fn concurrent_cases_on_the_shared_redis_server_see_only_their_own_prefix() {
    let url = zeroship_testkit::redis::redis().url();

    async fn one_case(url: String, label: &'static str) -> bool {
        let prefix = zeroship_testkit::redis::case_prefix();
        let key = format!("{prefix}:marker");
        let mut client = compio_redis::Client::connect(&url)
            .await
            .expect("connect to the shared Redis server");
        client.set(&key, label.as_bytes(), None).await.expect("set this case's marker");
        // Give a concurrent case's write a window to land on the same key if
        // the two cases' prefixes were not actually distinct.
        compio::time::sleep(Duration::from_millis(200)).await;
        let seen = client
            .get(&key)
            .await
            .expect("get this case's marker")
            .expect("the key this case wrote is still there");
        client.del(&key).await.ok();
        String::from_utf8(seen).expect("a label is valid UTF-8") == label
    }

    let (a_ok, b_ok) =
        futures::future::join(one_case(url.clone(), "case-a"), one_case(url, "case-b")).await;
    assert!(
        a_ok,
        "a case must read back its own write under its own prefix, not a concurrent case's"
    );
    assert!(
        b_ok,
        "a case must read back its own write under its own prefix, not a concurrent case's"
    );
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

fn schema_dump(container_id: &str, database: &str) -> String {
    // `pg_dump` brackets its output with `\restrict`/`\unrestrict` carrying a
    // random key, so two dumps of identical catalogs differ in those lines.
    let output = shared::exec_in_container(
        container_id,
        &[
            "pg_dump",
            "-U",
            "postgres",
            "--schema-only",
            database,
        ],
    )
    .expect("dump the schema");
    assert!(
        output.success(),
        "pg_dump failed: {}",
        output.stderr_text()
    );
    String::from_utf8_lossy(&output.stdout)
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

/// A server with room for exactly two sessions, so a third is refused with
/// SQLSTATE 53300 and the fixture reports the connection budget.
fn tiny_spec(inputs: &str) -> shared::Spec {
    let mut spec = test_spec(inputs);
    spec.args = vec![
        "docker-entrypoint.sh".to_owned(),
        "postgres".to_owned(),
        "-c".to_owned(),
        "max_connections=2".to_owned(),
        "-c".to_owned(),
        "superuser_reserved_connections=0".to_owned(),
    ];
    spec
}

#[compio::test]
async fn a_connection_with_no_slot_left_names_the_shared_budget() {
    let dir = scratch("slot-budget");
    let lease = shared::join(&Scope::at(&dir, GRACE), &tiny_spec("slot-budget"), |_| Ok(()))
        .expect("boot the slot-budget server");
    let url = format!(
        "postgresql://postgres:fixture@127.0.0.1:{}/postgres",
        lease.port
    );

    // Hold the two sessions the server has, so the next connection is refused.
    let mut held = Vec::new();
    for _ in 0..2 {
        held.push(connect(&url).await);
    }

    let outcome = AssertUnwindSafe(zeroship_testkit::postgres::connect_str(&url))
        .catch_unwind()
        .await;
    let panic = outcome.expect_err("a server with no slot left must refuse");
    let message = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
        .unwrap_or_default();
    assert!(
        message.contains("connection slot"),
        "the refusal must name the connection budget: {message}"
    );

    drop(held);
    drop(lease);
    cleanup(&dir);
}
