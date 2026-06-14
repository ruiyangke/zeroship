//! HTTP-level integration tests for the `.zship` deploy endpoint.
//!
//! Where `deploy_test.rs` exercises `deploy::ingest()` directly,
//! these tests drive the FULL handler path:
//!
//!     auth check → content-type check → streaming body
//!     → tmp file → mmap → ingest → registry write → response
//!
//! via `ntex::web::test::init_service`. The aim is to catch
//! regressions in the wiring (status codes, body shape, side
//! effects on tmp dir / blob store) rather than re-cover the
//! ingest logic itself.
//!
//! All cases gate on `CONTROL_TEST_DB` — Postgres is required to
//! construct `AppState` (registry/env_store/auth all dial the DB
//! on startup), so the file is a silent skip in dev.
//!
//! Note on the missing case: a "body exceeds the 256 MiB cap"
//! test isn't here — sending 256+ MiB through ntex test plumbing
//! is too slow for unit-test speed. The same code path is
//! exercised more cheaply by `stream_tmp_tests::cap_exceeded_removes_tmp_file`
//! in `crates/control/src/api.rs`.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use ntex::http::StatusCode;
use ntex::web::{self, test};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use zeroship_bundle::{
    AssetEntry, AuthConfig, BlobStore, LocalDiskBlobStore, Manifest,
    ManifestMetadata, ScopeDef, WorkerCode,
};
use zeroship_control::{
    api, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString,
    StripeStore,
};

mod common;

// ---------------------------------------------------------------------------
// Test gating
// ---------------------------------------------------------------------------

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB")
        .or_else(|_| std::env::var("PG_TEST_URL"))
        .ok()
}

// ---------------------------------------------------------------------------
// Tmp directory helpers
// ---------------------------------------------------------------------------

fn tmpdir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "zship-http-test-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&p).expect("mkdir tmp");
    p
}

/// Seed a throwaway owner user so `create_app` (which binds an owner membership
/// FKed to `zeroship.users`) succeeds. These tests exercise the deploy HTTP
/// path, not authz, so the owner identity is immaterial.
async fn seed_owner(state: &AppState, label: &str) -> Uuid {
    let owner_id = Uuid::new_v4();
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2::citext, $3)",
            &[
                &owner_id,
                &format!("{label}-owner-{owner_id}@zeroship.test"),
                &label,
            ],
        )
        .await
        .expect("seed owner user");
    owner_id
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

fn dir_is_empty(path: &PathBuf) -> bool {
    match std::fs::read_dir(path) {
        Ok(mut iter) => iter.next().is_none(),
        // If the directory doesn't exist that's also "empty enough"
        // for the purposes of asserting nothing was written.
        Err(_) => true,
    }
}

// ---------------------------------------------------------------------------
// `.zship` builder — copied from `deploy_test.rs`. The two test files
// can't easily share helpers (separate compilation units, no shared
// `mod common`) so we keep an exact copy here.
// ---------------------------------------------------------------------------

/// Build a minimal valid manifest referencing `assets` + an optional
/// worker bundle, with `built_at` set to a fixed RFC 3339 string.
fn manifest_for(
    worker_hash: Option<&str>,
    assets: &[(&str, &str, &str)], // (path, hash, content_type)
) -> Manifest {
    let mut a: HashMap<String, AssetEntry> = HashMap::new();
    for (p, h, ct) in assets {
        a.insert(
            (*p).to_string(),
            AssetEntry {
                hash: (*h).to_string(),
                content_type: (*ct).to_string(),
                size: 0,
                cache: None,
                updated_at: 0,
                variants: HashMap::new(),
            },
        );
    }
    Manifest {
        version: 1,
        deploy_hash: None,
        worker: worker_hash.map(|h| WorkerCode {
            entry: "index.js".into(),
            modules: HashMap::from([("index.js".to_string(), h.to_string())]),
        }),
        resources: HashMap::new(),
        schemas: HashMap::new(),
        aliases: HashMap::new(),
        transformer: None,
        assets: a,
        runtime_assets: HashMap::new(),
        asset_version: 0,
        sourcemaps: HashMap::new(),
        auth: Default::default(),
        metadata: ManifestMetadata {
            compiler: Some("test".into()),
            built_at: "2026-04-29T00:00:00Z".into(),
        },
        exports: None,
    }
}

