//! Live-PG regression tests for user-facing OAuth grant handlers.

#![allow(clippy::future_not_send)]

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use compio_postgres::{connect, NoTls};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::{
    api, oauth_grants_handlers, AppState, EnvStore, Quota, RateLimiter,
    Registry, SecretString, StripeStore,
};

use crate::common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn db_url() -> String {
    crate::common::require_control_db()
}

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-oauth-grants-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

struct Fixture {
    state: Arc<AppState>,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
}

impl Fixture {
    /// Build a fixture on the single physical `zeroship` DB. `registry`,
    /// `control_pg`, and every handler share that one database — there is no
    /// separate auth DB any more (the former `--auth-db` was config-only).
    async fn new(db_url: &str, label: &str) -> Self {
        let (control_pg_client, control_pg_conn) =
            connect(db_url, NoTls).await.expect("control-pg connect");
        compio::runtime::spawn(async move {
            let _ = control_pg_conn.run().await;
        })
        .detach();

        let blob_root = tmpdir(&format!("blob-{label}"));
        let deploy_tmp_dir = tmpdir(&format!("deploy-{label}"));
        let registry = Registry::new(db_url).await.expect("registry");
    zeroship_control::plan_catalog::seed_plans(&registry).await.expect("seed built-in plans");
        let env_store =
            EnvStore::new(registry.clone(), TEST_MASTER_KEY).expect("env store");
        let stripe_store = StripeStore::new(registry.clone());
        let blob_store: Arc<dyn BlobStore> =
            Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
        let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
            zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
                .expect("workflow blob store"),
        );

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
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            origin_scheme: zeroship_core::config::OriginScheme::Https,
            trust_proxy: false,
            deploy_tmp_dir: deploy_tmp_dir.clone(),
            control_pg: Arc::new(control_pg_client),
            app_base_domain: "zeroship.localhost".to_string(),
            trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
            expected_oauth_audience: "control.zeroship.ai".to_string(),
            static_policies: zeroship_authz::load_platform_policies()
                .expect("bundled authz policies parse"),
            auth_provider: zeroship_control::platform_auth_provider("https://auth.zeroship.test/oauth2", Some(common::platform_jwks_url())),
            // A real, non-zero pairwise salt so the disconnect-app cascade
            // writes a `token_revocations` marker under a `pws_` the test can
            // re-derive with the SAME salt + sector (Batch A fix 4).
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
            pairwise_salt: zeroship_core::auth::derive_pairwise_salt(b"control-test-stash"),
            projected_charge_cache: std::sync::Arc::new(
                zeroship_control::billing_read::ProjectedChargeCache::default(),
            ),
        });

        Self {
            state,
            blob_root,
            deploy_tmp_dir,
        }
    }

    async fn cleanup_clients(&self, client_ids: &[String]) {
        if client_ids.is_empty() {
            return;
        }
        let ids = client_ids.iter().map(String::as_str).collect::<Vec<_>>();
        let _ = self
            .state
            .control_pg
            .execute(
                "DELETE FROM zeroship.oauth_grants WHERE client_id = ANY($1)",
                &[&ids],
            )
            .await;
        let _ = self
            .state
            .control_pg
            .execute(
                "DELETE FROM zeroship.oauth_clients WHERE client_id = ANY($1)",
                &[&ids],
            )
            .await;
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

struct AccountCaller {
    user_id: Uuid,
    token: String,
}

impl AccountCaller {
    fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }

    async fn cleanup(&self, state: &AppState) {
        cleanup_user(state, self.user_id).await;
    }
}

// "account self-service": AccountRead + AccountWrite. A scope always lowers to
// `Resource::Any`, matching the wrapper policy this used to carry (it was
// already unconstrained), so the translation is direct.
async fn account_caller(state: &AppState, label: &str) -> AccountCaller {
    let user_id = insert_user(state, label).await;
    let token = common::platform_token_for_client(
        user_id,
        "account:read account:write",
        common::CONSOLE_CLIENT_ID,
    );

    AccountCaller { user_id, token }
}

async fn insert_user(state: &AppState, label: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    let email = format!("{label}-{user_id}@zeroship.test");
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, $3, NOW())",
            &[&user_id, &email, &label],
        )
        .await
        .expect("insert test user");
    user_id
}

async fn cleanup_user(state: &AppState, user_id: Uuid) {
    let _ = state
        .control_pg
        .execute(
            "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
            &[&user_id],
        )
        .await;
    let _ = state
        .control_pg
        .execute(
            "DELETE FROM zeroship.oauth_grants WHERE user_id = $1",
            &[&user_id],
        )
        .await;
    let _ = state
        .control_pg
        .execute("DELETE FROM zeroship.platform_admin_roles WHERE user_id = $1", &[&user_id])
        .await;
    let _ = state
        .control_pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
        .await;
}

async fn insert_client(state: &AppState, client_id: &str, created_by: Uuid) {
    let redirect_uri = format!("https://{client_id}.example/callback");
    let redirect_uris = vec![redirect_uri.as_str()];
    let scopes = vec!["apps:read", "env:read"];
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.oauth_clients \
                (client_id, client_name, client_uri, logo_uri, redirect_uris, scopes, \
                 skip_consent, created_by) \
             VALUES ($1, $2, $3, $4, $5, $6, false, $7)",
            &[
                &client_id,
                &format!("Client {client_id}"),
                &Some(format!("https://{client_id}.example")),
                &Some(format!("https://{client_id}.example/logo.png")),
                &redirect_uris,
                &scopes,
                &created_by,
            ],
        )
        .await
        .expect("insert oauth client");
}

