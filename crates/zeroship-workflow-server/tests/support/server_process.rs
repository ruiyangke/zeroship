#![allow(
    clippy::future_not_send,
    reason = "a spawned process and its compio clients stay on the test runtime"
)]

use ntex::{client::Client, http::StatusCode};
#[path = "queue_control.rs"]
mod queue_control;
use std::{
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

/// The request-body budget this fixture configures, FAR below the production
/// default on purpose: a boundary probed against a small cap costs one small
/// body, and a route that answers to a larger one has to say so.
pub const MAX_REQUEST_BYTES: usize = 1024;

pub struct ServerProcess {
    child: Child,
    _control: queue_control::Control,
    config: PathBuf,
    log: PathBuf,
    pub url: String,
    pub address: std::net::SocketAddr,
}
impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl ServerProcess {
    /// A process configured as a deployment configures one, sweep lane included.
    pub async fn start(
        database: &str,
        peers: &Path,
        directory: &Path,
        name: &str,
        client: &Client,
    ) -> Self {
        Self::spawn_with(database, peers, directory, name, client, true).await
    }

    /// The same process with `workflow.maintenance_sweeps` off, so it composes no
    /// sweep lane at all.
    ///
    /// For a case whose subject is a route, a protocol or a duty rather than the
    /// lane: the lane asserts its own authority over every maintenance row of
    /// this queue, so a running one competes with the claim the case makes for
    /// itself and there is no placement to expire that would stop it. The lane's
    /// own reach through the real cadence is bound by
    /// `crates/zeroship-workflow-server/tests/maintenance_lane.rs`, which keeps
    /// it on.
    pub async fn without_maintenance_sweeps(
        database: &str,
        peers: &Path,
        directory: &Path,
        name: &str,
        client: &Client,
    ) -> Self {
        Self::spawn_with(database, peers, directory, name, client, false).await
    }

    /// How many app-facts observations the Control peer has answered.
    pub fn control_facts_requests(&self) -> usize {
        self._control.facts_requests()
    }

    async fn spawn_with(
        database: &str,
        peers: &Path,
        directory: &Path,
        name: &str,
        client: &Client,
        maintenance_sweeps: bool,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let config = directory.join(format!("{name}.toml"));
        let control = queue_control::Control::start(directory, name, database).await;
        super::platform::write_private(
            &config,
            toml::to_string(&serde_json::json!({"workflow":{
                "listen":address.to_string(),"database_url":database,"service_peers_file":peers,
                "control_url":control.url(),"service_key_file":control.key_file,
                // The payload store this process stages and collects objects
                // through. ONE root for every process this fixture starts,
                // because that is what a deployment owes: an object one replica
                // wrote is one another replica and the workers must be able to
                // read. A root per process would model a misconfiguration.
                "storage_url":directory.join("payload-objects"),
                "http_threads":2,"max_request_bytes":MAX_REQUEST_BYTES,
                "maintenance_sweeps":maintenance_sweeps,
            }}))
            .unwrap(),
        );
        let log = directory.join(format!("{name}.log"));
        let child = spawn(&config, &log);
        let mut server = Self {
            child,
            _control: control,
            config,
            log,
            url: format!("http://{address}"),
            address,
        };
        server.ready(client).await;
        server
    }
    pub async fn ready(&mut self, client: &Client) {
        let result = compio::time::timeout(Duration::from_secs(30), async {
            loop {
                assert!(
                    self.child.try_wait().unwrap().is_none(),
                    "{}",
                    std::fs::read_to_string(&self.log).unwrap()
                );
                if let Ok(response) = client.get(format!("{}/readyz", self.url)).send().await {
                    if response.status() == StatusCode::OK {
                        break;
                    }
                }
                compio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            result.is_ok(),
            "coordinator did not become ready: {}",
            std::fs::read_to_string(&self.log).unwrap()
        );
    }
    pub async fn restart(&mut self, client: &Client) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.child = spawn(&self.config, &self.log);
        self.ready(client).await;
    }
    pub async fn expect_failure(&mut self) {
        let status = compio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    break status;
                }
                compio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("coordinator retained a failed authentication database connection");
        assert!(!status.success());
    }
}
fn spawn(config: &Path, log: &Path) -> Child {
    let output = std::fs::File::create(log).unwrap();
    Command::new(env!("CARGO_BIN_EXE_zeroship-workflow-server"))
        .env_clear()
        .arg("--config")
        .arg(config)
        .stdin(Stdio::null())
        .stdout(output.try_clone().unwrap())
        .stderr(output)
        .spawn()
        .unwrap()
}
