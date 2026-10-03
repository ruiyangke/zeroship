//! Authenticated Control operations over the app deployment catalog: journal and
//! queue retention, and the manifest summary a journal records for a deployment.
//! Customer journals never enter Control.
//!
//! The summary is here rather than in a module of its own because it answers out
//! of the same catalog, under the same role check, through the same handle.
//!
//! Every ledger operation runs on Control's retention executor
//! (`Registry::retention`), which holds a fixed set of sessions for the whole
//! process, so serving threads open none.

#![expect(
    clippy::future_not_send,
    reason = "ORM and HTTP stay on their compio thread"
)]

use crate::{AppState, publication::Catalog};
use ntex::web::{
    self,
    types::{Json, State},
};
use std::{sync::Arc, time::Duration};
use zeroship_core::{
    service_assertion::{ServiceIssuer, presented_issuer},
    service_identity::{AuthError, ServiceEndpoint, endpoints},
    service_peers::{WORKFLOW_SERVICE_NAME, service_issuer},
    workflow_coordination::{Failure, FailureCode},
    workflow_deployments::{HoldGeneration, HoldReceipt, HoldRequest, HoldScope, QueueHoldRequest},
};
use zeroship_workflow::service::{DeployRegistration, DeployRegistrationRequest};
use zeroship_workflow_manager::deployments::{DeploymentHolds, Error as DeploymentError};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// The deployment-hold surface every serving thread shares.
///
/// It holds no database and no HTTP client: the retention executor owns the
/// sessions, and every caller is authorized by the role it authenticated under.
#[derive(Debug)]
pub struct DeploymentHoldApi {
    executor: Catalog,
}

impl DeploymentHoldApi {
    /// Serve holds on `executor`.
    #[must_use]
    pub const fn new(executor: Catalog) -> Self {
        Self { executor }
    }

    /// The workflow declarations of one deployment, for the journal holder.
    ///
    /// Derived on the read path from the manifest the catalog already stores,
    /// not from a column written at publish. The manifest and its hash are one
    /// row, so re-verifying them here re-checks the binding the publish checked
    /// and cannot answer from a summary that has drifted from the catalog; and
    /// every deployment ever published has an answer, because `manifest_json`
    /// is the column publish has always written.
    ///
    /// # Errors
    /// Refuses a deployment that is not the named app's, a stored manifest that
    /// no longer verifies or parses, and unavailable storage.
    pub async fn registration(
        &self,
        request: &DeployRegistrationRequest,
    ) -> Result<DeployRegistration, DeploymentError> {
        let (app, deployment) = (request.app_id.clone(), request.deploy_id.as_str().to_owned());
        compio::time::timeout(REQUEST_TIMEOUT, async {
            let record = self
                .executor
                .run(move |database| async move {
                    DeploymentHolds::new(database)?
                        .manifest(&app, &deployment)
                        .await
                })
                .await?;
            crate::publication::VerifiedDeployment::verify(
                record.manifest_json,
                record.deploy_hash,
            )
            .map(|deployment| deployment.deploy_registration(&request.deploy_id))
            .map_err(|error| {
                DeploymentError::Internal(format!(
                    "stored deployment manifest no longer verifies: {error}"
                ))
            })
        })
        .await
        .map_err(|_| DeploymentError::Timeout)?
    }

    /// The journal-scoped pair for a host whose authority is its own role.
    ///
    /// The HTTP boundary authenticates the workflow service before calling
    /// this, and the journal a hold protects belongs to that service rather
    /// than to a placement, so there is no assignment to verify and no worker
    /// to name. The ledger refuses a deployment that is not the app's, a
    /// deployment whose reclamation has closed admission, a stale generation
    /// or transition, and unavailable storage.
    ///
    /// # Errors
    /// Refuses a request naming a placement, foreign deployments, closed
    /// admission, stale generations and unavailable storage.
    pub async fn acquire_asserted(
        &self,
        request: &HoldRequest,
    ) -> Result<HoldReceipt, DeploymentError> {
        self.change_asserted(request, true).await
    }

