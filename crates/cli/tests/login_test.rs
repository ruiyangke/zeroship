use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
    at: Instant,
}

#[derive(Debug)]
struct MockServer {
    url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(responses: Vec<(u16, &'static str)>) -> Self {
        Self::start_owned(
            responses
                .into_iter()
                .map(|(status, body)| (status, body.to_string()))
                .collect(),
        )
    }

    fn start_owned(responses: Vec<(u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        listener
            .set_nonblocking(true)
            .expect("set mock server nonblocking");
        let url = format!("http://{}", listener.local_addr().expect("local addr"));
        let responses: Vec<(u16, String)> = responses
            .into_iter()
            .map(|(status, body)| (status, body.replace("{{BASE_URL}}", &url)))
            .collect();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let thread_requests = Arc::clone(&requests);
        let handle = std::thread::spawn(move || {
            for (status, body) in responses {
                let mut stream = accept_with_timeout(&listener);
                let request = read_request(&mut stream);
                thread_requests.lock().expect("lock requests").push(request);
                write_response(&mut stream, status, &body);
            }
        });
        Self {
            url,
            requests,
            handle: Some(handle),
        }
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("lock requests").clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.join().expect("mock server thread");
        }
    }
}

fn accept_with_timeout(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("accept request: {e}"),
        }
    }
}

/// A JWT-shaped access token whose payload is
/// `{"sub":"11111111-1111-4111-8111-111111111111"}`.
///
/// The OP's token response is a plain RFC 6749 body with no `principal_id`
/// field, so the identity the CLI prints has to come out of the token itself.
const PRINCIPAL_JWT: &str =
    "e30.eyJzdWIiOiIxMTExMTExMS0xMTExLTQxMTEtODExMS0xMTExMTExMTExMTEifQ.sig";
const PRINCIPAL_ID: &str = "11111111-1111-4111-8111-111111111111";

/// RFC 9728 protected-resource metadata: control naming the authorization
/// server whose tokens it accepts. `{{BASE_URL}}` is the mock server, which
/// stands in for both control and the OP.
const PROTECTED_RESOURCE_METADATA: &str =
    r#"{"resource":"control.zeroship.ai","authorization_servers":["{{BASE_URL}}/oauth2"]}"#;

fn device_authorization_body(device_code: &str) -> String {
    format!(
        r#"{{"device_code":"{device_code}","user_code":"BCDF-GHJK-LMNP","verification_uri":"http://auth.test/device","verification_uri_complete":"http://auth.test/device?user_code=BCDF-GHJK-LMNP","interval":1,"expires_in":60}}"#
    )
}

fn op_token_body(refresh_token: &str) -> String {
    format!(
        r#"{{"access_token":"{PRINCIPAL_JWT}","token_type":"Bearer","expires_in":900,"scope":"apps:deploy apps:read apps:write secrets:read offline_access","refresh_token":"{refresh_token}"}}"#
    )
}

