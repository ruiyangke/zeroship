//! Worker-facing deployment metadata; customer journals never enter Control.

#![expect(
    clippy::future_not_send,
    reason = "ORM and HTTP stay on their compio thread"
)]

use crate::AppState;
use ntex::web::{
    self,
    types::{Json, State},
};
use std::{
    cell::Cell,
    future::{poll_fn, Future},
    rc::Rc,
    sync::Arc,
    task::Poll,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zeroship_core::{
    schema_name::SchemaName,
    service_assertion::presented_issuer,
    service_identity::{endpoints, AuthError, ServiceEndpoint},
    service_peers::{service_issuer, WORKER_SERVICE_NAME},
    workflow_coordination::{Failure, FailureCode, VerifyAssignment, WorkerId},
    workflow_deployments::{HoldReceipt, HoldRequest, HoldScope},
};
use zeroship_data_orm::{
    binding::DbBinding, encryption::ProjectKeySource, orm::Database, ConnectOptions,
};
use zeroship_workflow_client::{self as coordination, ControlCoordinator, Options};
use zeroship_workflow_manager::deployments::{self, DeploymentHolds, Error as DeploymentError};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Runtime-local platform metadata handles. No creator connection is accepted.
#[derive(Debug)]
pub struct DeploymentHoldApi {
    ledger: DeploymentHolds,
    coordinator: Option<ControlCoordinator>,
}
impl DeploymentHoldApi {
    /// Bind the Control process's existing database and outbound service signer.
    /// An unconfigured signer leaves these protected operations unavailable.
    ///
    /// # Errors
    /// Refuses unavailable deployment metadata or invalid coordinator endpoints.
    pub async fn connect(state: &AppState, coordinator_url: &str) -> Result<Self, DeploymentError> {
        let coordinator = if state.service_auth.signing_identity().is_some() {
            Some(
                ControlCoordinator::new(
                    coordinator_url,
                    state.service_auth.clone(),
                    Options::default(),
                )
                .map_err(coordination_error)?,
            )
        } else {
            None
        };
        let database = Database::connect(
            DbBinding::new(
                "platform",
                "control-deployments",
                SchemaName::new("zeroship").map_err(|_| unavailable())?,
            ),
            ConnectOptions::new(
                state.registry.workflow_store_db_url().to_owned(),
                ProjectKeySource::unavailable(),
            )
            .connection_authority(),
            deployments::collections()?,
        )
        .await?;
        Ok(Self {
            ledger: DeploymentHolds::new(database)?,
            coordinator,
        })
    }

    /// Compose a verified platform database and Control's coordinator client.
    ///
    /// # Errors
    /// Refuses incompatible deployment metadata.
    pub fn new(
        database: Database,
        coordinator: ControlCoordinator,
    ) -> Result<Self, DeploymentError> {
        Ok(Self {
            ledger: DeploymentHolds::new(database)?,
            coordinator: Some(coordinator),
        })
    }

    /// The HTTP boundary supplies the authenticated worker, never a body field.
    ///
    /// # Errors
    /// Refuses foreign or expired placement, stale holds and unavailable stores.
    pub async fn acquire(
        &self,
        worker: &WorkerId,
        request: &HoldRequest,
    ) -> Result<HoldReceipt, DeploymentError> {
        self.change(worker, request, true).await
    }

    /// # Errors
    /// Refuses foreign or expired placement, stale generations and unavailable stores.
    pub async fn release(
        &self,
        worker: &WorkerId,
        request: &HoldRequest,
    ) -> Result<HoldReceipt, DeploymentError> {
        self.change(worker, request, false).await
    }

    async fn change(
        &self,
        worker: &WorkerId,
        request: &HoldRequest,
        acquire: bool,
    ) -> Result<HoldReceipt, DeploymentError> {
        let coordinator = self.coordinator.as_ref().ok_or_else(unavailable)?;
        let assignment = VerifyAssignment {
            app_id: request.app_id.clone(),
            worker_id: worker.clone(),
            assignment_revision: request.assignment_revision,
        };
        let budget = AuthorityBudget::new();
        let authorize = || async {
            budget.cap(verify_authority(coordinator, &assignment).await?);
            Ok(())
        };
        budget
            .run(async {
                // Authenticate app scope before looking up deployment metadata, then
                // repeat after lock waits and before committing its generation.
                authorize().await?;
                let scope = HoldScope::for_app(request.app_id.clone());
                if acquire {
                    self.ledger
                        .acquire_authorized(
                            &scope,
                            &request.deploy_id,
                            request.generation,
                            authorize,
                        )
                        .await
                } else {
                    self.ledger
                        .release_authorized(
                            &scope,
                            &request.deploy_id,
                            request.generation,
                            authorize,
                        )
                        .await
                }
            })
            .await
    }
}

/// Revalidation can shorten authority while the ledger waits for database I/O.
/// Expiry cancels work before commit dispatch. Once commit is in flight, this
/// bounds the caller's wait; a retry recovers its possibly committed receipt.
struct AuthorityBudget(Cell<Instant>);

impl AuthorityBudget {
    fn new() -> Self {
        Self(Cell::new(Instant::now() + REQUEST_TIMEOUT))
    }

    fn cap(&self, deadline: Instant) {
        self.0.set(self.0.get().min(deadline));
    }

    async fn run<T>(
        &self,
        future: impl Future<Output = Result<T, DeploymentError>>,
    ) -> Result<T, DeploymentError> {
        let mut future = Box::pin(future);
        let mut deadline = self.0.get();
        let mut timer = Box::pin(compio::time::sleep_until(deadline));
        poll_fn(move |context| {
            if Instant::now() >= self.0.get() {
                return Poll::Ready(Err(DeploymentError::Timeout));
            }
            if deadline != self.0.get() {
                deadline = self.0.get();
                timer = Box::pin(compio::time::sleep_until(deadline));
            }
            if timer.as_mut().poll(context).is_ready() {
                return Poll::Ready(Err(DeploymentError::Timeout));
            }
            let result = future.as_mut().poll(context);
            if Instant::now() >= self.0.get() {
                return Poll::Ready(Err(DeploymentError::Timeout));
            }
            if deadline != self.0.get() {
                deadline = self.0.get();
                timer = Box::pin(compio::time::sleep_until(deadline));
                if timer.as_mut().poll(context).is_ready() {
                    return Poll::Ready(Err(DeploymentError::Timeout));
                }
            }
            result
        })
        .await
    }
}

async fn verify_authority(
    coordinator: &ControlCoordinator,
    request: &VerifyAssignment,
) -> Result<Instant, DeploymentError> {
    let assignment = coordinator
        .verify_assignment(request)
        .await
        .map_err(coordination_error)?;
    // Wire expiry uses the coordinator's wall clock; these hosts require
    // synchronized clocks. Capture before reading local wall time so conversion
    // cannot extend the lease by time spent preparing the monotonic deadline.
    let sampled_at = Instant::now();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| unavailable())?
        .as_millis();
    let expires = u128::try_from(assignment.expires_at.get()).map_err(|_| unavailable())?;
    let remaining = expires
        .checked_sub(now)
        .filter(|remaining| *remaining > 0)
        .ok_or(DeploymentError::PermissionDenied)?;
    sampled_at
        .checked_add(Duration::from_millis(
            u64::try_from(remaining).map_err(|_| unavailable())?,
        ))
        .ok_or_else(unavailable)
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
        );
}

