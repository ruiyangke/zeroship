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
    ManifestMetadata, RuntimeDescriptorEntry, ScopeDef, WorkerCode,
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
        net: Default::default(),
        metadata: ManifestMetadata {
            compiler: Some("test".into()),
            built_at: "2026-04-29T00:00:00Z".into(),
        },
        exports: None,
        migrations: Vec::new(),
        runtime_descriptor: None,
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
    build_state_inner(db_url, label, None).await
}

/// Like [`build_test_state`] but ALSO attaches a privileged provisioning DSN
/// (`with_provisioning_dsn`) so the P6 deploy-migrate phase can actually
/// `CREATE SCHEMA` / `CREATE ROLE` + apply the bundle's `.ir.json` migrations.
/// PR9c HIGH (operator-only gate) e2e: drives the REAL deploy() → migrate phase
/// through `?approved_versions=`. In test the control DB superuser (`:5440
/// postgres/zeroship`) IS a CREATEROLE principal, so reusing `db_url` as the
/// provisioning DSN puts the per-app schema in the same DB we can introspect.
async fn build_migrate_capable_state(db_url: &str, label: &str) -> Fixture {
    build_state_inner(db_url, label, Some(db_url.to_string())).await
}

async fn build_state_inner(db_url: &str, label: &str, provision_dsn: Option<String>) -> Fixture {
    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("dtmp-{label}"));

    // Concrete LocalDiskBlobStore handle: kept in the fixture so the
    // assertion `bs.has_blob(...)` works without having to downcast
    // the trait-object stored on AppState.
    let blob_store_concrete = Arc::new(
        LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"),
    );

    let registry = Registry::new(db_url).await.expect("registry");
    let registry = match provision_dsn {
        Some(dsn) => registry.with_provisioning_dsn(dsn),
        None => registry,
    };
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
        auth_provider: zeroship_control::hydra_auth_provider("http://127.0.0.1:9"),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        metering_provider: zeroship_control::metering::provider::build_provider(
            &zeroship_control::metering::provider::MeteringProviderConfig::native(),
        )
        .expect("native provider builds"),
        tax_provider: zeroship_control::tax::build_tax_provider(
            &zeroship_control::tax::TaxProviderConfig::native(),
        )
        .expect("native tax provider builds"),
        notifier: std::sync::Arc::new(zeroship_control::notify::RecordingNotifier::new()),
        pairwise_salt: [0u8; 32],
        projected_charge_cache: std::sync::Arc::new(
            zeroship_control::billing_read::ProjectedChargeCache::default(),
        ),
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

// ---------------------------------------------------------------------------
// PR9c HIGH — the operator-only `?approved_versions=` go-live gate, driven
// through the REAL `deploy()` HTTP handler (NOT the apply seam in isolation,
// NOT the cedar policy in isolation). This is the e2e the critique flagged as
// MISSING: before this, the actual wiring (query parse → `AppsApproveMigration`
// 403 → actor stamp → seam routing) was pinned ONLY by a source-grep guard test
// that asserts STRING PRESENCE, not behavior. A refactor that drops/reorders the
// `AppsApproveMigration` check, mis-parses the query, or routes a creator's set
// as approved would pass every behavioral test and only fail the brittle grep
// (exactly the faithful-e2e failure mode). These three cases pin the real
// handler→gate→seam path.
// ---------------------------------------------------------------------------

/// A `.zship` carrying ONE `.ir.json` migration file (`name`, `body`) plus a
/// worker module (so the bundle is a valid deploy). The IR body is its own
/// content-addressed blob referenced from `manifest.migrations`.
fn zship_with_ir_migration(ir_name: &str, ir_body: &str) -> Vec<u8> {
    let server = b"export default { fetch() { return new Response('ok'); } }";
    let server_hash = sha256_hex(server);
    let ir_bytes = ir_body.as_bytes().to_vec();
    let ir_hash = sha256_hex(&ir_bytes);
    let descriptor = br#"{"version":1,"collections":{}}"#;
    let descriptor_hash = sha256_hex(descriptor);

    let mut manifest = manifest_for(Some(&server_hash), &[]);
    manifest.migrations = vec![zeroship_bundle::MigrationFileEntry {
        name: ir_name.to_string(),
        hash: ir_hash.clone(),
    }];
    manifest.runtime_descriptor = Some(RuntimeDescriptorEntry {
        hash: descriptor_hash.clone(),
    });
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let blobs = vec![
        (server_hash.clone(), server.to_vec()),
        (ir_hash.clone(), ir_bytes),
        (descriptor_hash, descriptor.to_vec()),
    ];
    build_zship(&manifest_bytes, &blobs, true)
}

