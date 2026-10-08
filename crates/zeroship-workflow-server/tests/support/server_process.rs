#![allow(
    clippy::future_not_send,
    reason = "a spawned process and its compio clients stay on the test runtime"
)]

use ntex::{client::Client, http::StatusCode};
mod queue_control;
use std::{
    io::{Read as _, Seek as _, SeekFrom, Write as _},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

/// The request-body budget this fixture configures, FAR below the production
/// default on purpose: a boundary probed against a small cap costs one small
/// body, and a route that answers to a larger one has to say so.
pub const MAX_REQUEST_BYTES: usize = 1024;

/// How much of a server's log a failing case reports, from the tail.
///
/// A process that ran long enough to fail has its startup chatter far behind
/// the entries that explain the failure, so the tail holds what a reader came
/// for; the bound keeps one server from burying the failure it is meant to
/// explain under the whole of what it ever wrote. Signed, because a seek from
/// the end takes an `i64` and this runs on the unwinding path, where a fallible
/// conversion could not be reported.
const REPORTED_LOG_BYTES: i64 = 64 * 1024;

/// The header a failing case prints before the server log it attaches.
///
/// Named, so a reader who sees several servers' output in one run knows which
/// process each block came from.
fn log_header(name: &str) -> String {
    format!("===== zeroship-workflow-server {name} log =====")
}

/// Print the tail of a server's stdout-and-stderr log to stderr.
///
/// Called only while this thread unwinds, so a passing case stays quiet, and
/// nothing here may panic: a second panic during unwinding aborts the process
/// and hides the failure this report exists to explain. Every I/O error is
/// dropped rather than unwrapped, and the tail begins after any continuation
/// bytes a cut through a multi-byte character left at the front.
fn report_log(name: &str, log: &Path) {
    let mut stderr = std::io::stderr();
    let mut file = match std::fs::File::open(log) {
        Ok(file) => file,
        Err(error) => {
            let _ = writeln!(stderr, "{} (unreadable: {error})", log_header(name));
            return;
        }
    };
    let len = file.metadata().map_or(0, |meta| meta.len());
    let bound = u64::try_from(REPORTED_LOG_BYTES).unwrap_or(u64::MAX);
    if len > bound {
        let _ = file.seek(SeekFrom::End(-REPORTED_LOG_BYTES));
    }
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    let start = bytes
        .iter()
        .position(|&byte| byte & 0b1100_0000 != 0b1000_0000)
        .unwrap_or(bytes.len());
    let _ = writeln!(stderr, "{}", log_header(name));
    let _ = stderr.write_all(&bytes[start..]);
}

pub struct ServerProcess {
    child: Child,
    _control: queue_control::Control,
    config: PathBuf,
    log: PathBuf,
    name: String,
    pub url: String,
    pub address: std::net::SocketAddr,
}
impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            report_log(&self.name, &self.log);
        }
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
                        name: name.to_owned(),
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
                name: name.to_owned(),
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

    /// A server over a fresh database, with the platform that owns its work
    /// directory held BESIDE it.
    ///
    /// The server comes first in the tuple so it drops first: a failing case
    /// reaches `ServerProcess`'s drop while the log is still on disk, and only
    /// afterwards does the platform remove the work directory that holds it.
    async fn live_server(name: &str) -> (ServerProcess, super::super::platform::Platform) {
        let platform = super::super::platform::Platform::fresh_database().await;
        let control = service_issuer(CONTROL_SERVICE_NAME).unwrap();
        let control_key = ServiceSigningKey::generate();
        let peers = platform.work.path().join(format!("{name}-peers.json"));
        super::super::platform::write_private(
            &peers,
            serde_json::to_vec(&json!({"keys":[{
                "iss":control.as_str(),"x":control_key.public_jwk_x()
            }]}))
            .unwrap(),
        );
        let http = Client::new().await;
        let server = ServerProcess::without_maintenance_sweeps(
            &platform,
            &peers,
            platform.work.path(),
            name,
            &http,
        )
        .await;
        (server, platform)
    }

    /// The child half of
    /// `a_failing_case_reports_its_server_log_and_a_passing_case_does_not`:
    /// start a server, then fail, so the server is dropped while the thread
    /// unwinds and its log is reported.
    #[ntex::test]
    #[ignore = "run by a_failing_case_reports_its_server_log_and_a_passing_case_does_not"]
    async fn panics_after_starting_a_server() {
        let _live = live_server("panics").await;
        panic!("this case fails on purpose");
    }

    /// The control for that helper: start a server, then pass, so nothing is
    /// reported on the way down.
    #[ntex::test]
    #[ignore = "run by a_failing_case_reports_its_server_log_and_a_passing_case_does_not"]
    async fn passes_after_starting_a_server() {
        let _live = live_server("passes").await;
    }

    /// A case that unwinds with a live server prints that server's log; a case
    /// that passes prints nothing.
    ///
    /// The trigger is this process's own unwind, so the arms run as children
    /// of this test binary: a panic cannot be caught here without changing what
    /// the drop observes. Each child waits for its server to answer `/readyz`
    /// before it fails or passes, so the log is known to hold the startup line
    /// before the drop reads it and the assertion races nothing.
    #[test]
    fn a_failing_case_reports_its_server_log_and_a_passing_case_does_not() {
        let failed = run_ignored_helper("panics_after_starting_a_server");
        assert!(
            !failed.status.success(),
            "the failing helper must fail, so its drop runs while unwinding"
        );
        let stderr = String::from_utf8_lossy(&failed.stderr);
        assert!(
            stderr.contains(&log_header("panics")),
            "a failing case did not report its server log:\n{stderr}"
        );
        assert!(
            stderr.contains("workflow coordinator listening"),
            "the reported log did not carry the server's startup line:\n{stderr}"
        );

        let passed = run_ignored_helper("passes_after_starting_a_server");
        assert!(
            passed.status.success(),
            "the passing helper must pass: {}",
            String::from_utf8_lossy(&passed.stderr)
        );
        let stderr = String::from_utf8_lossy(&passed.stderr);
        assert!(
            !stderr.contains(&log_header("passes")),
            "a passing case printed its server log:\n{stderr}"
        );
        assert!(
            !stderr.contains("workflow coordinator listening"),
            "a passing case printed the server's startup line:\n{stderr}"
        );
    }

    /// Run one ignored helper of this test binary to completion, capturing both
    /// streams. The child fails or passes on its own; the caller reads what it
    /// reported on stderr.
    fn run_ignored_helper(name: &str) -> std::process::Output {
        let exe = std::env::current_exe().expect("this test binary");
        let test = format!(
            "{}::{name}",
            module_path!().split_once("::").expect("crate prefix").1
        );
        Command::new(exe)
            .args([
                "--exact",
                test.as_str(),
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .output()
            .expect("run the helper in a child of this test binary")
    }
}
