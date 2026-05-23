//! Admin/operator API end-to-end tests.
//!
//! Pg-gated tests need a live Postgres at PG_TEST_URL (defaults to
//! the docker-compose fixture); pure-auth tests run without pg by
//! disabling the database in the AppState fixture.
//!
//! ```bash
//! docker compose up -d postgres
//! PG_TEST_URL='postgres://postgres:zeroship@localhost:5440/zeroship' \
//!     cargo test -p zeroship-sandbox --test sandbox_admin_e2e -- \
//!     --ignored --test-threads=1
//! ```
//!
//! The non-pg auth tests run in the default unit/integration suite —
//! they don't need pg and exercise admin_check's bearer-from-file
//! semantics.

use std::sync::Arc;

use compio_postgres::{Pool, PoolConfig};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_sandbox::admin_handlers;
use zeroship_sandbox::backend::{Backend, SandboxInfo};
use zeroship_sandbox::config::{ApiToken, SandboxConfig};
use zeroship_sandbox::db::Database;

const TEST_URL_ENV: &str = "PG_TEST_URL";
const DEFAULT_URL: &str = "postgres://postgres:zeroship@localhost:5440/zeroship";

fn test_url() -> String {
    std::env::var(TEST_URL_ENV).unwrap_or_else(|_| DEFAULT_URL.to_string())
}

fn make_cfg(token: &str) -> SandboxConfig {
    // A7 (deferred): `token` is `pub(crate)`; out-of-crate construction
    // goes through `SandboxConfig::new_fixture` (the only legal public
    // constructor for a non-`from_env` config) + the `with_token`
    // builder. Other fields stay `pub` so tests that vary
    // `snapshot_enabled` / `port` continue to mutate via direct field
    // assignment on the returned value.
    SandboxConfig::new_fixture().with_token(ApiToken::new(token))
}

fn make_state(database: Option<Arc<Database>>) -> Arc<zeroship_sandbox::AppState> {
    make_state_with_admin_token(database, None)
}

fn make_state_with_admin_token(
    database: Option<Arc<Database>>,
    admin_token: Option<String>,
) -> Arc<zeroship_sandbox::AppState> {
    let cfg = make_cfg("ignored-creator-token");
    let backend = Backend::from_config(&cfg).expect("backend");
    // A5: `admin_token` is `pub(crate)`; out-of-crate construction
    // goes through `AppState::new_fixture` + the `with_admin_token`
    // builder (which rejects empty strings — the post-Round-4
    // footgun).
    // A6b: `database` is `pub(crate)`; set via `with_database` instead
    // of struct-field assignment. `None` skips the builder entirely
    // so the field stays at its `new_fixture` default.
    let mut state = zeroship_sandbox::AppState::new_fixture(cfg, backend);
    if let Some(db) = database {
        state = state.with_database(db);
    }
    let state = state
        .with_admin_token(admin_token)
        .expect("admin_token must be non-empty when Some");
    Arc::new(state)
}

// `AdminTokenFile`, `EnvGuard`, and `ENV_LOCK` are gone. The
// `load_admin_token` function is now pure — it takes an
// `Option<&Path>` — so its tests live inline in `lib.rs` and don't
// need to mutate process env. The integration tests in this file
// inject `admin_token` directly into `AppState` via
// `make_state_with_admin_token`.

macro_rules! make_app {
    ($state:expr) => {
        test::init_service(
            ntex::web::App::new()
                .state($state)
                .service(
                    web::resource("/admin/sandboxes")
                        .route(web::get().to(admin_handlers::list_all_sandboxes)),
                )
                .service(
                    web::resource("/admin/sandboxes/{id}")
                        .route(web::get().to(admin_handlers::get_sandbox_detail)),
                )
                .service(
                    web::resource("/admin/users/{user_id}/sandboxes")
                        .route(web::get().to(admin_handlers::list_user_sandboxes)),
                )
                .service(
                    web::resource("/admin/users/{user_id}/shares")
                        .route(web::get().to(admin_handlers::list_user_shares)),
                )
                .service(
                    web::resource("/admin/users/{user_id}/export")
                        .route(web::get().to(admin_handlers::export_user)),
                )
                .service(
                    web::resource("/admin/users/{user_id}")
                        .route(web::delete().to(admin_handlers::delete_user)),
                )
                .service(
                    web::resource("/admin/hosts")
                        .route(web::get().to(admin_handlers::list_hosts)),
                )
                .service(
                    web::resource("/admin/sandboxes/{id}/snapshot")
                        .route(web::post().to(admin_handlers::snapshot_sandbox)),
                )
                .service(
                    web::resource("/admin/sandboxes/{id}/wake")
                        .route(web::post().to(admin_handlers::wake_sandbox)),
                ),
        )
        .await
    };
}