/// Mount the single deploy route under the test app, mirroring `main.rs`'s
/// PayloadConfig.
async fn deploy_service(
    state: Arc<AppState>,
) -> ntex::Pipeline<
    impl ntex::Service<
        ntex::http::Request,
        Response = ntex::web::WebResponse,
        Error = ntex::web::Error,
    >,
> {
    test::init_service(
        web::App::new().state(state).service(
            web::resource("/api/apps/{id}/deploy")
                .state(web::types::PayloadConfig::new(
                    zeroship_control::deploy::MAX_COMPRESSED_BYTES,
                ))
                .route(web::post().to(api::deploy)),
        ),
    )
    .await
}

/// Whether `col` exists on the per-app schema's `table` (the per-app schema is
/// `"<app_id>"`, created by the migrate phase in the provisioning DB = the
/// control DB in test).
async fn http_column_exists(
    conn: &compio_postgres::Client,
    app_id: &Uuid,
    table: &str,
    col: &str,
) -> bool {
    let schema = app_id.to_string();
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
            &[&schema, &table, &col],
        )
        .await
        .expect("column probe");
    !rows.is_empty()
}

/// The DISTINCT journal actors stamped in the per-app meta journal — the
/// forensic `by` column the §2.2 immutable journal records. An operator-approved
/// go-live stamps `deploy-ir-approved:<approver>`; a routine deploy stamps the
/// static `deploy-ir`.
async fn http_journal_actors(conn: &compio_postgres::Client, app_id: &Uuid) -> Vec<String> {
    let meta = format!("{}_migrations", app_id);
    let q = format!("\"{}\".schema_migrations", meta.replace('"', "\"\""));
    let lit = q.replace('\'', "''");
    let present = conn
        .query(&format!("SELECT to_regclass('{lit}') IS NOT NULL AS p"), &[])
        .await
        .expect("regclass probe");
    if !present[0].get::<_, bool>("p") {
        return Vec::new();
    }
    let rows = conn
        .query(
            &format!("SELECT DISTINCT \"by\" AS actor FROM {q} ORDER BY actor"),
            &[],
        )
        .await
        .expect("read journal actors");
    rows.iter().map(|r| r.get::<_, String>("actor")).collect()
}

/// Drop the per-app schema + meta schema + migrator role so a re-run is clean.
async fn http_cleanup_app_schema(conn: &compio_postgres::Client, app_id: &Uuid) {
    let schema = app_id.to_string();
    let role = zeroship_migrate::migrator_role_name(&schema).unwrap();
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE; DROP SCHEMA IF EXISTS {} CASCADE;",
            q(&schema),
            q(&format!("{schema}_migrations"))
        ))
        .await;
    let _ = conn
        .batch_execute(&format!("DROP ROLE IF EXISTS {}", q(&role)))
        .await;
}

/// Seed a creator-owned app + return `(app_id, owner_id)`. The owner is a fresh
/// non-admin user; the app's `members(handle)` table is created by a FIRST
/// routine deploy through the SAME HTTP handler (so the rename EXPAND has a live
/// table to target). The routine create needs no approval, so we use admin here
/// to keep the setup orthogonal to the gate under test.
async fn setup_app_with_members<S>(
    fx: &Fixture,
    app: &ntex::Pipeline<S>,
    label: &str,
) -> (Uuid, Uuid)
where
    S: ntex::Service<
        ntex::http::Request,
        Response = ntex::web::WebResponse,
        Error = ntex::web::Error,
    >,
{
    let owner_id = seed_owner(&fx.state, label).await;
    let app_name = format!("{label}-{}", &Uuid::new_v4().simple().to_string()[..10]);
    let record = fx
        .state
        .registry
        .create_app(
            &app_name,
            &zeroship_control::bootstrap_console::free_plan_id(),
            &owner_id,
        )
        .await
        .expect("create app");
    let app_id = record.id;

    let create = r#"{"ir_version":1,"name":"create_members","ops":[
        {"op":"createTable","name":"members","columns":[
            {"name":"handle","type":"text","nullable":false}
        ]}
    ]}"#;
    let body = zship_with_ir_migration("0001_create_members.ir.json", create);
    let admin = common::authz_fixture::admin_pat(&fx.state).await;
    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{app_id}/deploy"))
        .header("authorization", admin.bearer())
        .header("content-type", "application/x-zship")
        .set_payload(body)
        .to_request();
    let resp = test::call_service(app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "routine createTable deploy #1 must succeed"
    );
    admin.cleanup(&fx.state).await;
    (app_id, owner_id)
}

/// The renameColumn `.ir.json` body (handle → username) — the online-rename whose
/// EXPAND is destructive/approval-gated.
const RENAME_IR: &str = r#"{"ir_version":1,"name":"rename_handle","ops":[
    {"op":"renameColumn","table":"members","from":"handle","to":"username","type":"text"}
]}"#;

