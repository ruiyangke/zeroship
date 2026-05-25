//! regression test — typed-id round-trip.
//!
//! `POST /sandboxes` returns `sandbox_id: "sbx_<base62>"`. Every
//! follow-up call (GET / DELETE / exec / file-tree / files /
//! preview-share) MUST accept that exact string back. The earlier
//! handler regression (`parse_uuid` → `s.parse::<Uuid>()`) rejected
//! the typed-id and 400'ed every follow-up. This test would have
//! caught the regression — it would have been impossible to ship.
//!
//! The test does not boot a real Nomad cluster; it injects a fake
//! sandbox into the in-memory registry with the typed-id wire shape
//! AND populates the nomad-ch backend's internal state via
//! `_test_inject_sandbox`. Then it exercises the parse-and-ownership
//! layer of every public endpoint that takes a path-bound sandbox id.
//!
//! What we assert at every endpoint:
//!   - The typed-id form parses past the boundary check (no
//!     400 "invalid sandbox id").
//!   - For endpoints that don't actually talk to the agent (GET,
//!     mint-share-list, ...), we get the expected success status.
//!   - For backend-touching endpoints (exec, file-tree, files), we
//!     accept either Ok or a 5xx that came FROM the backend (i.e.
//!     past parse) — what we explicitly reject is 400, which is
//!     the symptom of the regression.

use std::sync::Arc;

use ed25519_dalek::SigningKey;
use ntex::http::StatusCode;
use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_sandbox::backend::{Backend, SandboxInfo};
use zeroship_sandbox::config::{ApiToken, SandboxConfig};
use zeroship_sandbox::{handlers, preview_share_handlers};

fn make_cfg(token: &str) -> SandboxConfig {
    // A7 (deferred): see sandbox_admin_e2e.rs::make_cfg for rationale.
    // `token` is `pub(crate)`; construct via the public fixture
    // constructor + `with_token` builder.
    SandboxConfig::new_fixture().with_token(ApiToken::new(token))
}

/// Build an AppState pre-loaded with one sandbox whose registry id
/// is `sandbox_uuid` and whose `SandboxInfo.sandbox_id` is the
/// typed-id form `sbx_<base62>` (matching what `POST /sandboxes`
/// would return). `user_id` and `project_id` are typed-ids too so
/// the handlers' boundary check passes.
fn make_state_with_typed_id_fixture(token: &str) -> (Arc<zeroship_sandbox::AppState>, Uuid, String, String) {
    let cfg = make_cfg(token);
    let backend = Backend::builder(&cfg).build().expect("backend");
    let sandbox_uuid = Uuid::now_v7();
    let sandbox_typed = format!(
        "sbx_{}",
        zeroship_core::typed_id::uuid_to_base62(&sandbox_uuid)
    );
    let user_typed = zeroship_core::typed_id::generate("usr");
    let project_typed = zeroship_core::typed_id::generate("prj");

    let sk = SigningKey::from_bytes(&[7u8; 32]);
    if let Backend::NomadCh(b) = &backend {
        b._test_inject_sandbox(
            sandbox_uuid,
            &user_typed,
            sk,
            "http://127.0.0.1:1".to_string(), // unreachable on purpose
            42,
        );
    } else {
        unreachable!("test pinned to nomad-ch");
    }

    // CRITICAL: `info.sandbox_id` carries the typed-id wire form, which
    // is what `POST /sandboxes` returns to the caller. Endpoints that
    // surface info.sandbox_id in responses MUST keep this exact string.
    let info = SandboxInfo {
        sandbox_id: sandbox_typed.clone(),
        user_id: user_typed.clone(),
        project_id: project_typed.clone(),
        backend: "nomad-ch".into(),
        backend_hint: "test".into(),
        created_at_secs: 0,
        last_used_at_secs: 0,
    };

    // A5: out-of-crate construction goes through `new_fixture`.
    let state = zeroship_sandbox::AppState::new_fixture(cfg, backend);
    state.sandboxes.insert(sandbox_uuid, info);
    let state = Arc::new(state);
    (state, sandbox_uuid, sandbox_typed, user_typed)
}

