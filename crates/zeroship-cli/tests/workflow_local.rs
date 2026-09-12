//! Exercise the CLI binding and background worker with a real compiled archive.

use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Host {
    child: Child,
    port: u16,
    log: tempfile::NamedTempFile,
}
impl Host {
    fn start(root: &Path) -> Self {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let log = tempfile::NamedTempFile::new().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_zeroship"));
        command
            .current_dir(root)
            .args(["serve", "app.zship", "--workers=1"])
            .arg(format!("--port={port}"))
            .env("APP_ID", "untrusted-variable")
            .env("ZEROSHIP_WORKFLOW_SQLITE_PATH", root.join("unused.sqlite"))
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.reopen().unwrap()))
            .stderr(Stdio::from(log.reopen().unwrap()));
        for key in [
            "DATABASE_URL",
            "ZEROSHIP_KV_CONFIG_FILE",
            "ZEROSHIP_KV_PATH",
            "ZEROSHIP_STORAGE_URL",
            "ZEROSHIP_DEV",
            "ZEROSHIP_DIE_WITH_PARENT",
            "ZEROSHIP_RUNTIME_DESCRIPTOR",
        ] {
            command.env_remove(key);
        }
        let mut host = Self {
            child: command.spawn().unwrap(),
            port,
            log,
        };
        host.until("/ping", |reply| reply == &json!({"ready": true}));
        host
    }

    fn request(&mut self, path: &str) -> Result<Value, String> {
        if let Some(status) = self.child.try_wait().unwrap() {
            panic!(
                "CLI exited {status}: {}",
                std::fs::read_to_string(self.log.path()).unwrap()
            );
        }
        let mut socket = TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", self.port).parse().unwrap(),
            Duration::from_millis(250),
        )
        .map_err(|error| error.to_string())?;
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            socket,
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut reader = BufReader::new(socket);
        let mut status = String::new();
        reader
            .read_line(&mut status)
            .map_err(|error| error.to_string())?;
        let mut length = None;
        loop {
            let mut header = String::new();
            reader
                .read_line(&mut header)
                .map_err(|error| error.to_string())?;
            if header == "\r\n" {
                break;
            }
            if header.is_empty() {
                return Err("response headers ended early".into());
            }
            if let Some((name, value)) = header.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse::<usize>().ok();
                }
            }
        }
        let length = length
            .filter(|length| *length <= 64 * 1024)
            .ok_or("expected a bounded JSON response")?;
        let mut body = vec![0; length];
        reader
            .read_exact(&mut body)
            .map_err(|error| error.to_string())?;
        if !status.starts_with("HTTP/1.1 200 ") {
            return Err(format!("{status}{}", String::from_utf8_lossy(&body)));
        }
        serde_json::from_slice(&body).map_err(|error| error.to_string())
    }

    fn until(&mut self, path: &str, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let reply = self.request(path);
            if let Ok(reply) = &reply {
                if predicate(reply) {
                    return reply.clone();
                }
            }
            assert!(
                Instant::now() < deadline,
                "CLI did not converge: {reply:?}; {}",
                std::fs::read_to_string(self.log.path()).unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn cli_resumes_a_workflow_from_retained_code_after_process_death() {
    let root = tempfile::tempdir().unwrap();
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().unwrap().parent().unwrap();
    let compiled = Command::new("pnpm")
        .current_dir(workspace.join("sdks/vite-plugin"))
        .args(["exec", "tsx"])
        .arg(manifest.join("tests/fixtures/app-bundle.ts"))
        .arg(root.path())
        .arg("original")
        .output()
        .expect("build the workflow fixture");
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let mut host = Host::start(root.path());
    assert_eq!(
        host.request("/version").unwrap(),
        json!({"version":"original:lazy"})
    );
    assert!(!root.path().join("unused.sqlite").exists());
    assert!(root.path().join(".zeroship/workflows.sqlite").exists());
    let started = host.request("/start").unwrap();
    let run = started["id"].as_str().unwrap();
    let status = format!("/status?id={run}");
    host.until(&status, |value| value["state"] == "waiting");
    let identity = std::fs::read_to_string(root.path().join(".zeroship/app-id")).unwrap();
    assert!(zeroship_core::app_id::AppId::parse(&identity).is_ok());
    assert_ne!(identity, "untrusted-variable");
    drop(host);
    std::fs::remove_dir_all(root.path().join("src")).unwrap();
    let mut host = Host::start(root.path());
    assert_eq!(
        host.request("/version").unwrap(),
        json!({"version":"original:lazy"})
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join(".zeroship/app-id")).unwrap(),
        identity
    );
    host.request(&format!("/signal?id={run}")).unwrap();
    let completed = host.until(&status, |value| value["state"] == "completed");
    assert_eq!(completed["output"], "original:original:lazy");
}

#[test]
fn removed_workflow_bundle_flag_is_rejected() {
    let output = Command::new(env!("CARGO_BIN_EXE_zeroship"))
        .args(["serve", "app.zship", "--workflow-bundle=another.zship"])
        .env_remove("ZEROSHIP_DIE_WITH_PARENT")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown flag `--workflow-bundle`"));
}