async fn insert_grant(state: &AppState, user_id: Uuid, client_id: &str, scopes: &[&str]) {
    let granted_scopes = scopes.to_vec();
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.oauth_grants \
                (user_id, client_id, granted_scopes, granted_at, last_used_at) \
             VALUES ($1, $2, $3, NOW(), NOW())",
            &[&user_id, &client_id, &granted_scopes],
        )
        .await
        .expect("insert oauth grant");
}

/// Seed a `zeroship.app_user_identities` row with a minted relay alias keyed on
/// `(client_id, user_id)` — the row the gateway writes and the alias consent
/// mints. The relay revocation cascade revokes THIS row.
///
/// `sector` is the app's apex `sector_identifier`. The `pairwise_sub` is DERIVED
/// from it with the same salt the `AppState` carries, so the row holds the value
/// the gateway would actually project for this `(app, user)` rather than an
/// invented string. An invented one is not merely cosmetic: it is the value a
/// live token carries, so any assertion downstream of a token mint for this pair
/// would be comparing against something no real credential ever holds.
async fn insert_identity_with_alias(
    state: &AppState,
    client_id: &str,
    sector: &str,
    user_id: Uuid,
    relay_email: &str,
) {
    let pairwise_sub =
        zeroship_core::auth::derive_pairwise(&state.pairwise_salt, &user_id.to_string(), sector);
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.app_user_identities \
                (app_client_id, global_user_id, pairwise_sub, relay_email) \
             VALUES ($1, $2, $3, $4)",
            &[&client_id, &user_id, &pairwise_sub, &relay_email],
        )
        .await
        .expect("insert app_user_identities row");
}

/// The active-alias resolution the 5b relay webhook runs on EVERY inbound
/// (`relay::resolve_active_alias`, sub-spec §4.5). `None` ⇒ the webhook emits a
/// bounce + 200 (the revoked/unknown-alias branch). We assert against the REAL
/// auth-store gate, not a stub, so this is the faithful cross-service seam.
async fn alias_is_active(state: &AppState, relay_email: &str) -> bool {
    zeroship_auth::store::relay::resolve_active_alias(state.control_pg.as_ref(), relay_email)
        .await
        .expect("resolve_active_alias")
        .is_some()
}

async fn identity_revoked_at_is_set(state: &AppState, client_id: &str, user_id: Uuid) -> bool {
    let rows = state
        .control_pg
        .query(
            "SELECT revoked_at FROM zeroship.app_user_identities \
             WHERE app_client_id = $1 AND global_user_id = $2",
            &[&client_id, &user_id],
        )
        .await
        .expect("query identity revoked_at");
    rows.first()
        .and_then(|r| r.get::<_, Option<chrono::DateTime<Utc>>>("revoked_at"))
        .is_some()
}

async fn identity_row_count(state: &AppState, client_id: &str) -> i64 {
    let rows = state
        .control_pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM zeroship.app_user_identities \
             WHERE app_client_id = $1",
            &[&client_id],
        )
        .await
        .expect("count app_user_identities");
    rows[0].get("n")
}

async fn cleanup_identities(state: &AppState, client_id: &str) {
    let _ = state
        .control_pg
        .execute(
            "DELETE FROM zeroship.app_user_identities WHERE app_client_id = $1",
            &[&client_id],
        )
        .await;
}

async fn count_grant(state: &AppState, user_id: Uuid, client_id: &str) -> i64 {
    let rows = state
        .control_pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n \
             FROM zeroship.oauth_grants \
             WHERE user_id = $1 AND client_id = $2",
            &[&user_id, &client_id],
        )
        .await
        .expect("count oauth grant");
    rows[0].get("n")
}

async fn audit_event_count(
    state: &AppState,
    user_id: Uuid,
    event_type: &str,
    client_id: &str,
) -> i64 {
    let rows = state
        .control_pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n \
             FROM zeroship.audit_events \
             WHERE actor_user_id = $1 AND event_type = $2 AND client_id = $3",
            &[&user_id, &event_type, &client_id],
        )
        .await
        .expect("count audit events");
    rows[0].get("n")
}

macro_rules! init_control {
    ($fx:expr) => {{
        test::init_service(
            web::App::new()
                .state($fx.state.clone())
                .configure(oauth_grants_handlers::configure),
        )
        .await
    }};
}