/// Build a test app wiring every public endpoint that takes a
/// path-bound sandbox id. Mirrors the full route table in
/// `crates/sandbox/src/main.rs` so the test exercises the actual
/// route matchers, not a stripped-down subset.
macro_rules! make_app {
    ($state:expr) => {
        test::init_service(
            ntex::web::App::new()
                .state($state)
                .state(
                    web::types::PayloadConfig::default().limit(256 * 1024 * 1024),
                )
                .service(
                    web::resource("/sandboxes")
                        .route(web::post().to(handlers::create_sandbox))
                        .route(web::get().to(handlers::list_sandboxes)),
                )
                .service(
                    web::resource("/sandboxes/{id}")
                        .route(web::get().to(handlers::get_sandbox))
                        .route(web::delete().to(handlers::stop_sandbox)),
                )
                .service(
                    web::resource("/sandboxes/{id}/exec")
                        .route(web::post().to(handlers::exec)),
                )
                .service(
                    web::resource("/sandboxes/{id}/file-tree")
                        .route(web::get().to(handlers::file_tree)),
                )
                .service(
                    web::resource("/sandboxes/{id}/files/{path}*")
                        .route(web::get().to(handlers::read_file))
                        .route(web::put().to(handlers::write_file))
                        .route(web::delete().to(handlers::delete_file)),
                )
                .service(
                    web::resource("/sandboxes/{id}/preview/{port}/share")
                        .route(
                            web::post()
                                .to(preview_share_handlers::mint_share),
                        )
                        .route(
                            web::get()
                                .to(preview_share_handlers::list_share),
                        )
                        .route(
                            web::delete()
                                .to(preview_share_handlers::revoke_all_share),
                        ),
                ),
        )
        .await
    };
}

/// `GET /sandboxes/{typed_id}?user_id=…` is the authoritative
/// round-trip. It only touches the in-memory registry — no backend
/// call — so a non-200 here is unambiguously a parse-layer bug.
#[ntex::test]
async fn get_round_trips_typed_id() {
    let (state, _uuid, sandbox_typed, user_typed) =
        make_state_with_typed_id_fixture("tok");
    let app = make_app!(state);

    let path = format!("/sandboxes/{sandbox_typed}?user_id={user_typed}");
    let req = test::TestRequest::get()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET on typed-id sandbox MUST return 200 (round-2 fixer / CRITICAL #1)"
    );

    // : the sandbox_id in the response
    // body MUST be the same `sbx_<base62>` form the caller passed in
    // (not a hyphenated UUID). Audit tools that join on payload-id
    // depend on this consistency.
    let body = test::read_body(resp).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(
        v.get("sandbox_id").and_then(|x| x.as_str()),
        Some(sandbox_typed.as_str()),
        "GET response sandbox_id must round-trip the typed-id form"
    );
}

/// `GET /sandboxes/{typed_id}?user_id=…` 404s when the user_id
/// doesn't match — confirms the parse succeeded (else it would 400).
#[ntex::test]
async fn get_typed_id_with_wrong_user_returns_404_not_400() {
    let (state, _uuid, sandbox_typed, _user) =
        make_state_with_typed_id_fixture("tok");
    let app = make_app!(state);

    let other_user = zeroship_core::typed_id::generate("usr");
    let path = format!("/sandboxes/{sandbox_typed}?user_id={other_user}");
    let req = test::TestRequest::get()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .to_request();
    let resp = test::call_service(&app, req).await;
    // 404 (the registry rejected the user_id mismatch). 400 here
    // would mean the parse layer choked, which is the regression.
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "wrong user_id must yield 404, not 400 (else parse_sandbox_id_to_uuid is broken)"
    );
}

