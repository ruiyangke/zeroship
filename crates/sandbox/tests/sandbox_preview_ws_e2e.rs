//! Phase-2 preview-WebSocket e2e (preview-URL § II.1 + § II.2).
//!
//! Coverage (matches the design doc's Phase 2 test plan, agent-side
//! AND controller-side, lines 1931-1935):
//!
//! - Round-trip splice: client opens a WS through the controller →
//!   agent → upstream echo server; bytes flow both directions after 101.
//! - Anonymous Upgrade through the controller → 401 uniform.
//! - Wrong-creator Upgrade → 404 uniform.
//! - Empty-body upgrade is signed under V1_1_Ws and accepted.
//! - **CRITICAL** (round-2 D-14): a captured V1_1 HTTP signature on a
//!   body matching `b"sec-websocket-key=…"` MUST NOT validate as a
//!   V1_1_Ws Upgrade.
//! - Body-cap on Upgrade: a non-empty body → 400.
//!
//! ## Test fixture topology
//!
//! ```text
//! test client (raw TCP)
//!     │
//!     ▼  GET /sandboxes/{id}/preview/{port}/ws  HTTP/1.1
//!     │  Upgrade: websocket; Authorization: Bearer …
//!     │
//! ╔═══════════════════════════════════════════╗
//! ║  controller's preview_ws::serve on PortC  ║
//! ╚═══════════════════════════════════════════╝
//!     │
//!     ▼ /proxy/{port}/ws  + V1_1_Ws sig + Sec-WebSocket-Key
//!     │
//! ╔═══════════════════════════════════════════╗
//! ║  agent's proxy_ws::serve on PortA + 1     ║   (= PortAWs)
//! ╚═══════════════════════════════════════════╝
//!     │
//!     ▼ raw TCP to 127.0.0.1:PortU
//!     │
//! ╔═══════════════════════════════════════════╗
//! ║  fixture WS-echo upstream on PortU        ║
//! ╚═══════════════════════════════════════════╝
//! ```

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::SigningKey;
use uuid::Uuid;

use zeroship_sandbox::backend::{Backend, SandboxInfo};
use zeroship_sandbox::config::{ApiToken, K8sConfig, NomadCHConfig, SandboxConfig};
use zeroship_sandbox::registry::SandboxRegistry;
use zeroship_sandbox_agent::sig::{self, CanonicalKind};

// ─── fixture: WS-echo upstream ──────────────────────────────────────

/// Tiny fixture upstream that completes a WebSocket handshake and
/// echoes raw bytes back. After 101 we don't speak the WS protocol —
/// the splice on both sides is byte-level, so for our test it's
/// sufficient to send arbitrary bytes and assert they round-trip.
struct WsEchoUpstream {
    port: u16,
    stop: Arc<AtomicBool>,
}

impl WsEchoUpstream {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_c = stop.clone();
        thread::spawn(move || {
            while !stop_c.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let stop_inner = stop_c.clone();
                        thread::spawn(move || handle_one(stream, stop_inner));
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self { port, stop }
    }
}

impl Drop for WsEchoUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn handle_one(mut stream: TcpStream, stop: Arc<AtomicBool>) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    // Read the request head until \r\n\r\n.
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 1024];
    loop {
        let n = match stream.read(&mut tmp) {
            Ok(0) => return,
            Ok(n) => n,
            Err(_) => return,
        };
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 32 * 1024 {
            return;
        }
    }
    // Reply with 101 (no real Sec-WebSocket-Accept since the splice
    // is opaque; tests don't run a real WS client). Produce a
    // non-empty Sec-WebSocket-Accept header so a future proper-WS
    // client could be plugged in.
    let resp = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: dummy\r\n\r\n";
    if stream.write_all(resp).is_err() {
        return;
    }
    // Echo loop: read bytes, write them back, until close or stop.
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .ok();
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let mut buf = [0u8; 4096];
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if stream.write_all(&buf[..n]).is_err() {
                    return;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => return,
        }
    }
}

// ─── fixture: state with sandbox pre-injected ───────────────────────

