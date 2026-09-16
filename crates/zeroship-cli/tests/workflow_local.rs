//! Exercise the CLI binding and background worker with a real compiled archive.

use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::{local_dev_app_id, AppId},
    schema_name::SchemaName,
    workflow_deployments::HoldScope,
};
use zeroship_data_orm::{
    binding::DbBinding,
    encryption::ProjectKeySource,
    orm::{Database, Output},
    value, ConnectOptions, Value as Record,
};
use zeroship_workflow_manager::deployments;

struct Host {
    child: Child,
    port: u16,
    log: tempfile::NamedTempFile,
}
impl Host {
    fn start(root: &Path, app: Option<&AppId>, native_dev: bool) -> Self {
        Self::launch(root, app, native_dev, None)
    }

    fn launch(root: &Path, app: Option<&AppId>, native_dev: bool, config: Option<&Path>) -> Self {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let log = tempfile::NamedTempFile::new().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_zeroship"));
        command
            .current_dir(root)
            .args(["serve", "app.zship", "--workers=1"])
            .arg(format!("--port={port}"))
            .env_remove("APP_ID")
            .env("ZEROSHIP_WORKFLOW_SQLITE_PATH", root.join("unused.sqlite"))
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.reopen().unwrap()))
            .stderr(Stdio::from(log.reopen().unwrap()));
        if let Some(app) = app {
            command.env("APP_ID", app.as_str());
        }
        if let Some(config) = config {
            command.arg(format!("--workflow-config={}", config.display()));
        }
        for key in [
            "DATABASE_URL",
            "ZEROSHIP_KV_CONFIG_FILE",
            "ZEROSHIP_KV_PATH",
            "ZEROSHIP_STORAGE_URL",
            "ZEROSHIP_DEV",
            "ZEROSHIP_DIE_WITH_PARENT",
            "ZEROSHIP_RUNTIME_DESCRIPTOR",
        ] {
            command.env_remove(key);
        }
        if native_dev {
            command
                .args([
                    "--dev-bootstrap=dev-entry.mjs",
                    "--dev-entry-loader=createDevEntryLoader",
                ])
                .env("ZEROSHIP_DEV", "1");
        }
        let mut host = Self {
            child: command.spawn().unwrap(),
            port,
            log,
        };
        host.until("/ping", |reply| reply == &json!({"ready": true}));
        host
    }

    fn request(&mut self, path: &str) -> Result<Value, String> {
        if let Some(status) = self.child.try_wait().unwrap() {
            panic!(
                "CLI exited {status}: {}",
                std::fs::read_to_string(self.log.path()).unwrap()
            );
        }
        let mut socket = TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", self.port).parse().unwrap(),
            Duration::from_millis(250),
        )
        .map_err(|error| error.to_string())?;
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            socket,
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut reader = BufReader::new(socket);
        let mut status = String::new();
        reader
            .read_line(&mut status)
            .map_err(|error| error.to_string())?;
        let mut length = None;
        loop {
            let mut header = String::new();
            reader
                .read_line(&mut header)
                .map_err(|error| error.to_string())?;
            if header == "\r\n" {
                break;
            }
            if header.is_empty() {
                return Err("response headers ended early".into());
            }
            if let Some((name, value)) = header.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse::<usize>().ok();
                }
            }
        }
        let length = length
            .filter(|length| *length <= 64 * 1024)
            .ok_or("expected a bounded JSON response")?;
        let mut body = vec![0; length];
        reader
            .read_exact(&mut body)
            .map_err(|error| error.to_string())?;
        if !status.starts_with("HTTP/1.1 200 ") {
            return Err(format!("{status}{}", String::from_utf8_lossy(&body)));
        }
        serde_json::from_slice(&body).map_err(|error| error.to_string())
    }

    fn until(&mut self, path: &str, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let reply = self.request(path);
            if let Ok(reply) = &reply {
                if predicate(reply) {
                    return reply.clone();
                }
            }
            assert!(
                Instant::now() < deadline,
                "CLI did not converge: {reply:?}; {}",
                std::fs::read_to_string(self.log.path()).unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn cli_resumes_a_workflow_from_retained_code_after_process_death() {
    resume_after_process_death(None, false);
}

#[test]
fn cli_resumes_a_workflow_with_a_configured_app_identity() {
    resume_after_process_death(Some(&AppId::mint()), false);
}

#[test]
fn native_dev_entry_keeps_workflow_replay_on_the_retained_archive() {
    resume_after_process_death(None, true);
}

fn write_dev_entry(root: &Path, version: &str) {
    std::fs::write(
        root.join("dev-entry.mjs"),
        format!(
            r#"
export function createDevEntryLoader() {{
  return async () => ({{
    async fetch(request, env) {{
      const url = new URL(request.url);
      if (url.pathname === "/ping") return Response.json({{ ready: true }});
      if (url.pathname === "/version") return Response.json({{ version: {version} }});
      if (url.pathname === "/start") {{
        const run = await env.workflows.Example.start();
        return Response.json({{ id: run.id }});
      }}
      const run = env.workflows.Example.get(url.searchParams.get("id"));
      if (url.pathname === "/signal") return Response.json(await run.signal({{ type: "resume" }}));
      return Response.json(await run.status());
    }}
  }});
}}
"#,
            version = serde_json::to_string(version).unwrap(),
        ),
    )
    .unwrap();
}

/// Compile the fixture app into `root/app.zship`, replacing an earlier build.
/// Its run waits for a signal until `signal_timeout`.
fn compile(root: &Path, version: &str, signal_timeout: &str) {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().unwrap().parent().unwrap();
    let compiled = Command::new("pnpm")
        .current_dir(workspace.join("packages/vite-plugin"))
        .args(["exec", "tsx"])
        .arg(manifest.join("tests/fixtures/app-bundle.ts"))
        .arg(root)
        .arg(version)
        .arg("10ms")
        .arg(signal_timeout)
        .output()
        .expect("build the workflow fixture");
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
}

fn resume_after_process_death(configured_app: Option<&AppId>, native_dev: bool) {
    let expected_app = configured_app.cloned().unwrap_or_else(local_dev_app_id);
    let root = tempfile::tempdir().unwrap();
    compile(root.path(), "original", "1h");
    if native_dev {
        write_dev_entry(root.path(), "live-original");
    }
    let mut host = Host::start(root.path(), configured_app, native_dev);
    assert_eq!(
        host.request("/version").unwrap(),
        json!({"version": if native_dev { "live-original" } else { "original:lazy" }})
    );
    assert!(!root.path().join("unused.sqlite").exists());
    let database = root
        .path()
        .join(".zeroship")
        .join(format!("zs-{}.sqlite", expected_app.as_str()));
    assert!(database.exists());
    // Manager metadata shares the one local platform file with the deployment
    // catalog; there is no workflow-only database or object directory.
    assert!(root
        .path()
        .join(".zeroship/platform/metadata.sqlite")
        .exists());
    assert!(!root
        .path()
        .join(".zeroship/deployments/index.sqlite")
        .exists());
    assert!(!root.path().join(".zeroship/workflows.sqlite").exists());
    assert!(!root.path().join(".zeroship/workflow-objects").exists());
    let started = host.request("/start").unwrap();
    let run = started["id"].as_str().unwrap();
    let status = format!("/status?id={run}");
    host.until(&status, |value| value["state"] == "waiting");
    assert!(!root.path().join(".zeroship/app-id").exists());
    drop(host);
    std::fs::remove_dir_all(root.path().join("src")).unwrap();
    if native_dev {
        write_dev_entry(root.path(), "live-replacement");
    }
    let mut host = Host::start(root.path(), configured_app, native_dev);
    assert_eq!(
        host.request("/version").unwrap(),
        json!({"version": if native_dev { "live-replacement" } else { "original:lazy" }})
    );
    assert!(database.exists());
    assert!(!root.path().join(".zeroship/app-id").exists());
    host.request(&format!("/signal?id={run}")).unwrap();
    let completed = host.until(&status, |value| value["state"] == "completed");
    assert_eq!(completed["output"], "original:original:lazy");
}

/// The running host writes the same file; retry only its brief lock contention.
#[expect(
    clippy::future_not_send,
    reason = "the reader owns the compio runtime it blocks on"
)]
async fn contended<T>(
    mut operation: impl AsyncFnMut() -> Result<T, zeroship_data_orm::error::DbError>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match operation().await {
            Ok(value) => return value,
            Err(zeroship_data_orm::error::DbError::LockContention { .. })
                if Instant::now() < deadline =>
            {
                compio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) => panic!("platform metadata read failed: {error}"),
        }
    }
}

