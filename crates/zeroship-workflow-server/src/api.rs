//! Authenticated metadata operations for the workflow coordinator.
#![allow(
    clippy::future_not_send,
    reason = "HTTP handlers and compio pools stay on their runtime thread"
)]

mod jobs;
mod policy;
mod schedules;

use crate::{coordinator::Error, SharedState};
use ntex::{
    http::StatusCode,
    web::{
        self,
        types::{Json, State},
    },
};
use serde::{de::DeserializeOwned, Serialize};
use std::time::Duration;
use zeroship_core::{
    service_identity::endpoints,
    workflow_coordination::{
        AcknowledgeManagement, AssignScope, AssignedScope, Failure, FailureCode, ManageRun,
        ManagementStatus, PublishWakeHint, RegisterWorker, ReleaseScope, ScopePage,
        VerifyAssignment, WorkerPage,
    },
};

pub const DEFAULT_MAX_REQUEST_BYTES: usize = 64 * 1024;
pub fn configure(config: &mut web::ServiceConfig) {
    configure_with_limit(config, DEFAULT_MAX_REQUEST_BYTES);
}
pub fn configure_with_limit(config: &mut web::ServiceConfig, limit: usize) {
    jobs::configure(config);
    policy::configure(config);
    schedules::configure(config);
    config
        .state(web::types::JsonConfig::default().limit(limit))
        .service(web::resource("/healthz").route(web::get().to(health)))
        .service(web::resource("/readyz").route(web::get().to(ready)))
        .service(
            web::resource(endpoints::WORKFLOW_WORKERS.path_template())
                .route(web::post().to(workers)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_ASSIGN.path_template()).route(web::post().to(assign)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_VERIFY_ASSIGNMENT.path_template())
                .route(web::post().to(verify_assignment)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_RECOVERY.path_template())
                .route(web::post().to(recovery)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_MANAGE.path_template()).route(web::post().to(manage)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_MANAGEMENT_STATUS.path_template())
                .route(web::post().to(management_status)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_REGISTER.path_template())
                .route(web::post().to(register)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_ASSIGNMENTS.path_template())
                .route(web::post().to(assignments)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_RENEW.path_template()).route(web::post().to(renew)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_RELEASE.path_template())
                .route(web::post().to(release)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_WAKE.path_template()).route(web::post().to(wake)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_MANAGEMENT_POLL.path_template())
                .route(web::post().to(management_poll)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_MANAGEMENT_ACK.path_template())
                .route(web::post().to(management_ack)),
        )
        .service(web::resource("/{path:.*}").route(web::route().to(not_found)));
}
fn authorization(request: &web::HttpRequest) -> Option<&str> {
    request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
}
fn respond<T: Serialize>(result: Result<T, Error>) -> web::HttpResponse {
    match result {
        Ok(value) => web::HttpResponse::Ok().json(&value),
        Err(error) => {
            let (status, code) = match error {
                Error::Invalid => (StatusCode::BAD_REQUEST, FailureCode::Invalid),
                Error::Unauthenticated => (StatusCode::UNAUTHORIZED, FailureCode::Unauthenticated),
                Error::RequestTooLarge => {
                    (StatusCode::PAYLOAD_TOO_LARGE, FailureCode::RequestTooLarge)
                }
                Error::Denied => (StatusCode::FORBIDDEN, FailureCode::Denied),
                Error::Conflict => (StatusCode::CONFLICT, FailureCode::Conflict),
                Error::Capacity => (StatusCode::TOO_MANY_REQUESTS, FailureCode::Capacity),
                Error::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, FailureCode::Unavailable),
            };
            // Authentication and body-limit errors can leave request bytes
            // unread. Advertise closure so clients cannot reuse that socket.
            web::HttpResponse::build(status)
                .force_close()
                .json(&Failure { code })
        }
    }
}
async fn health() -> web::HttpResponse {
    web::HttpResponse::Ok().finish()
}
async fn not_found() -> web::HttpResponse {
    web::HttpResponse::NotFound().force_close().finish()
}
async fn ready(state: State<SharedState>) -> web::HttpResponse {
    match compio::time::timeout(
        Duration::from_secs(5),
        Box::pin(async {
            state.service.verify().await?;
            state.auth.ready().await
        }),
    )
    .await
    {
        Ok(Ok(())) => web::HttpResponse::Ok().finish(),
        _ => web::HttpResponse::ServiceUnavailable().finish(),
    }
}
async fn read_json<T: DeserializeOwned + 'static>(
    request: &web::HttpRequest,
    body: web::types::Payload,
) -> Result<T, Error> {
    // Authenticate before allocating the buffered metadata body.
    let mut body = body.into_inner();
    compio::time::timeout(
        Duration::from_secs(5),
        <Json<T> as web::FromRequest<web::error::DefaultError>>::from_request(request, &mut body),
    )
    .await
    .map_err(|_| Error::Invalid)?
    .map(Json::into_inner)
    .map_err(|error| match error {
        web::error::JsonPayloadError::Overflow => Error::RequestTooLarge,
        _ => Error::Invalid,
    })
}

async fn workers(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let _actor = compio::time::timeout(
                Duration::from_secs(5),
                state
                    .auth
                    .peer(authorization(&request), endpoints::WORKFLOW_WORKERS),
            )
            .await
            .map_err(|_| Error::Unavailable)??;
            let command: WorkerPage = read_json(&request, body).await?;
            state
                .service
                .manager
                .ready_workers(command.after.as_ref())
                .await
                .map_err(Error::from)
        }
        .await,
    )
}

