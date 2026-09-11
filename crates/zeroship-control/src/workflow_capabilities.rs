//! App-scoped workflow authority issued to active enrolled worker hosts.

use crate::AppState;
use ntex::web::{
    self,
    types::{Path, State},
};
use std::sync::Arc;
use zeroship_core::{
    app_id::AppId, service_assertion::presented_issuer, service_identity::endpoints,
};
use zeroship_workflow::service::capability::{
    mint_app_capability, AppGrant, AppOperation, IssuedAppCapability,
    APP_CAPABILITY_MAX_LIFETIME_SECONDS,
};

pub fn configure(config: &mut web::ServiceConfig) {
    config.service(
        web::resource(endpoints::CONTROL_WORKFLOW_CAPABILITY.path_template())
            .route(web::post().to(issue)),
    );
}

async fn issue(
    request: web::HttpRequest,
    state: State<Arc<AppState>>,
    app: Path<String>,
) -> web::HttpResponse {
    let header = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok());
    // Enrollment credentials identify a role but are not serving credentials.
    // The existing verifier checks this instance against the active registry.
    if presented_issuer(header).is_none_or(|issuer| issuer.instance().is_none()) {
        return web::HttpResponse::Unauthorized().finish();
    }
    if let Some(response) = crate::internal::check_service_auth(
        &request,
        &state,
        endpoints::CONTROL_WORKFLOW_CAPABILITY,
    )
    .await
    {
        return response;
    }
    let app = match AppId::parse(&app.into_inner()) {
        Ok(app) => app,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid app identity"}))
        }
    };
    match state.registry.get_app(&app.uuid()).await {
        Ok(Some(_)) => {}
        Ok(None) => return web::HttpResponse::NotFound().finish(),
        Err(error) => {
            tracing::error!(%error, "workflow capability app lookup failed");
            return web::HttpResponse::ServiceUnavailable().finish();
        }
    }
    let Some((issuer, key)) = state.service_auth.signing_identity() else {
        return web::HttpResponse::ServiceUnavailable().finish();
    };
    let Ok(control_issuer) = crate::internal::control_service_issuer() else {
        return web::HttpResponse::ServiceUnavailable().finish();
    };
    if issuer != &control_issuer {
        return web::HttpResponse::ServiceUnavailable().finish();
    }
    let now = chrono::Utc::now().timestamp();
    let Some(expires_at) = now.checked_add(APP_CAPABILITY_MAX_LIFETIME_SECONDS) else {
        return web::HttpResponse::ServiceUnavailable().finish();
    };
    let grant = AppGrant {
        app_id: app,
        operations: [
            AppOperation::Start,
            AppOperation::List,
            AppOperation::Status,
            AppOperation::Signal,
            AppOperation::Broadcast,
            AppOperation::Control,
            AppOperation::Restart,
            AppOperation::ReadOutput,
            AppOperation::IssueSignalToken,
            AppOperation::RevokeSignalTokens,
        ]
        .into(),
    };
    match mint_app_capability(key, grant, now, APP_CAPABILITY_MAX_LIFETIME_SECONDS) {
        Ok(token) => web::HttpResponse::Ok()
            .header("cache-control", "no-store")
            .json(&IssuedAppCapability { token, expires_at }),
        Err(error) => {
            tracing::error!(%error, "workflow capability signing failed");
            web::HttpResponse::ServiceUnavailable().finish()
        }
    }
}