    /// # Errors
    /// Refuses a request naming a placement, foreign deployments, stale
    /// generations and unavailable storage.
    pub async fn release_asserted(
        &self,
        request: &HoldRequest,
    ) -> Result<HoldReceipt, DeploymentError> {
        self.change_asserted(request, false).await
    }

    /// The HTTP boundary authenticates the workflow service before calling this.
    /// Queue ownership is stable across manager replicas and has no worker lease.
    ///
    /// # Errors
    /// Refuses foreign deployments, stale generations and unavailable storage.
    pub async fn acquire_queue(
        &self,
        request: &QueueHoldRequest,
    ) -> Result<HoldReceipt, DeploymentError> {
        self.change_queue(request, true).await
    }

    /// # Errors
    /// Refuses foreign deployments, stale generations and unavailable storage.
    pub async fn release_queue(
        &self,
        request: &QueueHoldRequest,
    ) -> Result<HoldReceipt, DeploymentError> {
        self.change_queue(request, false).await
    }

    async fn change_queue(
        &self,
        request: &QueueHoldRequest,
        acquire: bool,
    ) -> Result<HoldReceipt, DeploymentError> {
        self.change_unverified(
            &HoldScope::for_queue(request.app_id.clone()),
            request.deploy_id.as_str(),
            request.generation,
            acquire,
        )
        .await
    }

    async fn change_asserted(
        &self,
        request: &HoldRequest,
        acquire: bool,
    ) -> Result<HoldReceipt, DeploymentError> {
        self.change_unverified(
            &HoldScope::for_app(request.app_id.clone()),
            &request.deploy_id,
            request.generation,
            acquire,
        )
        .await
    }

    /// Apply a hold whose caller was authenticated by role, with no placement
    /// read. The scope decides which holder the ledger acts as, and the host
    /// derives it from the endpoint and the verified role, never from the body.
    async fn change_unverified(
        &self,
        scope: &HoldScope,
        deployment: &str,
        generation: HoldGeneration,
        acquire: bool,
    ) -> Result<HoldReceipt, DeploymentError> {
        let (scope, deployment) = (scope.clone(), deployment.to_owned());
        compio::time::timeout(
            REQUEST_TIMEOUT,
            self.executor.run(move |database| async move {
                let ledger = DeploymentHolds::new(database)?;
                if acquire {
                    ledger.acquire(&scope, &deployment, generation).await
                } else {
                    ledger.release(&scope, &deployment, generation).await
                }
            }),
        )
        .await
        .map_err(|_| DeploymentError::Timeout)?
    }
}

pub fn configure(config: &mut web::ServiceConfig) {
    config
        .service(
            web::resource(endpoints::CONTROL_DEPLOYMENT_HOLD_ACQUIRE.path_template())
                .state(web::types::JsonConfig::default().limit(MAX_REQUEST_BYTES))
                .route(web::post().to(acquire)),
        )
        .service(
            web::resource(endpoints::CONTROL_DEPLOYMENT_HOLD_RELEASE.path_template())
                .state(web::types::JsonConfig::default().limit(MAX_REQUEST_BYTES))
                .route(web::post().to(release)),
        )
        .service(
            web::resource(endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_ACQUIRE.path_template())
                .state(web::types::JsonConfig::default().limit(MAX_REQUEST_BYTES))
                .route(web::post().to(acquire_queue)),
        )
        .service(
            web::resource(endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_RELEASE.path_template())
                .state(web::types::JsonConfig::default().limit(MAX_REQUEST_BYTES))
                .route(web::post().to(release_queue)),
        )
        .service(
            web::resource(endpoints::CONTROL_DEPLOY_REGISTRATION.path_template())
                .state(web::types::JsonConfig::default().limit(MAX_REQUEST_BYTES))
                .route(web::post().to(deploy_registration)),
        );
}

async fn deploy_registration(
    request: web::HttpRequest,
    state: State<Arc<AppState>>,
    api: State<Arc<DeploymentHoldApi>>,
    body: web::types::Payload,
) -> web::HttpResponse {
    match handle_registration(request, state, api, body).await {
        Ok(registration) => web::HttpResponse::Ok().json(&registration),
        Err(code) => refusal(code),
    }
}

