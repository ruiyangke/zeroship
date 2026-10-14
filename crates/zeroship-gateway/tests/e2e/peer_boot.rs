//! The gateway's startup refusal on missing, unreadable or forged service key
//! material, driven against the real binary. It also covers the address the
//! gateway announces once its bind succeeds.
//!
//! `zeroship-core`'s loader tests rule on `load_peer_bundle` and the signing-key
//! reader; none of them can rule on whether `main` CALLS them, or on what an
//! operator sees when it does. These spawn the shipped binary.
//!
//! NO ARM RESTS ON THE EXIT CODE. Every launch here also exits non-zero when the
//! fence holds, because the gateway has a later refusal it cannot get past in
//! this environment - the broker master secret. So each arm rules on WHERE the
//! process stopped: a refusal arm requires the fence's own sentence AND the
//! absence of the later one, and the control requires the reverse.
//!
//! The gateway verifies no identity envelope, so F4 as worded does not reach it.
//! It still holds a service key and presents assertions, and the assertion
//! verifier resolves material from the issuer parsed out of what it is shown -
//! so a document that maps two issuers onto one key makes this process able to
//! present as the worker, which is the wider consequence under test.

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use base64::Engine as _;
use ed25519_dalek::pkcs8::EncodePrivateKey as _;
use zeroship_core::service_peers::{
    CONTROL_SERVICE_NAME, GATEWAY_SERVICE_NAME, SERVICE_TRUST_DOMAIN, WORKER_SERVICE_NAME,
};

/// The sentence a launch stopped at this fence prints.
const SERVICE_KEY_REFUSAL: &str = "refusing to start - service key material rejected";

/// The NEXT boot refusal, reached only once the process is past this fence.
const LATER_REFUSAL: &str = "without a readable";

const STRONG: &str = "0123456789abcdef0123456789abcdef";

/// A fixture directory that removes itself even when an assertion panics.
struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "zeroship-gateway-peer-boot-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create fixture directory");
        Self(path)
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        self.write_bytes(name, contents.as_bytes())
    }

    /// The loaders refuse a group- or world-readable secret, so a fixture at 0644
    /// would fail every arm for the wrong reason.
    fn write_bytes(&self, name: &str, contents: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).expect("write fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("owner-only fixture secret");
        }
        path
    }

    fn absent(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        let _ = std::fs::remove_file(&path);
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A deterministic Ed25519 keypair. A fixed seed rather than a generated key
/// keeps the fixture reproducible and removes the "two keys came out identical"
/// guard a random fixture needs: different seeds cannot collide.
fn signing_key(seed: u8) -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
}

/// The RFC 7517 `x` member: the raw public half, base64url and unpadded.
fn public_x(key: &ed25519_dalek::SigningKey) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes())
}

/// The gateway's own signing key as a PKCS#8 DER file, with the `x` member a
/// peer document must publish for the loader to accept it.
fn gateway_key(fixture: &Fixture) -> (PathBuf, String) {
    let key = signing_key(0x22);
    let der = key.to_pkcs8_der().expect("encode the fixture key as PKCS#8");
    let path = fixture.write_bytes("svc-gateway.der", der.as_bytes());
    (path, public_x(&key))
}

fn peer_entry(service: &str, x: &str) -> String {
    format!(
        r#"{{"kty":"OKP","crv":"Ed25519","iss":"spiffe://{SERVICE_TRUST_DOMAIN}/{service}","x":"{x}"}}"#
    )
}

