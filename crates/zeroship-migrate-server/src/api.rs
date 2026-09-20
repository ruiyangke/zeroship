use std::sync::Arc;

use ntex::http::StatusCode;
use ntex::web;
use ntex::web::types::{Json, Path, State};
use serde_json::json;
use uuid::Uuid;
use zeroship_authn::rate_limit::RateLimitDecision;
use zeroship_authz::Action;
use zeroship_core::DatabaseId;
use zeroship_id::AppId;

use zeroship_core::schema_bundle::{SchemaBundle, MIGRATE_AUDIENCE};
use zeroship_core::service_assertion::presented_issuer;
use zeroship_core::service_identity::{endpoints, verify_service_call};

use crate::apply::{
    apply_error_kind, apply_ir_documents, ApplyMigrationsRequest, ApplyRequestError,
};
use crate::auth::AuthError;
use crate::bundle::{apply_schema_bundle, BundleError};
use crate::MigrationServiceState;

/// JSON extractor budget for a migration apply request.
///
/// The CLI posts its generated IR envelope verbatim, so this route needs an
/// explicit budget above Ntex's small default while retaining a bounded body.
const APPLY_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;

/// JSON extractor budget for a schema bundle.
///
/// A bundle carries the whole ordered series as SQL, so it is large but bounded
/// by what the platform itself generates, not by anything a creator writes.
const SCHEMA_BUNDLE_BODY_BYTES: usize = 4 * 1024 * 1024;

/// THE APPROVAL AND POLICY ENDPOINTS ARE GONE, and their absence is the change
/// rather than a gap.
///
/// `POST /v1/apps/{app}/migrations/{migration}/approve` drove a state machine no
/// caller could reach: the level it gated on started at `Never` and only ever
/// loosened on a policy rule no artifact in the tree sets. Removing it removes a
/// capability - operator approval of destructive creator migrations - deliberately.
///
/// `PUT/GET /v1/apps/{app}/policy` and `GET .../policy/versions` wrote and read
/// `zeroship.migrated_app_policies`, a table this change deletes with them. A
/// migration policy now arrives with the apply request, declared in the
/// creator's repository and folded at build time, so there is nothing to store
/// and nothing to mutate at runtime.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/v1/apps/{app_id}/databases/{database_id}/migrations/apply")
            .state(web::types::JsonConfig::default().limit(APPLY_REQUEST_BODY_BYTES))
            .route(web::post().to(apply)),
    )
    .service(
        web::resource(endpoints::MIGRATE_SCHEMA_BUNDLE.path_template())
            .state(web::types::JsonConfig::default().limit(SCHEMA_BUNDLE_BODY_BYTES))
            .route(web::post().to(schema_bundle)),
    )
    .service(web::resource("/v1/apps/{app_id}/migrations/plan").route(web::post().to(stub_phase2)))
    .service(web::resource("/v1/apps/{app_id}/migrations/status").route(web::get().to(stub_phase2)))
    .service(web::resource("/v1/apps/{app_id}/migrations/rollback").route(web::post().to(rollback)))
    .service(web::resource("/healthz").route(web::get().to(healthz)))
    .service(web::resource("/readyz").route(web::get().to(readyz)));
}

