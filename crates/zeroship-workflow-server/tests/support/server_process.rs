#![allow(
    clippy::future_not_send,
    reason = "a spawned process and its compio clients stay on the test runtime"
)]

use ntex::{client::Client, http::StatusCode};
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
        platform: &super::platform::Platform,
        peers: &Path,
        directory: &Path,
        name: &str,
        client: &Client,
    ) -> Self {
        Self::spawn_with(platform, peers, directory, name, client, true).await
    }

    /// The same process with `workflow.maintenance_sweeps` off, so it composes no
    /// sweep lane at all.
    ///
    /// For a case whose subject is a route, a protocol or a duty rather than the
    /// lane: the lane asserts its own authority over every maintenance row of
    /// this queue, so a running one competes with the claim the case makes for
    /// itself and there is no placement to expire that would stop it. The lane's
    /// own reach through the real cadence is bound by
    /// `crates/zeroship-workflow-server/tests/integration/maintenance_lane.rs`, which keeps
    /// it on.
    pub async fn without_maintenance_sweeps(
        platform: &super::platform::Platform,
        peers: &Path,
        directory: &Path,
        name: &str,
        client: &Client,
    ) -> Self {
        Self::spawn_with(platform, peers, directory, name, client, false).await
    }

    /// How many app-facts observations the Control peer has answered.
    pub fn control_facts_requests(&self) -> usize {
        self._control.facts_requests()
    }

    async fn spawn_with(
        platform: &super::platform::Platform,
        peers: &Path,
        directory: &Path,
        name: &str,
        client: &Client,
        maintenance_sweeps: bool,
    ) -> Self {
        let config = directory.join(format!("{name}.toml"));
        let mut control =
            Some(queue_control::Control::start(directory, name, platform.admin_url.as_str()).await);
        let log = directory.join(format!("{name}.log"));
        // The OS hands out an ephemeral port, and two cases starting at once can
        // be handed the same one before either child binds it. Re-reserve and
        // retry rather than fail the case on another case's port.
        for _ in 0..16 {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            drop(listener);
            super::platform::write_private(
                &config,
                toml::to_string(&serde_json::json!({"workflow":{
                    "listen":address.to_string(),"database_url":platform.runtime_url,"service_peers_file":peers,
                    "control_url":control.as_ref().unwrap().url(),"service_key_file":control.as_ref().unwrap().key_file,
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
            let mut child = spawn(&config, &log);
            let url = format!("http://{address}");
            match wait_started(&mut child, &log, client, &url).await {
                Started::Ready => {
                    return Self {
                        child,
                        _control: control.take().unwrap(),
                        config,
                        log,
                        url,
                        address,
                    }
                }
                Started::AddressInUse => {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        panic!("could not reserve a listening port for {name} after repeated collisions");
    }
    pub async fn ready(&mut self, client: &Client) {
        match wait_started(&mut self.child, &self.log, client, &self.url).await {
            Started::Ready => {}
            Started::AddressInUse => {
                panic!("the coordinator's port was taken while it was starting")
            }
        }
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

/// How a spawned process's readiness wait ended.
enum Started {
    Ready,
    /// The process exited because another case's server already owns the
    /// reserved address, so the caller reserves a different one.
    AddressInUse,
}

/// Wait for `child` to answer `/readyz`, or report why it stopped.
///
/// # Panics
/// When the process exits for any reason other than an address collision, or
/// does not become ready within the wait.
async fn wait_started(child: &mut Child, log: &Path, client: &Client, url: &str) -> Started {
    let result = compio::time::timeout(Duration::from_secs(30), async {
        loop {
            if child.try_wait().unwrap().is_some() {
                let text = std::fs::read_to_string(log).unwrap_or_default();
                if text.contains("Address already in use") {
                    return Started::AddressInUse;
                }
                panic!("{text}");
            }
            if let Ok(response) = client.get(format!("{url}/readyz")).send().await {
                if response.status() == StatusCode::OK {
                    return Started::Ready;
                }
            }
            compio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    match result {
        Ok(started) => started,
        Err(_) => panic!(
            "coordinator did not become ready: {}",
            std::fs::read_to_string(log).unwrap_or_default()
        ),
    }
}