fn make_cfg(token: &str) -> SandboxConfig {
    SandboxConfig {
        port: 9091,
        token: ApiToken::new(token),
        backend: "nomad-ch".into(),
        image: "img".into(),
        workspace_root: std::path::PathBuf::from("/var/zeroship/projects"),
        network: "n".into(),
        memory_mb: 1024,
        cpus: 2.0,
        idle_timeout_secs: 1800,
        max_lifetime_secs: 28800,
        auto_pull: false,
        k8s: K8sConfig {
            namespace: "default".into(),
            image: "i".into(),
            runtime_class: "kvm-sandbox".into(),
            ready_timeout_secs: 120,
            use_port_forward: false,
            port_forward_start: 18000,
            user_home_size: "5Gi".into(),
            user_home_storage_class: None,
            startup_orphan_cleanup: false,
        },
        nomad_ch: NomadCHConfig {
            nomad_addr: "http://127.0.0.1:4646".into(),
            datacenter: "dc1".into(),
            wrapper_path: std::path::PathBuf::from("/etc/zeroship/nomad-vm-wrapper.sh"),
            runtime_dir: std::path::PathBuf::from("/var/lib/zeroship/ch"),
            host_state_dir: std::path::PathBuf::from("/var/zeroship/ch"),
            user_home_dir_root: std::path::PathBuf::from("/var/zeroship/ch/users"),
            vm_index_floor: 1,
            vm_index_ceil: 200,
            alloc_running_timeout_secs: 60,
            agent_livez_timeout_secs: 30,
            host_fence_timeout_secs: 30,
            startup_orphan_cleanup: false,
            subnet_second_octet: 99,
        },
        create_retry_max: 2,
        create_retry_total_timeout_secs: 90,
    }
}

fn make_state(
    token: &str,
    user_id: &str,
    agent_http_port: u16,
    sk: SigningKey,
) -> (Arc<zeroship_sandbox::AppState>, Uuid) {
    let cfg = make_cfg(token);
    let backend = Backend::from_config(&cfg).expect("backend");
    let sandbox_id = Uuid::now_v7();
    if let Backend::NomadCh(b) = &backend {
        b._test_inject_sandbox(
            sandbox_id,
            user_id,
            sk,
            // agent_url's HTTP port. The controller's preview_ws
            // derives the WS port as http_port + 1.
            format!("http://127.0.0.1:{agent_http_port}"),
            42,
        );
    } else {
        unreachable!("test pinned to nomad-ch");
    }
    let registry = SandboxRegistry::new();
    // NOTE: legacy hyphenated UUID form. The typed-id wire shape is
    // covered in `tests/sandbox_typed_id_e2e.rs`.
    let info = SandboxInfo {
        sandbox_id: sandbox_id.to_string(),
        user_id: user_id.to_string(),
        project_id: "p".into(),
        backend: "nomad-ch".into(),
        backend_hint: "test".into(),
        created_at_secs: 0,
        last_used_at_secs: 0,
    };
    registry.insert(sandbox_id, info);
    let state = Arc::new(zeroship_sandbox::AppState {
        config: cfg,
        sandboxes: registry,
        backend,
        mint_rate_limiter: Some(zeroship_sandbox::preview_share_handlers::MintRateLimiter::new()),
        // Phase-0 sandbox-pg-state: tests run pg-disabled.
        database: None,
        persist: None,
        shutdown: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        admin_token: None,
    });
    (state, sandbox_id)
}

// ─── helpers: sign + send ───────────────────────────────────────────

const TEST_SK: [u8; 32] = [42u8; 32];

fn pick_unused_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

fn sign_v1_1_ws(method: &str, path_query: &str, sk: &SigningKey) -> (u64, String, String) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nonce = format!("ws-e2e-{pid}-{n}");
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let sig = sig::sign_kind(CanonicalKind::V1_1_Ws, sk, method, path_query, b"", ts, &nonce);
    (ts, nonce, sig)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Wait until a TCP listener accepts a connection on `port`. The
/// async listener is spawned in a different runtime thread; tests
/// race the listen-bind against the first connect attempt.
fn wait_for_port(port: u16) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("port :{port} never came up");
}

// ─── helpers: spawn agent + controller listeners on compio ─────────

