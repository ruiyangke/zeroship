use crate::SharedState;
use ntex::{
    http::StatusCode,
    web::{
        self,
        types::{Json, Path, State},
    },
};
use serde::Serialize;
use zeroship_core::{app_id::AppId, service_identity::endpoints};
use zeroship_workflow::{
    operations::{RestartOptions, RunOperation, SignalOptions, StartOptions},
    service::{
        capability::{AppOperation, SignalTarget},
        wire::{CompleteTask, Failure, Mutation, PollTask, TaskCredential},
        DeployRegistration, SignalTokenRequest,
    },
    WorkflowServiceError,
};

pub fn configure(config: &mut web::ServiceConfig) {
    config
        .state(web::types::JsonConfig::default().limit(1024 * 1024))
        .service(web::resource("/healthz").route(web::get().to(health)))
        .service(
            web::resource("/v1/apps/{app_id}/workflows/{name}/runs").route(web::post().to(start)),
        )
        .service(
            web::resource("/v1/apps/{app_id}/workflow-runs/{run_id}").route(web::get().to(status)),
        )
        .service(
            web::resource("/v1/apps/{app_id}/workflow-runs/{run_id}/signals")
                .route(web::post().to(signal)),
        )
        .service(
            web::resource("/v1/apps/{app_id}/workflow-runs/{run_id}/transition")
                .route(web::post().to(transition)),
        )
        .service(
            web::resource("/v1/apps/{app_id}/workflow-runs/{run_id}/restart")
                .route(web::post().to(restart)),
        )
        .service(
            web::resource("/v1/apps/{app_id}/workflow-topics/{topic}")
                .route(web::post().to(broadcast)),
        )
        .service(
            web::resource("/v1/apps/{app_id}/workflow-signal-tokens")
                .route(web::post().to(issue_signal_token)),
        )
        .service(
            web::resource("/v1/apps/{app_id}/workflow-signal-tokens/revoke")
                .route(web::post().to(revoke_signal_tokens)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_DEPLOY.path_template())
                .route(web::post().to(activate_deploy)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_TASK_POLL.path_template())
                .route(web::post().to(poll)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_TASK_HEARTBEAT.path_template())
                .route(web::post().to(heartbeat)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_TASK_COMPLETE.path_template())
                .route(web::post().to(complete)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_TASK_RELEASE.path_template())
                .route(web::post().to(release)),
        );
}

