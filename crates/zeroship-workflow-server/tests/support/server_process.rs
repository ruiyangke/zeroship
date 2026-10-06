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
                if response.status() == StatusCode::OK && child_listens(child.id(), address.port()) {
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

/// Whether `pid` holds the LISTEN socket on `127.0.0.1:port`.
///
/// Read from `/proc`, because the answer is about one process's open
/// descriptors and not about what the kernel has bound: another case's server
/// bound to the same address answers `/readyz` exactly as this child does, and
/// only ownership tells the two apart. Scope is IPv4 loopback, which is what
/// the fixture binds; any other bind fails every start loudly rather than
/// misreading an address.
fn child_listens(pid: u32, port: u16) -> bool {
    let Ok(table) = std::fs::read_to_string("/proc/net/tcp") else {
        return false;
    };
    let local = format!("0100007F:{port:04X}");
    let Some(inode) = table.lines().skip(1).find_map(|line| {
        let mut fields = line.split_whitespace();
        let _slot = fields.next()?;
        let bound = fields.next()?;
        let _remote = fields.next()?;
        let state = fields.next()?;
        if bound != local || state != "0A" {
            return None;
        }
        fields.nth(5)
    }) else {
        return false;
    };
    let socket = format!("socket:[{inode}]");
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    entries.flatten().any(|entry| {
        std::fs::read_link(entry.path())
            .is_ok_and(|target| target.to_string_lossy() == socket)
    })
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
    use std::io::BufRead as _;

    /// The readiness wait must not believe a `/readyz` answered by a process
    /// that does not own the reserved address. A foreign server holds the
    /// socket, so the real child fails to bind and the wait reports the
    /// collision rather than running the case against the foreign server's
    /// database.
    #[ntex::test]
    async fn a_foreign_server_on_the_reserved_address_is_not_adopted() {
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
                use std::io::{Read as _, Write as _};
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

    /// The ownership predicate answers for the process that holds the socket,
    /// not for the address alone. This process's listener is its own; a
    /// listener a child of this test binary holds is not, even though the
    /// address is bound either way.
    #[test]
    fn the_ownership_predicate_follows_the_process_that_holds_the_socket() {
        let mine = TcpListener::bind("127.0.0.1:0").unwrap();
        let mine_port = mine.local_addr().unwrap().port();
        assert!(
            child_listens(std::process::id(), mine_port),
            "this process holds a listener on {mine_port}"
        );

        let mut child = run_ignored_helper("holds_a_listener_and_reports_its_port");
        // The helper's port is the first line of its output that is exactly a
        // port: the harness prints its own `test <name> ... ` prefix on the
        // same stream. A helper that never reports fails this test rather than
        // hanging it.
        let (sender, receiver) = std::sync::mpsc::channel();
        let stdout = child.stdout.take().expect("the helper's stdout");
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            let mut reported = false;
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        if !reported {
                            let _ = sender
                                .send(Err("the helper closed its output without a port".to_owned()));
                        }
                        return;
                    }
                    Ok(_) => {
                        // Keep draining after the port: closing this end early
                        // makes the helper's own result write fail.
                        if !reported {
                            if let Ok(port) = line.trim().parse::<u16>() {
                                let _ = sender.send(Ok(port));
                                reported = true;
                            }
                        }
                    }
                    Err(error) => {
                        if !reported {
                            let _ = sender.send(Err(format!("reading the helper's port: {error}")));
                        }
                        return;
                    }
                }
            }
        });
        let held_port = match receiver.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(port)) => port,
            Ok(Err(error)) => panic!("{error}"),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the helper reported no port");
            }
        };

        assert!(
            child_listens(child.id(), held_port),
            "the child holds a listener on {held_port}"
        );
        assert!(
            !child_listens(std::process::id(), held_port),
            "this process does not hold the child's listener on {held_port}"
        );

        // Closing the helper's stdin ends its read and lets it exit.
        drop(child.stdin.take());
        let status = child.wait().expect("wait for the helper");
        assert!(status.success(), "the helper exited with {status}");
    }

    /// Run one ignored helper of this test binary, with stdin and stdout piped
    /// so the helper can report a value and then wait to be released.
    fn run_ignored_helper(name: &str) -> Child {
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
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("run the helper in a child of this test binary")
    }

    /// The child half of
    /// `the_ownership_predicate_follows_the_process_that_holds_the_socket`:
    /// bind a listener, report the port the kernel gave it, then hold the
    /// socket until this process's stdin closes.
    #[test]
    #[ignore = "run by the_ownership_predicate_follows_the_process_that_holds_the_socket"]
    fn holds_a_listener_and_reports_its_port() {
        use std::io::{Read as _, Write as _};
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral listener");
        let port = listener.local_addr().expect("the bound address").port();
        let mut stdout = std::io::stdout();
        writeln!(stdout, "\n{port}").expect("report the port");
        stdout.flush().expect("flush the port");
        let mut ignored = String::new();
        let _ = std::io::stdin().read_to_string(&mut ignored);
    }
}
