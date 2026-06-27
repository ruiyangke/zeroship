#![allow(dead_code)]

use std::fs;
use std::net::{TcpStream as StdTcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::{
    EnvSnapshot, FetchOutcome, HostPort, ModuleEntry, NetPolicy, RequestCtx, Runtime, SettledFetch,
};

pub const LOCALHOST: &str = "127.0.0.1";

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub fn lock_env() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

pub struct EnvGuard {
    prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    pub fn set_dev() -> Self {
        let keys = ["ZEROSHIP_DEV", "ZEROSHIP_NET_GLOBAL_MAX_SOCKETS"];
        let prev = keys
            .into_iter()
            .map(|key| (key, std::env::var_os(key)))
            .collect::<Vec<_>>();
        unsafe {
            std::env::set_var("ZEROSHIP_DEV", "1");
            std::env::remove_var("ZEROSHIP_NET_GLOBAL_MAX_SOCKETS");
        }
        Self { prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            for (key, value) in &self.prev {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
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
    NetPolicy::allowlist(
        vec![HostPort::new(host, port)],
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
        FetchOutcome::Response { status, body, .. } => JsResult { status, body },
        FetchOutcome::Pending { rx, .. } => match compio::time::timeout(max_wait, rx.recv()).await {
            Ok(Ok(SettledFetch::Response { status, body, .. })) => JsResult { status, body },
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

#[derive(Clone, Debug)]
pub struct ServerInfo {
    pub host: &'static str,
    pub port: u16,
    pub source: String,
}

#[derive(Clone, Debug)]
pub struct TlsPostgresInfo {
    pub host: &'static str,
    pub port: u16,
    pub source: String,
    pub ca_pem: String,
    pub wrong_ca_pem: String,
}

struct PgTlsCertFiles {
    ca_pem: String,
    server_cert_pem: String,
    server_key_pem: String,
    wrong_ca_pem: String,
}

pub fn ensure_pg_migrate_postgres() -> ServerInfo {
    const PORT: u16 = 5440;
    if tcp_reachable(LOCALHOST, PORT) {
        return ServerInfo {
            host: LOCALHOST,
            port: PORT,
            source: "existing 127.0.0.1:5440".to_string(),
        };
    }

    run_docker(&["start", "appbase-migrate-postgres-1"])
        .unwrap_or_else(|err| panic!("failed to start appbase-migrate-postgres-1 for pg e2e: {err}"));
    wait_for_port(LOCALHOST, PORT, Duration::from_secs(45), "migrate Postgres");
    ServerInfo {
        host: LOCALHOST,
        port: PORT,
        source: "docker start appbase-migrate-postgres-1".to_string(),
    }
}

pub fn ensure_tls_postgres() -> TlsPostgresInfo {
    const NAME: &str = "zeroship-runtime-pg-tls-e2e";
    const PORT: u16 = 5441;

    let (certs, regenerated) = ensure_pg_tls_cert_files();
    if regenerated {
        let _ = run_docker(&["rm", "-f", NAME]);
    }

    if tcp_reachable(LOCALHOST, PORT) {
        let running = run_docker(&["inspect", "-f", "{{.State.Running}}", NAME]).ok();
        assert_eq!(
            running.as_deref(),
            Some("true"),
            "TLS Postgres e2e port {LOCALHOST}:{PORT} is reachable, but expected docker \
             container {NAME} is not running; free the port or start the expected container"
        );
        return TlsPostgresInfo {
            host: LOCALHOST,
            port: PORT,
            source: format!("existing docker container {NAME}"),
            ca_pem: certs.ca_pem,
            wrong_ca_pem: certs.wrong_ca_pem,
        };
    }

    if !regenerated && run_docker(&["start", NAME]).is_ok() {
        wait_for_port(LOCALHOST, PORT, Duration::from_secs(45), "TLS Postgres");
    } else {
        run_tls_postgres_container(NAME, PORT, &certs);
        wait_for_port(LOCALHOST, PORT, Duration::from_secs(120), "TLS Postgres");
    }

    wait_for_docker_success(
        NAME,
        &["pg_isready", "-U", "postgres", "-d", "postgres"],
        Duration::from_secs(120),
        "TLS Postgres readiness",
    );

    TlsPostgresInfo {
        host: LOCALHOST,
        port: PORT,
        source: format!("docker container {NAME}"),
        ca_pem: certs.ca_pem,
        wrong_ca_pem: certs.wrong_ca_pem,
    }
}

pub fn ensure_mysql() -> ServerInfo {
    const NAME: &str = "zeroship-runtime-mysql2-e2e";
    const PORT: u16 = 3307;
    if tcp_reachable(LOCALHOST, PORT) {
        return ServerInfo {
            host: LOCALHOST,
            port: PORT,
            source: "existing 127.0.0.1:3307".to_string(),
        };
    }

    if run_docker(&["start", NAME]).is_err() {
        run_docker(&[
            "run",
            "-d",
            "--name",
            NAME,
            "-e",
            "MYSQL_ROOT_PASSWORD=zeroship",
            "-e",
            "MYSQL_DATABASE=zeroship_e2e",
            "-p",
            "127.0.0.1:3307:3306",
            "mysql:8",
        ])
        .unwrap_or_else(|err| panic!("failed to docker run mysql:8 for mysql2 e2e: {err}"));
    }

    wait_for_port(LOCALHOST, PORT, Duration::from_secs(120), "MySQL");
    wait_for_docker_success(
        NAME,
        &["mysqladmin", "ping", "-h", "127.0.0.1", "-uroot", "-pzeroship", "--silent"],
        Duration::from_secs(120),
        "MySQL readiness",
    );
    ServerInfo {
        host: LOCALHOST,
        port: PORT,
        source: format!("docker container {NAME}"),
    }
}

pub fn ensure_redis() -> ServerInfo {
    const NAME: &str = "zeroship-runtime-redis-e2e";
    const PORT: u16 = 6391;
    if tcp_reachable(LOCALHOST, PORT) {
        return ServerInfo {
            host: LOCALHOST,
            port: PORT,
            source: "existing 127.0.0.1:6391".to_string(),
        };
    }

    if run_docker(&["start", NAME]).is_err() {
        run_docker(&[
            "run",
            "-d",
            "--name",
            NAME,
            "-p",
            "127.0.0.1:6391:6379",
            "redis:7",
        ])
        .unwrap_or_else(|err| panic!("failed to docker run redis:7 for ioredis e2e: {err}"));
    }

    wait_for_port(LOCALHOST, PORT, Duration::from_secs(45), "Redis");
    wait_for_docker_success(
        NAME,
        &["redis-cli", "ping"],
        Duration::from_secs(45),
        "Redis readiness",
    );
    ServerInfo {
        host: LOCALHOST,
        port: PORT,
        source: format!("docker container {NAME}"),
    }
}

pub fn ensure_memcached() -> ServerInfo {
    const NAME: &str = "zeroship-runtime-memjs-e2e";
    const PORT: u16 = 11212;
    if tcp_reachable(LOCALHOST, PORT) {
        let running = run_docker(&["inspect", "-f", "{{.State.Running}}", NAME]).ok();
        assert_eq!(
            running.as_deref(),
            Some("true"),
            "memcached e2e port {LOCALHOST}:{PORT} is reachable, but expected docker \
             container {NAME} is not running; free the port or start the expected container"
        );
        return ServerInfo {
            host: LOCALHOST,
            port: PORT,
            source: format!("existing docker container {NAME}"),
        };
    }

    if run_docker(&["start", NAME]).is_err() {
        run_docker(&[
            "run",
            "-d",
            "--name",
            NAME,
            "-p",
            "127.0.0.1:11212:11211",
            "memcached:1.6",
        ])
        .unwrap_or_else(|err| panic!("failed to docker run memcached:1.6 for memjs e2e: {err}"));
    }

    wait_for_port(LOCALHOST, PORT, Duration::from_secs(45), "memcached");
    ServerInfo {
        host: LOCALHOST,
        port: PORT,
        source: format!("docker container {NAME}"),
    }
}

pub fn tcp_reachable(host: &str, port: u16) -> bool {
    let Ok(mut addrs) = (host, port).to_socket_addrs() else {
        return false;
    };
    addrs.any(|addr| StdTcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok())
}

fn wait_for_port(host: &str, port: u16, timeout: Duration, label: &str) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if tcp_reachable(host, port) {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("{label} did not open {host}:{port} within {timeout:?}");
}

fn wait_for_docker_success(name: &str, exec_args: &[&str], timeout: Duration, label: &str) {
    let start = Instant::now();
    let mut last = String::new();
    while start.elapsed() < timeout {
        let mut args = vec!["exec", name];
        args.extend_from_slice(exec_args);
        match run_docker(&args) {
            Ok(_) => return,
            Err(err) => last = err,
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let logs = run_docker(&["logs", "--tail", "80", name]).unwrap_or_else(|err| err);
    panic!("{label} did not become ready within {timeout:?}; last error: {last}; logs:\n{logs}");
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

fn run_tls_postgres_container(name: &str, port: u16, certs: &PgTlsCertFiles) {
    const BOOTSTRAP: &str = r#"
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

    let cert_env = format!(
        "ZS_PG_TLS_CERT_B64={}",
        BASE64.encode(certs.server_cert_pem.as_bytes())
    );
    let key_env = format!(
        "ZS_PG_TLS_KEY_B64={}",
        BASE64.encode(certs.server_key_pem.as_bytes())
    );
    let publish = format!("127.0.0.1:{port}:5432");
    let args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        name.to_string(),
        "-e".to_string(),
        "POSTGRES_PASSWORD=zeroship".to_string(),
        "-e".to_string(),
        "POSTGRES_DB=postgres".to_string(),
        "-e".to_string(),
        cert_env,
        "-e".to_string(),
        key_env,
        "-p".to_string(),
        publish,
        "--entrypoint".to_string(),
        "bash".to_string(),
        "postgres:16".to_string(),
        "-ceu".to_string(),
        BOOTSTRAP.to_string(),
    ];
    run_docker_owned(&args)
        .unwrap_or_else(|err| panic!("failed to docker run postgres:16 TLS e2e server: {err}"));
}

fn read_file(path: &std::path::Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|err| panic!("failed to read {path:?}: {err}"))
}

fn write_file(path: &std::path::Path, contents: &str) {
    fs::write(path, contents).unwrap_or_else(|err| panic!("failed to write {path:?}: {err}"));
}

fn run_docker_owned(args: &[String]) -> Result<String, String> {
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    run_docker(&refs)
}

fn run_docker(args: &[&str]) -> Result<String, String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .map_err(|err| format!("docker {} failed to spawn: {err}", args.join(" ")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(format!(
            "docker {} exited with status {}\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ))
    }
}
