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

/// One scripted answer. An empty `status` closes the connection after reading
/// the request, the way a reply lost after Control committed looks to the CLI.
/// A `{command}` in `body` is replaced by the request's `Idempotency-Key`.
#[cfg(unix)]
struct Route {
    method: &'static str,
    path: &'static str,
    status: &'static str,
    body: &'static str,
}

/// A request as the stub saw it.
#[cfg(unix)]
#[derive(Debug, Clone)]
struct Seen {
    line: String,
    idempotency_key: Option<String>,
    body: Vec<u8>,
}

#[cfg(unix)]
struct ControlStub {
    base_url: String,
    server: JoinHandle<Vec<Seen>>,
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
                let (status, body) = if request.line == expected {
                    (
                        route.status,
                        route.body.replace(
                            "{command}",
                            request.idempotency_key.as_deref().unwrap_or(""),
                        ),
                    )
                } else {
                    (
                        "500 Internal Server Error",
                        r#"{"error":"unexpected request"}"#.to_string(),
                    )
                };
                if status.is_empty() {
                    drop(stream);
                    continue;
                }
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

    fn finish(self) -> Vec<Seen> {
        self.server.join().expect("control stub completed")
    }
}

#[cfg(unix)]
fn lines(seen: &[Seen]) -> Vec<&str> {
    seen.iter().map(|request| request.line.as_str()).collect()
}

/// Control's acceptance, naming the command the request carried.
#[cfg(unix)]
const ACCEPTED: &str = r#"{"command_id":"{command}","deploy_id":"dep_034klb07lrb9jgma6imvmx000","deploy_hash":"sha256:test","blobs_uploaded":1,"blobs_deduped":0,"lifecycle_revision":1}"#;

#[cfg(unix)]
fn read_request(stream: &mut TcpStream) -> Seen {
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
    let idempotency_key = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("idempotency-key")
            .then(|| value.trim().to_string())
    });
    let line = headers
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
    Seen {
        line,
        idempotency_key,
        body: bytes[header_end..header_end + content_length].to_vec(),
    }
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
            body: ACCEPTED,
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
        lines(&requests),
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
            body: ACCEPTED,
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
        lines(&requests),
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

/// A reply lost after the upload is resent under the same command id with the
/// same bytes, and the command id the CLI prints is the one it sent.
#[cfg(unix)]
#[test]
fn a_lost_reply_resends_the_same_command_and_bytes() {
    let deploy = "/api/apps/app_034klb07lrb9jgma6imvmx021/deploy";
    let control = ControlStub::start(vec![
        Route {
            method: "POST",
            path: deploy,
            status: "",
            body: "",
        },
        Route {
            method: "POST",
            path: deploy,
            status: "200 OK",
            body: ACCEPTED,
        },
    ]);
    let project = project(
        "lost-reply",
        Some("app_034klb07lrb9jgma6imvmx021"),
        &control.base_url,
        &[],
    );

    let output = run_deploy(project.path());
    let requests = control.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "deploy failed\nstderr={stderr}");
    assert_eq!(
        lines(&requests),
        [format!("POST {deploy}"), format!("POST {deploy}")]
    );
    let key = requests[0]
        .idempotency_key
        .clone()
        .expect("the deploy carries its command id");
    assert!(
        zeroship_core::DeployCommandId::parse(&key).is_ok(),
        "the key is a canonical command id: {key}"
    );
    assert_eq!(requests[1].idempotency_key.as_deref(), Some(key.as_str()));
    let archive = std::fs::read(project.path().join("dist/app.zship")).expect("read archive");
    assert!(requests.iter().all(|request| request.body == archive));
    assert!(
        stderr.contains(&format!("command_id: {key}")),
        "the command id is printed before the upload:\n{stderr}"
    );
    assert!(stderr.contains("Deployed successfully!"), "{stderr}");
}

/// When no attempt is answered the deploy fails naming its command id, and
/// re-running with that id resends the same command instead of a new one.
#[cfg(unix)]
#[test]
fn an_unanswered_deploy_can_be_resumed_by_its_command_id() {
    let deploy = "/api/apps/app_034klb07lrb9jgma6imvmx022/deploy";
    let unavailable = || Route {
        method: "POST",
        path: deploy,
        status: "503 Service Unavailable",
        body: r#"{"error":"unavailable"}"#,
    };
    let control = ControlStub::start(vec![unavailable(), unavailable(), unavailable()]);
    let project = project(
        "resumed",
        Some("app_034klb07lrb9jgma6imvmx022"),
        &control.base_url,
        &[],
    );

    let output = run_deploy(project.path());
    let requests = control.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "an unanswered deploy must fail:\n{stderr}"
    );
    assert_eq!(requests.len(), 3, "attempts stop at the retry bound");
    let key = requests[0]
        .idempotency_key
        .clone()
        .expect("the deploy carries its command id");
    assert!(requests
        .iter()
        .all(|request| request.idempotency_key.as_deref() == Some(key.as_str())));
    let resume = format!("--command-id={key}");
    assert!(
        stderr.contains("outcome is unknown") && stderr.contains(&resume),
        "the failure names the command to resume:\n{stderr}"
    );

    let control = ControlStub::start(vec![Route {
        method: "POST",
        path: deploy,
        status: "200 OK",
        body: ACCEPTED,
    }]);
    rewrite_control(project.path(), &control.base_url);
    let output = run_deploy_with(project.path(), &[&resume]);
    let requests = control.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the resumed deploy failed:\n{stderr}"
    );
    assert_eq!(requests[0].idempotency_key.as_deref(), Some(key.as_str()));
}

/// Point an existing project at another control stub.
#[cfg(unix)]
fn rewrite_control(project: &Path, control_url: &str) {
    let path = project.join("zeroship.jsonc");
    let config = std::fs::read_to_string(&path).expect("read project config");
    let start = config.find("\"control\": ").expect("control member");
    let end = start + config[start..].find(",\n").expect("control member ends");
    let rewritten = format!(
        "{}\"control\": {control_url:?}{}",
        &config[..start],
        &config[end..]
    );
    std::fs::write(path, rewritten).expect("write project config");
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
    run_deploy_with(project, &[])
}

#[cfg(unix)]
fn run_deploy_with(project: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zeroship"))
        .arg("deploy")
        .args(args)
        .current_dir(project)
        .env("ZEROSHIP_TOKEN", "test-token")
        .env_remove("ZEROSHIP_CONFIG")
        .env_remove("ZEROSHIP_CONTROL_URL")
        .output()
        .expect("run zeroship deploy")
}