#[compio::test]
async fn list_returns_empty_when_no_grants() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "empty").await;
    let caller = account_caller(&fx.state, "empty").await;
    let app = init_control!(fx);

    let req = test::TestRequest::get()
        .uri("/me/oauth-grants")
        .header("authorization", caller.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("list json");
    assert_eq!(body, json!([]));

    caller.cleanup(&fx.state).await;

    // Teardown: the service and the fixture both hold connections, and locals
    // are dropped only after the body returns - by which point the runtime is
    // gone and the sockets can no longer be closed. Drop them explicitly, then
    // wait for the close to land.
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn list_returns_user_grants_with_client_metadata() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "metadata").await;
    let caller = account_caller(&fx.state, "metadata").await;
    let app = init_control!(fx);
    let client_id = format!("oauth-grant-metadata-{}", Uuid::new_v4().simple());
    insert_client(&fx.state, &client_id, caller.user_id).await;
    insert_grant(&fx.state, caller.user_id, &client_id, &["apps:read", "env:read"]).await;

    let req = test::TestRequest::get()
        .uri("/me/oauth-grants")
        .header("authorization", caller.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("list json");
    let grants = body.as_array().expect("grants array");
    assert_eq!(grants.len(), 1);
    let grant = &grants[0];
    assert_eq!(grant["client_id"].as_str(), Some(client_id.as_str()));
    assert_eq!(grant["client_name"], format!("Client {client_id}"));
    assert_eq!(
        grant["client_uri"],
        format!("https://{client_id}.example")
    );
    assert_eq!(
        grant["logo_uri"],
        format!("https://{client_id}.example/logo.png")
    );
    assert_eq!(grant["granted_scopes"], json!(["apps:read", "env:read"]));
    assert!(grant["granted_at"].as_str().is_some_and(|value| value.contains('T')));
    assert!(grant["last_used_at"].as_str().is_some_and(|value| value.contains('T')));

    fx.cleanup_clients(&[client_id]).await;
    caller.cleanup(&fx.state).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn list_does_not_leak_other_users_grants() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "isolation").await;
    let caller = account_caller(&fx.state, "isolation-a").await;
    let other_user = insert_user(&fx.state, "isolation-b").await;
    let app = init_control!(fx);
    let client_a = format!("oauth-grant-a-{}", Uuid::new_v4().simple());
    let client_b = format!("oauth-grant-b-{}", Uuid::new_v4().simple());
    insert_client(&fx.state, &client_a, caller.user_id).await;
    insert_client(&fx.state, &client_b, caller.user_id).await;
    insert_grant(&fx.state, caller.user_id, &client_a, &["apps:read"]).await;
    insert_grant(&fx.state, other_user, &client_b, &["env:read"]).await;

    let req = test::TestRequest::get()
        .uri("/me/oauth-grants")
        .header("authorization", caller.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("list json");
    let grants = body.as_array().expect("grants array");
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0]["client_id"].as_str(), Some(client_a.as_str()));

    fx.cleanup_clients(&[client_a, client_b]).await;
    cleanup_user(&fx.state, other_user).await;
    caller.cleanup(&fx.state).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn revoke_removes_grant_row() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "revoke-row").await;
    let caller = account_caller(&fx.state, "revoke-row").await;
    let app = init_control!(fx);
    let client_id = format!("oauth-grant-revoke-{}", Uuid::new_v4().simple());
    insert_client(&fx.state, &client_id, caller.user_id).await;
    insert_grant(&fx.state, caller.user_id, &client_id, &["apps:read"]).await;

    let req = test::TestRequest::delete()
        .uri(&format!("/me/oauth-grants/{client_id}"))
        .header("authorization", caller.bearer())
        .to_request();
    // Status only: a retained `WebResponse` keeps the app state - and its
    // Postgres client - alive past the teardown below.
    let status = test::call_service(&app, req).await.status();

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(count_grant(&fx.state, caller.user_id, &client_id).await, 0);
    assert_eq!(
        audit_event_count(&fx.state, caller.user_id, "oauth_grant_revoke", &client_id).await,
        1
    );

    fx.cleanup_clients(&[client_id]).await;
    caller.cleanup(&fx.state).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn revoke_removes_native_grant_for_user_client_pair() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "revoke-native").await;
    let caller = account_caller(&fx.state, "revoke-native").await;
    let app = init_control!(fx);
    let client_id = format!("oauth-grant-native-{}", Uuid::new_v4().simple());
    insert_client(&fx.state, &client_id, caller.user_id).await;
    insert_grant(&fx.state, caller.user_id, &client_id, &["apps:read"]).await;

    let req = test::TestRequest::delete()
        .uri(&format!("/me/oauth-grants/{client_id}"))
        .header("authorization", caller.bearer())
        .to_request();
    // Status only: a retained `WebResponse` keeps the app state - and its
    // Postgres client - alive past the teardown below.
    let status = test::call_service(&app, req).await.status();

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(count_grant(&fx.state, caller.user_id, &client_id).await, 0);
    assert_eq!(
        audit_event_count(&fx.state, caller.user_id, "oauth_grant_revoke", &client_id).await,
        1
    );

    fx.cleanup_clients(&[client_id]).await;
    caller.cleanup(&fx.state).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn revoke_returns_404_when_no_grant() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "missing").await;
    let caller = account_caller(&fx.state, "missing").await;
    let app = init_control!(fx);
    let client_id = format!("oauth-grant-missing-{}", Uuid::new_v4().simple());

    let req = test::TestRequest::delete()
        .uri(&format!("/me/oauth-grants/{client_id}"))
        .header("authorization", caller.bearer())
        .to_request();
    // Status only: a retained `WebResponse` keeps the app state - and its
    // Postgres client - alive past the teardown below.
    let status = test::call_service(&app, req).await.status();

    assert_eq!(status, StatusCode::NOT_FOUND);

    caller.cleanup(&fx.state).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn revoke_does_not_affect_other_users() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "other-user").await;
    let owner = account_caller(&fx.state, "other-user-owner").await;
    let revoker = account_caller(&fx.state, "other-user-revoker").await;
    let app = init_control!(fx);
    let client_id = format!("oauth-grant-other-{}", Uuid::new_v4().simple());
    insert_client(&fx.state, &client_id, owner.user_id).await;
    insert_grant(&fx.state, owner.user_id, &client_id, &["apps:read"]).await;

    let req = test::TestRequest::delete()
        .uri(&format!("/me/oauth-grants/{client_id}"))
        .header("authorization", revoker.bearer())
        .to_request();
    // Status only: a retained `WebResponse` keeps the app state - and its
    // Postgres client - alive past the teardown below.
    let status = test::call_service(&app, req).await.status();

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(count_grant(&fx.state, owner.user_id, &client_id).await, 1);

    fx.cleanup_clients(&[client_id]).await;
    revoker.cleanup(&fx.state).await;
    owner.cleanup(&fx.state).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// 5c §6 — the B4 revocation cascade: revoking a grant sets
