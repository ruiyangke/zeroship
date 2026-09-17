//! Control's first SERVING boot: the real binary, an owned migrated database,
//! and a platform issuer it can verify tokens against.
//!
//! Every other process test in this crate asserts a REFUSAL. This one asserts
//! the opposite - that control comes up and answers its readiness probe - which
//! is the primitive the cross-service journeys need and an in-process router
//! cannot supply.
//!
//! Four inputs are required and nothing else: the control key, a pairwise salt,
//! the master key (each decoding to the floor its validator enforces), and the
//! platform issuer.

mod common;

#[path = "workflow_support/postgres.rs"]
mod workflow_postgres;

use std::io::{Read as _, Write as _};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;
use ed25519_dalek::pkcs8::EncodePrivateKey as _;
use zeroship_core::service_peers::{
    CONTROL_SERVICE_NAME, GATEWAY_SERVICE_NAME, SERVICE_TRUST_DOMAIN, WORKER_SERVICE_NAME,
};

/// A distinct Ed25519 key per service. The loader refuses a document that maps
/// two issuers onto one key, so fixed but DIFFERENT seeds keep the peer bundle a
/// well-formed SET rather than a collision.
fn signing_key(seed: u8) -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
}

/// The RFC 7517 `x` member: the raw public half, base64url and unpadded.
fn public_x(key: &ed25519_dalek::SigningKey) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes())
}

fn peer_entry(service: &str, x: &str) -> String {
    format!(
        r#"{{"kty":"OKP","crv":"Ed25519","iss":"spiffe://{SERVICE_TRUST_DOMAIN}/{service}","x":"{x}"}}"#
    )
}

/// 64 hex characters, so every credential decodes to 32 bytes and clears the
/// master-key and salt floors. A shorter string looks generous and decodes
/// below them, which the refusal reports by SUBSYSTEM rather than by length.
const STRONG_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The issuer behind the JWKS fixture. Control derives
/// `{issuer}/.well-known/jwks.json`, and the fixture exposes only the full path.
fn issuer_of(jwks_url: &str) -> String {
    jwks_url
        .strip_suffix("/.well-known/jwks.json")
        .expect("the platform fixture serves the well-known path")
        .to_owned()
}

/// Ask control for `path` over a raw socket, returning its status and body.
/// A readiness question needs no HTTP client, and keeping the BODY is what makes
/// a "not ready" answer diagnosable rather than a bare 503.
///
/// A read timeout ends the read rather than failing it. ntex keeps the
/// connection alive, so waiting for EOF stalls to the timeout; treating that as
/// an error would discard the bytes that already carried the status line.
fn get(port: u16, path: &str) -> Option<(u16, String)> {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .ok()?;
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(read) => response.extend_from_slice(&buf[..read]),
        }
    }
    let response = String::from_utf8_lossy(&response).into_owned();
    let status = response.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, response))
}

/// A free loopback port for the child to bind.
///
/// Control logs the address it was CONFIGURED with rather than the one it
/// bound, so `--port 0` announces `127.0.0.1:0` and the real port cannot be
/// recovered from the log. The port comes from a probe listener instead, which
/// the caller releases immediately before spawning; a collision fails loudly
/// rather than silently, printing the log that names the address in use.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("probe listener");
    listener
        .local_addr()
        .expect("probe listener address")
        .port()
}

#[test]
fn control_serves_readyz_on_an_owned_database() {
    let database = workflow_postgres::Database::new();
    let platform = common::PlatformJwks::start();
    let scratch = tempfile::tempdir().expect("scratch directory");

    // The service-identity material a BOOT requires, which a `--check-config`
    // dry run does not: a dry run establishes each credential's source without
    // I/O, so it passes while the boot refuses on this later gate.
    let keys = scratch.path().join("keys");
    std::fs::create_dir_all(&keys).expect("key directory");
    let service_key = signing_key(0x22);
    let der = service_key.to_pkcs8_der().expect("encode the service key");
    let key_path = keys.join("svc-control.pem");
    std::fs::write(&key_path, der.as_bytes()).expect("service key file");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
            .expect("owner-only service key");
    }
    let peers_path = keys.join("service-peers.json");
    std::fs::write(
        &peers_path,
        format!(
            r#"{{"keys":[{},{},{}]}}"#,
            peer_entry(CONTROL_SERVICE_NAME, &public_x(&service_key)),
            peer_entry(GATEWAY_SERVICE_NAME, &public_x(&signing_key(0x33))),
            peer_entry(WORKER_SERVICE_NAME, &public_x(&signing_key(0x44))),
        ),
    )
    .expect("service peers file");

    let port = free_port();
    let log_path = scratch.path().join("control.log");
    let sink = std::fs::File::create(&log_path).expect("control log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_zeroship-control"))
        .env_clear()
        .env("ZEROSHIP_CONTROL_KEY", STRONG_HEX)
        .env("ZEROSHIP_PAIRWISE_SALT", STRONG_HEX)
        .env("ZEROSHIP_CONTROL_MASTER_KEY", STRONG_HEX)
        .env("ZEROSHIP_CONTROL_DATABASE_URL", database.url())
        .arg("--no-config")
        .arg("--auth-platform-issuer")
        .arg(issuer_of(&platform.jwks_url()))
        .arg("--service-key-file")
        .arg(&key_path)
        .arg("--service-peers-file")
        .arg(&peers_path)
        // The default billing provider is evaluation-grade and refuses to run
        // without an explicit opt-in. This suite exercises serving, not billing,
        // so it takes the documented escape rather than standing up a provider.
        .arg("--allow-unsupported-billing")
        .arg("--port")
        .arg(port.to_string())
        .arg("--blob-store")
        .arg(scratch.path().join("blobs"))
        .arg("--deploy-tmp-dir")
        .arg(scratch.path().join("deploy"))
        .stdout(Stdio::from(sink.try_clone().expect("clone the log")))
        .stderr(Stdio::from(sink))
        .spawn()
        .expect("spawn zeroship-control");

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last = None;
    while Instant::now() < deadline {
        last = get(port, "/readyz");
        if last.as_ref().is_some_and(|(status, _)| *status == 200) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    // Control's routes are /api, /internal, /healthz and /readyz. The edge sends
    // the whole /v1/* namespace to the migration service, so a route here would
    // be shadowed and unreachable: an unknown path and a /v1 path must answer
    // identically. Probed while the child is still serving.
    let unknown = get(port, "/no-such-route").expect("control answers an unknown path");
    let v1 = get(port, "/v1/apps").expect("control answers a /v1 path");

    let _ = child.kill();
    let _ = child.wait();
    let (status, body) = last.unwrap_or((0, "no response".to_owned()));
    let described = if status == 0 {
        "NO RESPONSE AT ALL: connect, write or read produced nothing".to_owned()
    } else {
        format!("status {status}, body:\n{body}")
    };
    assert_eq!(
        status,
        200,
        "control never answered /readyz with 200: {described}\nLog:\n{}",
        std::fs::read_to_string(&log_path).unwrap_or_default()
    );
    assert_eq!(
        v1.0, unknown.0,
        "control must declare no /v1 route: the edge sends the whole /v1/* namespace to the \
         migration service, so one here would be shadowed and unreachable.\n  \
         /v1/apps -> {v1:?}\n  /no-such-route -> {unknown:?}"
    );
}