/// Like [`manifest_for`] but with a worker module and a set of declared
/// `auth.scopes` ids (each gets a throwaway `label`). Used by the
/// scope-collision deploy tests.
fn manifest_with_scopes(worker_hash: &str, scope_ids: &[&str]) -> Manifest {
    let mut m = manifest_for(Some(worker_hash), &[]);
    m.auth = AuthConfig {
        scopes: scope_ids
            .iter()
            .map(|id| ScopeDef {
                id: (*id).to_string(),
                label: format!("label for {id}"),
                description: None,
            })
            .collect(),
    };
    m
}

/// Pack `(name, bytes)` entries into a tar archive, then zstd-compress
/// the whole thing. Manifest goes first if `manifest_first`; otherwise
/// the entries are appended in the order given.
fn build_zship(
    manifest_bytes: &[u8],
    blobs: &[(String, Vec<u8>)],
    manifest_first: bool,
) -> Vec<u8> {
    let mut tar_buf: Vec<u8> = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        let write_manifest = |b: &mut tar::Builder<&mut Vec<u8>>| {
            let mut header = tar::Header::new_gnu();
            header.set_size(manifest_bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            b.append_data(&mut header, "manifest.json", manifest_bytes)
                .expect("append manifest");
        };
        let write_blobs = |b: &mut tar::Builder<&mut Vec<u8>>| {
            for (hash, bytes) in blobs {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                b.append_data(&mut header, format!("blobs/{hash}"), bytes.as_slice())
                    .expect("append blob");
            }
        };
        if manifest_first {
            write_manifest(&mut builder);
            write_blobs(&mut builder);
        } else {
            write_blobs(&mut builder);
            write_manifest(&mut builder);
        }
        builder.finish().expect("tar finish");
    }
    let mut compressed: Vec<u8> = Vec::new();
    {
        let mut enc = zstd::Encoder::new(&mut compressed, 0).expect("zstd enc");
        enc.write_all(&tar_buf).expect("zstd write");
        enc.finish().expect("zstd finish");
    }
    compressed
}

// ---------------------------------------------------------------------------
// AppState construction
// ---------------------------------------------------------------------------

/// Test fixtures held alongside the `Arc<AppState>` so the caller can
/// reach into the underlying tmp dirs / stores for assertions.
struct Fixture {
    state: Arc<AppState>,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
    blob_store: Arc<LocalDiskBlobStore>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

async fn build_test_state(db_url: &str, label: &str) -> Fixture {
    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("dtmp-{label}"));

    // Concrete LocalDiskBlobStore handle: kept in the fixture so the
    // assertion `bs.has_blob(...)` works without having to downcast
    // the trait-object stored on AppState.
    let blob_store_concrete = Arc::new(
        LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"),
    );

    let registry = Registry::new(db_url).await.expect("registry");
    zeroship_control::bootstrap_console::seed_plans(&registry).await.expect("seed built-in plans");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false)
        .expect("env store");
    let stripe_store = StripeStore::new(registry.clone());

    let blob_store: Arc<dyn BlobStore> = blob_store_concrete.clone();

    let (control_pg_client, control_pg_conn) =
        compio_postgres::connect(db_url, compio_postgres::NoTls)
            .await
            .expect("control-pg connect");
    compio::runtime::spawn(async move {
        let _ = control_pg_conn.run().await;
    })
    .detach();
    let control_pg = Arc::new(control_pg_client);

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        blob_store,
        control_key: SecretString::new("test-control-key".to_string()),
        master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
        stripe_webhook_secret: SecretString::new(String::new()),
        stripe_secret_key: SecretString::new(String::new()),
        stripe_base_url: "https://api.stripe.com".to_string(),
        worker_urls: Vec::new(),
        worker_key: SecretString::new(String::new()),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        insecure_dev: false,
        trust_proxy: false,
        deploy_tmp_dir: deploy_tmp_dir.clone(),
        control_pg,
        hydra_admin_url: "http://127.0.0.1:4445".to_string(),
        app_base_domain: "zeroship.localhost".to_string(),
        trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
        expected_oauth_audience: "control.zeroship.ai".to_string(),
        static_policies: zeroship_authz::load_platform_policies()
            .expect("bundled authz policies parse"),
        pat_issuer: Arc::new(zeroship_control::token_handlers::PatIssuer::dev_insecure()),
        hydra_introspector: Arc::new(zeroship_core::hydra::HydraIntrospector::new(
            "http://127.0.0.1:9",
        )),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        metering_provider: zeroship_control::metering::provider::build_provider(
            &zeroship_control::metering::provider::MeteringProviderConfig::native(),
        )
        .expect("native provider builds"),
        pairwise_salt: [0u8; 32],
    });

    Fixture {
        state,
        blob_root,
        deploy_tmp_dir,
        blob_store: blob_store_concrete,
    }
}