/// Discover the operator-reviewed version-id set the approval channel must carry
/// for the rename bundle — the SAME read-only plan the control-plane approval
/// endpoint surfaces (so the test approves the REAL reviewed set, never a blanket
/// pass). Reconstructs the bundle dir on disk + runs `plan_reviewed_versions`
/// against the provisioning DSN (= the control DB in test).
async fn rename_reviewed_versions(db_url: &str, app_id: &Uuid) -> Vec<String> {
    let mut dir = std::env::temp_dir();
    dir.push(format!("zship-http-rev-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).expect("mkdir reviewed dir");
    std::fs::write(dir.join("0002_rename_handle.ir.json"), RENAME_IR).expect("write ir");
    let versions = zeroship_control::deploy_migrate::plan_reviewed_versions(db_url, app_id, &dir)
        .await
        .expect("plan reviewed versions");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        !versions.is_empty(),
        "the online rename must require approval for at least one version"
    );
    versions
}

// (1) A creator holding only `apps:deploy` (owns the app, NO platform role)
// POSTing `?approved_versions=<v>` is 403'd by the operator-only
// `AppsApproveMigration` gate BEFORE the migrate phase — and NO migration runs.
#[compio::test]
async fn deploy_approved_versions_by_creator_is_403_and_runs_no_migration() {
    let Some(db_url) = db_url() else {
        eprintln!("[deploy_http_test] CONTROL_TEST_DB not set — skipping");
        return;
    };
    let fx = build_migrate_capable_state(&db_url, "approve-creator").await;
    let app = deploy_service(fx.state.clone()).await;
    let (app_id, owner_id) = setup_app_with_members(&fx, &app, "approve-creator").await;

    let reviewed = rename_reviewed_versions(&db_url, &app_id).await;
    let approved_q = reviewed.join(",");

    // The creator PAT: owns the app (cedar `app_owner_of`), holds apps:deploy, but
    // NOT `migrations:approve` (excluded by app_owner.cedar). The owner is the
    // bundle author — the anti-bypass principal the gate must refuse.
    let creator = common::authz_fixture::creator_deploy_pat(&fx.state, owner_id).await;
    let body = zship_with_ir_migration("0002_rename_handle.ir.json", RENAME_IR);
    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{app_id}/deploy?approved_versions={approved_q}"))
        .header("authorization", creator.bearer())
        .header("content-type", "application/x-zship")
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a creator (apps:deploy, no migrations:approve) passing ?approved_versions= must be 403'd \
         BEFORE the migrate phase — owning the app must NOT grant self-approval of its destructive \
         migrations"
    );

    // FAIL CLOSED: the gate fired before the migrate phase — the EXPAND never ran.
    assert!(
        http_column_exists(&fx.state.control_pg, &app_id, "members", "handle").await,
        "the old column is untouched — the refused approval ran no migration"
    );
    assert!(
        !http_column_exists(&fx.state.control_pg, &app_id, "members", "username").await,
        "the new column was NEVER created — the EXPAND never reached apply"
    );

    creator.cleanup(&fx.state).await;
    http_cleanup_app_schema(&fx.state.control_pg, &app_id).await;
    let _ = fx.state.registry.delete_app(&app_id).await;
}

// (2) An ADMIN (platform role ⇒ `migrations:approve`) POSTing the SAME
// `?approved_versions=<v>` completes the EXPAND and journals the
// `deploy-ir-approved:<approver>` forensic actor.
#[compio::test]
async fn deploy_approved_versions_by_admin_completes_expand_and_journals_approver() {
    let Some(db_url) = db_url() else {
        eprintln!("[deploy_http_test] CONTROL_TEST_DB not set — skipping");
        return;
    };
    let fx = build_migrate_capable_state(&db_url, "approve-admin").await;
    let app = deploy_service(fx.state.clone()).await;
    let (app_id, _owner_id) = setup_app_with_members(&fx, &app, "approve-admin").await;

    let reviewed = rename_reviewed_versions(&db_url, &app_id).await;
    let approved_q = reviewed.join(",");

    let admin = common::authz_fixture::admin_pat(&fx.state).await;
    let body = zship_with_ir_migration("0002_rename_handle.ir.json", RENAME_IR);
    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{app_id}/deploy?approved_versions={approved_q}"))
        .header("authorization", admin.bearer())
        .header("content-type", "application/x-zship")
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "an admin (migrations:approve) approving the reviewed set completes the EXPAND go-live"
    );

    // The EXPAND completed: the NEW column exists, the OLD column is still present
    // (the CONTRACT is pending, cross-deploy).
    assert!(
        http_column_exists(&fx.state.control_pg, &app_id, "members", "username").await,
        "the approved EXPAND created the new column"
    );
    assert!(
        http_column_exists(&fx.state.control_pg, &app_id, "members", "handle").await,
        "the old column is still present — the CONTRACT is pending, not applied"
    );

    // The forensic actor is the operator-approved marker carrying the approver's
    // principal — NOT the static routine `deploy-ir` string.
    let actors = http_journal_actors(&fx.state.control_pg, &app_id).await;
    assert!(
        actors.iter().any(|a| a.starts_with("deploy-ir-approved:")),
        "an operator-approved go-live must journal `deploy-ir-approved:<approver>`, got {actors:?}"
    );

    admin.cleanup(&fx.state).await;
    http_cleanup_app_schema(&fx.state.control_pg, &app_id).await;
    let _ = fx.state.registry.delete_app(&app_id).await;
}

