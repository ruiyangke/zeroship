use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::{
    EgressRule, EnvSnapshot, FetchOutcome, ModuleEntry, NetPolicy, RequestCtx, Runtime,
    SettledFetch, Verdict,
};

pub const LOCALHOST: &str = "127.0.0.1";

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub fn lock_env() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

/// The two runtime settings these six modules share: dev mode ON (so the SSRF
/// fast path admits the loopback fixture servers) and the process-wide socket
/// ceiling at its default (so a cap a sibling test lowered cannot refuse
/// connections here). Both are runtime cells, so restoring them means storing
/// back the value read at construction rather than putting an environment
/// variable back.
///
/// The cells are process-wide, which is exactly why [`lock_env`] still exists
/// and every user of this guard still takes it. What changed is the mechanism:
/// `std::env::set_var` races concurrent libc `getenv` (undefined behaviour),
/// an atomic store does not.
pub struct SettingsGuard {
    prev_dev: bool,
    prev_global_max_sockets: u32,
}

impl SettingsGuard {
    pub fn set_dev() -> Self {
        let prev_dev = zeroship_runtime::dev_mode_enabled();
        let prev_global_max_sockets = zeroship_runtime::global_max_sockets();
        zeroship_runtime::set_dev_mode(true);
        zeroship_runtime::set_global_max_sockets(DEFAULT_GLOBAL_MAX_SOCKETS);
        Self {
            prev_dev,
            prev_global_max_sockets,
        }
    }
}

/// Mirrors `DEFAULT_GLOBAL_MAX_SOCKETS` in
/// `crates/zeroship-runtime/src/transport/net_policy.rs`. These modules want the cap
/// out of the way, not a specific number; a divergence would only mean a
/// larger or smaller "effectively unlimited".
const DEFAULT_GLOBAL_MAX_SOCKETS: u32 = 4096;

impl Drop for SettingsGuard {
    fn drop(&mut self) {
        zeroship_runtime::set_dev_mode(self.prev_dev);
        zeroship_runtime::set_global_max_sockets(self.prev_global_max_sockets);
    }
}

pub struct JsResult {
    pub status: u16,
    pub body: String,
}

pub fn module(specifier: impl Into<String>, source: impl Into<String>) -> ModuleEntry {
    ModuleEntry {
        specifier: specifier.into(),
        source: source.into(),
    }
}

pub fn allowlist(host: &str, port: u16, max_sockets: u32, egress_ceiling: u64) -> NetPolicy {
    NetPolicy::rules(
        vec![accept_target(host, port)],
        max_sockets,
        egress_ceiling,
    )
    .unwrap()
}

pub async fn run_js(
    module_src: String,
    mut extra_modules: Vec<ModuleEntry>,
    policy: NetPolicy,
    max_wait: Duration,
) -> JsResult {
    let mut modules = vec![module("index.js", module_src)];
    modules.append(&mut extra_modules);
    let runtime = Runtime::builder()
        .modules(modules)
        .net_policy(policy)
        .build();
    runtime.start_pump();

    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);
    drive_fetch_outcome(outcome, max_wait).await
}

async fn drive_fetch_outcome(outcome: FetchOutcome, max_wait: Duration) -> JsResult {
    match outcome {
        FetchOutcome::Response { status, body, .. } => JsResult {
            status,
            body: String::from_utf8_lossy(&body).into_owned(),
        },
        FetchOutcome::Pending { rx, .. } => match compio::time::timeout(max_wait, rx.recv()).await {
            Ok(Ok(SettledFetch::Response { status, body, .. })) => JsResult {
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            },
            Ok(Ok(SettledFetch::Stream { status, body_reader, .. })) => {
                let mut body = Vec::new();
                for chunk in body_reader.drain() {
                    body.extend_from_slice(&chunk);
                }
                JsResult {
                    status,
                    body: String::from_utf8_lossy(&body).into_owned(),
                }
            }
            Ok(Ok(SettledFetch::WebSocketUpgrade { .. })) => JsResult {
                status: 500,
                body: "unexpected websocket upgrade".to_string(),
            },
            Ok(Err(e)) => JsResult {
                status: 500,
                body: format!("ERR: {}", e.message),
            },
            Err(_) => JsResult {
                status: 599,
                body: "TIMEOUT".to_string(),
            },
        },
        FetchOutcome::Stream {
            status,
            body_reader,
            ..
        } => {
            let mut body = Vec::new();
            for chunk in body_reader.drain() {
                body.extend_from_slice(&chunk);
            }
            JsResult {
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            }
        }
        FetchOutcome::WebSocketUpgrade { .. } => JsResult {
            status: 500,
            body: "unexpected websocket upgrade".to_string(),
        },
    }
}

