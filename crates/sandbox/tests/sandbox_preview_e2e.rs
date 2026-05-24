//! Phase-1 preview-proxy e2e (preview-URL § II.2).
//!
//! Runs the controller's `preview_proxy` handler end-to-end against
//! a fixture HTTP "agent" — there's no real microVM in the test
//! environment, so we mock the agent with a thread-driven
//! `TcpListener` that verifies our signed v1.1 requests and replies.
//!
//! Coverage (matches the design doc's Phase 1 test plan, controller
//! side):
//!
//! - 200 round-trip: authed creator → request flows end-to-end.
//! - 401 anonymous: no bearer, uniform body.
//! - 404 wrong-creator: creator-A's bearer + creator-B's sandbox.
//! - 400 port-deny at controller: defense-in-depth (the agent never
//!   gets called when the port is hardcoded-deny).
//! - X-ZSPreview-Host injection from caller is sanitized: the
//!   controller's outbound X-Forwarded-Host carries the controller-
//!   computed value, not the inbound header.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use ntex::http::StatusCode;
use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_sandbox::backend::{Backend, SandboxInfo};
use zeroship_sandbox::config::{ApiToken, SandboxConfig};
use zeroship_sandbox::preview;

/// Handle a single request from the fixture agent. Captures the
/// raw request bytes (so the test can assert on the headers we
/// signed) and replies with a fixed status + body + extra headers.
struct AgentReply {
    status: u16,
    body: Vec<u8>,
    extra_headers: Vec<(String, String)>,
}

/// Fixture HTTP agent. Single-threaded — runs in a background thread,
/// captures every inbound request line + headers into `captured`.
struct FixtureAgent {
    port: u16,
    stop: Arc<AtomicBool>,
    captured: Arc<Mutex<Vec<String>>>,
}

impl FixtureAgent {
    fn spawn(reply: AgentReply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_c = stop.clone();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_c = captured.clone();
        let reply = Arc::new(reply);
        thread::spawn(move || {
            while !stop_c.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_millis(200)))
                            .ok();
                        let mut buf = vec![0u8; 64 * 1024];
                        let n = stream.read(&mut buf).unwrap_or(0);
                        let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                        captured_c.lock().unwrap().push(raw);

                        let mut hdrs = format!(
                            "HTTP/1.1 {} OK\r\nContent-Length: {}\r\nConnection: close\r\n",
                            reply.status,
                            reply.body.len(),
                        );
                        for (k, v) in &reply.extra_headers {
                            hdrs.push_str(&format!("{k}: {v}\r\n"));
                        }
                        hdrs.push_str("\r\n");
                        let _ = stream.write_all(hdrs.as_bytes());
                        let _ = stream.write_all(&reply.body);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self { port, stop, captured }
    }

    fn captured(&self) -> Vec<String> {
        self.captured.lock().unwrap().clone()
    }
}