/// `app_user_identities.revoked_at` AND a subsequent inbound to that alias
/// bounces (the real 5b `resolve_active_alias` gate now returns `None`). The
/// DELETE + UPDATE commit atomically (BEGIN/COMMIT) on control's existing
/// `control_pg` connection — no per-call connect. This is the full faithful loop:
/// the relay alias was forwarding (active) → revoke → it bounces (inactive).
#[compio::test]
async fn revoke_cascade_revokes_relay_alias_so_inbound_bounces() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "cascade").await;
    let caller = account_caller(&fx.state, "cascade").await;
    let app = init_control!(fx);
    let client_id = format!("oauth-grant-cascade-{}", Uuid::new_v4().simple());
    let sector = format!("https://{client_id}.zeroship.localhost");
    insert_client(&fx.state, &client_id, caller.user_id).await;
    insert_grant(&fx.state, caller.user_id, &client_id, &["apps:read", "email"]).await;
    let relay_email = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());
    insert_identity_with_alias(&fx.state, &client_id, &sector, caller.user_id, &relay_email).await;

    // Pre-condition: the alias forwards (active map present — what 5b resolves).
    assert!(
        alias_is_active(&fx.state, &relay_email).await,
        "alias must be active (forwarding) BEFORE revoke"
    );

    // Revoke via the REAL control HTTP handler (runs the §6 cascade).
    let req = test::TestRequest::delete()
        .uri(&format!("/me/oauth-grants/{client_id}"))
        .header("authorization", caller.bearer())
        .to_request();
    // Status only: a retained `WebResponse` keeps the app state - and its
    // Postgres client - alive past the teardown below.
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The grant is gone AND the alias is revoked — committed atomically.
    assert_eq!(count_grant(&fx.state, caller.user_id, &client_id).await, 0);
    assert!(
        identity_revoked_at_is_set(&fx.state, &client_id, caller.user_id).await,
        "revoke must set app_user_identities.revoked_at (the cascade UPDATE)"
    );
    // The faithful seam: the 5b webhook's active-map resolution now returns
    // None ⇒ inbound to this alias BOUNCES (revoked/unknown-alias branch, §8).
    assert!(
        !alias_is_active(&fx.state, &relay_email).await,
        "after revoke, inbound to the alias must bounce (resolve_active_alias → None)"
    );

    cleanup_identities(&fx.state, &client_id).await;
    fx.cleanup_clients(&[client_id]).await;
    caller.cleanup(&fx.state).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// Seed the `zeroship.app_oauth_clients` extension row carrying the app's apex
/// `sector_identifier` — what the disconnect-app cascade reads (Batch A fix 4)
/// to derive the per-app `pws_` before writing the token-family marker.
/// `app_oauth_clients.app_id` FKs `zeroship.apps`, so we seed a minimal app row
/// first. Returns the seeded `app_id` so the caller can clean it up.
async fn insert_app_oauth_client(state: &AppState, client_id: &str, sector: &str) -> Uuid {
    let app_id = Uuid::new_v4();
    // This case is about the OAuth client row, not about who owns the app.
    let project = common::unowned_project(&state.control_pg).await;
    // `apps.plan_id` FKs `zeroship.plans`; the column default is the literal
    // string 'free', but the catalog's built-in free tier is keyed by the
    // derived `pln_<base62>` id (`free_plan_id()`), NOT 'free'. Seed it
    // explicitly so the insert satisfies `apps_plan_fk`. The fixture already
    // seeded the built-in plans (`seed_plans`), so this id is present.
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.apps (id, name, plan_id, project_id, organization_id) \
             SELECT $1, $2, $3, p.id, p.organization_id FROM zeroship.projects p WHERE p.id = $4",
            &[
                &app_id,
                &format!("app-{}", app_id.simple()),
                &zeroship_control::plan_catalog::free_plan_id(),
                &project,
            ],
        )
        .await
        .expect("insert apps row");
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.app_oauth_clients (app_id, client_id, sector_identifier) \
             VALUES ($1, $2, $3)",
            &[&app_id, &client_id, &sector],
        )
        .await
        .expect("insert app_oauth_clients row");
    app_id
}