/// Read-only views of the host's platform metadata file, through bindings of
/// its own beside the running host.
struct Platform {
    runtime: compio::runtime::Runtime,
    manager: Database,
    catalog: Database,
    app: AppId,
}

impl Platform {
    fn open(root: &Path, app: &AppId) -> Self {
        let runtime = compio::runtime::Runtime::new().unwrap();
        let url = format!(
            "sqlite:{}",
            root.join(".zeroship/platform/metadata.sqlite").display()
        );
        let (manager, catalog) = runtime.block_on(async {
            let connect = async |schema: &zeroship_data_orm::schema::Schema| {
                contended(async || {
                    Database::connect(
                        DbBinding::new(
                            "platform",
                            "workflow-local-test",
                            SchemaName::new("main").unwrap(),
                        ),
                        ConnectOptions::new(url.as_str(), ProjectKeySource::unavailable())
                            .connection_authority(),
                        schema.clone(),
                    )
                    .await
                })
                .await
            };
            (
                connect(&zeroship_workflow_manager::collections().unwrap()).await,
                connect(&deployments::collections().unwrap()).await,
            )
        });
        Self {
            runtime,
            manager,
            catalog,
            app: app.clone(),
        }
    }

    fn rows(&self, database: &Database, collection: &str, filter: &Record) -> Vec<Record> {
        self.runtime.block_on(async {
            let output = contended(async || {
                database
                    .collection(collection)?
                    .find(filter.clone(), value!({"limit":64}))
                    .await
            })
            .await;
            let Output::Rows { rows, .. } = output else {
                panic!("expected platform metadata rows");
            };
            rows
        })
    }

