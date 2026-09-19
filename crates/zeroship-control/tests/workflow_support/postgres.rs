//! Process-owned migrated template and disposable workflow databases.
//!
//! TWO servers, one per private zone. A native helper owns both Testcontainers
//! handles and watches the test's stdin pipe, so the cached servers are removed
//! even if the test process aborts.
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::OnceLock;

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

fn template() -> &'static Template {
    static TEMPLATE: OnceLock<Template> = OnceLock::new();
    TEMPLATE.get_or_init(|| {
        let root = root();
        // BUILT BY THE SUITE, NOT BY THE TEST. The workflow environment is a
        // fixture artifact, and building it here would make every run depend on
        // build locks, feature resolution and the source tree - and let a test
        // compile its own subject. `cargo test` does not build `[[example]]`
        // targets and `CARGO_BIN_EXE_` is not set for them, so the path cannot
        // be derived either; the suite that runs these tests must provide it.
        let Some(binary) = zeroship_core::test_env!("ZEROSHIP_WORKFLOW_TEST_ENVIRONMENT_BIN")
        else {
            panic!(
                "ZEROSHIP_WORKFLOW_TEST_ENVIRONMENT_BIN is required: \
                 `cargo xtask test workflow` builds the workflow environment \
                 example and passes its path. Running this target directly \
                 needs that variable set to the built example."
            )
        };
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
        // empty rather than cloned from anything. Its app schemas arrive the
        // way production's do: through the migration service's provisioning and
        // apply path.
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

    /// The CREATOR database: the worker, its app schemas and their journals.
    /// It has no `zeroship` schema, which is what the worker's boot posture
    /// gate refuses to start without.
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
