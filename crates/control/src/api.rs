//! Admin API handlers — app CRUD, deploy, plan, usage.

use std::path::Path as StdPath;
use std::sync::Arc;
use std::time::Duration;

use futures::Stream;
use ntex::http::StatusCode;
use ntex::web;
use ntex::web::types::{Json, Path, State};
use serde::Deserialize;
use uuid::Uuid;
use zeroship_authz::{Action, EntityCache, Resource};

use crate::app_oauth_client;
use crate::authz_guard::AuthzGuard;
use crate::deploy::{self, IngestError};
use crate::registry::RegistryError;
use crate::AppState;

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateAppBody {
    pub name: String,
    #[serde(default = "default_plan")]
    pub plan_id: String,
}

/// Default plan for a `create_app` with no explicit `plan_id`: the built-in
/// free tier's catalog id (`pln_…`). PR4 dropped the free-text `"free"` —
/// the plan must be a real catalog id so the FK + server-side gate accept it.
fn default_plan() -> String {
    crate::bootstrap_console::free_plan_id()
}

#[derive(Deserialize)]
pub struct SetPlanBody {
    pub plan_id: String,
}

/// Body for the operator credit-grant endpoint `POST /api/billing/credit`
/// (billing-ops gap #26, PR-2). The operator supplies the creator, a positive
/// amount, and an optional kind/expiry/note. Currency is USD-pinned (v1) — the
/// `credit::grant` boundary rejects any other. The idempotency key arrives in the
/// `Idempotency-Key` header (not the body) so a retried POST is a no-op.
#[derive(Deserialize)]
pub struct GrantCreditBody {
    /// The creator (a `users.id` UUID — the `creator_billing` key).
    pub creator_id: Uuid,
    /// Positive grant amount in cents.
    pub amount_cents: i64,
    /// Grant kind — one of `grant`/`promo`/`goodwill`. Defaults to `grant`.
    #[serde(default = "default_credit_kind")]
    pub kind: String,
    /// Currency (USD-pinned in v1). Defaults to `usd`.
    #[serde(default = "default_credit_currency")]
    pub currency: String,
    /// Optional expiry — a grant past this instant is not consumable. None ⇒ never.
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Optional operator audit note ('promo X', 'goodwill ticket #…').
    #[serde(default)]
    pub note: Option<String>,
}

fn default_credit_kind() -> String {
    "grant".to_string()
}

fn default_credit_currency() -> String {
    crate::credit::CREDIT_CURRENCY.to_string()
}

/// Body for the operator refund endpoint `POST /api/invoices/{id}/refunds`
/// (billing-ops gap #26, PR-3). The operator supplies the amount + destination; the
/// tax split is OPTIONAL — when omitted, the endpoint derives it proportionally from
/// the invoice's frozen `tax_cents`/`total_cents`. The idempotency key arrives in the
/// `Idempotency-Key` header (not the body) so a retried POST is a no-op.
#[derive(Deserialize)]
pub struct RefundBody {
    /// Positive amount to refund, in cents.
    pub amount_cents: i64,
    /// Where the refund goes: `cash` (a Stripe `Refund` re_… to the card) or
    /// `credit` (a platform-native `refund_to_credit` grant). Defaults to `credit`
    /// (DECISION 3 — keep money on-platform unless cash is explicitly requested).
    #[serde(default = "default_refund_destination")]
    pub destination: String,
    /// Optional explicit pre-tax portion. When omitted (with `tax_cents`), the
    /// endpoint derives a proportional split from the invoice's tax ratio.
    #[serde(default)]
    pub subtotal_cents: Option<i64>,
    /// Optional explicit tax portion. See `subtotal_cents`.
    #[serde(default)]
    pub tax_cents: Option<i64>,
    /// Optional operator audit reason.
    #[serde(default)]
    pub reason: Option<String>,
}

fn default_refund_destination() -> String {
    "credit".to_string()
}

// ---------------------------------------------------------------------------
// Error → HttpResponse
// ---------------------------------------------------------------------------

fn error_response(e: RegistryError) -> web::HttpResponse {
    match e {
        RegistryError::NotFound(msg) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({ "error": msg }))
        }
        RegistryError::AlreadyExists(msg) => {
            web::HttpResponse::Conflict().json(&serde_json::json!({ "error": msg }))
        }
        RegistryError::Conflict(msg) => {
            web::HttpResponse::Conflict().json(&serde_json::json!({ "error": msg }))
        }
        RegistryError::InvalidInput(msg) => {
            web::HttpResponse::BadRequest().json(&serde_json::json!({ "error": msg }))
        }
        RegistryError::Database(msg) => {
            infrastructure_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "registry database error",
                msg,
            )
        }
        RegistryError::FxUnresolved => infrastructure_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "pricing misconfigured",
            "global default FX missing".to_string(),
        ),
    }
}

fn infrastructure_error_response(
    status: StatusCode,
    context: &'static str,
    detail: impl std::fmt::Display,
) -> web::HttpResponse {
    let request_id = Uuid::new_v4();
    tracing::error!(
        request_id = %request_id,
        context,
        error = %detail,
        "control-plane infrastructure error"
    );
    web::HttpResponse::build(status).json(&serde_json::json!({"error": "internal error"}))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn create_app(
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
    body: Json<CreateAppBody>,
) -> web::HttpResponse {
    if let Err(resp) = authz.require(Action::AppsWrite, Resource::Any, &state).await {
        return resp;
    }
    match state
        .registry
        .create_app(&body.name, &body.plan_id, &authz.principal_id)
        .await
    {
        Ok(record) => {
            // The create bound the principal as the app's owner. Invalidate the
            // principal's entity-cache so the very next request (e.g. a deploy
            // of the app just created) sees the fresh owner membership instead
            // of a stale "no memberships" snapshot.
            EntityCache::invalidate(authz.principal_id);
            // Slice 1d (§1.1): provision the per-app public PKCE OAuth client
            // BEFORE the app is routable, so the route-sync push that makes the
            // host live already carries Some(oauth_client_id) — no cold-start
            // 503. Best-effort relative to the create response: a Hydra/DB
            // hiccup here is logged + metered, and the next deploy re-provisions
            // (ensure_app_client is idempotent). The app still exists.
            // No manifest exists at create, so no declared scopes yet — the
            // client gets the baseline allowlist; the first deploy mirrors the
            // manifest's `auth.scopes`.
            if let Err(e) = state
                .provision_app_oauth_client(&record.id, &record.name, &[])
                .await
            {
                tracing::error!(
                    app_id = %record.id,
                    app_name = %record.name,
                    error = %e,
                    "control: per-app OAuth client provisioning failed on create (will retry on deploy)"
                );
            }
            // Include api_key in the create response (it's skipped from normal serialization)
            let mut json = serde_json::to_value(&record).unwrap();
            json["api_key"] = serde_json::Value::String(record.api_key.clone());
            web::HttpResponse::Created().json(&json)
        }
        Err(e) => error_response(e),
    }
}

pub async fn list_apps(
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Err(resp) = authz.require(Action::AppsRead, Resource::Any, &state).await {
        return resp;
    }
    // The self-service policy grants every creator `apps:read` on the platform
    // surface, so the gate above passes for ordinary creators too. The DATA must
    // therefore be scoped to ownership: only platform staff with a fleet-wide
    // read role (admin/readonly/support/billing) see every app; everyone else
    // sees only the apps they are a member of. Without this scope the broadened
    // gate would be a fleet-wide cross-tenant read (the exact C1 leak, just at
    // the list endpoint).
    let result = match fleet_wide_reader(&state, authz.principal_id).await {
        Ok(true) => state.registry.list_apps().await,
        Ok(false) => state.registry.list_apps_for_owner(&authz.principal_id).await,
        Err(resp) => return resp,
    };
    match result {
        Ok(apps) => web::HttpResponse::Ok().json(&apps),
        Err(e) => error_response(e),
    }
}

/// Returns true when the principal holds a platform role that authorizes a
/// fleet-wide read (admin / readonly / support / billing). These are the roles
/// whose Cedar policy permits `apps:read` on an unconstrained resource
/// (`billing.cedar` grants it too, so `get_app` already lets billing staff read
/// any single app by id) — so they are the principals allowed to see every
/// tenant's apps in the list endpoint. This SQL role set MUST stay in sync with
/// the Cedar policies that grant unconstrained `apps:read`; omitting a role here
/// under-scopes its list relative to its actual authority (the 7.0 defect, where
/// `billing` was missing and billing staff got an empty `/api/apps`).
async fn fleet_wide_reader(
    state: &AppState,
    principal_id: Uuid,
) -> Result<bool, web::HttpResponse> {
    let rows = state
        .control_pg
        .query(
            "SELECT 1 FROM zeroship.platform_admin_roles \
             WHERE user_id = $1 AND role IN ('admin', 'readonly', 'support', 'billing')",
            &[&principal_id],
        )
        .await
        .map_err(|err| {
            infrastructure_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "platform role lookup",
                err,
            )
        })?;
    Ok(!rows.is_empty())
}

pub async fn get_app(
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    if let Err(resp) = authz
        .require(Action::AppsRead, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }
    match state.registry.get_app(&uid).await {
        Ok(Some(record)) => web::HttpResponse::Ok().json(&record),
        Ok(None) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Err(e) => error_response(e),
    }
}

/// Why a single `purge_app`-delete failed. Lets the HTTP handler map a faithful
/// status code while the cron reaper logs + isolates per-app.
///
/// `pub` (not `pub(crate)`): the orphaned-app reaper integration test drives the
/// REAL shared `purge_app` path (no shim), so the symbol must cross the crate
/// boundary into the test crate.
#[derive(Debug)]
pub enum PurgeError {
    /// Deleting the app's manifest keyspace failed. A `NotFound`/empty prefix
    /// is success inside `delete_app_manifests`, so this only surfaces real
    /// auth/config/transport failures that must block the DB cascade.
    Manifests(zeroship_bundle::BlobError),
    /// The atomic DB cascade delete failed.
    Registry(RegistryError),
}

