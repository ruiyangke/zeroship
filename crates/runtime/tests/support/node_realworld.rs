#![allow(dead_code)]

use std::net::{TcpStream as StdTcpStream, ToSocketAddrs};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

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