fn authorization(request: &web::HttpRequest) -> Option<&str> {
    request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
}
fn app_id(id: &str) -> Result<AppId, WorkflowServiceError> {
    AppId::parse(id)
        .map_err(|_| WorkflowServiceError::InvalidRequest("invalid workflow app identity".into()))
}
pub(crate) fn respond<T: Serialize>(result: Result<T, WorkflowServiceError>) -> web::HttpResponse {
    match result {
        Ok(value) => web::HttpResponse::Ok().json(&value),
        Err(error) => failure(error),
    }
}
pub(crate) fn failure(error: WorkflowServiceError) -> web::HttpResponse {
    let status = match &error {
        WorkflowServiceError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
        WorkflowServiceError::Unauthenticated => StatusCode::UNAUTHORIZED,
        WorkflowServiceError::PermissionDenied => StatusCode::FORBIDDEN,
        WorkflowServiceError::NotFound(_) => StatusCode::NOT_FOUND,
        WorkflowServiceError::Conflict(_) => StatusCode::CONFLICT,
        WorkflowServiceError::ResourceExhausted(_) => StatusCode::TOO_MANY_REQUESTS,
        WorkflowServiceError::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        WorkflowServiceError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        WorkflowServiceError::Timeout => StatusCode::GATEWAY_TIMEOUT,
        WorkflowServiceError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    if status.is_server_error() {
        tracing::warn!(code = error.code(), "workflow request failed");
    }
    web::HttpResponse::build(status).json(&Failure::from_error(&error))
}
async fn health() -> web::HttpResponse {
    web::HttpResponse::Ok().finish()
}

async fn start(
    request: web::HttpRequest,
    state: State<SharedState>,
    path: Path<(String, String)>,
    body: JsonBody<Mutation<StartOptions>>,
) -> web::HttpResponse {
    respond(
        async {
            let (app, name) = path.into_inner();
            let app = app_id(&app)?;
            state
                .auth
                .app(authorization(&request), &app, AppOperation::Start)?;
            let body = body.map_err(json_error)?.into_inner();
            state
                .service
                .for_app(app)
                .start(&body.request_id, &name, body.options)
                .await
        }
        .await,
    )
}
async fn status(
    request: web::HttpRequest,
    state: State<SharedState>,
    path: Path<(String, String)>,
) -> web::HttpResponse {
    respond(
        async {
            let (app, run) = path.into_inner();
            let app = app_id(&app)?;
            state
                .auth
                .app(authorization(&request), &app, AppOperation::Status)?;
            state.service.for_app(app).status(&run).await
        }
        .await,
    )
}
async fn signal(
    request: web::HttpRequest,
    state: State<SharedState>,
    path: Path<(String, String)>,
    body: JsonBody<Mutation<SignalOptions>>,
) -> web::HttpResponse {
    respond(
        async {
            let (app, run) = path.into_inner();
            let app = app_id(&app)?;
            state
                .auth
                .app(authorization(&request), &app, AppOperation::Signal)?;
            let body = body.map_err(json_error)?.into_inner();
            state
                .service
                .for_app(app)
                .signal(&body.request_id, &run, body.options)
                .await
        }
        .await,
    )
}
async fn transition(
    request: web::HttpRequest,
    state: State<SharedState>,
    path: Path<(String, String)>,
    body: JsonBody<Mutation<RunOperation>>,
) -> web::HttpResponse {
    respond(
        async {
            let (app, run) = path.into_inner();
            let app = app_id(&app)?;
            state
                .auth
                .app(authorization(&request), &app, AppOperation::Control)?;
            let body = body.map_err(json_error)?.into_inner();
            state
                .service
                .for_app(app)
                .transition(&body.request_id, &run, body.options)
                .await
        }
        .await,
    )
}
async fn restart(
    request: web::HttpRequest,
    state: State<SharedState>,
    path: Path<(String, String)>,
    body: JsonBody<Mutation<RestartOptions>>,
) -> web::HttpResponse {
    respond(
        async {
            let (app, run) = path.into_inner();
            let app = app_id(&app)?;
            state
                .auth
                .app(authorization(&request), &app, AppOperation::Restart)?;
            let body = body.map_err(json_error)?.into_inner();
            state
                .service
                .for_app(app)
                .restart(&body.request_id, &run, body.options)
                .await
        }
        .await,
    )
}
async fn broadcast(
    request: web::HttpRequest,
    state: State<SharedState>,
    path: Path<(String, String)>,
    body: JsonBody<Mutation<SignalOptions>>,
) -> web::HttpResponse {
    respond(
        async {
            let (app, topic) = path.into_inner();
            let app = app_id(&app)?;
            state
                .auth
                .app(authorization(&request), &app, AppOperation::Broadcast)?;
            let body = body.map_err(json_error)?.into_inner();
            state
                .service
                .for_app(app)
                .broadcast(&body.request_id, &topic, body.options)
                .await
        }
        .await,
    )
}
async fn issue_signal_token(
    request: web::HttpRequest,
    state: State<SharedState>,
    path: Path<String>,
    body: JsonBody<Mutation<SignalTokenRequest>>,
) -> web::HttpResponse {
    respond(
        async {
            let app = app_id(&path.into_inner())?;
            state.auth.app(
                authorization(&request),
                &app,
                AppOperation::IssueSignalToken,
            )?;
            let body = body.map_err(json_error)?.into_inner();
            state
                .service
                .for_app(app)
                .issue_signal_token(&body.request_id, body.options)
                .await
        }
        .await,
    )
}
async fn revoke_signal_tokens(
    request: web::HttpRequest,
    state: State<SharedState>,
    path: Path<String>,
    body: JsonBody<Mutation<Option<SignalTarget>>>,
) -> web::HttpResponse {
    respond(
        async {
            let app = app_id(&path.into_inner())?;
            state.auth.app(
                authorization(&request),
                &app,
                AppOperation::RevokeSignalTokens,
            )?;
            let body = body.map_err(json_error)?.into_inner();
            state
                .service
                .for_app(app)
                .revoke_signal_tokens(&body.request_id, body.options)
                .await
        }
        .await,
    )
}
async fn activate_deploy(
    request: web::HttpRequest,
    state: State<SharedState>,
    path: Path<String>,
    body: JsonBody<DeployRegistration>,
) -> web::HttpResponse {
    respond(
        async {
            state
                .auth
                .peer(authorization(&request), endpoints::WORKFLOW_DEPLOY)
                .await?;
            let app = app_id(&path.into_inner())?;
            state
                .service
                .activate_deploy(&app, &body.map_err(json_error)?.into_inner())
                .await
        }
        .await,
    )
}
async fn poll(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: JsonBody<PollTask>,
) -> web::HttpResponse {
    respond(
        async {
            let worker = state
                .auth
                .worker(authorization(&request), endpoints::WORKFLOW_TASK_POLL)
                .await?;
            body.map_err(json_error)?;
            state.service.poll(&worker).await
        }
        .await,
    )
}
async fn heartbeat(
    request: web::HttpRequest,
    state: State<SharedState>,
    path: Path<String>,
    body: JsonBody<TaskCredential>,
) -> web::HttpResponse {
    respond(
        async {
            let worker = state
                .auth
                .worker(authorization(&request), endpoints::WORKFLOW_TASK_HEARTBEAT)
                .await?;
            let body = body.map_err(json_error)?.into_inner();
            state
                .service
                .heartbeat(&worker, &path.into_inner(), &body.token)
                .await
        }
        .await,
    )
}
async fn complete(
    request: web::HttpRequest,
    state: State<SharedState>,
    path: Path<String>,
    body: JsonBody<CompleteTask>,
) -> web::HttpResponse {
    respond(
        async {
            let worker = state
                .auth
                .worker(authorization(&request), endpoints::WORKFLOW_TASK_COMPLETE)
                .await?;
            let body = body.map_err(json_error)?.into_inner();
            state
                .service
                .complete(&worker, &path.into_inner(), &body.token, body.execution)
                .await
        }
        .await,
    )
}
async fn release(
    request: web::HttpRequest,
    state: State<SharedState>,
    path: Path<String>,
    body: JsonBody<TaskCredential>,
) -> web::HttpResponse {
    respond(
        async {
            let worker = state
                .auth
                .worker(authorization(&request), endpoints::WORKFLOW_TASK_RELEASE)
                .await?;
            let body = body.map_err(json_error)?.into_inner();
            state
                .service
                .release(&worker, &path.into_inner(), &body.token)
                .await
        }
        .await,
    )
}

type JsonBody<T> = Result<Json<T>, web::error::JsonPayloadError>;
fn json_error(error: web::error::JsonPayloadError) -> WorkflowServiceError {
    match error {
        web::error::JsonPayloadError::Overflow => WorkflowServiceError::PayloadTooLarge,
        _ => WorkflowServiceError::InvalidRequest("invalid workflow JSON request".into()),
    }
}