// ---------------------------------------------------------------------------
// Test cases
// ---------------------------------------------------------------------------

#[compio::test]
async fn deploy_happy_path_returns_200_with_deploy_hash() {
    let Some(db_url) = db_url() else {
        eprintln!("[deploy_http_test] CONTROL_TEST_DB not set — skipping");
        return;
    };

    let fx = build_test_state(&db_url, "happy").await;

    // Real app row in the test DB.
    let owner_id = seed_owner(&fx.state, "httpd").await;
    let app_name = format!("httpd-{}", &Uuid::new_v4().simple().to_string()[..10]);
    let record = fx
        .state
        .registry
        .create_app(&app_name, &zeroship_control::bootstrap_console::free_plan_id(), &owner_id)
        .await
        .expect("create app");
    let app_id = record.id;

    // Build a `.zship` carrying a single asset + worker module.
    let html = b"<!doctype html><body>http happy path</body>";
    let html_hash = sha256_hex(html);
    let server = b"export default { fetch() { return new Response('ok'); } }";
    let server_hash = sha256_hex(server);

    let manifest = manifest_for(
        Some(&server_hash),
        &[("/index.html", &html_hash, "text/html")],
    );
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let blobs = vec![
        (html_hash.clone(), html.to_vec()),
        (server_hash.clone(), server.to_vec()),
    ];
    let body = build_zship(&manifest_bytes, &blobs, true);

    // Mount a single deploy route under the test app. PayloadConfig
    // mirrors what main.rs configures so the upper bound matches prod.
    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .service(
                web::resource("/api/apps/{id}/deploy")
                    .state(web::types::PayloadConfig::new(
                        zeroship_control::deploy::MAX_COMPRESSED_BYTES,
                    ))
                    .route(web::post().to(api::deploy)),
            ),
    )
    .await;

    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{app_id}/deploy"))
        .header("authorization", pat.bearer())
        .header("content-type", "application/x-zship")
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "expected 200");

    // ntex 3.x has no `read_body_json` helper — read bytes + parse.
    let bytes = test::read_body(resp).await;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).expect("response is JSON");
    let deploy_hash = json
        .get("deploy_hash")
        .and_then(|v| v.as_str())
        .expect("deploy_hash present");
    assert_eq!(deploy_hash.len(), 64, "deploy_hash is sha256 hex");
    assert_eq!(
        json.get("blobs_uploaded").and_then(|v| v.as_u64()),
        Some(2),
        "both blobs uploaded fresh"
    );
    assert!(
        json.get("blobs_deduped").and_then(|v| v.as_u64()).is_some(),
        "blobs_deduped present"
    );

    // Side effects: both blobs hit the on-disk blob store.
    assert!(
        fx.blob_store.has_blob(&html_hash).await.unwrap(),
        "html blob persisted"
    );
    assert!(
        fx.blob_store.has_blob(&server_hash).await.unwrap(),
        "server blob persisted"
    );

    // Tmp dir must be empty after success — the handler unlinks the
    // staging file regardless of outcome.
    assert!(
        dir_is_empty(&fx.deploy_tmp_dir),
        "deploy_tmp_dir should be empty after a successful deploy, got {:?}",
        std::fs::read_dir(&fx.deploy_tmp_dir)
            .map(|it| it.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
            .unwrap_or_default(),
    );

    // Cleanup: drop the app row so reruns under the same DB don't
    // collect noise.
    let _ = fx.state.registry.delete_app(&app_id).await;
    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn deploy_wrong_content_type_returns_415_without_consuming_body() {
    let Some(db_url) = db_url() else {
        eprintln!("[deploy_http_test] CONTROL_TEST_DB not set — skipping");
        return;
    };

    let fx = build_test_state(&db_url, "ct").await;
    let app_id = Uuid::new_v4(); // route never reaches DB lookup
    let body: Vec<u8> = b"this is not a zship payload".to_vec();

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .service(
                web::resource("/api/apps/{id}/deploy")
                    .state(web::types::PayloadConfig::new(
                        zeroship_control::deploy::MAX_COMPRESSED_BYTES,
                    ))
                    .route(web::post().to(api::deploy)),
            ),
    )
    .await;

    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{app_id}/deploy"))
        .header("authorization", pat.bearer())
        .header("content-type", "application/octet-stream")
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "expected 415"
    );

    // The handler rejects on content-type BEFORE streaming. Tmp dir
    // must therefore be untouched.
    assert!(
        dir_is_empty(&fx.deploy_tmp_dir),
        "deploy_tmp_dir must be empty on 415 (body should not stream)",
    );
    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn deploy_missing_auth_returns_401_without_consuming_body() {
    let Some(db_url) = db_url() else {
        eprintln!("[deploy_http_test] CONTROL_TEST_DB not set — skipping");
        return;
    };

    let fx = build_test_state(&db_url, "auth").await;
    let app_id = Uuid::new_v4();
    let body: Vec<u8> = b"won't ever be looked at".to_vec();

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .service(
                web::resource("/api/apps/{id}/deploy")
                    .state(web::types::PayloadConfig::new(
                        zeroship_control::deploy::MAX_COMPRESSED_BYTES,
                    ))
                    .route(web::post().to(api::deploy)),
            ),
    )
    .await;

    // No authorization header at all.
    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{app_id}/deploy"))
        .header("content-type", "application/x-zship")
        .set_payload(body.clone())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "no bearer → 401",
    );
    assert!(
        dir_is_empty(&fx.deploy_tmp_dir),
        "no bearer must not stream body to tmp",
    );

    // Invalid PAT bearer should also reject — same status.
    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{app_id}/deploy"))
        .header("authorization", "Bearer not-a-valid-pat")
        .header("content-type", "application/x-zship")
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "wrong bearer → 401",
    );
    assert!(
        dir_is_empty(&fx.deploy_tmp_dir),
        "wrong bearer must not stream body to tmp",
    );
}

