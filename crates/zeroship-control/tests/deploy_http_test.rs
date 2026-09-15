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
//! All cases gate on a test database - Postgres is required to
//! construct `AppState` (registry/env_store/auth all dial the DB
//! on startup). `common::require_control_db` REFUSES the run when
//! there is no migrated database, rather than skipping it.
//!
//! Note on the missing case: a "body exceeds the 256 MiB cap"
//! test isn't here — sending 256+ MiB through ntex test plumbing
//! is too slow for unit-test speed. The same code path is
//! exercised more cheaply by `stream_tmp_tests::cap_exceeded_removes_tmp_file`
//! in `crates/zeroship-control/src/api.rs`.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use ntex::http::StatusCode;
use ntex::web::{self, test};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroship_core::{AppId, DeployCommandId, UserId};

use zeroship_bundle::{
    AssetEntry, AuthConfig, BlobStore, LocalDiskBlobStore, Manifest, ManifestMetadata, ScopeDef,
    WorkerCode,
};
use zeroship_control::{
    api, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};

use crate::common;

// ---------------------------------------------------------------------------
// Test gating
// ---------------------------------------------------------------------------

fn db_url() -> String {
    crate::common::require_control_db()
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

/// Seed the principal that will BOTH own the app and send the deploy.
///
/// These tests exercise the deploy HTTP path rather than authz, but the owner
/// identity is not immaterial: `create_app` binds an `app_members` owner row,
/// and that row is now the only thing that authorizes `apps:deploy`. The
/// previous helper seeded a throwaway owner unrelated to the bearer, which
/// passed only because the deleted universal-allow policy covered the gap.
async fn seed_owner(state: &AppState, _label: &str) -> common::authz_fixture::SeededPrincipal {
    common::authz_fixture::seeded_principal(state).await
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
        runtime_date: None,
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
        schedules: Vec::new(),
        workflows: None,
        asset_version: 0,
        sourcemaps: HashMap::new(),
        auth: Default::default(),
        net: Default::default(),
        metadata: ManifestMetadata {
            compiler: Some("test".into()),
            built_at: "2026-04-29T00:00:00Z".into(),
        },
        exports: None,
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
    // The default admin quota is deliberately far above anything a test issues,
    // so the limiter never interferes with a test that is about something else.
    build_test_state_with_admin_quota(db_url, label, Quota::per_minute(10_000, 100)).await
}

/// Same fixture with a caller-chosen admin quota, for the tests that are ABOUT
/// the limiter and need one small enough to trip.
/// Same fixture with a caller-chosen admin quota AND `trust_proxy` on, so a
/// limiter test can send a unique `X-Forwarded-For` and get its OWN bucket.
///
/// The bucket lives in Postgres keyed by caller identity, and every test
/// otherwise resolves to the same unresolved-client identity - so a test that
/// deliberately exhausts the bucket would drain it for every test sharing the
/// database, whatever quota their own fixture declares.
async fn build_test_state_with_admin_quota(
    db_url: &str,
    label: &str,
    admin_quota: Quota,
) -> Fixture {
    // Only the limiter test needs a distinguishable caller identity.
    let trust_proxy_for_limiter = true;
    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("dtmp-{label}"));

    // Concrete LocalDiskBlobStore handle: kept in the fixture so the
    // assertion `bs.has_blob(...)` works without having to downcast
    // the trait-object stored on AppState.
    let blob_store_concrete =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));

    let registry = Registry::new(db_url).await.expect("registry");
    zeroship_control::plan_catalog::seed_plans(&registry)
        .await
        .expect("seed built-in plans");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());

    let blob_store: Arc<dyn BlobStore> = blob_store_concrete.clone();
    let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
        zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
            .expect("workflow blob store"),
    );

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
        service_auth: std::sync::Arc::new(zeroship_core::service_peers::ServiceAuth::unconfigured()),
        registry,
        env_store,
        stripe_store,
        blob_store,
        workflow_blob_store,
        control_key: SecretString::new("test-control-key".to_string()),
        master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
        stripe_webhook_secret: SecretString::new(String::new()),
        stripe_secret_key: SecretString::new(String::new()),
        stripe_base_url: "https://api.stripe.com".to_string(),
        gateway_url: "http://127.0.0.1:9".to_string(),
        worker_urls: Vec::new(),
        admin_limiter: Arc::new(RateLimiter::new(admin_quota)),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        origin_scheme: zeroship_core::config::OriginScheme::Https,
        trust_proxy: trust_proxy_for_limiter,
        worker_enrolment: zeroship_control::worker_join::EnrolmentEnvelope::closed(),
        deploy_tmp_dir: deploy_tmp_dir.clone(),
        control_pg,
        app_base_domain: "zeroship.localhost".to_string(),
        trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
        expected_oauth_audience: "control.zeroship.ai".to_string(),
        static_policies: zeroship_authz::load_platform_policies()
            .expect("bundled authz policies parse"),
        auth_provider: zeroship_control::platform_auth_provider(
            "https://auth.zeroship.test/oauth2",
            Some(common::platform_jwks_url()),
        ),
        // No platform deploy-token mint here: that is control's OUTBOUND
        // destination for the device flow, and no fixture below drives one.
        provider_registry: zeroship_control::metering::provider::builtin_registry(),
        billing_stack: zeroship_control::metering::provider::BillingStack::for_tests(),
        billing_stream: None,
        tax_provider: zeroship_control::tax::build_tax_provider(
            &zeroship_control::tax::TaxProviderConfig::native(),
        )
        .expect("native tax provider builds"),
        notifier: std::sync::Arc::new(zeroship_control::notify::RecordingNotifier::new()),
        mailer: std::sync::Arc::new(zeroship_mailer::RecordingMailer::new()),
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
    let db_url = db_url();

    let fx = build_test_state(&db_url, "happy").await;

    // Real app row in the test DB.
    let pat = seed_owner(&fx.state, "httpd").await;
    let owner_id = &pat.user_id;
    let app_name = format!("httpd-{}", &Uuid::new_v4().simple().to_string()[..10]);
    let record = fx
        .state
        .registry
        .create_app(
            &app_name,
            &zeroship_control::plan_catalog::free_plan_id(),
            owner_id,
            None,
            None,
        )
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
        web::App::new().state(fx.state.clone()).service(
            web::resource("/api/apps/{id}/deploy")
                .state(web::types::PayloadConfig::new(
                    zeroship_control::deploy::MAX_COMPRESSED_BYTES,
                ))
                .route(web::post().to(api::deploy)),
        ),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{}/deploy", app_id.as_str()))
        .header("authorization", pat.bearer())
        .header("content-type", "application/x-zship")
        .header("idempotency-key", DeployCommandId::mint().as_str())
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "expected 200");

    // ntex 3.x has no `read_body_json` helper — read bytes + parse.
    let bytes = test::read_body(resp).await;
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("response is JSON");
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
    let _ = fx.state.registry.archive_app(&app_id).await;
    pat.cleanup(&fx.state).await;

    // Teardown: the service and the fixture both hold connections, and locals
    // are dropped only after the body returns - by which point the runtime is
    // gone and the sockets can no longer be closed. Drop them explicitly, then
    // wait for the close to land.
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn deploy_wrong_content_type_returns_415_without_consuming_body() {
    let db_url = db_url();

    let fx = build_test_state(&db_url, "ct").await;
    // A REAL app owned by the caller. The content-type check sits behind the
    // authz gate, so a caller with no membership is refused before reaching it
    // and this test would assert 403 instead of the 415 it is about.
    let pat = seed_owner(&fx.state, "ct").await;
    let record = fx
        .state
        .registry
        .create_app(
            &format!("ct-{}", &Uuid::new_v4().simple().to_string()[..10]),
            &zeroship_control::plan_catalog::free_plan_id(),
            &pat.user_id,
            None,
            None,
        )
        .await
        .expect("create app");
    let app_id = record.id;
    let body: Vec<u8> = b"this is not a zship payload".to_vec();

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

    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{}/deploy", app_id.as_str()))
        .header("authorization", pat.bearer())
        .header("content-type", "application/octet-stream")
        .set_payload(body)
        .to_request();
    // Read the status out and let the response go: a `WebResponse` owns the
    // request that borrowed the app state, so holding one to end of scope keeps
    // the state's Postgres client alive past the teardown below.
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "expected 415");

    // The handler rejects on content-type BEFORE streaming. Tmp dir
    // must therefore be untouched.
    assert!(
        dir_is_empty(&fx.deploy_tmp_dir),
        "deploy_tmp_dir must be empty on 415 (body should not stream)",
    );
    pat.cleanup(&fx.state).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn deploy_missing_auth_returns_401_without_consuming_body() {
    let db_url = db_url();

    let fx = build_test_state(&db_url, "auth").await;
    let app_id = AppId::mint();
    let body: Vec<u8> = b"won't ever be looked at".to_vec();

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

    // No authorization header at all.
    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{}/deploy", app_id.as_str()))
        .header("content-type", "application/x-zship")
        .header("idempotency-key", DeployCommandId::mint().as_str())
        .set_payload(body.clone())
        .to_request();
    // Status only: a retained `WebResponse` keeps the app state - and its
    // Postgres client - alive past the teardown at the end of this test.
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no bearer -> 401");
    assert!(
        dir_is_empty(&fx.deploy_tmp_dir),
        "no bearer must not stream body to tmp",
    );

    // Invalid PAT bearer should also reject — same status.
    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{}/deploy", app_id.as_str()))
        .header("authorization", "Bearer not-a-valid-pat")
        .header("content-type", "application/x-zship")
        .header("idempotency-key", DeployCommandId::mint().as_str())
        .set_payload(body)
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::UNAUTHORIZED, "wrong bearer -> 401");
    assert!(
        dir_is_empty(&fx.deploy_tmp_dir),
        "wrong bearer must not stream body to tmp",
    );

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn deploy_manifest_not_first_returns_400() {
    let db_url = db_url();

    let fx = build_test_state(&db_url, "mf").await;

    // Real app — the handler walks all the way through ingest to
    // produce the structured BadRequest, so the app must exist for
    // the registry path to behave normally up to the rejection.
    let pat = seed_owner(&fx.state, "httpmf").await;
    let owner_id = &pat.user_id;
    let app_name = format!("httpmf-{}", &Uuid::new_v4().simple().to_string()[..10]);
    let record = fx
        .state
        .registry
        .create_app(
            &app_name,
            &zeroship_control::plan_catalog::free_plan_id(),
            owner_id,
            None,
            None,
        )
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
        web::App::new().state(fx.state.clone()).service(
            web::resource("/api/apps/{id}/deploy")
                .state(web::types::PayloadConfig::new(
                    zeroship_control::deploy::MAX_COMPRESSED_BYTES,
                ))
                .route(web::post().to(api::deploy)),
        ),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{}/deploy", app_id.as_str()))
        .header("authorization", pat.bearer())
        .header("content-type", "application/x-zship")
        .header("idempotency-key", DeployCommandId::mint().as_str())
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "expected 400");

    let bytes = test::read_body(resp).await;
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("response is JSON");
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

    let _ = fx.state.registry.archive_app(&app_id).await;
    pat.cleanup(&fx.state).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// Spec §5.1/§5.2 regression: a manifest declaring a scope that collides
