//! Process-owned migrated template and disposable workflow databases.
//! A native helper owns the Testcontainers handle and watches the test's stdin
//! pipe, so the cached server is removed even if the test process aborts.
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::OnceLock;

struct Template {
    _process: Child,
    _input: ChildStdin,
    url: String,
}

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

pub fn template_url() -> String {
    static TEMPLATE: OnceLock<Template> = OnceLock::new();
    TEMPLATE
        .get_or_init(|| {
            let root = root();
            let build = Command::new(env!("CARGO"))
                .args([
                    "build",
                    "--locked",
                    "-p",
                    "zeroship-control",
                    "--example",
                    "workflow-test-environment",
                    "--message-format=json",
                ])
                .current_dir(&root)
                .output()
                .expect("build workflow test environment");
            assert!(
                build.status.success(),
                "workflow environment build failed: {}",
                String::from_utf8_lossy(&build.stderr)
            );
            let binary = String::from_utf8_lossy(&build.stdout)
                .lines()
                .find_map(|line| {
                    let artifact: serde_json::Value = serde_json::from_str(line).ok()?;
                    (artifact["target"]["name"] == "workflow-test-environment")
                        .then(|| artifact["executable"].as_str().map(str::to_owned))
                        .flatten()
                })
                .expect("workflow environment executable");
            let logs = root.join("target/workflow-tests");
            std::fs::create_dir_all(&logs).expect("workflow environment logs");
            let path = logs.join(format!("environment-{}.log", uuid::Uuid::new_v4()));
            let log = std::fs::File::create(&path).expect("workflow environment log");
            let mut process = Command::new(binary)
                .arg(std::process::id().to_string())
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(log)
                .spawn()
                .expect("start workflow test environment");
            let input = process
                .stdin
                .take()
                .expect("workflow environment lifetime pipe");
            let mut ready = String::new();
            BufReader::new(process.stdout.take().unwrap())
                .read_line(&mut ready)
                .expect("read workflow environment readiness");
            let value: serde_json::Value = serde_json::from_str(&ready).unwrap_or_else(|error| {
                panic!(
                    "workflow database startup failed: {error}; see {}",
                    path.display()
                )
            });
            Template {
                _process: process,
                _input: input,
                url: value["url"].as_str().expect("workflow database URL").into(),
            }
        })
        .url
        .clone()
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