fn document(entries: &[String]) -> String {
    format!(r#"{{"keys":[{}]}}"#, entries.join(","))
}

fn gateway(key: Option<&Path>, peers: Option<&Path>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zeroship-gate"));
    cmd.env_clear()
        .env("ZEROSHIP_CONTROL_KEY", "control-key-material")
        .env("ZEROSHIP_GATEWAY_STASH_SIGNING_KEY", STRONG)
        .env("ZEROSHIP_PAIRWISE_SALT", STRONG)
        .env(
            "ZEROSHIP_GATEWAY_DATABASE_URL",
            "postgres://u:p@127.0.0.1:1/unreached",
        )
        .args(["--port", "0", "--worker-urls", "http://127.0.0.1:1"]);
    if let Some(key) = key {
        cmd.env("ZEROSHIP_GATEWAY_SERVICE_KEY_FILE", key);
    }
    if let Some(peers) = peers {
        cmd.env("ZEROSHIP_GATEWAY_SERVICE_PEERS_FILE", peers);
    }
    cmd.output().expect("spawn zeroship-gate")
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A process that stopped AT the fence cannot have reached what follows it.
fn assert_stopped_at_the_fence(text: &str, label: &str) {
    assert!(
        !text.contains(LATER_REFUSAL),
        "{label} walked PAST the fence and stopped at the later refusal instead:\n{text}"
    );
}

#[test]
fn an_unset_service_key_refuses_and_names_both_settings() {
    let output = gateway(None, None);
    let text = combined(&output);

    assert!(!output.status.success(), "the gateway BOOTED with no key material");
    assert!(
        text.contains(SERVICE_KEY_REFUSAL),
        "the unconfigured refusal does not say {SERVICE_KEY_REFUSAL:?}:\n{text}"
    );
    // It cannot name a file, so it must name the settings.
    for setting in ["gateway.service_key_file", "gateway.service_peers_file"] {
        assert!(
            text.contains(setting),
            "the unconfigured refusal does not name {setting:?}:\n{text}"
        );
    }
    assert_stopped_at_the_fence(&text, "the unconfigured gateway");
}

#[test]
fn an_absent_peer_document_is_refused_by_name() {
    let fixture = Fixture::new();
    let (key, _) = gateway_key(&fixture);
    let absent = fixture.absent("service-peers-absent.json");
    let output = gateway(Some(&key), Some(&absent));
    let text = combined(&output);

    assert!(!output.status.success(), "the gateway BOOTED on an absent document");
    assert!(
        text.contains(SERVICE_KEY_REFUSAL),
        "the refusal does not say {SERVICE_KEY_REFUSAL:?}:\n{text}"
    );
    assert!(
        text.contains(absent.to_str().expect("UTF-8 path")),
        "the refusal does not name the absent file:\n{text}"
    );
    assert_stopped_at_the_fence(&text, "the gateway with an absent document");
}

#[test]
fn an_unparseable_peer_document_is_refused_by_name() {
    let fixture = Fixture::new();
    let (key, _) = gateway_key(&fixture);
    let malformed = fixture.write("service-peers-malformed.json", "{ this is not a JWKS document");
    let output = gateway(Some(&key), Some(&malformed));
    let text = combined(&output);

    assert!(!output.status.success(), "the gateway BOOTED on an unparseable document");
    assert!(
        text.contains(SERVICE_KEY_REFUSAL),
        "the refusal does not say {SERVICE_KEY_REFUSAL:?}:\n{text}"
    );
    assert!(
        text.contains(malformed.to_str().expect("UTF-8 path")),
        "the refusal does not name the unparseable file:\n{text}"
    );
    assert_stopped_at_the_fence(&text, "the gateway with an unparseable document");
}

/// The gateway's own key also published under the WORKER's issuer: a document
/// that maps two issuers onto one key makes this process able to present as the
/// worker. The document differs from the control below in exactly one member.
#[test]
fn a_document_publishing_its_own_key_as_the_workers_is_refused() {
    let fixture = Fixture::new();
    let (key, gateway_x) = gateway_key(&fixture);
    let forged = fixture.write(
        "service-peers-gateway-mints-its-own.json",
        &document(&[
            peer_entry(WORKER_SERVICE_NAME, &gateway_x),
            peer_entry(GATEWAY_SERVICE_NAME, &gateway_x),
        ]),
    );
    let output = gateway(Some(&key), Some(&forged));
    let text = combined(&output);

    assert!(
        !output.status.success(),
        "the gateway BOOTED on a document publishing its own key as the worker's"
    );
    assert!(
        text.contains(SERVICE_KEY_REFUSAL),
        "the refusal does not say {SERVICE_KEY_REFUSAL:?}:\n{text}"
    );
    assert_stopped_at_the_fence(&text, "the gateway on a self-published document");
}

/// The one-variable control: only the document changes from the arm above. No
/// broker master secret is supplied in ANY gateway arm, so a gateway with valid
/// key material walks PAST this fence and stops at that one instead - which is
/// what makes this positive evidence rather than a differently-shaped failure.
#[test]
fn valid_key_material_walks_past_the_fence() {
    let fixture = Fixture::new();
    let (key, gateway_x) = gateway_key(&fixture);
    let control_x = public_x(&signing_key(0x11));
    let peers = fixture.write(
        "service-peers.json",
        &document(&[
            peer_entry(CONTROL_SERVICE_NAME, &control_x),
            peer_entry(GATEWAY_SERVICE_NAME, &gateway_x),
        ]),
    );
    let output = gateway(Some(&key), Some(&peers));
    let text = combined(&output);

    assert!(
        !text.contains(SERVICE_KEY_REFUSAL),
        "the gateway refused VALID key material:\n{text}"
    );
    assert!(
        text.contains(LATER_REFUSAL),
        "the gateway did not reach the broker secret, so it never passed the fence:\n{text}"
    );
}

/// A serving gateway child on a chosen port: a full boot configuration with
/// valid service identity and a broker master secret. Its tracing stream is
/// piped for the caller to read and its `stderr` is captured; killed when
/// dropped.
struct Serving {
    child: Child,
    stderr: PathBuf,
    /// The fixture directory holding the service key, peer document and broker
    /// secret the child reads; kept alive for the child's whole run.
    _fixture: Fixture,
}

impl Serving {
    /// Spawn the gateway on `port`. The boot reaches the bind without a dialled
    /// database: the DSN is carried for the request path's pool and never opened
    /// at startup.
    fn start(port: u16) -> Self {
        let fixture = Fixture::new();
        let (key, gateway_x) = gateway_key(&fixture);
        let control_x = public_x(&signing_key(0x11));
        let peers = fixture.write(
            "service-peers.json",
            &document(&[
                peer_entry(CONTROL_SERVICE_NAME, &control_x),
                peer_entry(GATEWAY_SERVICE_NAME, &gateway_x),
            ]),
        );
        let broker = fixture.write("broker-master", STRONG);
        let stderr = fixture.0.join("gateway.err");
        let sink = std::fs::File::create(&stderr).expect("gateway stderr log");
        let child = Command::new(env!("CARGO_BIN_EXE_zeroship-gate"))
            .env_clear()
            .env("ZEROSHIP_CONTROL_KEY", "control-key-material")
            .env("ZEROSHIP_GATEWAY_STASH_SIGNING_KEY", STRONG)
            .env("ZEROSHIP_PAIRWISE_SALT", STRONG)
            .env(
                "ZEROSHIP_GATEWAY_DATABASE_URL",
                "postgres://u:p@127.0.0.1:1/unreached",
            )
            .env("ZEROSHIP_GATEWAY_SERVICE_KEY_FILE", &key)
            .env("ZEROSHIP_GATEWAY_SERVICE_PEERS_FILE", &peers)
            .env("ZEROSHIP_GATEWAY_BROKER_SECRET_FILE", &broker)
            .args(["--port", &port.to_string(), "--worker-urls", "http://127.0.0.1:1"])
            // Pin the announcement's format so this reader does not depend on
            // the child's stderr being a non-terminal.
            .args(["--observability-log-format", "json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::from(sink))
            .spawn()
            .expect("spawn zeroship-gate");
        Self {
            child,
            stderr,
            _fixture: fixture,
        }
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// How a watch of a child's tracing stream ended.
enum Announce {
    /// The listening line was read, naming its advertised port.
    Port(u16),
    /// The stream ended first, carrying everything read.
    Ended(String),
}

/// Read `stdout` on a background thread until it announces `marker`, then keep
/// draining so the child never blocks on a full pipe. The test thread waits on
/// the returned channel with `recv_timeout`, so a child that stalls fails by
/// timeout rather than hanging the run, and nothing polls on a sleep.
fn await_announcement(stdout: ChildStdout, marker: &'static str) -> mpsc::Receiver<Announce> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut log = String::new();
        let mut announced = false;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    if !announced {
                        let _ = sender.send(Announce::Ended(log));
                    }
                    return;
                }
                Ok(_) => {
                    if !announced {
                        if let Some(port) = announced_port(&line, marker) {
                            let _ = sender.send(Announce::Port(port));
                            announced = true;
                        }
                    }
                    log.push_str(&line);
                }
                Err(error) => {
                    let _ = sender.send(Announce::Ended(format!("{log}\nread error: {error}")));
                    return;
                }
            }
        }
    });
    receiver
}

/// The port a single log line announced, if it carried `marker`.
///
/// The tracing line is JSON on a non-TTY stream; a plain `bind=host:port`
/// spelling is accepted too, so this reader is not pinned to one format.
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

/// A port another process holds is not this gateway listening: the gateway must
/// stop on the collision without ever claiming it is listening.
#[test]
fn a_held_port_is_never_announced_as_listening() {
    let foreign = std::net::TcpListener::bind("127.0.0.1:0").expect("hold a port");
    let port = foreign.local_addr().expect("the held port").port();
    let mut serving = Serving::start(port);
    let announcement = await_announcement(
        serving.child.stdout.take().expect("piped stdout"),
        "gateway listening",
    );
    match announcement.recv_timeout(Duration::from_mins(1)) {
        Ok(Announce::Port(port)) => panic!(
            "the gateway claimed to be listening on port {port} it never bound; stderr:\n{}",
            serving.stderr()
        ),
        Ok(Announce::Ended(log)) => assert!(
            !log.contains("gateway listening"),
            "the gateway claimed to be listening on a port it never bound:\n{log}"
        ),
        Err(error) => panic!(
            "the gateway never stopped on the port another process holds ({error}); stderr:\n{}",
            serving.stderr()
        ),
    }
    assert!(
        serving.stderr().contains("Address already in use"),
        "the gateway did not name the address collision:\n{}",
        serving.stderr()
    );
    drop(foreign);
}

/// A `--port 0` boot announces the port it actually bound, and the process
/// really listens there. The socket is in the LISTEN state before the line is
/// logged, so the ownership check after the announcement races nothing.
#[test]
fn a_zero_port_boot_announces_the_port_it_bound() {
    let mut serving = Serving::start(0);
    let announcement = await_announcement(
        serving.child.stdout.take().expect("piped stdout"),
        "gateway listening",
    );
    let port = match announcement.recv_timeout(Duration::from_mins(1)) {
        Ok(Announce::Port(port)) => port,
        Ok(Announce::Ended(log)) => panic!(
            "the gateway exited before announcing a bound port:\n{log}\nstderr:\n{}",
            serving.stderr()
        ),
        Err(error) => panic!(
            "the gateway never announced a bound port ({error}); stderr:\n{}",
            serving.stderr()
        ),
    };
    assert_ne!(port, 0, "a --port 0 boot announced the requested port, not the bound one");
    assert!(
        zeroship_testkit::listen::child_listens(serving.child.id(), port),
        "the gateway announced port {port} but does not own a listening socket on it; stderr:\n{}",
        serving.stderr()
    );
}