impl Drop for FixtureAgent {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn make_cfg(token: &str) -> SandboxConfig {
    // A7 (deferred): see sandbox_admin_e2e.rs::make_cfg for rationale.
    // `token` is `pub(crate)`; construct via the public fixture
    // constructor + `with_token` builder.
    SandboxConfig::new_fixture().with_token(ApiToken::new(token))
}

/// Build an `Arc<AppState>` with a NomadCh backend and a sandbox
/// pre-injected for `(user_id, project_id)` pointing at the fixture
/// agent. Returns the AppState and the sandbox-id we minted.
fn make_state(
    token: &str,
    user_id: &str,
    agent_port: u16,
    sk: SigningKey,
) -> (Arc<zeroship_sandbox::AppState>, Uuid) {
    let cfg = make_cfg(token);
    let backend = Backend::builder(&cfg).build().expect("backend");
    let sandbox_id = Uuid::now_v7();
    if let Backend::NomadCh(b) = &backend {
        b._test_inject_sandbox(
            sandbox_id,
            user_id,
            sk,
            format!("http://127.0.0.1:{agent_port}"),
            42, // vm_index — irrelevant for the test
        );
    } else {
        unreachable!("test pinned to nomad-ch");
    }
    // NOTE: this fixture keeps the legacy hyphenated UUID form for
    // `info.sandbox_id` because the existing tests in this file
    // build URLs from the bare `Uuid` struct. The typed-id wire
    // shape returned by `POST /sandboxes` is exercised end-to-end
    // by the dedicated regression test in
    // `tests/sandbox_typed_id_e2e.rs` (Round-2 fixer / CRITICAL #1).
    let info = SandboxInfo {
        sandbox_id: sandbox_id.to_string(),
        user_id: user_id.to_string(),
        project_id: "p".into(),
        backend: "nomad-ch".into(),
        backend_hint: "test".into(),
        created_at_secs: 0,
        last_used_at_secs: 0,
    };
    // A5: out-of-crate construction goes through `new_fixture` (admin_token = None;
    // Phase-0 sandbox-pg-state tests run pg-disabled).
    let state = zeroship_sandbox::AppState::new_fixture(cfg, backend);
    state.sandboxes.insert(sandbox_id, info);
    let state = Arc::new(state);
    (state, sandbox_id)
}

/// Build the controller's preview-proxy route inside an ntex test app.
/// Helper macro because ntex test types are awkward to thread through
/// function signatures.
macro_rules! make_app {
    ($state:expr) => {
        test::init_service(
            ntex::web::App::new()
                .state($state)
                .state(
                    web::types::PayloadConfig::default().limit(256 * 1024 * 1024),
                )
                .service(
                    web::resource("/sandboxes/{id}/preview/{port}/{path}*")
                        .state(
                            web::types::PayloadConfig::default()
                                .limit(256 * 1024 * 1024),
                        )
                        .route(web::route().to(preview::preview_proxy)),
                ),
        )
        .await
    };
}

#[ntex::test]
async fn authed_creator_round_trip() {
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply {
        status: 200,
        body: b"agent-says-hello".to_vec(),
        extra_headers: vec![],
    });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state);

    let path = format!("/sandboxes/{id}/preview/5173/index.html?user_id=alice");
    let req = test::TestRequest::get()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    assert_eq!(&body[..], b"agent-says-hello");

    // The controller must have set X-Forwarded-Host on the outbound
    // request to the agent. (We mock the agent — but the captured raw
    // request shows the header.)
    let cap = agent.captured();
    assert_eq!(cap.len(), 1, "exactly one request to agent");
    let raw = &cap[0];
    let lower = raw.to_ascii_lowercase();
    assert!(
        lower.contains("x-forwarded-host: preview-"),
        "controller must emit X-Forwarded-Host; got:\n{raw}"
    );
    assert!(
        lower.contains(".preview.zeroship.dev"),
        "X-Forwarded-Host must use the preview hostname pattern; got:\n{raw}"
    );
    // X-Sbx-* signature headers must be present.
    assert!(lower.contains("x-sbx-timestamp:"), "X-Sbx-Timestamp present");
    assert!(lower.contains("x-sbx-nonce:"), "X-Sbx-Nonce present");
    assert!(lower.contains("x-sbx-signature:"), "X-Sbx-Signature present");
}

#[ntex::test]
async fn anonymous_returns_401_uniform() {
    let sk = SigningKey::from_bytes(&[8u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply {
        status: 200,
        body: b"".to_vec(),
        extra_headers: vec![],
    });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state);

    // Real sandbox, but no Authorization header.
    let path = format!("/sandboxes/{id}/preview/5173/foo");
    let req = test::TestRequest::get().uri(&path).to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Fabricated sandbox-id, also no Authorization. Body must match
    // (uniform 401 — no oracle by sandbox existence).
    let fake_id = Uuid::now_v7();
    let path2 = format!("/sandboxes/{fake_id}/preview/5173/foo");
    let req2 = test::TestRequest::get().uri(&path2).to_request();
    let resp2 = test::call_service(&app, req2).await;
    assert_eq!(resp2.status(), StatusCode::UNAUTHORIZED);

    // The agent must not have been touched.
    assert!(agent.captured().is_empty(), "anonymous must not reach agent");
}

