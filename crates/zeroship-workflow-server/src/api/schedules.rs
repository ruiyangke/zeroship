//! Schedule publication is a Control capability, independent of worker placement.

use super::{authorization, read_json, respond};
use crate::{coordinator::Error, SharedState};
use ntex::web::{self, types::State};
use std::time::Duration;
use zeroship_core::{
    service_identity::{endpoints, ServiceEndpoint},
    service_peers::{service_issuer, CONTROL_SERVICE_NAME},
    workflow_schedules::{ActivateSchedules, DisableSchedules, RegisterSchedules},
};
use zeroship_workflow_manager::scheduling::{Options, Scheduler};

pub fn configure(config: &mut web::ServiceConfig) {
    config
        .service(
            web::resource(endpoints::WORKFLOW_SCHEDULE_REGISTER.path_template())
                .route(web::post().to(register)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_SCHEDULE_ACTIVATE.path_template())
                .route(web::post().to(activate)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_SCHEDULE_DISABLE.path_template())
                .route(web::post().to(disable)),
        );
}

async fn authenticate(
    request: &web::HttpRequest,
    state: &SharedState,
    endpoint: ServiceEndpoint,
) -> Result<(), Error> {
    let issuer = compio::time::timeout(
        Duration::from_secs(5),
        state.auth.peer(authorization(request), endpoint),
    )
    .await
    .map_err(|_| Error::Unavailable)??;
    let control = service_issuer(CONTROL_SERVICE_NAME).map_err(|_| Error::Unavailable)?;
    if issuer != control {
        return Err(Error::Unauthenticated);
    }
    Ok(())
}

async fn register(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            authenticate(&request, &state, endpoints::WORKFLOW_SCHEDULE_REGISTER).await?;
            let command: RegisterSchedules = read_json(&request, body).await?;
            let scheduler = Scheduler::new(state.service.queue.clone(), Options::default())?;
            scheduler.prepare(&command).await?;
            // Preserve the accepted request's order in the echo. The native
            // store separately canonicalizes metadata for immutable replay.
            Ok(command)
        }
        .await,
    )
}

async fn activate(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            authenticate(&request, &state, endpoints::WORKFLOW_SCHEDULE_ACTIVATE).await?;
            let command: ActivateSchedules = read_json(&request, body).await?;
            Scheduler::new(state.service.queue.clone(), Options::default())?
                .activate(&command)
                .await
                .map_err(Error::from)
        }
        .await,
    )
}

async fn disable(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            authenticate(&request, &state, endpoints::WORKFLOW_SCHEDULE_DISABLE).await?;
            let command: DisableSchedules = read_json(&request, body).await?;
            Scheduler::new(state.service.queue.clone(), Options::default())?
                .disable(&command)
                .await
                .map_err(Error::from)
        }
        .await,
    )
}
