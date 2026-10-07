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
        Self::spawn_with(platform, peers, directory, name, client, true, serde_json::json!({}))
            .await
    }

    /// The same process with `settings` added to its `workflow` table, for a
    /// case whose subject is one setting's reach into the running process.
    pub async fn start_with(
        platform: &super::platform::Platform,
        peers: &Path,
        directory: &Path,
        name: &str,
        client: &Client,
        settings: serde_json::Value,
    ) -> Self {
        Self::spawn_with(platform, peers, directory, name, client, true, settings).await
    }

    /// The same process with `workflow.maintenance_sweeps` off, so it composes no
    /// sweep lane at all.
    ///
    /// For a case whose subject is a route, a protocol or a duty rather than the
    /// lane: the lane asserts its own authority over every maintenance row of
    /// this queue, so a running one competes with the claim the case makes for
    /// itself and nothing the case holds would stop it. The lane's
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
        Self::spawn_with(
            platform,
            peers,
            directory,
            name,
            client,
            false,
            serde_json::json!({}),
        )
        .await
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
        settings: serde_json::Value,
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
            write_config(
                &config,
                platform,
                peers,
                control.as_ref().unwrap(),
                address,
                maintenance_sweeps,
                &settings,
            );
            let mut child = spawn(&config, &log);
            let url = format!("http://{address}");
            match wait_started(&mut child, &log, client, address).await {
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

    /// Spawn one server on `address`, or report that another process holds it.
    ///
    /// A single attempt at a caller-chosen address, for a case that pins what
    /// the readiness wait does with a collision. The re-reserving constructors
    /// pick their own address and cannot be pointed at one another process
    /// already owns, so a case that must force the race uses this.
    pub async fn spawn_at(
        platform: &super::platform::Platform,
        peers: &Path,
        directory: &Path,
        name: &str,
        client: &Client,
        address: std::net::SocketAddr,
    ) -> Option<Self> {
        let config = directory.join(format!("{name}.toml"));
        let mut control =
            Some(queue_control::Control::start(directory, name, platform.admin_url.as_str()).await);
        let log = directory.join(format!("{name}.log"));
        write_config(
            &config,
            platform,
            peers,
            control.as_ref().unwrap(),
            address,
            false,
            &serde_json::json!({}),
        );
        let mut child = spawn(&config, &log);
        let url = format!("http://{address}");
        match wait_started(&mut child, &log, client, address).await {
            Started::Ready => Some(Self {
                child,
                _control: control.take().unwrap(),
                config,
                log,
                url,
                address,
            }),
            Started::AddressInUse => {
                let _ = child.kill();
                let _ = child.wait();
                None
            }
        }
    }
    pub async fn ready(&mut self, client: &Client) {
        match wait_started(&mut self.child, &self.log, client, self.address).await {
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

/// Write the coordinator's config for one reserved address.
fn write_config(
    config: &Path,
    platform: &super::platform::Platform,
    peers: &Path,
    control: &queue_control::Control,
    address: std::net::SocketAddr,
    maintenance_sweeps: bool,
    settings: &serde_json::Value,
) {
    let directory = config.parent().expect("the config has a directory");
    let mut workflow = serde_json::json!({
        "listen":address.to_string(),"database_url":platform.runtime_url,"service_peers_file":peers,
        "control_url":control.url(),"service_key_file":control.key_file,
        // The payload store this process stages and collects objects
        // through. ONE root for every process this fixture starts,
        // because that is what a deployment owes: an object one replica
        // wrote is one another replica and the workers must be able to
        // read. A root per process would model a misconfiguration.
        "storage_url":directory.join("payload-objects"),
        "http_threads":2,"max_request_bytes":MAX_REQUEST_BYTES,
        "maintenance_sweeps":maintenance_sweeps,
    });
    for (key, value) in settings.as_object().expect("settings are a table") {
        workflow[key] = value.clone();
    }
    super::platform::write_private(
        config,
        toml::to_string(&serde_json::json!({"workflow": workflow})).unwrap(),
    );
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
/// A readiness answer is accepted only from THIS child. The reserved port is
/// released before the child binds it, so another case's server can take the
/// port and answer `/readyz` first; accepting that answer points every request
/// at a service holding a different database, and its first authenticated call
/// is refused.
///
/// # Panics
/// When the process exits for any reason other than an address collision, or
/// does not become ready within the wait.
async fn wait_started(
    child: &mut Child,
    log: &Path,
    client: &Client,
    address: std::net::SocketAddr,
) -> Started {
    let url = format!("http://{address}");
    let result = compio::time::timeout(Duration::from_secs(30), async {
        loop {
            if child.try_wait().unwrap().is_some() {
                return child_stopped(log);
            }
            if let Ok(response) = client.get(format!("{url}/readyz")).send().await {
                if response.status() == StatusCode::OK
                    && zeroship_testkit::listen::child_listens(child.id(), address.port())
                {
                    // The child that owns the socket can still have exited
                    // since, freeing the address for another case.
                    if child.try_wait().unwrap().is_some() {
                        return child_stopped(log);
                    }
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

/// How a readiness wait ends when its child has already stopped.
fn child_stopped(log: &Path) -> Started {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    if text.contains("Address already in use") {
        Started::AddressInUse
    } else {
        panic!("{text}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zeroship_core::{
        service_assertion::ServiceSigningKey,
        service_peers::{service_issuer, CONTROL_SERVICE_NAME},
    };

    /// The readiness wait must not believe a `/readyz` answered by a process
    /// that does not own the reserved address. A foreign server holds the
    /// socket, so the real child fails to bind and the wait reports the
    /// collision rather than running the case against the foreign server's
    /// database.
    #[ntex::test]
    async fn a_foreign_server_on_the_reserved_address_is_not_adopted() {
        use std::io::{Read as _, Write as _};
        let platform = super::super::platform::Platform::fresh_database().await;
        let control = service_issuer(CONTROL_SERVICE_NAME).unwrap();
        let control_key = ServiceSigningKey::generate();
        let peers = platform.work.path().join("collision-peers.json");
        super::super::platform::write_private(
            &peers,
            serde_json::to_vec(&json!({"keys":[{
                "iss":control.as_str(),"x":control_key.public_jwk_x()
            }]}))
            .unwrap(),
        );
        let http = Client::new().await;
        // Hold the address with a server that answers `/readyz` for a database
        // it does not own.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut head = Vec::new();
                let mut chunk = [0_u8; 256];
                while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => head.extend_from_slice(&chunk[..read]),
                    }
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                );
                let _ = stream.flush();
            }
        });
        let spawned = ServerProcess::spawn_at(
            &platform,
            &peers,
            platform.work.path(),
            "collision",
            &http,
            address,
        )
        .await;
        assert!(
            spawned.is_none(),
            "the readiness wait adopted a server that does not own the address"
        );
    }
}
