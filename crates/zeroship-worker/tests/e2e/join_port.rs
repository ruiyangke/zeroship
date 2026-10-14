//! The port a worker BOUND is the port it advertises when it joins.
//!
//! `crates/zeroship-worker/src/join.rs`'s own tests rule on the wire the join
//! writes, but they hand the port in themselves; none of them can rule on
//! whether `main` passes the port the listener actually bound. This spawns the
//! shipped binary with `--port 0`, listens for the announcement, and reads the
//! port the stand-in Control was asked to register - the two must be one port,
//! and it must be the port the child owns a LISTEN socket on.
//!
//! A stand-in Control rather than the real one: the join is the whole subject,
//! and a real Control would make the fixture own its enrollment grammar and a
//! rotated token file for one request. The stand-in records the body and
//! answers the one status the join accepts, so what is asserted here is what
//! the worker PUT on the wire, not what Control did with it.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::Duration;

use base64::Engine as _;
use zeroship_core::service_peers::{
    CONTROL_SERVICE_NAME, GATEWAY_SERVICE_NAME, SERVICE_TRUST_DOMAIN,
};
use zeroship_core::worker_join::WORKER_JOIN_PATH;

/// 64 hex characters, so the control key clears the boot gate's nonempty floor.
const STRONG_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A deterministic Ed25519 keypair, so the peer document is reproducible.
fn signing_key(seed: u8) -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
}

fn public_x(key: &ed25519_dalek::SigningKey) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes())
}

fn peer_entry(service: &str, x: &str) -> String {
    format!(
        r#"{{"kty":"OKP","crv":"Ed25519","iss":"spiffe://{SERVICE_TRUST_DOMAIN}/{service}","x":"{x}"}}"#
    )
}

/// The peer document a worker needs to build its gateway envelope verifier: the
/// gateway's key, plus Control's so the document is the deployment's shape.
fn peers_document() -> String {
    format!(
        r#"{{"keys":[{},{}]}}"#,
        peer_entry(GATEWAY_SERVICE_NAME, &public_x(&signing_key(0x22))),
        peer_entry(CONTROL_SERVICE_NAME, &public_x(&signing_key(0x11))),
    )
}

/// Write `contents` at 0600, the mode the join-token reader requires of a
/// bearer credential.
fn write_secret(path: &std::path::Path, contents: &str) {
    std::fs::write(path, contents).expect("write the join token");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("owner-only join token");
    }
}

/// One request a stand-in Control read.
struct Request {
    path: String,
    body: String,
}

/// A stand-in Control plane that records the port each join advertised.
///
/// Its accept thread blocks in `incoming()`, so dropping the fixture sets the
/// stop flag and wakes the accept with one connection to its own address -
/// nothing polls, sleeps or leaks a thread. The value is declared before the
/// worker it serves, so the worker is reaped first and no late request races
/// the shutdown.
struct StandIn {
    port: u16,
    advertised: mpsc::Receiver<u16>,
    stop: Arc<AtomicBool>,
    accept: Option<std::thread::JoinHandle<()>>,
}

impl Drop for StandIn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept: one connection, no sleep. The loop reads
        // the flag on its next turn and returns.
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

/// Answer every request the worker makes, recording the port in a join body and
/// admitting the join so the boot continues.
fn stand_in_control() -> StandIn {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("stand-in listener");
    let port = listener.local_addr().expect("stand-in address").port();
    let (sender, advertised) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let accept = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            if stopping.load(Ordering::SeqCst) {
                return;
            }
            let Ok(request) = read_request(&mut stream) else {
                continue;
            };
            let (status, body) = if request.path == WORKER_JOIN_PATH {
                let value: serde_json::Value =
                    serde_json::from_str(&request.body).unwrap_or(serde_json::Value::Null);
                let port = value
                    .get("port")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|port| u16::try_from(port).ok())
                    .expect("the join body carries a port");
                let _ = sender.send(port);
                (
                    "201 Created",
                    format!(
                        r#"{{"instance_id":"{}"}}"#,
                        zeroship_core::typed_id::new_worker_instance_id()
                    ),
                )
            } else {
                ("200 OK", "{}".to_string())
            };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    StandIn {
        port,
        advertised,
        stop,
        accept: Some(accept),
    }
}

