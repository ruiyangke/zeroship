use super::*;
use futures::StreamExt;
use zeroship_storage::backend::{ChunkResult, ChunkSource};
use zeroship_workflow::{
    engine::WorkflowOutputRef,
    service::{
        wire::{
            ReadAppPayload, ReadTaskPayload, PAYLOAD_HEADER, REQUEST_ID_HEADER, TASK_TOKEN_HEADER,
        },
        PayloadRead, RequestId, TaskToken,
    },
};

pub(super) fn configure(config: &mut web::ServiceConfig) {
    config
        .service(
            web::resource(endpoints::WORKFLOW_TASK_UPLOAD.path_template())
                .route(web::post().to(upload)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_TASK_READ.path_template())
                .route(web::post().to(read_task)),
        )
        .service(
            web::resource("/v1/apps/{app_id}/workflow-runs/{run_id}/payloads/read")
                .route(web::post().to(read_app)),
        );
}

async fn upload(
    request: web::HttpRequest,
    state: State<SharedState>,
    path: Path<String>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let worker = state
                .auth
                .worker(authorization(&request), endpoints::WORKFLOW_TASK_UPLOAD)
                .await?;
            let token = TaskToken::try_from(header(&request, TASK_TOKEN_HEADER)?.to_owned())?;
            let request_id = RequestId::try_from(header(&request, REQUEST_ID_HEADER)?.to_owned())?;
            let reference: WorkflowOutputRef =
                serde_json::from_str(header(&request, PAYLOAD_HEADER)?).map_err(|_| {
                    WorkflowServiceError::InvalidRequest(
                        "invalid workflow payload descriptor".into(),
                    )
                })?;
            state
                .service
                .stage_payload(
                    &worker,
                    &path.into_inner(),
                    &token,
                    &request_id,
                    reference,
                    Box::new(RequestBody(body)),
                )
                .await
        }
        .await,
    )
}

async fn read_task(
    request: web::HttpRequest,
    state: State<SharedState>,
    path: Path<String>,
    body: JsonBody<ReadTaskPayload>,
) -> web::HttpResponse {
    streaming(
        async {
            let worker = state
                .auth
                .worker(authorization(&request), endpoints::WORKFLOW_TASK_READ)
                .await?;
            let body = body.map_err(json_error)?.into_inner();
            state
                .service
                .read_task_payload(&worker, &path.into_inner(), &body.token, &body.reference)
                .await
        }
        .await,
    )
}

async fn read_app(
    request: web::HttpRequest,
    state: State<SharedState>,
    path: Path<(String, String)>,
    body: JsonBody<ReadAppPayload>,
) -> web::HttpResponse {
    streaming(
        async {
            let (app, run) = path.into_inner();
            let app = app_id(&app)?;
            state
                .auth
                .app(authorization(&request), &app, AppOperation::ReadOutput)?;
            let body = body.map_err(json_error)?.into_inner();
            state
                .service
                .for_app(app)
                .read_payload(&run, body.generation, body.slot)
                .await
        }
        .await,
    )
}

fn header<'a>(request: &'a web::HttpRequest, name: &str) -> Result<&'a str, WorkflowServiceError> {
    request
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            WorkflowServiceError::InvalidRequest("missing workflow payload header".into())
        })
}
fn streaming(result: Result<PayloadRead, WorkflowServiceError>) -> web::HttpResponse {
    let read = match result {
        Ok(read) => read,
        Err(error) => return failure(error),
    };
    let descriptor = match serde_json::to_string(&read.reference) {
        Ok(value) => value,
        Err(_) => {
            return failure(WorkflowServiceError::Internal(
                "encode workflow payload descriptor".into(),
            ))
        }
    };
    let stream = futures::stream::unfold(read.body, |mut body| async move {
        body.next_chunk().await.map(|chunk| {
            (
                chunk.map(|bytes| ntex::util::Bytes::copy_from_slice(&bytes)),
                body,
            )
        })
    });
    web::HttpResponse::Ok()
        .header(PAYLOAD_HEADER, descriptor)
        .header("content-length", read.reference.size.to_string())
        .content_type(
            read.reference
                .content_type
                .as_deref()
                .unwrap_or("application/octet-stream"),
        )
        .streaming(Box::pin(stream))
}

struct RequestBody(web::types::Payload);
#[async_trait::async_trait(?Send)]
impl ChunkSource for RequestBody {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        self.0.next().await.map(|chunk| {
            chunk
                .map(|bytes| bytes::Bytes::copy_from_slice(&bytes))
                .map_err(|_| {
                    zeroship_storage::StorageError::Stream("workflow upload interrupted".into())
                })
        })
    }
}
