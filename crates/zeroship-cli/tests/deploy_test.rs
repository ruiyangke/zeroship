#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::{Command, Output};
#[cfg(unix)]
use std::thread::JoinHandle;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
struct Route {
    method: &'static str,
    path: &'static str,
    status: &'static str,
    body: &'static str,
}

#[cfg(unix)]
struct ControlStub {
    base_url: String,
    server: JoinHandle<Vec<String>>,
}

#[cfg(unix)]
impl ControlStub {
    fn start(routes: Vec<Route>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind control stub");
        listener
            .set_nonblocking(true)
            .expect("make control stub nonblocking");
        let base_url = format!(
            "http://{}",
            listener.local_addr().expect("control stub address")
        );
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for route in routes {
                let deadline = Instant::now() + Duration::from_secs(5);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "CLI did not reach the control stub"
                            );
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("accept control request: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("bound control request read");
                let request = read_request(&mut stream);
                seen.push(request.clone());
                let expected = format!("{} {}", route.method, route.path);
                let (status, body) = if request == expected {
                    (route.status, route.body)
                } else {
                    (
                        "500 Internal Server Error",
                        r#"{"error":"unexpected request"}"#,
                    )
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("write control response");
            }
            seen
        });
        Self { base_url, server }
    }

    fn finish(self) -> Vec<String> {
        self.server.join().expect("control stub completed")
    }
}

#[cfg(unix)]
fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).expect("read control request");
        assert_ne!(read, 0, "control request ended before its headers");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(at) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end]).expect("request headers are UTF-8");
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("valid content length"))
        })
        .unwrap_or_default();
    let request = headers
        .lines()
        .next()
        .and_then(|line| {
            line.rsplit_once(" HTTP/")
                .map(|(request, _)| request.to_string())
        })
        .expect("HTTP request line");
    while bytes.len() < header_end + content_length {
        let read = stream.read(&mut chunk).expect("read control request body");
        assert_ne!(read, 0, "control request body ended early");
        bytes.extend_from_slice(&chunk[..read]);
    }
    request
}

#[cfg(unix)]
#[test]
fn deploy_warns_for_each_declared_secret_missing_from_the_app() {
    let control = ControlStub::start(vec![
        Route {
            method: "GET",
            path: "/api/apps/app_034klb07lrb9jgma6imvmx019/secrets",
            status: "200 OK",
            body: r#"{"secrets":["PRESENT"]}"#,
        },
        Route {
            method: "POST",
            path: "/api/apps/app_034klb07lrb9jgma6imvmx019/deploy",
            status: "200 OK",
            body: r#"{"deploy_hash":"sha256:test"}"#,
        },
    ]);
    let project = project(
        "secret-warning-test",
        Some("app_034klb07lrb9jgma6imvmx019"),
        &control.base_url,
        &["PRESENT", "MISSING"],
    );

    let output = run_deploy(project.path());
    let requests = control.finish();
    assert!(
        output.status.success(),
        "deploy failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        requests,
        [
            "GET /api/apps/app_034klb07lrb9jgma6imvmx019/secrets",
            "POST /api/apps/app_034klb07lrb9jgma6imvmx019/deploy",
        ],
        "the secret-name check must happen before the archive upload"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "zeroship deploy: warning: zeroship.jsonc declares `MISSING`, but `zeroship secret list` does not show it"
        ),
        "missing secret warning absent:\n{stderr}"
    );
    assert!(
        !stderr.contains("declares `PRESENT`"),
        "a configured secret was falsely reported missing:\n{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn first_deploy_creates_the_app_and_persists_its_id() {
    let control = ControlStub::start(vec![
        Route {
            method: "GET",
            path: "/api/apps",
            status: "200 OK",
            body: "[]",
        },
        Route {
            method: "POST",
            path: "/api/apps",
            status: "201 Created",
            body: r#"{"id":"app_034klb07lrb9jgma6imvmx020","name":"first-deploy"}"#,
        },
        Route {
            method: "POST",
            path: "/api/apps/app_034klb07lrb9jgma6imvmx020/deploy",
            status: "200 OK",
            body: r#"{"deploy_hash":"sha256:test"}"#,
        },
    ]);
    let project = project("first-deploy", None, &control.base_url, &[]);

    let output = run_deploy(project.path());
    let requests = control.finish();
    assert!(
        output.status.success(),
        "deploy failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        requests,
        [
            "GET /api/apps",
            "POST /api/apps",
            "POST /api/apps/app_034klb07lrb9jgma6imvmx020/deploy",
        ]
    );
    let written = std::fs::read_to_string(project.path().join("zeroship.jsonc"))
        .expect("read updated project config");
    assert!(
        written.contains("\"app\": \"app_034klb07lrb9jgma6imvmx020\""),
        "created app id was not persisted:\n{written}"
    );
}

#[cfg(unix)]
fn project(
    name: &str,
    app_id: Option<&str>,
    control_url: &str,
    secrets: &[&str],
) -> tempfile::TempDir {
    let project = tempfile::tempdir().expect("create project directory");
    std::fs::create_dir(project.path().join("dist")).expect("create dist");
    std::fs::write(project.path().join("dist/app.zship"), b"test archive").expect("write archive");
    let app = app_id
        .map(|id| format!("  \"app\": {id:?},\n"))
        .unwrap_or_default();
    let secrets = serde_json::to_string(secrets).expect("serialize secret names");
    std::fs::write(
        project.path().join("zeroship.jsonc"),
        format!(
            "{{\n  \"name\": {name:?},\n{app}  \"control\": {control_url:?},\n  \"runtime_date\": \"2026-08-14\",\n  \"build\": {{ \"mode\": \"full\", \"dist\": \"dist\", \"output\": \"dist/app.zship\" }},\n  \"migrations\": {{ \"dir\": \"migrations\", \"out\": \"generated/zeroship\" }},\n  \"secrets\": {secrets}\n}}\n"
        ),
    )
    .expect("write project config");
    project
}

#[cfg(unix)]
fn run_deploy(project: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zeroship"))
        .arg("deploy")
        .current_dir(project)
        .env("ZEROSHIP_TOKEN", "test-token")
        .env_remove("ZEROSHIP_CONFIG")
        .env_remove("ZEROSHIP_CONTROL_URL")
        .output()
        .expect("run zeroship deploy")
}
