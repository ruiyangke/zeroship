//! Authenticated Control operations over the app deployment catalog: journal and
//! queue retention, and the manifest summary a journal records for a deployment.
//! Customer journals never enter Control.
//!
//! The summary is here rather than in a module of its own because it answers out
//! of the same catalog, under the same role check, through the same handle.
//!
//! Every ledger operation runs on Control's retention executor
//! (`Registry::retention`), which holds a fixed set of sessions for the whole
//! process, so serving threads open none. A placed hold's first placement check
//! runs on the calling thread, so a caller it refuses never takes a lane; its
//! transaction, the checks inside it and the budget that decides whether it
//! may commit run on the lane that sends COMMIT.

#![expect(
    clippy::future_not_send,
    reason = "ORM and HTTP stay on their compio thread"
)]

use crate::{AppState, publication::Catalog};
use ntex::web::{
    self,
    types::{Json, State},
};
use std::{
    cell::{Cell, RefCell},
    future::{Future, poll_fn},
    rc::Rc,
    sync::{Arc, Weak},
    task::Poll,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zeroship_core::{
    service_assertion::{ServiceIssuer, presented_issuer},
    service_identity::{AuthError, ServiceEndpoint, endpoints},
    service_peers::{ServiceAuth, WORKER_SERVICE_NAME, WORKFLOW_SERVICE_NAME, service_issuer},
    workflow_coordination::{Failure, FailureCode, VerifyAssignment, WorkerId},
    workflow_deployments::{HoldGeneration, HoldReceipt, HoldRequest, HoldScope, QueueHoldRequest},
};
use zeroship_workflow::service::{DeployRegistration, DeployRegistrationRequest};
use zeroship_workflow_client::{self as coordination, ControlCoordinator, Options};
use zeroship_workflow_manager::deployments::{DeploymentHolds, Error as DeploymentError};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// The deployment-hold surface every serving thread shares.
///
/// It holds no database and no HTTP client: the retention executor owns the
/// sessions, and each thread that verifies a placement builds its own
/// coordinator client.
#[derive(Debug)]
pub struct DeploymentHoldApi {
    executor: Catalog,
    coordinator: Arc<CoordinatorSource>,
}

/// What a thread builds its coordinator client from.
#[derive(Debug)]
struct CoordinatorSource {
    url: String,
    auth: Arc<ServiceAuth>,
    options: Options,
}

thread_local! {
    /// The coordinator clients this thread has built, one per hold API that
    /// ran on it: serving threads for a placed hold's first check, executor
    /// threads for the checks inside its transaction. An HTTP client's pooled
    /// streams belong to the thread that opens them, so each thread keeps its
    /// own.
    static COORDINATORS: RefCell<Vec<(Weak<CoordinatorSource>, Rc<ControlCoordinator>)>> =
        const { RefCell::new(Vec::new()) };
}

impl CoordinatorSource {
    /// Build the client once to refuse what every thread would refuse.
    fn new(url: &str, auth: Arc<ServiceAuth>, options: Options) -> Result<Self, DeploymentError> {
        let source = Self {
            url: url.to_owned(),
            auth,
            options,
        };
        source.build().map_err(coordination_error)?;
        Ok(source)
    }

    fn build(&self) -> Result<ControlCoordinator, coordination::Error> {
        ControlCoordinator::new(&self.url, self.auth.clone(), self.options.clone())
    }

    /// This thread's client for `source`, built on first use.
    fn client(source: &Arc<Self>) -> Result<Rc<ControlCoordinator>, DeploymentError> {
        COORDINATORS.with_borrow_mut(|built| {
            built.retain(|(owner, _)| owner.strong_count() > 0);
            if let Some((_, client)) = built
                .iter()
                .find(|(owner, _)| std::ptr::eq(owner.as_ptr(), Arc::as_ptr(source)))
            {
                return Ok(Rc::clone(client));
            }
            let client = Rc::new(source.build().map_err(coordination_error)?);
            built.push((Arc::downgrade(source), Rc::clone(&client)));
            Ok(client)
        })
    }
}

impl DeploymentHoldApi {
    /// Serve holds on `executor`, verifying placements with the coordinator at
    /// `coordinator_url` under Control's signer.
    ///
    /// The client is built here once to refuse, and again on each thread that
    /// uses it, so a coordinator the client refuses refuses the boot rather
    /// than every placed hold.
    ///
    /// # Errors
    /// Refuses a coordinator origin the client refuses and a signer that is not
    /// Control's.
    pub fn new(
        executor: Catalog,
        coordinator_url: &str,
        auth: Arc<ServiceAuth>,
        options: Options,
    ) -> Result<Self, DeploymentError> {
        Ok(Self {
            executor,
            coordinator: Arc::new(CoordinatorSource::new(coordinator_url, auth, options)?),
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
    /// to name. Everything else the placed pair refuses is refused here, by the
    /// same ledger operation: a deployment that is not the app's, a deployment
    /// whose reclamation has closed admission, a stale generation or transition,
    /// and unavailable storage.
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
        // A caller with no placement may not name one. The field is the placed
        // pair's authorization input, so accepting it here would leave a body
        // field that reads like authority and is checked by nothing.
        if request.assignment_revision.is_some() {
            return Err(DeploymentError::InvalidRequest(
                "an asserted deployment hold names no placement".into(),
            ));
        }
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

    async fn change(
        &self,
        worker: &WorkerId,
        request: &HoldRequest,
        acquire: bool,
    ) -> Result<HoldReceipt, DeploymentError> {
        // A placed caller names its placement. Without one there is nothing to
        // verify, and a worker is authorized by verification alone.
        let assignment_revision = request.assignment_revision.ok_or_else(|| {
            DeploymentError::InvalidRequest("a placed deployment hold names its placement".into())
        })?;
        let assignment = VerifyAssignment {
            app_id: request.app_id.clone(),
            worker_id: worker.clone(),
            assignment_revision,
        };
        // The caller's whole wait, queueing for a lane included, counts
        // against the authority budget.
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        // Authenticate app scope on the calling thread, before occupying a
        // lane or reading deployment metadata, so a caller its placement does
        // not cover is refused without taking a retention session.
        let coordinator = CoordinatorSource::client(&self.coordinator)?;
        let lease = compio::time::timeout(
            REQUEST_TIMEOUT,
            verify_authority(&coordinator, &assignment),
        )
        .await
        .map_err(|_| DeploymentError::Timeout)??;
        let authority = deadline.min(lease);
        let source = Arc::clone(&self.coordinator);
        let scope = HoldScope::for_app(request.app_id.clone());
        let (deployment, generation) = (request.deploy_id.clone(), request.generation);
        compio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            self.executor.run(move |database| async move {
                // The budget runs on the executor thread, which is the thread
                // that dispatches COMMIT, so its expiry stops COMMIT there.
                let budget = AuthorityBudget::until(authority);
                let coordinator = CoordinatorSource::client(&source)?;
                let ledger = DeploymentHolds::new(database)?;
                let authorize = || async {
                    budget.cap(verify_authority(&coordinator, &assignment).await?);
                    Ok(())
                };
                // Re-verify after lock waits and before committing the
                // generation.
                budget
                    .run(async {
                        if acquire {
                            ledger
                                .acquire_authorized(&scope, &deployment, generation, authorize)
                                .await
                        } else {
                            ledger
                                .release_authorized(&scope, &deployment, generation, authorize)
                                .await
                        }
                    })
                    .await
            }),
        )
        .await
        .map_err(|_| DeploymentError::Timeout)?
    }
}

/// Revalidation can shorten authority while the ledger waits for database I/O.
/// Expiry cancels work before commit dispatch, so the budget must run on the
/// thread that dispatches COMMIT. Once commit is in flight, this bounds the
/// caller's wait; a retry recovers its possibly committed receipt.
struct AuthorityBudget(Cell<Instant>);

impl AuthorityBudget {
    const fn until(deadline: Instant) -> Self {
        Self(Cell::new(deadline))
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
        // Only the verified workflow role reaches body decoding. It cannot
        // select a journal holder or substitute a worker's placement identity.
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
/// Which authority a journal-hold caller presented.
///
/// Two principals may hold a journal, and they are authorized differently: a
/// worker by the placement it names, the workflow service by its own role. This
/// is the whole of the difference, it is decided from the credential before any
/// body is read, and there is no third arm - a principal that is neither is
/// refused.
enum HoldCaller {
    /// A joined worker instance, whose placement is verified on every call.
    Placed(WorkerId),
    /// The workflow service's role, which holds the journal itself. An instance
    /// credential is NOT admitted here: the role is the authority, exactly as on
    /// the queue-scoped pair.
    Asserted,
}

/// A malformed role constant is this deployment's own defect rather than a
/// verdict on the credential, so it answers unavailable exactly as the
/// queue-scoped pair does; every other refusal is unauthenticated.
fn hold_caller(issuer: &ServiceIssuer) -> Result<HoldCaller, FailureCode> {
    let worker = service_issuer(WORKER_SERVICE_NAME).map_err(|_| FailureCode::Unavailable)?;
    if issuer.principal() == worker.principal() {
        let instance = issuer.instance().ok_or(FailureCode::Unauthenticated)?;
        return WorkerId::parse(instance)
            .map(HoldCaller::Placed)
            .map_err(|_| FailureCode::Unauthenticated);
    }
    let service = service_issuer(WORKFLOW_SERVICE_NAME).map_err(|_| FailureCode::Unavailable)?;
    if issuer == &service {
        return Ok(HoldCaller::Asserted);
    }
    Err(FailureCode::Unauthenticated)
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
        let caller = hold_caller(&issuer)?;
        crate::internal::verify_service_caller(&state, authorization, endpoint)
            .await
            .map_err(|error| match error {
                AuthError::StoreUnavailable => FailureCode::Unavailable,
                _ => FailureCode::Unauthenticated,
            })?;
        // The issuer selector now belongs to the verified enrolled instance or
        // to the verified service role. Only then is a body decoded, so neither
        // one can select the other's authority through a field.
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
        let result = match (&caller, acquire) {
            (HoldCaller::Placed(worker), true) => api.acquire(worker, &command).await,
            (HoldCaller::Placed(worker), false) => api.release(worker, &command).await,
            (HoldCaller::Asserted, true) => api.acquire_asserted(&command).await,
            (HoldCaller::Asserted, false) => api.release_asserted(&command).await,
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

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_core::{
        service_assertion::{ServiceSigningKey, ServiceTrustBundle, TransportAssertionVerifier},
        service_peers::{CONTROL_SERVICE_NAME, ServiceKeyring},
    };

    const ORIGIN: &str = "http://127.0.0.1:9093";

    fn signer(service: &str) -> Arc<ServiceAuth> {
        Arc::new(ServiceAuth::new(
            ServiceKeyring::from_parts(
                service_issuer(service).unwrap(),
                ServiceSigningKey::generate(),
                ServiceTrustBundle::new(),
            )
            .unwrap(),
            Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
        ))
    }

    /// The hold API's coordinator is judged once, where the process builds it,
    /// so what every executor thread would refuse refuses the boot instead.
    #[test]
    fn the_coordinator_is_refused_where_the_process_builds_it() {
        assert!(
            CoordinatorSource::new(ORIGIN, signer(CONTROL_SERVICE_NAME), Options::default())
                .is_ok()
        );
        for (origin, auth) in [
            (ORIGIN, Arc::new(ServiceAuth::unconfigured())),
            (ORIGIN, signer(WORKER_SERVICE_NAME)),
            ("http://coordinator.internal:9093", signer(CONTROL_SERVICE_NAME)),
            ("not a url", signer(CONTROL_SERVICE_NAME)),
        ] {
            assert!(
                CoordinatorSource::new(origin, auth, Options::default()).is_err(),
                "{origin}"
            );
        }
    }

    /// Each thread builds one client per hold API and reuses it, so placement
    /// checks share its pooled streams; another API gets a client of its own.
    #[test]
    fn a_thread_reuses_its_client_for_one_api_and_builds_another_for_the_next() {
        let first = Arc::new(
            CoordinatorSource::new(ORIGIN, signer(CONTROL_SERVICE_NAME), Options::default())
                .unwrap(),
        );
        let second = Arc::new(
            CoordinatorSource::new(ORIGIN, signer(CONTROL_SERVICE_NAME), Options::default())
                .unwrap(),
        );
        let client = CoordinatorSource::client(&first).unwrap();
        assert!(Rc::ptr_eq(&client, &CoordinatorSource::client(&first).unwrap()));
        assert!(!Rc::ptr_eq(&client, &CoordinatorSource::client(&second).unwrap()));
        // A dropped API's client leaves the thread with it.
        drop(first);
        CoordinatorSource::client(&second).unwrap();
        COORDINATORS.with_borrow(|built| {
            assert_eq!(built.len(), 1);
            assert!(std::ptr::eq(built[0].0.as_ptr(), Arc::as_ptr(&second)));
        });
    }
}
