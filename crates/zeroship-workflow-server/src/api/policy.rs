use super::{authorization, read_json, respond};
use crate::{coordinator::Error, SharedState};
use ntex::web::{self, types::State};
use std::time::Duration;
use zeroship_core::{service_identity::endpoints, workflow_policy::PolicyLeaseRequest};
use zeroship_workflow_manager::Error as NativeError;

pub fn configure(config: &mut web::ServiceConfig) {
    config.service(
        web::resource(endpoints::WORKFLOW_POLICY_LEASE.path_template())
            .route(web::post().to(lease)),
    );
}

async fn lease(
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
                    .worker(authorization(&request), endpoints::WORKFLOW_POLICY_LEASE),
            )
            .await
            .map_err(|_| Error::Unavailable)??;
            let command: PolicyLeaseRequest = read_json(&request, body).await?;
            let source = state.policy_source.as_ref().ok_or(Error::Unavailable)?;
            let signing_key = actor.signing_key_id();
            state
                .service
                .manager
                .policy_lease(
                    actor.id(),
                    &signing_key,
                    &command,
                    source.as_ref(),
                    || async {
                        state.auth.revalidate_worker(&actor).await.map_err(|error| {
                            if error == Error::Denied {
                                NativeError::Denied
                            } else {
                                NativeError::Unavailable
                            }
                        })
                    },
                )
                .await?
                .lease()
                .map_err(Error::from)
        }
        .await,
    )
}