    /// Deployments in the order the manager activated them.
    fn activated(&self) -> Vec<String> {
        let mut activations = self.rows(
            &self.manager,
            "schedule_activations",
            &value!({"app_id":self.app.as_str()}),
        );
        activations.sort_by_key(|row| row["revision"].as_i64());
        activations
            .iter()
            .map(|row| row["deployment_id"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Every holder of `deployment` in the ledger the collector consults.
    fn holders(&self, deployment: &str) -> Vec<(String, String)> {
        self.rows(
            &self.catalog,
            "app_deploy_holds",
            &value!({"app_id":self.app.as_str(),"deploy_id":deployment}),
        )
        .iter()
        .map(|row| {
            (
                row["holder_id"].as_str().unwrap().to_owned(),
                row["state"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
    }

    /// The manager's queue hold intent and the ledger state of the queue
    /// holder for `deployment`.
    fn queue_hold(&self, deployment: &str) -> (String, String) {
        let intent = self.rows(
            &self.manager,
            "deployment_holds",
            &value!({"app_id":self.app.as_str(),"deployment_id":deployment}),
        );
        let queue = HoldScope::for_queue(self.app.clone());
        let ledger: Vec<_> = self
            .holders(deployment)
            .into_iter()
            .filter(|(holder, _)| holder == queue.holder())
            .collect();
        assert_eq!((intent.len(), ledger.len()), (1, 1));
        (
            intent[0]["state"].as_str().unwrap().to_owned(),
            ledger[0].1.clone(),
        )
    }

    /// The manager's journal release duty for `deployment`.
    fn journal_duty(&self, deployment: &str) -> String {
        let intent = self.rows(
            &self.manager,
            "deployment_holds",
            &value!({"app_id":self.app.as_str(),"deployment_id":deployment}),
        );
        assert_eq!(intent.len(), 1);
        intent[0]["journal_state"].as_str().unwrap().to_owned()
    }

    /// Whether the collector's fence commits for `deployment`, which it does
    /// only once every holder class gave the deployment back. A committed fence
    /// is what authorizes deleting the manifest, so this is not a probe.
    fn reclaim(&self, deployment: &str) -> Result<String, deployments::Error> {
        self.runtime.block_on(async {
            deployments::fence_reclamation(&self.catalog, &self.app, deployment).await
        })
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one host lifetime carries both holder classes to reclamation"
)]
fn republished_bundle_releases_both_deployment_holders() {
    let root = tempfile::tempdir().unwrap();
    // The signal wait's timeout job outlives the signal, so a short timeout
    // lets every job pinned to the original deployment settle within the test.
    compile(root.path(), "original", "2s");
    // Frequent maintenance passes and the shortest grace the manager accepts.
    let config = root.path().join("workflow.toml");
    std::fs::write(
        &config,
        "[manager]\ndriver_interval_ms = 50\nhold_grace_ms = 5001\n",
    )
    .unwrap();
    let mut host = Host::launch(root.path(), None, false, Some(&config));
    let started = host.request("/start").unwrap();
    let run = started["id"].as_str().unwrap().to_owned();
    let status = format!("/status?id={run}");
    host.until(&status, |value| value["state"] == "waiting");
    host.request(&format!("/signal?id={run}")).unwrap();
    let completed = host.until(&status, |value| value["state"] == "completed");
    assert_eq!(completed["output"], "original:original:lazy");
    drop(host);

    // Publishing a new bundle activates it and supersedes the original.
    compile(root.path(), "replacement", "2s");
    let mut host = Host::launch(root.path(), None, false, Some(&config));
    assert_eq!(
        host.request("/version").unwrap(),
        json!({"version": "replacement:lazy"})
    );
    let platform = Platform::open(root.path(), &local_dev_app_id());
    let [original, replacement] = <[String; 2]>::try_from(platform.activated()).unwrap();
    assert_ne!(original, replacement);
    let held = (String::from("held"), String::from("held"));

    // Once the original deployment's jobs settle and its grace passes, the
    // manager releases its queue hold, in its own intent and in the ledger.
    let released = (String::from("released"), String::from("released"));
    let deadline = Instant::now() + Duration::from_secs(30);
    while platform.queue_hold(&original) != released {
        assert!(
            Instant::now() < deadline,
            "the superseded deployment stayed held: {}",
            std::fs::read_to_string(host.log.path()).unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(platform.queue_hold(&replacement), held);
    // The journal holder answers its own release job, and refuses: the run's
    // retained generation still needs the original deployment's code.
    let journal = HoldScope::for_app(local_dev_app_id());
    let deadline = Instant::now() + Duration::from_secs(60);
    while platform.journal_duty(&original) == "pending" {
        assert!(
            Instant::now() < deadline,
            "no journal release was asked for: {}",
            std::fs::read_to_string(host.log.path()).unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        platform
            .holders(&original)
            .into_iter()
            .find(|(holder, _)| *holder == journal.holder())
            .map(|(_, state)| state),
        Some("held".into())
    );
    // Retry a TRANSPORT answer, never a verdict. The local platform is one
    // SQLite file that the running host is also writing, so this call can come
    // back `Unavailable` from lock contention - which is not a refusal and not
    // a permission, and matching it against `Conflict` failed the test at
    // roughly one run in two. Conflict and Ok are both verdicts and are
    // returned as they are; only the transient answer is retried.
    let deadline = Instant::now() + Duration::from_secs(30);
    let verdict = loop {
        match platform.reclaim(&original) {
            Err(deployments::Error::Unavailable(message)) => {
                assert!(
                    Instant::now() < deadline,
                    "the deployment database never answered: {message}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            verdict => break verdict,
        }
    };
    assert!(matches!(verdict, Err(deployments::Error::Conflict(_))));
    drop(host);

    // A third bundle supersedes the replacement, which no run ever used. Both
    // holder classes give it back, and the collector's fence then commits.
    compile(root.path(), "final", "2s");
    let host = Host::launch(root.path(), None, false, Some(&config));
    let deadline = Instant::now() + Duration::from_secs(90);
    // The ledger records the release; the manager discharges its journal duty
    // when it applies the settled reply, which is a later transaction.
    while platform
        .holders(&replacement)
        .iter()
        .any(|(_, state)| state != "released")
        || platform.journal_duty(&replacement) != "released"
    {
        assert!(
            Instant::now() < deadline,
            "the replacement stayed held: {}",
            std::fs::read_to_string(host.log.path()).unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(platform.queue_hold(&replacement), released);
    assert_eq!(platform.journal_duty(&replacement), "released");
    assert!(platform.reclaim(&replacement).is_ok());
    assert!(matches!(
        platform.reclaim(&original),
        Err(deployments::Error::Conflict(_))
    ));
    drop(host);
}

#[test]
fn removed_workflow_bundle_flag_is_rejected() {
    let output = Command::new(env!("CARGO_BIN_EXE_zeroship"))
        .args(["serve", "app.zship", "--workflow-bundle=another.zship"])
        .env_remove("ZEROSHIP_DIE_WITH_PARENT")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown flag `--workflow-bundle`"));
}