fn spawn_agent_ws(
    agent_state: zeroship_sandbox_agent::AppState,
    ws_port: u16,
) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let _stop_c = stop.clone();
    thread::spawn(move || {
        compio::runtime::Runtime::new().unwrap().block_on(async move {
            let _ = zeroship_sandbox_agent::proxy_ws::serve(agent_state, ws_port).await;
        });
    });
    wait_for_port(ws_port);
    stop
}

fn spawn_controller_ws(
    state: Arc<zeroship_sandbox::AppState>,
    ws_port: u16,
) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let _stop_c = stop.clone();
    thread::spawn(move || {
        compio::runtime::Runtime::new().unwrap().block_on(async move {
            let _ = zeroship_sandbox::preview_ws::serve(state, ws_port).await;
        });
    });
    wait_for_port(ws_port);
    stop
}

/// Build an agent AppState whose Verifier accepts signatures from the
/// test signing key. Reuses `state_with_paths` from the agent crate.
fn make_agent_state(label: &str) -> zeroship_sandbox_agent::AppState {
    let dir = std::env::temp_dir().join(format!(
        "zsbx-wse2e-{label}-{}-{}",
        std::process::id(),
        std::time::Instant::now().elapsed().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let pubkey_path = dir.join("controller-pubkey");
    let pk = SigningKey::from_bytes(&TEST_SK).verifying_key();
    std::fs::write(&pubkey_path, pk.as_bytes()).unwrap();
    zeroship_sandbox_agent::state_with_paths(&pubkey_path, &dir.join("ws")).unwrap()
}

// ─── helpers: client side of the WS handshake ──────────────────────

/// Send an HTTP/1.1 head and read until \r\n\r\n. Returns the head
/// bytes (everything up through the second CRLF).
fn send_and_read_head(stream: &mut TcpStream, head: &[u8]) -> Vec<u8> {
    stream.write_all(head).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp).unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 32 * 1024 {
            break;
        }
    }
    buf
}

fn parse_status(head: &[u8]) -> u16 {
    let line = head.split(|b| *b == b'\n').next().unwrap_or(b"");
    let line = std::str::from_utf8(line.strip_suffix(b"\r").unwrap_or(line)).unwrap_or("");
    let mut parts = line.splitn(3, ' ');
    let _ = parts.next();
    parts.next().unwrap_or("0").parse().unwrap_or(0)
}

// ─── tests ──────────────────────────────────────────────────────────

#[test]
fn ws_round_trip_through_controller_and_agent() {
    // Topology: client → controller_ws_port → agent_ws_port → echo
    // upstream.
    let echo = WsEchoUpstream::spawn();
    let agent_http_port = pick_unused_port();
    let agent_ws_port = agent_http_port + 1;

    // Spawn the agent's WS listener (same port the controller will
    // dial). We don't run the agent's HTTP listener — the test only
    // exercises the WS path.
    let agent_state = make_agent_state("rt");
    let _agent_stop = spawn_agent_ws(agent_state, agent_ws_port);

    // Build controller state pointing at the agent's HTTP port (the
    // controller derives the WS port as +1 internally).
    let sk = SigningKey::from_bytes(&TEST_SK);
    let (state, sandbox_id) = make_state("tok", "alice", agent_http_port, sk);
    let controller_ws_port = pick_unused_port();
    let _ctrl_stop = spawn_controller_ws(state, controller_ws_port);

    // Open a TCP connection to the controller's WS port. Send a
    // signed WS-Upgrade request.
    let mut stream = TcpStream::connect(("127.0.0.1", controller_ws_port)).unwrap();
    let path = format!(
        "/sandboxes/{sandbox_id}/preview/{up}/ws?user_id=alice",
        up = echo.port
    );
    let head = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: 127.0.0.1:{controller_ws_port}\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Authorization: Bearer tok\r\n\
         \r\n"
    );

    let resp_head = send_and_read_head(&mut stream, head.as_bytes());
    let status = parse_status(&resp_head);
    assert_eq!(status, 101, "expected 101 Switching Protocols, got: {}\nhead:\n{}", status, String::from_utf8_lossy(&resp_head));

    // After 101 the splice is byte-level. Send arbitrary bytes; the
    // upstream echoes them back through both proxies.
    let payload = b"hello-ws-world";
    stream.write_all(payload).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut got = vec![0u8; payload.len()];
    let mut total = 0;
    while total < payload.len() {
        let n = stream.read(&mut got[total..]).unwrap();
        if n == 0 {
            break;
        }
        total += n;
    }
    assert_eq!(&got[..total], payload, "WS bytes must round-trip");

    drop(stream);
    drop(echo);
}

