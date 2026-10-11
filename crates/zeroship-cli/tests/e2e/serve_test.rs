//! `zeroship serve` on every system the CLI ships for.
//!
//! The dev server is what a creator runs on their own machine, so the first
//! thing every platform must show is that it boots a plain script and answers a
//! request with what that script returns. The limit a platform cannot enforce
//! is refused at startup, before anything is bound or opened.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_zeroship");

/// An ephemeral loopback port, released at once so the child can bind it.
fn reserve_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("reserve a port")
        .local_addr()
        .expect("read the reserved port")
        .port()
}

/// A running `zeroship serve`, killed when dropped.
struct Served {
    child: Child,
    port: u16,
    log: tempfile::NamedTempFile,
}

impl Served {
    fn log(&self) -> String {
        std::fs::read_to_string(self.log.path()).unwrap_or_default()
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Serve `app.js` from `dir` on a fresh port, relaunching on a new one when a
/// parallel process took the reserved port before the child bound it.
fn serve(dir: &Path) -> Served {
    for _ in 0..16 {
        let port = reserve_port();
        let log = tempfile::NamedTempFile::new().expect("log file");
        let child = Command::new(BIN)
            .args(["serve", "app.js", &format!("--port={port}"), "--workers=1"])
            .env_remove("ZEROSHIP_DIE_WITH_PARENT")
            .env_remove("DATABASE_URL")
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(log.reopen().expect("reopen the log"))
            .stderr(log.reopen().expect("reopen the log"))
            .spawn()
            .expect("spawn zeroship serve");
        let mut served = Served { child, port, log };
        let deadline = Instant::now() + Duration::from_mins(1);
        loop {
            if let Some(status) = served.child.try_wait().expect("poll the child") {
                let log = served.log();
                if log.contains(&format!("port {port} is already in use")) {
                    break;
                }
                panic!("zeroship serve exited with {status} before it listened; log:\n{log}");
            }
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return served;
            }
            assert!(
                Instant::now() < deadline,
                "zeroship serve never listened on {port}; log:\n{}",
                served.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    panic!("every reserved port was taken before zeroship serve bound it");
}

/// One HTTP/1.1 request over a fresh connection, returning the status line and
/// the body its `Content-Length` frames. The server keeps the connection open
/// after answering, so the response ends where its length says, not at EOF.
fn get(port: u16, path: &str) -> (String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("set a read timeout");
    write!(stream, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").expect("send the request");
    let mut received = Vec::new();
    let mut chunk = [0_u8; 4096];
    let head_end = loop {
        if let Some(at) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            break at;
        }
        let read = stream.read(&mut chunk).expect("read the response head");
        assert!(read > 0, "the connection closed before the response head ended");
        received.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&received[..head_end]).into_owned();
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().ok())?
        })
        .unwrap_or_else(|| panic!("the response carries no Content-Length:\n{head}"));
    let mut body = received[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).expect("read the response body");
        assert!(read > 0, "the connection closed before the body ended");
        body.extend_from_slice(&chunk[..read]);
    }
    let status = head.lines().next().unwrap_or_default().to_owned();
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// The script's own answer, which nothing else listening on the port could
/// give: its body is minted for this run.
#[test]
fn serve_answers_a_request_with_what_the_script_returns() {
    let dir = tempfile::tempdir().expect("tempdir");
    let nonce = format!("served-{}", uuid::Uuid::new_v4());
    std::fs::write(
        dir.path().join("app.js"),
        format!("export default {{ fetch() {{ return new Response('{nonce}'); }} }};\n"),
    )
    .expect("write app.js");

    let served = serve(dir.path());
    let (status, body) = get(served.port, "/");
    assert!(
        status.starts_with("HTTP/1.1 200"),
        "the script's response must succeed; got {status}\nlog:\n{}",
        served.log()
    );
    assert_eq!(body, nonce, "the body must be the script's; log:\n{}", served.log());
}

/// The refusal is the whole outcome: a non-zero exit, a message naming the
/// flag, and no server. The control is the same invocation without the flag,
/// which gets past flag parsing and stops only at the missing app file.
#[cfg(not(target_os = "linux"))]
#[test]
fn serve_refuses_a_cpu_limit_it_cannot_enforce() {
    let dir = tempfile::tempdir().expect("tempdir");
    let refused = Command::new(BIN)
        .args(["serve", "absent.js", "--port=0", "--workers=1", "--cpu-limit=50"])
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .output()
        .expect("run zeroship serve");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(1), "stderr:\n{stderr}");
    assert!(
        stderr.contains("--cpu-limit is enforced on Linux only"),
        "the refusal must name the flag and where it is enforced; stderr:\n{stderr}"
    );

    let unlimited = Command::new(BIN)
        .args(["serve", "absent.js", "--port=0", "--workers=1"])
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .output()
        .expect("run zeroship serve");
    let stderr = String::from_utf8_lossy(&unlimited.stderr);
    assert_eq!(unlimited.status.code(), Some(1), "stderr:\n{stderr}");
    assert!(
        stderr.contains("expected a JS file") && !stderr.contains("--cpu-limit"),
        "without the flag the run passes flag parsing and stops at the missing app; \
         stderr:\n{stderr}"
    );
}
