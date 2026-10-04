//! The creator-facing run calls, answered from this service's own journal.

use super::{authorization, read_json, LocatePayload};
use crate::{coordinator::Error, SharedState};
use ntex::{
    http::StatusCode,
    web::{self, types::State},
};
use serde::Serialize;
use std::time::Duration;
use zeroship_core::{
    app_id::AppId,
    service_identity::{endpoints, ServiceEndpoint},
    workflow_coordination::{
        ReadStepOutput, RestartRun, RunFailure, RunScope, SignalRun, StartRun, StepOutputLocation,
        TransitionRun,
    },
    workflow_policy::MAX_INPUT_BYTES_CEILING,
};
use zeroship_workflow::{
    service::{stage_start_input, AppWorkflows, StepOutput},
    WorkflowServiceError,
};

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
            web::resource(endpoints::WORKFLOW_RUN_START.path_template())
                // Carries a creator value, like `signal`, and answers to the
                // same platform ceiling: a run's input is what
                // `max_input_bytes` governs.
                .state(web::types::JsonConfig::default().limit(RUN_CALL_BODY_BYTES))
                .route(web::post().to(start)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_RUN_STATUS.path_template())
                .route(web::post().to(status)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_RUN_SIGNAL.path_template())
                // Carries a creator value, so it needs more than the
                // service-wide metadata budget.
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
        )
        .service(
            web::resource(endpoints::WORKFLOW_RUN_STEP_OUTPUT.path_template())
                .route(web::post().to(step_output)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_RUN_OUTPUT.path_template())
                .route(web::post().to(output)),
        );
}

/// Resolve the app this call may act for, and bind it to the journal.
///
/// THE ZONE IS NEVER READ FROM THE BODY. It is the zone frozen on the instance
/// row whose key verified the request. The body names only an app; whether
/// this worker may act for it is the service's answer, not the caller's
/// assertion.
///
/// THE QUEUE FENCES BEFORE ANYTHING IS OBSERVED. Observing an app reads
/// Control and writes this service's policy ledger, and Control answers an
/// unknown app differently from a known one, so a caller who could make this
/// service observe any id it named would learn which ids exist in other zones
/// and leave a ledger row for each. The app must first hold a queue scope in
/// the caller's zone - this queue's own frozen copy of the app's zone - and an
/// app with none, unknown or foreign, is refused `PermissionDenied` with
/// nothing read from Control and nothing written. Only then is the app
/// observed, and the observation refuses a deleted app to every zone.
async fn bind(
    request: &web::HttpRequest,
    state: &SharedState,
    endpoint: ServiceEndpoint,
    app: &AppId,
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
        .queue()
        .require_scope_in_zone(app, actor.zone())
        .await
        .map_err(|error| refused(Error::from(error)))?;
    let source = state
        .policy_source
        .as_ref()
        .ok_or(RunFailure::Unavailable {})?;
    // The observation is taken once for the zone decision and reused by the
    // journal binding below; `ControlPolicies` caches it, so this is the same
    // observation `RunService::app` installs.
    source
        .observe(app)
        .await
        .map_err(|error| refused(Error::from(error)))?
        .admits_zone(actor.zone())
        .map_err(|error| refused(Error::from(error)))?;
    state
        .runs
        .app(source.as_ref(), app)
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
        WorkflowServiceError::Internal(_)
            | WorkflowServiceError::InvalidResponse(_)
            | WorkflowServiceError::Unavailable(_)
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
        WorkflowServiceError::InvalidResponse(_) | WorkflowServiceError::Internal(_) => {
            RunFailure::Internal {}
        }
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

/// Admit a run from the creator's own value, staging that value into this
/// service's store first.
///
/// STAGING COMES FIRST, and it is object I/O outside the admitting transaction,
/// which is why it precedes the call that takes the app lock. One identity
/// covers both: the request id keys the object and the start receipt, so a
/// retried start restages to the same object and replays the same reply.
///
/// The descriptor is composed HERE, from the bytes this process serialized. It is
/// never read from the body, and `CreatorStartOptions` has no field that could
/// carry one, so no caller can name an object it did not supply the bytes for.
async fn start(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let command: StartRun = read_json(&request, body).await.map_err(refused)?;
            let api = bind(
                &request,
                &state,
                endpoints::WORKFLOW_RUN_START,
                &command.app_id,
            )
            .await?;
            let staged = stage_start_input(
                &state.payloads,
                &api,
                &command.request_id,
                &command.input,
                api.captured_authority()
                    .map_err(|error| refusal(&error))?
                    .policy()
                    .max_input_bytes,
            )
            .await
            .map_err(|error| refusal(&error))?;
            api.start(
                &command.request_id,
                &command.workflow_name,
                command.options.with_input(staged),
            )
            .await
            .map_err(|error| refusal(&error))
        }
        .await,
    )
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
                &command.app_id,
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
                &command.app_id,
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
                &command.app_id,
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
                &command.app_id,
            )
            .await?;
            api.restart(&command.request_id, command.run_id.as_str(), command.options)
                .await
                .map_err(|error| refusal(&error))
        }
        .await,
    )
}

/// Locate what one completed step recorded, without opening it.
///
/// The journal lookup crosses and the bytes do not. `read_step_output` proves
/// under the run lock which object the step owns and compares the row's
/// descriptor against the recorded one; `LocatePayload` then answers with the key
/// and that descriptor instead of a body, so this reply stays small whatever the
/// step returned. An output the journal kept inline was never an object, and the
/// value is what locates it.
async fn step_output(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let command: ReadStepOutput = read_json(&request, body).await.map_err(refused)?;
            let api = bind(
                &request,
                &state,
                endpoints::WORKFLOW_RUN_STEP_OUTPUT,
                &command.app_id,
            )
            .await?;
            let located = api
                .read_step_output(
                    command.run_id.as_str(),
                    &command.name,
                    command.occurrence,
                    LocatePayload,
                )
                .await
                .map_err(|error| refusal(&error))?;
            Ok(match located {
                StepOutput::Object(payload) => StepOutputLocation::Object { payload },
                StepOutput::Inline(value) => StepOutputLocation::Inline { value },
            })
        }
        .await,
    )
}

/// Locate what a settled run returned, without opening it.
///
/// Same split as `step_output`, minus the inline arm: a run's own output is always
/// an object, so a run that returned nothing owns none here and is reported as
/// missing -- the same absence `status` reports by carrying no descriptor.
async fn output(
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
                endpoints::WORKFLOW_RUN_OUTPUT,
                &command.app_id,
            )
            .await?;
            api.read_output(command.run_id.as_str(), LocatePayload)
                .await
                .map_err(|error| refusal(&error))
        }
        .await,
    )
}
