use std::sync::Arc;

use ntex::web;
use ntex::web::types::{Json, Path, State};
use serde_json::json;
use uuid::Uuid;
use zeroship_authn::rate_limit::RateLimitDecision;
use zeroship_authz::Action;
use zeroship_core::DatabaseId;

use crate::apply::{
    apply_error_kind, apply_ir_documents, ApplyMigrationsRequest, ApplyRequestError,
};
use crate::auth::AuthError;
use crate::MigrationServiceState;

/// JSON extractor budget for a migration apply request.
///
/// The CLI posts its generated IR envelope verbatim, so this route needs an
/// explicit budget above Ntex's small default while retaining a bounded body.
const APPLY_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;

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
        web::resource("/v1/databases/{database_id}/migrations/apply")
            .state(web::types::JsonConfig::default().limit(APPLY_REQUEST_BODY_BYTES))
            .route(web::post().to(apply)),
    )
    .service(web::resource("/healthz").route(web::get().to(healthz)))
    .service(web::resource("/readyz").route(web::get().to(readyz)));
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
            match state.control_plane.probe().await {
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
/// # THERE IS NO APP IN THIS REQUEST
///
/// A migration is not an app operation. The DATABASE is the target - its schema
/// is what the DDL writes - and it is also the authorization subject: the
/// bearer is checked for [`Action::DatabaseMigrate`] at
/// [`zeroship_authz::Resource::Database`], which resolves to a qualifying seat
/// on the project that owns the database. That is the same authority that
/// created it, so a database with no app bound to it is migratable, which is
/// the state `zeroship db create` leaves behind and the reason an app in this
/// path was a coupling rather than a fence.
///
/// The target rides in the path rather than the body because the CLI posts the
/// recorded migration set VERBATIM - it does not parse or rewrite what the
/// recorder handed it, and a target it had to splice in would be a target it
/// could get wrong.
///
/// The segment is typed, so a uuid-rendered id or an `app_` where a `dbs_`
/// belongs is a 404 from the extractor rather than a request that authorizes
/// against something the roster does not hold.
pub async fn apply(
    req: web::HttpRequest,
    state: State<Arc<MigrationServiceState>>,
    path: Path<DatabaseId>,
    body: Json<ApplyMigrationsRequest>,
) -> web::HttpResponse {
    let database_id = path.into_inner();
    let caller = match authorize_mutation(&req, &state, &database_id).await {
        Ok(caller) => caller,
        Err(response) => return response,
    };

    match apply_ir_documents(
        &state.provision_dsn,
        &state.tmp_dir,
        &database_id,
        &body,
        &state.policy_config,
        &caller.principal_id,
    )
    .await
    {
        Ok(report) => {
            tracing::info!(
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

async fn authorize_mutation(
    req: &web::HttpRequest,
    state: &MigrationServiceState,
    database_id: &DatabaseId,
) -> Result<crate::auth::VerifiedCaller, web::HttpResponse> {
    let Some(token) = bearer_token(req) else {
        return Err(web::HttpResponse::Unauthorized().json(&json!({"error": "unauthenticated"})));
    };
    let request_id = request_id(req);
    let source_ip = source_ip(req, state.trust_proxy);
    let caller = match state
        .authenticator
        .verify_action(
            token,
            database_id,
            Action::DatabaseMigrate,
            source_ip,
            &request_id,
        )
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
    // The request carries neither `migration_id` nor `gated_versions`: with no approval
    // endpoint there is no row to name and no decision to inform, so a body carrying them
    // would describe an action the caller cannot take.
    web::HttpResponse::build(status).json(&json!({
        "error": kind,
        "detail": err.to_string(),
    }))
}