/// Tear down an app and all of its side-effecting state, in the ONE canonical
/// order used by both the `delete_app` HTTP handler and the orphaned-app reaper:
///
///   1. Manifest-keyspace delete — `BlobStore::delete_app_manifests` removes
///      every `manifests/<app_id>/…` object (empty/absent prefix is success).
///      Content-addressed blobs under `blobs/` are shared and NOT deleted
///      here. This preserves today's "artifact purge first, DB cascade
///      second" ordering: a manifest-delete failure blocks the DB cascade.
///   2. `registry.delete_app` — the ATOMIC DB cascade (apps row + per-app
///      `oauth_clients` row in one txn; the real FK chain tears down every
///      dependent row in the `zeroship` schema). Returns `false` if the row was
///      already gone.
///   3. Per-app Hydra OAuth client delete — best-effort + idempotent (Hydra is a
///      separate source of truth, not reachable by a DB FK). Ordered AFTER the
///      DB delete so a Hydra outage can never strand a live app with no client;
///      a failure here is logged, not fatal (the route is already dead).
///
/// Returns `Ok(true)` if a DB row was deleted, `Ok(false)` if it was already
/// gone. There is ONE deletion path; two callers (the `delete_app` HTTP handler
/// and the `orphaned_app_reaper` cron).
pub async fn purge_app(state: &AppState, app_id: &Uuid) -> Result<bool, PurgeError> {
    // 1. Delete the app's manifest keyspace first. `delete_app_manifests`
    //    swallows empty/absent prefixes (the app may never have deployed) and
    //    only returns an error on real auth/config/transport failures, which
    //    must block the DB cascade so we never orphan live artifacts.
    state
        .blob_store
        .delete_app_manifests(app_id)
        .await
        .map_err(PurgeError::Manifests)?;

    // 2. Atomic DB cascade.
    let deleted = state
        .registry
        .delete_app(app_id)
        .await
        .map_err(PurgeError::Registry)?;

    if deleted {
        // 3. Per-app Hydra OAuth client delete — best-effort + idempotent.
        if let Err(e) = state.delete_app_oauth_client(app_id).await {
            tracing::error!(
                app_id = %app_id,
                error = %e,
                "control: per-app OAuth client delete failed on app purge (Hydra client leaked — GC later)"
            );
        }
    }
    Ok(deleted)
}

pub async fn delete_app(
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    if let Err(resp) = authz
        .require(Action::AppsDelete, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }
    match purge_app(&state, &uid).await {
        Ok(true) => web::HttpResponse::Ok().json(&serde_json::json!({"deleted": true})),
        Ok(false) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Err(PurgeError::Manifests(e)) => infrastructure_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "delete app manifests",
            e,
        ),
        Err(PurgeError::Registry(e)) => error_response(e),
    }
}

/// Streaming `.zship` ingest. Replaces the legacy raw-bundle path —
/// deploy bundles now arrive as zstd-compressed tar archives carrying
/// `manifest.json` + `blobs/<sha256>` entries. See
/// `docs/reference/zship.md` for the wire format and ingestion
/// algorithm.
pub async fn deploy(
    req: web::HttpRequest,
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
    mut body: web::types::Payload,
) -> web::HttpResponse {
    // Authz + uuid + content-type rejections happen BEFORE any body byte
    // is consumed, so rejected callers cannot tie up tmp file slots.
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    if let Err(resp) = authz
        .require(Action::AppsDeploy, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }

    // Hard cut: only `application/x-zship` is accepted. The legacy
    // raw `.appbundle` and `application/javascript` paths are gone.
    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !is_zship_content_type(content_type) {
        return web::HttpResponse::UnsupportedMediaType().json(&serde_json::json!({
            "error": "unsupported content type",
            "detail": "expected application/x-zship",
        }));
    }

    // Stream the request body to a tmp file under the configured
    // deploy tmp dir. Tmp files live for the duration of the deploy
    // and are removed after ingest (success or error). Path includes
    // a uuid so concurrent deploys don't trample each other. The
    // helper itself enforces `MAX_COMPRESSED_BYTES` while writing —
    // see `stream_body_to_tmp_file`. ntex's `Payload` implements
    // `Stream<Item = Result<Bytes, PayloadError>>` directly, so the
    // generic helper accepts it without an adapter.
    let tmp_path = state
        .deploy_tmp_dir
        .join(format!("zeroship-deploy-{}.zship", uuid::Uuid::new_v4().simple()));

    match stream_body_to_tmp_file(
        &mut body,
        &tmp_path,
        zeroship_bundle::MAX_COMPRESSED_BYTES as u64,
    )
    .await
    {
        Ok(_written) => { /* fall through to mmap + ingest */ }
        Err(StreamToTmpError::TooLarge { cap, observed }) => {
            return web::HttpResponse::PayloadTooLarge().json(&serde_json::json!({
                "error": "deploy too large",
                "cap_bytes": cap,
                "observed_bytes": observed,
            }));
        }
        Err(StreamToTmpError::PayloadError(detail)) => {
            return web::HttpResponse::BadRequest().json(&serde_json::json!({
                "error": "payload error",
                "detail": detail,
            }));
        }
        Err(e) => {
            return infrastructure_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "deploy stream to tmp failed",
                format_args!("{e}; path={}", tmp_path.display()),
            );
        }
    }

    // mmap + ingest. The std::fs::File::open is sync but cheap (no I/O
    // beyond opening a fd); Mmap::map sets up VM mappings without
    // reading bytes. tar/zstd then page-fault through the slice, which
    // the kernel services from page cache.
    let file = match std::fs::File::open(&tmp_path) {
        Ok(f) => f,
        Err(e) => {
            let _ = compio::fs::remove_file(&tmp_path).await;
            return infrastructure_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "deploy tmp re-open failed",
                format_args!("{e}; path={}", tmp_path.display()),
            );
        }
    };
    // SAFETY: tmp file is owned by this handler, written exclusively by
    // us (create_new), fsynced before mapping, and not modified by any
    // other process for the lifetime of `mmap`.
    #[allow(unsafe_code)]
    let mmap = match unsafe { memmap2::Mmap::map(&file) } {
        Ok(m) => m,
        Err(e) => {
            drop(file);
            let _ = compio::fs::remove_file(&tmp_path).await;
            return infrastructure_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "deploy mmap failed",
                format_args!("{e}; path={}", tmp_path.display()),
            );
        }
    };

    let result = deploy::ingest(&state.blob_store, &uid, &mmap[..]).await;

    // Drop mmap + file before unlinking. On Linux unlink-while-mapped
    // is fine, but explicit drop avoids edge cases on other platforms.
    drop(mmap);
    drop(file);
    let _ = compio::fs::remove_file(&tmp_path).await;

    match result {
        Ok(success) => {
            // Slice 1d (§1.1): re-provision the per-app OAuth client BEFORE the
            // manifest commit so the route-sync invariant holds without a
            // cold-start window. The manifest commit (below) is what makes the
            // app's host resolvable to the gateway's 5s route-sync pull; doing
            // the OAuth provisioning first means that by the time the route is
            // published, the client (and its control.app_oauth_clients row that
            // get_routes LEFT-JOINs into oauth_client_id) already exists — so a
            // route-sync pull can never observe a live route with
            // oauth_client_id=None. Reconcile is diff-then-PUT (a no-op deploy
            // makes no Hydra call) and idempotent. Best-effort relative to the
            // deploy response: a Hydra hiccup is logged and the next deploy
            // re-provisions; the deploy 200 does NOT imply provisioning
            // succeeded (the SDK relies on retryable-503 client_not_provisioned
            // handling for that rare window).
            // Slice 3 (§5.1/§5.2): re-parse the ingested manifest to extract its
            // declared `auth.scopes`. `ingest` only enforces scope-id FORMAT
            // (ScopeDef::validate_id_format inside Manifest::validate), NOT the
            // platform-vocabulary collision rule — so the deploy handler MUST run
            // the full `validate_app_scopes` guard here and HARD-FAIL the deploy
            // before anything is provisioned or committed. A re-parse failure is a
            // control-side programming error (ingest already parsed+validated the
            // same bytes), but we still abort the deploy rather than silently drop
            // declared scopes (which would wipe the app_scope_defs registry on the
            // next provision).
            let declared_scopes =
                match serde_json::from_str::<zeroship_bundle::Manifest>(&success.manifest_json) {
                    Ok(m) => m.auth.scopes,
                    Err(e) => {
                        tracing::error!(
                            app_id = %uid,
                            error = %e,
                            "control: could not re-parse ingested manifest for declared scopes"
                        );
                        return web::HttpResponse::InternalServerError().json(&serde_json::json!({
                            "error": "manifest reparse failed",
                            "detail": e.to_string(),
                        }));
                    }
                };

            // Reject a colliding/reserved/malformed declared scope (e.g.
            // `billing:read`) with a 400 BEFORE the manifest is committed or the
            // route published — the creator gets a real error instead of a
            // silently un-provisioned scope set.
            if let Err(e) = app_oauth_client::validate_app_scopes(&declared_scopes) {
                let (id, reason) = match &e {
                    app_oauth_client::AppOauthClientError::InvalidScope { id, reason } => {
                        (id.clone(), reason.clone())
                    }
                    other => (String::new(), other.to_string()),
                };
                return web::HttpResponse::BadRequest().json(&serde_json::json!({
                    "error": "invalid_scope",
                    "scope": id,
                    "detail": reason,
                }));
            }

            // Slice 1d (§1.1): re-provision the per-app OAuth client BEFORE the
            // manifest commit so the route-sync invariant holds. Scopes are
            // already validated above, so `ensure_app_client`'s internal
            // `validate_app_scopes` cannot reject; any error here is a Hydra/DB
            // hiccup and stays best-effort relative to the deploy response.
            match state.registry.get_app(&uid).await {
                Ok(Some(app)) => {
                    if let Err(e) = state
                        .provision_app_oauth_client(&uid, &app.name, &declared_scopes)
                        .await
                    {
                        tracing::error!(
                            app_id = %uid,
                            error = %e,
                            "control: per-app OAuth client reconcile failed on deploy"
                        );
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::error!(
                    app_id = %uid,
                    error = %e,
                    "control: deploy could not load app for OAuth client reconcile"
                ),
            }

            // P6 (schema-authority §8): apply the bundle's DB migrations BEFORE
            // the go-live commit. The `.zship` carries its versioned migration
            // files (manifest.migrations → content-addressed blobs); we
            // reconstruct them on disk and hand them to `zeroship-migrate`
            // (Confined, schema "<app_id>", the per-app migrator role). A
            // migrate FAILURE returns here and DOES NOT commit go-live — the old
            // bundle keeps serving its already-migrated schema (§8.3). The app
            // identity is the trusted path id `uid`, never a request body.
            //
            // Half-state contract (§8.4): migrate commits its journal, then
            // go-live commits. If migrate succeeds but the go-live UPDATE below
            // fails, the schema is ahead of the live code (additive-forward =
            // safe); the next deploy's roll-forward reconciles. No verify gate.
            if let Err(resp) = run_deploy_migrations(&uid, &success.manifest_json, &state).await {
                return resp;
            }

            // Atomic UPDATE: deploy_hash + manifest_json land together
            // so the gateway never sees half-applied state. Committed AFTER
            // OAuth provisioning + the migrate phase so the route only becomes
            // resolvable once the client exists AND the schema is applied.
            match state
                .registry
                .set_deploy_with_manifest(&uid, &success.deploy_hash, &success.manifest_json)
                .await
            {
                Ok(true) => web::HttpResponse::Ok().json(&serde_json::json!({
                    "deploy_hash": success.deploy_hash,
                    "blobs_uploaded": success.blobs_uploaded,
                    "blobs_deduped": success.blobs_deduped,
                })),
                Ok(false) => web::HttpResponse::NotFound()
                    .json(&serde_json::json!({"error": "app not found"})),
                Err(e) => error_response(e),
            }
        }
        Err(e) => ingest_error_to_response(e),
    }
}

/// P6 migrate phase: reconstruct the bundle's migration files from the blob
/// store and apply them via `zeroship-migrate` (Confined, schema `"<app_id>"`)
/// BEFORE the go-live commit (schema-authority §8).
///
/// Returns `Ok(())` on success (incl. the no-migrations no-op), or
/// `Err(HttpResponse)` the deploy handler returns verbatim — in which case the
/// caller MUST NOT commit go-live. A migrate failure is a 422 (the creator's
/// migrations are at fault: a denied/destructive/unparseable migration) or a
/// 503 (infra: connect / provision); both keep the old bundle serving.
async fn run_deploy_migrations(
    app_id: &Uuid,
    manifest_json: &str,
    state: &AppState,
) -> Result<(), web::HttpResponse> {
    // Re-parse the (already-validated) ingested manifest for its migrations.
    let manifest: zeroship_bundle::Manifest = match serde_json::from_str(manifest_json) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(app_id = %app_id, error = %e, "control: deploy-migrate manifest reparse failed");
            return Err(web::HttpResponse::InternalServerError().json(&serde_json::json!({
                "error": "manifest reparse failed",
                "detail": e.to_string(),
            })));
        }
    };

    // No migrations ⇒ nothing to do (the app ships no schema). Skip even the
    // tmp-dir + admin connection.
    if manifest.migrations.is_empty() {
        return Ok(());
    }

    // Reconstruct the migration files under a per-deploy tmp dir. `validate()`
    // already enforced bare-filename safety (no separators / traversal), so the
    // join stays confined to `mig_dir`. The dir is removed on every exit path.
    let mig_dir = state
        .deploy_tmp_dir
        .join(format!("zeroship-migrations-{}", Uuid::new_v4().simple()));
    if let Err(e) = compio::fs::create_dir_all(&mig_dir).await {
        return Err(infrastructure_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "deploy-migrate tmp dir",
            format_args!("{e}; path={}", mig_dir.display()),
        ));
    }

    // The privileged provisioning DSN (CREATEROLE + CREATE on db) — distinct
    // from control's least-privilege `zeroship_control` DSN, which CANNOT
    // `CREATE SCHEMA` / `CREATE ROLE`. Absent ⇒ deploy-migrate infrastructure
    // is unconfigured; fail BEFORE touching the DB rather than 503 mid-CREATE.
    let Some(provision_dsn) = state.registry.migrate_dsn() else {
        let _ = std::fs::remove_dir_all(&mig_dir);
        return Err(infrastructure_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "migration_infrastructure",
            "no provisioning DSN configured (set --provision-db / PROVISION_DATABASE_URL \
             to a CREATEROLE admin role)",
        ));
    };

    let write_result = reconstruct_migration_files(&manifest, app_id, state, &mig_dir).await;
    let outcome = match write_result {
        Ok(()) => {
            crate::deploy_migrate::apply_bundle_migrations(provision_dsn, app_id, &mig_dir).await
        }
        Err(resp) => {
            let _ = std::fs::remove_dir_all(&mig_dir);
            return Err(resp);
        }
    };
    let _ = std::fs::remove_dir_all(&mig_dir);

    match outcome {
        Ok(report) => {
            tracing::info!(
                app_id = %app_id,
                applied = report.applied.len(),
                skipped = report.skipped.len(),
                "control: deploy-migrate applied"
            );
            Ok(())
        }
        Err(e) => {
            use crate::deploy_migrate::DeployMigrateError as DME;
            // A creator-fault migration (bad grammar / denied / destructive /
            // checksum drift) is a 422 the creator can act on; an infra fault
            // (connect / provision) is a 503. Either way: NO go-live.
            let (status, kind) = match &e {
                // Creator-fault: bad `.sql` grammar / denied / destructive / drift,
                // OR a `.ir.json` the fail-closed gate refused (malformed / future
                // ir_version / structural reject / ownership / checksum) or that
                // could not lower. The creator can act on all of these → 422.
                DME::Load(_) | DME::Apply(_) | DME::Ir { .. } => {
                    (StatusCode::UNPROCESSABLE_ENTITY, "migration_failed")
                }
                // Infra-fault: connect / provision / live-introspection / IR file
                // read — not the creator's migration content → 503.
                DME::Connect(_)
                | DME::ProvisionSchema(_)
                | DME::ProvisionRole(_)
                | DME::Snapshot(_)
                | DME::IrRead { .. } => {
                    (StatusCode::SERVICE_UNAVAILABLE, "migration_infrastructure")
                }
            };
            tracing::error!(app_id = %app_id, error = %e, "control: deploy-migrate failed; NOT committing go-live");
            Err(web::HttpResponse::build(status).json(&serde_json::json!({
                "error": kind,
                "detail": e.to_string(),
            })))
        }
    }
}