/// with the closed platform-delegated vocabulary (`billing:read`) MUST be
/// REJECTED by the deploy handler with a 4xx — NOT accepted with a 200 and
/// then silently un-provisioned. `ingest` only enforces scope-id FORMAT, so
/// the collision guard has to run in the handler before the manifest commit.
#[compio::test]
async fn deploy_colliding_scope_returns_400_invalid_scope() {
    let db_url = db_url();

    let fx = build_test_state(&db_url, "scopecollide").await;

    let pat = seed_owner(&fx.state, "httpsc").await;
    let owner_id = &pat.user_id;
    let app_name = format!("httpsc-{}", &Uuid::new_v4().simple().to_string()[..10]);
    let record = fx
        .state
        .registry
        .create_app(
            &app_name,
            &zeroship_control::plan_catalog::free_plan_id(),
            owner_id,
            None,
            None,
        )
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

    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{}/deploy", app_id.as_str()))
        .header("authorization", pat.bearer())
        .header("content-type", "application/x-zship")
        .header("idempotency-key", DeployCommandId::mint().as_str())
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "colliding scope must hard-fail the deploy with 400"
    );

    let bytes = test::read_body(resp).await;
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("response is JSON");
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

    let _ = fx.state.registry.archive_app(&app_id).await;
    pat.cleanup(&fx.state).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// Companion to the collision test: a well-formed, NON-colliding declared
/// scope (`read:billing`, the mirror order, which is NOT in the platform
/// vocabulary) deploys cleanly with a 200.
#[compio::test]
async fn deploy_noncolliding_scope_returns_200() {
    let db_url = db_url();

    let fx = build_test_state(&db_url, "scopeok").await;

    let pat = seed_owner(&fx.state, "httpok").await;
    let owner_id = &pat.user_id;
    let app_name = format!("httpok-{}", &Uuid::new_v4().simple().to_string()[..10]);
    let record = fx
        .state
        .registry
        .create_app(
            &app_name,
            &zeroship_control::plan_catalog::free_plan_id(),
            owner_id,
            None,
            None,
        )
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

    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{}/deploy", app_id.as_str()))
        .header("authorization", pat.bearer())
        .header("content-type", "application/x-zship")
        .header("idempotency-key", DeployCommandId::mint().as_str())
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "non-colliding scope must deploy cleanly"
    );

    let bytes = test::read_body(resp).await;
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("response is JSON");
    assert_eq!(
        json.get("deploy_hash")
            .and_then(|v| v.as_str())
            .map(str::len),
        Some(64),
        "deploy committed with a sha256 deploy_hash"
    );

    let _ = fx.state.registry.archive_app(&app_id).await;
    pat.cleanup(&fx.state).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