#[test]
fn ws_anonymous_returns_401() {
    let agent_http_port = pick_unused_port();
    let agent_ws_port = agent_http_port + 1;
    let agent_state = make_agent_state("anon");
    let _stop = spawn_agent_ws(agent_state, agent_ws_port);

    let sk = SigningKey::from_bytes(&TEST_SK);
    let (state, sandbox_id) = make_state("tok", "alice", agent_http_port, sk);
    let controller_ws_port = pick_unused_port();
    let _ctrl_stop = spawn_controller_ws(state, controller_ws_port);

    let mut stream = TcpStream::connect(("127.0.0.1", controller_ws_port)).unwrap();
    let path = format!("/sandboxes/{sandbox_id}/preview/5173/ws?user_id=alice");
    // No Authorization header.
    let head = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: 127.0.0.1:{controller_ws_port}\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    );
    let resp_head = send_and_read_head(&mut stream, head.as_bytes());
    assert_eq!(parse_status(&resp_head), 401);
}

#[test]
fn ws_wrong_creator_returns_404() {
    let agent_http_port = pick_unused_port();
    let agent_ws_port = agent_http_port + 1;
    let agent_state = make_agent_state("wrong");
    let _stop = spawn_agent_ws(agent_state, agent_ws_port);

    let sk = SigningKey::from_bytes(&TEST_SK);
    let (state, sandbox_id) = make_state("tok", "alice", agent_http_port, sk);
    let controller_ws_port = pick_unused_port();
    let _ctrl_stop = spawn_controller_ws(state, controller_ws_port);

    let mut stream = TcpStream::connect(("127.0.0.1", controller_ws_port)).unwrap();
    // Creator-A's token, but query says user_id=bob → not the owner.
    let path = format!("/sandboxes/{sandbox_id}/preview/5173/ws?user_id=bob");
    let head = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: 127.0.0.1:{controller_ws_port}\r\n\
         Authorization: Bearer tok\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Key: AAAAAAAAAAAAAAAAAAAAAA==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    );
    let resp_head = send_and_read_head(&mut stream, head.as_bytes());
    assert_eq!(parse_status(&resp_head), 404);
}

#[test]
fn agent_ws_v1_1_http_signature_replay_rejected() {
    // **CRITICAL forgery defense (D-14).** A V1_1 HTTP signature on
    // a body containing `b"sec-websocket-key=..."` MUST NOT validate
    // as a V1_1_Ws Upgrade. We hit the agent's WS port directly with
    // such a captured signature; the agent must 401.
    let agent_http_port = pick_unused_port();
    let agent_ws_port = agent_http_port + 1;
    let agent_state = make_agent_state("replay");
    let _stop = spawn_agent_ws(agent_state, agent_ws_port);

    // Craft a V1_1 HTTP signature over the `/proxy/5173/ws` path with
    // a body that includes the Sec-WebSocket-Key wire bytes (the
    // round-2 forgery target). Then replay it as an Upgrade.
    let sk = SigningKey::from_bytes(&TEST_SK);
    let path_query = "/proxy/5173/ws";
    let body = b"sec-websocket-key=dGhlIHNhbXBsZSBub25jZQ==";
    let ts = unix_now();
    let nonce = "replay-test-1";
    let v1_1_sig = sig::sign_kind(CanonicalKind::V1_1, &sk, "GET", path_query, body, ts, nonce);

    let mut stream = TcpStream::connect(("127.0.0.1", agent_ws_port)).unwrap();
    // Send an Upgrade request bearing the V1_1 (HTTP) signature in
    // X-Sbx-Signature. Body is empty (Upgrade contract); the bytes
    // were what was signed, but the Upgrade wire has no body, so the
    // canonical the agent computes uses the empty-body hash. Either
    // way, the V1_1 signature has no `-WS` domain tag, so the V1_1_Ws
    // verifier rejects it.
    let head = format!(
        "GET {path_query} HTTP/1.1\r\n\
         Host: 127.0.0.1:{agent_ws_port}\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         X-Sbx-Timestamp: {ts}\r\n\
         X-Sbx-Nonce: {nonce}\r\n\
         X-Sbx-Signature: {v1_1_sig}\r\n\
         \r\n"
    );
    let resp_head = send_and_read_head(&mut stream, head.as_bytes());
    assert_eq!(
        parse_status(&resp_head),
        401,
        "V1_1 HTTP signature MUST NOT validate as V1_1_Ws Upgrade; got:\n{}",
        String::from_utf8_lossy(&resp_head)
    );
}

