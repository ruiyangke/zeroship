use super::{authorization, read_json, respond, LocatePayload};
use crate::{auth::VerifiedWorker, coordinator::Error, SharedState};
use ntex::web::{self, types::State};
use std::time::{Duration, Instant};
use zeroship_core::{
    app_id::AppId,
    service_identity::{endpoints, ServiceEndpoint},
    workflow_coordination::{
        AssignedScope, PayloadLocation, PayloadReservation, PinnedDeployment, ReadTaskPayload,
        ReservePayload, ResolveTaskExecutable, WorkerId,
    },
    workflow_jobs::{Settlement, SubmitJob},
    workflow_policy::MAX_JOURNAL_BYTES_CEILING,
};
use zeroship_workflow::{
    service::{
        delivery::{
            AcceptedJob, ClaimedTask, RenewedTask, ReportedExecution, ReportedGrant, TaskClaim,
        },
        AppWorkflows, TaskToken, WorkerIdentity,
    },
    WorkflowServiceError,
};
use zeroship_workflow_client::{
    ClaimedDelivery, JobReceiptQuery, ReleaseDelivery, RenewDelivery, RenewedDelivery, Reported,
    SettleDelivery,
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
        )
        .service(
            web::resource(endpoints::WORKFLOW_TASK_PAYLOAD.path_template())
                .route(web::post().to(task_payload)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_TASK_EXECUTABLE.path_template())
                .route(web::post().to(task_executable)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_JOB_RELEASE.path_template())
                .route(web::post().to(release)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_JOB_RECEIPT.path_template())
                .route(web::post().to(receipt)),
        )
        .service(
            web::resource(endpoints::WORKFLOW_TASK_PAYLOAD_RESERVE.path_template())
                .route(web::post().to(task_payload_reserve)),
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
/// THE QUEUE COMMITS FIRST. The claim transaction numbers the attempt and opens
/// the recovery responsibility an intent-producing job needs, so a journal that
/// hands out a task is never followed by a queue rollback that would leave the
/// task authorized by nothing. The wire lease is taken LAST, after the journal
/// work, so the authority the caller receives is what is actually left rather
/// than what was left before this service did its own I/O.
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
                Reported::Outcome(outcome) => {
                    settled(
                        &state,
                        &actor,
                        &Settlement {
                            delivery: command.delivery.clone(),
                            outcome: outcome.clone(),
                            successors: command.successors.clone(),
                        },
                    )
                    .await
                }
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
                        .complete_reported_job(
                            &claim,
                            &grant,
                            reported.execution.clone(),
                            &reported.confirmed,
                        )
                        .await
                        .map_err(journal_error)?;
                    // The journal receipt does not cross back. Its two fields are
                    // the logical job the caller sent and the outcome the
                    // settlement receipt already carries, so a second copy would
                    // be a half the caller has nothing to check it against.
                    let settlement = receipt.settlement(&grant).map_err(journal_error)?;
                    settled(&state, &actor, &settlement).await
                }
            }
        }
        .await,
    )
}

/// Locate one object a live dispatch's replay edge names.
///
/// THE WORKER IS NEVER READ FROM THE BODY, and here that is load-bearing twice
/// rather than once. A task row is keyed by (id, worker, token hash), so the
/// identity substituted here is half of the lookup: presenting another worker's
/// task and token finds no row at all. The same substitution is what the run
/// routes make, for the same reason.
///
/// The APP in the body selects which journal to ask and grants nothing. Unlike a
/// placement, it is not a claim this service has to verify: the task credential
/// is the authority, the journal holds only its hash, and a body naming another
/// app reaches a journal where this caller's task does not exist.
///
/// What crosses back is a key and a descriptor. The lock this takes is released
/// before the caller opens the object, which is the point of splitting it -- in
/// process the object open happens inside `lock_app_state` and `lock_run`, so an
/// object-store round trip serializes against every journal mutation for the app.
async fn task_payload(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let actor = authenticate(&request, &state, endpoints::WORKFLOW_TASK_PAYLOAD).await?;
            let command: ReadTaskPayload = read_json(&request, body).await?;
            let token = TaskToken::try_from(command.token).map_err(|_| Error::Unauthenticated)?;
            let worker = WorkerIdentity::new(actor.id().as_str().to_owned())
                .map_err(|_| Error::Unauthenticated)?;
            let journal = journal(&state, &command.app_id).await?;
            let located: PayloadLocation = journal
                .service()
                .read_task_payload(
                    &worker,
                    &command.task_id,
                    &token,
                    &command.reference,
                    LocatePayload,
                )
                .await
                .map_err(journal_error)?;
            Ok(located)
        }
        .await,
    )
}