#[compio::test]
async fn deploy_manifest_not_first_returns_400() {
    let Some(db_url) = db_url() else {
        eprintln!("[deploy_http_test] CONTROL_TEST_DB not set — skipping");
        return;
    };

    let fx = build_test_state(&db_url, "mf").await;

    // Real app — the handler walks all the way through ingest to
    // produce the structured BadRequest, so the app must exist for
    // the registry path to behave normally up to the rejection.
    let owner_id = seed_owner(&fx.state, "httpmf").await;
    let app_name = format!("httpmf-{}", &Uuid::new_v4().simple().to_string()[..10]);
    let record = fx
        .state
        .registry
        .create_app(&app_name, &zeroship_control::bootstrap_console::free_plan_id(), &owner_id)
        .await
        .expect("create app");
    let app_id = record.id;

    // Build a payload with manifest LAST.
    let payload = b"some payload bytes";
    let hash = sha256_hex(payload);
    let manifest = manifest_for(None, &[]);
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let blobs = vec![(hash, payload.to_vec())];
    let body = build_zship(&manifest_bytes, &blobs, /*manifest_first=*/ false);

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .service(
                web::resource("/api/apps/{id}/deploy")
                    .state(web::types::PayloadConfig::new(
                        zeroship_control::deploy::MAX_COMPRESSED_BYTES,
                    ))
                    .route(web::post().to(api::deploy)),
            ),
    )
    .await;

    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{app_id}/deploy"))
        .header("authorization", pat.bearer())
        .header("content-type", "application/x-zship")
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "expected 400");

    let bytes = test::read_body(resp).await;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).expect("response is JSON");
    let error = json
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        error.contains("manifest must be first"),
        "expected 'manifest must be first…' error, got {error:?}",
    );

    // Even on error, the handler unlinks the tmp file.
    assert!(
        dir_is_empty(&fx.deploy_tmp_dir),
        "deploy_tmp_dir must be empty after a rejected deploy",
    );

    let _ = fx.state.registry.delete_app(&app_id).await;
    pat.cleanup(&fx.state).await;
}