/// Bare hyphenated UUID is still accepted (back-compat). Internal
/// tooling and existing fixtures rely on this.
#[ntex::test]
async fn get_round_trips_hyphenated_uuid_back_compat() {
    let (state, sandbox_uuid, _typed, user_typed) =
        make_state_with_typed_id_fixture("tok");
    let app = make_app!(state);

    let hyphenated = sandbox_uuid.to_string();
    let path = format!("/sandboxes/{hyphenated}?user_id={user_typed}");
    let req = test::TestRequest::get()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "hyphenated UUID must still parse for back-compat"
    );
}

/// Garbage path-id 400s with the new error message.
#[ntex::test]
async fn get_garbage_id_returns_400() {
    let (state, _uuid, _typed, user_typed) =
        make_state_with_typed_id_fixture("tok");
    let app = make_app!(state);

    let path = format!("/sandboxes/garbage_xyz?user_id={user_typed}");
    let req = test::TestRequest::get()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Wrong-prefix typed-id (e.g. `usr_…` in a sandbox-id slot) MUST 400.
/// This is the path-traversal-hardening boundary check (Invariant 2).
#[ntex::test]
async fn get_wrong_prefix_typed_id_returns_400() {
    let (state, _uuid, _typed, user_typed) =
        make_state_with_typed_id_fixture("tok");
    let app = make_app!(state);

    let usr_in_sandbox_slot = zeroship_core::typed_id::generate("usr");
    let path = format!("/sandboxes/{usr_in_sandbox_slot}?user_id={user_typed}");
    let req = test::TestRequest::get()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Round-trip the typed-id on routes that go through `require_owner`
/// and into the backend. We don't care if the backend op succeeds
/// (the fixture agent is unreachable on purpose); we care that the
/// path parses and the request reaches the backend layer rather than
/// 400'ing out at parse time. A 5xx is the expected post-parse
/// outcome here; a 400 means the regression is back.
#[ntex::test]
async fn exec_typed_id_passes_parse_layer() {
    let (state, _uuid, sandbox_typed, user_typed) =
        make_state_with_typed_id_fixture("tok");
    let app = make_app!(state);

    let path = format!("/sandboxes/{sandbox_typed}/exec?user_id={user_typed}");
    let req = test::TestRequest::post()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .header("Content-Type", "application/json")
        .set_payload(r#"{"cmd":"echo hello"}"#)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_ne!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "exec on typed-id MUST NOT 400 — parse layer must accept it"
    );
    assert_ne!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "exec on typed-id MUST NOT 404 — owner check must pass"
    );
    // It will 5xx because the fixture agent at 127.0.0.1:1 is
    // unreachable — that's the BACKEND reporting, which is exactly
    // what we want: the request reached the backend layer.
    assert!(
        resp.status().is_server_error(),
        "expected 5xx from unreachable agent; got {:?}",
        resp.status()
    );
}

#[ntex::test]
async fn file_tree_typed_id_passes_parse_layer() {
    let (state, _uuid, sandbox_typed, user_typed) =
        make_state_with_typed_id_fixture("tok");
    let app = make_app!(state);

    let path = format!("/sandboxes/{sandbox_typed}/file-tree?user_id={user_typed}");
    let req = test::TestRequest::get()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_ne!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "file-tree on typed-id MUST NOT 400"
    );
    assert_ne!(resp.status(), StatusCode::NOT_FOUND);
}

#[ntex::test]
async fn read_file_typed_id_passes_parse_layer() {
    let (state, _uuid, sandbox_typed, user_typed) =
        make_state_with_typed_id_fixture("tok");
    let app = make_app!(state);

    let path = format!("/sandboxes/{sandbox_typed}/files/foo.txt?user_id={user_typed}");
    let req = test::TestRequest::get()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .to_request();
    let resp = test::call_service(&app, req).await;
    // backend.read_file will fail (unreachable agent) → expect 4xx
    // OTHER than 400 (could be 404 from "file not found" parsing in
    // the err mapper) — the only thing we want to rule out is the
    // parse-layer 400 with body containing 'invalid sandbox id'.
    if resp.status() == StatusCode::BAD_REQUEST {
        let body = test::read_body(resp).await;
        let s = String::from_utf8_lossy(&body);
        assert!(
            !s.contains("invalid sandbox id"),
            "read_file 400 must not be from parse_sandbox_id_to_uuid; got body: {s}"
        );
    }
}

