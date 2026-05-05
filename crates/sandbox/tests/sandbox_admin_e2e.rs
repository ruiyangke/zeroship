#![allow(unsafe_code)]

//! Phase-3 admin/operator API end-to-end tests.
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
use zeroship_sandbox::config::{ApiToken, K8sConfig, NomadCHConfig, SandboxConfig};
use zeroship_sandbox::db::Database;
use zeroship_sandbox::registry::SandboxRegistry;

const TEST_URL_ENV: &str = "PG_TEST_URL";
const DEFAULT_URL: &str = "postgres://postgres:zeroship@localhost:5440/zeroship";

fn test_url() -> String {
    std::env::var(TEST_URL_ENV).unwrap_or_else(|_| DEFAULT_URL.to_string())
}

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

fn make_state(database: Option<Arc<Database>>) -> Arc<zeroship_sandbox::AppState> {
    let cfg = make_cfg("ignored-creator-token");
    let backend = Backend::from_config(&cfg).expect("backend");
    let registry = SandboxRegistry::new();
    Arc::new(zeroship_sandbox::AppState {
        config: cfg,
        sandboxes: registry,
        backend,
        mint_rate_limiter: Some(
            zeroship_sandbox::preview_share_handlers::MintRateLimiter::new(),
        ),
        database,
        persist: None,
        shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    })
}

/// Build an admin-token file at `path` with `token` content; chmod
/// 0o400 on Unix. Cleanup on drop.
struct AdminTokenFile {
    path: std::path::PathBuf,
}

impl AdminTokenFile {
    fn new(token: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "zsbx-admin-token-{}-{}",
            std::process::id(),
            Uuid::now_v7().simple()
        ));
        std::fs::write(&path, token).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        }
        Self { path }
    }
}

impl Drop for AdminTokenFile {
    fn drop(&mut self) {
        // Best-effort: chmod 0600 first so we can unlink, then remove.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(
                &self.path,
                std::fs::Permissions::from_mode(0o600),
            );
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Process-global env-mutation lock. The `SANDBOX_ADMIN_TOKEN_PATH`
/// env var is process-wide; cargo runs integration tests in parallel
/// by default. This lock serializes every guard's set/restore so a
/// peer test never observes a half-mutated env state. Tests still
/// pass with `--test-threads=1` (recommended) but no longer require
/// it.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hold the env var while the guard is alive; restore on drop. The
/// guard owns the [`ENV_LOCK`] mutex for its entire lifetime so the
/// env-var snapshot is observable only by this test.
struct EnvGuard {
    name: &'static str,
    prev: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvGuard {
    fn set(name: &'static str, value: &str) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os(name);
        // SAFETY: ENV_LOCK serializes every concurrent test that
        // mutates the same env var; the lock is held for the guard's
        // lifetime so no peer can observe a partial state.
        unsafe {
            std::env::set_var(name, value);
        }
        Self {
            name,
            prev,
            _lock: lock,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: see EnvGuard::set; the lock is still held.
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var(self.name, v),
                None => std::env::remove_var(self.name),
            }
        }
    }
}

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
                ),
        )
        .await
    };
}

// ────────────────────────────────────────────────────────────────────
// Auth tests — no pg required.
// ────────────────────────────────────────────────────────────────────

#[ntex::test]
async fn admin_disabled_when_token_path_unset() {
    // EnvGuard with empty value mutates+takes lock; the second mutation
    // here drops the var entirely. The lock guards both.
    let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var_os("SANDBOX_ADMIN_TOKEN_PATH");
    // SAFETY: ENV_LOCK serialises mutations; lock held until end-of-scope.
    unsafe {
        std::env::remove_var("SANDBOX_ADMIN_TOKEN_PATH");
    }
    let state = make_state(None);
    let svc = make_app!(state);

    let req = test::TestRequest::default()
        .uri("/admin/sandboxes")
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

    // Restore.
    // SAFETY: see above.
    unsafe {
        if let Some(v) = prev {
            std::env::set_var("SANDBOX_ADMIN_TOKEN_PATH", v);
        }
    }
    drop(lock);
}

#[ntex::test]
async fn admin_401_without_bearer() {
    let token = "operator-bearer-12345";
    let token_file = AdminTokenFile::new(token);
    let _g = EnvGuard::set(
        "SANDBOX_ADMIN_TOKEN_PATH",
        token_file.path.to_str().unwrap(),
    );
    let state = make_state(None);
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
    let token_file = AdminTokenFile::new(token);
    let _g = EnvGuard::set(
        "SANDBOX_ADMIN_TOKEN_PATH",
        token_file.path.to_str().unwrap(),
    );
    let state = make_state(None);
    let svc = make_app!(state);

    let req = test::TestRequest::default()
        .uri("/admin/sandboxes")
        .header("authorization", "Bearer not-the-right-token-aa")
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
    let token_file = AdminTokenFile::new(token);
    let _g = EnvGuard::set(
        "SANDBOX_ADMIN_TOKEN_PATH",
        token_file.path.to_str().unwrap(),
    );
    let state = make_state(None);
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
    let token_file = AdminTokenFile::new(token);
    let _g = EnvGuard::set(
        "SANDBOX_ADMIN_TOKEN_PATH",
        token_file.path.to_str().unwrap(),
    );
    let state = make_state(Some(Arc::new(db)));
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
    let token_file = AdminTokenFile::new(token);
    let _g = EnvGuard::set(
        "SANDBOX_ADMIN_TOKEN_PATH",
        token_file.path.to_str().unwrap(),
    );
    let state = make_state(Some(Arc::new(db)));
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
    let token_file = AdminTokenFile::new(token);
    let _g = EnvGuard::set(
        "SANDBOX_ADMIN_TOKEN_PATH",
        token_file.path.to_str().unwrap(),
    );
    let state = make_state(Some(Arc::new(db)));
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
    let token_file = AdminTokenFile::new(token);
    let _g = EnvGuard::set(
        "SANDBOX_ADMIN_TOKEN_PATH",
        token_file.path.to_str().unwrap(),
    );
    let state = make_state(Some(Arc::new(db)));
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
    let token_file = AdminTokenFile::new(token);
    let _g = EnvGuard::set(
        "SANDBOX_ADMIN_TOKEN_PATH",
        token_file.path.to_str().unwrap(),
    );
    let state = make_state(Some(Arc::new(db)));
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
    let token_file = AdminTokenFile::new(token);
    let _g = EnvGuard::set(
        "SANDBOX_ADMIN_TOKEN_PATH",
        token_file.path.to_str().unwrap(),
    );
    let state = make_state(Some(Arc::new(db)));
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
    let token_file = AdminTokenFile::new(token);
    let _g = EnvGuard::set(
        "SANDBOX_ADMIN_TOKEN_PATH",
        token_file.path.to_str().unwrap(),
    );
    let state = make_state(Some(Arc::new(db)));
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
    let token_file = AdminTokenFile::new(token);
    let _g = EnvGuard::set(
        "SANDBOX_ADMIN_TOKEN_PATH",
        token_file.path.to_str().unwrap(),
    );
    let state = make_state(Some(Arc::new(db)));
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