#[test]
fn device_grant_flow_polls_the_op_until_approved_and_stores_a_refresh_token() {
    let server = MockServer::start_owned(vec![
        (200, PROTECTED_RESOURCE_METADATA.to_string()),
        (200, device_authorization_body("dev-123")),
        (400, r#"{"error":"authorization_pending"}"#.to_string()),
        (200, op_token_body("zrt_root")),
    ]);
    let config = tempfile::tempdir().expect("tempdir");

    let output = Command::new(env!("CARGO_BIN_EXE_zeroship"))
        .arg("login")
        .arg("--control")
        .arg(&server.url)
        .env("ZEROSHIP_CONFIG_HOME", config.path())
        .output()
        .expect("run zeroship login");

    assert!(
        output.status.success(),
        "login failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&format!("Signed in as {PRINCIPAL_ID}")),
        "stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Waiting for approval..."),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 4, "requests={requests:?}");
    // Discovery first: control names the authorization server it trusts, so
    // the CLI cannot be pointed at an OP whose tokens control would refuse.
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/.well-known/oauth-protected-resource");

    // Then the OP's own RFC 8628 endpoints, form-encoded as OAuth requires.
    assert_eq!(requests[1].method, "POST");
    assert_eq!(requests[1].path, "/oauth2/device/authorization");
    assert_header(
        &requests[1],
        "content-type",
        "application/x-www-form-urlencoded",
    );
    assert!(
        requests[1].body.contains("client_id=zeroship-cli"),
        "body={}",
        requests[1].body
    );
    assert!(
        requests[1].body.contains("offline_access"),
        "the CLI must ask for a refresh token; body={}",
        requests[1].body
    );
    assert!(requests[1].body.contains("apps%3Adeploy"), "body={}", requests[1].body);
    assert!(requests[1].body.contains("secrets%3Aread"), "body={}", requests[1].body);
    assert!(
        !requests[1].body.contains("openid"),
        "the device grant mints no id_token, so openid is noise; body={}",
        requests[1].body
    );

    assert_eq!(requests[2].path, "/oauth2/token");
    assert_header(
        &requests[2],
        "content-type",
        "application/x-www-form-urlencoded",
    );
    assert!(requests[2].body.contains("device_code=dev-123"), "body={}", requests[2].body);
    assert_eq!(requests[3].path, "/oauth2/token");
    assert!(
        requests[3].at.duration_since(requests[2].at) >= Duration::from_millis(900),
        "token polling did not wait for the server interval"
    );

    let token = read_token(config.path());
    assert_eq!(token["access_token"], PRINCIPAL_JWT);
    assert_eq!(
        token["refresh_token"], "zrt_root",
        "without this the CLI has nothing to rotate and a 15-minute session"
    );
    assert_eq!(token["provider"], "platform");
    assert_eq!(token["auth_url"], format!("{}/oauth2", server.url));
    assert_eq!(
        token["token_endpoint"],
        format!("{}/oauth2/token", server.url),
        "the rotation endpoint is stored so a refresh needs no second discovery"
    );
    assert_eq!(token["control_url"], server.url);
    assert_eq!(token["client_id"], "zeroship-cli");
    assert!(token["expires_at"].as_u64().expect("expires_at") > now_secs());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(token_path(config.path()))
            .expect("token metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn an_expired_access_token_rotates_and_the_successor_is_persisted_before_it_is_used() {
    // The control call after the rotation fails. If the CLI persisted the
    // rotated credential only after a successful use, the file would still
    // hold `zrt_old` here - and `zrt_old` is now a REUSE presentation that
    // revokes the whole family, locking the user out. Writing first is what
    // makes a crash between rotation and use survivable.
    let server = MockServer::start_owned(vec![
        (200, op_token_body("zrt_new")),
        (500, r#"{"error":"boom"}"#.to_string()),
    ]);
    let config = tempfile::tempdir().expect("tempdir");
    write_token(
        config.path(),
        &serde_json::json!({
            "access_token": "e30.eyJzdWIiOiJzdGFsZSJ9.sig",
            "refresh_token": "zrt_old",
            "expires_at": now_secs() - 1,
            "auth_url": format!("{}/oauth2", server.url),
            "client_id": "zeroship-cli",
            "provider": "platform",
            "control_url": server.url,
            "token_endpoint": format!("{}/oauth2/token", server.url),
        }),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_zeroship"))
        .arg("secret")
        .arg("list")
        .arg("--app=11111111-1111-4111-8111-111111111111")
        .arg(format!("--control={}", server.url))
        .env("ZEROSHIP_CONFIG_HOME", config.path())
        .output()
        .expect("run zeroship secret list");
    assert!(
        !output.status.success(),
        "the control call was mocked as a 500 and must fail\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "requests={requests:?}");
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/oauth2/token");
    assert_header(
        &requests[0],
        "content-type",
        "application/x-www-form-urlencoded",
    );
    assert!(
        requests[0].body.contains("grant_type=refresh_token"),
        "body={}",
        requests[0].body
    );
    assert!(
        requests[0].body.contains("refresh_token=zrt_old"),
        "body={}",
        requests[0].body
    );
    assert!(
        requests[0].body.contains("client_id=zeroship-cli"),
        "a public client identifies itself on the token endpoint; body={}",
        requests[0].body
    );

    let token = read_token(config.path());
    assert_eq!(
        token["refresh_token"], "zrt_new",
        "the rotated refresh token must survive a failure of whatever used the access token"
    );
    assert_eq!(token["access_token"], PRINCIPAL_JWT);
    assert!(token["expires_at"].as_u64().expect("expires_at") > now_secs());
}

#[test]
fn a_refused_rotation_reports_that_the_session_ended_rather_than_a_raw_http_error() {
    // Family revocation (a reuse detection, an operator revoke, an account
    // disable) surfaces here as `invalid_grant`. The human's next step is
    // `zeroship login`, so say that.
    let server = MockServer::start_owned(vec![(
        400,
        r#"{"error":"invalid_grant","error_description":"refresh token is invalid"}"#.to_string(),
    )]);
    let config = tempfile::tempdir().expect("tempdir");
    write_token(
        config.path(),
        &serde_json::json!({
            "access_token": "e30.eyJzdWIiOiJzdGFsZSJ9.sig",
            "refresh_token": "zrt_revoked",
            "expires_at": now_secs() - 1,
            "auth_url": format!("{}/oauth2", server.url),
            "client_id": "zeroship-cli",
            "provider": "platform",
            "control_url": server.url,
            "token_endpoint": format!("{}/oauth2/token", server.url),
        }),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_zeroship"))
        .arg("secret")
        .arg("list")
        .arg("--app=11111111-1111-4111-8111-111111111111")
        .arg(format!("--control={}", server.url))
        .env("ZEROSHIP_CONFIG_HOME", config.path())
        .output()
        .expect("run zeroship secret list");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("zeroship login"),
        "a dead refresh family must point at re-login; stderr={stderr}"
    );
}

#[test]
fn login_honors_project_config_environment_and_prints_provenance() {
    let server = MockServer::start_owned(vec![
        (200, PROTECTED_RESOURCE_METADATA.to_string()),
        (200, device_authorization_body("dev-config")),
        (400, r#"{"error":"authorization_pending"}"#.to_string()),
        (200, op_token_body("zrt_config")),
    ]);
    let project = tempfile::tempdir().expect("project tempdir");
    let token_config = tempfile::tempdir().expect("token tempdir");
    let common = |name: &str, control: &str, environments: &str| {
        format!(
            r#"{{
  "name": "{name}",
  "control": "{control}",
  "runtime_date": "2026-08-14",
  "build": {{ "mode": "full", "dist": "dist", "output": "dist/app.zship" }},
  "migrations": {{ "dir": "migrations", "out": "generated/zeroship" }}{environments}
}}"#
        )
    };
    std::fs::write(
        project.path().join("zeroship.jsonc"),
        common("auto-config", &format!("{}/wrong", server.url), ""),
    )
    .expect("write auto config");
    std::fs::write(
        project.path().join("alternate.jsonc"),
        common(
            "selected-config",
            &format!("{}/alternate-root", server.url),
            &format!(
                ",\n  \"environments\": {{\n    \"staging\": {{\n      \"app\": \"11111111-1111-4111-8111-111111111111\",\n      \"control\": \"{}/selected\"\n    }}\n  }}",
                server.url
            ),
        ),
    )
    .expect("write selected config");

    let output = Command::new(env!("CARGO_BIN_EXE_zeroship"))
        .arg("login")
        .arg("--config=alternate.jsonc")
        .arg("--env=staging")
        .current_dir(project.path())
        .env("ZEROSHIP_CONFIG_HOME", token_config.path())
        .env_remove("ZEROSHIP_CONFIG")
        .env_remove("ZEROSHIP_CONTROL_URL")
        .output()
        .expect("run zeroship login with project config environment");

    assert!(
        output.status.success(),
        "login failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!(
            "zeroship login: control = {}/selected (from zeroship.jsonc environments.staging)",
            server.url
        )),
        "stderr={stderr}"
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 4, "requests={requests:?}");
    // Only DISCOVERY is addressed relative to the resolved control URL. The
    // authorization server it names is absolute, so the OP legs do not inherit
    // the `/selected` prefix.
    assert_eq!(
        requests[0].path,
        "/selected/.well-known/oauth-protected-resource"
    );
    assert_eq!(requests[1].path, "/oauth2/device/authorization");
    assert_eq!(requests[2].path, "/oauth2/token");
    assert_eq!(requests[3].path, "/oauth2/token");
}

#[test]
fn supabase_provider_flag_still_drives_the_one_device_flow() {
    let server = MockServer::start_owned(vec![
        (200, PROTECTED_RESOURCE_METADATA.to_string()),
        (200, device_authorization_body("supabase-dev-123")),
        (400, r#"{"error":"authorization_pending"}"#.to_string()),
        (200, op_token_body("zrt_supabase")),
    ]);
    let config = tempfile::tempdir().expect("tempdir");

    let output = Command::new(env!("CARGO_BIN_EXE_zeroship"))
        .arg("login")
        .arg("--provider=supabase")
        .arg("--control")
        .arg(&server.url)
        .env("ZEROSHIP_CONFIG_HOME", config.path())
        .output()
        .expect("run zeroship login --provider=supabase");

    assert!(
        output.status.success(),
        "login failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&format!("Signed in as {PRINCIPAL_ID}")),
        "stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 4, "requests={requests:?}");
    assert_eq!(requests[0].path, "/.well-known/oauth-protected-resource");
    assert_eq!(requests[1].method, "POST");
    assert_eq!(requests[1].path, "/oauth2/device/authorization");
    assert!(requests[1].body.contains("client_id=zeroship-cli"));
    assert!(requests[1].body.contains("apps%3Awrite"));
    assert_eq!(requests[2].path, "/oauth2/token");
    assert!(requests[2].body.contains("supabase-dev-123"));
    assert_eq!(requests[3].path, "/oauth2/token");
    assert!(
        requests[3].at.duration_since(requests[2].at) >= Duration::from_millis(900),
        "token polling did not wait for the server interval"
    );

    let token = read_token(config.path());
    assert_eq!(token["access_token"], PRINCIPAL_JWT);
    assert_eq!(token["refresh_token"], "zrt_supabase");
    assert_eq!(token["provider"], "platform");
    assert_eq!(token["auth_url"], format!("{}/oauth2", server.url));
    assert_eq!(token["control_url"], server.url);
    assert!(token["anon_key"].is_null());
    assert!(token["expires_at"].as_u64().expect("expires_at") > now_secs());
}

#[test]
fn whoami_loads_platform_credentials_from_token() {
    let config = tempfile::tempdir().expect("tempdir");
    write_token(
        config.path(),
        &serde_json::json!({
            "access_token": "e30.eyJzdWIiOiJ1c3JfMTIzIn0.sig",
            "refresh_token": "",
            "expires_at": now_secs() + 3600,
            "auth_url": "http://control.test",
            "client_id": "zeroship-cli",
            "provider": "platform"
        }),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_zeroship"))
        .arg("whoami")
        .env("ZEROSHIP_CONFIG_HOME", config.path())
        .output()
        .expect("run zeroship whoami");

    assert!(
        output.status.success(),
        "whoami failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("usr_123"), "stdout={stdout}");
    assert!(stdout.contains("Token expires at"), "stdout={stdout}");
}

#[test]
fn logout_clears_file() {
    let config = tempfile::tempdir().expect("tempdir");
    write_token(
        config.path(),
        &serde_json::json!({
            "access_token": "access-logout",
            "refresh_token": "",
            "expires_at": now_secs() + 3600,
            "auth_url": "http://control.test",
            "client_id": "zeroship-cli",
            "provider": "platform"
        }),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_zeroship"))
        .arg("logout")
        .env("ZEROSHIP_CONFIG_HOME", config.path())
        .output()
        .expect("run zeroship logout");

    assert!(
        output.status.success(),
        "logout failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!token_path(config.path()).exists());
}

fn read_request(stream: &mut TcpStream) -> RecordedRequest {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");

    let mut bytes = Vec::new();
    let mut buf = [0_u8; 1024];
    loop {
        let n = stream.read(&mut buf).expect("read request");
        assert_ne!(n, 0, "client closed before headers");
        bytes.extend_from_slice(&buf[..n]);
        if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }

    let header_end = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header terminator")
        + 4;
    let headers_text = String::from_utf8_lossy(&bytes[..header_end]).to_string();
    let mut lines = headers_text.split("\r\n");
    let request_line = lines.next().expect("request line");
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().expect("method").to_string();
    let path = request_parts.next().expect("path").to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    let content_length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);

    while bytes.len() < header_end + content_length {
        let n = stream.read(&mut buf).expect("read body");
        assert_ne!(n, 0, "client closed before body");
        bytes.extend_from_slice(&buf[..n]);
    }

    RecordedRequest {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&bytes[header_end..header_end + content_length])
            .to_string(),
        at: Instant::now(),
    }
}

fn write_response(stream: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Test",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("write response");
}

fn assert_header(request: &RecordedRequest, name: &str, value: &str) {
    assert!(
        request
            .headers
            .iter()
            .any(|(header_name, header_value)| header_name == name && header_value == value),
        "missing header {name}: {value}; headers={:?}",
        request.headers
    );
}

fn token_path(config: &Path) -> std::path::PathBuf {
    config.join("zeroship").join("token.json")
}

fn write_token(config: &Path, value: &serde_json::Value) {
    let path = token_path(config);
    std::fs::create_dir_all(path.parent().expect("token parent")).expect("create token dir");
    std::fs::write(path, serde_json::to_vec_pretty(value).expect("serialize token"))
        .expect("write token");
}

fn read_token(config: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(token_path(config)).expect("read token"))
        .expect("parse token")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("now")
        .as_secs()
}