/// Install or upgrade a PLATFORM-owned schema inside a creator database.
///
/// # There is no app in this request, and that is the shape
///
/// A creator database holds the schemas of every app in it, so the target is not
/// derivable from an app id and none is carried. Authorization is therefore not
/// "does this creator own that app" - there is no creator here. It is "is this a
/// platform service the allowlist grants this endpoint", decided on the service
/// assertion alone.
///
/// # It is not throttled by the mutation rate limiter
///
/// That limiter is a per-source-IP control over creator-driven DDL, and a
/// platform caller is ONE address issuing provisioning for the whole fleet;
/// applying it here would throttle the fleet to a creator's quota. Replay of an
/// assertion is already refused by the single-use claim store, and the operation
/// is idempotent.
pub async fn schema_bundle(
    req: web::HttpRequest,
    state: State<Arc<MigrationServiceState>>,
    body: Json<SchemaBundle>,
) -> web::HttpResponse {
    let caller = match authorize_platform_service(&req, &state).await {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    match apply_schema_bundle(&state.provision_dsn, &body).await {
        Ok(outcome) => {
            tracing::info!(
                schema = %outcome.schema,
                bundle = %outcome.bundle,
                version = outcome.version,
                action = ?outcome.action,
                caller = %caller,
                "migrate-server: applied schema bundle"
            );
            web::HttpResponse::Ok().json(&outcome)
        }
        Err(error) => schema_bundle_error_response(&error),
    }
}

/// Authorize a platform service for the schema-bundle endpoint.
///
/// Returns the verified issuer so the log line names WHICH service provisioned.
async fn authorize_platform_service(
    req: &web::HttpRequest,
    state: &MigrationServiceState,
) -> Result<String, web::HttpResponse> {
    let Some(peers) = state.peers.as_ref() else {
        tracing::error!(
            "migrate-server: schema bundle refused - no peer bundle is configured; set \
             migrate_server.service_peers_file"
        );
        return Err(web::HttpResponse::ServiceUnavailable().json(&json!({
            "error": "schema_bundle_unconfigured",
            "detail": "this service verifies no platform peers",
        })));
    };
    let header = req
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok());
    let issuer = presented_issuer(header).ok_or_else(|| {
        web::HttpResponse::Unauthorized().json(&json!({"error": "unauthenticated"}))
    })?;
    verify_service_call(
        peers.as_ref(),
        header,
        MIGRATE_AUDIENCE,
        endpoints::MIGRATE_SCHEMA_BUNDLE,
    )
    .await
    .map_err(|error| {
        tracing::debug!(?error, "migrate-server: schema bundle assertion rejected");
        web::HttpResponse::Unauthorized().json(&json!({"error": "unauthenticated"}))
    })?;
    Ok(issuer.as_str().to_owned())
}

fn schema_bundle_error_response(error: &BundleError) -> web::HttpResponse {
    let (status, kind) = error.kind();
    if status.is_server_error() {
        tracing::error!(%error, "migrate-server: schema bundle failed");
        return web::HttpResponse::build(status).json(&json!({
            "error": kind,
            "detail": "schema bundle service unavailable",
        }));
    }
    tracing::warn!(%error, "migrate-server: schema bundle refused");
    web::HttpResponse::build(status).json(&json!({
        "error": kind,
        "detail": error.to_string(),
    }))
}

/// Liveness. Constant 200 by design: it must not touch Postgres, or a database
/// blip would get this container killed on top of the outage.
pub async fn healthz() -> web::HttpResponse {
    web::HttpResponse::Ok().json(&json!({"ok": true}))
}

/// Readiness. Every endpoint this service exposes reads or writes Postgres, so
/// an unreachable database means it cannot serve. The probe is bounded,
/// cached and single-flighted; the body carries no DSN and no driver text.
///
/// A process that booted on the credential dev escape is NOT ready, whatever
/// Postgres says: the seal key it would confine a creator migration with is a
/// placeholder. That check is short-circuited FIRST so a doomed process does
/// not also generate database probes.
pub async fn readyz(state: State<Arc<MigrationServiceState>>) -> web::HttpResponse {
    if zeroship_core::config::dev_escape_active() {
        return web::HttpResponse::ServiceUnavailable().json(&json!({"ready": false}));
    }
    let ready = state
        .readiness
        .ready(|| async {
            match state.schema_apply_store.probe().await {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "migrate-server readiness: postgres unreachable"
                    );
                    false
                }
            }
        })
        .await;
    if ready {
        web::HttpResponse::Ok().json(&json!({"ready": true}))
    } else {
        web::HttpResponse::ServiceUnavailable().json(&json!({"ready": false}))
    }
}