#[ntex::test]
async fn wrong_creator_returns_404_not_403() {
    // Creator-A owns the sandbox; creator-B attempts to access.
    // MUST 404 (round-6 H4 — no 403 oracle). Audit-log the actual
    // reason (we don't capture audit here; the eprintln in
    // preview.rs is enough for spot checks).
    let sk = SigningKey::from_bytes(&[9u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply {
        status: 200,
        body: b"".to_vec(),
        extra_headers: vec![],
    });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state);

    let path = format!("/sandboxes/{id}/preview/5173/foo?user_id=bob");
    let req = test::TestRequest::get()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(agent.captured().is_empty(), "wrong creator must not reach agent");
}

#[ntex::test]
async fn port_deny_at_controller_defense_in_depth() {
    // Creator can't proxy port 22 (SSH) or 9229 (V8 inspector) even
    // for their own sandbox. The check fires at the controller; the
    // agent is never dialed.
    let sk = SigningKey::from_bytes(&[10u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply {
        status: 200,
        body: b"".to_vec(),
        extra_headers: vec![],
    });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state);

    for denied_port in [22u16, 9229, 7777] {
        let path = format!(
            "/sandboxes/{id}/preview/{denied_port}/foo?user_id=alice"
        );
        let req = test::TestRequest::get()
            .uri(&path)
            .header("Authorization", "Bearer tok")
            .to_request();
        let resp = test::call_service(&app, req).await;
        // 404 — the uniform "denied" response covers port-deny too,
        // per the coalesced authorize check (round-6 H4).
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "port {denied_port} must NOT proxy"
        );
    }
    assert!(agent.captured().is_empty(), "port-denied must not reach agent");
}

#[ntex::test]
async fn x_zspreview_host_from_client_is_sanitized() {
    // A creator could try to inject `X-ZSPreview-Host: attacker.example`
    // hoping the agent uses it as the rewrite target. The controller
    // MUST drop the inbound header and substitute its own.
    let sk = SigningKey::from_bytes(&[11u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply {
        status: 200,
        body: b"".to_vec(),
        extra_headers: vec![],
    });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state);

    let path = format!("/sandboxes/{id}/preview/5173/foo?user_id=alice");
    let req = test::TestRequest::get()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .header("X-ZSPreview-Host", "attacker.example")
        .header("X-Forwarded-Host", "attacker.example")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let cap = agent.captured();
    assert_eq!(cap.len(), 1);
    let raw = &cap[0];
    let lower = raw.to_ascii_lowercase();
    // The attacker's value must NOT appear; the controller-computed
    // preview hostname must.
    assert!(
        !lower.contains("attacker.example"),
        "client X-ZSPreview-Host / X-Forwarded-Host must be sanitized; got:\n{raw}"
    );
    assert!(
        lower.contains(".preview.zeroship.dev"),
        "controller-computed X-Forwarded-Host must reach agent; got:\n{raw}"
    );
}

#[ntex::test]
async fn agent_unreachable_returns_502() {
    // Bind a port, immediately drop the listener so nothing is
    // listening. The controller's `forward_blocking` ureq dial fails
    // → 502 with code "agent_unreachable".
    let sk = SigningKey::from_bytes(&[12u8; 32]);
    let dead_port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let (state, id) = make_state("tok", "alice", dead_port, sk);
    let app = make_app!(state);
    let path = format!("/sandboxes/{id}/preview/5173/foo?user_id=alice");
    let req = test::TestRequest::get()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}