/// Batch A fix 4: a dashboard "disconnect app" (DELETE /me/oauth-grants/{id})
/// must write the per-app token-family marker so the user's LIVE access token
/// dies — not just the relay alias. After revoke we assert a
/// `auth.token_revocations` row exists for `(client_id, pws_)` AND that the
/// REAL gateway reader (`is_family_revoked_since`) — keyed exactly as the
/// wrapper / Bearer / DPoP arms key it — now reports a still-live token (one
/// whose `iat` predates the marker) as revoked. PG-gated.
#[compio::test]
async fn revoke_grant_writes_token_family_marker_that_rejects_live_token() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "tokmarker").await;
    let caller = account_caller(&fx.state, "tokmarker").await;
    let app = init_control!(fx);
    let client_id = format!("oac_tokmarker_{}", Uuid::new_v4().simple());
    let sector = format!("https://{}.zeroship.localhost", Uuid::new_v4().simple());
    insert_client(&fx.state, &client_id, caller.user_id).await;
    let app_id = insert_app_oauth_client(&fx.state, &client_id, &sector).await;
    insert_grant(&fx.state, caller.user_id, &client_id, &["apps:read", "email"]).await;
    let relay_email = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());
    insert_identity_with_alias(&fx.state, &client_id, &sector, caller.user_id, &relay_email).await;

    // The per-app pws_ the gateway projects for this (app, user) — derived with
    // the SAME salt the AppState carries + the app's sector. A live token for
    // this user carries this sub.
    let pws = zeroship_core::auth::derive_pairwise(
        &fx.state.pairwise_salt,
        &caller.user_id.to_string(),
        &sector,
    );
    // A token issued BEFORE the disconnect (iat in the past) — what "live"
    // means: still cryptographically valid, must be rejected after revoke.
    let live_token_iat = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
        - 60;

    // Pre-condition: no marker yet ⇒ the live token is NOT revoked.
    assert!(
        !zeroship_authz::wrapper_revocation::is_family_revoked_since(
            fx.state.control_pg.as_ref(),
            &client_id,
            &pws,
            live_token_iat,
        )
        .await
        .expect("pre-revoke family check"),
        "before disconnect, the live token must NOT be family-revoked"
    );

    // Disconnect the app via the REAL control HTTP handler.
    let req = test::TestRequest::delete()
        .uri(&format!("/me/oauth-grants/{client_id}"))
        .header("authorization", caller.bearer())
        .to_request();
    // Status only: a retained `WebResponse` keeps the app state - and its
    // Postgres client - alive past the teardown below.
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The marker row exists for (client_id, pws_).
    let marker_rows = fx
        .state
        .control_pg
        .query(
            "SELECT 1 FROM zeroship.token_revocations WHERE client_id = $1 AND sub = $2",
            &[&client_id, &pws],
        )
        .await
        .expect("query token_revocations");
    assert_eq!(
        marker_rows.len(),
        1,
        "disconnect-app must write a token_revocations row keyed on (client_id, pws_)"
    );

    // The faithful seam: the EXACT reader the gateway arms run now reports the
    // still-live token as revoked. A regression that dropped this write (or
    // keyed it on the global UUID) would leave the live token accepted here.
    assert!(
        zeroship_authz::wrapper_revocation::is_family_revoked_since(
            fx.state.control_pg.as_ref(),
            &client_id,
            &pws,
            live_token_iat,
        )
        .await
        .expect("post-revoke family check"),
        "after disconnect, the live token (iat before the marker) must be family-revoked"
    );

    // Cleanup.
    fx.state
        .control_pg
        .execute(
            "DELETE FROM zeroship.token_revocations WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .ok();
    fx.state
        .control_pg
        .execute(
            "DELETE FROM zeroship.app_oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .ok();
    cleanup_identities(&fx.state, &client_id).await;
    fx.cleanup_clients(&[client_id]).await;
    // The apps row FKs app_oauth_clients (deleted above) — drop it last.
    fx.state
        .control_pg
        .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id])
        .await
        .ok();
    caller.cleanup(&fx.state).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// 5c §6.1 — re-grant stability: revoke then re-grant reuses the SAME alias