// ---------------------------------------------------------------------------
// Deploy/migration decoupling
// ---------------------------------------------------------------------------

fn zship_with_legacy_migrations_key() -> Vec<u8> {
    let server = b"export default { fetch() { return new Response('ok'); } }";
    let server_hash = sha256_hex(server);
    let ir = br#"{"ir_version":1,"name":"legacy","ops":[{"op":"createTable","name":"legacy_notes","columns":[{"name":"title","type":"text","nullable":false}]}]}"#;
    let ir_hash = sha256_hex(ir);
    let manifest_json = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "worker": {
            "entry": "index.js",
            "modules": { "index.js": server_hash.clone() },
        },
        "assets": {},
        "runtime_assets": {},
        "asset_version": 0,
        "sourcemaps": {},
        "metadata": {
            "compiler": "test",
            "built_at": "2026-06-29T00:00:00Z",
        },
        "migrations": [
            { "name": "20260629000000_legacy.ir.json", "hash": ir_hash.clone() },
        ],
    }))
    .unwrap();
    build_zship(
        &manifest_json,
        &[(server_hash, server.to_vec()), (ir_hash, ir.to_vec())],
        true,
    )
}

fn worker_only_zship() -> Vec<u8> {
    let server = b"export default { fetch() { return new Response('ok'); } }";
    let server_hash = sha256_hex(server);
    let manifest = manifest_for(Some(&server_hash), &[]);
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    build_zship(&manifest_bytes, &[(server_hash, server.to_vec())], true)
}

async fn deploy_service(
    state: Arc<AppState>,
) -> ntex::Pipeline<
    impl ntex::Service<ntex::http::Request, Response = ntex::web::WebResponse, Error = ntex::web::Error>,
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

async fn schema_exists(conn: &compio_postgres::Client, app_id: &AppId) -> bool {
    let schema = app_id.as_str();
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.schemata WHERE schema_name = $1",
            &[&schema],
        )
        .await
        .expect("schema probe");
    !rows.is_empty()
}