async fn handle_registration(
    request: web::HttpRequest,
    state: State<Arc<AppState>>,
    api: State<Arc<DeploymentHoldApi>>,
    body: web::types::Payload,
) -> Result<DeployRegistration, FailureCode> {
    compio::time::timeout(REQUEST_TIMEOUT, async {
        let authorization = request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok());
        let issuer = presented_issuer(authorization).ok_or(FailureCode::Unauthenticated)?;
        // Whole-issuer equality, as on the queue-scoped pair: this is a
        // role-arity credential, so an instance-arity `svc/workflow/<id>` is
        // refused. A worker is refused here too - it holds the artifacts and
        // derives this from them, so it has no reason to ask.
        let role = service_issuer(WORKFLOW_SERVICE_NAME).map_err(|_| FailureCode::Unavailable)?;
        if issuer != role {
            return Err(FailureCode::Unauthenticated);
        }
        crate::internal::verify_service_caller(
            &state,
            authorization,
            endpoints::CONTROL_DEPLOY_REGISTRATION,
        )
        .await
        .map_err(|error| match error {
            AuthError::StoreUnavailable => FailureCode::Unavailable,
            _ => FailureCode::Unauthenticated,
        })?;
        // Only the verified workflow role reaches body decoding.
        let mut body = body.into_inner();
        let query = <Json<DeployRegistrationRequest> as web::FromRequest<
            web::error::DefaultError,
        >>::from_request(&request, &mut body)
        .await
        .map_err(|error| match error {
            web::error::JsonPayloadError::Overflow => FailureCode::RequestTooLarge,
            _ => FailureCode::Invalid,
        })?
        .into_inner();
        api.registration(&query).await.map_err(|error| failure(&error))
    })
    .await
    .map_err(|_| FailureCode::Unavailable)?
}

async fn acquire_queue(
    request: web::HttpRequest,
    state: State<Arc<AppState>>,
    api: State<Arc<DeploymentHoldApi>>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        handle_queue(
            request,
            state,
            api,
            body,
            endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_ACQUIRE,
            true,
        )
        .await,
    )
}

async fn release_queue(
    request: web::HttpRequest,
    state: State<Arc<AppState>>,
    api: State<Arc<DeploymentHoldApi>>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        handle_queue(
            request,
            state,
            api,
            body,
            endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_RELEASE,
            false,
        )
        .await,
    )
}

async fn handle_queue(
    request: web::HttpRequest,
    state: State<Arc<AppState>>,
    api: State<Arc<DeploymentHoldApi>>,
    body: web::types::Payload,
    endpoint: ServiceEndpoint,
    acquire: bool,
) -> Result<HoldReceipt, FailureCode> {
    compio::time::timeout(REQUEST_TIMEOUT, async {
        let authorization = request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok());
        let issuer = presented_issuer(authorization).ok_or(FailureCode::Unauthenticated)?;
        let role = service_issuer(WORKFLOW_SERVICE_NAME).map_err(|_| FailureCode::Unavailable)?;
        if issuer != role {
            return Err(FailureCode::Unauthenticated);
        }
        crate::internal::verify_service_caller(&state, authorization, endpoint)
            .await
            .map_err(|error| match error {
                AuthError::StoreUnavailable => FailureCode::Unavailable,
                _ => FailureCode::Unauthenticated,
            })?;
        // Only the verified workflow role reaches body decoding, and the body
        // cannot select a journal holder.
        let mut body = body.into_inner();
        let command =
            <Json<QueueHoldRequest> as web::FromRequest<web::error::DefaultError>>::from_request(
                &request, &mut body,
            )
            .await
            .map_err(|error| match error {
                web::error::JsonPayloadError::Overflow => FailureCode::RequestTooLarge,
                _ => FailureCode::Invalid,
            })?
            .into_inner();
        let result = if acquire {
            api.acquire_queue(&command).await
        } else {
            api.release_queue(&command).await
        };
        result.map_err(|error| failure(&error))
    })
    .await
    .map_err(|_| FailureCode::Unavailable)?
}