/// (Apple Hide-My-Email model). After re-grant `revoked_at` is cleared and the
/// alias forwards again — no new alias, no dead-alias bounce. The auth-side
/// writer (`mint_alias_at_consent`) clears `revoked_at` on the deterministic
/// row; here we exercise that clear directly to prove the row is reusable.
#[compio::test]
async fn re_grant_reuses_same_alias_with_cleared_revoked_at() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "regrant").await;
    let caller = account_caller(&fx.state, "regrant").await;
    let app = init_control!(fx);
    let client_id = format!("oauth-grant-regrant-{}", Uuid::new_v4().simple());
    let sector = format!("https://{client_id}.zeroship.localhost");
    insert_client(&fx.state, &client_id, caller.user_id).await;
    insert_grant(&fx.state, caller.user_id, &client_id, &["email"]).await;
    let relay_email = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());
    insert_identity_with_alias(&fx.state, &client_id, &sector, caller.user_id, &relay_email).await;

    // Revoke → alias goes inactive.
    let req = test::TestRequest::delete()
        .uri(&format!("/me/oauth-grants/{client_id}"))
        .header("authorization", caller.bearer())
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::NO_CONTENT
    );
    assert!(!alias_is_active(&fx.state, &relay_email).await);

    // Re-grant, modeled as auth's `accept_consent` does it: re-insert the grant
    // ledger row AND clear the alias's revoked_at. The structural revoke gate
    // (BLOCKER fix) requires BOTH — `mint_alias_at_consent` un-revokes the alias
    // row, and the grant upsert restores the live `zeroship.oauth_grants` row the
    // alias's `EXISTS` gate consults. (Un-revoking the alias alone, without the
    // grant, leaves it correctly inert — that is the structural fix's whole
    // point and is asserted by the §10 race test.)
    insert_grant(&fx.state, caller.user_id, &client_id, &["email"]).await;
    let reused = zeroship_auth::store::relay::mint_alias_at_consent(
        fx.state.control_pg.as_ref(),
        &client_id,
        caller.user_id,
        "relay.zeroship.localhost",
    )
    .await
    .expect("re-grant mint");
    assert_eq!(
        reused.as_deref(),
        Some(relay_email.as_str()),
        "re-grant must reuse the SAME alias (Apple Hide-My-Email), not mint a new one"
    );
    assert!(
        alias_is_active(&fx.state, &relay_email).await,
        "after re-grant (grant restored + revoked_at cleared) the alias forwards again"
    );

    cleanup_identities(&fx.state, &client_id).await;
    fx.cleanup_clients(&[client_id]).await;
    caller.cleanup(&fx.state).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// Raw `(grant_present, revoked_at_is_set)` snapshot of the terminal state, so
/// the race test can assert the STRUCTURAL invariant directly (grant-absent ⇒
/// alias-inert) independent of which writer won the `revoked_at` write.
async fn grant_and_alias_state(
    state: &AppState,
    client_id: &str,
    user_id: Uuid,
) -> (bool, bool) {
    let grant_present = count_grant(state, user_id, client_id).await > 0;
    let revoked_set = identity_revoked_at_is_set(state, client_id, user_id).await;
    (grant_present, revoked_set)
}

