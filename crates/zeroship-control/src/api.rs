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

#[derive(Debug, Deserialize)]
pub struct CreateAppBody {
    pub name: String,
    #[serde(default = "default_plan")]
    pub plan_id: String,
}

/// Default plan for a `create_app` with no explicit `plan_id`: the built-in
/// free tier's catalog id (`pln_…`). The plan must be a real catalog id so
/// the FK + server-side gate accept it.
fn default_plan() -> String {
    crate::plan_catalog::free_plan_id()
}

#[derive(Debug, Deserialize)]
pub struct SetPlanBody {
    pub plan_id: String,
}

/// Body for the operator credit-grant endpoint `POST /api/billing/credit`
/// (billing-ops gap #26, PR-2). The operator supplies the creator, a positive
/// amount, and an optional kind/expiry/note. Currency is USD-pinned (v1) — the
/// `credit::grant` boundary rejects any other. The idempotency key arrives in the
/// `Idempotency-Key` header (not the body) so a retried POST is a no-op.
#[derive(Debug, Deserialize)]
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
#[derive(Debug, Deserialize)]
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
        // 409, not 400: the name is well-formed and simply unavailable, which
        // is the same thing a creator does about it as a duplicate name. A 400
        // would file it with the charset rejection and read as "malformed".
        RegistryError::ReservedName(msg) => {
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
        // Reachable only through a caller that does not build the remedy body.
        // `deploy` intercepts this variant before `error_response` and answers
        // with `schema_precondition_response`, which carries the command.
        RegistryError::SchemaNotApplied { .. } => {
            web::HttpResponse::Conflict().json(&serde_json::json!({
                "error": "schema_not_applied",
                "detail": e.to_string(),
            }))
        }
    }
}

/// The 409 a deploy gets when its runtime schema descriptor does not name the
/// schema the app's database holds.
///
/// 409, not 400: the artifact is well-formed and the request is well-formed;
/// what is wrong is the ORDER two correct operations happened in. And 409 is
/// load-bearing beyond readability - `should_resolve_or_create_after_deploy_failure`
/// in the CLI returns true only on 404, so a 409 does not trigger the
/// auto-create-and-retry path and the creator sees this body rather than a
/// second failure against a freshly created app.
///
/// `remedy` is a command, not a sentence. The CLI prints this body raw, so a
/// creator can copy the line out of the terminal.
fn schema_precondition_response(
    app_id: &uuid::Uuid,
    descriptor_sha256: &Option<String>,
    applied_sha256: &Option<String>,
) -> web::HttpResponse {
    let (error, detail) = match descriptor_sha256 {
        Some(_) => (
            "schema_not_applied",
            "this build's migrations have not been applied to the app's database. \
             Deploying it would run code against a schema it was not built for - and \
             where the two disagree about masking, the runtime would serve the plain \
             value believing it was masked.",
        ),
        None => (
            "schema_descriptor_missing",
            "this artifact carries no runtime schema descriptor, but the app has \
             applied migrations. Deploying it would boot the app with `env.db` \
             uninstalled over a live database. Build with the migrations present.",
        ),
    };
    web::HttpResponse::Conflict().json(&serde_json::json!({
        "error": error,
        "detail": detail,
        "deploy_descriptor_sha256": descriptor_sha256,
        "applied_descriptor_sha256": applied_sha256,
        "remedy": format!("zeroship migrate --app={app_id}"),
    }))
}