async fn acquire(
    request: web::HttpRequest,
    state: State<Arc<AppState>>,
    api: State<Arc<DeploymentHoldApi>>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        handle(
            request,
            state,
            api,
            body,
            endpoints::CONTROL_DEPLOYMENT_HOLD_ACQUIRE,
            true,
        )
        .await,
    )
}
async fn release(
    request: web::HttpRequest,
    state: State<Arc<AppState>>,
    api: State<Arc<DeploymentHoldApi>>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        handle(
            request,
            state,
            api,
            body,
            endpoints::CONTROL_DEPLOYMENT_HOLD_RELEASE,
            false,
        )
        .await,
    )
}
/// A journal hold is taken only on the strength of the workflow service's own
/// role, which holds the journal itself. An instance credential is NOT admitted
/// here: the role is the authority, exactly as on the queue-scoped pair.
///
/// A malformed role constant is this deployment's own defect rather than a
/// verdict on the credential, so it answers unavailable exactly as the
/// queue-scoped pair does; every other refusal is unauthenticated.
fn asserted_caller(issuer: &ServiceIssuer) -> Result<(), FailureCode> {
    let service = service_issuer(WORKFLOW_SERVICE_NAME).map_err(|_| FailureCode::Unavailable)?;
    if issuer == &service {
        Ok(())
    } else {
        Err(FailureCode::Unauthenticated)
    }
}

async fn handle(
    request: web::HttpRequest,
    state: State<Arc<AppState>>,
    api: State<Arc<DeploymentHoldApi>>,
    body: web::types::Payload,
    endpoint: ServiceEndpoint,
    acquire: bool,
) -> Result<HoldReceipt, FailureCode> {
    compio::time::timeout(REQUEST_TIMEOUT, async {
        let authorization = request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok());
        let issuer = presented_issuer(authorization).ok_or(FailureCode::Unauthenticated)?;
        // The role is decided before any body is read, so a body cannot
        // select an authority the credential does not hold.
        asserted_caller(&issuer)?;
        crate::internal::verify_service_caller(&state, authorization, endpoint)
            .await
            .map_err(|error| match error {
                AuthError::StoreUnavailable => FailureCode::Unavailable,
                _ => FailureCode::Unauthenticated,
            })?;
        let mut body = body.into_inner();
        let command =
            <Json<HoldRequest> as web::FromRequest<web::error::DefaultError>>::from_request(
                &request, &mut body,
            )
            .await
            .map_err(|error| match error {
                web::error::JsonPayloadError::Overflow => FailureCode::RequestTooLarge,
                _ => FailureCode::Invalid,
            })?
            .into_inner();
        let result = if acquire {
            api.acquire_asserted(&command).await
        } else {
            api.release_asserted(&command).await
        };
        result.map_err(|error| failure(&error))
    })
    .await
    .map_err(|_| FailureCode::Unavailable)?
}
fn respond(result: Result<HoldReceipt, FailureCode>) -> web::HttpResponse {
    match result {
        Ok(receipt) => web::HttpResponse::Ok().json(&receipt),
        Err(code) => refusal(code),
    }
}

/// One refusal shape for every operation this module serves, so a caller reads
/// the same status and the same body whichever one it asked for.
fn refusal(code: FailureCode) -> web::HttpResponse {
    let status = match code {
        FailureCode::Invalid => 400,
        FailureCode::Unauthenticated => 401,
        FailureCode::Denied => 403,
        FailureCode::Conflict => 409,
        FailureCode::RequestTooLarge => 413,
        FailureCode::Capacity => 429,
        FailureCode::Unavailable => 503,
    };
    web::HttpResponse::build(
        ntex::http::StatusCode::from_u16(status).expect("deployment catalog response status"),
    )
    .force_close()
    .json(&Failure { code })
}
const fn failure(error: &DeploymentError) -> FailureCode {
    match error {
        DeploymentError::InvalidRequest(_) => FailureCode::Invalid,
        DeploymentError::Unauthenticated => FailureCode::Unauthenticated,
        DeploymentError::PermissionDenied => FailureCode::Denied,
        DeploymentError::Conflict(_) => FailureCode::Conflict,
        DeploymentError::ResourceExhausted(_) => FailureCode::Capacity,
        _ => FailureCode::Unavailable,
    }
}

