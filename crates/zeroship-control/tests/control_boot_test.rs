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

/// Every ntex arbiter id the child announced, read out of its own log.
///
/// ntex names each serving thread `{system name}:worker:{id}` and logs it as
/// the arbiter starts, so the SET of ids is the number of threads the pool was
/// built with - and therefore the number of compio io_uring rings this process
/// charges to the per-user locked-memory budget. That is the quantity
/// `control.threads` exists to bound, and reading it back from a live process
/// is the only thing that binds the setting to `HttpServer::workers`: a
/// `--check-config` row is satisfied by a resolver alone.
fn arbiter_ids(log: &str) -> std::collections::BTreeSet<u32> {
    const MARKER: &str = "zeroship-control:worker:";
    let mut ids = std::collections::BTreeSet::new();
    for (offset, _) in log.match_indices(MARKER) {
        let digits: String = log[offset + MARKER.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if let Ok(id) = digits.parse::<u32>() {
            ids.insert(id);
        }
    }
    ids
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
        // ONE SERVING THREAD, and the assertion below reads back how many the
        // process actually built. Each ntex arbiter creates its own compio
        // io_uring runtime, so an unbounded default makes this fixture claim
        // one ring per core from a budget (`ulimit -l`) the whole host shares -
        // which is how a boot on an idle machine fails with
        // `Cannot allocate memory (os error 12)`.
        .arg("--threads")
        .arg("1")
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

    let log = std::fs::read_to_string(&log_path).unwrap_or_default();

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

    // `--threads 1` above, so the pool must hold arbiter 0 and nothing else.
    // Before `control.threads` reached `HttpServer::workers`, ntex sized the
    // pool from the affinity mask and this set was every core on the machine.
    //
    // WHAT THIS DOES NOT CATCH: an unplumbed setting on a single-core host,
    // where the framework default and the requested count coincide. The
    // instrument below - the set is non-empty, so the process really did
    // announce its arbiters - is what separates that from a log this fixture
    // failed to capture at all.
    let ids = arbiter_ids(&log);
    assert!(
        !ids.is_empty(),
        "control announced no ntex arbiter at all, so the count below would be \
         vacuous. Log:\n{log}"
    );
    assert_eq!(
        ids,
        std::collections::BTreeSet::from([0]),
        "control was launched with --threads 1 and built {} serving threads; each is an \
         io_uring ring charged to `ulimit -l`. Log:\n{log}",
        ids.len()
    );
}

/// Control's key and a peer document naming it, as a serving boot requires.
fn service_identity(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(dir).expect("key directory");
    let service_key = signing_key(0x22);
    let key = dir.join("svc-control.pem");
    std::fs::write(
        &key,
        service_key
            .to_pkcs8_der()
            .expect("encode the service key")
            .as_bytes(),
    )
    .expect("service key file");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))
        .expect("owner-only service key");
    let peers = dir.join("service-peers.json");
    std::fs::write(
        &peers,
        format!(
            r#"{{"keys":[{}]}}"#,
            peer_entry(CONTROL_SERVICE_NAME, &public_x(&service_key)),
        ),
    )
    .expect("service peers file");
    (key, peers)
}

/// Every client session connected to one database, by the name it announced,
/// read over a connection to ANOTHER database so the census never counts
/// itself.
struct Census {
    runtime: compio::runtime::Runtime,
    client: compio_postgres::Client,
    database: String,
}

impl Census {
    fn of(url: &str) -> Self {
        let mut admin = url::Url::parse(url).expect("database URL");
        let database = admin.path().trim_start_matches('/').to_owned();
        admin.set_path("/postgres");
        let runtime = compio::runtime::Runtime::new().expect("census runtime");
        let client = runtime.block_on(async {
            let (client, connection) =
                compio_postgres::connect(admin.as_str(), compio_postgres::NoTls)
                    .await
                    .expect("census connection");
            compio::runtime::spawn(async move {
                let _ = connection.run().await;
            })
            .detach();
            client
        });
        Self {
            runtime,
            client,
            database,
        }
    }

    fn sessions(&self) -> Vec<String> {
        self.runtime.block_on(async {
            self.client
                .query(
                    "SELECT coalesce(application_name, '') FROM pg_stat_activity \
                      WHERE datname = $1 AND backend_type = 'client backend'",
                    &[&self.database],
                )
                .await
                .expect("census query")
                .into_iter()
                .map(|row| row.get(0))
                .collect()
        })
    }
}

/// A serving Control child, killed when dropped.
struct Serving {
    child: std::process::Child,
    port: u16,
    log: std::path::PathBuf,
}