/// Log the real cause, return a generic body -- plus the correlation id
/// that joins the two.
///
/// The id was already minted and already logged here; it just never left
/// this helper. That made failures routed through this helper undiagnosable
/// from the outside: a creator could report "it returned internal error"
/// and an operator had no key to search the logs by. The message stays
/// generic on purpose. This is not a message redesign; it is the difference
/// between undiagnosable and reportable.
///
/// ## Why `trace_id` and not `request_id`
///
/// The wire contract already chose. `sdks/rpc/src/error.ts` documents the
/// envelope as `{ code, message, details?, retryable, trace_id? }` and lifts
/// `trace_id`/`traceId`; `RpcCtx` (`crates/zeroship-runtime/src/rpc/ctx_holder.rs`)
/// carries a `trace_id` field. This tree already has more request-id
/// concepts than it can join -- the gateway's `X-Request-Id`, the runtime's
/// per-isolate `u64`, and `authz_guard::request_id` in this very crate --
/// and adding a fourth spelling of "the id in the error body" would make
/// that worse, not better. So the id goes out under the name the clients
/// already read.
///
/// The log field is renamed to match: the body value and the log value are
/// the same string under the same key, so an operator handed a `trace_id`
/// greps for `trace_id` and finds the cause. A field that is emitted under
/// one name and logged under another correlates nothing.
///
/// ## What this does NOT close
///
/// Only responses routed through `infrastructure_error_response` are
/// correlated. `env_handlers::env_err_response`, used by
/// `control.env.listVars`, remains id-less. Other direct control-plane 5xx
/// bodies also remain outside this helper.
///
/// The app-dispatch path remains uncorrelated:
///
/// - generic sanitized 5xx bodies carry a per-isolate `request_id`;
/// - public-code 5xx bodies carry no id; and
/// - `@zeroship/rpc` can lift `trace_id`, but this path emits none.
///
/// The generic body's counter is logged by that isolate, but it is not a
/// globally unique id and does not join to the gateway's `X-Request-Id`.
/// Closing this gap needs an edge id propagated through the worker and into
/// app and workflow dispatch, which is separate work.
pub(crate) fn infrastructure_error_response(
    status: StatusCode,
    context: &'static str,
    detail: impl std::fmt::Display,
) -> web::HttpResponse {
    let trace_id = Uuid::new_v4().to_string();
    tracing::error!(
        trace_id = %trace_id,
        context,
        error = %detail,
        "control-plane infrastructure error"
    );
    web::HttpResponse::build(status).json(&serde_json::json!({
        "error": "internal error",
        "trace_id": trace_id,
    }))
}

#[cfg(test)]
pub(crate) mod infrastructure_error_test_support {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing::{Event, Subscriber};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry::LookupSpan;

    #[derive(Default)]
    struct FieldVisitor {
        fields: HashMap<String, String>,
    }

    impl Visit for FieldVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.fields
                .insert(field.name().to_string(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
    }

    struct CaptureLayer {
        events: Arc<Mutex<Vec<HashMap<String, String>>>>,
    }

    impl<S> Layer<S> for CaptureLayer
    where
        S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    {
        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            let mut visitor = FieldVisitor::default();
            event.record(&mut visitor);
            self.events
                .lock()
                .expect("trace event buffer mutex poisoned")
                .push(visitor.fields);
        }
    }

    pub(crate) fn capture<R>(f: impl FnOnce() -> R) -> (R, Vec<HashMap<String, String>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(CaptureLayer {
            events: Arc::clone(&events),
        });
        let result = tracing::subscriber::with_default(subscriber, f);
        let captured = events
            .lock()
            .expect("trace event buffer mutex poisoned")
            .clone();
        (result, captured)
    }