/// BLOCKER §10 race regression — the relay revoke↔re-consent race.
///
/// auth's `accept_consent` (grant upsert + alias un-revoke) and control's
/// `revoke_grant_cascade` (grant DELETE + alias `revoked_at=now()` UPDATE) write
/// across two schemas with NO shared mutex (auth holds a `pg_advisory_lock` that
/// does NOT block control's row DELETE/UPDATE). So an interleaving can reach a
/// terminal state where the GRANT IS ABSENT but the alias's `revoked_at` is NULL
/// — which, under the OLD `resolve_active_alias` (alias-flag-only gate), would
/// keep forwarding third-party mail to the real inbox after the user revoked.
///
/// The structural fix makes `resolve_active_alias` ALSO require a live
/// `zeroship.oauth_grants` row, so a deleted grant silences the alias regardless
/// of the `revoked_at` write ordering. This test drives BOTH commit orders and
/// asserts the load-bearing invariant in each: **grant-absent ⇒ alias-inert**
/// (the alias does NOT resolve), even when `revoked_at` was left NULL by the
/// losing writer.
#[compio::test]
async fn revoke_vs_reconsent_race_grant_absent_implies_alias_inert() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "revoke-race").await;
    let caller = account_caller(&fx.state, "revoke-race").await;
    let app = init_control!(fx);

    // A re-consent re-grant, modeled exactly as auth's accept_consent does it:
    // upsert the grant ledger row AND clear the alias's revoked_at (the
    // mint_alias_at_consent un-revoke). We run the alias un-revoke via the REAL
    // auth-store writer so this is a faithful cross-service seam, not a stub.
    async fn reconsent(state: &AppState, client_id: &str, user_id: Uuid, relay_domain: &str) {
        // grant upsert (the ledger write accept_consent performs under its lock)
        state
            .control_pg
            .execute(
                "INSERT INTO zeroship.oauth_grants \
                    (user_id, client_id, granted_scopes, granted_at, updated_at) \
                 VALUES ($1, $2, $3, NOW(), NOW()) \
                 ON CONFLICT (user_id, client_id) DO UPDATE \
                 SET granted_scopes = EXCLUDED.granted_scopes, updated_at = NOW()",
                &[&user_id, &client_id, &vec!["email".to_string()]],
            )
            .await
            .expect("reconsent grant upsert");
        // alias un-revoke (the REAL auth-side writer)
        zeroship_auth::store::relay::mint_alias_at_consent(
            state.control_pg.as_ref(),
            client_id,
            user_id,
            relay_domain,
        )
        .await
        .expect("reconsent alias un-revoke");
    }

    // ── Commit order A: re-consent commits FULLY, then revoke commits FULLY ──
    // Terminal: grant absent (revoke DELETEd it last) + revoked_at set. The
    // alias must be inert by EITHER gate.
    {
        let client_id = format!("oac_race_a_{}", Uuid::new_v4().simple());
        let sector = format!("https://{}.zeroship.localhost", Uuid::new_v4().simple());
        insert_client(&fx.state, &client_id, caller.user_id).await;
        let app_id = insert_app_oauth_client(&fx.state, &client_id, &sector).await;
        insert_grant(&fx.state, caller.user_id, &client_id, &["email"]).await;
        let relay_email = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());
        insert_identity_with_alias(&fx.state, &client_id, &sector, caller.user_id, &relay_email).await;
        assert!(alias_is_active(&fx.state, &relay_email).await, "active before");

        // First revoke (sets revoked_at + DELETEs grant), then re-consent fully
        // re-grants (un-revoke + grant), then revoke AGAIN as the LAST writer.
        let status = test::call_service(
            &app,
            test::TestRequest::delete()
                .uri(&format!("/me/oauth-grants/{client_id}"))
                .header("authorization", caller.bearer())
                .to_request(),
        )
        .await
        .status();
        assert_eq!(status, StatusCode::NO_CONTENT);
        reconsent(&fx.state, &client_id, caller.user_id, "relay.zeroship.localhost").await;
        // After re-consent the alias forwards again (grant present, revoked_at cleared).
        assert!(
            alias_is_active(&fx.state, &relay_email).await,
            "order A: re-consent must restore forwarding"
        );
        // Revoke is the LAST writer → terminal grant-absent.
        let status = test::call_service(
            &app,
            test::TestRequest::delete()
                .uri(&format!("/me/oauth-grants/{client_id}"))
                .header("authorization", caller.bearer())
                .to_request(),
        )
        .await
        .status();
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (grant_present, _revoked) =
            grant_and_alias_state(&fx.state, &client_id, caller.user_id).await;
        assert!(!grant_present, "order A terminal: grant absent");
        assert!(
            !alias_is_active(&fx.state, &relay_email).await,
            "order A: grant-absent ⇒ alias inert"
        );

        fx.state
            .control_pg
            .execute(
                "DELETE FROM zeroship.token_revocations WHERE client_id = $1",
                &[&client_id],
            )
            .await
            .ok();
        cleanup_identities(&fx.state, &client_id).await;
        fx.cleanup_clients(std::slice::from_ref(&client_id)).await;
        fx.state
            .control_pg
            .execute(
                "DELETE FROM zeroship.app_oauth_clients WHERE client_id = $1",
                &[&client_id],
            )
            .await
            .ok();
        fx.state
            .control_pg
            .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id])
            .await
            .ok();
    }

    // ── Commit order B (the DANGEROUS interleaving): revoke commits, then a
    // re-consent's alias UN-REVOKE lands AFTER it but the grant is NOT
    // re-inserted (the writer raced losing the grant DELETE). Terminal: grant
    // ABSENT but revoked_at NULL. The OLD alias-flag-only gate would FORWARD
    // here (the privacy failure); the structural gate keeps it INERT. ──
    {
        let client_id = format!("oac_race_b_{}", Uuid::new_v4().simple());
        let sector = format!("https://{}.zeroship.localhost", Uuid::new_v4().simple());
        insert_client(&fx.state, &client_id, caller.user_id).await;
        let app_id = insert_app_oauth_client(&fx.state, &client_id, &sector).await;
        insert_grant(&fx.state, caller.user_id, &client_id, &["email"]).await;
        let relay_email = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());
        insert_identity_with_alias(&fx.state, &client_id, &sector, caller.user_id, &relay_email).await;
        assert!(alias_is_active(&fx.state, &relay_email).await, "active before");

        // Revoke commits: grant DELETEd, revoked_at set.
        let status = test::call_service(
            &app,
            test::TestRequest::delete()
                .uri(&format!("/me/oauth-grants/{client_id}"))
                .header("authorization", caller.bearer())
                .to_request(),
        )
        .await
        .status();
        assert_eq!(status, StatusCode::NO_CONTENT);

        // A concurrent re-consent's ALIAS un-revoke lands AFTER the revoke but
        // its grant upsert lost the race (never ran / was DELETEd): we replay
        // ONLY the alias un-revoke (the real writer), leaving the grant absent.
        // This is precisely the interleaving where revoked_at returns to NULL
        // with NO live grant — the terminal state the structural gate must
        // neutralize.
        zeroship_auth::store::relay::mint_alias_at_consent(
            fx.state.control_pg.as_ref(),
            &client_id,
            caller.user_id,
            "relay.zeroship.localhost",
        )
        .await
        .expect("stray alias un-revoke after revoke");

        let (grant_present, revoked_set) =
            grant_and_alias_state(&fx.state, &client_id, caller.user_id).await;
        assert!(!grant_present, "order B terminal: grant ABSENT");
        assert!(
            !revoked_set,
            "order B precondition: the stray un-revoke cleared revoked_at (alias-flag says ACTIVE)"
        );
        // The whole point: alias-flag says active, but the structural gate sees
        // no grant ⇒ INERT. Pre-fix this asserted-true (live forwarding leak).
        assert!(
            !alias_is_active(&fx.state, &relay_email).await,
            "order B: grant-absent ⇒ alias inert EVEN WITH revoked_at NULL (structural gate)"
        );

        fx.state
            .control_pg
            .execute(
                "DELETE FROM zeroship.token_revocations WHERE client_id = $1",
                &[&client_id],
            )
            .await
            .ok();
        cleanup_identities(&fx.state, &client_id).await;
        fx.cleanup_clients(std::slice::from_ref(&client_id)).await;
        fx.state
            .control_pg
            .execute(
                "DELETE FROM zeroship.app_oauth_clients WHERE client_id = $1",
                &[&client_id],
            )
            .await
            .ok();
        fx.state
            .control_pg
            .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id])
            .await
            .ok();
    }

    caller.cleanup(&fx.state).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// Archive removes runtime routing but preserves OAuth identity and grant rows.
