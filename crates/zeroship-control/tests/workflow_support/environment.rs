//! Own the migrated Testcontainers servers until the test process closes stdin.
//!
//! TWO servers, because the fleet runs two PRIVATE ZONES. The platform server
//! carries the `zeroship` schema and every platform table; the creator server
//! carries app schemas and their workflow journals and has no platform schema
//! at all. A fleet hands Control, the manager and the gateway the first and the
//! worker the second, so "Control cannot reach a creator journal" and "a worker
//! cannot reach a platform table" are properties of the connection rather than
//! of a grant somebody could widen.
//!
//! The creator server is seeded from the platform server's ROLE GLOBALS, not
//! from its database: roles are cluster-wide, so a creator cluster needs the
//! same `zeroship_worker` login and the same per-app role machinery, and
//! nothing else.
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use testcontainers::core::{CmdWaitFor, ExecCommand, IntoContainerPort, WaitFor};
use testcontainers::{runners::SyncRunner, Container, GenericImage, ImageExt};

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

/// The shared server tuning. Both zones take it: one server hosts every fleet
/// the test binary runs at once, and one fleet is a control plane, a worker, a
/// gateway, a CDC relay and the test process, each holding connection pools.
/// The stock ceiling is reached while fewer fleets run than a machine has
/// cores, and a service that cannot connect exits rather than waits, so the
/// suite would fail as a dead worker rather than as a refused connection.
fn image(owner: &str, database: &str) -> testcontainers::ContainerRequest<GenericImage> {
    GenericImage::new("postgres", "18")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stdout(
            "PostgreSQL init process complete; ready for start up.",
        ))
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_PASSWORD", "workflow-fixture")
        .with_env_var("POSTGRES_DB", database)
        .with_label("zeroship.workflow.test-process", owner.to_owned())
        .with_cmd([
            "postgres",
            "-c",
            "wal_level=logical",
            "-c",
            "max_slot_wal_keep_size=128MB",
            "-c",
            "max_replication_slots=128",
            "-c",
            "max_wal_senders=128",
            "-c",
            "max_connections=512",
            "-c",
            "fsync=off",
        ])
        .with_startup_timeout(Duration::from_secs(120))
}

fn url_for(server: &Container<GenericImage>, database: &str) -> String {
    let mut url =
        url::Url::parse(&format!("postgres://postgres:workflow-fixture@localhost/{database}"))
            .unwrap();
    url.set_host(Some(&server.get_host().expect("Postgres host").to_string()))
        .unwrap();
    url.set_port(Some(
        server.get_host_port_ipv4(5432).expect("Postgres port"),
    ))
    .unwrap();
    url.to_string()
}

/// The cluster's role globals, minus the `postgres` role initdb already made.
fn role_globals(server: &Container<GenericImage>) -> Vec<u8> {
    let mut result = server
        .exec(
            ExecCommand::new(["pg_dumpall", "-U", "postgres", "--globals-only"])
                .with_cmd_ready_condition(CmdWaitFor::exit_code(0)),
        )
        .expect("dump the platform cluster's role globals");
    let dump = result.stdout_to_vec().expect("read the role globals");
    String::from_utf8(dump)
        .expect("UTF-8 role dump")
        .replacen("CREATE ROLE postgres;\n", "", 1)
        .into_bytes()
}

fn main() {
    let owner = std::env::args().nth(1).expect("workflow test process id");
    let work = tempfile::tempdir().expect("workflow migration directory");
    let platform = image(&owner, "workflow_template")
        .start()
        .expect("workflow tests require Docker and PostgreSQL");
    let url = url_for(&platform, "workflow_template");
    let root = root();
    let config = serde_json::json!({ "env": { "platform": {
        "url": url, "dir": root.join("db/migrations-ts"), "schema": "zeroship", "owner_app": "zeroship_platform",
        "registry": root.join("policies/platform-table-owners.json"), "policy": [root.join("policies/platform.policy.toml")],
    } } });
    let config_path = work.path().join("migrate.toml");
    std::fs::write(
        &config_path,
        toml::to_string(&config).expect("migration config"),
    )
    .expect("write config");
    std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600))
        .expect("private migration config");
    let logs = root.join("target/workflow-tests");
    std::fs::create_dir_all(&logs).expect("migration log directory");
    let log_path = logs.join(format!("migrate-{}.log", uuid::Uuid::new_v4()));
    let log = std::fs::File::create(&log_path).expect("migration log");
    let status = Command::new("node")
        .arg(root.join("packages/zero-migrate-cli/dist/cli-bin.js"))
        .args(["apply", "--config"])
        .arg(config_path)
        .args(["--env", "platform", "--approve"])
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .status()
        .expect("run the built migration CLI; prepare SDKs with pnpm build");
    assert!(
        status.success(),
        "workflow platform migrations failed; see {}",
        log_path.display()
    );

    // The creator cluster: the same roles, and no platform schema. Seeded AFTER
    // the migrations so it carries the roles those migrations create, including
    // the per-app role template every apply grants from.
    let creator = image(&owner, "creator_template")
        .with_copy_to(
            "/docker-entrypoint-initdb.d/roles.sql",
            role_globals(&platform),
        )
        .start()
        .expect("workflow tests require a second Docker PostgreSQL for the creator zone");
    let creator_url = url_for(&creator, "creator_template");

    println!(
        "{}",
        serde_json::json!({"url": url, "creator_url": creator_url})
    );
    std::io::stdout()
        .flush()
        .expect("publish workflow databases");
    // The pipe closes on normal exit, panic, abort, or termination of the owner.
    let mut input = Vec::new();
    std::io::stdin()
        .read_to_end(&mut input)
        .expect("wait for workflow tests");
    for (server, name) in [(&platform, "platform"), (&creator, "creator")] {
        if let Ok(stderr) = server.stderr_to_vec() {
            let _ = std::fs::write(logs.join(format!("postgres-{name}-{owner}.log")), stderr);
        }
    }
    drop(creator);
    drop(platform);
    drop(work);
}
