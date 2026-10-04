use super::{authorization, read_json, respond, LocatePayload};
use crate::{auth::VerifiedWorker, coordinator::Error, SharedState};
use ntex::web::{self, types::State};
use std::{
    cell::Cell,
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId,
    service_identity::{endpoints, ServiceEndpoint},
    workflow_coordination::{
        PayloadLocation, PayloadReservation, PinnedDeployment, ReadTaskPayload, ReservePayload,
        ResolveTaskExecutable, WorkerId,
    },
    workflow_jobs::{ClaimJobs, ClaimedJobs, Delivery, JobSpec, JournalSettlement},
    workflow_policy::MAX_JOURNAL_BYTES_CEILING,
};
use zeroship_workflow::{
    service::{
        delivery::{
            AcceptedJob, ClaimedTask, DeferredReason, DeliveredTask, JobAcceptance, JobReceipt,
            RenewedTask, ReportedExecution, ReportedGrant, TaskClaim,
        },
        AppWorkflows, TaskToken, WorkerIdentity,
    },
    WorkflowServiceError,
};
use zeroship_workflow_client::{
    ClaimedDelivery, GiveBackReason, JobReceiptQuery, ReleaseDelivery, RenewDelivery,
    RenewedDelivery, SettleDelivery,
};
use zeroship_workflow_manager::{
    coordinator::{Admission, ClaimSkipReason, ZoneClaim},
    DeliveryGrant, Error as NativeError, GiveBack,
};

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
/// OBSERVING THE APP IS POLICY I/O, SO THIS IS FENCED OFF THE BODY ALONE. Binding
/// upserts the app's policy ledger row and asks Control for its facts, whose
/// refusals differ by whether the app exists -- an oracle if any caller could
/// name any app. Every route therefore proves against the queue, under the
/// credential that verified the request, that the caller holds the delivery it
/// names (or, for the task routes that carry no delivery, a live delivery of
/// the app) BEFORE reaching here; the claim reaches it only for a grant it just
/// committed in the caller's own zone.
/// The app selects which journal to ask; it is never itself the authorization.
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
        WorkflowServiceError::Internal(_)
            | WorkflowServiceError::InvalidResponse(_)
            | WorkflowServiceError::Unavailable(_)
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
        // delivery: nothing on this path carries one to be fenced. A peer reply
        // that contradicts its request is likewise a host condition with nothing
        // the caller can act on.
        WorkflowServiceError::IngressFenced(_)
        | WorkflowServiceError::InvalidResponse(_)
        | WorkflowServiceError::Internal(_) => Error::Unavailable,
    }
}

