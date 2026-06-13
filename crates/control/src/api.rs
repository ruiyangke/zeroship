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

            // Atomic UPDATE: deploy_hash + manifest_json land together
            // so the gateway never sees half-applied state. Committed AFTER
            // OAuth provisioning so the route only becomes resolvable once the
            // client exists.
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
    if let Err(resp) = authz
        .require(Action::BillingWrite, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }
    match state.registry.set_plan(&uid, &body.plan_id).await {
        Ok(true) => web::HttpResponse::Ok().json(&serde_json::json!({"updated": true})),
        Ok(false) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Err(e) => error_response(e),
    }
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
}

impl From<crate::plan_catalog::Plan> for PlanDto {
    fn from(p: crate::plan_catalog::Plan) -> Self {
        Self { id: p.id, name: p.name, price: p.price, runtime: p.runtime, archived: p.archived }
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
    };
    let catalog = crate::plan_catalog::PlanCatalog::new(state.registry.clone());
    match catalog.upsert(&plan, archived).await {
        Ok(written) => web::HttpResponse::Ok().json(&PlanDto::from(written)),
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
    let catalog = crate::plan_catalog::PlanCatalog::new(state.registry.clone());
    match catalog.archive(&id).await {
        Ok(true) => web::HttpResponse::Ok().json(&serde_json::json!({"archived": true})),
        Ok(false) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"plan not found"}))
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