/// Stream each carried migration blob out of the blob store and write it to
/// `mig_dir/<name>`. Returns `Err(HttpResponse)` (a 503/500) if a referenced
/// blob is missing or a write fails — both abort the deploy before go-live.
async fn reconstruct_migration_files(
    manifest: &zeroship_bundle::Manifest,
    app_id: &Uuid,
    state: &AppState,
    mig_dir: &StdPath,
) -> Result<(), web::HttpResponse> {
    for entry in &manifest.migrations {
        let bytes = match state.blob_store.get_blob(&entry.hash).await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(
                    app_id = %app_id, name = %entry.name, hash = %entry.hash, error = %e,
                    "control: deploy-migrate could not read migration blob"
                );
                return Err(web::HttpResponse::ServiceUnavailable().json(&serde_json::json!({
                    "error": "migration_blob_unavailable",
                    "detail": format!("migration {} (blob {}): {e}", entry.name, entry.hash),
                })));
            }
        };
        let path = mig_dir.join(&entry.name);
        // Defense-in-depth against a duplicate `entry.name` slipping past
        // Manifest::validate: `create_new` (O_EXCL) refuses to truncate an
        // existing file, so a name collision fails the deploy loudly instead
        // of silently clobbering an already-written migration's content.
        use compio::io::AsyncWriteAtExt;
        let mut file = match compio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                return Err(infrastructure_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "deploy-migrate open migration file",
                    format_args!("{e}; path={}", path.display()),
                ));
            }
        };
        if let Err(e) = file.write_all_at(bytes.to_vec(), 0).await.0 {
            return Err(infrastructure_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "deploy-migrate write migration file",
                format_args!("{e}; path={}", path.display()),
            ));
        }
    }
    Ok(())
}

/// Permissive content-type check. We accept the canonical
/// `application/x-zship` plus parameterised variants like
/// `application/x-zship; charset=utf-8` (some clients add charset
/// even on binary uploads).
fn is_zship_content_type(value: &str) -> bool {
    let primary = value.split(';').next().unwrap_or("").trim();
    primary.eq_ignore_ascii_case("application/x-zship")
}

/// Map the structured ingest error to an HTTP response.
fn ingest_error_to_response(e: IngestError) -> web::HttpResponse {
    match e {
        IngestError::BadRequest { error, detail } => web::HttpResponse::BadRequest()
            .json(&serde_json::json!({"error": error, "detail": detail})),
        IngestError::TooLarge { cap_bytes, observed_bytes } => {
            web::HttpResponse::PayloadTooLarge().json(&serde_json::json!({
                "error": "deploy too large",
                "cap_bytes": cap_bytes,
                "observed_bytes": observed_bytes,
            }))
        }
        IngestError::UnsupportedMediaType => web::HttpResponse::UnsupportedMediaType()
            .json(&serde_json::json!({
                "error": "unsupported content type",
                "detail": "expected application/x-zship",
            })),
        IngestError::BlobStoreUnavailable(detail) => {
            infrastructure_error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "deploy blob store unavailable",
                detail,
            )
        }
        IngestError::Internal(detail) => {
            infrastructure_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "deploy ingest internal error",
                detail,
            )
        }
    }
}

pub async fn set_plan(
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
    body: Json<SetPlanBody>,
) -> web::HttpResponse {
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": "invalid uuid"}))
        }
    };

    // MAJOR-4: two authority levels for assigning a plan.
    //   * OPERATOR — BillingWrite on Resource::Any (master-key / operator) — may
    //     assign ANY plan (including operator-only tiers).
    //   * app_owner / CREATOR — BillingWrite on Resource::App{id} — may assign
    //     ONLY a plan flagged `assignable_by_creator = true`. Without this gate a
    //     creator could PUT a cheaper operator plan (e.g. unlimited/console) and
    //     underpay — the asymmetry the reduction-only spend-limit override
    //     already closes for caps.
    // We probe the operator grant first; if it is denied we fall back to the
    // app-scoped grant AND enforce the creator-assignability guardrail.
    let is_operator = authz
        .require(Action::BillingWrite, Resource::Any, &state)
        .await
        .is_ok();
    if !is_operator {
        if let Err(resp) = authz
            .require(Action::BillingWrite, Resource::App { id: uid.to_string() }, &state)
            .await
        {
            return resp;
        }
        // app_owner principal: the target plan MUST be creator-assignable.
        let catalog = crate::plan_catalog::PlanCatalog::new(state.registry.clone());
        match catalog.get(&body.plan_id).await {
            Ok(Some(plan)) if plan.assignable_by_creator => { /* allowed */ }
            Ok(Some(_)) => {
                return web::HttpResponse::Forbidden().json(&serde_json::json!({
                    "error": "plan not assignable by creator",
                    "detail": "this plan can only be assigned by an operator; choose a \
                               creator-assignable plan or contact support to upgrade",
                }));
            }
            Ok(None) => {
                return web::HttpResponse::BadRequest()
                    .json(&serde_json::json!({"error": "unknown plan"}));
            }
            Err(e) => return error_response(e),
        }
    }

    // PR-4 (full usage-segment proration): record a plan-change-events row with
    // A cumulative usage_at_change snapshot IN THE SAME TXN as the apps.plan_id
    // flip, under the per-creator advisory lock. The target plan must be a real,
    // non-archived plan; validate it via the catalog (segment pricing reads the
    // live catalog at reconcile time — the row freezes NO base fee, round 4
    // CRITICAL-1).
    let catalog = crate::plan_catalog::PlanCatalog::new(state.registry.clone());
    match catalog.get(&body.plan_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": "unknown plan"}))
        }
        Err(e) => return error_response(e),
    }

    // Resolve the app's current plan (the from-plan) and its owning creator (for
    // the advisory lock + period attribution). An app with NO owner row (e.g. the
    // system console) has no billable creator — flip the plan without recording a
    // proration timeline (there is no creator to bill).
    let conn = match state.registry.conn().await {
        Ok(c) => c,
        Err(e) => return error_response(RegistryError::Database(e.to_string())),
    };
    let from_plan_id: Option<String> = match conn
        .query("SELECT plan_id FROM zeroship.apps WHERE id = $1", &[&uid])
        .await
    {
        Ok(rows) => rows.first().map(|r| r.get::<_, String>("plan_id")),
        Err(e) => return error_response(RegistryError::Database(e.to_string())),
    };
    if from_plan_id.is_none() {
        // The app row does not exist at all.
        return web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}));
    }
    let owner: Option<Uuid> = match conn
        .query(
            "SELECT user_id FROM zeroship.app_members WHERE app_id = $1 AND role = 'owner' LIMIT 1",
            &[&uid],
        )
        .await
    {
        Ok(rows) => rows.first().map(|r| r.get::<_, Uuid>("user_id")),
        Err(e) => return error_response(RegistryError::Database(e.to_string())),
    };
    drop(conn);

    let Some(creator_id) = owner else {
        // No billable creator (system app): plain flip, no proration timeline.
        return match state.registry.set_plan(&uid, &body.plan_id).await {
            Ok(true) => web::HttpResponse::Ok().json(&serde_json::json!({"updated": true})),
            Ok(false) => {
                web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
            }
            Err(e) => error_response(e),
        };
    };

    // The shared server-side write path (advisory lock + server-derived usage
    // snapshot + plan flip + cap + finalized-period attribution) in ONE txn.
    match crate::proration::record_plan_change_tx(
        &state.registry,
        &uid,
        &creator_id,
        from_plan_id.as_deref(),
        &body.plan_id,
        chrono::Utc::now().timestamp(),
    )
    .await
    {
        Ok(crate::proration::PlanChangeOutcome::AppNotFound) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Ok(_) => web::HttpResponse::Ok().json(&serde_json::json!({"updated": true})),
        Err(e) => error_response(e),
    }
}