async fn assign(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let _actor = compio::time::timeout(
                Duration::from_secs(5),
                state
                    .auth
                    .peer(authorization(&request), endpoints::WORKFLOW_ASSIGN),
            )
            .await
            .map_err(|_| Error::Unavailable)??;
            let command: AssignScope = read_json(&request, body).await?;
            state
                .service
                .manager
                .assign(&command)
                .await
                .map_err(Error::from)
        }
        .await,
    )
}

async fn verify_assignment(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let _actor = compio::time::timeout(
                Duration::from_secs(5),
                state.auth.peer(
                    authorization(&request),
                    endpoints::WORKFLOW_VERIFY_ASSIGNMENT,
                ),
            )
            .await
            .map_err(|_| Error::Unavailable)??;
            let command: VerifyAssignment = read_json(&request, body).await?;
            let assignment = state.service.manager.verify_assignment(&command).await?;
            compio::time::timeout(
                Duration::from_secs(5),
                state.auth.active_worker(&command.worker_id),
            )
            .await
            .map_err(|_| Error::Unavailable)??;
            Ok(assignment)
        }
        .await,
    )
}

async fn recovery(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let _actor = compio::time::timeout(
                Duration::from_secs(5),
                state
                    .auth
                    .peer(authorization(&request), endpoints::WORKFLOW_RECOVERY),
            )
            .await
            .map_err(|_| Error::Unavailable)??;
            let command: ScopePage = read_json(&request, body).await?;
            state
                .service
                .manager
                .recovery_scopes(command.after.as_ref())
                .await
                .map_err(Error::from)
        }
        .await,
    )
}

async fn manage(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let actor = compio::time::timeout(
                Duration::from_secs(5),
                state
                    .auth
                    .peer(authorization(&request), endpoints::WORKFLOW_MANAGE),
            )
            .await
            .map_err(|_| Error::Unavailable)??;
            let command: ManageRun = read_json(&request, body).await?;
            state
                .service
                .manager
                .manage(&actor, &command)
                .await
                .map_err(Error::from)
        }
        .await,
    )
}

async fn management_status(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let _actor = compio::time::timeout(
                Duration::from_secs(5),
                state.auth.peer(
                    authorization(&request),
                    endpoints::WORKFLOW_MANAGEMENT_STATUS,
                ),
            )
            .await
            .map_err(|_| Error::Unavailable)??;
            let command: ManagementStatus = read_json(&request, body).await?;
            state
                .service
                .manager
                .management_receipt(&command.app_id, &command.request_id)
                .await
                .map_err(Error::from)
        }
        .await,
    )
}

async fn register(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let actor = compio::time::timeout(
                Duration::from_secs(5),
                state
                    .auth
                    .worker(authorization(&request), endpoints::WORKFLOW_REGISTER),
            )
            .await
            .map_err(|_| Error::Unavailable)??;
            let command: RegisterWorker = read_json(&request, body).await?;
            state
                .service
                .manager
                .register(actor.id(), &command)
                .await
                .map_err(Error::from)
        }
        .await,
    )
}

async fn assignments(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let actor = compio::time::timeout(
                Duration::from_secs(5),
                state
                    .auth
                    .worker(authorization(&request), endpoints::WORKFLOW_ASSIGNMENTS),
            )
            .await
            .map_err(|_| Error::Unavailable)??;
            let command: ScopePage = read_json(&request, body).await?;
            state
                .service
                .manager
                .assignments(actor.id(), command.after.as_ref())
                .await
                .map_err(Error::from)
        }
        .await,
    )
}

async fn renew(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let actor = compio::time::timeout(
                Duration::from_secs(5),
                state
                    .auth
                    .worker(authorization(&request), endpoints::WORKFLOW_RENEW),
            )
            .await
            .map_err(|_| Error::Unavailable)??;
            let command: AssignedScope = read_json(&request, body).await?;
            state
                .service
                .manager
                .renew(actor.id(), &command)
                .await
                .map_err(Error::from)
        }
        .await,
    )
}

async fn release(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let actor = compio::time::timeout(
                Duration::from_secs(5),
                state
                    .auth
                    .worker(authorization(&request), endpoints::WORKFLOW_RELEASE),
            )
            .await
            .map_err(|_| Error::Unavailable)??;
            let command: ReleaseScope = read_json(&request, body).await?;
            state
                .service
                .manager
                .release(actor.id(), &command)
                .await
                .map_err(Error::from)
        }
        .await,
    )
}

async fn wake(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let actor = compio::time::timeout(
                Duration::from_secs(5),
                state
                    .auth
                    .worker(authorization(&request), endpoints::WORKFLOW_WAKE),
            )
            .await
            .map_err(|_| Error::Unavailable)??;
            let command: PublishWakeHint = read_json(&request, body).await?;
            state
                .service
                .manager
                .publish_wake(actor.id(), &command)
                .await
                .map_err(Error::from)
        }
        .await,
    )
}

async fn management_poll(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let actor = compio::time::timeout(
                Duration::from_secs(5),
                state
                    .auth
                    .worker(authorization(&request), endpoints::WORKFLOW_MANAGEMENT_POLL),
            )
            .await
            .map_err(|_| Error::Unavailable)??;
            let command: AssignedScope = read_json(&request, body).await?;
            state
                .service
                .manager
                .pending_management(actor.id(), &command)
                .await
                .map_err(Error::from)
        }
        .await,
    )
}

async fn management_ack(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let actor = compio::time::timeout(
                Duration::from_secs(5),
                state
                    .auth
                    .worker(authorization(&request), endpoints::WORKFLOW_MANAGEMENT_ACK),
            )
            .await
            .map_err(|_| Error::Unavailable)??;
            let command: AcknowledgeManagement = read_json(&request, body).await?;
            state
                .service
                .manager
                .acknowledge_management(actor.id(), &command)
                .await
                .map_err(Error::from)
        }
        .await,
    )
}