    pub(crate) fn assert_logged_trace_id(
        events: &[HashMap<String, String>],
        trace_id: &str,
    ) {
        assert_eq!(events.len(), 1, "expected one infrastructure error event");
        assert_eq!(
            events[0].get("trace_id").map(String::as_str),
            Some(trace_id),
            "log trace_id must match the response body"
        );
        assert!(
            !events[0].contains_key("request_id"),
            "infrastructure error event must use trace_id, not request_id"
        );
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// The JSON body of a successful create-app response.
///
/// It is exactly the serialized [`zeroship_core::types::AppRecord`], and the
/// point of naming the function is that the "exactly" is now enforceable.
///
/// THE CREATE RESPONSE NO LONGER HANDS OUT AN API KEY. It used to: the field is
/// `#[serde(skip_serializing)]` on the struct, and this site re-added it by hand
/// so the caller got the plaintext key back. Nothing ever validated that key.
/// The gateway's `check_api_key` was the only code that could have, it lost its
/// call site when RPC v1 replaced it with the compiled per-resource
/// `EffectivePolicy`, and both it and the hash it compared against are deleted.
/// Handing a caller a credential no request path consults is worse than handing
/// them nothing: it reads as an authentication mechanism.
///
/// `AppRecord::api_key` and the `zeroship.apps.api_key` column both still
/// exist - this crate's `dev_provision` binary prints the value and a dozen
/// shell harnesses parse that line - so this stops the key leaving over HTTP
/// without pretending the column is gone.
fn create_app_response_body(record: &zeroship_core::types::AppRecord) -> serde_json::Value {
    serde_json::to_value(record).unwrap_or_else(|_| serde_json::json!({}))
}

#[cfg(test)]
mod create_app_response_tests {
    use super::create_app_response_body;

    fn record() -> zeroship_core::types::AppRecord {
        zeroship_core::types::AppRecord {
            id: uuid::Uuid::nil(),
            name: "sample".to_string(),
            plan_id: "free".to_string(),
            deploy_hash: None,
            archived_at: None,
            api_key: "plaintext-key-that-must-not-be-returned".to_string(),
            created_at: "2026-09-05T00:00:00Z".to_string(),
            updated_at: "2026-09-05T00:00:00Z".to_string(),
        }
    }

    /// The create response must not carry an api-key field under ANY spelling,
    /// and must not carry the key's value under some other name either. The
    /// second half matters: an assertion on the key name alone would pass
    /// against a body that renamed the field and kept leaking the secret.
    #[test]
    fn create_response_carries_no_api_key() {
        let body = create_app_response_body(&record());
        let object = body.as_object().expect("create response is a JSON object");

        for key in object.keys() {
            assert!(
                !key.contains("api_key") && !key.contains("apiKey"),
                "the create-app response must not carry an api-key field; found {key:?}"
            );
        }

        assert!(
            !serde_json::to_string(&body)
                .expect("serialize create response")
                .contains("plaintext-key-that-must-not-be-returned"),
            "the create-app response must not carry the plaintext key under any name"
        );
    }

    /// CONTROL for the assertion above: the same body must still carry the
    /// fields a caller needs, so "no api key" cannot be satisfied by an empty
    /// object. Without this, `create_app_response_body` returning `json!({})`
    /// would pass the test it exists to fail.
    #[test]
    fn create_response_still_carries_the_app_identity() {
        let body = create_app_response_body(&record());
        for key in ["id", "name", "plan_id", "created_at", "updated_at"] {
            assert!(
                body.get(key).is_some(),
                "the create-app response must still carry {key:?}"
            );
        }
    }
}

pub async fn create_app(
    req: web::HttpRequest,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
    body: Json<CreateAppBody>,
) -> web::HttpResponse {
    if let Some(resp) = crate::env_handlers::admin_rate_limit(&req, &state).await {
        return resp;
    }
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
            // Provision the per-app public PKCE OAuth client immediately after
            // creating the app. This is best-effort relative to the create
            // response: a DB hiccup here is logged + metered, and the next
            // deploy re-provisions (`ensure_app_client` is idempotent). The app
            // still exists.
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
            web::HttpResponse::Created().json(&create_app_response_body(&record))
        }
        Err(e) => error_response(e),
    }
}

pub async fn list_apps(
    req: web::HttpRequest,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(resp) = crate::env_handlers::admin_rate_limit(&req, &state).await {
        return resp;
    }
    if let Err(resp) = authz.require(Action::AppsRead, Resource::Any, &state).await {
        return resp;
    }
    // The self-service policy grants every creator `apps:read` on the platform
    // surface, so the gate above passes for ordinary creators too. The DATA is
    // therefore scoped to membership, always: a caller sees the apps they are a
    // member of and nothing else. Without that scope the broadened gate would
    // be a fleet-wide cross-tenant read (the exact C1 leak, at the list
    // endpoint).
    //
    // There is no fleet-wide arm any more. It was selected by a direct SQL read
    // of `platform_admin_roles` rather than by Cedar - so it was invisible to
    // every audit of the policy set - and it returned every tenant's apps to any
    // of the four deleted staff roles. A vendor wanting a fleet-wide list builds
    // it in the portal against its own copy of the data.
    match state.registry.list_apps_for_owner(&authz.principal_id).await {
        Ok(apps) => web::HttpResponse::Ok().json(&apps),
        Err(e) => error_response(e),
    }
}