/// Restore therefore recovers the same origin and pairwise identity rather than
/// minting a second security domain for the same app.
#[compio::test]
async fn app_archive_preserves_relay_identities() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "appdel").await;
    // Two users with aliases on the SAME app (same client_id). Both also hold an
    // active grant — the structural revoke-coherence gate requires a live
    // `oauth_grants` row for the alias to forward, so "active before" only holds
    // with the grant present (the realistic deployed-app state).
    let user_a = insert_user(&fx.state, "appdel-a").await;
    let user_b = insert_user(&fx.state, "appdel-b").await;
    let client_id = format!("client-{}", Uuid::new_v4().simple());
    insert_client(&fx.state, &client_id, user_a).await;
    let sector = format!("https://{client_id}.zeroship.localhost");
    let app_uuid = insert_app_oauth_client(&fx.state, &client_id, &sector).await;
    insert_grant(&fx.state, user_a, &client_id, &["email"]).await;
    insert_grant(&fx.state, user_b, &client_id, &["email"]).await;
    let alias_a = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());
    let alias_b = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());
    insert_identity_with_alias(&fx.state, &client_id, &sector, user_a, &alias_a).await;
    insert_identity_with_alias(&fx.state, &client_id, &sector, user_b, &alias_b).await;

    assert!(alias_is_active(&fx.state, &alias_a).await);
    assert!(alias_is_active(&fx.state, &alias_b).await);

    fx.state
        .registry
        .archive_app(&app_uuid)
        .await
        .expect("archive app")
        .expect("app exists");

    assert!(
        alias_is_active(&fx.state, &alias_a).await,
        "archive retains user A's identity binding"
    );
    assert!(
        alias_is_active(&fx.state, &alias_b).await,
        "archive retains user B's identity binding"
    );
    assert_eq!(
        identity_row_count(&fx.state, &client_id).await,
        2,
        "archive preserves app_user_identities"
    );
    assert_eq!(count_grant(&fx.state, user_a, &client_id).await, 1);
    assert_eq!(count_grant(&fx.state, user_b, &client_id).await, 1);
    assert!(!fx.state.registry.get_routes().await.unwrap().contains_key(&app_uuid));

    cleanup_identities(&fx.state, &client_id).await;
    fx.cleanup_clients(std::slice::from_ref(&client_id)).await;
    fx.state
        .control_pg
        .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_uuid])
        .await
        .ok();
    cleanup_user(&fx.state, user_a).await;
    cleanup_user(&fx.state, user_b).await;

    drop(fx);
    common::drain_pg().await;
}

/// App archive happy path: the handler returns the retained app record with its
/// archive timestamp.
#[compio::test]
async fn app_archive_returns_200_with_retained_record() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "atomic-ok").await;

    // The caller OWNS the app: `create_app` binds the owner `app_members` row,
    // and that row is the only thing that authorizes `apps:archive` now. This
    // used to seed an unrelated owner and delete as a platform admin, which
    // worked only because of the deleted universal-allow policy.
    let caller = common::authz_fixture::seeded_principal(&fx.state).await;
    let app_name = format!("atomicok{}", Uuid::new_v4().simple());
    let record = fx
        .state
        .registry
        .create_app(
            &app_name,
            &zeroship_control::plan_catalog::free_plan_id(),
            &caller.user_id,
            None,
        )
        .await
        .expect("create app");
    let app_id = record.id;

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .service(
                web::resource("/api/apps/{id}/archive")
                    .route(web::put().to(api::archive_app)),
            ),
    )
    .await;

    let req = test::TestRequest::put()
        .uri(&format!("/api/apps/{app_id}/archive"))
        .header("authorization", caller.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "archive returns a clean 200"
    );
    let body: Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("archive body json");
    assert!(body.get("archived_at").and_then(Value::as_str).is_some());
    assert_eq!(body.get("id").and_then(Value::as_str), Some(app_id.to_string().as_str()));

    caller.cleanup(&fx.state).await;
    cleanup_user(&fx.state, caller.user_id).await;
    fx.state
        .control_pg
        .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id])
        .await
        .ok();

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn unauthenticated_request_returns_401() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "unauth").await;
    let app = init_control!(fx);

    let req = test::TestRequest::get().uri("/me/oauth-grants").to_request();
    // Status only: a retained `WebResponse` keeps the app state - and its
    // Postgres client - alive past the teardown below.
    let status = test::call_service(&app, req).await.status();

    assert_eq!(status, StatusCode::UNAUTHORIZED);

    drop(app);
    drop(fx);
    common::drain_pg().await;
}
