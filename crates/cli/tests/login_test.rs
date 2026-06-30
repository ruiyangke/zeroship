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

#[test]
fn device_grant_flow_polls_until_approved() {
    let server = MockServer::start(vec![
        (
            200,
            r#"{"device_code":"dev-123","user_code":"ABCD-EFGH","verification_uri":"http://auth.test/device","interval":1,"expires_in":60}"#,
        ),
        (400, r#"{"error":"authorization_pending"}"#),
        (
            200,
            r#"{"access_token":"access-123","refresh_token":"refresh-123","expires_in":3600,"token_type":"bearer"}"#,
        ),
        (200, r#"{"email":"dev@example.com","sub":"usr_123"}"#),
    ]);
    let config = tempfile::tempdir().expect("tempdir");

    let output = Command::new(env!("CARGO_BIN_EXE_zeroship"))
        .arg("login")
        .arg("--auth-url")
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
        String::from_utf8_lossy(&output.stdout).contains("Signed in as dev@example.com"),
        "stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Waiting for approval..."),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/oauth2/device/auth");
    assert_eq!(
        requests[0].body,
        "client_id=zeroship-cli&scope=openid+offline_access+apps%3Adeploy+apps%3Aread"
    );
    assert_eq!(requests[1].path, "/oauth2/token");
    assert!(requests[1]
        .body
        .contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"));
    assert!(requests[1].body.contains("device_code=dev-123"));
    assert_eq!(requests[2].path, "/oauth2/token");
    assert!(
        requests[2].at.duration_since(requests[1].at) >= Duration::from_millis(900),
        "token polling did not wait for the server interval"
    );
    assert_eq!(requests[3].method, "GET");
    assert_eq!(requests[3].path, "/userinfo");
    assert_header(&requests[3], "authorization", "Bearer access-123");

    let token = read_token(config.path());
    assert_eq!(token["access_token"], "access-123");
    assert_eq!(token["refresh_token"], "refresh-123");
    assert_eq!(token["auth_url"], server.url);
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
fn supabase_device_flow_uses_control_then_refreshes_gotrue_session() {
    let server = MockServer::start(vec![
        (
            200,
            r#"{"device_code":"supabase-dev-123","user_code":"BCDF-GHJK","verification_uri":"http://auth.test/device","verification_uri_complete":"http://auth.test/device?user_code=BCDF-GHJK","interval":1,"expires_in":60}"#,
        ),
        (400, r#"{"error":"authorization_pending"}"#),
        (
            200,
            r#"{"refresh_token":"bound-refresh","token_type":"Bearer","provider":"supabase","auth_url":"{{BASE_URL}}","token_endpoint":"{{BASE_URL}}/auth/v1/token?grant_type=refresh_token","anon_key":"anon-test-key"}"#,
        ),
        (
            200,
            r#"{"access_token":"gotrue-access","refresh_token":"gotrue-refresh-rotated","expires_in":120,"token_type":"bearer"}"#,
        ),
        (200, r#"{"email":"supabase@example.com","id":"gotrue-user-id"}"#),
    ]);
    let config = tempfile::tempdir().expect("tempdir");
    let token_endpoint = format!("{}/auth/v1/token?grant_type=refresh_token", server.url);

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
        String::from_utf8_lossy(&output.stdout).contains("Signed in as supabase@example.com"),
        "stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/api/device/auth");
    assert_header(&requests[0], "content-type", "application/json");
    assert!(requests[0].body.contains(r#""client_id":"zeroship-cli""#));
    assert_eq!(requests[1].path, "/api/device/token");
    assert!(requests[1].body.contains("supabase-dev-123"));
    assert_eq!(requests[2].path, "/api/device/token");
    assert!(
        requests[2].at.duration_since(requests[1].at) >= Duration::from_millis(900),
        "token polling did not wait for the server interval"
    );
    assert_eq!(
        requests[3].path,
        "/auth/v1/token?grant_type=refresh_token"
    );
    assert_header(&requests[3], "apikey", "anon-test-key");
    assert!(requests[3].body.contains("refresh_token=bound-refresh"));
    assert_eq!(requests[4].path, "/auth/v1/user");
    assert_header(&requests[4], "authorization", "Bearer gotrue-access");
    assert_header(&requests[4], "apikey", "anon-test-key");

    let token = read_token(config.path());
    assert_eq!(token["access_token"], "gotrue-access");
    assert_eq!(token["refresh_token"], "gotrue-refresh-rotated");
    assert_eq!(token["provider"], "supabase");
    assert_eq!(token["auth_url"], server.url);
    assert_eq!(token["control_url"], server.url);
    assert_eq!(token["token_endpoint"], token_endpoint);
    assert_eq!(token["anon_key"], "anon-test-key");
    assert!(token["expires_at"].as_u64().expect("expires_at") > now_secs());
}

#[test]
fn whoami_loads_credentials_and_calls_userinfo() {
    let server = MockServer::start(vec![(200, r#"{"email":"dev@example.com"}"#)]);
    let config = tempfile::tempdir().expect("tempdir");
    write_token(
        config.path(),
        &serde_json::json!({
            "access_token": "access-whoami",
            "refresh_token": "refresh-whoami",
            "expires_at": now_secs() + 3600,
            "auth_url": server.url.clone(),
            "client_id": "zeroship-cli"
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
    assert!(stdout.contains("dev@example.com"), "stdout={stdout}");
    assert!(stdout.contains("Token expires at"), "stdout={stdout}");

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/userinfo");
    assert_header(&requests[0], "authorization", "Bearer access-whoami");
}

#[test]
fn logout_revokes_token_and_clears_file() {
    let server = MockServer::start(vec![(200, "{}")]);
    let config = tempfile::tempdir().expect("tempdir");
    write_token(
        config.path(),
        &serde_json::json!({
            "access_token": "access-logout",
            "refresh_token": "refresh-logout",
            "expires_at": now_secs() + 3600,
            "auth_url": server.url.clone(),
            "client_id": "zeroship-cli"
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

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/oauth2/revoke");
    assert!(requests[0].body.contains("token=refresh-logout"));
    assert!(requests[0].body.contains("token_type_hint=refresh_token"));
    assert!(requests[0].body.contains("client_id=zeroship-cli"));
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