// (3) An ABSENT/empty `?approved_versions=` is a ROUTINE deploy: the operator-only
// gate never fires (so even a creator could run it), but the online-rename EXPAND
// is REFUSED at the approval gate (422) and NOTHING migrates.
#[compio::test]
async fn deploy_without_approved_versions_refuses_expand_routine() {
    let Some(db_url) = db_url() else {
        eprintln!("[deploy_http_test] CONTROL_TEST_DB not set — skipping");
        return;
    };
    let fx = build_migrate_capable_state(&db_url, "approve-empty").await;
    let app = deploy_service(fx.state.clone()).await;
    let (app_id, owner_id) = setup_app_with_members(&fx, &app, "approve-empty").await;

    // The creator deploys the rename with NO approval query — the gate does not
    // fire (empty set ⇒ routine), but the EXPAND is refused at the approval gate.
    let creator = common::authz_fixture::creator_deploy_pat(&fx.state, owner_id).await;
    let body = zship_with_ir_migration("0002_rename_handle.ir.json", RENAME_IR);
    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{app_id}/deploy"))
        .header("authorization", creator.bearer())
        .header("content-type", "application/x-zship")
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "a routine deploy (no ?approved_versions=) carrying an online-rename EXPAND is refused \
         at the approval gate with 422 — the destructive op never completes"
    );

    // FAIL CLOSED: nothing migrated.
    assert!(
        http_column_exists(&fx.state.control_pg, &app_id, "members", "handle").await,
        "old column untouched on the refused routine deploy"
    );
    assert!(
        !http_column_exists(&fx.state.control_pg, &app_id, "members", "username").await,
        "the EXPAND was refused — no new column"
    );

    creator.cleanup(&fx.state).await;
    http_cleanup_app_schema(&fx.state.control_pg, &app_id).await;
    let _ = fx.state.registry.delete_app(&app_id).await;
}

// (4) PR9c H2 — an admin approving a go-live with a WRONG `?expected_manifest=`
// (standing in for a set tampered/reordered between approval and apply) is refused
// 422 by the real handler BEFORE any DDL, and NOTHING migrates. Pins the
// query-param → run_deploy_migrations → prevalidate H2 gate wiring end to end.
#[compio::test]
async fn deploy_approved_with_wrong_expected_manifest_is_422_and_runs_no_migration() {
    let Some(db_url) = db_url() else {
        eprintln!("[deploy_http_test] CONTROL_TEST_DB not set — skipping");
        return;
    };
    let fx = build_migrate_capable_state(&db_url, "approve-h2").await;
    let app = deploy_service(fx.state.clone()).await;
    let (app_id, _owner_id) = setup_app_with_members(&fx, &app, "approve-h2").await;

    let reviewed = rename_reviewed_versions(&db_url, &app_id).await;
    let approved_q = reviewed.join(",");
    // A wrong reviewed-manifest hash (64 hex chars) — the arrived bundle cannot
    // recompute it, so the H2 gate refuses before any DDL.
    let wrong_manifest = "deadbeef".repeat(8);

    let admin = common::authz_fixture::admin_pat(&fx.state).await;
    let body = zship_with_ir_migration("0002_rename_handle.ir.json", RENAME_IR);
    let req = test::TestRequest::post()
        .uri(&format!(
            "/api/apps/{app_id}/deploy?approved_versions={approved_q}&expected_manifest={wrong_manifest}"
        ))
        .header("authorization", admin.bearer())
        .header("content-type", "application/x-zship")
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "an approved go-live whose bundle does not match the operator-approved manifest hash is \
         refused 422 before any DDL"
    );

    // FAIL CLOSED: the EXPAND never ran.
    assert!(
        http_column_exists(&fx.state.control_pg, &app_id, "members", "handle").await,
        "old column untouched — the H2 mismatch ran no migration"
    );
    assert!(
        !http_column_exists(&fx.state.control_pg, &app_id, "members", "username").await,
        "the EXPAND never reached apply"
    );

    admin.cleanup(&fx.state).await;
    http_cleanup_app_schema(&fx.state.control_pg, &app_id).await;
    let _ = fx.state.registry.delete_app(&app_id).await;
}