/// Apply a creator's frozen IR into the schema of the DATABASE the request
/// names.
///
/// # Both ids are in the path, and neither is redundant
///
/// The DATABASE is the target: its schema is what the DDL writes, and an app
/// may hold several databases, so a target derived from the app could only
/// address one of them. It rides in the path rather than the body because the
/// CLI posts the build's `migrations.ir.json` VERBATIM - it does not parse or
/// rewrite that file, and a target it had to splice in would be a target it
/// could get wrong.
///
/// The APP is the authorization subject: the bearer is checked for
/// [`Action::AppsDeploy`] on it, exactly as every other mutation here is. It is
/// also what [`MigrationServiceState::bindings`] asks about - whether that app
/// still reaches that database - which is a separate question from whether the
/// principal may deploy the app, with a separate remedy.
///
/// Both segments are typed, so a uuid-rendered id or a `dbs_` where an `app_`
/// belongs is a 404 from the extractor rather than a request that authorizes
/// against something the roster does not hold.
pub async fn apply(
    req: web::HttpRequest,
    state: State<Arc<MigrationServiceState>>,
    path: Path<(AppId, DatabaseId)>,
    body: Json<ApplyMigrationsRequest>,
) -> web::HttpResponse {
    let (app_id, database_id) = path.into_inner();
    let caller = match authorize_mutation(&req, &state, &app_id).await {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    // ADMISSION, above every side effect. A refused apply must leave no
    // temporary directory, no role, no ledger row and no lock behind.
    match state
        .bindings
        .holds_live_binding(&app_id, &database_id)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return apply_error_response(ApplyRequestError::DatabaseNotBound {
                app_id,
                database_id,
            })
        }
        Err(error) => {
            tracing::error!(
                error = %error,
                app_id = app_id.as_str(),
                database_id = database_id.as_str(),
                "migrate-server: binding admission read failed"
            );
            return web::HttpResponse::ServiceUnavailable().json(&json!({
                "error": "binding_admission_unavailable",
                "detail": "the control plane could not be asked whether this app holds this \
                           database",
            }));
        }
    }

    match apply_ir_documents(
        &state.provision_dsn,
        &state.tmp_dir,
        &app_id,
        &database_id,
        &body,
        &state.policy_config,
        &state.schema_apply_store,
        &caller.principal_id,
    )
    .await
    {
        Ok(report) => {
            tracing::info!(
                app_id = app_id.as_str(),
                database_id = database_id.as_str(),
                principal_id = caller.principal_id.as_str(),
                applied = report.applied.len(),
                skipped = report.skipped.len(),
                "migrate-server: applied frozen IR migrations"
            );
            web::HttpResponse::Ok().json(&report)
        }
        Err(err) => apply_error_response(err),
    }
}

/// The Phase 2 rollback implementation is absent, but its mutating route is
/// already protected so replacing the stub cannot silently publish a new DDL
/// path without bearer authorization and shared throttling.
pub async fn rollback(
    req: web::HttpRequest,
    state: State<Arc<MigrationServiceState>>,
    app_id: Path<AppId>,
) -> web::HttpResponse {
    if let Err(response) = authorize_mutation(&req, &state, &app_id.into_inner()).await {
        return response;
    }
    stub_phase2().await
}

async fn authorize_mutation(
    req: &web::HttpRequest,
    state: &MigrationServiceState,
    app_id: &AppId,
) -> Result<crate::auth::VerifiedCaller, web::HttpResponse> {
    let Some(token) = bearer_token(req) else {
        return Err(web::HttpResponse::Unauthorized().json(&json!({"error": "unauthenticated"})));
    };
    let request_id = request_id(req);
    let source_ip = source_ip(req, state.trust_proxy);
    let caller = match state
        .authenticator
        .verify_action(token, app_id, Action::AppsDeploy, source_ip, &request_id)
        .await
    {
        Ok(caller) => caller,
        Err(err) => return Err(auth_error_response(err)),
    };
    if let Some(response) = mutation_rate_limit_response(
        state.mutation_rate_limiter.consume(source_ip).await,
        source_ip,
    ) {
        return Err(response);
    }
    Ok(caller)
}

pub async fn stub_phase2() -> web::HttpResponse {
    web::HttpResponse::build(StatusCode::NOT_IMPLEMENTED).json(&json!({
        "error": "not_implemented",
        "detail": "migration plan/status/rollback endpoints are Phase 2"
    }))
}