// ---------------------------------------------------------------------------
// Spend-limit override (billing PR5, M4) — the creator-facing cap.
//
// `PUT /api/apps/:id/spend-limit` body `{ "cents": <u64|null> }` sets (or, with
// null, clears back to the plan default) the per-app spend-limit override.
// `GET` returns the effective limit + current state. Authz is BillingWrite /
// BillingRead on `Resource::App(id)` — the same app-membership gate `set_plan`
// uses.
//
// REDUCTION-ONLY by design (#9): the override is bounded above by the plan's
// `spend_limit_default_cents`, so a creator can only LOWER their effective cap,
// never raise it above what the plan already grants. This is intentional and
// safe — there is NO privilege-escalation path: raising your effective headroom
// means UPGRADING the plan (an operator/billing-gated action), not editing this
// override. We therefore do NOT model a separate `spend_limit_max_cents`
// column; the plan default IS the ceiling. A request above it is rejected 403.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SetSpendLimitBody {
    /// New override in cents, or `null` to clear back to the plan default.
    pub cents: Option<u64>,
}

pub async fn set_spend_limit(
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
    body: Json<SetSpendLimitBody>,
) -> web::HttpResponse {
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": "invalid uuid"}))
        }
    };
    if let Err(resp) = authz
        .require(Action::BillingWrite, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }

    // Resolve the app's plan default so an override can't exceed it.
    let plan_default = match resolve_plan_default_cents(&state, &uid).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            return web::HttpResponse::NotFound()
                .json(&serde_json::json!({"error": "app not found"}))
        }
        Err(e) => return error_response(e),
    };
    if let Some(req_cents) = body.cents {
        if req_cents > plan_default {
            return web::HttpResponse::Forbidden().json(&serde_json::json!({
                "error": "spend limit exceeds plan maximum",
                "plan_max_cents": plan_default,
            }));
        }
    }

    let engine = crate::spend::SpendEngine::new(state.registry.clone());
    match engine.set_limit(&uid, body.cents).await {
        Ok(()) => {
            crate::audit::log_with_detail(
                &state.registry,
                crate::audit::AuditEntry {
                    app_id: Some(uid),
                    creator_id: None,
                    // #7 — populate the actor from the AuthzGuard so a
                    // billing-write audit row records WHO changed the cap.
                    actor_user_id: Some(authz.principal_id),
                    actor_token_id: authz.token_id,
                    action: crate::audit::Action::SetSpendLimit,
                    resource: Some("spend_limit"),
                    source_ip: None,
                },
                // Log the resolved `plan_default` bound alongside the requested
                // cents so the audit row shows the reduction-only ceiling the
                // override was checked against (#7).
                &serde_json::json!({ "cents": body.cents, "plan_default_cents": plan_default }),
            )
            .await;
            web::HttpResponse::Ok().json(&serde_json::json!({"updated": true}))
        }
        Err(e) => error_response(e),
    }
}

pub async fn get_spend_limit(
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": "invalid uuid"}))
        }
    };
    if let Err(resp) = authz
        .require(Action::BillingRead, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }
    let conn = match state.registry.conn().await {
        Ok(c) => c,
        Err(e) => return error_response(e),
    };
    let rows = match conn
        .query(
            "SELECT a.plan_id, l.spend_limit_cents, s.state \
             FROM zeroship.apps a \
             LEFT JOIN zeroship.app_spend_limit l ON l.app_id = a.id \
             LEFT JOIN zeroship.app_spend_state s ON s.app_id = a.id \
             WHERE a.id = $1",
            &[&uid],
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return error_response(RegistryError::Database(e.to_string())),
    };
    let Some(row) = rows.first() else {
        return web::HttpResponse::NotFound()
            .json(&serde_json::json!({"error": "app not found"}));
    };
    let plan_id: String = row.get("plan_id");
    let override_cents: Option<i64> = row.get("spend_limit_cents");
    let state_str: Option<String> = row.get("state");
    let catalog = crate::plan_catalog::PlanCatalog::new(state.registry.clone());
    let plan_default = match catalog.get(&plan_id).await {
        Ok(Some(p)) => p.price.spend_limit_default_cents,
        Ok(None) => 0,
        Err(e) => return error_response(e),
    };
    let effective = override_cents
        .and_then(|o| u64::try_from(o).ok())
        .unwrap_or(plan_default);
    web::HttpResponse::Ok().json(&serde_json::json!({
        "effective_limit_cents": effective,
        "override_cents": override_cents,
        "plan_default_cents": plan_default,
        "state": state_str.as_deref().unwrap_or("allow"),
    }))
}

// ---------------------------------------------------------------------------
// Creator billing READ APIs (billing-ops gap #26, PR-7 — `BillingRead`).
//
// Six creator-scoped read endpoints. The APP-scoped reads (invoice history,
// projected-charge, billing-status) gate `require(BillingRead, Resource::App{id})`
// — the SAME membership gate `get_spend_limit` uses, so a creator sees only
// OWNED apps and an operator (`Resource::Any`) sees any. The CREATOR-keyed reads
// (credit-balance, payment-method) gate `can_act_anywhere(BillingRead)` (the
// caller must be a billing-capable creator) and force the target creator to
// `self` UNLESS the caller is an operator, who may target any via `?creator_id`.
//
// SECURITY: no raw Stripe ids, no other creator's data, no app-token escalation.
// Every response is a clean DTO from `crate::billing_read` (no internal columns).
// ---------------------------------------------------------------------------

/// Pagination query for `GET /api/apps/{id}/invoices`. Defaults: 50 newest,
/// offset 0. `limit` is clamped to `[1, 200]` to bound a single read.
#[derive(Deserialize)]
pub struct InvoiceListQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// Optional `?creator_id=` for the creator-keyed reads. Honoured ONLY for an
/// operator (`Resource::Any`); a non-operator caller is always forced to self.
#[derive(Deserialize)]
pub struct CreatorScopeQuery {
    #[serde(default)]
    pub creator_id: Option<Uuid>,
}

/// Resolve the OWNING creator (`app_members.role='owner'`) for an app, or `None`
/// when the app has no owner row (a system app) / does not exist.
async fn owner_of_app(state: &AppState, app_id: &Uuid) -> Result<Option<Uuid>, RegistryError> {
    let conn = state.registry.conn().await?;
    let rows = conn
        .query(
            "SELECT user_id FROM zeroship.app_members WHERE app_id = $1 AND role = 'owner' LIMIT 1",
            &[app_id],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    Ok(rows.first().map(|r| r.get::<_, Uuid>("user_id")))
}

/// `GET /api/apps/{id}/invoices` — invoice history for the OWNER of app `{id}`,
/// newest-first, paginated.
///
/// AUTHZ GRAIN — OWNER-LEVEL (SEC). The response is the OWNER's entire cross-app
/// invoice history (an invoice is creator-keyed), so a non-owner app member
/// (editor/viewer with `billing:read` on this one app) must NOT see the owner's
/// whole billing envelope. We gate `BillingRead on App{id}` (capability +
/// existence), then require the caller to BE the owner of `{id}`
/// (`owner_of_app(id) == principal_id`) OR an operator (`Resource::Any`).
pub async fn list_app_invoices(
    id: Path<String>,
    query: web::types::Query<InvoiceListQuery>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": "invalid uuid"}))
        }
    };
    if let Err(resp) = authz
        .require(Action::BillingRead, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }
    let creator_id = match owner_of_app(&state, &uid).await {
        Ok(Some(c)) => c,
        Ok(None) => return web::HttpResponse::Ok().json(&serde_json::json!({ "invoices": [] })),
        Err(e) => return error_response(e),
    };
    // OWNER-or-operator: only the app's OWNER (the invoice creator) reads the
    // owner's cross-app invoice history; a non-owner member does not.
    if creator_id != authz.principal_id {
        match authz.is_operator(Action::BillingRead, &state).await {
            Ok(true) => {}
            Ok(false) => {
                return web::HttpResponse::Forbidden()
                    .json(&serde_json::json!({"error": "forbidden"}))
            }
            Err(resp) => return resp,
        }
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let offset = query.offset.unwrap_or(0).max(0);
    match crate::billing_read::list_invoices_for_creator(&state.registry, &creator_id, limit, offset)
        .await
    {
        Ok(invoices) => web::HttpResponse::Ok().json(&serde_json::json!({ "invoices": invoices })),
        Err(e) => error_response(e),
    }
}