/// A live server one case holds for its whole run.
///
/// Whatever keeps the server is released when the case drops this: a database
/// of the case's own on a server every test process of the worktree shares, or
/// a container the case started. Bind it to a name at the top of the case, so it
/// outlives the requests that use it.
pub struct ServerInfo {
    pub host: &'static str,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
    _held: Box<dyn std::any::Any>,
}

pub struct TlsPostgresInfo {
    pub host: &'static str,
    pub port: u16,
    pub ca_pem: String,
    pub wrong_ca_pem: String,
    _container: Container<GenericImage>,
}

struct PgTlsCertFiles {
    ca_pem: String,
    server_cert_pem: String,
    server_key_pem: String,
    wrong_ca_pem: String,
}

/// A database of this case's own on the bare PostgreSQL server every test
/// process of the worktree shares, dropped with the returned handle.
pub fn ensure_pg_migrate_postgres() -> ServerInfo {
    let database = zeroship_testkit::postgres::server::Postgres::start();
    ServerInfo {
        host: LOCALHOST,
        port: database.port(),
        user: database.user().to_owned(),
        password: database.password().to_owned(),
        database: database.name().to_owned(),
        _held: Box::new(database),
    }
}

/// A TLS-only PostgreSQL server this case starts and removes when it drops the
/// returned handle.
///
/// Its configuration - certificates this run generated, a `pg_hba.conf` that
/// refuses plaintext - is the subject of the one case that uses it, so no
/// other case could share it.
pub fn ensure_tls_postgres() -> TlsPostgresInfo {
    let (certs, _) = ensure_pg_tls_cert_files();
    let container = GenericImage::new("postgres", "16")
        .with_exposed_port(5432.tcp())
        // The image's entrypoint initialises the data directory behind a
        // temporary server, which accepts no TCP connection, and then starts
        // the final one. The init-complete line on standard output is printed
        // between the two; the ready line on standard error is the final
        // server's, because the temporary server's output reaches standard
        // output through `pg_ctl`.
        .with_wait_for(WaitFor::message_on_stdout(
            "PostgreSQL init process complete; ready for start up.",
        ))
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_entrypoint("bash")
        .with_env_var("POSTGRES_PASSWORD", "zeroship")
        .with_env_var("POSTGRES_DB", "postgres")
        .with_env_var(
            "ZS_PG_TLS_CERT_B64",
            BASE64.encode(certs.server_cert_pem.as_bytes()),
        )
        .with_env_var(
            "ZS_PG_TLS_KEY_B64",
            BASE64.encode(certs.server_key_pem.as_bytes()),
        )
        .with_cmd(["-ceu".to_string(), TLS_BOOTSTRAP.to_string()])
        .with_startup_timeout(Duration::from_secs(120))
        .start()
        .expect("TLS PostgreSQL e2e tests require Docker");
    let port = container
        .get_host_port_ipv4(5432)
        .expect("mapped TLS PostgreSQL port");
    TlsPostgresInfo {
        host: LOCALHOST,
        port,
        ca_pem: certs.ca_pem,
        wrong_ca_pem: certs.wrong_ca_pem,
        _container: container,
    }
}

/// A database of this case's own on the MySQL server every test process of the
/// worktree shares, dropped with the returned handle.
pub fn ensure_mysql() -> ServerInfo {
    let server = zeroship_testkit::mysql::server();
    let database = server
        .case_database()
        .expect("create this case's database on the shared MySQL server");
    ServerInfo {
        host: server.host(),
        port: server.port(),
        user: server.user().to_owned(),
        password: server.password().to_owned(),
        database: database.name().to_owned(),
        _held: Box::new(database),
    }
}

/// The standalone Redis server of the shared Redis fixture, held for this case.
pub fn ensure_redis() -> ServerInfo {
    let fixtures = zeroship_testkit::redis::fixtures();
    let url = url::Url::parse(&fixtures.redis_url()).expect("the Redis fixture URL parses");
    let port = url.port().expect("the Redis fixture URL names its port");
    ServerInfo {
        host: LOCALHOST,
        port,
        user: String::new(),
        password: String::new(),
        database: String::new(),
        _held: Box::new(fixtures),
    }
}

/// A memcached server this case starts and removes when it drops the returned
/// handle.
pub fn ensure_memcached() -> ServerInfo {
    let container = GenericImage::new("memcached", "1.6")
        .with_exposed_port(11211.tcp())
        .with_wait_for(WaitFor::message_on_stderr("server listening"))
        // The default verbosity does not print the listening line the wait
        // condition matches, so the server is started verbose.
        .with_cmd(["-vv"])
        .with_startup_timeout(Duration::from_secs(60))
        .start()
        .expect("memcached e2e tests require Docker");
    let port = container
        .get_host_port_ipv4(11211)
        .expect("mapped memcached port");
    ServerInfo {
        host: LOCALHOST,
        port,
        user: String::new(),
        password: String::new(),
        database: String::new(),
        _held: Box::new(container),
    }
}