#[ntex::test]
async fn write_file_typed_id_passes_parse_layer() {
    let (state, _uuid, sandbox_typed, user_typed) =
        make_state_with_typed_id_fixture("tok");
    let app = make_app!(state);

    let path = format!("/sandboxes/{sandbox_typed}/files/foo.txt?user_id={user_typed}");
    let req = test::TestRequest::put()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .header("Content-Type", "application/octet-stream")
        .set_payload(b"hello".to_vec())
        .to_request();
    let resp = test::call_service(&app, req).await;
    if resp.status() == StatusCode::BAD_REQUEST {
        let body = test::read_body(resp).await;
        let s = String::from_utf8_lossy(&body);
        assert!(
            !s.contains("invalid sandbox id"),
            "write_file 400 must not be from parse_sandbox_id_to_uuid; got body: {s}"
        );
    }
}

#[ntex::test]
async fn delete_file_typed_id_passes_parse_layer() {
    let (state, _uuid, sandbox_typed, user_typed) =
        make_state_with_typed_id_fixture("tok");
    let app = make_app!(state);

    let path = format!("/sandboxes/{sandbox_typed}/files/foo.txt?user_id={user_typed}");
    let req = test::TestRequest::delete()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .to_request();
    let resp = test::call_service(&app, req).await;
    if resp.status() == StatusCode::BAD_REQUEST {
        let body = test::read_body(resp).await;
        let s = String::from_utf8_lossy(&body);
        assert!(
            !s.contains("invalid sandbox id"),
            "delete_file 400 must not be from parse_sandbox_id_to_uuid; got body: {s}"
        );
    }
}

/// Mint-share is in `preview_share_handlers` and runs a separate
/// parse path; verify the typed-id form passes.
#[ntex::test]
async fn list_share_typed_id_passes_parse_layer() {
    let (state, _uuid, sandbox_typed, user_typed) =
        make_state_with_typed_id_fixture("tok");
    let app = make_app!(state);

    let path = format!("/sandboxes/{sandbox_typed}/preview/5173/share?user_id={user_typed}");
    let req = test::TestRequest::get()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .to_request();
    let resp = test::call_service(&app, req).await;
    // list_share returns 200 with [] for an unknown / no-shares
    // sandbox; what matters is it's NOT 400 (parse failure).
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "list_share on typed-id must succeed; got {:?}",
        resp.status()
    );
}

/// `DELETE /sandboxes/{typed_id}` — the stop endpoint reaches into
/// the backend. We accept any non-400 response (ok / 5xx); what we
/// reject is parse-layer 400. Also asserts the response payload
/// echoes the typed-id form.
#[ntex::test]
async fn stop_typed_id_passes_parse_and_response_uses_typed_id() {
    let (state, _uuid, sandbox_typed, user_typed) =
        make_state_with_typed_id_fixture("tok");
    let app = make_app!(state);

    let path = format!("/sandboxes/{sandbox_typed}?user_id={user_typed}");
    let req = test::TestRequest::delete()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .to_request();
    let resp = test::call_service(&app, req).await;
    // backend.stop will probably succeed or 5xx — Nomad isn't there
    // but the in-memory state was injected. What we reject is 400.
    assert_ne!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "stop on typed-id MUST NOT 400 — parse layer must accept it"
    );

    if resp.status() == StatusCode::OK {
        // The response sandbox_id must echo the typed-id form, not a
        // hyphenated UUID. The earlier implementation returned the
        // wrong shape here.
        let body = test::read_body(resp).await;
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(
            v.get("sandbox_id").and_then(|x| x.as_str()),
            Some(sandbox_typed.as_str()),
            "stop response sandbox_id must round-trip the typed-id form"
        );
    }
}
