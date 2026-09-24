//! The creator-facing run calls, answered from this service's own journal.

use super::{authorization, read_json};
use crate::{coordinator::Error, SharedState};
use ntex::{
    http::StatusCode,
    web::{self, types::State},
};
use serde::Serialize;
use std::time::Duration;
use zeroship_core::{
    service_identity::{endpoints, ServiceEndpoint},
    workflow_coordination::{
        AssignedScope, RestartRun, RunFailure, RunScope, SignalRun, TransitionRun,
        VerifyAssignment,
    },
    workflow_policy::MAX_INPUT_BYTES_CEILING,
};
use zeroship_workflow::{service::AppWorkflows, WorkflowServiceError};

/// JSON extractor budget for a run call that carries a creator value.
///
/// A signal payload answers to `AppPolicy::max_input_bytes`, which the platform
/// refuses above `MAX_INPUT_BYTES_CEILING`, so the budget is derived from that
/// ceiling rather than chosen. The headroom is the envelope around the value --
/// the scope, the run and the request identity -- which is bounded and small,
/// and it is added rather than assumed because the bound governs the VALUE
/// while this governs the whole message.
const RUN_CALL_BODY_BYTES: usize = MAX_INPUT_BYTES_CEILING + 8 * 1024;

pub fn configure(config: &mut web::ServiceConfig) {
    config
        .service(
            web::resource(endpoints::WORKFLOW_RUN_STATUS.path_template())
                .route(web::post().to(status)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_RUN_SIGNAL.path_template())
                // The only run call that carries a creator value, and the only
                // one that needs more than the service-wide metadata budget.
                .state(web::types::JsonConfig::default().limit(RUN_CALL_BODY_BYTES))
                .route(web::post().to(signal)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_RUN_TRANSITION.path_template())
                .route(web::post().to(transition)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_RUN_RESTART.path_template())
                .route(web::post().to(restart)),
        );
}

/// Resolve the app this call may act for, and bind it to the journal.
///
/// THE WORKER IS NEVER READ FROM THE BODY. It is the instance whose key
/// verified the request, substituted into the selector in place of anything a
/// body could claim, exactly as the manager's own placement lookup does. The
/// body names an app and a placement revision; whether this worker holds that
/// placement is the manager's answer, not the caller's assertion.
async fn bind(
    request: &web::HttpRequest,
    state: &SharedState,
    endpoint: ServiceEndpoint,
    scope: &AssignedScope,
) -> Result<AppWorkflows, RunFailure> {
    let actor = compio::time::timeout(
        Duration::from_secs(5),
        state.auth.worker(authorization(request), endpoint),
    )
    .await
    .map_err(|_| RunFailure::Unavailable {})?
    .map_err(refused)?;
    state
        .service
        .manager
        .verify_assignment(&VerifyAssignment {
            app_id: scope.app_id.clone(),
            worker_id: actor.id().clone(),
            assignment_revision: scope.assignment_revision,
        })
        .await
        .map_err(|error| refused(Error::from(error)))?;
    let source = state
        .policy_source
        .as_ref()
        .ok_or(RunFailure::Unavailable {})?;
    state
        .runs
        .app(source.as_ref(), &scope.app_id)
        .await
        .map_err(|error| refusal(&error))
}

/// Carry a coordination refusal into the creator-facing contract.
fn refused(error: Error) -> RunFailure {
    match error {
        Error::Invalid => RunFailure::InvalidRequest {
            message: error.to_string(),
        },
        Error::Unauthenticated => RunFailure::Unauthenticated {},
        Error::RequestTooLarge => RunFailure::PayloadTooLarge {},
        Error::Denied => RunFailure::PermissionDenied {},
        Error::Conflict => RunFailure::Conflict {
            message: error.to_string(),
        },
        Error::Capacity => RunFailure::ResourceExhausted {
            message: error.to_string(),
        },
        Error::Unavailable => RunFailure::Unavailable {},
    }
}

