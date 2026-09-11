//! Process-owned migrated template and disposable workflow databases.
//! Testcontainers' reaper removes the template server when the test process
//! disconnects. Individual fixtures drop only databases created on that server.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};

struct Template {
    _server: Container<GenericImage>,
    url: String,
    _work: tempfile::TempDir,
}

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

pub fn template_url() -> String {
    static TEMPLATE: OnceLock<Template> = OnceLock::new();
    TEMPLATE.get_or_init(|| {
        let work = tempfile::tempdir().expect("workflow migration directory");
        let server = GenericImage::new("postgres", "18")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stdout("PostgreSQL init process complete; ready for start up."))
            .with_wait_for(WaitFor::message_on_stderr("database system is ready to accept connections"))
            .with_env_var("POSTGRES_PASSWORD", "workflow-fixture")
            .with_env_var("POSTGRES_DB", "workflow_template")
            .with_cmd(["postgres", "-c", "wal_level=logical", "-c", "max_replication_slots=128", "-c", "max_wal_senders=128", "-c", "fsync=off"])
            .with_startup_timeout(Duration::from_secs(120))
            .start().expect("workflow tests require Docker and PostgreSQL");
        let mut url = url::Url::parse("postgres://postgres:workflow-fixture@localhost/workflow_template").unwrap();
        url.set_host(Some(&server.get_host().expect("Postgres host").to_string())).unwrap();
        url.set_port(Some(server.get_host_port_ipv4(5432).expect("Postgres port"))).unwrap();
        let url = url.to_string();
        let root = root();
        let config = serde_json::json!({ "env": { "platform": {
            "url": url, "dir": root.join("db/migrations-ts"), "schema": "zeroship", "owner_app": "zeroship_platform",
            "registry": root.join("policies/platform-table-owners.json"), "policy": [root.join("policies/platform.policy.toml")],
        } } });
        let config_path = work.path().join("migrate.toml");
        std::fs::write(&config_path, toml::to_string(&config).expect("migration config")).expect("write config");
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).expect("private migration config");
        let logs = root.join("target/workflow-tests");
        std::fs::create_dir_all(&logs).expect("migration log directory");
        let log_path = logs.join(format!("migrate-{}.log", uuid::Uuid::new_v4()));
        let log = std::fs::File::create(&log_path).expect("migration log");
        let status = Command::new("node")
            .arg(root.join("packages/zero-migrate-cli/dist/cli-bin.js"))
            .args(["apply", "--config"]).arg(config_path).args(["--env", "platform", "--approve"])
            .current_dir(&root).stdin(Stdio::null())
            .stdout(log.try_clone().unwrap()).stderr(log)
            .status().expect("run the built migration CLI; prepare SDKs with pnpm build");
        assert!(status.success(), "workflow platform migrations failed; see {}", log_path.display());
        Template { _server: server, url, _work: work }
    }).url.clone()
}

#[derive(Debug)]
pub struct Database {
    url: String,
    name: String,
}

impl Database {
    pub fn new() -> Self {
        static CLONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = CLONE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let template = template_url();
        let name = format!("workflow_{}", uuid::Uuid::new_v4().simple());
        let mut url = url::Url::parse(&template).unwrap();
        url.set_path("/postgres");
        execute(
            url.to_string(),
            format!("CREATE DATABASE {name} TEMPLATE workflow_template"),
        );
        url.set_path(&format!("/{name}"));
        Self {
            url: url.into(),
            name,
        }
    }

    pub fn url(&self) -> String {
        self.url.clone()
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let mut url = url::Url::parse(&self.url).unwrap();
        url.set_path("/postgres");
        execute(
            url.to_string(),
            format!("DROP DATABASE {} WITH (FORCE)", self.name),
        );
    }
}

fn execute(url: String, sql: String) {
    std::thread::spawn(move || {
        compio::runtime::Runtime::new()
            .expect("fixture runtime")
            .block_on(async {
                let (client, connection) = compio_postgres::connect(&url, compio_postgres::NoTls)
                    .await
                    .expect("fixture Postgres connection");
                let driver = compio::runtime::spawn(connection.run());
                client
                    .batch_execute(&sql)
                    .await
                    .expect("fixture database lifecycle");
                drop(client);
                driver
                    .await
                    .expect("fixture Postgres task")
                    .expect("fixture Postgres driver");
            });
    })
    .join()
    .expect("fixture database thread");
}