/// `GET /api/invoices/{id}` — frozen-snapshot line detail. The `{id}` is the
/// internal `inv_…` id.
///
/// AUTHZ GRAIN — CREATOR-LEVEL (SEC, CRITICAL-1). An invoice is creator-keyed:
/// the reconciler stamps `invoice.creator_id` as the `role='owner'` user
/// (`cron/billing_reconcile.rs`). The invoice envelope spans EVERY app that
/// creator owns, so the caller must BE that creator OR an operator
/// (`Resource::Any`). We do NOT loop the creator's apps and accept any
/// `BillingRead` grant: `list_apps_for_owner` is role-AGNOSTIC, so a creator who
/// is merely a viewer/editor on the attacker's app would appear in the list and
/// let the attacker read the victim's whole invoice. A single
/// `creator_id == principal_id || is_operator` check closes that cross-creator
/// hole AND removes the per-app Cedar-loop audit amplification.
pub async fn get_invoice(
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let invoice_id = id.into_inner();
    let detail = match crate::billing_read::get_invoice_detail(&state.registry, &invoice_id).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            // Gate billing capability before 404 so the endpoint never leaks
            // invoice existence to a non-billing token.
            match authz.can_act_anywhere(Action::BillingRead, &state).await {
                Ok(true) => {}
                Ok(false) => {
                    return web::HttpResponse::Forbidden()
                        .json(&serde_json::json!({"error": "forbidden"}))
                }
                Err(resp) => return resp,
            }
            return web::HttpResponse::NotFound()
                .json(&serde_json::json!({"error": "invoice not found"}));
        }
        Err(e) => return error_response(e),
    };

    // The caller is the invoice's creator, OR an operator. Nothing else reads it.
    if detail.creator_id != authz.principal_id {
        match authz.is_operator(Action::BillingRead, &state).await {
            Ok(true) => {}
            Ok(false) => {
                return web::HttpResponse::Forbidden()
                    .json(&serde_json::json!({"error": "forbidden"}))
            }
            Err(resp) => return resp,
        }
    }
    web::HttpResponse::Ok().json(&detail)
}

/// `GET /api/apps/{id}/projected-charge` — current-period projected charge over
/// LIVE aggregates, labelled NON-AUTHORITATIVE (MAJOR-5). Cached 60s so polling
/// cannot hammer a full pricing pass. Authz: `BillingRead` on `Resource::App{id}`.
pub async fn get_projected_charge(
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": "invalid uuid"}))
        }
    };
    if let Err(resp) = authz
        .require(Action::BillingRead, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }
    let now = chrono::Utc::now().timestamp();
    match crate::billing_read::projected_charge(
        &state.registry,
        &state.projected_charge_cache,
        &uid,
        now,
    )
    .await
    {
        Ok(Some(p)) => web::HttpResponse::Ok().json(&p),
        Ok(None) => web::HttpResponse::NotFound().json(&serde_json::json!({"error": "app not found"})),
        Err(e) => error_response(e),
    }
}

/// `GET /api/billing/credit-balance` — the caller's USD credit balance + recent
/// ledger. Creator-keyed: gated `can_act_anywhere(BillingRead)`; the target is
/// `self` unless the caller is an operator passing `?creator_id=`.
pub async fn get_credit_balance(
    query: web::types::Query<CreatorScopeQuery>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let target = match resolve_creator_target(&authz, &state, query.creator_id).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    match crate::billing_read::credit_balance(&state.registry, &target, 50).await {
        Ok(balance) => web::HttpResponse::Ok().json(&balance),
        Err(e) => error_response(e),
    }
}

/// `GET /api/billing/payment-method` — the caller's PM status (presence only,
/// never the raw provider id). Creator-keyed, scoped exactly like credit-balance.
pub async fn get_payment_method(
    query: web::types::Query<CreatorScopeQuery>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let target = match resolve_creator_target(&authz, &state, query.creator_id).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    match crate::billing_read::payment_method_status(&state.registry, &target).await {
        Ok(status) => web::HttpResponse::Ok().json(&status),
        Err(e) => error_response(e),
    }
}

/// `GET /api/apps/{id}/billing-status` — plan + spend cap + spend/account state.
/// Authz: `BillingRead` on `Resource::App{id}`.
pub async fn get_billing_status(
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": "invalid uuid"}))
        }
    };
    if let Err(resp) = authz
        .require(Action::BillingRead, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }
    match crate::billing_read::billing_status(&state.registry, &uid).await {
        Ok(Some(status)) => web::HttpResponse::Ok().json(&status),
        Ok(None) => web::HttpResponse::NotFound().json(&serde_json::json!({"error": "app not found"})),
        Err(e) => error_response(e),
    }
}

/// Resolve the target creator for a creator-keyed billing read.
///
/// - **Operator** (`BillingRead` on `Resource::Any`): may target any creator via
///   the optional `creator_id`; absent it, targets self.
/// - **Creator** (billing-capable on at least one owned app): forced to `self`.
///   A `creator_id` naming ANOTHER creator is 403; a caller with NO billing
///   capability anywhere is 403.
async fn resolve_creator_target(
    authz: &AuthzGuard,
    state: &AppState,
    requested: Option<Uuid>,
) -> Result<Uuid, web::HttpResponse> {
    let is_op = authz.is_operator(Action::BillingRead, state).await?;
    if is_op {
        return Ok(requested.unwrap_or(authz.principal_id));
    }
    if !authz.can_act_anywhere(Action::BillingRead, state).await? {
        return Err(web::HttpResponse::Forbidden()
            .json(&serde_json::json!({"error": "forbidden"})));
    }
    if let Some(req) = requested {
        if req != authz.principal_id {
            return Err(web::HttpResponse::Forbidden().json(&serde_json::json!({
                "error": "forbidden",
                "detail": "only an operator may read another creator's billing",
            })));
        }
    }
    Ok(authz.principal_id)
}

/// Resolve an app's plan-default spend limit. `Ok(None)` when the app row is
/// missing. Used to bound a creator override.
async fn resolve_plan_default_cents(
    state: &AppState,
    app_id: &Uuid,
) -> Result<Option<u64>, RegistryError> {
    let conn = state.registry.conn().await?;
    let rows = conn
        .query("SELECT plan_id FROM zeroship.apps WHERE id = $1", &[app_id])
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let plan_id: String = row.get("plan_id");
    let catalog = crate::plan_catalog::PlanCatalog::new(state.registry.clone());
    Ok(catalog
        .get(&plan_id)
        .await?
        .map(|p| p.price.spend_limit_default_cents))
}

// ---------------------------------------------------------------------------
// Plan catalog (billing PR4) — operator-editable, server-side pricing catalog.
//
// Reads (`GET /api/plans`, `GET /api/plans/:id`) require BillingRead on
// `Resource::Any` (a fleet-wide read — the catalog is global operator config,
// not tenant data). Writes (`PUT`/`DELETE`) require BillingWrite on
// `Resource::Any` (operator / master-key authority). DELETE archives (soft
// delete) so existing `apps.plan_id` FKs + historical billing runs stay
// resolvable — there is no hard DELETE.
// ---------------------------------------------------------------------------

/// JSON shape for a plan in the catalog API. `price`/`runtime` serialize the
/// pure types verbatim (the same JSON the DB JSONB columns hold).
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PlanDto {
    pub id: String,
    pub name: String,
    pub price: crate::pricing::PlanPrice,
    pub runtime: zeroship_core::types::AppRuntimeLimits,
    #[serde(default)]
    pub archived: bool,
    /// MAJOR-4: whether a creator (app_owner) may self-assign this plan. Surfaced
    /// in the read so operators can see/audit which tiers are creator-assignable.
    #[serde(default)]
    pub assignable_by_creator: bool,
}

impl From<crate::plan_catalog::Plan> for PlanDto {
    fn from(p: crate::plan_catalog::Plan) -> Self {
        Self {
            id: p.id,
            name: p.name,
            price: p.price,
            runtime: p.runtime,
            archived: p.archived,
            assignable_by_creator: p.assignable_by_creator,
        }
    }
}

/// Body for `PUT /api/plans/:id`. `id` comes from the path; the body carries
/// the editable fields. A new id mints a row; an existing id updates it.
///
/// `archived` is OPTIONAL: omitting it preserves the existing row's archived
/// flag (a name/price edit must not silently un-archive a plan). Send
/// `"archived": false` explicitly to un-archive, `true` to archive.
#[derive(Deserialize)]
pub struct UpsertPlanBody {
    pub name: String,
    pub price: crate::pricing::PlanPrice,
    pub runtime: zeroship_core::types::AppRuntimeLimits,
    #[serde(default)]
    pub archived: Option<bool>,
    /// MAJOR-4: operator-controlled flag — may a creator self-assign this plan?
    /// Defaults to `false` (fail-closed: an operator-minted plan is NOT
    /// creator-assignable unless explicitly opted in).
    #[serde(default)]
    pub assignable_by_creator: bool,
}

pub async fn list_plans(
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Err(resp) = authz.require(Action::BillingRead, Resource::Any, &state).await {
        return resp;
    }
    let catalog = crate::plan_catalog::PlanCatalog::new(state.registry.clone());
    match catalog.list().await {
        Ok(plans) => {
            let dtos: Vec<PlanDto> = plans.into_iter().map(PlanDto::from).collect();
            web::HttpResponse::Ok().json(&dtos)
        }
        Err(e) => error_response(e),
    }
}

pub async fn get_plan(
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Err(resp) = authz.require(Action::BillingRead, Resource::Any, &state).await {
        return resp;
    }
    let catalog = crate::plan_catalog::PlanCatalog::new(state.registry.clone());
    match catalog.get(&id).await {
        Ok(Some(plan)) => web::HttpResponse::Ok().json(&PlanDto::from(plan)),
        Ok(None) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"plan not found"}))
        }
        Err(e) => error_response(e),
    }
}

pub async fn upsert_plan(
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
    body: Json<UpsertPlanBody>,
) -> web::HttpResponse {
    if let Err(resp) = authz.require(Action::BillingWrite, Resource::Any, &state).await {
        return resp;
    }
    // The plan id MUST be a well-formed `pln_<base62>` typed id so the catalog
    // namespace can't be polluted with free-text ids (the CT-A1 class).
    let id = id.into_inner();
    if zeroship_core::typed_id::parse_with_prefix(&id, zeroship_core::typed_id::PLAN_PREFIX)
        .is_err()
    {
        return web::HttpResponse::BadRequest()
            .json(&serde_json::json!({"error":"plan id must be a pln_<base62> typed id"}));
    }
    let body = body.into_inner();
    // Semantic validation at the write boundary: a malformed price model
    // (e.g. non-monotonic tier boundaries) is a 400, not a silently-wrong
    // charge later (#13/#4).
    if let Err(msg) = body.price.validate() {
        return web::HttpResponse::BadRequest()
            .json(&serde_json::json!({"error": format!("invalid price model: {msg}")}));
    }
    let archived = body.archived;
    let plan = crate::plan_catalog::Plan {
        id,
        name: body.name,
        price: body.price,
        runtime: body.runtime,
        // Placeholder — the upsert uses the `archived` arg, not this field;
        // `None` ⇒ preserve existing (a PUT without `archived` can't un-archive).
        archived: archived.unwrap_or(false),
        assignable_by_creator: body.assignable_by_creator,
    };
    let catalog = crate::plan_catalog::PlanCatalog::new(state.registry.clone());
    match catalog.upsert(&plan, archived).await {
        Ok(written) => {
            // MINOR-1: a plan's FX/price is the highest-leverage money lever —
            // audit WHO wrote it + the new price model (mirrors SetSpendLimit's
            // actor logging), so an unexpected price change is attributable.
            crate::audit::log_with_detail(
                &state.registry,
                crate::audit::AuditEntry {
                    app_id: None,
                    creator_id: None,
                    actor_user_id: Some(authz.principal_id),
                    actor_token_id: authz.token_id,
                    action: crate::audit::Action::PlanUpserted,
                    resource: Some(&written.id),
                    source_ip: None,
                },
                &serde_json::json!({
                    "plan_id": written.id,
                    "name": written.name,
                    "base_fee_cents": written.price.base_fee_cents,
                    "included_units": written.price.included_units,
                    "fx_pico_cents_per_unit": written.price.fx_pico_cents_per_unit,
                    "spend_limit_default_cents": written.price.spend_limit_default_cents,
                    "archived": written.archived,
                }),
            )
            .await;
            web::HttpResponse::Ok().json(&PlanDto::from(written))
        }
        Err(e) => error_response(e),
    }
}

