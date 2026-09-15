//! Own the migrated Testcontainers server until the test process closes stdin.
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    GenericImage, ImageExt,
};

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn main() {
    let owner = std::env::args().nth(1).expect("workflow test process id");
    let work = tempfile::tempdir().expect("workflow migration directory");
    let server = GenericImage::new("postgres", "18")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stdout(
            "PostgreSQL init process complete; ready for start up.",
        ))
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_PASSWORD", "workflow-fixture")
        .with_env_var("POSTGRES_DB", "workflow_template")
        .with_label("zeroship.workflow.test-process", owner.clone())
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
            // One server hosts every fleet the test binary runs at once, and
            // one fleet is a control plane, a worker, a gateway, a CDC relay
            // and the test process, each holding connection pools. The stock
            // ceiling is reached while fewer fleets run than a machine has
            // cores, and a service that cannot connect exits rather than
            // waits, so the suite fails as a dead worker rather than as a
            // refused connection.
            "-c",
            "max_connections=512",
            "-c",
            "fsync=off",
        ])
        .with_startup_timeout(Duration::from_secs(120))
        .start()
        .expect("workflow tests require Docker and PostgreSQL");
    let mut url =
        url::Url::parse("postgres://postgres:workflow-fixture@localhost/workflow_template")
            .unwrap();
    url.set_host(Some(&server.get_host().expect("Postgres host").to_string()))
        .unwrap();
    url.set_port(Some(
        server.get_host_port_ipv4(5432).expect("Postgres port"),
    ))
    .unwrap();
    let url = url.to_string();
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

    println!("{}", serde_json::json!({"url":url}));
    std::io::stdout()
        .flush()
        .expect("publish workflow database");
    // The pipe closes on normal exit, panic, abort, or termination of the owner.
    let mut input = Vec::new();
    std::io::stdin()
        .read_to_end(&mut input)
        .expect("wait for workflow tests");
    if let Ok(stderr) = server.stderr_to_vec() {
        let _ = std::fs::write(logs.join(format!("postgres-{owner}.log")), stderr);
    }
    drop(server);
    drop(work);
}