fn ensure_pg_tls_cert_files() -> (PgTlsCertFiles, bool) {
    let dir = PathBuf::from("/tmp/zeroship-runtime-pg-tls-e2e");
    let ca_path = dir.join("ca.pem");
    let cert_path = dir.join("server.pem");
    let key_path = dir.join("server.key");
    let wrong_ca_path = dir.join("wrong-ca.pem");

    if ca_path.exists() && cert_path.exists() && key_path.exists() && wrong_ca_path.exists() {
        return (
            PgTlsCertFiles {
                ca_pem: read_file(&ca_path),
                server_cert_pem: read_file(&cert_path),
                server_key_pem: read_file(&key_path),
                wrong_ca_pem: read_file(&wrong_ca_path),
            },
            false,
        );
    }

    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)
        .unwrap_or_else(|err| panic!("failed to create TLS Postgres cert dir {dir:?}: {err}"));
    let certs = generate_pg_tls_certs();
    write_file(&ca_path, &certs.ca_pem);
    write_file(&cert_path, &certs.server_cert_pem);
    write_file(&key_path, &certs.server_key_pem);
    write_file(&wrong_ca_path, &certs.wrong_ca_pem);
    (certs, true)
}

fn generate_pg_tls_certs() -> PgTlsCertFiles {
    let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "zeroship pg tls e2e root");
    ca_params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    ca_params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    ca_params.key_usages.push(KeyUsagePurpose::CrlSign);
    let ca_key = KeyPair::generate().unwrap();
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);

    let leaf_key = KeyPair::generate().unwrap();
    let mut leaf_params =
        CertificateParams::new(vec!["localhost".to_string(), LOCALHOST.to_string()]).unwrap();
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, "localhost");
    leaf_params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ServerAuth);
    leaf_params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    leaf_params.use_authority_key_identifier_extension = true;
    let leaf_cert = leaf_params.signed_by(&leaf_key, &issuer).unwrap();

    PgTlsCertFiles {
        ca_pem: ca_cert.pem(),
        server_cert_pem: leaf_cert.pem(),
        server_key_pem: leaf_key.serialize_pem(),
        wrong_ca_pem: generate_wrong_ca_pem(),
    }
}

fn generate_wrong_ca_pem() -> String {
    let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "zeroship wrong pg tls e2e root");
    ca_params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    ca_params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    ca_params.key_usages.push(KeyUsagePurpose::CrlSign);
    let ca_key = KeyPair::generate().unwrap();
    ca_params.self_signed(&ca_key).unwrap().pem()
}

/// The bootstrap the TLS Postgres image runs before handing over to the server.
///
/// It decodes the certificates the test generated, writes the `pg_hba.conf`
/// that rejects plaintext and accepts TLS, and starts the server with TLS on.
const TLS_BOOTSTRAP: &str = r#"
set -euo pipefail
cert_dir=/var/lib/postgresql/certs
mkdir -p "$cert_dir"
printf '%s' "$ZS_PG_TLS_CERT_B64" | base64 -d > "$cert_dir/server.crt"
printf '%s' "$ZS_PG_TLS_KEY_B64" | base64 -d > "$cert_dir/server.key"
cat > "$cert_dir/pg_hba.conf" <<'HBA'
local all all trust
hostnossl all all all reject
hostssl all all all trust
HBA
chown -R postgres:postgres "$cert_dir"
chmod 700 "$cert_dir"
chmod 600 "$cert_dir/server.key"
chmod 644 "$cert_dir/server.crt" "$cert_dir/pg_hba.conf"
exec docker-entrypoint.sh postgres \
  -c ssl=on \
  -c ssl_cert_file="$cert_dir/server.crt" \
  -c ssl_key_file="$cert_dir/server.key" \
  -c hba_file="$cert_dir/pg_hba.conf"
"#;

fn read_file(path: &std::path::Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|err| panic!("failed to read {path:?}: {err}"))
}

fn write_file(path: &std::path::Path, contents: &str) {
    fs::write(path, contents).unwrap_or_else(|err| panic!("failed to write {path:?}: {err}"));
}


/// Build an ACCEPT rule for a `node:net` test target.
///
/// These tests target literal addresses, and an IP literal is NOT a
/// representable `Name` - it must be written as a range, so a reader of a rule
/// always knows which check decides it. `is_blocked_ip` would refuse loopback
/// outright; these tests run with dev mode on (`SettingsGuard::set_dev`),
/// which bypasses the floor and nothing else.
fn accept_target(host: &str, port: u16) -> EgressRule {
    let destination = match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => format!("{v4}/32"),
        Ok(std::net::IpAddr::V6(v6)) => format!("{v6}/128"),
        Err(_) => host.to_string(),
    };
    EgressRule::parse(Verdict::Accept, &destination, port).expect("valid test egress rule")
}