/// The correlation id for this request, stamped onto the authz audit row.
///
/// Honours an inbound `x-request-id` so the id in our audit matches the one the
/// caller and the rest of the platform already know the request by; mints one
/// only when the caller sent none. Minting unconditionally would produce an
/// audit trail whose ids appear in no other log. Mirrors the control plane's
/// `request_id` helper so the two services agree on the identifier.
fn request_id(req: &web::HttpRequest) -> String {
    req.headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string())
}

fn bearer_token(req: &web::HttpRequest) -> Option<&str> {
    let header = req
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    zeroship_core::auth::extract_bearer(header)
}

/// Resolve one source identity for both authorization and mutation throttling.
///
/// With proxy trust disabled, an inbound forwarding header is ignored. With it
/// enabled, the rightmost usable address is the one-hop proxy's claim about its
/// peer; the client-controlled left edge is never trusted.
fn source_ip(req: &web::HttpRequest, trust_proxy: bool) -> Option<std::net::IpAddr> {
    let forwarded_for = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok());
    zeroship_core::client_ip::resolve_client_ip(
        forwarded_for,
        req.peer_addr().map(|address| address.ip()),
        trust_proxy,
    )
}

fn mutation_rate_limit_response(
    decision: Result<RateLimitDecision, String>,
    source_ip: Option<std::net::IpAddr>,
) -> Option<web::HttpResponse> {
    match decision {
        Ok(RateLimitDecision::Allowed) => None,
        Ok(RateLimitDecision::Throttled(limited)) => Some(
            web::HttpResponse::TooManyRequests()
                .header("retry-after", retry_after_header(limited.retry_after_secs))
                .json(&json!({"error": "rate_limited"})),
        ),
        Err(error) => {
            tracing::error!(
                error = %error,
                source_ip = ?source_ip,
                "migrated: shared mutation rate-limit consume failed"
            );
            Some(
                web::HttpResponse::ServiceUnavailable()
                    .header("retry-after", "1")
                    .json(&json!({"error": "rate_limit_unavailable"})),
            )
        }
    }
}

fn retry_after_header(seconds: f64) -> String {
    if seconds.is_finite() {
        format!("{:.0}", seconds.ceil().clamp(1.0, 3600.0))
    } else {
        "60".to_string()
    }
}

fn auth_error_response(err: AuthError) -> web::HttpResponse {
    match err {
        AuthError::Unauthorized => {
            web::HttpResponse::Unauthorized().json(&json!({"error": "unauthenticated"}))
        }
        AuthError::Forbidden => web::HttpResponse::Forbidden().json(&json!({"error": "forbidden"})),
        AuthError::Infrastructure(detail) => {
            tracing::error!(error = %detail, "migrated: auth infrastructure error");
            web::HttpResponse::InternalServerError().json(&json!({"error": "auth_error"}))
        }
    }
}

fn apply_error_response(err: ApplyRequestError) -> web::HttpResponse {
    let (status, kind) = apply_error_kind(&err);
    if status.is_server_error() {
        tracing::error!(error = %err, "migrated: migration apply failed");
        return web::HttpResponse::build(status).json(&json!({
            "error": kind,
            "detail": "migration service unavailable",
        }));
    } else {
        tracing::debug!(error = %err, "migrated: migration request rejected");
    }
    // THE REMEDY NAMES THE CALL, and only where there is one to name. A
    // database with no schema is waiting on the cluster reconciler, which no
    // creator request can hurry, so that refusal carries none.
    if let ApplyRequestError::DatabaseNotBound { database_id, .. } = &err {
        return web::HttpResponse::build(status).json(&json!({
            "error": kind,
            "detail": err.to_string(),
            "remedy": format!("POST /api/databases/{}/bindings", database_id.as_str()),
        }));
    }
    // `migration_id` and `gated_versions` used to ride along here, and both existed
    // only for the approval refusals: the id so an operator could approve that row,
    // the version list so they knew what they were approving. With no approval
    // endpoint there is no row to name and no decision to inform, so a body that
    // still carried them would be describing an action the caller cannot take.
    web::HttpResponse::build(status).json(&json!({
        "error": kind,
        "detail": err.to_string(),
    }))
}