#[compio::test]
async fn deploy_rejects_legacy_migration_approval_query() {
    let db_url = db_url();
    let fx = build_test_state(&db_url, "legacy-query").await;
    let app = deploy_service(fx.state.clone()).await;
    let pat = seed_owner(&fx.state, "legacy-query").await;
    let owner_id = &pat.user_id;
    let record = fx
        .state
        .registry
        .create_app(
            &format!(
                "legacy-query-{}",
                &Uuid::new_v4().simple().to_string()[..10]
            ),
            &zeroship_control::plan_catalog::free_plan_id(),
            owner_id,
            None,
            None,
        )
        .await
        .expect("create app");
    let app_id = record.id;
    let bundle = worker_only_zship();

    let req = test::TestRequest::post()
        .uri(&format!(
            "/api/apps/{}/deploy?approved_versions=abc&expected_manifest={}",
            app_id.as_str(),
            "deadbeef".repeat(8)
        ))
        .header("authorization", pat.bearer())
        .header("content-type", "application/x-zship")
        .header("idempotency-key", DeployCommandId::mint().as_str())
        .set_payload(bundle)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = test::read_body(resp).await;
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json error body");
    assert_eq!(
        json.get("error").and_then(|v| v.as_str()),
        Some("migration_approval_removed")
    );

    let _ = fx.state.registry.archive_app(&app_id).await;
    pat.cleanup(&fx.state).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn deploy_rejects_legacy_manifest_migrations_and_runs_no_migration() {
    let db_url = db_url();
    let fx = build_test_state(&db_url, "legacy-manifest").await;
    let app = deploy_service(fx.state.clone()).await;
    let pat = seed_owner(&fx.state, "legacy-manifest").await;
    let owner_id = &pat.user_id;
    let record = fx
        .state
        .registry
        .create_app(
            &format!(
                "legacy-manifest-{}",
                &Uuid::new_v4().simple().to_string()[..10]
            ),
            &zeroship_control::plan_catalog::free_plan_id(),
            owner_id,
            None,
            None,
        )
        .await
        .expect("create app");
    let app_id = record.id;
    let bundle = zship_with_legacy_migrations_key();

    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{}/deploy", app_id.as_str()))
        .header("authorization", pat.bearer())
        .header("content-type", "application/x-zship")
        .header("idempotency-key", DeployCommandId::mint().as_str())
        .set_payload(bundle)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = test::read_body(resp).await;
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json error body");
    assert!(
        json.get("detail")
            .and_then(|v| v.as_str())
            .is_some_and(|detail| detail.contains("manifest.migrations")
                && detail.contains("migration service")),
        "legacy manifest error should point to the migration service: {json}"
    );
    assert!(
        !schema_exists(&fx.state.control_pg, &app_id).await,
        "deploy must not create/apply the per-app schema from a legacy migration-bearing bundle"
    );

    let _ = fx.state.registry.archive_app(&app_id).await;
    pat.cleanup(&fx.state).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// Deploying to an app id that does not exist writes no blobs.
///
/// The handler streams the body, mmaps it and calls `deploy::ingest` (which
/// writes every blob into the blob store) and only afterwards looks the app up,
/// so a bundle for a nonexistent app used to leave blobs behind with nothing to
/// reference or bill them. That arm was reachable only by a caller holding a
/// fleet-wide grant; an ordinary creator was already denied by authz on an app
/// they do not own.
///
/// With the fleet-wide grant deleted, EVERY caller is that ordinary creator, so
/// the refusal is now 403 at the gate and the ingest is never entered. The
/// blob assertion is the part that still earns its keep - it is what would go
/// red if the gate were ever moved after the stream again.
///
/// 403 rather than 404 is also the better answer on its own terms: the response
/// is identical for an app that does not exist and one the caller does not own,
/// so it is not an existence oracle.
#[compio::test]
async fn deploy_to_nonexistent_app_does_not_write_blobs() {
    let db_url = db_url();

    let fx = build_test_state(&db_url, "ghost").await;

    // A well-formed bundle, so nothing rejects it for its own shape. The only
    // thing wrong with this request is that the app does not exist.
    let html = b"<!doctype html><body>ghost app</body>";
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

    // Never created through the registry, so no app row exists for it.
    let ghost_id = AppId::mint();
    let pat = common::authz_fixture::seeded_principal(&fx.state).await;
    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{}/deploy", ghost_id.as_str()))
        .header("authorization", pat.bearer())
        .header("content-type", "application/x-zship")
        .header("idempotency-key", DeployCommandId::mint().as_str())
        .set_payload(body)
        .to_request();
    // Status only: a retained `WebResponse` keeps the app state - and its
    // Postgres client - alive past the teardown at the end of this test.
    let status = test::call_service(&app, req).await.status();

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "deploying to an app the caller is not a member of must be refused at the gate",
    );
    // Assert on the blob directory, not the store root: the store creates
    // `blobs/` and `manifests/` when it is constructed, so the root is never
    // empty and asserting on it would fail no matter what the handler did.
    assert!(
        dir_is_empty(&fx.blob_root.join("blobs")),
        "a deploy for a nonexistent app must not leave blobs behind: the app is \
         never created, so nothing will ever reference, bill, or garbage-collect them",
    );

    pat.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// Deploy is rate limited.
///
/// It is the most expensive endpoint the control plane exposes - it streams a
/// body to disk, mmaps it, and writes every blob in the bundle - and nothing
/// bounded how often one caller could ask for that.
///
/// The caller is AUTHENTICATED on purpose. `AuthzGuard` is an extractor, so an
/// unauthenticated request is rejected before any handler body runs and would
/// never reach the limiter: a version of this test without credentials passes
/// through 31 straight 401s and proves nothing.
#[compio::test]
async fn deploy_is_rate_limited() {
    let db_url = db_url();
    // Small admin quota so 31 requests actually cross it.
    let fx =
        build_test_state_with_admin_quota(&db_url, "ratelimit", Quota::per_minute(5, 60)).await;
    let app_id = AppId::mint();

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

    // The admin bucket allows 30/minute per caller; go one past it.
    let pat = common::authz_fixture::seeded_principal(&fx.state).await;
    // Unique caller identity so this test owns its bucket outright.
    // Wide random space: the bucket persists in the database between runs, so a
    // narrow one would eventually reuse an identity that still has tokens spent.
    let r = Uuid::new_v4().as_u128();
    let caller_ip = format!(
        "10.{}.{}.{}",
        (r >> 16) as u8,
        (r >> 8) as u8,
        (r as u8) | 1
    );
    let mut statuses = Vec::new();
    for _ in 0..31 {
        let req = test::TestRequest::post()
            .uri(&format!("/api/apps/{}/deploy", app_id.as_str()))
            .header("authorization", pat.bearer())
            .header("x-forwarded-for", caller_ip.as_str())
            .header("content-type", "application/x-zship")
            .header("idempotency-key", DeployCommandId::mint().as_str())
            .set_payload(b"never read".to_vec())
            .to_request();
        statuses.push(test::call_service(&app, req).await.status());
    }

    assert!(
        statuses.contains(&StatusCode::TOO_MANY_REQUESTS),
        "31 authenticated deploys must trip the limiter; got {statuses:?}",
    );
    assert!(
        dir_is_empty(&fx.deploy_tmp_dir),
        "a throttled deploy must not stream its body to tmp",
    );

    pat.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

// ---------------------------------------------------------------------------
// The deploy schema precondition
//
// The runtime descriptor carried by the `.zship` is the SOLE schema authority
// for `env.db` - there is no live introspection left. So if a deploy goes live
// before its migrations apply, the descriptor says a column is masked while the
// database still holds plaintext there, and the runtime serves the plain value
// believing it is masked. Nothing downstream detects it.
//
// The guard runs under the app row lock in `publication::catalog::accept`, the
// transaction that makes a deploy live. These cases drive the full HTTP
// handler, so they measure the shipped path rather than the registry call.
//
// ON THE FIXTURE, AND WHY IT IS NOT `manifest_for`. Every other deploy case in
// this file uses `manifest_for`, which hardcodes `runtime_descriptor: None`
// (correctly - those are schema-less apps). A case that reused it unchanged
// would take the "no descriptor, no applied schema" arm, which the guard
// PERMITS, and would print exactly what a working guard prints while ruling on
// nothing. `manifest_with_descriptor` below is the whole difference, and the
// fixture mutation named on the first case is what pins it.
// ---------------------------------------------------------------------------

/// Bytes that stand in for `schema.runtime.json`.
///
/// Deliberately NOT a real descriptor. Control's ingest validates the blob's
/// HASH FORMAT and its PRESENCE in the tar and never parses the body
/// (`zeroship-bundle/src/unpack.rs`, the `runtime_descriptor` arm of the blob
/// gather), so the guard is reachable with any bytes. Lifting a committed
/// `examples/*/generated/schema.runtime.json` would be worse than useless: all
/// of them are `"version": 1` while the packer accepts only v2, so they would
/// fail for a reason that has nothing to do with this guard.
fn descriptor_blob(marker: &str) -> Vec<u8> {
    format!(r#"{{"version":2,"marker":"{marker}","collections":{{}},"storage":{{}}}}"#).into_bytes()
}

/// A manifest that CARRIES a runtime schema descriptor - the state
/// [`manifest_for`] cannot construct.
fn manifest_with_descriptor(worker_hash: &str, descriptor_hash: &str) -> Manifest {
    let mut m = manifest_for(Some(worker_hash), &[]);
    m.runtime_descriptor = Some(zeroship_bundle::RuntimeDescriptorEntry {
        hash: descriptor_hash.to_string(),
    });
    m
}

/// A `.zship` whose manifest names the descriptor and whose tar carries the
/// matching blob. `None` builds the schema-less artifact instead.
fn zship_with_descriptor(descriptor: Option<&[u8]>) -> Vec<u8> {
    let server = b"export default { fetch() { return new Response('ok'); } }";
    let server_hash = sha256_hex(server);
    let mut blobs = vec![(server_hash.clone(), server.to_vec())];
    let manifest = match descriptor {
        Some(bytes) => {
            let hash = sha256_hex(bytes);
            blobs.push((hash.clone(), bytes.to_vec()));
            manifest_with_descriptor(&server_hash, &hash)
        }
        None => manifest_for(Some(&server_hash), &[]),
    };
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    build_zship(&manifest_bytes, &blobs, true)
}

/// Write the ledger row `migrated` writes when an apply request succeeds.
///
/// `applied_at` is a parameter so a case can order two rows deterministically:
/// the predicate compares against the NEWEST applied row, and a case that
/// cannot control which row is newest cannot tell "newest" from "any".
async fn insert_applied_migration(
    conn: &compio_postgres::Client,
    app_id: &AppId,
    submitted_by: &UserId,
    descriptor_sha256: &str,
    applied_at: &str,
) {
    conn.execute(
        "INSERT INTO zeroship.app_schema_applies \
            (app_id, migration_id, status, request_body, effective_profile, \
             ceiling_id, ceiling_version, applied_versions, submitted_by, \
             applied_at, descriptor_sha256) \
         VALUES ($1, $2, 'applied', '{}'::jsonb, '{}'::jsonb, 'test-ceiling', 1, \
                 '[]'::jsonb, $3, $4::text::timestamptz, $5)",
        &[
            &app_id.as_str(),
            &Uuid::now_v7(),
            &submitted_by.as_str(),
            &applied_at,
            &descriptor_sha256,
        ],
    )
    .await
    .expect("insert applied migration row");
}

/// The app's live deploy hash AS THE GATEWAY WOULD READ IT.
///
/// Read through `get_routes()` rather than off `zeroship.apps`, because that is
/// the projection the gateway polls. Asserting the 409 alone would pass on a
/// build that answers 409 AND commits the UPDATE.
async fn live_deploy_hash(state: &AppState, app_id: &AppId) -> Option<String> {
    state
        .registry
        .get_routes()
        .await
        .expect("get_routes")
        .get(app_id)
        .and_then(|entry| entry.deploy_hash.clone())
}

async fn post_zship(
    app: &ntex::Pipeline<
        impl ntex::Service<
            ntex::http::Request,
            Response = ntex::web::WebResponse,
            Error = ntex::web::Error,
        >,
    >,
    app_id: &AppId,
    bearer: &str,
    body: Vec<u8>,
) -> (StatusCode, serde_json::Value) {
    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{}/deploy", app_id.as_str()))
        .header("authorization", bearer)
        .header("content-type", "application/x-zship")
        .header("idempotency-key", DeployCommandId::mint().as_str())
        .set_payload(body)
        .to_request();
    let resp = test::call_service(app, req).await;
    let status = resp.status();
    let bytes = test::read_body(resp).await;
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// Create an app named for `label`, owned by `owner_id`.
async fn create_labelled_app(state: &AppState, label: &str, owner_id: &UserId) -> AppId {
    state
        .registry
        .create_app(
            &format!("{label}-{}", &Uuid::new_v4().simple().to_string()[..10]),
            &zeroship_control::plan_catalog::free_plan_id(),
            owner_id,
            None,
            None,
        )
        .await
        .expect("create app")
        .id
}

/// ARM 1. A descriptor-carrying deploy over an app with NO applied schema is
/// refused, and nothing goes live.
///
/// MUTATIONS THIS MUST SURVIVE, both run:
///  - CODE: skip the schema admission in `publication::catalog::accept`.
///    This case must go RED.
///  - FIXTURE: build the bundle with `zship_with_descriptor(None)`. This case
///    must ALSO go red - with no descriptor and no applied schema the guard
///    correctly answers 200, so a case that cannot tell those two apart is not
///    measuring the guard.
#[compio::test]
async fn deploy_with_unapplied_schema_is_refused_and_nothing_goes_live() {
    let db_url = db_url();
    let fx = build_test_state(&db_url, "schema-unapplied").await;
    let app = deploy_service(fx.state.clone()).await;
    let pat = seed_owner(&fx.state, "schema-unapplied").await;
    let app_id = create_labelled_app(&fx.state, "schemaunapplied", &pat.user_id).await;

    assert_eq!(
        live_deploy_hash(&fx.state, &app_id).await,
        None,
        "CONTROL: a freshly created app has no live deploy, so the same read \
         after the refusal is attributable to the refusal",
    );

    let (status, body) = post_zship(
        &app,
        &app_id,
        &pat.bearer(),
        zship_with_descriptor(Some(&descriptor_blob("v1"))),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT, "expected 409, body: {body}");
    assert_eq!(
        body.get("error").and_then(|v| v.as_str()),
        Some("schema_not_applied"),
    );
    assert!(
        body.get("remedy")
            .and_then(|v| v.as_str())
            .is_some_and(
                |remedy| remedy.contains("zeroship migrate") && remedy.contains(app_id.as_str())
            ),
        "the body must carry the remedy command naming this app, got {body}",
    );
    // THE ASSERTION THAT MAKES THE 409 MEAN SOMETHING. A build that answers 409
    // and commits the UPDATE anyway satisfies every check above and leaks
    // exactly what this guard exists to stop.
    assert_eq!(
        live_deploy_hash(&fx.state, &app_id).await,
        None,
        "the refused deploy must not be live in the projection the gateway reads",
    );

    let _ = fx.state.registry.archive_app(&app_id).await;
    pat.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// ARM 2. THE CONTROL, differing from arm 1 in ONE variable: the same bytes,
/// over an app whose newest applied migration recorded THIS descriptor.
#[compio::test]
async fn deploy_matching_the_applied_descriptor_goes_live() {
    let db_url = db_url();
    let fx = build_test_state(&db_url, "schema-applied").await;
    let app = deploy_service(fx.state.clone()).await;
    let pat = seed_owner(&fx.state, "schema-applied").await;
    let app_id = create_labelled_app(&fx.state, "schemaapplied", &pat.user_id).await;

    let blob = descriptor_blob("v1");
    insert_applied_migration(
        &fx.state.control_pg,
        &app_id,
        &pat.user_id,
        &sha256_hex(&blob),
        "2026-08-28T00:00:00Z",
    )
    .await;

    let (status, body) = post_zship(
        &app,
        &app_id,
        &pat.bearer(),
        zship_with_descriptor(Some(&blob)),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "expected 200, body: {body}");
    assert_eq!(
        live_deploy_hash(&fx.state, &app_id).await.as_deref(),
        body.get("deploy_hash").and_then(|v| v.as_str()),
        "the accepted deploy is live in the projection the gateway reads",
    );

    let _ = fx.state.registry.archive_app(&app_id).await;
    pat.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// ARM 3. The bypass that costs one JSON key.
///
/// `runtime_descriptor` is `skip_serializing_if = "Option::is_none"` on a
/// creator-produced artifact, so a one-armed guard is defeated by deleting the
/// field. The app would then boot with `env.db` uninstalled over a live
/// database.
#[compio::test]
async fn deploy_without_a_descriptor_is_refused_when_the_app_has_applied_schema() {
    let db_url = db_url();
    let fx = build_test_state(&db_url, "schema-missing").await;
    let app = deploy_service(fx.state.clone()).await;
    let pat = seed_owner(&fx.state, "schema-missing").await;
    let app_id = create_labelled_app(&fx.state, "schemamissing", &pat.user_id).await;

    insert_applied_migration(
        &fx.state.control_pg,
        &app_id,
        &pat.user_id,
        &sha256_hex(&descriptor_blob("v1")),
        "2026-08-28T00:00:00Z",
    )
    .await;

    let (status, body) =
        post_zship(&app, &app_id, &pat.bearer(), zship_with_descriptor(None)).await;

    assert_eq!(status, StatusCode::CONFLICT, "expected 409, body: {body}");
    assert_eq!(
        body.get("error").and_then(|v| v.as_str()),
        Some("schema_descriptor_missing"),
    );
    assert_eq!(
        live_deploy_hash(&fx.state, &app_id).await,
        None,
        "a descriptor-less deploy over a schema'd app must not go live",
    );

    let _ = fx.state.registry.archive_app(&app_id).await;
    pat.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// ARM 4. NEWEST APPLIED, NOT MEMBERSHIP.
///
/// This is the case an `IN (...)` implementation passes: N-1's descriptor WAS
/// applied once, so it is a member of the set forever, and a rollback across a
/// migration boundary would go live while the database sits at N. The two rows
/// differ only in `applied_at`, which is what "newest" is decided on.
#[compio::test]
async fn deploy_rolling_back_to_a_previously_applied_descriptor_is_refused() {
    let db_url = db_url();
    let fx = build_test_state(&db_url, "schema-rollback").await;
    let app = deploy_service(fx.state.clone()).await;
    let pat = seed_owner(&fx.state, "schema-rollback").await;
    let app_id = create_labelled_app(&fx.state, "schemarollback", &pat.user_id).await;

    let old = descriptor_blob("v1");
    let new = descriptor_blob("v2");
    insert_applied_migration(
        &fx.state.control_pg,
        &app_id,
        &pat.user_id,
        &sha256_hex(&old),
        "2026-08-27T00:00:00Z",
    )
    .await;
    insert_applied_migration(
        &fx.state.control_pg,
        &app_id,
        &pat.user_id,
        &sha256_hex(&new),
        "2026-08-28T00:00:00Z",
    )
    .await;

    let (status, body) = post_zship(
        &app,
        &app_id,
        &pat.bearer(),
        zship_with_descriptor(Some(&old)),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a descriptor that was applied ONCE but is not the newest must be refused, body: {body}",
    );
    assert_eq!(
        live_deploy_hash(&fx.state, &app_id).await,
        None,
        "the rolled-back deploy must not be live",
    );

    // The control: the NEWEST descriptor over the SAME two rows is accepted, so
    // the refusal above is about which row is newest and not about the app
    // holding two rows at all.
    let (status, body) = post_zship(
        &app,
        &app_id,
        &pat.bearer(),
        zship_with_descriptor(Some(&new)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "expected 200, body: {body}");
    assert!(live_deploy_hash(&fx.state, &app_id).await.is_some());

    let _ = fx.state.registry.archive_app(&app_id).await;
    pat.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

// ---------------------------------------------------------------------------
// Deploy command identity
//
// A deploy names its command in one `Idempotency-Key` header. The receipt binds
// the app, the actor, the content type and the digest of the bytes the handler
// consumed; an exact retry answers from it, and anything else under the same id
// is a conflict that says nothing about what the id was first used for.
// ---------------------------------------------------------------------------

/// POST a `.zship` with the given `Idempotency-Key` values, verbatim.
async fn post_command(
    app: &ntex::Pipeline<
        impl ntex::Service<
            ntex::http::Request,
            Response = ntex::web::WebResponse,
            Error = ntex::web::Error,
        >,
    >,
    app_id: &AppId,
    bearer: &str,
    keys: &[&str],
    body: Vec<u8>,
) -> (StatusCode, bool, serde_json::Value) {
    let mut req = test::TestRequest::post()
        .uri(&format!("/api/apps/{}/deploy", app_id.as_str()))
        .header("authorization", bearer)
        .header("content-type", "application/x-zship");
    for key in keys {
        req = req.header("idempotency-key", *key);
    }
    let resp = test::call_service(app, req.set_payload(body).to_request()).await;
    let status = resp.status();
    let replayed = resp
        .headers()
        .get("idempotent-replayed")
        .is_some_and(|value| value == "true");
    let bytes = test::read_body(resp).await;
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, replayed, json)
}

/// The app's lifecycle revision, its intents in revision order as
/// `(revision, action, deploy_id)`, and its command receipt count.
async fn publication_rows(
    state: &AppState,
    app_id: &AppId,
) -> (i64, Vec<(i64, String, Option<String>)>, i64) {
    let revision: i64 = state
        .control_pg
        .query_one(
            "SELECT lifecycle_revision FROM zeroship.apps WHERE id = $1",
            &[&app_id.as_str()],
        )
        .await
        .expect("lifecycle revision")
        .get(0);
    let intents = state
        .control_pg
        .query(
            "SELECT revision, action, deploy_id FROM zeroship.app_lifecycle_intents \
              WHERE app_id = $1 ORDER BY revision",
            &[&app_id.as_str()],
        )
        .await
        .expect("lifecycle intents")
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect();
    let receipts: i64 = state
        .control_pg
        .query_one(
            "SELECT COUNT(*)::bigint FROM zeroship.app_deploy_commands WHERE app_id = $1",
            &[&app_id.as_str()],
        )
        .await
        .expect("receipts")
        .get(0);
    (revision, intents, receipts)
}

/// A worker-only artifact whose single module is told apart by `marker`.
fn marked_zship(marker: &str) -> (Vec<u8>, String) {
    let server = format!("export default {{ fetch() {{ return new Response('{marker}'); }} }}");
    let server_hash = sha256_hex(server.as_bytes());
    let manifest = manifest_for(Some(&server_hash), &[]);
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    (
        build_zship(
            &manifest_bytes,
            &[(server_hash.clone(), server.into_bytes())],
            true,
        ),
        server_hash,
    )
}

/// Every malformed command identity is refused before a body byte is read,
/// and the canonical id one variable away is accepted.
#[compio::test]
async fn deploy_requires_one_canonical_command_id_before_reading_the_body() {
    let fx = build_test_state(&db_url(), "command-header").await;
    let app = deploy_service(fx.state.clone()).await;
    let pat = seed_owner(&fx.state, "command-header").await;
    let app_id = create_labelled_app(&fx.state, "commandheader", &pat.user_id).await;
    let (body, _) = marked_zship("command-header");
    let first = DeployCommandId::mint();
    let second = DeployCommandId::mint();
    let foreign = AppId::mint();
    let upper = first.as_str().to_ascii_uppercase();

    for keys in [
        vec![],
        vec![first.as_str(), second.as_str()],
        vec![first.as_str(), first.as_str()],
        vec!["dcm_bad"],
        vec![foreign.as_str()],
        vec![upper.as_str()],
        vec![""],
    ] {
        let (status, _, json) =
            post_command(&app, &app_id, &pat.bearer(), &keys, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{keys:?}: {json}");
        assert_eq!(json["error"], "invalid_idempotency_key", "{keys:?}");
        assert!(
            dir_is_empty(&fx.deploy_tmp_dir),
            "{keys:?}: the body streamed before the command id was checked"
        );
    }
    assert_eq!(publication_rows(&fx.state, &app_id).await, (0, vec![], 0));

    let (status, replayed, json) =
        post_command(&app, &app_id, &pat.bearer(), &[first.as_str()], body).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(!replayed);
    assert_eq!(json["command_id"], first.as_str());
    assert_eq!(json["lifecycle_revision"], 1);

    let _ = fx.state.registry.archive_app(&app_id).await;
    pat.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// A lost reply is recovered by resending the same bytes under the same id:
/// the original result comes back even after a later deploy moved the app and
/// the app's schema changed, and nothing is ingested, admitted, retargeted or
/// published again.
#[compio::test]
async fn an_exact_retry_returns_the_first_acceptance_without_republishing() {
    let fx = build_test_state(&db_url(), "command-replay").await;
    let app = deploy_service(fx.state.clone()).await;
    let pat = seed_owner(&fx.state, "command-replay").await;
    let app_id = create_labelled_app(&fx.state, "commandreplay", &pat.user_id).await;
    let (first_body, first_module) = marked_zship("first");
    let (second_body, _) = marked_zship("second");
    let first = DeployCommandId::mint();

    let (status, replayed, accepted) = post_command(
        &app,
        &app_id,
        &pat.bearer(),
        &[first.as_str()],
        first_body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert!(!replayed);
    assert_eq!(accepted["lifecycle_revision"], 1);
    assert_eq!(accepted["blobs_uploaded"], 1);

    let (status, _, later) = post_command(
        &app,
        &app_id,
        &pat.bearer(),
        &[DeployCommandId::mint().as_str()],
        second_body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{later}");
    // The retry must not ingest: remove the first artifact's module and check
    // that the replay does not put it back.
    std::fs::remove_file(fx.blob_store.local_path(&first_module).expect("local blob"))
        .expect("remove the first module blob");
    // Admission would now refuse the first artifact, which declares no schema.
    insert_applied_migration(
        &fx.state.control_pg,
        &app_id,
        &pat.user_id,
        &sha256_hex(b"a schema applied after the first deploy"),
        "2026-09-14T00:00:00Z",
    )
    .await;
    let before = publication_rows(&fx.state, &app_id).await;
    assert_eq!(before.0, 2);

    let (status, replayed, replay) =
        post_command(&app, &app_id, &pat.bearer(), &[first.as_str()], first_body).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert!(replayed, "an exact retry is marked as a replay");
    assert_eq!(
        replay, accepted,
        "the retry returns the first acceptance unchanged"
    );
    assert_eq!(publication_rows(&fx.state, &app_id).await, before);
    assert_eq!(
        live_deploy_hash(&fx.state, &app_id).await.as_deref(),
        later["deploy_hash"].as_str(),
        "a replay never retargets the app"
    );
    assert!(
        !fx.blob_store.has_blob(&first_module).await.unwrap(),
        "a replay answered from the receipt must not ingest the artifact again"
    );
    assert!(dir_is_empty(&fx.deploy_tmp_dir));

    let _ = fx.state.registry.archive_app(&app_id).await;
    pat.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// The same command id with other bytes, from another actor, or on another app
/// is refused without revealing anything about the first deploy, and without
/// ingesting the refused artifact.
#[compio::test]
async fn a_reused_command_id_conflicts_without_disclosing_its_receipt() {
    let fx = build_test_state(&db_url(), "command-conflict").await;
    let app = deploy_service(fx.state.clone()).await;
    let pat = seed_owner(&fx.state, "command-conflict").await;
    let app_id = create_labelled_app(&fx.state, "commandconflict", &pat.user_id).await;
    let other_app = create_labelled_app(&fx.state, "commandother", &pat.user_id).await;
    let (body, _) = marked_zship("original");
    let (changed, changed_module) = marked_zship("changed");
    let command = DeployCommandId::mint();

    let (status, _, accepted) = post_command(
        &app,
        &app_id,
        &pat.bearer(),
        &[command.as_str()],
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");

    // A second member who may deploy the same app. An admin reaches every
    // project in the organization without a project seat.
    let teammate = seed_owner(&fx.state, "command-conflict-teammate").await;
    let organization: String = fx
        .state
        .control_pg
        .query_one(
            "SELECT organization_id FROM zeroship.apps WHERE id = $1",
            &[&app_id.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    zeroship_control::organizations::add_member(
        &fx.state.registry,
        &pat.user_id,
        &organization,
        &zeroship_control::organizations::AddMemberBody {
            user_id: teammate.user_id.clone(),
            role: "admin".to_string(),
        },
        None,
    )
    .await
    .expect("seat an admin");
    // The control: the teammate can deploy this app under a fresh command.
    let (status, _, own) = post_command(
        &app,
        &app_id,
        &teammate.bearer(),
        &[DeployCommandId::mint().as_str()],
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{own}");
    let before = publication_rows(&fx.state, &app_id).await;

    for (target, bearer, bytes, case) in [
        (&app_id, pat.bearer(), changed.clone(), "changed body"),
        (&app_id, teammate.bearer(), body.clone(), "another actor"),
        (&other_app, pat.bearer(), body.clone(), "another app"),
    ] {
        let (status, replayed, json) =
            post_command(&app, target, &bearer, &[command.as_str()], bytes).await;
        assert_eq!(status, StatusCode::CONFLICT, "{case}: {json}");
        assert!(!replayed, "{case}");
        assert_eq!(json["error"], "idempotency_key_conflict", "{case}");
        for field in [
            "deploy_hash",
            "deploy_id",
            "command_id",
            "lifecycle_revision",
        ] {
            assert!(
                json.get(field).is_none(),
                "{case} disclosed {field}: {json}"
            );
        }
    }
    assert_eq!(publication_rows(&fx.state, &app_id).await, before);
    assert_eq!(
        publication_rows(&fx.state, &other_app).await,
        (0, vec![], 0)
    );
    assert!(
        !fx.blob_store.has_blob(&changed_module).await.unwrap(),
        "a conflicting artifact must not be ingested"
    );

    for target in [&app_id, &other_app] {
        let _ = fx.state.registry.archive_app(target).await;
    }
    teammate.cleanup(&fx.state).await;
    pat.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// Two copies of one deploy racing each other accept once: one receipt, one
/// revision, one intent, and both callers see the same result.
#[compio::test]
async fn concurrent_duplicate_deploys_accept_once() {
    let fx = build_test_state(&db_url(), "command-race").await;
    let app = deploy_service(fx.state.clone()).await;
    let pat = seed_owner(&fx.state, "command-race").await;
    let app_id = create_labelled_app(&fx.state, "commandrace", &pat.user_id).await;
    let (body, _) = marked_zship("race");
    let command = DeployCommandId::mint();

    let bearer = pat.bearer();
    let keys = [command.as_str()];
    let ((first, first_replayed, first_body), (second, second_replayed, second_body)) = futures::join!(
        post_command(&app, &app_id, &bearer, &keys, body.clone()),
        post_command(&app, &app_id, &bearer, &keys, body.clone()),
    );
    assert_eq!(first, StatusCode::OK, "{first_body}");
    assert_eq!(second, StatusCode::OK, "{second_body}");
    assert_eq!(
        first_body, second_body,
        "both callers see the one acceptance"
    );
    assert!(
        !(first_replayed && second_replayed),
        "one of the two requests performed the acceptance"
    );
    let (revision, intents, receipts) = publication_rows(&fx.state, &app_id).await;
    assert_eq!((revision, intents.len(), receipts), (1, 1, 1));

    let _ = fx.state.registry.archive_app(&app_id).await;
    pat.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// Redeploying an earlier artifact is an intentional rollback: a new command,
/// a new revision and a new activation of the original deployment row. The
/// artifact hash never stands in for the command.
#[compio::test]
async fn a_rollback_to_an_earlier_artifact_is_a_new_activation() {
    let fx = build_test_state(&db_url(), "command-rollback").await;
    let app = deploy_service(fx.state.clone()).await;
    let pat = seed_owner(&fx.state, "command-rollback").await;
    let app_id = create_labelled_app(&fx.state, "commandrollback", &pat.user_id).await;
    let (first, _) = marked_zship("rollback-first");
    let (second, _) = marked_zship("rollback-second");

    let mut results = Vec::new();
    for body in [first.clone(), second, first] {
        let (status, replayed, json) = post_command(
            &app,
            &app_id,
            &pat.bearer(),
            &[DeployCommandId::mint().as_str()],
            body,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert!(!replayed);
        results.push(json);
    }
    assert_eq!(results[2]["deploy_hash"], results[0]["deploy_hash"]);
    assert_eq!(
        results[2]["deploy_id"], results[0]["deploy_id"],
        "the rollback selects the original deployment row"
    );
    assert_ne!(results[2]["command_id"], results[0]["command_id"]);
    assert_eq!(results[2]["lifecycle_revision"], 3);
    let (revision, intents, receipts) = publication_rows(&fx.state, &app_id).await;
    assert_eq!((revision, receipts), (3, 3));
    let expected: Vec<_> = results
        .iter()
        .enumerate()
        .map(|(index, json)| {
            (
                i64::try_from(index + 1).unwrap(),
                "activate".to_string(),
                json["deploy_id"].as_str().map(str::to_owned),
            )
        })
        .collect();
    assert_eq!(intents, expected);
    assert_eq!(
        live_deploy_hash(&fx.state, &app_id).await.as_deref(),
        results[0]["deploy_hash"].as_str()
    );

    let _ = fx.state.registry.archive_app(&app_id).await;
    pat.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// A refused deploy leaves no receipt, revision, intent or pointer behind, so
/// the same command is evaluated afresh once the creator fixes the cause.
#[compio::test]
async fn a_refused_deploy_commits_nothing_and_the_same_command_can_succeed_later() {
    let fx = build_test_state(&db_url(), "command-refused").await;
    let app = deploy_service(fx.state.clone()).await;
    let pat = seed_owner(&fx.state, "command-refused").await;
    let app_id = create_labelled_app(&fx.state, "commandrefused", &pat.user_id).await;
    let blob = descriptor_blob("refused");
    let body = zship_with_descriptor(Some(&blob));
    let command = DeployCommandId::mint();

    let (status, _, json) = post_command(
        &app,
        &app_id,
        &pat.bearer(),
        &[command.as_str()],
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(json["error"], "schema_not_applied");
    assert_eq!(publication_rows(&fx.state, &app_id).await, (0, vec![], 0));
    assert_eq!(live_deploy_hash(&fx.state, &app_id).await, None);

    // A schedule the manager would refuse is refused before acceptance too.
    let server = b"export default { fetch() { return new Response('ok'); } }";
    let server_hash = sha256_hex(server);
    let mut manifest = manifest_for(Some(&server_hash), &[]);
    manifest.workflows = Some(serde_json::json!(["Nightly"]));
    manifest.schedules = vec![serde_json::json!({
        "name": "too-often",
        "workflowName": "Nightly",
        "schedule": {"kind": "interval", "interval_ms": 1, "anchor": "epoch"},
    })];
    let invalid = build_zship(
        &serde_json::to_vec(&manifest).unwrap(),
        &[(server_hash, server.to_vec())],
        true,
    );
    let (status, _, json) = post_command(
        &app,
        &app_id,
        &pat.bearer(),
        &[DeployCommandId::mint().as_str()],
        invalid,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["error"], "invalid_workflow_schedules");
    assert_eq!(publication_rows(&fx.state, &app_id).await, (0, vec![], 0));

    insert_applied_migration(
        &fx.state.control_pg,
        &app_id,
        &pat.user_id,
        &sha256_hex(&blob),
        "2026-09-14T00:00:00Z",
    )
    .await;
    let (status, replayed, json) =
        post_command(&app, &app_id, &pat.bearer(), &[command.as_str()], body).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(!replayed, "a refusal is not a receipt");
    assert_eq!(json["lifecycle_revision"], 1);
    assert_eq!(publication_rows(&fx.state, &app_id).await.2, 1);

    let _ = fx.state.registry.archive_app(&app_id).await;
    pat.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// A deleted app is refused before any receipt lookup: an exact retry of a
/// command it once accepted cannot report success or stage code again.
#[compio::test]
async fn a_deleted_app_refuses_command_replay() {
    let fx = build_test_state(&db_url(), "command-deleted").await;
    let app = deploy_service(fx.state.clone()).await;
    let pat = seed_owner(&fx.state, "command-deleted").await;
    let app_id = create_labelled_app(&fx.state, "commanddeleted", &pat.user_id).await;
    let (body, _) = marked_zship("deleted");
    let command = DeployCommandId::mint();

    let (status, _, json) = post_command(
        &app,
        &app_id,
        &pat.bearer(),
        &[command.as_str()],
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    fx.state
        .registry
        .archive_app(&app_id)
        .await
        .expect("archive")
        .expect("the app exists");
    zeroship_control::organizations::delete_app(&fx.state.registry, &pat.user_id, &app_id, None)
        .await
        .expect("delete");
    let before = publication_rows(&fx.state, &app_id).await;

    let (status, replayed, json) =
        post_command(&app, &app_id, &pat.bearer(), &[command.as_str()], body).await;
    assert!(
        status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND,
        "a deleted app must be refused, got {status}: {json}"
    );
    assert!(!replayed);
    assert!(json.get("deploy_hash").is_none(), "{json}");
    assert_eq!(publication_rows(&fx.state, &app_id).await, before);
    let deleted: (Option<String>, bool) = fx
        .state
        .control_pg
        .query_one(
            "SELECT deploy_hash, deleted_at IS NOT NULL FROM zeroship.apps WHERE id = $1",
            &[&app_id.as_str()],
        )
        .await
        .map(|row| (row.get(0), row.get(1)))
        .unwrap();
    assert_eq!(
        deleted,
        (None, true),
        "the deleted app stays deleted and empty"
    );

    pat.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}