/// Convert an engine refusal into the creator-facing wire refusal.
///
/// Two arms name a host condition a creator cannot act on, so their wording is
/// replaced and the operator gets the original here. This is the same contract
/// `to_op_error` in `crates/zeroship-workflow-v8/src/error.rs` holds on the
/// in-process path; what makes it structural rather than a discipline to
/// remember is that `RunFailure` gives those two arms nowhere to put a message.
///
/// The match is wildcard-free, so a new engine refusal stops compiling here
/// until it is given a wire arm rather than being folded into a neighbour.
fn refusal(error: &WorkflowServiceError) -> RunFailure {
    if matches!(
        error,
        WorkflowServiceError::Internal(_) | WorkflowServiceError::Unavailable(_)
    ) {
        tracing::warn!(
            code = error.code(),
            detail = %error,
            "workflow refusal reported opaquely to a creator call"
        );
    }
    match error {
        WorkflowServiceError::InvalidRequest(message) => RunFailure::InvalidRequest {
            message: message.clone(),
        },
        WorkflowServiceError::Unauthenticated => RunFailure::Unauthenticated {},
        WorkflowServiceError::PermissionDenied => RunFailure::PermissionDenied {},
        WorkflowServiceError::NotFound(message) => RunFailure::NotFound {
            message: message.clone(),
        },
        WorkflowServiceError::Conflict(message) => RunFailure::Conflict {
            message: message.clone(),
        },
        WorkflowServiceError::ResourceExhausted(message) => RunFailure::ResourceExhausted {
            message: message.clone(),
        },
        WorkflowServiceError::PayloadTooLarge => RunFailure::PayloadTooLarge {},
        WorkflowServiceError::Timeout => RunFailure::Timeout {},
        WorkflowServiceError::IngressFenced(after) => RunFailure::IngressFenced { after: *after },
        WorkflowServiceError::Unavailable(_) => RunFailure::Unavailable {},
        WorkflowServiceError::Internal(_) => RunFailure::Internal {},
    }
}

/// A refusal is carried by the status its own code pairs with, so the client
/// can refuse a peer whose status and body disagree.
fn respond<T: Serialize>(result: Result<T, RunFailure>) -> web::HttpResponse {
    match result {
        Ok(value) => web::HttpResponse::Ok().json(&value),
        Err(failure) => {
            let status = StatusCode::from_u16(failure.status())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            web::HttpResponse::build(status).force_close().json(&failure)
        }
    }
}

async fn status(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let command: RunScope = read_json(&request, body).await.map_err(refused)?;
            let api = bind(
                &request,
                &state,
                endpoints::WORKFLOW_RUN_STATUS,
                &command.scope,
            )
            .await?;
            api.status(command.run_id.as_str())
                .await
                .map_err(|error| refusal(&error))
        }
        .await,
    )
}

async fn signal(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let command: SignalRun = read_json(&request, body).await.map_err(refused)?;
            let api = bind(
                &request,
                &state,
                endpoints::WORKFLOW_RUN_SIGNAL,
                &command.scope,
            )
            .await?;
            api.signal(&command.request_id, command.run_id.as_str(), command.options)
                .await
                .map_err(|error| refusal(&error))
        }
        .await,
    )
}

async fn transition(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let command: TransitionRun = read_json(&request, body).await.map_err(refused)?;
            let api = bind(
                &request,
                &state,
                endpoints::WORKFLOW_RUN_TRANSITION,
                &command.scope,
            )
            .await?;
            api.transition(
                &command.request_id,
                command.run_id.as_str(),
                command.operation,
            )
            .await
            .map_err(|error| refusal(&error))
        }
        .await,
    )
}

async fn restart(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let command: RestartRun = read_json(&request, body).await.map_err(refused)?;
            let api = bind(
                &request,
                &state,
                endpoints::WORKFLOW_RUN_RESTART,
                &command.scope,
            )
            .await?;
            api.restart(&command.request_id, command.run_id.as_str(), command.options)
                .await
                .map_err(|error| refusal(&error))
        }
        .await,
    )
}