/// Resolve which deployment a live dispatch replays against.
///
/// Same authority and same selector split as [`task_payload`]: the task
/// credential in the body authorizes it, the app names the journal, and the
/// worker identity is substituted from the credential rather than read from the
/// body -- which is half the task lookup, so another worker's dispatch finds no
/// row.
///
/// THE ARTIFACT IS NOT HERE AND CANNOT BE. A loaded executable carries the
/// creator's module source under a budget twice `MAX_JOURNAL_BYTES_CEILING`, so
/// a deployment at its permitted size would not fit a reply; and this process
/// holds no artifact store to read one from. What crosses is the pin the journal
/// proved, and the caller loads that deployment from the object store it already
/// binds -- addressed by the deploy hash, so both hosts resolve identical bytes.
async fn task_executable(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let actor = authenticate(&request, &state, endpoints::WORKFLOW_TASK_EXECUTABLE).await?;
            let command: ResolveTaskExecutable = read_json(&request, body).await?;
            let token = TaskToken::try_from(command.token).map_err(|_| Error::Unauthenticated)?;
            let worker = WorkerIdentity::new(actor.id().as_str().to_owned())
                .map_err(|_| Error::Unauthenticated)?;
            let journal = journal(&state, &command.app_id).await?;
            let pinned: PinnedDeployment = journal
                .service()
                .resolve_task_executable(&worker, &command.task_id, &token)
                .await
                .map_err(journal_error)?;
            Ok(pinned)
        }
        .await,
    )
}

/// Hand a claimed journal task back without settling its delivery.
///
/// THE DELIVERY STAYS UNSETTLED, deliberately. A release gives up creator work
/// the holder cannot finish; the journal marks the task released and makes the
/// run due, `reclaim` expires that row on the next pass, and the queue
/// redelivers. Settling here would report an outcome no execution produced.
///
/// NOT FENCED ON DEPLOYMENT ADMISSIBILITY, unlike a completion, and that is a
/// property rather than an omission. A release commits no execution, so there is
/// nothing to refuse; and refusing it would be actively worse -- a holder whose
/// deployment was just parked could not give the work back, and the task would
/// sit leased until its deadline lapsed instead of reopening at once.
/// `tasks::assign` re-checks deployment availability before the next dispatch, so
/// the work is not lost and is not replayed against an inadmissible deployment.
async fn release(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    let started = Instant::now();
    respond(
        async {
            let _actor = authenticate(&request, &state, endpoints::WORKFLOW_JOB_RELEASE).await?;
            let command: ReleaseDelivery<ClaimedTask> = read_json(&request, body).await?;
            let journal = journal(&state, &command.delivery.job.app_id).await?;
            let grant =
                ReportedGrant::resume(
                    command.delivery.clone(),
                    Some(command.task.remaining_ms),
                    started,
                )
                .map_err(journal_error)?;
            let claim = TaskClaim::resume(command.task, command.delivery, started)
                .map_err(journal_error)?;
            journal
                .release_job(&claim, &grant)
                .await
                .map_err(journal_error)
        }
        .await,
    )
}

/// Read the committed outcome of one logical job.
///
/// ADDRESSED BY THE JOB, and authorized by the app that job names rather than by
/// a task credential: there is no task to present once an attempt has committed,
/// which is exactly the case this read exists for. What bounds it is the same
/// binding every delivery call takes -- the journal of the app the delivery
/// names -- and `job_receipt` checks the job against that app itself.
///
/// ABSENCE IS A FACT, not a refusal: no attempt has committed one yet, so the
/// reply is a null body rather than an error, and a holder reads it as
/// "settle nothing, retry".
async fn receipt(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let _actor = authenticate(&request, &state, endpoints::WORKFLOW_JOB_RECEIPT).await?;
            let command: JobReceiptQuery = read_json(&request, body).await?;
            let journal = journal(&state, &command.job.app_id).await?;
            journal
                .job_receipt(&command.job)
                .await
                .map_err(journal_error)
        }
        .await,
    )
}

/// Reserve the row an upload will be keyed by.
///
/// Same authority and selector split as [`task_payload`]: the task credential in
/// the body authorizes it, the app names the journal, and the worker identity is
/// substituted from the credential.
///
/// THE BYTES ARE NOT HERE AND NEVER WILL BE. This answers with a payload id, and
/// the caller writes the object to the store it already binds. In one process the
/// staging call holds a lock across that write so collection cannot race a live
/// writer; no request boundary can hold that lock, which is why the reservation
/// and the confirm are two calls and why the confirm is a compare-and-swap
/// against the deadline this returns.
async fn task_payload_reserve(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(
        async {
            let actor =
                authenticate(&request, &state, endpoints::WORKFLOW_TASK_PAYLOAD_RESERVE).await?;
            let command: ReservePayload = read_json(&request, body).await?;
            let token = TaskToken::try_from(command.token).map_err(|_| Error::Unauthenticated)?;
            let worker = WorkerIdentity::new(actor.id().as_str().to_owned())
                .map_err(|_| Error::Unauthenticated)?;
            let journal = journal(&state, &command.app_id).await?;
            let reserved: PayloadReservation = journal
                .service()
                .reserve_task_payload(
                    &worker,
                    &command.task_id,
                    &token,
                    &command.request_id,
                    &command.reference,
                )
                .await
                .map_err(journal_error)?;
            Ok(reserved)
        }
        .await,
    )
}
