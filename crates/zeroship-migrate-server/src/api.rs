use std::sync::Arc;

use ntex::http::StatusCode;
use ntex::web;
use ntex::web::types::{Json, Path, State};
use serde_json::json;
use uuid::Uuid;
use zeroship_authn::rate_limit::RateLimitDecision;
use zeroship_authz::Action;

use crate::apply::{
    apply_error_kind, apply_ir_documents, ApplyMigrationsRequest, ApplyRequestError,
};
use crate::auth::AuthError;
use crate::provisioning::provision_database;
use crate::session::CompioPgSession;
use crate::MigrationServiceState;

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
        web::resource("/v1/databases/{database_id}").route(web::post().to(create_database)),
    )
    .service(web::resource("/v1/apps/{app_id}/migrations/apply").route(web::post().to(apply)))
    .service(web::resource("/v1/apps/{app_id}/migrations/plan").route(web::post().to(stub_phase2)))
    .service(web::resource("/v1/apps/{app_id}/migrations/status").route(web::get().to(stub_phase2)))
    .service(web::resource("/v1/apps/{app_id}/migrations/rollback").route(web::post().to(rollback)))
    .service(web::resource("/healthz").route(web::get().to(healthz)))
    .service(web::resource("/readyz").route(web::get().to(readyz)));
}

/// Explicitly create the data schema and migrator role for an app-derived
/// database id.
///
/// The UUID is still the app id in this pre-rekey step. Keeping the database
/// route now lets the later database-entity change replace only id resolution,
/// without moving lifecycle authority or adding a compatibility route.
pub async fn create_database(
    req: web::HttpRequest,
    state: State<Arc<MigrationServiceState>>,
    database_id: Path<Uuid>,
) -> web::HttpResponse {
    let database_id = database_id.into_inner();
    let caller = match authorize_mutation(&req, &state, database_id).await {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    let session = match CompioPgSession::connect(&state.provision_dsn).await {
        Ok(session) => session,
        Err(error) => {
            tracing::error!(
                error = %error,
                database_id = %database_id,
                "migrate-server: database create connection failed"
            );
            return database_infrastructure_response();
        }
    };
    if let Err(error) = provision_database(session.client(), &database_id.to_string()).await {
        tracing::error!(
            error = %error,
            database_id = %database_id,
            principal_id = %caller.principal_id,
            "migrate-server: database create failed"
        );
        return database_infrastructure_response();
    }

    tracing::info!(
        database_id = %database_id,
        principal_id = %caller.principal_id,
        "migrate-server: database created"
    );
    web::HttpResponse::Ok().json(&json!({"database_id": database_id}))
}

fn database_infrastructure_response() -> web::HttpResponse {
    web::HttpResponse::ServiceUnavailable().json(&json!({
        "error": "database_infrastructure",
        "detail": "database service unavailable",
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

pub async fn apply(
    req: web::HttpRequest,
    state: State<Arc<MigrationServiceState>>,
    app_id: Path<Uuid>,
    body: Json<ApplyMigrationsRequest>,
) -> web::HttpResponse {
    let app_id = app_id.into_inner();
    let caller = match authorize_mutation(&req, &state, app_id).await {
        Ok(caller) => caller,
        Err(response) => return response,
    };

    match apply_ir_documents(
        &state.provision_dsn,
        &state.tmp_dir,
        &app_id,
        &body,
        &state.policy_config,
        &state.schema_apply_store,
        caller.principal_id,
    )
    .await
    {
        Ok(report) => {
            tracing::info!(
                app_id = %app_id,
                principal_id = %caller.principal_id,
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
    app_id: Path<Uuid>,
) -> web::HttpResponse {
    if let Err(response) = authorize_mutation(&req, &state, app_id.into_inner()).await {
        return response;
    }
    stub_phase2().await
}

async fn authorize_mutation(
    req: &web::HttpRequest,
    state: &MigrationServiceState,
    app_id: Uuid,
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
    if let ApplyRequestError::DatabaseNotCreated { database_id } = &err {
        return web::HttpResponse::build(status).json(&json!({
            "error": kind,
            "detail": err.to_string(),
            "remedy": format!("POST /v1/databases/{database_id}"),
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
