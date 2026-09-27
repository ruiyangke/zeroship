use super::{authorization, read_json, respond};
use crate::{auth::VerifiedWorker, coordinator::Error, SharedState};
use ntex::web::{self, types::State};
use std::time::{Duration, Instant};
use zeroship_core::{
    app_id::AppId,
    service_identity::{endpoints, ServiceEndpoint},
    workflow_coordination::{AssignedScope, WorkerId},
    workflow_jobs::{Settlement, SubmitJob},
    workflow_policy::MAX_JOURNAL_BYTES_CEILING,
};
use zeroship_workflow::{
    service::{
        delivery::{
            AcceptedJob, ClaimedTask, RenewedTask, ReportedExecution, ReportedGrant, TaskClaim,
        },
        AppWorkflows,
    },
    WorkflowServiceError,
};
use zeroship_workflow_client::{
    ClaimedDelivery, RenewDelivery, RenewedDelivery, Reported, SettleDelivery, SettledDelivery,
};
use zeroship_workflow_manager::Error as NativeError;

/// JSON extractor budget for a settlement that carries an outcome batch.
///
/// The batch has to fit the run's replay journal, so it answers to
/// `AppPolicy::max_journal_bytes`, which the platform refuses above
/// `MAX_JOURNAL_BYTES_CEILING`. The service-wide metadata budget governs the
/// other three job routes, whose bodies carry identity and durations only. The
/// headroom is the envelope around the batch -- the delivery and the task it was
/// produced under -- which is bounded and small, and it is added rather than
/// assumed because the ceiling governs the BATCH while this governs the message.
pub const SETTLE_BODY_BYTES: usize = MAX_JOURNAL_BYTES_CEILING + 8 * 1024;

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
                // The only job call that carries a journal quantity, and the
                // only one that needs more than the metadata budget.
                .state(web::types::JsonConfig::default().limit(SETTLE_BODY_BYTES))
                .route(web::post().to(settle)),
        );
}

/// Bind the app this call acts for to this service's journal.
///
/// THE APP IS NEVER READ FROM THE BODY AS A PLACEMENT. It comes from the
/// delivery the manager half has already matched against its own queue row under
/// the credential that verified the request, so this cannot reach a journal for
/// an app the caller holds no delivery on.
async fn journal(state: &SharedState, app: &AppId) -> Result<AppWorkflows, Error> {
    let source = state.policy_source.as_ref().ok_or(Error::Unavailable)?;
    state
        .runs
        .app(source.as_ref(), app)
        .await
        .map_err(journal_error)
}