/// Spec §5.1/§5.2 regression: a manifest declaring a scope that collides
/// with the closed platform-delegated vocabulary (`billing:read`) MUST be
/// REJECTED by the deploy handler with a 4xx — NOT accepted with a 200 and
/// then silently un-provisioned. `ingest` only enforces scope-id FORMAT, so
/// the collision guard has to run in the handler before the manifest commit.
#[compio::test]
async fn deploy_colliding_scope_returns_400_invalid_scope() {
    let Some(db_url) = db_url() else {
        eprintln!("[deploy_http_test] CONTROL_TEST_DB not set — skipping");
        return;
    };

    let fx = build_test_state(&db_url, "scopecollide").await;

    let owner_id = seed_owner(&fx.state, "httpsc").await;
    let app_name = format!("httpsc-{}", &Uuid::new_v4().simple().to_string()[..10]);
    let record = fx
        .state
        .registry
        .create_app(&app_name, &zeroship_control::bootstrap_console::free_plan_id(), &owner_id)
        .await
        .expect("create app");
    let app_id = record.id;

    // Well-formed scope id, but `billing:read` is in the closed
    // `Scope::parse` platform vocabulary — must collide.
    let server = b"export default { fetch() { return new Response('ok'); } }";
    let server_hash = sha256_hex(server);
    let manifest = manifest_with_scopes(&server_hash, &["billing:read"]);
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let blobs = vec![(server_hash.clone(), server.to_vec())];
    let body = build_zship(&manifest_bytes, &blobs, true);

    let app = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::resource("/api/apps/{id}/deploy")
                .state(web::types::PayloadConfig::new(
                    zeroship_control::deploy::MAX_COMPRESSED_BYTES,
                ))
                .route(web::post().to(api::deploy)),
        ),
    )
    .await;

    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{app_id}/deploy"))
        .header("authorization", pat.bearer())
        .header("content-type", "application/x-zship")
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "colliding scope must hard-fail the deploy with 400"
    );

    let bytes = test::read_body(resp).await;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).expect("response is JSON");
    assert_eq!(
        json.get("error").and_then(|v| v.as_str()),
        Some("invalid_scope"),
        "error tag must be invalid_scope, got {json:?}"
    );
    assert_eq!(
        json.get("scope").and_then(|v| v.as_str()),
        Some("billing:read"),
        "offending scope id surfaced to the creator"
    );

    // The deploy must NOT have committed: deploy_hash stays unset.
    let after = fx
        .state
        .registry
        .get_app(&app_id)
        .await
        .expect("get app")
        .expect("app still exists");
    assert!(
        after.deploy_hash.is_none(),
        "a rejected-scope deploy must not commit a deploy_hash"
    );

    assert!(
        dir_is_empty(&fx.deploy_tmp_dir),
        "deploy_tmp_dir must be empty after a rejected deploy",
    );

    let _ = fx.state.registry.delete_app(&app_id).await;
    pat.cleanup(&fx.state).await;
}

/// Companion to the collision test: a well-formed, NON-colliding declared
/// scope (`read:billing`, the mirror order, which is NOT in the platform
/// vocabulary) deploys cleanly with a 200.
#[compio::test]
async fn deploy_noncolliding_scope_returns_200() {
    let Some(db_url) = db_url() else {
        eprintln!("[deploy_http_test] CONTROL_TEST_DB not set — skipping");
        return;
    };

    let fx = build_test_state(&db_url, "scopeok").await;

    let owner_id = seed_owner(&fx.state, "httpok").await;
    let app_name = format!("httpok-{}", &Uuid::new_v4().simple().to_string()[..10]);
    let record = fx
        .state
        .registry
        .create_app(&app_name, &zeroship_control::bootstrap_console::free_plan_id(), &owner_id)
        .await
        .expect("create app");
    let app_id = record.id;

    let server = b"export default { fetch() { return new Response('ok'); } }";
    let server_hash = sha256_hex(server);
    let manifest = manifest_with_scopes(&server_hash, &["read:billing"]);
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let blobs = vec![(server_hash.clone(), server.to_vec())];
    let body = build_zship(&manifest_bytes, &blobs, true);

    let app = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::resource("/api/apps/{id}/deploy")
                .state(web::types::PayloadConfig::new(
                    zeroship_control::deploy::MAX_COMPRESSED_BYTES,
                ))
                .route(web::post().to(api::deploy)),
        ),
    )
    .await;

    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{app_id}/deploy"))
        .header("authorization", pat.bearer())
        .header("content-type", "application/x-zship")
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "non-colliding scope must deploy cleanly"
    );

    let bytes = test::read_body(resp).await;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).expect("response is JSON");
    assert_eq!(
        json.get("deploy_hash").and_then(|v| v.as_str()).map(str::len),
        Some(64),
        "deploy committed with a sha256 deploy_hash"
    );

    let _ = fx.state.registry.delete_app(&app_id).await;
    pat.cleanup(&fx.state).await;
}