#[test]
fn agent_ws_non_empty_body_rejected() {
    // RFC 6455 §1.3 — Upgrade carries no body. A non-empty body is 400.
    let agent_http_port = pick_unused_port();
    let agent_ws_port = agent_http_port + 1;
    let agent_state = make_agent_state("body");
    let _stop = spawn_agent_ws(agent_state, agent_ws_port);

    let sk = SigningKey::from_bytes(&TEST_SK);
    let path_query = "/proxy/5173/ws";
    let (ts, nonce, sig_v) = sign_v1_1_ws("POST", path_query, &sk);

    let mut stream = TcpStream::connect(("127.0.0.1", agent_ws_port)).unwrap();
    let body = b"this-should-not-be-here";
    let head = format!(
        "POST {path_query} HTTP/1.1\r\n\
         Host: 127.0.0.1:{agent_ws_port}\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Key: AAAAAAAAAAAAAAAAAAAAAA==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Content-Length: {len}\r\n\
         X-Sbx-Timestamp: {ts}\r\n\
         X-Sbx-Nonce: {nonce}\r\n\
         X-Sbx-Signature: {sig_v}\r\n\
         \r\n",
        len = body.len()
    );
    let mut buf = head.into_bytes();
    buf.extend_from_slice(body);
    let resp_head = send_and_read_head(&mut stream, &buf);
    assert_eq!(
        parse_status(&resp_head),
        400,
        "non-empty Upgrade body MUST be rejected; got:\n{}",
        String::from_utf8_lossy(&resp_head)
    );
}

#[test]
fn agent_ws_port_deny() {
    // Agent's defense-in-depth port deny — port 22 is blocked even if
    // the V1_1_Ws sig verifies.
    let agent_http_port = pick_unused_port();
    let agent_ws_port = agent_http_port + 1;
    let agent_state = make_agent_state("denyport");
    let _stop = spawn_agent_ws(agent_state, agent_ws_port);

    let sk = SigningKey::from_bytes(&TEST_SK);
    let path_query = "/proxy/22/x";
    let (ts, nonce, sig_v) = sign_v1_1_ws("GET", path_query, &sk);

    let mut stream = TcpStream::connect(("127.0.0.1", agent_ws_port)).unwrap();
    let head = format!(
        "GET {path_query} HTTP/1.1\r\n\
         Host: 127.0.0.1:{agent_ws_port}\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Key: AAAAAAAAAAAAAAAAAAAAAA==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         X-Sbx-Timestamp: {ts}\r\n\
         X-Sbx-Nonce: {nonce}\r\n\
         X-Sbx-Signature: {sig_v}\r\n\
         \r\n"
    );
    let resp_head = send_and_read_head(&mut stream, head.as_bytes());
    assert_eq!(parse_status(&resp_head), 400);
}

#[test]
fn agent_ws_non_upgrade_returns_426() {
    // The agent's WS port is Upgrade-only. A plain HTTP GET → 426.
    let agent_http_port = pick_unused_port();
    let agent_ws_port = agent_http_port + 1;
    let agent_state = make_agent_state("notup");
    let _stop = spawn_agent_ws(agent_state, agent_ws_port);

    let mut stream = TcpStream::connect(("127.0.0.1", agent_ws_port)).unwrap();
    let head = format!(
        "GET /proxy/5173/x HTTP/1.1\r\n\
         Host: 127.0.0.1:{agent_ws_port}\r\n\
         \r\n"
    );
    let resp_head = send_and_read_head(&mut stream, head.as_bytes());
    assert_eq!(parse_status(&resp_head), 426);
}