pub async fn archive_plan(
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Err(resp) = authz.require(Action::BillingWrite, Resource::Any, &state).await {
        return resp;
    }
    let id = id.into_inner();
    let catalog = crate::plan_catalog::PlanCatalog::new(state.registry.clone());
    match catalog.archive(&id).await {
        Ok(true) => {
            // MINOR-1: archiving removes a tier from new-app assignment — a money
            // lever change. Audit WHO archived which plan (mirrors SetSpendLimit).
            crate::audit::log_with_detail(
                &state.registry,
                crate::audit::AuditEntry {
                    app_id: None,
                    creator_id: None,
                    actor_user_id: Some(authz.principal_id),
                    actor_token_id: authz.token_id,
                    action: crate::audit::Action::PlanArchived,
                    resource: Some(&id),
                    source_ip: None,
                },
                &serde_json::json!({ "plan_id": id, "archived": true }),
            )
            .await;
            web::HttpResponse::Ok().json(&serde_json::json!({"archived": true}))
        }
        Ok(false) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"plan not found"}))
        }
        Err(e) => error_response(e),
    }
}

// ---------------------------------------------------------------------------
// Global pricing config (gap #28) — the operator-editable GLOBAL default FX.
//
// The global default FX (`pricing_config.id='global'.fx_pico_cents_per_unit`) is
// the price a CU sells for when a plan does NOT override it (`plans.fx == NULL`).
// Per-plan FX is already operator-editable via `PUT /api/plans/:id`; this is the
// missing runtime lever for the GLOBAL default — previously seed/DB-only, so an
// operator had to ship a migration to reprice globally.
//
// Both routes are OPERATOR-ONLY: `BillingRead`/`BillingWrite` on `Resource::Any`
// — the SAME fleet-wide gate the plan-catalog + fee-policy writes use. A creator
// (app-scoped `Resource::App{id}` grant) is NOT reachable here and gets a 403.
//
// The write enforces the near-zero FX floor (`>= MIN_FX_PICO_CENTS_PER_UNIT`)
// with a clean 400 BEFORE touching the DB (fail closed — never rely solely on
// the DB CHECK), and AUDITS the actor + old→new value (it reprices everyone).
//
// Reproducibility: changing the global default FX affects FUTURE pricing only.
// Finalized invoices snapshot their effective FX onto each line at finalize time,
// so a re-priced default never rewrites a settled invoice (no invoices are
// finalized in this config-write flow — no code needed here, only the guarantee).
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SetPricingConfigBody {
    /// New global default FX in pico-cents per CU. Must be
    /// `>= MIN_FX_PICO_CENTS_PER_UNIT`.
    pub fx_pico_cents_per_unit: u64,
}

pub async fn get_pricing_config(
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Err(resp) = authz.require(Action::BillingRead, Resource::Any, &state).await {
        return resp;
    }
    let store = crate::pricing_store::PricingStore::new(state.registry.clone());
    match store.default_fx_pico_cents_per_unit().await {
        // `None` here means the singleton is MISSING or stored below the floor —
        // a platform misconfiguration the reader logs loudly. Surface it as a
        // pricing-misconfigured 500 (the same fail-closed posture the sweeps take)
        // rather than fabricating a default the operator never set.
        Ok(Some(fx)) => web::HttpResponse::Ok()
            .json(&serde_json::json!({ "fx_pico_cents_per_unit": fx })),
        Ok(None) => infrastructure_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "pricing misconfigured",
            "global default FX missing or below floor",
        ),
        Err(e) => error_response(e),
    }
}

pub async fn set_pricing_config(
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
    body: Json<SetPricingConfigBody>,
) -> web::HttpResponse {
    // OPERATOR-ONLY — the SAME `Resource::Any` gate the plan/fee-policy writes
    // use. A creator's app-scoped `BillingWrite` is denied (403).
    if let Err(resp) = authz.require(Action::BillingWrite, Resource::Any, &state).await {
        return resp;
    }
    // Enforce the near-zero FX floor at the boundary (fail closed) — a clean 400
    // rather than relying on the DB CHECK to surface as an opaque 500.
    if body.fx_pico_cents_per_unit < crate::pricing::MIN_FX_PICO_CENTS_PER_UNIT {
        return web::HttpResponse::BadRequest().json(&serde_json::json!({
            "error": "fx below floor",
            "detail": format!(
                "fx_pico_cents_per_unit must be >= {} (a near-zero global FX prices all overage \
                 to ~$0; raise a plan's included_units to make a tier free instead)",
                crate::pricing::MIN_FX_PICO_CENTS_PER_UNIT
            ),
            "floor_pico_cents_per_unit": crate::pricing::MIN_FX_PICO_CENTS_PER_UNIT,
        }));
    }

    let store = crate::pricing_store::PricingStore::new(state.registry.clone());
    match store.set_default_fx(body.fx_pico_cents_per_unit).await {
        Ok(old_fx) => {
            // Audit WHO repriced the global default + the old→new transition. The
            // global FX is the highest-leverage money lever (it reprices every
            // inheriting plan), so an unexpected change must be attributable.
            crate::audit::log_with_detail(
                &state.registry,
                crate::audit::AuditEntry {
                    app_id: None,
                    creator_id: None,
                    actor_user_id: Some(authz.principal_id),
                    actor_token_id: authz.token_id,
                    action: crate::audit::Action::SetGlobalFx,
                    resource: Some("pricing_config"),
                    source_ip: None,
                },
                &serde_json::json!({
                    "old_fx_pico_cents_per_unit": old_fx,
                    "new_fx_pico_cents_per_unit": body.fx_pico_cents_per_unit,
                }),
            )
            .await;
            web::HttpResponse::Ok().json(&serde_json::json!({
                "fx_pico_cents_per_unit": body.fx_pico_cents_per_unit,
            }))
        }
        // `error_response` maps the store's below-floor/overflow rejection
        // (`RegistryError::InvalidInput`) to a 400 — defense in depth, the handler
        // already rejected below-floor above; any other error maps as usual.
        Err(e) => error_response(e),
    }
}

/// Operator credit-grant endpoint `POST /api/billing/credit` (billing-ops gap #26,
/// PR-2). OPERATOR-ONLY: `Action::BillingWrite` on `Resource::Any` (master-key /
/// operator). A creator/app token — which can at most hold `BillingWrite` on
/// `Resource::App{id}` — is 403 here (credit is a fleet-wide money lever, never
/// self-grantable). Requires an `Idempotency-Key` header; a reused key with the
/// SAME body returns the first grant (safe retry), a reused key with a DIFFERENT
/// body is 409 (no silent second grant). Currency is USD-pinned (v1).
pub async fn grant_credit(
    req: web::HttpRequest,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
    body: Json<GrantCreditBody>,
) -> web::HttpResponse {
    // OPERATOR-ONLY on Resource::Any. No App-scoped fallback: a creator may never
    // grant themselves credit.
    if let Err(resp) = authz.require(Action::BillingWrite, Resource::Any, &state).await {
        return resp;
    }

    // The idempotency key is a required header (the body carries the grant facts).
    let idem_key = req
        .headers()
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_default();
    if idem_key.is_empty() {
        return web::HttpResponse::BadRequest().json(&serde_json::json!({
            "error": "missing Idempotency-Key header",
            "detail": "POST /api/billing/credit requires an Idempotency-Key header so a \
                       retried grant is a no-op (no double grant)",
        }));
    }

    let body = body.into_inner();
    // The creator must have a `creator_billing` row (the FK target). Ensure it
    // exists — the same lazy create the Stripe-store / account-status paths use —
    // so an operator can grant credit before the creator's first invoice. A missing
    // `users` row surfaces as a clean FK error → 400, not a 500.
    //
    // MINOR-4: the lazy `creator_billing` upsert and the `credit_ledger` grant
    // INSERT run in ONE `conn.transaction()` so the endpoint's atomicity matches
    // its prose. (Grant idempotency already covers a retry; the txn makes the
    // upsert+grant a single unit so a half-applied grant can never be observed.)
    let mut conn = match state.registry.conn().await {
        Ok(c) => c,
        Err(e) => return error_response(RegistryError::Database(e.to_string())),
    };
    let tx = match conn.transaction().await {
        Ok(t) => t,
        Err(e) => return error_response(RegistryError::Database(e.to_string())),
    };
    if let Err(e) = tx
        .execute(
            "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1) \
             ON CONFLICT (creator_id) DO NOTHING",
            &[&body.creator_id],
        )
        .await
    {
        // MINOR-5: classify by SQLSTATE. Only a foreign_key_violation (23503) —
        // a non-existent `users` row — is a genuine "unknown creator" 400. Any
        // OTHER error (a transient DB failure, etc.) must NOT be mis-labelled a
        // 400; it falls through to the standard `error_response` → 500.
        if e.code() == Some(&compio_postgres::error::SqlState::FOREIGN_KEY_VIOLATION) {
            return web::HttpResponse::BadRequest().json(&serde_json::json!({
                "error": "unknown creator",
                "detail": format!("no billable creator for creator_id {}", body.creator_id),
            }));
        }
        return error_response(RegistryError::Database(e.to_string()));
    }

    let outcome = crate::credit::grant(
        &tx,
        &body.creator_id,
        body.amount_cents,
        &body.currency,
        &body.kind,
        body.expires_at,
        body.note.as_deref(),
        &idem_key,
    )
    .await;

    // On a grant error, roll back (drop the tx) and surface it — never commit a
    // half-applied unit. On success, commit the upsert+grant together; a commit
    // failure is a 500 (the grant did not durably land).
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => return error_response(e),
    };
    if let Err(e) = tx.commit().await {
        return error_response(RegistryError::Database(e.to_string()));
    }

    match outcome {
        crate::credit::GrantOutcome::Created(id) => {
            crate::audit::log_with_detail(
                &state.registry,
                crate::audit::AuditEntry {
                    app_id: None,
                    creator_id: Some(body.creator_id),
                    actor_user_id: Some(authz.principal_id),
                    actor_token_id: authz.token_id,
                    action: crate::audit::Action::CreditGranted,
                    resource: Some(&id),
                    source_ip: None,
                },
                &serde_json::json!({
                    "credit_id": id,
                    "creator_id": body.creator_id,
                    "amount_cents": body.amount_cents,
                    "kind": body.kind,
                    "currency": body.currency,
                    "expires_at": body.expires_at,
                }),
            )
            .await;
            web::HttpResponse::Created().json(&serde_json::json!({
                "credit_id": id,
                "created": true,
            }))
        }
        // Same key + same body — return the first grant (safe retry, no second grant).
        crate::credit::GrantOutcome::Duplicate(id) => {
            web::HttpResponse::Ok().json(&serde_json::json!({
                "credit_id": id,
                "created": false,
            }))
        }
        // Same key + DIFFERENT body — reject (mirrors Stripe's idempotency-conflict).
        crate::credit::GrantOutcome::Conflict => web::HttpResponse::Conflict()
            .json(&serde_json::json!({
                "error": "idempotency-key-reuse-conflict",
                "detail": "the Idempotency-Key was reused with a different request body; \
                           a credit grant key is bound to its exact (creator, amount, \
                           currency, kind, expiry, note) — no second grant was created",
            })),
    }
}