// ────────────────────────────────────────────────────────────────────
// Auth tests — no pg required.
// ────────────────────────────────────────────────────────────────────

#[ntex::test]
async fn admin_disabled_when_token_unset() {
    // Admin token is read once at boot; tests inject it directly via
    // `make_state_with_admin_token`.
    // `None` is the disabled-by-absence shape.
    let state = make_state(None);
    let svc = make_app!(state);

    let req = test::TestRequest::default()
        .uri("/admin/sandboxes")
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[ntex::test]
async fn admin_401_without_bearer() {
    let token = "operator-bearer-12345";
    let state = make_state_with_admin_token(None, Some(token.to_string()));
    let svc = make_app!(state);

    let req = test::TestRequest::default()
        .uri("/admin/sandboxes")
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[ntex::test]
async fn admin_401_with_wrong_bearer() {
    let token = "right-token-12345";
    let state = make_state_with_admin_token(None, Some(token.to_string()));
    let svc = make_app!(state);

    let req = test::TestRequest::default()
        .uri("/admin/sandboxes")
        .header("authorization", "Bearer not-the-right-token-aa")
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[ntex::test]
async fn admin_401_with_wrong_bearer_same_length() {
    // Bearer compare must behave the same when lengths match. A wrong
    // bearer of the same length must still 401.
    let token = "right-token-1234567890";
    let wrong = "wrong-token-9876543210";
    assert_eq!(token.len(), wrong.len(), "test fixture lengths must match");
    let state = make_state_with_admin_token(None, Some(token.to_string()));
    let svc = make_app!(state);

    let req = test::TestRequest::default()
        .uri("/admin/sandboxes")
        .header("authorization", &format!("Bearer {wrong}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[ntex::test]
async fn admin_503_with_correct_bearer_when_pg_disabled() {
    // Even with the right bearer, no database means 503 (pg
    // integration disabled). Distinct from the 503 returned for
    // "admin api disabled" — operators see the difference in the
    // response body.
    let token = "right-token-12345";
    let state = make_state_with_admin_token(None, Some(token.to_string()));
    let svc = make_app!(state);

    let req = test::TestRequest::default()
        .uri("/admin/sandboxes")
        .header("authorization", &format!("Bearer {token}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = test::read_body(resp).await;
    let s = std::str::from_utf8(&body).unwrap();
    assert!(s.contains("pg integration disabled"), "got body: {s}");
}

// ────────────────────────────────────────────────────────────────────
// Pg-gated round-trips.
// ────────────────────────────────────────────────────────────────────

async fn reset_schema(url: &str) {
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(url, cfg)
        .await
        .expect("connect for reset");
    let client = pool.get().await.expect("acquire for reset");
    client
        .batch_execute("DROP SCHEMA IF EXISTS sandbox CASCADE")
        .await
        .expect("drop schema");
}

async fn migrated_db() -> Database {
    let url = test_url();
    reset_schema(&url).await;
    let db = Database::from_test_config(url, true, 30).await.unwrap();
    db.run_pending_migrations().await.unwrap();
    db.upsert_host("test-admin", "nomad-ch").await.unwrap();
    db
}

async fn seed_sandbox(
    db: &Database,
    user_id: &str,
) -> (String, Uuid) {
    let sandbox_id = Uuid::now_v7();
    let info = SandboxInfo {
        sandbox_id: format!(
            "sbx_{}",
            zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
        ),
        user_id: user_id.to_string(),
        project_id: zeroship_core::typed_id::generate("prj"),
        backend: "nomad-ch".into(),
        backend_hint: "test".into(),
        created_at_secs: 1_700_000_000,
        last_used_at_secs: 1_700_000_000,
    };
    db.insert_sandbox(
        &info,
        db.host_id(),
        &"a".repeat(32),
        Some("http://127.0.0.1:1"),
        Some(1),
    )
    .await
    .unwrap();
    let typed = info.sandbox_id.clone();
    (typed, sandbox_id)
}

async fn seed_event(
    db: &Database,
    sandbox_typed: &str,
    user_id: &str,
    kind: &str,
) {
    let event = zeroship_sandbox::db::Database::new_event(
        sandbox_typed,
        user_id,
        kind,
        "{}".into(),
    );
    db.insert_event(&event).await.unwrap();
}

#[ntex::test]
#[ignore = "needs Postgres; Phase-3 admin list round-trip"]
async fn admin_list_sandboxes_filters_by_user() {
    let db = migrated_db().await;
    let user_a = zeroship_core::typed_id::generate("usr");
    let user_b = zeroship_core::typed_id::generate("usr");
    let _ = seed_sandbox(&db, &user_a).await;
    let _ = seed_sandbox(&db, &user_a).await;
    let _ = seed_sandbox(&db, &user_b).await;

    let token = "list-bearer-aaaaaa";
    let state = make_state_with_admin_token(Some(Arc::new(db)), Some(token.to_string()));
    let svc = make_app!(state);

    // No filter → all 3.
    let req = test::TestRequest::default()
        .uri("/admin/sandboxes")
        .header("authorization", &format!("Bearer {token}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let count = v["count"].as_i64().unwrap();
    assert!(count >= 3, "expected at least 3 sandboxes; got {count}");

    // Filter by user_a → 2.
    let req = test::TestRequest::default()
        .uri(&format!("/admin/sandboxes?user_id={user_a}"))
        .header("authorization", &format!("Bearer {token}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["count"].as_i64().unwrap(), 2);

    // Filter by user_b → 1.
    let req = test::TestRequest::default()
        .uri(&format!("/admin/sandboxes?user_id={user_b}"))
        .header("authorization", &format!("Bearer {token}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    let body = test::read_body(resp).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["count"].as_i64().unwrap(), 1);
}

#[ntex::test]
#[ignore = "needs Postgres; Phase-3 admin user-shortcut round-trip"]
async fn admin_user_sandboxes_returns_only_users_rows() {
    let db = migrated_db().await;
    let user_a = zeroship_core::typed_id::generate("usr");
    let user_b = zeroship_core::typed_id::generate("usr");
    let _ = seed_sandbox(&db, &user_a).await;
    let _ = seed_sandbox(&db, &user_b).await;

    let token = "user-bearer-aaaaaa";
    let state = make_state_with_admin_token(Some(Arc::new(db)), Some(token.to_string()));
    let svc = make_app!(state);

    let req = test::TestRequest::default()
        .uri(&format!("/admin/users/{user_a}/sandboxes"))
        .header("authorization", &format!("Bearer {token}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["count"].as_i64().unwrap(), 1);
    let arr = v["sandboxes"].as_array().unwrap();
    assert_eq!(arr[0]["user_id"].as_str(), Some(user_a.as_str()));
}

#[ntex::test]
#[ignore = "needs Postgres; Phase-3 admin export shape"]
async fn admin_user_export_includes_all_categories() {
    let db = migrated_db().await;
    let user_a = zeroship_core::typed_id::generate("usr");
    let (sandbox_typed, _) = seed_sandbox(&db, &user_a).await;
    seed_event(&db, &sandbox_typed, &user_a, "created").await;
    seed_event(&db, &sandbox_typed, &user_a, "started").await;

    let token = "export-bearer-aaaaaa";
    let state = make_state_with_admin_token(Some(Arc::new(db)), Some(token.to_string()));
    let svc = make_app!(state);

    let req = test::TestRequest::default()
        .uri(&format!("/admin/users/{user_a}/export"))
        .header("authorization", &format!("Bearer {token}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["user_id"].as_str(), Some(user_a.as_str()));
    assert!(v["exported_at_secs"].is_i64());
    assert_eq!(v["sandboxes"].as_array().unwrap().len(), 1);
    // events should include both seeded events.
    let events_returned = v["events_returned"].as_i64().unwrap();
    assert_eq!(events_returned, 2, "two seeded events must surface");
    assert_eq!(v["events_truncated"].as_bool().unwrap(), false);
    assert_eq!(v["events_total"].as_i64().unwrap(), 2);
    assert!(v["deleted_sandboxes"].as_array().unwrap().is_empty());
}

#[ntex::test]
#[ignore = "needs Postgres; Phase-3 admin export — empty user"]
async fn admin_user_export_for_unknown_user_returns_empty_arrays() {
    let db = migrated_db().await;
    let user_a = zeroship_core::typed_id::generate("usr");

    let token = "empty-bearer-aaaaaa";
    let state = make_state_with_admin_token(Some(Arc::new(db)), Some(token.to_string()));
    let svc = make_app!(state);

    let req = test::TestRequest::default()
        .uri(&format!("/admin/users/{user_a}/export"))
        .header("authorization", &format!("Bearer {token}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["sandboxes"].as_array().unwrap().len(), 0);
    assert_eq!(v["shares"].as_array().unwrap().len(), 0);
    assert_eq!(v["events_total"].as_i64().unwrap(), 0);
}

#[ntex::test]
#[ignore = "needs Postgres; Phase-3 admin GDPR delete cascade"]
async fn admin_gdpr_delete_removes_all_user_data() {
    let db = migrated_db().await;
    let user_a = zeroship_core::typed_id::generate("usr");
    let user_b = zeroship_core::typed_id::generate("usr");
    let (sandbox_typed_a, _) = seed_sandbox(&db, &user_a).await;
    let (sandbox_typed_b, _) = seed_sandbox(&db, &user_b).await;
    for kind in ["created", "started", "stopped"] {
        seed_event(&db, &sandbox_typed_a, &user_a, kind).await;
    }
    seed_event(&db, &sandbox_typed_b, &user_b, "created").await;

    let token = "delete-bearer-aaaaaa";
    let state = make_state_with_admin_token(Some(Arc::new(db)), Some(token.to_string()));
    let svc = make_app!(state);

    let req = test::TestRequest::default()
        .uri(&format!("/admin/users/{user_a}"))
        .method(ntex::http::Method::DELETE)
        .header("authorization", &format!("Bearer {token}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["deleted_user_id"].as_str(), Some(user_a.as_str()));
    assert_eq!(v["sandboxes_tombstoned"].as_i64().unwrap(), 1);
    // 3 'created'/'started'/'stopped' + 1 'gdpr.delete_user' audit = 4
    let events_deleted = v["events_deleted"].as_i64().unwrap();
    assert_eq!(events_deleted, 3, "events_deleted should be the pre-audit count");

    // Cross-user-leak guard: user_b's row + event remain.
    let url = test_url();
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();

    let row = client
        .query_one(
            "SELECT count(*)::BIGINT FROM sandbox.sandboxes WHERE user_id = $1::TEXT",
            &[&user_a],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 0, "user_a sandboxes must be gone");
    let row = client
        .query_one(
            "SELECT count(*)::BIGINT FROM sandbox.sandboxes WHERE user_id = $1::TEXT",
            &[&user_b],
        )
        .await
        .unwrap();
    assert_eq!(
        row.get::<_, i64>(0),
        1,
        "user_b sandbox must NOT be touched (cross-user leak guard)"
    );

    // Tombstone landed.
    let row = client
        .query_one(
            "SELECT count(*)::BIGINT FROM sandbox.deleted_sandboxes WHERE user_id = $1::TEXT",
            &[&user_a],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 1, "tombstone must be present");

    // Audit row written under the gdpr role for the same TX.
    let row = client
        .query_one(
            "SELECT count(*)::BIGINT FROM sandbox.events \
              WHERE user_id = $1::TEXT AND kind = 'gdpr.delete_user'",
            &[&user_a],
        )
        .await
        .unwrap();
    assert_eq!(
        row.get::<_, i64>(0),
        1,
        "gdpr.delete_user audit row must exist post-delete"
    );

    // user_b's events untouched.
    let row = client
        .query_one(
            "SELECT count(*)::BIGINT FROM sandbox.events WHERE user_id = $1::TEXT",
            &[&user_b],
        )
        .await
        .unwrap();
    assert_eq!(
        row.get::<_, i64>(0),
        1,
        "user_b events must NOT be touched (cross-user leak guard)"
    );
}

#[ntex::test]
#[ignore = "needs Postgres; Phase-3 GDPR delete idempotent on absent user"]
async fn admin_gdpr_delete_unknown_user_returns_zero_counts() {
    let db = migrated_db().await;
    let user_a = zeroship_core::typed_id::generate("usr");

    let token = "idem-bearer-aaaaaa";
    let state = make_state_with_admin_token(Some(Arc::new(db)), Some(token.to_string()));
    let svc = make_app!(state);

    let req = test::TestRequest::default()
        .uri(&format!("/admin/users/{user_a}"))
        .method(ntex::http::Method::DELETE)
        .header("authorization", &format!("Bearer {token}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["sandboxes_tombstoned"].as_i64().unwrap(), 0);
    assert_eq!(v["shares_deleted"].as_i64().unwrap(), 0);
    assert_eq!(v["events_deleted"].as_i64().unwrap(), 0);
    assert_eq!(v["sealed_files_unlinked"].as_i64().unwrap(), 0);
}

#[ntex::test]
#[ignore = "needs Postgres; Phase-3 admin hosts list"]
async fn admin_hosts_lists_known_hosts() {
    let db = migrated_db().await;

    let token = "hosts-bearer-aaaaaa";
    let state = make_state_with_admin_token(Some(Arc::new(db)), Some(token.to_string()));
    let svc = make_app!(state);

    let req = test::TestRequest::default()
        .uri("/admin/hosts")
        .header("authorization", &format!("Bearer {token}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["count"].as_i64().unwrap() >= 1);
    let hosts = v["hosts"].as_array().unwrap();
    assert!(
        hosts
            .iter()
            .any(|h| h["status"].as_str() == Some("alive")),
        "at least one host must be alive"
    );
}

#[ntex::test]
#[ignore = "needs Postgres; Phase-3 admin sandbox-detail"]
async fn admin_sandbox_detail_returns_pg_row_and_in_memory_flag() {
    let db = migrated_db().await;
    let user_a = zeroship_core::typed_id::generate("usr");
    let (sandbox_typed, _uuid) = seed_sandbox(&db, &user_a).await;

    let token = "detail-bearer-aaaaaa";
    let state = make_state_with_admin_token(Some(Arc::new(db)), Some(token.to_string()));
    let svc = make_app!(state);

    let req = test::TestRequest::default()
        .uri(&format!("/admin/sandboxes/{sandbox_typed}"))
        .header("authorization", &format!("Bearer {token}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v["row"]["sandbox_id"].as_str(),
        Some(sandbox_typed.as_str())
    );
    assert_eq!(v["row"]["user_id"].as_str(), Some(user_a.as_str()));
    // `in_memory` is false because we didn't populate the registry.
    assert_eq!(v["row"]["in_memory"].as_bool().unwrap(), false);
    assert!(v["agent_version"].is_null());
}

// Boot-time admin-token loader tests now live inline in lib.rs's
// `#[cfg(test)] mod tests` because `load_admin_token` is `pub(crate)`,
// pure, and takes a path argument without mutating process env.

// ────────────────────────────────────────────────────────────────────
// Regression tests
// ────────────────────────────────────────────────────────────────────

/// Regression test: the `gdpr.delete_user` audit row
/// must NOT surface in `GET /admin/users/{user_id}/export`. Pre-fix,
/// the export's events query selected every row WHERE user_id = $1, so
/// after a GDPR delete the user could re-discover their own erasure
/// record on a subsequent export.
#[ntex::test]
#[ignore = "needs Postgres; export filters audit row"]
async fn admin_user_export_after_gdpr_delete_excludes_audit_row() {
    let db = migrated_db().await;
    let user_a = zeroship_core::typed_id::generate("usr");
    let (sandbox_typed, _) = seed_sandbox(&db, &user_a).await;
    seed_event(&db, &sandbox_typed, &user_a, "created").await;

    let token = "regress-3-bearer-aa";
    let state = make_state_with_admin_token(Some(Arc::new(db)), Some(token.to_string()));
    let svc = make_app!(state);

    // Issue the GDPR delete first.
    let req = test::TestRequest::default()
        .uri(&format!("/admin/users/{user_a}"))
        .method(ntex::http::Method::DELETE)
        .header("authorization", &format!("Bearer {token}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Now export the (now-erased) user. The audit row exists in
    // sandbox.events but must be hidden from the export.
    let req = test::TestRequest::default()
        .uri(&format!("/admin/users/{user_a}/export"))
        .header("authorization", &format!("Bearer {token}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let events = v["events"].as_array().unwrap();
    assert!(
        events.iter().all(|e| e["kind"].as_str() != Some("gdpr.delete_user")),
        "post-erasure export must not include the gdpr.delete_user audit row; got {events:?}"
    );
    // events_total must mirror the same filter — counter should not
    // count the audit row either, otherwise an operator inspecting
    // events_total post-erasure would see "1 event" with nothing in
    // the array (confusing).
    assert_eq!(
        v["events_total"].as_i64().unwrap(),
        0,
        "events_total must reflect user-facing events only (audit row excluded)"
    );
}

/// Regression test: `DELETE /admin/users/{user_id}`
/// for a user with ZERO sandboxes must not write a synthetic
/// `sbx_<random>` id into `sandbox.events`. Pre-fix, the audit row
/// minted a never-existed typed-id and inserted it as `events.sandbox_id`,
/// polluting `idx_events_sandbox_ts` with an unmatchable key. Post-fix
/// (migration 0005) the column is NULLable and the audit row writes NULL.
#[ntex::test]
#[ignore = "needs Postgres; no synthetic sandbox_id"]
async fn admin_gdpr_delete_user_with_zero_sandboxes_writes_no_synthetic_id() {
    let db = migrated_db().await;
    let user_a = zeroship_core::typed_id::generate("usr");
    // No seed_sandbox call — the user has zero sandboxes.

    let token = "regress-4-bearer-aa";
    let state = make_state_with_admin_token(Some(Arc::new(db)), Some(token.to_string()));
    let svc = make_app!(state);

    let req = test::TestRequest::default()
        .uri(&format!("/admin/users/{user_a}"))
        .method(ntex::http::Method::DELETE)
        .header("authorization", &format!("Bearer {token}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["sandboxes_tombstoned"].as_i64().unwrap(), 0);

    // Inspect sandbox.events directly. The audit row must exist
    // (Art. 30 RoPA) but its sandbox_id must be NULL — not a
    // synthetic typed-id. Pre-fix, every zero-sandbox GDPR delete
    // produced exactly one polluted index entry per call.
    let url = test_url();
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();

    let row = client
        .query_one(
            "SELECT count(*)::BIGINT FROM sandbox.events \
              WHERE user_id = $1::TEXT \
                AND kind = 'gdpr.delete_user' \
                AND sandbox_id IS NULL",
            &[&user_a],
        )
        .await
        .unwrap();
    assert_eq!(
        row.get::<_, i64>(0),
        1,
        "audit row must exist with sandbox_id = NULL (no synthetic id)"
    );

    // Belt-and-suspenders: NO row exists with a non-NULL synthetic
    // sandbox_id for this user. (Pre-fix this was 1.)
    let row = client
        .query_one(
            "SELECT count(*)::BIGINT FROM sandbox.events \
              WHERE user_id = $1::TEXT \
                AND kind = 'gdpr.delete_user' \
                AND sandbox_id IS NOT NULL",
            &[&user_a],
        )
        .await
        .unwrap();
    assert_eq!(
        row.get::<_, i64>(0),
        0,
        "no synthetic sandbox_id may have been written"
    );
}

// ────────────────────────────────────────────────────────────────────
// Phase A snapshot wiring smoke. Verifies the admin endpoint moves
// past the 501 `feature_disabled` envelope when wiring is on.
// ────────────────────────────────────────────────────────────────────

fn make_state_with_snapshot_wiring(
    admin_token: Option<String>,
) -> Arc<zeroship_sandbox::AppState> {
    let mut cfg = make_cfg("ignored-creator-token");
    cfg.snapshot_enabled = true;
    let backend = Backend::from_config(&cfg).expect("backend");
    // Build the snapshot trio identically to AppState::from_config's
    // production path, but with an in-memory L1 root so the test
    // doesn't litter `/var/zeroship`.
    let l1_root = std::env::temp_dir().join(format!(
        "zsbx-admin-wiring-{}-{}",
        std::process::id(),
        Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&l1_root).unwrap();
    let store: std::sync::Arc<dyn zeroship_sandbox::snapshot_store::SnapshotStore> =
        std::sync::Arc::new(
            zeroship_sandbox::snapshot_store::LocalDiskSnapshotStore::new(l1_root),
        );
    let ch: std::sync::Arc<dyn zeroship_sandbox::snapshot_handler::ChRemoteClient> =
        std::sync::Arc::new(zeroship_sandbox::snapshot_handler::MockChRemoteClient::default());
    let rb: std::sync::Arc<dyn zeroship_sandbox::restore_handler::RestoreBackend> =
        std::sync::Arc::new(zeroship_sandbox::restore_handler::RealRestoreBackend::new(
            cfg.nomad_ch.clone(),
            cfg.memory_mb,
            cfg.cpus,
        ));
    // A5: out-of-crate construction goes through `new_fixture` +
    // `with_admin_token`.
    // A6b: snapshot trio fields are `pub(crate)`; set via the
    // `with_snapshot_store` / `with_ch_remote` / `with_restore_backend`
    // builders instead of struct-field assignment.
    let state = zeroship_sandbox::AppState::new_fixture(cfg, backend)
        .with_snapshot_store(store)
        .with_ch_remote(ch)
        .with_restore_backend(rb);
    let state = state
        .with_admin_token(admin_token)
        .expect("admin_token must be non-empty when Some");
    Arc::new(state)
}

#[ntex::test]
async fn snapshot_endpoint_moves_past_501_when_enabled() {
    // Phase B smoke: with snapshot_enabled = true and the trio
    // populated, the admin endpoint must not return 501
    // feature_disabled. With `database = None` (test shape), the
    // handler returns 503 because the wiring requires a real pg
    // handle to read/CAS the row. Production flips the bit through
    // `AppState::from_config` so the trio + db land together.
    //
    // Pre-Phase-B this returned 503 `wiring_partial` from a
    // hand-rolled short-circuit; Phase B replaced that short-circuit
    // with the real handler call, so the failure mode now comes from
    // the handler's own preflight (database missing → 503).
    let token = "admin-bearer-aaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let state = make_state_with_snapshot_wiring(Some(token.to_string()));
    let svc = make_app!(state);

    let sid = zeroship_core::typed_id::generate("sbx");
    let req = test::TestRequest::default()
        .method(ntex::http::Method::POST)
        .uri(&format!("/admin/sandboxes/{sid}/snapshot"))
        .header("authorization", &format!("Bearer {token}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_ne!(
        resp.status(),
        StatusCode::NOT_IMPLEMENTED,
        "Phase B wiring must move past 501 feature_disabled"
    );
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "expected 503 (database None in test shape); got {}",
        resp.status()
    );
}

#[ntex::test]
async fn snapshot_endpoint_returns_501_when_disabled() {
    // Confirms the off path is unchanged: snapshot_enabled = false
    // → 501 feature_disabled, regardless of admin auth.
    let token = "admin-bearer-bbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let state = make_state_with_admin_token(None, Some(token.to_string()));
    let svc = make_app!(state);

    let sid = zeroship_core::typed_id::generate("sbx");
    let req = test::TestRequest::default()
        .method(ntex::http::Method::POST)
        .uri(&format!("/admin/sandboxes/{sid}/snapshot"))
        .header("authorization", &format!("Bearer {token}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
}
