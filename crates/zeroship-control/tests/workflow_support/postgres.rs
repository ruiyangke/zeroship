//! Process-owned migrated template and disposable workflow databases.
//!
//! TWO servers, one per private zone. A native helper owns both Testcontainers
//! handles and watches the test's stdin pipe, so the cached servers are removed
//! even if the test process aborts.
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::OnceLock;

/// The `[[example]]` target in this package's manifest that owns the servers.
const ENVIRONMENT: &str = "workflow-test-environment";

struct Template {
    _process: Child,
    _input: ChildStdin,
    /// The platform zone: the migrated `zeroship` schema and every platform table.
    url: String,
    /// The creator zone: the same cluster roles, and no platform schema.
    creator_url: String,
}

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

#[path = "../../../../tests/testkit/src/nested_cargo.rs"]
mod nested_cargo;

/// Build the targets `selection` names for the host with the dev profile, and
/// return each executable cargo reports, by target name.
///
/// The cargo is the one that compiled this test, started in the workspace this
/// test was compiled from, so a worktree builds its own sources. What it
/// shares with the outer run is this process's environment, `CARGO_TARGET_DIR`
/// included, less the variables [`nested_cargo::cargo`] removes, and the
/// configuration files cargo finds from the workspace root. Nothing given on
/// the outer command line reaches it: `--target-dir`, `--config`, `--release`,
/// `--profile`, `--target` and feature flags stop at the outer cargo. The
/// executables come from cargo's own artifact report rather than from a
/// guessed target-directory layout.
///
/// The outer `cargo test` holds no build lock while its test binaries run, so
/// when both runs resolve the same target directory this build waits on
/// nothing, and it reuses every unit the outer run compiled with the same
/// profile and features.
pub fn build(what: &str, selection: &[&str]) -> BTreeMap<String, PathBuf> {
    let output = nested_cargo::cargo()
        .args([
            "build",
            "--locked",
            "--message-format=json-render-diagnostics",
        ])
        .args(selection)
        .current_dir(root())
        .output()
        .unwrap_or_else(|error| panic!("run cargo to build the {what}: {error}"));
    if !output.status.success() {
        let logs = root().join("target/workflow-tests");
        std::fs::create_dir_all(&logs).expect("workflow build log directory");
        let path = logs.join(format!("build-{}.log", uuid::Uuid::new_v4()));
        let mut contents = output.stderr;
        contents.extend_from_slice(&output.stdout);
        std::fs::write(&path, contents).expect("workflow build log");
        panic!("the {what} build failed; see {}", path.display());
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let message: serde_json::Value = serde_json::from_str(line).ok()?;
            Some((
                message["target"]["name"].as_str()?.to_owned(),
                PathBuf::from(message["executable"].as_str()?),
            ))
        })
        .collect()
}

fn template() -> &'static Template {
    static TEMPLATE: OnceLock<Template> = OnceLock::new();
    TEMPLATE.get_or_init(|| {
        let root = root();
        // No test can name an `[[example]]` executable: `CARGO_BIN_EXE_` covers
        // `[[bin]]` targets only, and a `cargo test --test <target>` run does
        // not build examples. The fixture builds the helper itself, once per
        // test process.
        let binary = build(
            "workflow test environment",
            &["-p", env!("CARGO_PKG_NAME"), "--example", ENVIRONMENT],
        )
        .remove(ENVIRONMENT)
        .unwrap_or_else(|| panic!("cargo reported no {ENVIRONMENT} executable"));
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
            url: value["url"].as_str().expect("platform database URL").into(),
            creator_url: value["creator_url"]
                .as_str()
                .expect("creator database URL")
                .into(),
        }
    })
}

/// One fleet's pair of databases: a platform database cloned from the migrated
/// template, and a creator database on the other cluster.
#[derive(Debug)]
pub struct Database {
    url: String,
    name: String,
    creator_url: String,
    creator_name: String,
}

impl Database {
    pub fn new() -> Self {
        static CLONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = CLONE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let template = template();
        let name = format!("workflow_{}", uuid::Uuid::new_v4().simple());
        let mut url = url::Url::parse(&template.url).unwrap();
        url.set_path("/postgres");
        execute(
            url.to_string(),
            format!("CREATE DATABASE {name} TEMPLATE workflow_template"),
        );
        url.set_path(&format!("/{name}"));

        // The creator database carries NO platform schema, so it is created
        // empty rather than cloned from anything. Its database schemas arrive
        // the way production's do: converged by the cluster reconciler's
        // statements and migrated through the migration service's apply path.
        let creator_name = format!("creator_{}", uuid::Uuid::new_v4().simple());
        let mut creator = url::Url::parse(&template.creator_url).unwrap();
        creator.set_path("/postgres");
        execute(creator.to_string(), format!("CREATE DATABASE {creator_name}"));
        creator.set_path(&format!("/{creator_name}"));

        Self {
            url: url.into(),
            name,
            creator_url: creator.into(),
            creator_name,
        }
    }

    /// The PLATFORM database: Control, the manager, the gateway and the CDC
    /// relay's identity reads.
    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// The CREATOR database: the worker and the database schemas it runs
    /// creator code against. It holds no workflow journal, and no `zeroship`
    /// schema - that absence is what the worker's boot posture gate refuses to
    /// start without.
    pub fn creator_url(&self) -> String {
        self.creator_url.clone()
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        for (url, name) in [
            (&self.url, &self.name),
            (&self.creator_url, &self.creator_name),
        ] {
            let mut url = url::Url::parse(url).unwrap();
            url.set_path("/postgres");
            execute(
                url.to_string(),
                format!("DROP DATABASE {name} WITH (FORCE)"),
            );
        }
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
