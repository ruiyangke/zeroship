use super::{authorization, read_json, respond};
use crate::{auth::VerifiedWorker, coordinator::Error, SharedState};
use ntex::web::{self, types::State};
use std::time::Duration;
use zeroship_core::{
    service_identity::{endpoints, ServiceEndpoint},
    workflow_coordination::{AssignedScope, WorkerId},
    workflow_jobs::{Delivery, Settlement, SubmitJob},
};
use zeroship_workflow_manager::Error as NativeError;

pub fn configure(config: &mut web::ServiceConfig) {
    config
        .service(
            web::resource(endpoints::WORKFLOW_JOB_SUBMIT.path_template())
                .route(web::post().to(submit)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_JOB_CLAIM.path_template())
                .route(web::post().to(claim)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_JOB_HEARTBEAT.path_template())
                .route(web::post().to(heartbeat)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_JOB_SETTLE.path_template())
                .route(web::post().to(settle)),
        );
}

async fn authenticate(
    request: &web::HttpRequest,
    state: &SharedState,
    endpoint: ServiceEndpoint,
) -> Result<VerifiedWorker, Error> {
    compio::time::timeout(
        Duration::from_secs(5),
        state.auth.worker(authorization(request), endpoint),
    )
    .await
    .map_err(|_| Error::Unavailable)?
}

async fn revalidate(state: &SharedState, actor: &VerifiedWorker) -> Result<WorkerId, NativeError> {
    state.auth.revalidate_worker(actor).await.map_err(|error| {
        if error == Error::Denied {
            NativeError::Denied
        } else {
            NativeError::Unavailable
        }
    })
}

async fn submit(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let actor = authenticate(&request, &state, endpoints::WORKFLOW_JOB_SUBMIT).await?;
            let command: SubmitJob = read_json(&request, body).await?;
            state
                .service
                .manager
                .submit_job(actor.id(), &command, || revalidate(&state, &actor))
                .await
                .map_err(Error::from)
        }
        .await,
    )
}

async fn claim(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let actor = authenticate(&request, &state, endpoints::WORKFLOW_JOB_CLAIM).await?;
            let command: AssignedScope = read_json(&request, body).await?;
            state
                .service
                .manager
                .claim_job(actor.id(), &command, || revalidate(&state, &actor))
                .await?
                .map(|grant| grant.lease())
                .transpose()
                .map_err(Error::from)
        }
        .await,
    )
}

async fn heartbeat(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let actor = authenticate(&request, &state, endpoints::WORKFLOW_JOB_HEARTBEAT).await?;
            let command: Delivery = read_json(&request, body).await?;
            state
                .service
                .manager
                .heartbeat_job(actor.id(), &command, || revalidate(&state, &actor))
                .await?
                .lease()
                .map_err(Error::from)
        }
        .await,
    )
}

async fn settle(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let actor = authenticate(&request, &state, endpoints::WORKFLOW_JOB_SETTLE).await?;
            let command: Settlement = read_json(&request, body).await?;
            state
                .service
                .manager
                .settle_job(actor.id(), &command, || revalidate(&state, &actor))
                .await
                .map_err(Error::from)
        }
        .await,
    )
}