/// Carry an engine refusal into the closed coordination contract.
///
/// The match is wildcard-free, so a new engine refusal stops compiling here
/// until it is given a coordination code rather than folded into a neighbour.
/// Two arms name a host condition the caller cannot act on, so the operator gets
/// the original here and the wire gets only the code.
fn journal_error(error: WorkflowServiceError) -> Error {
    if matches!(
        error,
        WorkflowServiceError::Internal(_) | WorkflowServiceError::Unavailable(_)
    ) {
        tracing::warn!(
            code = error.code(),
            detail = %error,
            "workflow journal refusal reported opaquely to a delivery call"
        );
    }
    match error {
        WorkflowServiceError::InvalidRequest(_) => Error::Invalid,
        WorkflowServiceError::Unauthenticated => Error::Unauthenticated,
        WorkflowServiceError::PermissionDenied => Error::Denied,
        // A journal that does not hold the run or task a live delivery names is
        // a conflict rather than a missing resource: the caller's delivery is
        // stale, and the answer it needs is "claim again", not "this URL is
        // wrong".
        WorkflowServiceError::NotFound(_) | WorkflowServiceError::Conflict(_) => Error::Conflict,
        WorkflowServiceError::ResourceExhausted(_) => Error::Capacity,
        WorkflowServiceError::PayloadTooLarge => Error::RequestTooLarge,
        // An attempt that ran out of its own I/O ceiling has committed nothing,
        // so the caller may take the same delivery again.
        WorkflowServiceError::Unavailable(_) | WorkflowServiceError::Timeout => Error::Unavailable,
        // Ingress epochs are established by the creator-facing calls, never by a
        // delivery: nothing on this path carries one to be fenced.
        WorkflowServiceError::IngressFenced(_) | WorkflowServiceError::Internal(_) => {
            Error::Unavailable
        }
    }
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

/// The queue half of a settlement, whichever half produced its outcome.
async fn settled(
    state: &SharedState,
    actor: &VerifiedWorker,
    settlement: &Settlement,
) -> Result<zeroship_core::workflow_jobs::SettlementReceipt, Error> {
    state
        .service
        .manager
        .settle_job(actor.id(), settlement, || revalidate(state, actor))
        .await
        .map_err(Error::from)
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

/// The delivery ceiling is operator policy, so it is read from the authoritative
/// source rather than accepted from the worker. The result is handed to the
/// manager unresolved: a source that cannot answer for an app must not preempt
/// that app's placement refusal, which would tell the worker to retry a scope it
/// can never hold.
async fn ceiling(state: &SharedState, app: &AppId) -> Result<i64, NativeError> {
    let source = state
        .policy_source
        .as_ref()
        .ok_or(NativeError::Unavailable)?;
    Ok(source.observe(app).await?.policy().max_delivery_attempts)
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

/// Claim a delivery and, for the one operation that hands out a task, accept it
/// into this service's journal under the grant just committed.
///
/// THE QUEUE COMMITS FIRST. `claimed_in` counts the delivery inside the claim
/// transaction, so an attempt that reaches creator code has already been counted
/// against the app's delivery ceiling; accepting first would let a journal that
/// handed out a task be followed by a queue rollback. The wire lease is taken
/// LAST, after the journal work, so the authority the caller receives is what is
/// actually left rather than what was left before this service did its own I/O.
///
/// TWO STORES, NO SHARED TRANSACTION. A failure between the halves leaves the
/// queue holding a leased row whose journal accepted nothing; that row's lease
/// expires and the job is redelivered, which is the same recovery an unreachable
/// worker gets.
async fn claim(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let actor = authenticate(&request, &state, endpoints::WORKFLOW_JOB_CLAIM).await?;
            let command: AssignedScope = read_json(&request, body).await?;
            let granted = state
                .service
                .manager
                .claim_job(actor.id(), &command, ceiling(&state, &command.app_id).await, || {
                    revalidate(&state, &actor)
                })
                .await?;
            let Some(grant) = granted else {
                return Ok(None);
            };
            let accepted: Option<AcceptedJob> =
                if grant.delivery().job.operation.accepts_execution() {
                    let journal = journal(&state, &command.app_id).await?;
                    Some(
                        journal
                            .accept_job(&grant)
                            .await
                            .and_then(zeroship_workflow::service::delivery::JobAcceptance::reported)
                            .map_err(journal_error)?,
                    )
                } else {
                    None
                };
            Ok(Some(ClaimedDelivery {
                lease: grant.lease()?,
                accepted,
            }))
        }
        .await,
    )
}

/// Renew a delivery's queue lease and, when the caller names one, the journal
/// task held under it.
///
/// THE QUEUE COMMITS FIRST, for the reason `renewal` records: the first renewal
/// of an attempt is what counts it against the delivery ceiling, so an attempt
/// whose journal task was extended is always an attempt the queue has counted.
/// The journal half then runs under the grant this renewal just produced, which
/// is what refuses it if the delivery has no authority left.
async fn heartbeat(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    let started = Instant::now();
    respond(
        async {
            let actor = authenticate(&request, &state, endpoints::WORKFLOW_JOB_HEARTBEAT).await?;
            let command: RenewDelivery<ClaimedTask> = read_json(&request, body).await?;
            let grant = state
                .service
                .manager
                .heartbeat_job(actor.id(), &command.delivery, || revalidate(&state, &actor))
                .await?;
            let renewal: Option<RenewedTask> = match command.task {
                Some(reported) => {
                    let app = grant.delivery().job.app_id.clone();
                    let claim = TaskClaim::resume(reported, grant.delivery().clone(), started)
                        .map_err(journal_error)?;
                    let journal = journal(&state, &app).await?;
                    Some(
                        journal
                            .heartbeat_job(&claim, &grant)
                            .await
                            .and_then(|renewal| renewal.reported())
                            .map_err(journal_error)?,
                    )
                }
                None => None,
            };
            Ok(RenewedDelivery {
                lease: grant.lease()?,
                renewal,
            })
        }
        .await,
    )
}

/// Settle a delivery, either with an outcome the caller's own journal committed
/// or by committing the execution it reports and settling what that produced.
///
/// THE JOURNAL COMMITS FIRST for the reported-execution half, because its commit
/// is what decides the outcome the queue is settled with. A failure between the
/// halves leaves the journal holding a receipt whose delivery is unsettled, which
/// the caller recovers by reading that receipt and settling it as the other half
/// of this endpoint.
async fn settle(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    let started = Instant::now();
    respond(
        async {
            let actor = authenticate(&request, &state, endpoints::WORKFLOW_JOB_SETTLE).await?;
            let command: SettleDelivery<ReportedExecution> = read_json(&request, body).await?;
            match command.reported().map_err(|_| Error::Invalid)? {
                Reported::Outcome(outcome) => Ok(SettledDelivery {
                    settlement: settled(
                        &state,
                        &actor,
                        &Settlement {
                            delivery: command.delivery.clone(),
                            outcome: outcome.clone(),
                            successors: command.successors.clone(),
                        },
                    )
                    .await?,
                    receipt: None,
                }),
                Reported::Execution(reported) => {
                    let journal = journal(&state, &command.delivery.job.app_id).await?;
                    let grant = ReportedGrant::resume(
                        command.delivery.clone(),
                        reported.grant_ms,
                        started,
                    )
                    .map_err(journal_error)?;
                    let claim =
                        TaskClaim::resume(reported.task.clone(), command.delivery.clone(), started)
                            .map_err(journal_error)?;
                    let receipt = journal
                        .complete_job(&claim, &grant, reported.execution.clone())
                        .await
                        .map_err(journal_error)?;
                    let settlement = receipt.settlement(&grant).map_err(journal_error)?;
                    Ok(SettledDelivery {
                        settlement: settled(&state, &actor, &settlement).await?,
                        receipt: Some(receipt),
                    })
                }
            }
        }
        .await,
    )
}