impl Serving {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What one boot held once all its serving threads were up.
struct Held {
    /// The most sessions any sample held, request connections aside.
    most: usize,
    /// The last sample's sessions, by name.
    last: std::collections::BTreeMap<String, usize>,
    /// Every name any sample saw.
    names: std::collections::BTreeSet<String>,
}

const CATALOG_SESSIONS: usize = 1;
const RETENTION_SESSIONS: usize = 1;

/// Boot Control on a fresh clone with `threads` serving threads and the
/// smallest executors, wait until every serving thread has started, then
/// sample its sessions over a window.
fn boot_and_count(threads: usize) -> Held {
    use zeroship_control::sessions;
    let database = workflow_postgres::Database::new();
    let census = Census::of(&database.url());
    // The control: before Control starts, the census sees nothing, so it does
    // not count its own connection.
    assert_eq!(census.sessions(), Vec::<String>::new());
    let platform = common::PlatformJwks::start();
    let scratch = tempfile::tempdir().expect("scratch directory");
    let (key, peers) = service_identity(&scratch.path().join("keys"));
    let port = free_port();
    let log = scratch.path().join("control.log");
    let sink = std::fs::File::create(&log).expect("control log");
    let serving = Serving {
        child: Command::new(env!("CARGO_BIN_EXE_zeroship-control"))
            .env_clear()
            .env("ZEROSHIP_CONTROL_KEY", STRONG_HEX)
            .env("ZEROSHIP_PAIRWISE_SALT", STRONG_HEX)
            .env("ZEROSHIP_CONTROL_MASTER_KEY", STRONG_HEX)
            .env("ZEROSHIP_CONTROL_DATABASE_URL", database.url())
            .arg("--no-config")
            .arg("--auth-platform-issuer")
            .arg(issuer_of(&platform.jwks_url()))
            .arg("--service-key-file")
            .arg(&key)
            .arg("--service-peers-file")
            .arg(&peers)
            .arg("--allow-unsupported-billing")
            // Each serving thread and each executor lane is an io_uring ring
            // charged to `ulimit -l`, so the executors take one lane each.
            .arg("--threads")
            .arg(threads.to_string())
            .arg("--catalog-max-connections")
            .arg(CATALOG_SESSIONS.to_string())
            .arg("--retention-max-connections")
            .arg(RETENTION_SESSIONS.to_string())
            .arg("--port")
            .arg(port.to_string())
            .arg("--blob-store")
            .arg(scratch.path().join("blobs"))
            .arg("--deploy-tmp-dir")
            .arg(scratch.path().join("deploy"))
            .stdout(Stdio::from(sink.try_clone().expect("clone the log")))
            .stderr(Stdio::from(sink))
            .spawn()
            .expect("spawn zeroship-control"),
        port,
        log,
    };

    let deadline = Instant::now() + Duration::from_secs(60);
    while get(serving.port, "/readyz").is_none_or(|(status, _)| status != 200) {
        assert!(
            Instant::now() < deadline,
            "control --threads {threads} never answered /readyz with 200. Log:\n{}",
            serving.log()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    // Every serving thread has started, so a session any thread opens as it
    // starts is already open when the census counts.
    let expected: std::collections::BTreeSet<u32> =
        (0..u32::try_from(threads).expect("thread count")).collect();
    while arbiter_ids(&serving.log()) != expected {
        assert!(
            Instant::now() < deadline,
            "control --threads {threads} announced arbiters {:?}. Log:\n{}",
            arbiter_ids(&serving.log()),
            serving.log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let mut held = Held {
        most: 0,
        last: std::collections::BTreeMap::new(),
        names: std::collections::BTreeSet::new(),
    };
    for _ in 0..20 {
        let sample = census.sessions();
        held.most = held
            .most
            .max(sample.iter().filter(|name| *name != sessions::REQUEST).count());
        held.last = std::collections::BTreeMap::new();
        for name in sample {
            *held.last.entry(name.clone()).or_default() += 1;
            held.names.insert(name);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // A request connection lives for one call. One still open once the calls
    // that could hold it have finished is a session kept per thread or per
    // process under the request name, which the bound above does not count.
    let drain = Instant::now() + Duration::from_secs(30);
    loop {
        let open = census
            .sessions()
            .into_iter()
            .filter(|name| name == sessions::REQUEST)
            .count();
        if open == 0 {
            break;
        }
        assert!(
            Instant::now() < drain,
            "control --threads {threads} still holds {open} request connections after its \
             boot-time calls finished; a request connection lives only for its call"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    held
}

/// Control's PostgreSQL sessions do not grow with its serving threads.
///
/// It holds one shared session, the catalog's and the retention executor's,
/// and nothing per thread: at most `1 + K + H` throughout, exactly that once it
/// has settled, and the same under one serving thread as under four. Request
/// connections come and go with the calls that open them (a cron's first tick
/// runs at boot), so they are attributed by name and left out of the bound,
/// and each boot must drain them to none.
#[test]
fn control_sessions_do_not_scale_with_serving_threads() {
    use zeroship_control::sessions;
    let bound = 1 + CATALOG_SESSIONS + RETENTION_SESSIONS;
    let mut settled = Vec::new();
    for threads in [1, 4] {
        let held = boot_and_count(threads);
        assert!(
            held.most <= bound,
            "control --threads {threads} held {} sessions besides request connections; \
             the process holds at most {bound} however many threads serve: {:?}",
            held.most,
            held.last
        );
        let resident: usize = held
            .last
            .iter()
            .filter(|(name, _)| *name != sessions::REQUEST)
            .map(|(_, count)| count)
            .sum();
        assert_eq!(
            resident, bound,
            "control --threads {threads} settled at {resident} sessions: {:?}",
            held.last
        );
        // The census sees Control's sessions, not only a count: each executor's
        // are there by name, as many as it was given.
        assert_eq!(held.last.get(sessions::SHARED), Some(&1), "{:?}", held.last);
        assert_eq!(
            held.last.get(sessions::CATALOG),
            Some(&CATALOG_SESSIONS),
            "{:?}",
            held.last
        );
        assert_eq!(
            held.last.get(sessions::RETENTION),
            Some(&RETENTION_SESSIONS),
            "{:?}",
            held.last
        );
        // Every session any sample saw came from a source that names itself.
        let sources = [
            sessions::SHARED,
            sessions::CATALOG,
            sessions::RETENTION,
            sessions::REQUEST,
        ];
        assert!(
            held.names.iter().all(|name| sources.contains(&name.as_str())),
            "control --threads {threads} held sessions no source announced: {:?}",
            held.names
        );
        settled.push(resident);
    }
    assert_eq!(
        settled[0], settled[1],
        "control held {} sessions under one serving thread and {} under four",
        settled[0], settled[1]
    );
}