pub async fn get_app(
    req: web::HttpRequest,
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(resp) = crate::env_handlers::admin_rate_limit(&req, &state).await {
        return resp;
    }
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

/// Archive an app. This is a single retry-safe database transition: it does
/// not delete manifests, billing evidence, migration history, or database
/// state. Active-route and background-workflow projections enforce the marker.
/// Route invalidation is pull-based: a stale gateway snapshot can continue to
/// serve until a successful poll, and archive does not terminate requests or
/// long-lived connections that were already admitted.
/// The worker version feed, OAuth identities, and relay aliases are retained;
/// archive does not independently disable those retained identity surfaces.
/// Metering ingest also remains active so late and in-flight usage is not lost.
/// A deploy may update retained code while archived, but cannot become routable
/// or acquire workflow work until this marker is removed.
pub async fn archive_app(
    req: web::HttpRequest,
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(resp) = crate::env_handlers::admin_rate_limit(&req, &state).await {
        return resp;
    }
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    if let Err(resp) = authz
        .require(Action::AppsArchive, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }
    match state.registry.archive_app(&uid).await {
        Ok(Some(record)) => web::HttpResponse::Ok().json(&record),
        Ok(None) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Err(e) => error_response(e),
    }
}

/// Restore an archived app. Normally the retained name and manifest let the
/// route-sync and workflow polling paths restore service without reconstruction.
/// An app whose manifest keyspace was already removed by the former partial
/// hard-delete path needs a staged redeploy; restore contains no repair shim.
pub async fn unarchive_app(
    req: web::HttpRequest,
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(resp) = crate::env_handlers::admin_rate_limit(&req, &state).await {
        return resp;
    }
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    if let Err(resp) = authz
        .require(Action::AppsArchive, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }
    match state.registry.unarchive_app(&uid).await {
        Ok(Some(record)) => web::HttpResponse::Ok().json(&record),
        Ok(None) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Err(e) => error_response(e),
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
    // Deploy is the most expensive endpoint here - it streams a body to disk,
    // mmaps it, and writes every blob in the bundle - and nothing bounded how
    // often one caller could ask for that. Shares the env handlers' admin bucket
    // so a caller cannot get a fresh allowance by switching surface.
    //
    // This runs AFTER authentication, not before it: `AuthzGuard` is an
    // extractor, so its `FromRequest` has already rejected an unauthenticated
    // caller by the time any handler body executes. Bounding the pre-auth cost
    // would take middleware, not a call here. What this does bound is
    // everything after auth, which is where the disk and blob-store work is.
    if let Some(resp) = crate::env_handlers::admin_rate_limit(&req, &state).await {
        return resp;
    }

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
    if has_legacy_deploy_migration_query(req.query_string()) {
        return web::HttpResponse::BadRequest().json(&serde_json::json!({
            "error": "migration_approval_removed",
            "detail": "deploy no longer applies migrations; run zeroship migrate against /v1/apps/{id}/migrations/apply",
        }));
    }

    // The app has to exist before we take the upload. `ingest` below persists
    // every blob in the bundle, and it runs before the lookup that produces the
    // deploy record - so without this gate a bundle aimed at an app that was
    // never created is written to the blob store and then answered 404, leaving
    // blobs nothing will reference, bill, or collect.
    //
    // Authz alone does not cover it: a caller holding a fleet-wide grant is
    // authorized for an app id whether or not a row exists behind it. This
    // joins the rejections above that all resolve before a body byte is read.
    match state.registry.get_app(&uid).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return web::HttpResponse::NotFound()
                .json(&serde_json::json!({"error":"app not found"}))
        }
        Err(e) => return error_response(e),
    }

    // Count this deploy as in flight for the rest of the handler. Everything
    // above resolves before a body byte is read, so a rejected caller occupies
    // nothing and must not appear in the count; everything below holds the
    // artifact on disk and then mapped, and returns from several places, so the
    // release has to ride on `Drop` rather than a matched decrement.
    //
    // This admits every deploy - it measures concurrency, it does not limit it.
    // The per-deploy take is bounded but the aggregate is not, and sizing a
    // limit needs the peak a real deployment reaches rather than a guess.
    //
    // The placement is measured, not assumed: panicking here fails exactly the
    // five `deploy_http_test` cases that stream an artifact (happy path, both
    // scope cases, manifest-not-first, legacy-manifest) and leaves the five
    // rejection cases passing - missing auth, wrong content type, unknown app,
    // legacy migration query, rate limited. That pins the boundary and nothing
    // more: no test drives two deploys at once, so the peak arithmetic is
    // covered only by the unit tests in `deploy_inflight`.
    let inflight = crate::deploy_inflight::DEPLOY_INFLIGHT.enter();
    if inflight.is_new_peak() {
        tracing::info!(
            concurrent_deploys = inflight.depth(),
            "deploy concurrency reached a new peak"
        );
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
            // Reconcile the per-app OAuth client before the manifest commit.
            // When reconciliation succeeds, the gateway's next 5s route-sync
            // pull sees the client through `get_routes`' LEFT JOIN on
            // `zeroship.app_oauth_clients`. Reconcile is idempotent and
            // best-effort relative to the deploy response: a DB
            // hiccup is logged and the next deploy retries, so deploy 200 does
            // NOT imply provisioning succeeded. The SDK handles that rare
            // window with a retryable 503 `client_not_provisioned` response.
            // Re-parse the ingested manifest to extract its declared
            // `auth.scopes`. `ingest` only enforces scope-id FORMAT
            // (ScopeDef::validate_id_format inside Manifest::validate), NOT the
            // platform-vocabulary collision rule — so the deploy handler MUST run
            // the full `validate_app_scopes` guard here and HARD-FAIL the deploy
            // before anything is provisioned or committed. A re-parse failure is a
            // control-side programming error (ingest already parsed+validated the
            // same bytes), but we still abort the deploy rather than silently drop
            // declared scopes (which would wipe the app_scope_defs registry on the
            // next provision).
            // The SAME re-parse also yields the runtime schema descriptor the
            // artifact carries. It is read here rather than re-parsed later so
            // there is exactly one interpretation of these bytes on this path.
            let (declared_scopes, descriptor_sha256) =
                match serde_json::from_str::<zeroship_bundle::Manifest>(&success.manifest_json) {
                    Ok(m) => (m.auth.scopes, m.runtime_descriptor.map(|entry| entry.hash)),
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

            // Attempt OAuth-client reconciliation BEFORE the manifest commit.
            // Scopes are already validated above, so `ensure_app_client`'s
            // internal `validate_app_scopes` cannot reject; a DB error is logged
            // and the deploy proceeds with the documented retryable-503 window.
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
            // so the gateway never sees half-applied state. Deploy only ships
            // code/assets/route metadata; database migrations are applied through
            // the migration service as a separate operation.
            match state
                .registry
                .set_deploy_with_manifest(
                    &uid,
                    &success.deploy_hash,
                    &success.manifest_json,
                    descriptor_sha256.as_deref(),
                )
                .await
            {
                Ok(true) => web::HttpResponse::Ok().json(&serde_json::json!({
                    "deploy_hash": success.deploy_hash,
                    "blobs_uploaded": success.blobs_uploaded,
                    "blobs_deduped": success.blobs_deduped,
                })),
                Ok(false) => web::HttpResponse::NotFound()
                    .json(&serde_json::json!({"error": "app not found"})),
                Err(RegistryError::SchemaNotApplied {
                    descriptor_sha256,
                    applied_sha256,
                }) => schema_precondition_response(&uid, &descriptor_sha256, &applied_sha256),
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

fn has_legacy_deploy_migration_query(query: &str) -> bool {
    url::form_urlencoded::parse(query.as_bytes()).any(|(key, _)| {
        let key = key.trim();
        key == "approved_versions" || key == "expected_manifest"
    })
}

#[cfg(test)]
mod deploy_query_tests {
    use super::has_legacy_deploy_migration_query;

    #[test]
    fn legacy_migration_query_keys_are_decoded_before_matching() {
        assert!(has_legacy_deploy_migration_query("approved_versions=1"));
        assert!(!has_legacy_deploy_migration_query("unrelated=value"));
        assert!(has_legacy_deploy_migration_query("approved%5Fversions=1"));
    }
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

    // MAJOR-4: assigning a plan needs `BillingWrite` on the app AND a target
    // plan flagged `assignable_by_creator = true`. Without the second gate a
    // creator could PUT a cheaper operator-only tier (unlimited/console) and
    // underpay - the asymmetry the reduction-only spend-limit override already
    // closes for caps.
    //
    // This used to probe `BillingWrite` on `Resource::Any` first and skip the
    // assignability gate for an "operator". That arm was satisfiable only by
    // the deleted universal-allow policy - `enforce` intersects a bearer's
    // wrapper with the static set, so no token could reach it either - and it
    // is gone with it. Operator-only tiers are now assigned by editing the
    // catalog row, not by holding a cross-tenant grant.
    if let Err(resp) = authz
        .require(Action::BillingWrite, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }
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

    // Record a plan-change event with a cumulative usage_at_change snapshot IN
    // THE SAME TXN as the apps.plan_id
    // flip, under the per-creator advisory lock. The target plan must be a real,
    // non-archived plan; validate it via the catalog (segment pricing reads the
    // live catalog at reconcile time — the row freezes NO base fee).
    //
    // BOTH arms matter, and the archived one is easy to leave out because
    // `PlanCatalog::get` selects `archived` without filtering on it. The
    // proration UPDATE downstream is guarded on
    // `EXISTS (... AND NOT archived)`, so an archived plan that reaches it
    // matches zero rows and returns `AppNotFound` — a 404 "app not found" for
    // an app that plainly exists, which sends the caller looking for a deleted
    // app. Refuse it here, where the reason is still known.
    let catalog = crate::plan_catalog::PlanCatalog::new(state.registry.clone());
    match catalog.get(&body.plan_id).await {
        Ok(Some(plan)) if plan.archived => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": "plan archived"}))
        }
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
// Spend-limit override — the creator-facing cap.
//
// `PUT /api/apps/:id/spend-limit` body `{ "cents": <u64|null> }` sets (or, with
// null, clears back to the plan default) the per-app spend-limit override.
// `GET` returns the effective limit + current state. Authz is BillingWrite /
// BillingRead on `Resource::App(id)` — the same app-membership gate `set_plan`
// uses.
//
// REDUCTION-ONLY by design: the override is bounded above by the plan's
// `spend_limit_default_cents`, so a creator can only LOWER their effective cap,
// never raise it above what the plan already grants. This is intentional and
// safe — there is NO privilege-escalation path: raising your effective headroom
// means UPGRADING the plan (an operator/billing-gated action), not editing this
// override. We therefore do NOT model a separate `spend_limit_max_cents`
// column; the plan default IS the ceiling. A request above it is rejected 403.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
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
#[derive(Debug, Deserialize)]
pub struct InvoiceListQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// Optional `?creator_id=` for the creator-keyed reads. Honoured ONLY for an
/// operator (`Resource::Any`); a non-operator caller is always forced to self.
#[derive(Debug, Deserialize)]
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
/// (`owner_of_app(id) == principal_id`).
///
/// There is no operator escape from that any more. The arm that let a
/// `BillingRead`-on-`Resource::Any` caller read another creator's invoices was
/// satisfiable only by the deleted universal-allow policy, so it was a
/// cross-tenant read with no remaining principal behind it.
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
    // OWNER only: the app's OWNER is the invoice creator and reads their own
    // cross-app invoice history; a non-owner member does not.
    if creator_id != authz.principal_id {
        return web::HttpResponse::Forbidden().json(&serde_json::json!({"error": "forbidden"}));
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
/// creator owns, so the caller must BE that creator. We do NOT loop the
/// creator's apps and accept any `BillingRead` grant: `list_apps_for_owner` is
/// role-AGNOSTIC, so a creator who is merely a viewer/editor on the attacker's
/// app would appear in the list and let the attacker read the victim's whole
/// invoice. A single `creator_id == principal_id` check closes that
/// cross-creator hole AND removes the per-app Cedar-loop audit amplification.
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

    // The caller is the invoice's creator. Nothing else reads it.
    if detail.creator_id != authz.principal_id {
        return web::HttpResponse::Forbidden().json(&serde_json::json!({"error": "forbidden"}));
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
/// The caller is ALWAYS forced to `self`: they must be billing-capable on at
/// least one owned app, and a `creator_id` naming ANOTHER creator is 403. A
/// caller with no billing capability anywhere is 403.
///
/// The `?creator_id=` parameter therefore now only confirms or contradicts the
/// caller's own id. It is kept rather than removed because the contradiction is
/// worth answering with a 403 instead of silently reading the caller's own
/// data under someone else's name.
async fn resolve_creator_target(
    authz: &AuthzGuard,
    state: &AppState,
    requested: Option<Uuid>,
) -> Result<Uuid, web::HttpResponse> {
    if !authz.can_act_anywhere(Action::BillingRead, state).await? {
        return Err(web::HttpResponse::Forbidden()
            .json(&serde_json::json!({"error": "forbidden"})));
    }
    if let Some(req) = requested {
        if req != authz.principal_id {
            return Err(web::HttpResponse::Forbidden().json(&serde_json::json!({
                "error": "forbidden",
                "detail": "a creator may read only their own billing",
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
    use super::infrastructure_error_test_support::{assert_logged_trace_id, capture};
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

    /// Assert the two properties every body produced by this helper must hold,
    /// and return the correlation id.
    ///
    /// 1. The message is still generic. Emitting the id is NOT permission
    ///    to emit the cause; `detail` goes to `tracing` and nowhere else.
    /// 2. A `trace_id` is present and is a parseable UUID -- the key an
    ///    operator greps the logs by. The field is named `trace_id`
    ///    because that is the spelling the SDKs already read; see
    ///    `infrastructure_error_response` for why a fourth id name was
    ///    not introduced.
    ///
    fn assert_sanitized_with_id(body: &serde_json::Value) -> String {
        assert_eq!(
            body.get("error").and_then(serde_json::Value::as_str),
            Some("internal error"),
            "the cause must stay out of the body: {body}"
        );
        let id = body
            .get("trace_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("body must carry trace_id: {body}"))
            .to_string();
        uuid::Uuid::parse_str(&id)
            .unwrap_or_else(|e| panic!("trace_id must be a UUID ({id}): {e}"));
        assert!(
            body.get("request_id").is_none(),
            "the id rides under `trace_id` only; a second spelling would be a \
             fourth id concept: {body}"
        );
        // Exactly two keys: emitting the id must not have opened the body up.
        assert_eq!(
            body.as_object().map(serde_json::Map::len),
            Some(2),
            "body carries error + trace_id and nothing else: {body}"
        );
        id
    }

    #[compio::test]
    async fn registry_database_error_response_is_sanitized() {
        let (resp, events) = capture(|| {
            error_response(RegistryError::Database(
                "db connect failed: postgres://internal/schema".into(),
            ))
        });
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(resp).await;
        let trace_id = assert_sanitized_with_id(&body);
        assert_logged_trace_id(&events, &trace_id);
        assert!(
            !body.to_string().contains("postgres://"),
            "the DSN must not ride out: {body}"
        );
    }

    #[compio::test]
    async fn ingest_infrastructure_error_response_is_sanitized() {
        let resp = ingest_error_to_response(IngestError::BlobStoreUnavailable(
            "put_blob_stream(abc): /var/private/blob path".into(),
        ));
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        assert_sanitized_with_id(&body);

        let resp = ingest_error_to_response(IngestError::Internal(
            "put_manifest: postgres://internal".into(),
        ));
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(resp).await;
        assert_sanitized_with_id(&body);
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
        let id = assert_sanitized_with_id(&body);
        assert!(
            !body.to_string().contains("worker.internal"),
            "the upstream host must not ride out: {body}"
        );
        assert!(
            !body.to_string().contains("secret body"),
            "the upstream body must not ride out: {body}"
        );

        // ONE-VARIABLE CONTROL on the id itself: a second call with a
        // different `detail` must get a DIFFERENT id. A constant would
        // satisfy every assertion above and correlate nothing.
        let other = infrastructure_error_response(
            StatusCode::BAD_GATEWAY,
            "worker logs unavailable",
            "a different failure",
        );
        let other_id = assert_sanitized_with_id(&body_json(other).await);
        assert_ne!(id, other_id, "each response gets its own correlation id");
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

    #[test]
    fn infrastructure_comment_names_the_uncorrelated_app_dispatch_shapes() {
        let source = include_str!("api.rs");
        let (comment, _) = source
            .split_once("pub(crate) fn infrastructure_error_response")
            .expect("infrastructure helper must remain documented");
        let comment = comment
            .rsplit_once("/// Log the real cause")
            .map(|(_, tail)| tail)
            .expect("infrastructure helper documentation must keep its anchor");

        for required in [
            "app-dispatch path remains uncorrelated",
            "generic sanitized 5xx bodies carry a per-isolate `request_id`",
            "public-code 5xx bodies carry no id",
            "`@zeroship/rpc` can lift `trace_id`, but this path emits none",
        ] {
            assert!(
                comment.contains(required),
                "infrastructure helper documentation must say {required:?}; got:\n{comment}"
            );
        }
    }

    #[test]
    fn proposal_keeps_app_dispatch_correlation_open() {
        let proposal = include_str!(
            "../../../docs/proposals/2026-08-14-error-message-quality.md"
        );

        for required in [
            "The control helper loop is closed; the app-dispatch loop is not.",
            "Generic sanitized app 5xx bodies carry a per-isolate `request_id`",
            "Public-code app 5xx bodies carry no id.",
            "`@zeroship/rpc` lifts `trace_id`, but app dispatch emits none.",
            "Option 1c remains open for app dispatch.",
            "`SET LOCAL ROLE` reports `invalid_parameter_value` / `22023`",
            "A missing schema with a fully-qualified query reports `undefined_table` / `42P01`",
            "A present role without required grants reports `insufficient_privilege` / `42501`",
            "`42P01` and `42501` must not be added to the missing-role classifier.",
        ] {
            assert!(
                proposal.contains(required),
                "the proposal must state the partial Option 1c scope: {required:?}"
            );
        }
    }

    #[test]
    fn correlation_claims_are_scoped_to_the_infrastructure_helper_family() {
        let compact = |text: &str| {
            text.split_whitespace()
                .filter(|token| !matches!(*token, "///" | "//" | "*"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let api = include_str!("api.rs");
        let (helper_comment, _) = api
            .split_once("pub(crate) fn infrastructure_error_response")
            .expect("infrastructure helper must remain documented");
        let helper_comment = helper_comment
            .rsplit_once("/// Log the real cause")
            .map(|(_, tail)| compact(tail))
            .expect("infrastructure helper documentation must keep its anchor");

        for required in [
            "Only responses routed through `infrastructure_error_response` are correlated.",
            "`env_handlers::env_err_response`, used by `control.env.listVars`, remains id-less.",
        ] {
            assert!(
                helper_comment.contains(required),
                "infrastructure helper documentation must say {required:?}; got:\n{helper_comment}"
            );
        }
        assert!(
            !helper_comment.contains("every control-plane infrastructure failure"),
            "the helper documentation must not claim coverage beyond its callers"
        );

        let (_, helper_tests) = api
            .split_once("#[cfg(test)]\nmod error_response_tests")
            .expect("helper response tests must remain present");
        let (helper_tests, _) = helper_tests
            .split_once("#[cfg(test)]\nmod stream_tmp_tests")
            .expect("helper response test section must remain bounded");
        assert!(
            compact(helper_tests)
                .contains("Assert the two properties every body produced by this helper must hold"),
            "helper test documentation must scope its body claim to the helper"
        );

        let sdk = compact(include_str!("../../../sdks/control/src/index.ts"));
        for required in [
            "Only responses produced by `infrastructure_error_response` carry this id.",
            "Failures such as `control.env.listVars` can remain id-less.",
        ] {
            assert!(
                sdk.contains(required),
                "the control SDK contract must say {required:?}"
            );
        }

        let reference = compact(include_str!("../../../docs/reference/control.md"));
        for required in [
            "`trace_id` is present only on responses produced by `infrastructure_error_response`.",
            "For example, `control.env.listVars` failures remain id-less.",
        ] {
            assert!(
                reference.contains(required),
                "the control reference must say {required:?}"
            );
        }
    }
}