async fn acquire(
    request: web::HttpRequest,
    state: State<Arc<AppState>>,
    api: State<Rc<DeploymentHoldApi>>,
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
    api: State<Rc<DeploymentHoldApi>>,
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
async fn handle(
    request: web::HttpRequest,
    state: State<Arc<AppState>>,
    api: State<Rc<DeploymentHoldApi>>,
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
        let role = service_issuer(WORKER_SERVICE_NAME).map_err(|_| FailureCode::Unavailable)?;
        if issuer.principal() != role.principal() {
            return Err(FailureCode::Unauthenticated);
        }
        let worker = WorkerId::parse(issuer.instance().ok_or(FailureCode::Unauthenticated)?)
            .map_err(|_| FailureCode::Unauthenticated)?;
        crate::internal::verify_service_caller(&state, authorization, endpoint)
            .await
            .map_err(|error| match error {
                AuthError::StoreUnavailable => FailureCode::Unavailable,
                _ => FailureCode::Unauthenticated,
            })?;
        // The issuer selector now belongs to the verified enrolled instance.
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
            api.acquire(&worker, &command).await
        } else {
            api.release(&worker, &command).await
        };
        result.map_err(failure)
    })
    .await
    .map_err(|_| FailureCode::Unavailable)?
}
fn respond(result: Result<HoldReceipt, FailureCode>) -> web::HttpResponse {
    match result {
        Ok(receipt) => web::HttpResponse::Ok().json(&receipt),
        Err(code) => {
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
                ntex::http::StatusCode::from_u16(status).expect("hold response status"),
            )
            .force_close()
            .json(&Failure { code })
        }
    }
}
fn failure(error: DeploymentError) -> FailureCode {
    match error {
        DeploymentError::InvalidRequest(_) => FailureCode::Invalid,
        DeploymentError::Unauthenticated => FailureCode::Unauthenticated,
        DeploymentError::PermissionDenied => FailureCode::Denied,
        DeploymentError::Conflict(_) => FailureCode::Conflict,
        DeploymentError::ResourceExhausted(_) => FailureCode::Capacity,
        _ => FailureCode::Unavailable,
    }
}
fn coordination_error(error: coordination::Error) -> DeploymentError {
    match error {
        coordination::Error::Refused(FailureCode::Denied | FailureCode::Conflict) => {
            DeploymentError::PermissionDenied
        }
        coordination::Error::InvalidConfig => {
            DeploymentError::InvalidRequest("invalid workflow coordinator configuration".into())
        }
        _ => unavailable(),
    }
}
fn unavailable() -> DeploymentError {
    DeploymentError::Unavailable("deployment hold authority unavailable".into())
}