/// Read one HTTP/1.1 request head and body from `stream`.
fn read_request(stream: &mut std::net::TcpStream) -> std::io::Result<Request> {
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 1024];
    let head_end = loop {
        if let Some(end) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(std::io::Error::other("closed before a request head"));
        }
        raw.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let path = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_owned();
    let wanted: usize = head
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse().ok())
        })
        .unwrap_or(0);
    let mut body = raw[head_end + 4..].to_vec();
    while body.len() < wanted {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Ok(Request {
        path,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// Read `stdout` on a background thread until it announces `marker`, then keep
/// draining so the child never blocks on a full pipe. The test waits on the
/// returned channel, so a child that stalls fails by timeout rather than
/// hanging the run, and nothing polls on a sleep.
fn await_announcement(stdout: ChildStdout, marker: &'static str) -> mpsc::Receiver<u16> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut announced = false;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    if !announced {
                        if let Some(port) = announced_port(&line, marker) {
                            let _ = sender.send(port);
                            announced = true;
                        }
                    }
                }
            }
        }
    });
    receiver
}

/// The port a single log line announced in its `bind` field, if it carried
/// `marker`. The tracing line is JSON on a non-TTY stream; a plain
/// `bind=host:port` spelling is accepted too, so this reader is not pinned to
/// one format.
fn announced_port(line: &str, marker: &str) -> Option<u16> {
    if !line.contains(marker) {
        return None;
    }
    let field = serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| {
            value
                .pointer("/fields/bind")
                .or_else(|| value.get("bind"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .or_else(|| {
            line.split("bind=")
                .nth(1)
                .map(|rest| rest.split_whitespace().next().unwrap_or("").to_owned())
        })?;
    field.parse::<std::net::SocketAddr>().ok().map(|a| a.port())
}

/// The spawned worker, killed and reaped when dropped so a case that panics on
/// any assertion leaks no process.
struct Worker(Child);

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A `--port 0` worker names the port it BOUND in its announcement, owns a
/// LISTEN socket on it, and advertises that same port to Control in the join.
#[test]
fn a_zero_port_worker_joins_with_the_port_it_bound() {
    let database = zeroship_testkit::postgres::platform().url("zeroship_worker");
    // Declared BEFORE the worker: values drop in reverse order, so the worker
    // is reaped first and no in-flight request reaches the stand-in after its
    // accept thread stops.
    let control = stand_in_control();
    let scratch = tempfile::tempdir().expect("worker scratch directory");
    let token = scratch.path().join("join-token");
    write_secret(&token, "HEADER.PAYLOAD.SIGNATURE");
    let peers = scratch.path().join("service-peers.json");
    std::fs::write(&peers, peers_document()).expect("write the peer document");
    let blobs = scratch.path().join("blobs");
    std::fs::create_dir_all(&blobs).expect("create the blob store root");
    let stderr = scratch.path().join("worker.err");
    let sink = std::fs::File::create(&stderr).expect("worker stderr log");

    let mut child = spawn_worker(&database, &control, &token, &peers, &blobs, sink);
    let announcement = await_announcement(
        child.0.stdout.take().expect("piped stdout"),
        "worker listening",
    );
    let port = match announcement.recv_timeout(Duration::from_mins(1)) {
        Ok(port) => port,
        Err(error) => panic!(
            "the worker never announced a bound port ({error}); stderr:\n{}",
            std::fs::read_to_string(&stderr).unwrap_or_default()
        ),
    };
    assert_ne!(
        port, 0,
        "a --port 0 worker announced the requested port, not the bound one"
    );
    assert!(
        zeroship_testkit::listen::child_listens(child.0.id(), port),
        "the worker announced port {port} but does not own a listening socket on it; stderr:\n{}",
        std::fs::read_to_string(&stderr).unwrap_or_default()
    );
    let advertised = control
        .advertised
        .recv_timeout(Duration::from_mins(1))
        .expect("the worker must join Control");
    assert_eq!(
        advertised, port,
        "the worker joined with port {advertised} but announced binding port {port}"
    );
}

/// Spawn the worker with `--port 0` against the stand-in Control.
fn spawn_worker(
    database: &url::Url,
    control: &StandIn,
    token: &std::path::Path,
    peers: &std::path::Path,
    blobs: &std::path::Path,
    stderr: std::fs::File,
) -> Worker {
    Worker(
        Command::new(env!("CARGO_BIN_EXE_zeroship-worker"))
            .env_clear()
            .env("ZEROSHIP_CONTROL_KEY", STRONG_HEX)
            .env("ZEROSHIP_WORKER_DATABASE_URL", database.as_str())
            .env("ZEROSHIP_WORKER_JOIN_TOKEN_FILE", token)
            .env("ZEROSHIP_WORKER_SERVICE_PEERS_FILE", peers)
            .args(["--port", "0"])
            .args(["--threads", "1"])
            .args(["--control-url", &format!("http://127.0.0.1:{}", control.port)])
            .args([
                "--cdc-relay-url",
                "wss://127.0.0.1:1/internal/v1/cdc/subscribe",
            ])
            .arg("--blob-store")
            .arg(blobs)
            .args(["--observability-log-format", "json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("spawn zeroship-worker"),
    )
}