/// Bind the journal a task call names, for a caller holding a live delivery of
/// that app.
///
/// A task call names an app and a task credential and no delivery, so the fence
/// before the journal is the queue's record that the caller holds a live lease
/// on some job of that app: every task credential is issued under one, and the
/// journal task's deadline never outlives the lease it was issued under. A body
/// naming any other app is refused `Denied` before the policy source is asked
/// to observe it.
async fn task_journal(
    state: &SharedState,
    actor: &VerifiedWorker,
    app: &AppId,
) -> Result<AppWorkflows, Error> {
    state
        .service
        .manager
        .queue()
        .require_live_holder(actor.id(), app)
        .await?;
    journal(state, app).await
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

/// Refuse a body naming a delivery the verified worker was not handed.
///
/// THE CREDENTIAL NAMES THE HOLDER; THE BODY ONLY NAMES THE DELIVERY. The journal
/// looks a task up by the delivery's worker, so a body naming another worker,
/// with that worker's task token, would otherwise act on that worker's task.
fn signed_for(actor: &VerifiedWorker, delivery: &Delivery) -> Result<(), Error> {
    if &delivery.worker_id != actor.id() {
        return Err(Error::Denied);
    }
    Ok(())
}

/// The journal's receipt for `job`, which the caller has already been proved to
/// hold.
async fn journal_receipt(
    state: &SharedState,
    job: &JobSpec,
) -> Result<Option<JobReceipt>, Error> {
    journal(state, &job.app_id)
        .await?
        .job_receipt(job)
        .await
        .map_err(journal_error)
}

/// The queue half of a settlement, whichever half produced its outcome.
async fn settled(
    state: &SharedState,
    actor: &VerifiedWorker,
    settlement: &JournalSettlement,
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

/// Claim a batch of deliveries across the caller's zone and, for the one
/// operation that hands out a task, accept each into this service's journal
/// under the grant just committed.
///
/// THE ZONE COMES FROM THE CREDENTIAL. The verified instance row names the zone
/// the claim pages, so a body cannot widen it, and an app of another zone is
/// never visited, observed or named to Control.
///
/// THE QUEUE COMMITS FIRST. The claim transaction numbers the attempt and opens
/// the recovery responsibility an intent-producing job needs, so a journal that
/// hands out a task is never followed by a queue rollback that would leave the
/// task authorized by nothing. The wire lease is taken LAST, after the journal
/// work, so the authority the caller receives is what is actually left rather
/// than what was left before this service did its own I/O.
///
/// A GRANT THE CALLER CANNOT USE IS GIVEN BACK IN THE SAME REQUEST. A journal
/// deferral, a journal that could not be reached for that app, and a grant the
/// journal work exhausted each return the row rather than reaching the caller,
/// and none of them fails the batch: they are per-app outcomes, and an error
/// reply would strand every delivery this request already committed. A row the
/// give-back cannot return - its lease already lapsed, its app is busy, or the
/// claim's give-back deadline passed - is redelivered by that lapse, which is
/// the recovery an unreachable worker gets.
///
/// THE BUDGET STARTS AT ARRIVAL and every step is inside it. The deadlines are
/// taken once, from the instant this request arrived, so authentication and
/// the body read are spent from them. Each grant's journal acceptance runs
/// inside the claim, before the next app is tried, and every give-back, here
/// and in the claim, ends by the claim's give-back deadline without waiting
/// for an app's lock, so no reply leaves after the caller has stopped waiting
/// for it.
async fn claim(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    let arrival = Instant::now();
    respond(
        async {
            let actor = authenticate(&request, &state, endpoints::WORKFLOW_JOB_CLAIM).await?;
            let command: ClaimJobs = read_json(&request, body).await?;
            let policies = state.policy_source.as_ref().ok_or(Error::Unavailable)?;
            let manager = &state.service.manager;
            let deadline = manager.claim_deadline(arrival, &command)?;
            let claim = ZoneClaim {
                worker: actor.id(),
                zone: actor.zone(),
                request: &command,
                deadline,
            };
            let room = ReplyRoom::new(ClaimJobs::MAX_REPLY_BYTES);
            let (batch, report) = manager
                .claim_in_zone(
                    &claim,
                    policies.as_ref(),
                    || revalidate(&state, &actor),
                    |grant| admit(&state, &actor, &room, grant),
                )
                .await?;
            for skipped in report.skipped {
                tracing::debug!(
                    app_id = %skipped.app_id.as_str(),
                    reason = ?skipped.reason,
                    "workflow claim skipped app"
                );
            }
            for (app, error) in report.lapsing {
                tracing::debug!(
                    app_id = %app.as_str(),
                    ?error,
                    "workflow claim left an unusable delivery to lapse"
                );
            }
            let mut deliveries = Vec::with_capacity(batch.grants.len());
            for (grant, accepted) in batch.grants {
                match grant.lease() {
                    Ok(lease) => deliveries.push(ClaimedDelivery { lease, accepted }),
                    Err(_) => give_back(&state, &actor, &claim, &grant).await,
                }
            }
            Ok(ClaimedJobs {
                deliveries,
                after: batch.after,
                lap_complete: batch.lap_complete,
            })
        }
        .await,
    )
}

/// The service's half of one grant its claim committed: the zone check again,
/// the journal's acceptance, and room in the reply.
///
/// THE ZONE IS CHECKED HERE AS WELL AS IN THE CLAIM. The claim passes a deleted
/// app or one of another zone before locking it, but the journal is what would
/// execute the job, so it accepts nothing until the verified worker's zone is
/// admitted for that app by an observation this service took itself. A refusal
/// gives the row back rather than failing the batch.
///
/// A TASK THE CALLER WILL NOT RECEIVE IS RELEASED WITH ITS ROW, while the grant
/// it was accepted under is still live: a task left to lapse on its own
/// deadline would hold one of the app's running units until then and count
/// against its run as a dispatch that stalled. That covers a reply with no room
/// left. A grant the reply finds exhausted is past that point - its task's
/// deadline was bounded by the same lease - so both halves are redelivered by
/// the lapse.
async fn admit(
    state: &SharedState,
    actor: &VerifiedWorker,
    room: &ReplyRoom,
    grant: DeliveryGrant,
) -> Admission<Option<AcceptedJob>> {
    let app = &grant.delivery().job.app_id;
    let admitted = match state.policy_source.as_ref() {
        Some(source) => source
            .observe(app)
            .await
            .and_then(|observation| observation.admits_zone(actor.zone())),
        None => Err(NativeError::Unavailable),
    };
    if let Err(error) = admitted {
        return Admission::GiveBack {
            reason: if error == NativeError::Denied {
                ClaimSkipReason::Denied
            } else {
                ClaimSkipReason::Unavailable
            },
            defer: GiveBack::Backoff,
        };
    }
    if !grant.delivery().job.operation.accepts_execution() {
        return room.fit(&grant, None);
    }
    let journal = match journal(state, app).await {
        Ok(journal) => journal,
        Err(error) => return refused(app, error),
    };
    let acceptance = match journal.accept_job(&grant).await {
        Ok(acceptance) => acceptance,
        Err(error) => return refused(app, journal_error(error)),
    };
    let task = match &acceptance {
        JobAcceptance::Execute(task) => Some(DeliveredTask::clone(task)),
        JobAcceptance::Settled(_) | JobAcceptance::Deferred { .. } => None,
    };
    let admitted = match acceptance {
        JobAcceptance::Deferred { reason } => {
            return Admission::GiveBack {
                reason: ClaimSkipReason::Deferred,
                defer: deferral(state, &grant, &reason).await,
            }
        }
        accepted => match accepted.reported() {
            Ok(accepted) => room.fit(&grant, Some(accepted)),
            Err(error) => refused(app, journal_error(error)),
        },
    };
    if let (Admission::Full | Admission::GiveBack { .. }, Some(task)) = (&admitted, &task) {
        if let Err(error) = journal.release_job(task, &grant).await {
            tracing::debug!(
                app_id = %app.as_str(),
                code = error.code(),
                "workflow claim left an unsent task to lapse"
            );
        }
    }
    admitted
}

/// A grant the journal could not take: back with a growing pause.
fn refused(app: &AppId, error: Error) -> Admission<Option<AcceptedJob>> {
    tracing::warn!(
        app_id = %app.as_str(),
        %error,
        "workflow claim could not accept a delivery into the journal"
    );
    Admission::GiveBack {
        reason: ClaimSkipReason::Refused,
        defer: GiveBack::Backoff,
    }
}

/// What one claim reply still has room for under [`ClaimJobs::MAX_REPLY_BYTES`].
///
/// Each delivery is measured as it would be encoded, with its lease measured
/// when it is admitted. The reply measures every lease again, later, and a
/// later measure carries no more digits, so the sum is an upper bound on what
/// the reply holds. The first delivery always fits: a reply that could carry
/// none would leave an app with a large journal unclaimable.
struct ReplyRoom {
    left: Cell<usize>,
    empty: Cell<bool>,
}

impl ReplyRoom {
    /// Room under `limit`, less the reply's envelope at its widest: a cursor
    /// naming an app and the longer spelling of `lapComplete`.
    fn new(limit: usize) -> Self {
        let envelope = serde_json::to_vec(&ClaimedJobs::<AcceptedJob> {
            deliveries: Vec::new(),
            after: Some(AppId::mint()),
            lap_complete: false,
        })
        .map_or(limit, |encoded| encoded.len());
        Self {
            left: Cell::new(limit.saturating_sub(envelope)),
            empty: Cell::new(true),
        }
    }

    /// Take room for `delivery` and its separator, or report that none is left.
    fn take(&self, delivery: &ClaimedDelivery<AcceptedJob>) -> bool {
        let Ok(encoded) = serde_json::to_vec(delivery) else {
            return false;
        };
        let size = encoded.len().saturating_add(1);
        if self.empty.replace(false) {
            self.left.set(self.left.get().saturating_sub(size));
            return true;
        }
        if size > self.left.get() {
            return false;
        }
        self.left.set(self.left.get() - size);
        true
    }

    /// Admit `grant` with its journal half if the reply has room for it.
    fn fit(
        &self,
        grant: &DeliveryGrant,
        accepted: Option<AcceptedJob>,
    ) -> Admission<Option<AcceptedJob>> {
        let Ok(lease) = grant.lease() else {
            return Admission::GiveBack {
                reason: ClaimSkipReason::Unavailable,
                defer: GiveBack::Backoff,
            };
        };
        let delivery = ClaimedDelivery { lease, accepted };
        if self.take(&delivery) {
            Admission::Deliver(delivery.accepted)
        } else {
            Admission::Full
        }
    }
}

/// The pause a short-lived refusal earns: the app is at its concurrency cap,
/// which the next settlement of one of its runs lifts.
const AT_CAP_BACKOFF: Duration = Duration::from_millis(100);

/// How long a deferred row stays unclaimable, by why the journal deferred it.
///
/// A run that is not due waits exactly until it is. Policy that refuses
/// dispatch is not looked at again before the observation it came from lapses,
/// so the row waits for that observation's validity. A missing deployment is
/// the one condition nothing here can predict the end of, so its pause grows
/// with every consecutive back-off.
async fn deferral(state: &SharedState, grant: &DeliveryGrant, reason: &DeferredReason) -> GiveBack {
    match reason {
        DeferredReason::NotDue { until } => GiveBack::Exact(until.get()),
        DeferredReason::AtCap => GiveBack::After(AT_CAP_BACKOFF),
        DeferredReason::PolicyOff => {
            let observed = match state.policy_source.as_ref() {
                Some(source) => source.observe(&grant.delivery().job.app_id).await.ok(),
                None => None,
            };
            observed
                .map(|observation| {
                    observation
                        .expires_at()
                        .saturating_duration_since(Instant::now())
                })
                .filter(|remaining| !remaining.is_zero())
                .map_or(GiveBack::Backoff, GiveBack::After)
        }
        DeferredReason::DeploymentUnavailable => GiveBack::Backoff,
    }
}

/// Return a grant the reply found exhausted, never failing the batch it came
/// from, within the time the claim keeps for its give-backs.
async fn give_back(
    state: &SharedState,
    actor: &VerifiedWorker,
    claim: &ZoneClaim<'_>,
    grant: &DeliveryGrant,
) {
    if let Err(error) = state
        .service
        .manager
        .give_back_claimed(claim, grant.delivery(), GiveBack::Backoff, || {
            revalidate(state, actor)
        })
        .await
    {
        tracing::debug!(
            app_id = %grant.delivery().job.app_id.as_str(),
            ?error,
            "workflow claim left an unusable delivery to lapse"
        );
    }
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

/// Settle a delivery with the outcome the journal decides: by committing the
/// execution the body reports, or, for a body that reports none, from the
/// receipt the journal already holds for the job.
///
/// THE OUTCOME NEVER COMES FROM THE CALLER, and neither do successors. A holder
/// reports what its executor produced; the journal's commit decides what that
/// means for the job, and the queue is settled with that decision and nothing
/// else. A journal receipt carries no successors, so a settlement publishes none.
///
/// THE JOURNAL COMMITS FIRST for an execution, because its commit is what decides
/// the outcome. A failure between the halves leaves the journal holding a receipt
/// whose delivery is unsettled, and the holder recovers it by sending the same
/// delivery again with no execution.
///
/// THE QUEUE FENCE COMES FIRST FOR EVERY BODY. Reaching the journal observes the
/// app's policy, so a caller whose delivery is not the queue's latest for that
/// job is refused before any journal is asked -- otherwise the refusal would
/// differ by whether the app exists in Control, and a receipt read several kinds
/// answer would still take that app's journal lock.
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
            signed_for(&actor, &command.delivery)?;
            state
                .service
                .manager
                .queue()
                .require_latest_delivery(&command.delivery)
                .await?;
            let Some(reported) = &command.execution else {
                let receipt = journal_receipt(&state, &command.delivery.job)
                    .await?
                    .ok_or(Error::Conflict)?;
                let grant = ReportedGrant::resume(command.delivery.clone(), None, started)
                    .map_err(journal_error)?;
                let settlement = receipt
                    .settlement(&grant)
                    .map_err(|refusal| journal_error(refusal.into()))?;
                return settled(&state, &actor, &settlement).await;
            };
            let journal = journal(&state, &command.delivery.job.app_id).await?;
            let grant = ReportedGrant::resume(command.delivery.clone(), reported.grant_ms, started)
                .map_err(journal_error)?;
            let claim = TaskClaim::resume(reported.task.clone(), command.delivery.clone(), started)
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
            // The journal receipt does not cross back. Its two fields are the
            // logical job the caller sent and the outcome the settlement receipt
            // already carries, so a second copy would be a half the caller has
            // nothing to check it against.
            let settlement = receipt
                .settlement(&grant)
                .map_err(|refusal| journal_error(refusal.into()))?;
            settled(&state, &actor, &settlement).await
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
/// The APP in the body selects which journal to ask and grants nothing by
/// itself. It must be an app the caller holds a live delivery of
/// ([`task_journal`]), and within it the task credential is the authority: the
/// journal holds only its hash, so a caller naming a task it was not handed
/// finds no row.
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
            let journal = task_journal(&state, &actor, &command.app_id).await?;
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
            let journal = task_journal(&state, &actor, &command.app_id).await?;
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

/// Give a delivery back without settling it, with the journal task held under
/// it when the caller names one.
///
/// THE DELIVERY STAYS UNSETTLED, deliberately. A release gives up work the
/// holder cannot do - creator work it cannot finish, or an app it could not
/// prepare; the journal marks the task released and makes the run due, and the
/// queue row returns to `ready` in the same request rather than staying leased
/// until its lease lapses. Settling here would report an outcome no execution
/// produced.
///
/// THE REASON DECIDES WHAT THE ROW OWES. An app the holder could not prepare ran
/// nothing, so its row counts no attempt and stays unclaimable for a pause that
/// grows with every consecutive back-off, and an app no worker can prepare does
/// not cycle through the zone at claim speed. An attempt that began and was
/// interrupted - it failed, was told to stop, or its host drained - did run, so
/// its row is claimable at once and the attempt counts toward the delivery
/// budget, once: an execution that fails before its first renewal would
/// otherwise be redelivered without end.
///
/// NOT FENCED ON DEPLOYMENT ADMISSIBILITY, unlike a completion, and that is a
/// property rather than an omission. A release commits no execution, so there is
/// nothing to refuse; and refusing it would be actively worse -- a holder whose
/// deployment was just parked could not give the work back, and the task would
/// sit leased until its deadline lapsed instead of reopening at once.
/// `tasks::assign` re-checks deployment availability before the next dispatch, so
/// the work is not lost and is not replayed against an inadmissible deployment.
///
/// AUTHORIZED BY THE TASK TOKEN AND THE CREDENTIAL TOGETHER. The journal finds
/// the task by the delivery's worker, so the body's worker has to be the one
/// that signed the request; a token alone would let any worker holding it give
/// back another worker's task.
async fn release(
    request: web::HttpRequest,
    state: State<SharedState>,
    body: web::types::Payload,
) -> web::HttpResponse {
    let started = Instant::now();
    respond(
        async {
            let actor = authenticate(&request, &state, endpoints::WORKFLOW_JOB_RELEASE).await?;
            let command: ReleaseDelivery<ClaimedTask> = read_json(&request, body).await?;
            signed_for(&actor, &command.delivery)?;
            // A release reaches the journal only for a delivery the queue
            // currently holds for the caller, fenced before any policy I/O.
            state
                .service
                .manager
                .queue()
                .require_latest_delivery(&command.delivery)
                .await?;
            if let Some(task) = command.task {
                let journal = journal(&state, &command.delivery.job.app_id).await?;
                let grant = ReportedGrant::resume(
                    command.delivery.clone(),
                    Some(task.remaining_ms),
                    started,
                )
                .map_err(journal_error)?;
                let claim = TaskClaim::resume(task, command.delivery.clone(), started)
                    .map_err(journal_error)?;
                journal
                    .release_job(&claim, &grant)
                    .await
                    .map_err(journal_error)?;
            }
            let defer = match command.reason {
                GiveBackReason::PreparationFailed => GiveBack::Backoff,
                GiveBackReason::Interrupted => GiveBack::Interrupted,
                GiveBackReason::Unsent => GiveBack::Unsent,
            };
            state
                .service
                .manager
                .give_back_job(actor.id(), &command.delivery, defer, || {
                    revalidate(&state, &actor)
                })
                .await
                .map_err(Error::from)
        }
        .await,
    )
}

/// Read the committed outcome of one logical job.
///
/// ADDRESSED BY THE JOB, and authorized by the queue's record of who holds it
/// rather than by a task credential: there is no task to present once an attempt
/// has committed, which is exactly the case this read exists for. The caller has
/// to be the worker the queue last delivered the job to, and that is proved
/// BEFORE the journal is asked, so a caller that does not hold the job reaches no
/// journal at all -- not its answer, and not the app lock several kinds take to
/// give one. A holder superseded by a later claim is refused as `Conflict`, the
/// same answer a superseded delivery gets everywhere else.
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
            let actor = authenticate(&request, &state, endpoints::WORKFLOW_JOB_RECEIPT).await?;
            let command: JobReceiptQuery = read_json(&request, body).await?;
            state
                .service
                .manager
                .queue()
                .require_latest_holder(actor.id(), &command.job)
                .await?;
            journal_receipt(&state, &command.job).await
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
            let journal = task_journal(&state, &actor, &command.app_id).await?;
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

#[cfg(test)]
mod tests;