/// Operator refund endpoint `POST /api/invoices/{id}/refunds` (billing-ops gap #26,
/// PR-3). OPERATOR-ONLY: `Action::BillingWrite` on `Resource::Any`. A creator/app
/// token (which can at most hold `BillingWrite` on `Resource::App{id}`) is 403 — a
/// refund moves real money / grants credit and is never self-serve in v1 (DECISION 5).
/// Requires an `Idempotency-Key` header; a reused key with the SAME body returns the
/// first refund (safe retry), a reused key with a DIFFERENT body is 409. The `{id}` is
/// the internal `inv_…` invoice id.
pub async fn refund_invoice(
    req: web::HttpRequest,
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
    body: Json<RefundBody>,
) -> web::HttpResponse {
    // OPERATOR-ONLY on Resource::Any. No App-scoped fallback.
    if let Err(resp) = authz.require(Action::BillingWrite, Resource::Any, &state).await {
        return resp;
    }
    let invoice_id = id.into_inner();
    let body = body.into_inner();

    let idem_key = req
        .headers()
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_default();
    if idem_key.is_empty() {
        return web::HttpResponse::BadRequest().json(&serde_json::json!({
            "error": "missing Idempotency-Key header",
            "detail": "POST /api/invoices/{id}/refunds requires an Idempotency-Key header so a \
                       retried refund is a no-op (no double refund)",
        }));
    }

    let Some(destination) = crate::refund::RefundDestination::parse(&body.destination) else {
        return web::HttpResponse::BadRequest().json(&serde_json::json!({
            "error": "invalid destination",
            "detail": "refund destination must be 'cash' or 'credit'",
        }));
    };

    let mut conn = match state.registry.conn().await {
        Ok(c) => c,
        Err(e) => return error_response(RegistryError::Database(e.to_string())),
    };

    // Resolve the tax split. If the operator supplied both subtotal+tax, use them
    // verbatim (the helper validates the split). Otherwise derive a PROPORTIONAL tax
    // split from the invoice's frozen tax ratio (MISSING-6 — a refund of a taxed
    // invoice returns proportional tax): tax = round_half_up(amount × inv.tax / inv.total).
    let (subtotal_cents, tax_cents) = match (body.subtotal_cents, body.tax_cents) {
        (Some(s), Some(t)) => (s, t),
        _ => {
            let inv = match conn
                .query(
                    "SELECT tax_cents, total_cents FROM zeroship.invoices WHERE id = $1",
                    &[&invoice_id],
                )
                .await
            {
                Ok(r) => r,
                Err(e) => return error_response(RegistryError::Database(e.to_string())),
            };
            let Some(row) = inv.first() else {
                return web::HttpResponse::NotFound().json(&serde_json::json!({
                    "error": "no such invoice",
                    "detail": format!("no invoice {invoice_id}"),
                }));
            };
            let inv_tax: i64 = row.get("tax_cents");
            let inv_total: i64 = row.get("total_cents");
            // round_half_up(amount × inv_tax / inv_total); 0 when the invoice is untaxed.
            let tax = if inv_total > 0 && inv_tax > 0 {
                let num = i128::from(body.amount_cents) * i128::from(inv_tax);
                let half = i128::from(inv_total) / 2;
                i64::try_from((num + half) / i128::from(inv_total)).unwrap_or(0)
            } else {
                0
            };
            (body.amount_cents - tax, tax)
        }
    };

    // Build the Stripe-backed refund provider (the cash leg). The credit leg never
    // touches it.
    let stripe = crate::stripe_client::StripeClient::new(crate::SecretString::new(
        state.stripe_secret_key.expose_secret().to_string(),
    ))
    .with_base_url(state.stripe_base_url.clone());
    let provider = crate::refund::StripeRefundProvider { stripe: &stripe };

    let outcome = crate::refund::issue_refund(
        &mut conn,
        &provider,
        &invoice_id,
        body.amount_cents,
        subtotal_cents,
        tax_cents,
        destination,
        body.reason.as_deref(),
        &idem_key,
    )
    .await;
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => return error_response(e),
    };

    use crate::refund::RefundOutcome;
    match outcome {
        RefundOutcome::Issued { refund_id, provider_ref } => {
            crate::audit::log_with_detail(
                &state.registry,
                crate::audit::AuditEntry {
                    app_id: None,
                    creator_id: None,
                    actor_user_id: Some(authz.principal_id),
                    actor_token_id: authz.token_id,
                    action: crate::audit::Action::InvoiceRefunded,
                    resource: Some(&refund_id),
                    source_ip: None,
                },
                &serde_json::json!({
                    "refund_id": refund_id,
                    "invoice_id": invoice_id,
                    "amount_cents": body.amount_cents,
                    "subtotal_cents": subtotal_cents,
                    "tax_cents": tax_cents,
                    "destination": destination.as_str(),
                    "provider_ref": provider_ref,
                }),
            )
            .await;
            web::HttpResponse::Created().json(&serde_json::json!({
                "refund_id": refund_id,
                "destination": destination.as_str(),
                "provider_ref": provider_ref,
                "created": true,
            }))
        }
        RefundOutcome::Duplicate(refund_id) => web::HttpResponse::Ok().json(&serde_json::json!({
            "refund_id": refund_id,
            "created": false,
        })),
        RefundOutcome::Conflict => web::HttpResponse::Conflict().json(&serde_json::json!({
            "error": "idempotency-key-reuse-conflict",
            "detail": "the Idempotency-Key was reused with a different request body; a refund \
                       key is bound to its exact (invoice, amount, split, destination) — no \
                       second refund was created",
        })),
        RefundOutcome::OverRefund(detail) => {
            web::HttpResponse::UnprocessableEntity().json(&serde_json::json!({
                "error": "over-refund",
                "detail": detail,
            }))
        }
        RefundOutcome::InvalidInvoice(detail) => {
            web::HttpResponse::BadRequest().json(&serde_json::json!({
                "error": "invalid invoice",
                "detail": detail,
            }))
        }
    }
}

/// Operator void+reissue endpoint `POST /api/invoices/{id}/void` (billing-ops gap #26,
/// PR-3). OPERATOR-ONLY: `Action::BillingWrite` on `Resource::Any`. Voids a finalized
/// invoice (the only legal `finalized→void` transition), restores any credit it
/// consumed (`void_reversal`), reissues a corrected invoice for the same period, and
/// auto-refunds any over-collection (the true-up bridge) — all under the per-creator
/// advisory lock. The `{id}` is the internal `inv_…` invoice id.
pub async fn void_invoice(
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Err(resp) = authz.require(Action::BillingWrite, Resource::Any, &state).await {
        return resp;
    }
    let invoice_id = id.into_inner();

    let stripe = crate::stripe_client::StripeClient::new(crate::SecretString::new(
        state.stripe_secret_key.expose_secret().to_string(),
    ))
    .with_base_url(state.stripe_base_url.clone());

    match crate::void_reissue::void_and_reissue(&state, &stripe, &invoice_id).await {
        Ok(outcome) => {
            crate::audit::log_with_detail(
                &state.registry,
                crate::audit::AuditEntry {
                    app_id: None,
                    creator_id: None,
                    actor_user_id: Some(authz.principal_id),
                    actor_token_id: authz.token_id,
                    action: crate::audit::Action::InvoiceVoided,
                    resource: Some(&outcome.voided_invoice_id),
                    source_ip: None,
                },
                &serde_json::json!({
                    "voided_invoice_id": outcome.voided_invoice_id,
                    "reissued_invoice_id": outcome.reissued_invoice_id,
                    "true_up_refund_id": outcome.true_up_refund_id,
                    "true_up_cents": outcome.true_up_cents,
                }),
            )
            .await;
            web::HttpResponse::Ok().json(&serde_json::json!({
                "voided_invoice_id": outcome.voided_invoice_id,
                "reissued_invoice_id": outcome.reissued_invoice_id,
                "true_up_refund_id": outcome.true_up_refund_id,
                "true_up_cents": outcome.true_up_cents,
            }))
        }
        Err(e) => error_response(e),
    }
}

pub async fn get_usage(
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    if let Err(resp) = authz
        .require(Action::BillingRead, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }
    // Usage now comes from the period-aggregated `usage_aggregates` table
    // (the metering pipeline), scoped to the current calendar-month period.
    // Returns the same `metric → total` map shape the dashboard consumes.
    let metering = crate::metering::Metering::new(state.registry.clone());
    match metering.current_period_totals(&uid).await {
        Ok(usage) => web::HttpResponse::Ok().json(&usage),
        Err(e) => error_response(e),
    }
}

pub async fn get_app_logs(
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    if let Err(resp) = authz
        .require(Action::DeploymentsRead, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }

    let mut lines = Vec::new();
    let mut errors = Vec::new();
    for worker_url in &state.worker_urls {
        match fetch_worker_logs(worker_url, state.worker_key.expose_secret(), &uid).await {
            Ok(mut worker_lines) => lines.append(&mut worker_lines),
            Err(e) => {
                tracing::warn!(
                    worker_url = %worker_url,
                    app_id = %uid,
                    error = %e,
                    "control: worker log fetch failed",
                );
                errors.push(format!("{worker_url}: {e}"));
            }
        }
    }

    if lines.is_empty() && !errors.is_empty() && errors.len() == state.worker_urls.len() {
        return infrastructure_error_response(
            StatusCode::BAD_GATEWAY,
            "worker logs unavailable",
            errors.join(" | "),
        );
    }

    web::HttpResponse::Ok().json(&lines)
}

async fn fetch_worker_logs(
    worker_url: &str,
    worker_key: &str,
    app_id: &Uuid,
) -> Result<Vec<String>, String> {
    let url = format!("{}/logs/{app_id}", worker_url.trim_end_matches('/'));
    let client = cyper::Client::new();
    let mut builder = client
        .get(&url)
        .map_err(|e| format!("invalid worker URL: {e}"))?;
    if !worker_key.is_empty() {
        builder = builder
            .header("authorization", &format!("Bearer {worker_key}"))
            .map_err(|e| format!("invalid auth header: {e}"))?;
    }

    let response = compio::time::timeout(Duration::from_secs(2), builder.send())
        .await
        .map_err(|_| "request timeout".to_string())?
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("read body: {e}"))?;
    if !status.is_success() {
        let body = String::from_utf8_lossy(&bytes);
        return Err(format!(
            "HTTP {} {}: {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
            body,
        ));
    }

    serde_json::from_slice::<Vec<String>>(&bytes)
        .map_err(|e| format!("parse logs JSON: {e}"))
}

// ---------------------------------------------------------------------------
// Streaming helper — used by `deploy()` to land the request body in a
// tmp file before mmap+ingest. Generic over the stream type so the
// helper is unit-testable with `futures::stream::iter`; production
// callers pass in `web::types::Payload` (which is `Stream<Item =
// Result<Bytes, PayloadError>>`).
// ---------------------------------------------------------------------------

/// Errors from `stream_body_to_tmp_file`. Maps cleanly onto HTTP
/// status codes — see `deploy()` for the response shape.
#[derive(Debug)]
pub(crate) enum StreamToTmpError {
    /// Couldn't open the tmp file for writing. Caller should return 500.
    OpenFailed(String),
    /// Body exceeded `max_bytes`. Tmp file has been removed.
    /// Caller should return 413.
    TooLarge { cap: u64, observed: u64 },
    /// Underlying payload error (client disconnected, decoding error,
    /// etc.). Tmp file has been removed. Caller should return 400.
    PayloadError(String),
    /// Disk write or sync failed. Tmp file has been removed (best
    /// effort). Caller should return 500.
    WriteFailed(String),
}

impl std::fmt::Display for StreamToTmpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenFailed(s) => write!(f, "tmp file open failed: {s}"),
            Self::TooLarge { cap, observed } => {
                write!(f, "body too large: {observed} > {cap}")
            }
            Self::PayloadError(s) => write!(f, "payload error: {s}"),
            Self::WriteFailed(s) => write!(f, "tmp write failed: {s}"),
        }
    }
}

/// Stream a body to a tmp file, enforcing `max_bytes` while writing.
/// On any error (incl. cap exceeded), the partial tmp file is removed.
/// On success, the file is fsynced and the total byte count returned.
///
/// Generic over the chunk type (`B: AsRef<[u8]>`) so this compiles
/// against both `ntex::util::Bytes` (production: `web::types::Payload`
/// yields ntex's bytes type) and stock `bytes::Bytes` (used by tests
/// constructing `futures::stream::iter`).
pub(crate) async fn stream_body_to_tmp_file<S, B, E>(
    stream: &mut S,
    tmp_path: &StdPath,
    max_bytes: u64,
) -> Result<u64, StreamToTmpError>
where
    S: Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    use compio::io::AsyncWriteAtExt;
    use futures::StreamExt;

    let file = compio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp_path)
        .await
        .map_err(|e| StreamToTmpError::OpenFailed(e.to_string()))?;

    let mut written: u64 = 0;
    while let Some(item) = stream.next().await {
        let chunk = match item {
            Ok(c) => c,
            Err(e) => {
                drop(file);
                let _ = compio::fs::remove_file(tmp_path).await;
                return Err(StreamToTmpError::PayloadError(e.to_string()));
            }
        };
        let chunk_slice: &[u8] = chunk.as_ref();
        let chunk_len = chunk_slice.len() as u64;
        let new_total = written + chunk_len;
        if new_total > max_bytes {
            drop(file);
            let _ = compio::fs::remove_file(tmp_path).await;
            return Err(StreamToTmpError::TooLarge {
                cap: max_bytes,
                observed: new_total,
            });
        }
        // compio File::write_all_at takes ownership of the buffer.
        // The chunk is a refcounted slice; copy into an owned Vec so
        // we can hand it to write_all_at. The to_vec() costs a single
        // chunk-sized alloc per chunk (typically 16-256 KiB).
        let owned: Vec<u8> = chunk_slice.to_vec();
        let compio::BufResult(res, _returned) =
            (&file).write_all_at(owned, written).await;
        if let Err(e) = res {
            drop(file);
            let _ = compio::fs::remove_file(tmp_path).await;
            return Err(StreamToTmpError::WriteFailed(format!(
                "write at offset {written}: {e}"
            )));
        }
        written = new_total;
    }
    if let Err(e) = file.sync_all().await {
        drop(file);
        let _ = compio::fs::remove_file(tmp_path).await;
        return Err(StreamToTmpError::WriteFailed(format!("sync_all: {e}")));
    }
    drop(file);
    Ok(written)
}

#[cfg(test)]
mod error_response_tests {
    use super::*;
    use ntex::http::StatusCode;
    use ntex::util::{stream_recv, BytesMut};

    async fn body_json(mut resp: web::HttpResponse) -> serde_json::Value {
        let mut body = resp.take_body();
        let mut buf = BytesMut::new();
        while let Some(item) = stream_recv(&mut body).await {
            buf.extend_from_slice(&item.expect("body chunk"));
        }
        serde_json::from_slice(&buf).expect("body is JSON")
    }

    #[compio::test]
    async fn registry_database_error_response_is_sanitized() {
        let resp = error_response(RegistryError::Database(
            "db connect failed: postgres://internal/schema".into(),
        ));
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(resp).await;
        assert_eq!(body, serde_json::json!({"error": "internal error"}));
    }

    #[compio::test]
    async fn ingest_infrastructure_error_response_is_sanitized() {
        let resp = ingest_error_to_response(IngestError::BlobStoreUnavailable(
            "put_blob_stream(abc): /var/private/blob path".into(),
        ));
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        assert_eq!(body, serde_json::json!({"error": "internal error"}));

        let resp = ingest_error_to_response(IngestError::Internal(
            "put_manifest: postgres://internal".into(),
        ));
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(resp).await;
        assert_eq!(body, serde_json::json!({"error": "internal error"}));
    }

    #[compio::test]
    async fn worker_logs_infrastructure_error_response_is_sanitized() {
        let resp = infrastructure_error_response(
            StatusCode::BAD_GATEWAY,
            "worker logs unavailable",
            "http://worker.internal:8080 HTTP 500: secret body",
        );
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let body = body_json(resp).await;
        assert_eq!(body, serde_json::json!({"error": "internal error"}));
    }
}

#[cfg(test)]
mod stream_tmp_tests {
    use super::*;
    use bytes::Bytes;
    use futures::stream;

    fn temp_path(label: &str) -> std::path::PathBuf {
        let unique = uuid::Uuid::new_v4().simple().to_string();
        std::env::temp_dir().join(format!("zs-stream-test-{label}-{unique}"))
    }

    #[compio::test]
    async fn happy_path_writes_concatenated_bytes() {
        let path = temp_path("happy");
        let chunks: Vec<Result<Bytes, &str>> = vec![
            Ok(Bytes::from_static(b"hello, ")),
            Ok(Bytes::from_static(b"streaming ")),
            Ok(Bytes::from_static(b"world!")),
        ];
        let mut s = stream::iter(chunks);
        let written = stream_body_to_tmp_file(&mut s, &path, 1024)
            .await
            .expect("stream ok");
        assert_eq!(written, b"hello, streaming world!".len() as u64);
        let contents = std::fs::read(&path).unwrap();
        assert_eq!(contents, b"hello, streaming world!");
        let _ = std::fs::remove_file(&path);
    }

    #[compio::test]
    async fn cap_exceeded_removes_tmp_file() {
        let path = temp_path("cap");
        let chunks: Vec<Result<Bytes, &str>> = vec![
            Ok(Bytes::from_static(b"AAAAAAAAAA")), // 10 bytes
            Ok(Bytes::from_static(b"BBBBBBBBBB")), // would push to 20, > 15
        ];
        let mut s = stream::iter(chunks);
        let err = stream_body_to_tmp_file(&mut s, &path, 15)
            .await
            .unwrap_err();
        match err {
            StreamToTmpError::TooLarge { cap: 15, observed: 20 } => {}
            other => panic!("expected TooLarge, got {other:?}"),
        }
        assert!(!path.exists(), "tmp file should be removed on cap-exceeded");
    }

    #[compio::test]
    async fn stream_error_removes_tmp_file() {
        let path = temp_path("err");
        let chunks: Vec<Result<Bytes, &str>> = vec![
            Ok(Bytes::from_static(b"some bytes")),
            Err("network blew up"),
        ];
        let mut s = stream::iter(chunks);
        let err = stream_body_to_tmp_file(&mut s, &path, 1024)
            .await
            .unwrap_err();
        match err {
            StreamToTmpError::PayloadError(detail) => {
                assert!(detail.contains("network blew up"), "got {detail}");
            }
            other => panic!("expected PayloadError, got {other:?}"),
        }
        assert!(!path.exists(), "tmp file should be removed on payload error");
    }

    #[compio::test]
    async fn empty_stream_writes_zero_bytes() {
        let path = temp_path("empty");
        let chunks: Vec<Result<Bytes, &str>> = vec![];
        let mut s = stream::iter(chunks);
        let written = stream_body_to_tmp_file(&mut s, &path, 1024)
            .await
            .expect("stream ok");
        assert_eq!(written, 0);
        // Empty file should exist (we created it before the loop).
        assert!(path.exists(), "tmp file should exist even when empty");
        let contents = std::fs::read(&path).unwrap();
        assert!(contents.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[compio::test]
    async fn create_new_fails_when_path_exists() {
        let path = temp_path("collide");
        std::fs::write(&path, b"pre-existing").unwrap();
        let chunks: Vec<Result<Bytes, &str>> = vec![Ok(Bytes::from_static(b"x"))];
        let mut s = stream::iter(chunks);
        let err = stream_body_to_tmp_file(&mut s, &path, 1024)
            .await
            .unwrap_err();
        match err {
            StreamToTmpError::OpenFailed(_) => {}
            other => panic!("expected OpenFailed, got {other:?}"),
        }
        // Pre-existing file must not be overwritten.
        let contents = std::fs::read(&path).unwrap();
        assert_eq!(contents, b"pre-existing");
        let _ = std::fs::remove_file(&path);
    }
}
