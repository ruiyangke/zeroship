# Round 7 artifact: SC-1 executable protocol and SC-2 cancellation linearization

This report supplies the two missing normative artifacts. “MUST”, “MUST NOT”,
and the Rust-like types below are contract text, not implementation suggestions.
The reducer tables are closed: an event is legal only in a listed row; the
illegal matrix supplies the typed error for every other state/event pair.

## Verified baseline and settled inputs

The present transaction implementation deliberately serializes same-app
top-level begins, but admits that cancellation between claim acquisition and
BEGIN completion leaks the claim
(crates/zeroship-data-v8/src/transaction/mod.rs:347-378). The present settle
path can also remove no client, declare success, release the claim, and send no
terminal SQL (crates/zeroship-data-v8/src/transaction/mod.rs:1059-1069).
Temporary client removal is real ownership today and needs an RAII restore guard
(crates/zeroship-data-v8/src/context.rs:66-109;
crates/zeroship-data-v8/src/exec.rs:188-201).

Savepoint rollback is incomplete today: the native orchestrator pops its depth
and sends only ROLLBACK TO
(crates/zeroship-data-v8/src/transaction/mod.rs:995-1010), while the driver
correctly documents that ROLLBACK TO leaves the savepoint defined and uses
ROLLBACK TO followed by RELEASE
(libs/compio-postgres/src/transaction.rs:63-82). Effects are physically stored
in one per-app vector, with current savepoint watermarks already implementing
release inheritance and rollback truncation
(crates/zeroship-data-v8/src/context.rs:213-231;
crates/zeroship-data-v8/src/context.rs:852-877), and a
confirmed root commit drains all of it
(crates/zeroship-data-v8/src/transaction/mod.rs:1084-1088). Those facts require
the equivalent explicit frame/effect ownership below, so state transitions can
assert conservation rather than relying on an implicit watermark side table.

Fork C is taken as normative. A 128-bit AppIncarnationId is privileged-minted,
persisted with state and epoch, carried in the binding, and checked before data
SQL; the authority domain is the pair (system_identifier, timeline_id);
tombstones are permanent; epoch mismatch means re-resolution while incarnation
mismatch means terminal denial
(docs/proposals/2026-08-26-sc5-service-ownership.md:101-129). Fork B is also
normative: an explicit transaction reads authority on a separate platform-role
session and can only tighten its BEGIN ceiling
(docs/proposals/2026-08-26-sc6-ceiling-read-contract.md:81-125). This is not
optional isolation polish: the current BEGIN path installs SET LOCAL ROLE for
the transaction lifetime
(crates/zeroship-data-v8/src/transaction/mod.rs:189-217;
crates/zeroship-data-v8/src/transaction/mod.rs:523-540).

# Artifact 1: SC-1 executable transaction protocol

## 1. Identity, admission, and owned record

~~~rust
struct AuthorityDomain {
    system_identifier: SystemIdentifier,
    timeline_id: TimelineId,
}

struct AppAuthority {
    app_id: AppId,
    domain: AuthorityDomain,
    incarnation: AppIncarnationId, // opaque 128-bit token
}

enum LifecycleState {
    Stable,
    Changing,
    Deprovisioned, // permanent tombstone
}

struct AuthorityObservation {
    app: AppAuthority,
    state: LifecycleState,
    epoch: SchemaEpoch,
    ceiling: MaskCeiling,
}

struct TxKey {
    runtime_instance_id: RuntimeInstanceId,
    tx_id: TxId,
    app: AppAuthority,
}

struct TxLocator {
    runtime_instance_id: RuntimeInstanceId,
    tx_id: TxId,
}

struct FrameCloseAttemptId(u128);
struct RootSettleAttemptId(u128);

enum BackendAdmissionScope {
    Postgres,
    Sqlite { thread_resource_id: DbThreadResourceId },
}

enum AdmissionKey {
    Postgres {
        runtime_instance_id: RuntimeInstanceId,
        app_id: AppId,
        incarnation: AppIncarnationId,
    },
    Sqlite {
        thread_resource_id: DbThreadResourceId,
        app_id: AppId,
        incarnation: AppIncarnationId,
    },
}

enum AdmissionError {
    QueueClosed,
    // Preserves the total, typed reserve failure; it is never stringified.
    Reserve(ActorError),
}

fn map_reserve_admission_error(error: ActorError) -> AdmissionError {
    AdmissionError::Reserve(error)
}

struct TxEntry {
    key: TxKey,
    admission: AdmissionKey,
    // Installed from ReservationHandle before Prepare can execute. Every
    // backend-death/fence event must name this exact generation.
    backend_actor_generation: Option<u64>,
    // Retained from successful reserve through Settled replay and released by
    // Forget. Its Drop can therefore never turn a live reservation into an
    // untracked CallerDrop cancellation.
    backend_reservation: Option<BackendReservationLease>,
    state: TxState,
    session: SessionOwner,
    claim: Option<ClaimGuard>,
    frames: Vec<Frame>,                 // root first, current frame last
    resolved_epoch: SchemaEpoch,
    begin_epoch: Option<SchemaEpoch>,
    begin_ceiling: Option<MaskCeiling>,
    effective_ceiling: Option<MaskCeiling>,
    next_frame_sequence: u64,           // monotonic; never depth-reused
    execution_timeout: Duration,
    deadline_at: Option<Instant>,
    // The exact Arc is also in ReservationControl. There is no reducer copy
    // and actor copy that can diverge.
    deadline_slots: Arc<ExplicitDeadlineSlots>,
    terminal_sql_timeout: Duration,
    terminal_interrupt_grace: Duration,
    // Minted at Create and installed in the actor control at reserve time,
    // before any cancellation can win the start gap.
    cancellation_token: CommandToken,
    // The same Arc is installed in the backend reservation control. SC-1 and
    // SC-2 never own independent result/fence gates.
    terminal_cutoff: Option<Arc<TerminalCutoffGate<RootFinishResult>>>,
    cancel_cutoff: Option<Arc<TerminalCutoffGate<CancelAck>>>,
    // Insert before accepting any reply-bearing request. Entries are never
    // removed until Forget, including after their reply is sent, so a reused
    // id cannot acquire a second responder or replay an effect-bearing action.
    retained_request_ids: HashSet<RequestId>,
    start_waiter: Option<StartWaiter>,
    terminal_waiters: Vec<TerminalWaiter>,
    deferred_replies: OutstandingReplies,
    terminal_refs: usize,
}

struct TerminalRecordRef {
    key: TxKey,
    registry: TxRegistrySender,
    released: bool,
}

fn acquire_terminal_record_ref_exact(
    entry: &mut TxEntry,
    registry: TxRegistrySender,
) -> Result<TerminalRecordRef, TxProtocolError> {
    if !matches!(entry.state, TxState::Settled { .. }) {
        return Err(TxProtocolError::TransactionNotSettled);
    }
    entry.terminal_refs = entry.terminal_refs.checked_add(1)
        .expect("terminal reference count overflow");
    Ok(TerminalRecordRef {
        key: entry.key.clone(),
        registry,
        released: false,
    })
}

impl Drop for TerminalRecordRef {
    fn drop(&mut self) {
        if !self.released {
            // Ordered through the same keyed reducer. The decrement is retained
            // even if Forget raced first; that Forget returned REF purely.
            self.registry.enqueue_infallible(RegistryEvent::Routed {
                key: self.key.clone(),
                event: TxEvent::ReleaseTerminalRef,
            });
            self.released = true;
        }
    }
}

struct ExplicitDeadlineSlots {
    route: DeadlineRoute,
    state: Mutex<ExplicitDeadlineState>,
}

struct DeadlineRoute {
    key: TxKey,
    event_sender: TxRegistrySender,
}

impl DeadlineRoute {
    fn schedule(&self, kind: DeadlineKind, generation: u64, at: Instant) {
        spawn_compio_timer(at, self.event_sender.clone(), RegistryEvent::Routed {
            key: self.key.clone(),
            event: TxEvent::DeadlineFired { kind, generation },
        });
    }
}

enum ExplicitDeadlineState {
    Disarmed,
    Armed {
        kind: DeadlineKind,
        generation: u64,
        at: Instant,
    },
    // The Registry reducer consumed the timer and owns the right to perform
    // its exact interrupt/fence transition. Duplicate delivery is stale.
    Fired {
        kind: DeadlineKind,
        generation: u64,
    },
}

impl ExplicitDeadlineSlots {
    fn arm_initial(&self, kind: DeadlineKind, generation: u64, at: Instant) {
        let mut state = self.state.lock();
        assert!(matches!(*state, ExplicitDeadlineState::Disarmed));
        *state = ExplicitDeadlineState::Armed { kind, generation, at };
        self.route.schedule(kind, generation, at);
    }

    fn claim_fire(
        &self,
        kind: DeadlineKind,
        generation: u64,
    ) -> Result<(), TxProtocolError> {
        let mut state = self.state.lock();
        match *state {
            ExplicitDeadlineState::Armed {
                kind: current,
                generation: current_generation,
                ..
            } if current == kind && current_generation == generation => {
                *state = ExplicitDeadlineState::Fired { kind, generation };
                Ok(())
            }
            _ => Err(TxProtocolError::StaleTransactionDeadline),
        }
    }

    fn replace_current(
        &self,
        expected_kind: DeadlineKind,
        next_kind: DeadlineKind,
        next_generation: u64,
        next_at: Instant,
    ) -> Result<(DeadlineKind, u64), TxProtocolError> {
        let mut state = self.state.lock();
        let (kind, generation) = match *state {
            ExplicitDeadlineState::Armed { kind, generation, .. }
            | ExplicitDeadlineState::Fired { kind, generation }
                if kind == expected_kind => (kind, generation),
            _ => return Err(TxProtocolError::StaleTransactionDeadline),
        };
        *state = ExplicitDeadlineState::Armed {
            kind: next_kind,
            generation: next_generation,
            at: next_at,
        };
        self.route.schedule(next_kind, next_generation, next_at);
        Ok((kind, generation))
    }

    fn replace_with_retirement(
        &self,
        next_generation: u64,
        next_at: Instant,
    ) -> Result<(DeadlineKind, u64), TxProtocolError> {
        let mut state = self.state.lock();
        let (kind, generation) = match *state {
            ExplicitDeadlineState::Armed { kind, generation, .. }
            | ExplicitDeadlineState::Fired { kind, generation }
                if matches!(kind,
                    DeadlineKind::Execution
                    | DeadlineKind::CancellationSql
                    | DeadlineKind::CancellationHardStop
                    | DeadlineKind::TerminalSql
                    | DeadlineKind::TerminalHardStop) => (kind, generation),
            // No admitted terminal path reaches quarantine with a Disarmed
            // machine or an already-RetirementFence generation.
            _ => return Err(TxProtocolError::StaleTransactionDeadline),
        };
        *state = ExplicitDeadlineState::Armed {
            kind: DeadlineKind::RetirementFence,
            generation: next_generation,
            at: next_at,
        };
        self.route.schedule(
            DeadlineKind::RetirementFence, next_generation, next_at,
        );
        Ok((kind, generation))
    }

    fn rearm_retirement(
        &self,
        fired_generation: u64,
        next_generation: u64,
        next_at: Instant,
    ) -> Result<(), TxProtocolError> {
        let mut state = self.state.lock();
        match *state {
            ExplicitDeadlineState::Fired {
                kind: DeadlineKind::RetirementFence,
                generation,
            } if generation == fired_generation => {}
            _ => return Err(TxProtocolError::StaleTransactionDeadline),
        }
        *state = ExplicitDeadlineState::Armed {
            kind: DeadlineKind::RetirementFence,
            generation: next_generation,
            at: next_at,
        };
        self.route.schedule(
            DeadlineKind::RetirementFence, next_generation, next_at,
        );
        Ok(())
    }
}

enum BackendReservationLease {
    Armed(ReservationHandle),
    // Drop emits only ForgetTerminal after refcount reaches zero; it can never
    // publish Cancel.
    Terminal(TerminalReservationLease),
}

impl BackendReservationLease {
    fn control(&self) -> &Arc<ReservationControl> {
        // Both lease forms retain the identical Arc installed by reserve.  A
        // terminal conversion consumes only the armed Drop behavior and its
        // private retirement capability; it does not replace control.
        match self {
            BackendReservationLease::Armed(handle) => &handle.control,
            BackendReservationLease::Terminal(lease) => &lease.control,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct TerminalDeliveryId(u128); // minted with the gate; never reused

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExplicitHardStopKind {
    Root,
    Cancel,
}

struct ExplicitHardStopPermit {
    // Minted from the same never-reused mailbox-delivery namespace.
    permit_id: TerminalDeliveryId,
    kind: ExplicitHardStopKind,
    key: TxKey,
    actor_generation: u64,
    deadline_slots: Arc<ExplicitDeadlineSlots>,
    cutoff_delivery_id: TerminalDeliveryId,
    completion_token: CommandToken,
    fence_token: CommandToken,
    // Registered before the reducer arms TerminalHardStop. It owns the typed
    // prepared fence route independently of this permit/publisher task.
    fence_job: DurableFenceJobHandle,
    // Pinned while and only while the reducer is in HardStopping. This makes
    // every current-permit publication infallible after claim. The reducer
    // drops the permit before entering Cancelling/Quarantining/Settled, breaking
    // the temporary entry -> permit -> mailbox pin cycle.
    mailbox: PinnedRegistryMailbox,
    publish_state: AtomicU8, // HardStopPermitPublishState
}

#[repr(u8)]
enum HardStopPermitPublishState {
    Open,
    ClaimedByPublisher,
    ConsumedByReducer,
}

struct HardStopPermitClaim {
    permit: Arc<ExplicitHardStopPermit>,
    mailbox: PinnedRegistryMailbox,
}

// Pins only the detached keyed-queue core and its dedupe record.  It contains
// no Arc<TxEntry>, ReservationControl, or TerminalCutoffGate, so retaining it
// cannot make the entry -> permit -> mailbox cycle back to the entry.  Forget
// clears every cutoff/permit field before it removes the detached queue key.
#[derive(Clone)]
struct PinnedRegistryMailbox(Arc<DetachedRegistryMailboxCore>);

impl HardStopPermitClaim {
    fn enqueue_preemption_infallible(&self, event: RegistryEvent) {
        self.mailbox.enqueue_once_infallible(self.permit.permit_id, event);
    }
}

impl ExplicitHardStopPermit {
    // Only the ownership-carrying reducer effect has this Arc. All descriptive
    // identity/deadline checks precede this CAS.
    fn try_claim_publisher(self: &Arc<Self>) -> Option<HardStopPermitClaim> {
        if self.permit_id.0 == 0 || self.publish_state.compare_exchange(
            HardStopPermitPublishState::Open as u8,
            HardStopPermitPublishState::ClaimedByPublisher as u8,
            AcqRel,
            Acquire,
        ).is_err() {
            return None;
        }
        Some(HardStopPermitClaim {
            permit: self.clone(),
            mailbox: self.mailbox.clone(),
        })
    }

    // Root preemption is the only branch that does not claim the terminal
    // cutoff. The detached mailbox performs state Open->Claimed and keyed
    // insertion in one non-allocating critical section; a publisher cannot die
    // after making the permit unavailable but before a durable event exists.
    fn claim_and_enqueue_preemption_infallible(
        self: &Arc<Self>,
        event: RegistryEvent,
    ) -> bool {
        self.mailbox.claim_permit_and_enqueue_once_infallible(
            &self.publish_state,
            HardStopPermitPublishState::Open as u8,
            HardStopPermitPublishState::ClaimedByPublisher as u8,
            self.permit_id,
            event,
        )
    }
}

impl TxRegistry {
    // Called under the matching TxEntry reducer lock. Open -> Consumed beats a
    // publisher that has not claimed. Claimed -> Consumed is safe because the
    // already-selected cutoff branch constrains that publisher to this same
    // keyed result, preemption, or fence completion.
    fn consume_hard_stop_permit_exact(
        &self,
        entry: &TxEntry,
        candidate: &Arc<ExplicitHardStopPermit>,
    ) -> Result<(), TxProtocolError> {
        let stored = match &entry.state {
            TxState::Settling { hard_stop, .. } => &hard_stop.permit,
            TxState::HardStopping { permit, .. } => permit,
            TxState::Cancelling {
                phase: CancelCleanupPhase::Awaiting { hard_stop, .. }, ..
            } => &hard_stop.permit,
            TxState::Cancelling {
                phase: CancelCleanupPhase::HardStopping { permit, .. }, ..
            } => permit,
            _ => return Err(TxProtocolError::StaleTransactionCompletion),
        };
        if stored.permit_id != candidate.permit_id
            || !Arc::ptr_eq(stored, candidate)
            || entry.backend_actor_generation != Some(candidate.actor_generation)
        {
            return Err(TxProtocolError::StaleTransactionCompletion);
        }
        loop {
            let observed = candidate.publish_state.load(Acquire);
            if observed == HardStopPermitPublishState::ConsumedByReducer as u8 {
                return Err(TxProtocolError::StaleTransactionCompletion);
            }
            if candidate.publish_state.compare_exchange(
                observed,
                HardStopPermitPublishState::ConsumedByReducer as u8,
                AcqRel,
                Acquire,
            ).is_ok() {
                return Ok(());
            }
        }
    }
}

// Registry mints this private capability while installing ActiveAction::Data.
// The same Arc is copied into ActiveCommandRef, ActorCommand::Execute,
// TerminalDelivery::ExplicitDataAbort, and its hard-stop trigger. Its fields are
// descriptive checks; Arc identity is the unforgeable current-action proof.
struct DataDeliveryPermit {
    permit_id: u128,
    key: TxKey,
    actor_generation: u64,
    data_token: CommandToken,
    cutoff_delivery_id: TerminalDeliveryId,
    // Registry creates this dormant job before it publishes Execute/ActiveData.
    // A failed registration rejects the Execute before data SQL can start.
    fence_job: DurableFenceJobHandle,
}

struct TerminalCutoffGate<T> {
    delivery_id: TerminalDeliveryId,
    state: Mutex<TerminalCutoffState<T>>,
}

// Constructed before a cutoff CAS. Preparation performs every fallible check:
// it validates the typed adapter, derives the immutable actor outcome, pins the
// exact incarnation-qualified Registry mailbox (or actor terminal sink), and
// reserves delivery_id in that endpoint. While the Arc exists the endpoint
// cannot close. enqueue_once and commit_retained are therefore infallible and
// idempotent; recovery can invoke them again after a producer dies.
struct PreparedTerminalDelivery<T> {
    delivery_id: TerminalDeliveryId,
    proof: Arc<T>,
    enqueue_once: Arc<dyn Fn(TerminalDeliveryId, Arc<T>) + Send + Sync>,
    commit_retained: Arc<dyn Fn() + Send + Sync>,
}

impl<T> PreparedTerminalDelivery<T> {
    fn insert_and_commit(&self) {
        // Retention precedes visibility: a consumer can never observe the keyed
        // event before late-Cancel/replay state contains the identical outcome.
        (self.commit_retained)();
        (self.enqueue_once)(self.delivery_id, self.proof.clone());
    }
}

// Endpoint preparation and durable-job registration happen before a hard-stop
// timer is armed. The route contains no semantic owner/attempt snapshot; that
// snapshot is bound later under terminal_owner_gate at the cutoff linearization.
struct PreparedFenceRoute<T> {
    delivery_id: TerminalDeliveryId,
    complete: Arc<dyn Fn(
        &TerminalFenceSnapshot,
        Arc<PhysicalGenerationFenceProof>,
    ) -> Arc<PreparedTerminalDelivery<T>> + Send + Sync>,
}

#[derive(Clone)]
struct TerminalFenceSnapshot {
    id: ReservationId,
    actor_generation: u64,
    full_terminal_word: u8,
    delivery_id: TerminalDeliveryId,
    class: TerminalPublicationClass,
    public_kind: ExplicitHardStopKind,
    attempt: TerminalAttemptSnapshot,
    source: Option<DbError>,
}

#[derive(Clone)]
enum TerminalAttemptSnapshot {
    ExplicitCommit,
    ExplicitRollback,
    ExplicitCancel { cause: CancelCause, phase: CancelPhaseProof },
    ExplicitDataAbort { source: DbError },
    AutocommitSuccess,
    AutocommitFailure { source: DbError },
    AutocommitCancel { cause: CancelCause, phase: CancelPhaseProof },
    // Bound only by generation-death recovery after routing is removed and
    // before any actor owner CAS; never used for an owned terminal attempt.
    AutocommitActorUnavailable,
}

// The supervisor owns the route strongly in a generation-scoped job registry;
// this handle is only an id and has no back-reference to the cutoff. A trigger
// is not armable until registration succeeds, so the one-shot timer can never
// fire and then discover that its endpoint/job could not be prepared.
#[derive(Clone, Copy, PartialEq, Eq)]
struct DurableFenceJobHandle {
    job_id: u128,
    delivery_id: TerminalDeliveryId,
}

enum TerminalCutoffState<T> {
    Open,
    Result {
        delivery: Arc<PreparedTerminalDelivery<T>>,
        delivery_enqueued: bool,
    },
    FencePending {
        job: DurableFenceJobHandle,
        snapshot: TerminalFenceSnapshot,
    },
    FenceResult {
        delivery: Arc<PreparedTerminalDelivery<T>>,
        delivery_enqueued: bool,
    },
}

enum CutoffDecision<T> {
    FenceWon,
    ResultWon(Arc<T>),
}

// Registration is performed at trigger construction:
//   supervisor.register_dormant_fence_job(
//       actor_generation, Arc::downgrade(cutoff), prepared_route)
// returns the handle stored in HardStopTrigger. Registration failure prevents
// the first-stage/SC-1 hard-stop deadline from being armed and returns a typed
// reserve/transition error. activate_infallible is a non-allocating state flip
// in that already-registered job.
impl<T> TerminalCutoffGate<T> {
    fn observe_late_hard_stop(
        &self,
        supervisor: &FenceJobRegistry,
        job: DurableFenceJobHandle,
    ) -> Option<HardStopPublish> {
        let mut state = self.state.lock();
        match &mut *state {
            TerminalCutoffState::Open => None,
            TerminalCutoffState::Result { delivery, delivery_enqueued } => {
                supervisor.cancel_dormant_infallible(job);
                if !*delivery_enqueued {
                    delivery.insert_and_commit();
                    *delivery_enqueued = true;
                }
                Some(HardStopPublish::ResultWon)
            }
            TerminalCutoffState::FencePending { job: current, .. } => {
                if *current == job {
                    supervisor.activate_infallible(job);
                } else {
                    supervisor.reject_job_alias_infallible(job);
                }
                Some(HardStopPublish::Fencing)
            }
            TerminalCutoffState::FenceResult { delivery, delivery_enqueued } => {
                supervisor.finish_or_cancel_infallible(job);
                if !*delivery_enqueued {
                    delivery.insert_and_commit();
                    *delivery_enqueued = true;
                }
                Some(HardStopPublish::Fencing)
            }
        }
    }

    fn publish_prepared_result(
        &self,
        delivery: Arc<PreparedTerminalDelivery<T>>,
    ) -> bool {
        assert_eq!(delivery.delivery_id, self.delivery_id);
        let mut state = self.state.lock();
        match &mut *state {
            TerminalCutoffState::FencePending { .. }
            | TerminalCutoffState::FenceResult { .. } => return false,
            TerminalCutoffState::Result {
                delivery: stored, delivery_enqueued,
            } => {
                // Recovery replays the already-selected prepared delivery; it
                // never substitutes a newly prepared payload with the same id.
                if !*delivery_enqueued {
                    stored.insert_and_commit();
                    *delivery_enqueued = true;
                }
                return true;
            }
            TerminalCutoffState::Open => {}
        }
        *state = TerminalCutoffState::Result {
            delivery: delivery.clone(),
            delivery_enqueued: false,
        };
        delivery.insert_and_commit();
        if let TerminalCutoffState::Result { delivery_enqueued, .. } = &mut *state {
            *delivery_enqueued = true;
        }
        true
    }

    // Caller holds terminal_owner_gate and constructed snapshot from the same
    // full word/attempt/delivery it is arbitrating. No fallible work remains.
    fn fence_or_ensure_result_delivery(
        &self,
        supervisor: &FenceJobRegistry,
        job: DurableFenceJobHandle,
        snapshot: TerminalFenceSnapshot,
    ) -> CutoffDecision<T> {
        assert_eq!(job.delivery_id, self.delivery_id);
        assert_eq!(snapshot.delivery_id, self.delivery_id);
        let mut state = self.state.lock();
        match &mut *state {
            TerminalCutoffState::Open => {
                supervisor.bind_snapshot_infallible(job, snapshot.clone());
                *state = TerminalCutoffState::FencePending { job, snapshot };
                // The registry already owns the route. From this instruction on,
                // actor/publisher death is irrelevant: a supervisor worker or its
                // recovery scan drives this same idempotent job.
                supervisor.activate_infallible(job);
                CutoffDecision::FenceWon
            }
            TerminalCutoffState::Result { delivery, delivery_enqueued } => {
                supervisor.cancel_dormant_infallible(job);
                if !*delivery_enqueued {
                    delivery.insert_and_commit();
                    *delivery_enqueued = true;
                }
                CutoffDecision::ResultWon(delivery.proof.clone())
            }
            TerminalCutoffState::FencePending { job: current, .. } => {
                assert_eq!(*current, job);
                supervisor.activate_infallible(job);
                CutoffDecision::FenceWon
            }
            TerminalCutoffState::FenceResult { delivery, delivery_enqueued } => {
                supervisor.finish_or_cancel_infallible(job);
                if !*delivery_enqueued {
                    delivery.insert_and_commit();
                    *delivery_enqueued = true;
                }
                CutoffDecision::ResultWon(delivery.proof.clone())
            }
        }
    }

    // Called by the supervisor job, not by the actor/publisher task. The job
    // registry supplies the route registered before timer arming.
    fn publish_physical_fence(
        &self,
        supervisor: &FenceJobRegistry,
        job: DurableFenceJobHandle,
        proof: Arc<PhysicalGenerationFenceProof>,
    ) {
        let mut state = self.state.lock();
        match &mut *state {
            TerminalCutoffState::FencePending {
                job: current, snapshot,
            } if *current == job => {
                let authenticated = proof.proof_id.0 == job.job_id
                    && proof.id == snapshot.id
                    && proof.actor_generation == snapshot.actor_generation
                    && proof.cutoff_delivery_id == snapshot.delivery_id
                    && proof.observed_full_word == snapshot.full_terminal_word
                    && proof.class == snapshot.class
                    && proof.public_kind == snapshot.public_kind
                    && proof.seal.authenticates(job, snapshot, proof.proof_id);
                if !authenticated {
                    // Keep FencePending. The durable worker records the fault,
                    // discards this capability, and re-proves physical closure;
                    // an unauthenticated value can never retire the cutoff.
                    supervisor.reject_physical_proof_and_retry_infallible(
                        job, physical_fence_proof_mismatch(),
                    );
                    return;
                }
                let route = supervisor.route_exact::<T>(job);
                let delivery = (route.complete)(snapshot, proof);
                assert_eq!(delivery.delivery_id, self.delivery_id);
                *state = TerminalCutoffState::FenceResult {
                    delivery: delivery.clone(),
                    delivery_enqueued: false,
                };
                supervisor.finish_infallible(job);
                delivery.insert_and_commit();
                if let TerminalCutoffState::FenceResult {
                    delivery_enqueued, ..
                } = &mut *state {
                    *delivery_enqueued = true;
                }
            }
            TerminalCutoffState::Result { delivery, delivery_enqueued }
            | TerminalCutoffState::FenceResult { delivery, delivery_enqueued } => {
                supervisor.finish_or_cancel_infallible(job);
                if !*delivery_enqueued {
                    delivery.insert_and_commit();
                    *delivery_enqueued = true;
                }
            }
            TerminalCutoffState::FencePending { .. }
            | TerminalCutoffState::Open => {
                // No capability is consumed. A physical worker may only finish
                // the exact job whose cutoff already records FencePending.
                supervisor.reject_physical_proof_and_retry_infallible(
                    job, physical_fence_cutoff_mismatch(),
                );
            }
        }
    }
}

// Supervisor restart/death recovery enumerates every active job. If its cutoff
// is FencePending, it re-runs physical fencing and publish_physical_fence with
// the same job id; if Result/FenceResult it finishes the job. Therefore there is
// no FencePending state whose only driver was a dead publisher. Forget is legal
// only after Result/FenceResult and removes the completed job record together
// with the terminal dedupe key.

enum SessionOwner {
    None,
    Acquiring(CommandToken),
    Command {
        token: CommandToken,
        lease: ActorSessionLease,
    },
    Registry(ActorSessionLease),
    Quarantined,
}

// The physical connection/transaction never crosses the actor boundary.  It
// lives in the actor's generation-qualified lease table.  This unforgeable
// reference is the only value moved between Registry state, an Issue effect,
// and the matching completion.  The actor accepts a command only when both the
// lease id and command token equal its table's current owner.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ActorSessionLease {
    actor_generation: u64,
    lease_id: u128,
    phase: ActorSessionPhase,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActorSessionPhase {
    PreBegin,
    Transaction,
}
~~~

`enqueue_once` linearizes at insertion into the reducer mailbox's keyed dedupe
set. The consumer removes the queued event but retains the key in the terminal
record, so an actor-death or hard-stop recovery cannot reinsert it. Forget removes
the proof and its key together. Consequently the transport may retry, but the
reducer observes and applies exactly one logical terminal completion.

Session ownership is likewise executable rather than implicit. The actor owns
the physical resource in a table keyed by `(actor_generation,lease_id)`;
Registry owns only the unique `ActorSessionLease`. An Issue for BEGIN, data,
frame control, root settlement, or cancellation carries that lease and first
changes `SessionOwner::Registry(lease)` to
`SessionOwner::Command{token,lease}`. The actor compares both values before
touching the resource. A matching nonterminal completion changes the same lease
back to Registry (and BEGIN changes only its phase); terminal completion consumes
the actor table entry. Prepare and platform-role AuthorityRead carry no data
lease. No effect or completion creates a second lease or transfers a raw
connection through the mailbox.

Registry owns a primary map by TxLocator and stores the complete TxKey in its
value. Routed lookup first finds (runtime_instance_id,tx_id), then compares the
stored and supplied AppAuthority. Thus a forged old/new authority is a typed
AppIncarnationMismatch rather than an indistinguishable miss. Create derives
AdmissionKey from TxKey plus trusted BackendAdmissionScope: PostgreSQL copies
the key's runtime_instance_id, app_id, and incarnation; SQLite copies the trusted thread
resource id plus app_id and incarnation, deliberately excluding AuthorityDomain
from physical-lane contention. No input can pair one transaction
identity with another admission identity. The admission rules are:

* PostgreSQL: one admitted top-level transaction per
  (runtime_instance_id, app_id, AppIncarnationId). Different isolates do not
  contend. AuthorityDomain remains in TxKey but is not a contention component.
* SQLite: one admitted top-level transaction per
  (DbThreadResourceId, app_id, AppIncarnationId). Isolates sharing that thread
  resource contend even if their authority-domain observation differs. Domain
  remains mandatory in TxKey and every Fork-C authority comparison, but it
  cannot split the single physical SQLite transaction lane. This is Fork A.
* Registry uniqueness and admission contention are independent. A unique tx_id
  never grants admission.
* Full AppAuthority is present in the registry TxKey, every routed operation,
  frame command, completion token, and retained terminal record. Admission
  carries app_id+incarnation but deliberately omits domain because it is a
  physical contention key, not a lifecycle authority key. A bare app_id is
  never sufficient for either purpose.
* AuthorityDomain is supplied by DbService from platform authority outside the
  rewindable per-app state. The actor never reconstructs it from the row that
  holds incarnation/epoch; otherwise PITR would restore the purported fence and
  the value it fences together. This is why Fork C qualifies the token by
  (system_identifier, timeline_id)
  (docs/proposals/2026-08-26-sc5-service-ownership.md:101-119).

The backend divergence above preserves the existing intentional wait
(crates/zeroship-data-v8/src/context.rs:165-193;
crates/zeroship-data-v8/src/transaction/mod.rs:354-378), while correcting the
documents’ old unqualified identities
(docs/proposals/2026-08-26-sc1-transaction-protocol.md:41-53).

## 2. Frame and effect representation

~~~rust
struct Frame {
    id: FrameId,
    parent: Option<FrameId>,
    savepoint: Option<SavepointName>, // None only for root
    status: FrameStatus,
    effects: Vec<PendingEffect>,
}

enum FrameStatus {
    Opening,
    Open,
    Releasing,
    RollingBackTo,
    ReleasingAfterRollbackTo,
}

enum FrameCloseKind {
    Release {
        frame: FrameId,
        value: OwnedJsValue,
    },
    RollbackTo {
        frame: FrameId,
        error: OwnedJsError,
    },
}

enum FrameCloseInput {
    First {
        attempt_id: FrameCloseAttemptId,
        kind: FrameCloseKind,
    },
    Join {
        attempt_id: FrameCloseAttemptId,
    },
}

enum RootSettleInput {
    First {
        attempt_id: RootSettleAttemptId,
        intent: RootIntent,
    },
    Join {
        attempt_id: RootSettleAttemptId,
    },
}

struct FrameWaiter {
    request_id: RequestId,
    reply: Reply<Result<SharedFrameCompletion, TxProtocolError>>,
}

struct FrameOpenWaiter {
    request_id: RequestId,
    reply: Reply<Result<FrameOpenReply, TxProtocolError>>,
}

struct PendingFrameClose {
    attempt_id: FrameCloseAttemptId,
    kind: FrameCloseKind,
    waiters: Vec<FrameWaiter>,
}

struct TerminalWaiter {
    request_id: RequestId,
    reply: Reply<Result<TerminalReply, TxProtocolError>>,
}

struct StartWaiter {
    request_id: RequestId,
    reply: Reply<Result<StartReply, TxProtocolError>>,
}

#[derive(Default)]
struct OutstandingReplies {
    items: Vec<InterruptedReply>,
}

enum InterruptedReply {
    Data {
        request_id: RequestId,
        reply: Reply<Result<DbRows, TxOutcomeError>>,
        original_error: Option<DbError>,
    },
    FrameOpen {
        waiter: FrameOpenWaiter,
        original_error: Option<DbError>,
    },
    FrameClose {
        kind: InterruptedFrameKind,
        waiters: Vec<FrameWaiter>,
        // NeverIssuedPending means CloseFrame was only latched behind another
        // command. No savepoint SQL ran, so terminal aborts project the
        // transaction error rather than a fictitious savepoint-command error.
        provenance: DeferredFrameProvenance,
        // Present for RollbackTo/ReleaseAfterRollbackTo so an owned JS/body or
        // earlier release failure is retained once and shared at drain.
        after: Option<AfterRollbackTo>,
        original_error: Option<DbError>,
    },
}

enum InterruptedFrameKind {
    Release,
    RollbackTo,
}

enum DeferredFrameProvenance {
    NeverIssuedPending,
    CommandIssued,
}

enum AfterRollbackTo {
    BodyRejected(OwnedJsError),
    ReleaseFailed(TxStatementError),
    PoisonRecovery(TxOutcomeError),
}

enum SavepointPriorFailure {
    Body(OwnedJsError),
    Release(DbError),
    Poison(Box<TxOutcomeError>),
}

struct SavepointCleanupFailure {
    prior: SavepointPriorFailure,
    cleanup: DbError,
}

type SharedFrameCompletion = Arc<FrameCompletion>;
type SharedTerminalOutcome = Arc<TerminalOutcome>;
~~~

The frame stack is strict LIFO. Only its last Open frame can start data SQL,
open a child, or close. The root is created only after BEGIN is confirmed. A
child is inserted as Opening before SAVEPOINT is sent and becomes Open only
after that command succeeds.

Effects returned by successful data SQL append to the current frame:

* RELEASE success pops the child and appends the child effects to its parent in
  order. No effect is published.
* ROLLBACK TO success immediately clears the child effects, because the matching
  database changes are then known to be undone. The reducer next sends RELEASE
  for the same savepoint; only its success pops the child.
* ROLLBACK TO failure retains the effects for diagnosis, poisons or quarantines
  the transaction, and publishes nothing.
* RELEASE-after-ROLLBACK failure leaves an empty child frame present and forces
  root cleanup; the already-cleared effects stay discarded.
* Only a confirmed root commit detaches and publishes the root effects. Every
  other terminal outcome discards every frame buffer.

The close reply is also total. A successful second RELEASE maps
`AfterRollbackTo` as follows: BodyRejected -> Rejected(original JS error),
ReleaseFailed -> Failed(SavepointReleaseFailed(original DB error)), and
PoisonRecovery -> Failed(the retained poison error). Any failure of ROLLBACK TO
or of that second RELEASE instead returns exactly
`SavepointRollbackFailed(SavepointCleanupFailure { prior, cleanup })`; `prior`
retains the body/release/poison failure and `cleanup` retains the failing cleanup
statement. Thus a second RELEASE failure has one wire code and never discards
the earlier cause. Its health decides Poisoned versus quarantine versus direct
AbortedByBackend exactly as the table specifies.

~~~rust
fn prior(after: AfterRollbackTo) -> SavepointPriorFailure {
    match after {
        AfterRollbackTo::BodyRejected(error) =>
            SavepointPriorFailure::Body(error),
        AfterRollbackTo::ReleaseFailed(error) =>
            SavepointPriorFailure::Release(error.error),
        AfterRollbackTo::PoisonRecovery(error) =>
            SavepointPriorFailure::Poison(Box::new(error)),
    }
}

fn finish_after_rollback_to(
    after: AfterRollbackTo,
    release: Result<(), TxStatementError>,
) -> FrameCompletion {
    match (after, release) {
        (AfterRollbackTo::BodyRejected(error), Ok(())) =>
            FrameCompletion::Rejected(error),
        (AfterRollbackTo::ReleaseFailed(original), Ok(())) =>
            FrameCompletion::Failed(
                TxOutcomeError::SavepointReleaseFailed(original.error)),
        (AfterRollbackTo::PoisonRecovery(error), Ok(())) =>
            FrameCompletion::Failed(error),
        (after, Err(cleanup)) => FrameCompletion::Failed(
            TxOutcomeError::SavepointRollbackFailed(SavepointCleanupFailure {
                prior: prior(after),
                cleanup: cleanup.error,
            })),
    }
}
~~~

Frame names use next_frame_sequence, not current depth. The simultaneous open
depth remains capped at eight, matching the present public limit
(crates/zeroship-data-v8/src/transaction/mod.rs:103-111,325-343). Monotonic
names prevent a failed cleanup from leaving a server savepoint that shadows a
later depth-reused name, the exact server behavior documented by the driver
(libs/compio-postgres/src/transaction.rs:66-74).

## 3. Closed state enum

~~~rust
enum TxState {
    // Registry entry exists; no admission claim, session, root frame, or timer.
    WaitingAdmission {
        token: AdmissionToken,
    },

    // Admission claim and RAII ClaimGuard are held; the independent deadline is
    // armed; platform-role preparation/session acquisition is in flight; BEGIN
    // has not been sent.
    Preparing {
        token: CommandToken,
    },

    // BEGIN/session-setup command is in flight. No operation can see the session.
    Starting {
        token: CommandToken,
    },

    // Transaction is healthy, root exists, and no command owns the data session.
    Idle,

    // Exactly one operation owns logical execution. During Authority it uses a
    // separate platform-role session and the data session remains parked; during
    // DataSql or FrameControl the command token owns the data session.
    InFlight {
        token: CommandToken,
        action: ActiveAction,
        stage: ActiveStage,
    },

    // A normal frame/root settlement arrived while an operation was active.
    // The intent is latched; no new operation starts and no terminal SQL has
    // yet been sent.
    Quiescing {
        token: CommandToken,
        action: ActiveAction,
        stage: ActiveStage,
        pending: SettleIntent,
    },

    // No command owns the session, but backend health is unsafe or frame-control
    // state is no longer trusted. A rollback-to of recovery_frame may recover a
    // child; otherwise root settlement is required.
    Poisoned {
        cause: PoisonCause,
        recovery_frame: FrameId,
    },

    // Forced cancellation or an uncertain backend result won while preparation
    // or a command was active. Interrupt/cleanup acknowledgement is outstanding;
    // accepted responders have moved to TxEntry.deferred_replies.
    Cancelling {
        cause: CleanupCause,
        cleanup: CleanupGoal,
        phase: CancelCleanupPhase,
    },

    // Root COMMIT or ROLLBACK has been issued exactly once and not answered.
    Settling {
        token: CommandToken,
        attempt_id: RootSettleAttemptId,
        intent: RootIntent,
        watchdog_fired: bool,
        // Endpoint pinned and durable supervisor job registered before either
        // terminal SQL or its first deadline became visible.
        hard_stop: RegisteredExplicitHardStop,
    },

    // The terminal watchdog interrupt did not produce a result within grace.
    // The backend generation is being fenced from routing before an
    // indeterminate terminal outcome releases admission.
    HardStopping {
        // A late real TerminalCompleted authenticates against terminal_token;
        // the independent physical-fence completion authenticates against
        // fence_token. They are deliberately never equal.
        terminal_token: CommandToken,
        fence_token: CommandToken,
        attempt_id: RootSettleAttemptId,
        intent: RootIntent,
        resource_generation: u64,
        permit: Arc<ExplicitHardStopPermit>,
        trigger: HardStopTrigger,
        // Set only if the ordered Cancel is reduced before the publisher's
        // permit-authenticated preemption event.
        pending_cancel: Option<ForcedReason>,
    },

    // A terminal outcome is known, but reducer-observed protocol mismatch or
    // indeterminate backend health means the actor generation is not yet safe
    // to retire. Frames, effects, session ownership, claim, and all waiters stay
    // owned here until a separately authenticated generation-retirement proof
    // arrives. No creator request or SQL is accepted in this state.
    Quarantining {
        token: CommandToken,
        retirement_id: GenerationRetirementId,
        actor_generation: u64,
        pending_outcome: Box<TerminalOutcome>,
        settle_attempt: Option<RootSettleAttemptId>,
        retirement_attempt: u32,
        // Moved out of the prior terminal/cancel/data state before that state
        // is replaced. Every retirement retry carries this same deduped list.
        superseded_fence_jobs: Vec<DurableFenceJobHandle>,
    },

    // Immutable terminal result. No session, claim, timer, frame, or effect is
    // live. The record remains long enough to replay duplicate observations.
    Settled {
        outcome: SharedTerminalOutcome,
    },
}

enum CancelCleanupPhase {
    Awaiting {
        watchdog_fired: bool,
        // Registered before CancellationSql is armed or Cancel is released.
        hard_stop: RegisteredExplicitHardStop,
    },
    HardStopping {
        fence_token: CommandToken,
        resource_generation: u64,
        permit: Arc<ExplicitHardStopPermit>,
        trigger: HardStopTrigger,
    },
}

#[derive(Clone)]
struct RegisteredExplicitHardStop {
    permit: Arc<ExplicitHardStopPermit>,
    trigger: HardStopTrigger,
    fence_token: CommandToken,
}

enum ActiveStage {
    Authority,
    DataSql,
    FrameControl,
}

enum ActiveAction {
    Data {
        request_id: RequestId,
        op_id: OperationId,
        frame: FrameId,
        plan: DbPlan,
        needs_ceiling: bool,
        completion_cutoff: Arc<TerminalCutoffGate<DataAbortProof>>,
        delivery_permit: Arc<DataDeliveryPermit>,
        hard_stop_trigger: HardStopTrigger,
        reply: Reply<Result<DbRows, TxOutcomeError>>,
    },
    OpenSavepoint {
        request_id: RequestId,
        frame: FrameId,
        reply: Reply<Result<FrameOpenReply, TxProtocolError>>,
    },
    ReleaseSavepoint {
        close: PendingFrameClose,
    },
    RollbackToSavepoint {
        attempt_id: FrameCloseAttemptId,
        frame: FrameId,
        after: AfterRollbackTo,
        waiters: Vec<FrameWaiter>,
    },
    ReleaseAfterRollbackTo {
        attempt_id: FrameCloseAttemptId,
        frame: FrameId,
        after: AfterRollbackTo,
        waiters: Vec<FrameWaiter>,
    },
}

enum SettleIntent {
    Frame(PendingFrameClose),
    Root {
        attempt_id: RootSettleAttemptId,
        intent: RootIntent,
    },
}
~~~

Preparing, Quiescing, and Cancelling are necessary missing states.
WaitingAdmission makes the backend-specific admission queue observable.
Quiescing is distinct from Settling because the latter promises terminal SQL has
already been issued. This closes the current empty-slot ambiguity documented in
SC-1 (docs/proposals/2026-08-26-sc1-transaction-protocol.md:75-77,109-115).

Poisoned means either backend health is unsafe or frame-control state is no
longer trusted. A pre-SQL validation/authority denial does not poison.
PostgreSQL exposes failed transaction health through ReadyForQuery and refuses
ordinary statements until rollback
(libs/compio-postgres/src/client.rs:507-533). A poisoned COMMIT remains legal
because PostgreSQL can answer COMMIT with the ROLLBACK tag
(crates/zeroship-data-v8/src/transaction/mod.rs:123-159).

## 4. Closed event and completion enums

~~~rust
enum RegistryEvent {
    Create {
        key: TxKey,
        // Trusted runtime metadata. AdmissionKey is derived inside Registry;
        // the creator never supplies an independent app/runtime identity.
        admission_scope: BackendAdmissionScope,
        resolved_epoch: SchemaEpoch,
        execution_timeout: Duration,
        terminal_sql_timeout: Duration,
        terminal_interrupt_grace: Duration,
        // Two distinct request identities, each answered exactly once. Ready
        // starts the user callback; terminal survives even if that callback
        // never returns a SettleRoot event.
        start: StartWaiter,
        terminal: TerminalWaiter,
    },
    Routed {
        key: TxKey,
        event: TxEvent,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CancelOrder {
    // Trusted start-handle publication while the exact key is still
    // WaitingAdmission. No ReservationControl or actor owner gate exists yet.
    PreAdmission,
    ForceWon,
    AfterNonterminalCompletion,
    TerminalCompletionWon,
    TerminalFenceWon,
    Joined,
}

enum TxEvent {
    AdmissionGranted {
        token: AdmissionToken,
        claim: ClaimGuard,
    },
    AdmissionFailed {
        token: AdmissionToken,
        error: AdmissionError,
    },

    PreparationCompleted {
        token: CommandToken,
        result: PrepareResult,
        completion: Arc<NonterminalCompletionPermit>,
    },
    BeginCompleted {
        token: CommandToken,
        result: BeginResult,
        completion: Arc<NonterminalCompletionPermit>,
    },

    StartOperation {
        request_id: RequestId,
        op_id: OperationId,
        frame: FrameId,
        plan: DbPlan,
        needs_ceiling: bool,
        reply: Reply<Result<DbRows, TxOutcomeError>>,
    },
    AuthorityCompleted {
        token: CommandToken,
        result: AuthorityResult,
        completion: Arc<NonterminalCompletionPermit>,
    },
    DataCompleted {
        token: CommandToken,
        result: DataResult,
        completion: Arc<NonterminalCompletionPermit>,
        // Some iff result health is TransactionRolledBack. It is minted by the
        // actor only after the transaction/session is confirmed ended.
        retirement: Option<BackendEndProof>,
    },
    DataAbortCompleted {
        token: CommandToken,
        proof: Arc<DataAbortProof>,
    },

    OpenFrame {
        request_id: RequestId,
        parent: FrameId,
        reply: Reply<Result<FrameOpenReply, TxProtocolError>>,
    },
    CloseFrame {
        input: FrameCloseInput,
        waiter: FrameWaiter,
    },
    FrameCommandCompleted {
        token: CommandToken,
        result: FrameCommandResult,
        completion: Arc<NonterminalCompletionPermit>,
        // Same iff rule as DataCompleted for a TransactionRolledBack error.
        retirement: Option<BackendEndProof>,
    },

    SettleRoot {
        input: RootSettleInput,
        request_id: RequestId,
        reply: Reply<Result<TerminalReply, TxProtocolError>>,
    },
    TerminalCompleted {
        token: CommandToken,
        result: RootFinishResult,
        retirement: BackendTerminalRetirement,
    },
    TerminalHardStopCompleted {
        token: CommandToken,
        resource_generation: u64,
        proof: Arc<GenerationRetirementProof>,
    },
    TerminalHardStopPreempted {
        permit: Arc<ExplicitHardStopPermit>,
        resource_generation: u64,
        cause: ForcedReason,
    },

    Cancel {
        // Lossless image of every actor CancelCause. SetupFailed,
        // BeginUncertain, and BackendUnknown are reducer-internal and therefore
        // cannot enter through this external force event.
        cause: ForcedReason,
        // Authenticated result of the shared command-gate/owner-gate critical
        // section; Registry never reconstructs this from a later atomic load.
        order: CancelOrder,
        waiter: Option<TerminalWaiter>, // None is a notice, not a request
    },
    DetachRequested {
        expected: AppAuthority,
    },
    DeadlineFired {
        kind: DeadlineKind,
        generation: u64,
    },
    LifecycleObserved {
        observed: AuthorityObservation,
    },
    CancellationCompleted {
        token: CommandToken,
        result: Arc<CancelAck>,
        retirement: BackendTerminalRetirement,
    },
    CancellationHardStopCompleted {
        token: CommandToken,
        resource_generation: u64,
        result: Arc<CancelAck>, // always Indeterminate when the fence won
        proof: Arc<GenerationRetirementProof>,
    },
    BackendActorUnavailable {
        actor_generation: u64,
        error: DbError,
        proof: Arc<GenerationRetirementProof>,
    },
    GenerationRetired {
        token: CommandToken,
        retirement_id: GenerationRetirementId,
        actor_generation: u64,
        proof: Arc<GenerationRetirementProof>,
    },
    // Internal, replyless, keyed event emitted by TerminalRecordRef::drop.
    ReleaseTerminalRef,
    Forget,
}

enum PrepareResult {
    Observed {
        lease: ActorSessionLease, // phase=PreBegin
        authority: AuthorityObservation,
    },
    Failed(DbError),
}

enum BeginResult {
    Opened(ActorSessionLease), // same lease_id, phase=Transaction
    NotOpened(DbError),
    MayHaveOpened(DbError),
}

enum AuthorityResult {
    Observed {
        authority: AuthorityObservation,
    },
    Unavailable(DbError),
}

enum DataResult {
    Succeeded {
        value: DbRows,
        effects: Vec<PendingEffect>,
    },
    Failed {
        error: DbError,
        health: HealthImpact,
    },
}

struct DataAbortProof {
    error: DbError,
    // This proof is constructed only after rollback or a physical connection
    // fence; its health is definitionally TransactionRolledBack.
    retirement: BackendTerminalRetirement,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct GenerationRetirementId(u128);

#[derive(Clone)]
struct BackendEndProof {
    key: TxKey,
    actor_generation: u64,
    command_token: CommandToken,
    // Private actor capability minted only after terminal SQL/auto-rollback or
    // connection close proves the reservation can execute no more data SQL.
    seal: BackendEndSeal,
}

#[derive(Clone)]
enum BackendTerminalRetirement {
    Ended(BackendEndProof),
    GenerationRetired(Arc<GenerationRetirementProof>),
    // The result is immutable, but reuse/release requires the Quarantining
    // subprotocol. No row may treat this as a retirement proof.
    NeedsGenerationRetirement,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GenerationRetirementKind {
    Quarantined,
    PhysicallyFenced,
}

struct GenerationRetirementProof {
    key: TxKey,
    actor_generation: u64,
    retirement_id: GenerationRetirementId,
    kind: GenerationRetirementKind,
    // Unforgeable supervisor seal minted only after this exact generation is
    // absent from every routing index and both of its SQLite/Postgres lanes can
    // accept no more work. Close/reset failure changes kind to PhysicallyFenced;
    // it does not prevent issuance of the safety proof.
    seal: GenerationRetirementSeal,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HealthImpact {
    Healthy,
    Poisoned,
    TransactionRolledBack,
    Unknown,
}

enum FrameCommandResult {
    Opened(Result<(), TxStatementError>),
    Released(Result<(), TxStatementError>),
    RolledBackTo(Result<(), TxStatementError>),
    ReleasedAfterRollbackTo(Result<(), TxStatementError>),
}

#[derive(Clone)]
enum RootFinishResult {
    Committed,
    RolledBack,
    Failed {
        error: DbError,
        certainty: FinishCertainty,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FinishCertainty {
    DefinitelyNotCommitted,
    Indeterminate,
}

enum CancelAck {
    NoTransaction,
    RolledBack,
    Indeterminate(DbError),
}

enum CancelAckKind {
    NoTransaction,
    RolledBack,
    Indeterminate,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RootDecision {
    Commit,
    Rollback,
}

enum RootIntent {
    Commit {
        value: OwnedJsValue,
    },
    Rollback {
        cause: RollbackCause,
    },
}

enum FrameCompletion {
    Resolved(OwnedJsValue),
    Rejected(OwnedJsError),
    Failed(TxOutcomeError),
}

enum FrameOpenReply {
    Ready { frame: FrameId },
    Failed(TxOutcomeError),
}

enum StartReply {
    Ready {
        key: TxKey,
        root: FrameId,
    },
    Failed(SharedTerminalOutcome),
}

#[derive(Clone, PartialEq, Eq)]
enum CancelReason {
    CallerDrop,
    Explicit,
    IsolateTeardown,
}

enum DeadlineKind {
    Execution,
    CancellationSql,
    CancellationHardStop,
    TerminalSql,
    TerminalHardStop,
    RetirementFence,
}

#[derive(Clone, PartialEq, Eq)]
enum ForcedReason {
    Cancel(CancelReason),
    DeadlineExceeded,
    Detach,
    AuthorityDenied(AuthorityDenyReason),
    EpochChanged(SchemaEpoch),
}

#[derive(Clone, PartialEq, Eq)]
enum CleanupCause {
    Forced(ForcedReason),
    SetupFailed(DbError),
    BeginUncertain(DbError),
    BackendUnknown(DbError),
}

fn forced_reason(cause: CancelCause) -> ForcedReason {
    match cause {
        CancelCause::CallerDrop => ForcedReason::Cancel(CancelReason::CallerDrop),
        CancelCause::Explicit => ForcedReason::Cancel(CancelReason::Explicit),
        CancelCause::IsolateTeardown =>
            ForcedReason::Cancel(CancelReason::IsolateTeardown),
        CancelCause::Deadline => ForcedReason::DeadlineExceeded,
        CancelCause::Detach => ForcedReason::Detach,
        CancelCause::AuthorityDenied(reason) =>
            ForcedReason::AuthorityDenied(reason),
        CancelCause::EpochChanged(epoch) => ForcedReason::EpochChanged(epoch),
        CancelCause::SetupFailed(_)
        | CancelCause::BeginUncertain(_)
        | CancelCause::BackendUnknown(_) =>
            unreachable!("reducer-only cleanup is not an external force"),
    }
}

fn cleanup_cause(cause: CancelCause) -> CleanupCause {
    match cause {
        CancelCause::SetupFailed(error) => CleanupCause::SetupFailed(error),
        CancelCause::BeginUncertain(error) =>
            CleanupCause::BeginUncertain(error),
        CancelCause::BackendUnknown(error) =>
            CleanupCause::BackendUnknown(error),
        external => CleanupCause::Forced(forced_reason(external)),
    }
}

fn actor_cause_for_cleanup(cause: &CleanupCause) -> CancelCause {
    match cause {
        CleanupCause::Forced(ForcedReason::Cancel(CancelReason::CallerDrop)) =>
            CancelCause::CallerDrop,
        CleanupCause::Forced(ForcedReason::Cancel(CancelReason::Explicit)) =>
            CancelCause::Explicit,
        CleanupCause::Forced(
            ForcedReason::Cancel(CancelReason::IsolateTeardown),
        ) => CancelCause::IsolateTeardown,
        CleanupCause::Forced(ForcedReason::DeadlineExceeded) =>
            CancelCause::Deadline,
        CleanupCause::Forced(ForcedReason::Detach) => CancelCause::Detach,
        CleanupCause::Forced(ForcedReason::AuthorityDenied(reason)) =>
            CancelCause::AuthorityDenied(reason.clone()),
        CleanupCause::Forced(ForcedReason::EpochChanged(epoch)) =>
            CancelCause::EpochChanged(*epoch),
        CleanupCause::SetupFailed(error) =>
            CancelCause::SetupFailed(error.clone()),
        CleanupCause::BeginUncertain(error) =>
            CancelCause::BeginUncertain(error.clone()),
        CleanupCause::BackendUnknown(error) =>
            CancelCause::BackendUnknown(error.clone()),
    }
}

fn cleanup_cause_matches_actor(
    expected: &CleanupCause,
    observed: &CancelCause,
) -> bool {
    &cleanup_cause(observed.clone()) == expected
}

fn latch_sc1_cleanup_intent_exact(
    control: &ReservationControl,
    cause: &CleanupCause,
) -> Result<CancelCause, TxProtocolError> {
    let desired = actor_cause_for_cleanup(cause);
    let _owner = control.terminal_owner_gate.lock();
    if control.generation_fenced.load(Acquire)
        || owner(control.terminal.load(Acquire)) != OWNER_OPEN
    {
        return Err(TxProtocolError::CancellationCauseMismatch);
    }
    let stored = control.cancel_cause.get_or_init(|| desired.clone());
    if stored != &desired {
        return Err(TxProtocolError::CancellationCauseMismatch);
    }
    // Cause publication precedes intent publication. This is the same ordering
    // used by an external Cancel handle, but authorizes the three trusted
    // reducer-only cleanup causes as well.
    control.terminal.fetch_or(CANCEL_INTENT, AcqRel);
    Ok(stored.clone())
}

enum CleanupGoal {
    NoTransaction,
    AbortIfOpened,
    // BEGIN is known open; cancellation must end the whole transaction whether
    // a statement is currently running or the session is Idle/Poisoned.
    OpenTransaction,
    QuarantineUnknown,
}

#[derive(Clone)]
struct TxStatementError {
    error: DbError,
    health: HealthImpact,
}

struct PoisonCause {
    error: DbError,
    health: HealthImpact,
}

enum RollbackCause {
    Body(OwnedJsError),
    Forced(ForcedReason),
    SetupFailed(DbError),
    BeginUncertain(DbError),
    BackendUnknown(DbError),
    Protocol(TxProtocolError),
}

enum TerminalOutcome {
    Committed(OwnedJsValue),
    RolledBack(RollbackCause),
    AdmissionFailed(AdmissionError),
    BeginFailed(DbError),
    Cancelled(CancelReason),
    DeadlineExceeded,
    Detached,
    IncarnationDenied,
    AppDeprovisioned,
    EpochChanged,
    CommitRolledBack,
    CommitFailed(DbError),
    CommitIndeterminate(DbError),
    RollbackFailed {
        rollback: DbError,
        original: RollbackCause,
    },
    AbortedByBackend(DbError),
    CleanupIndeterminate(DbError),
    CleanupProtocolMismatch {
        goal: CleanupGoal,
        acknowledgement: CancelAckKind,
    },
    CancellationProtocolMismatch {
        expected: CleanupCause,
        observed: CleanupCause,
    },
    TerminalResultMismatch {
        expected: RootDecision,
        observed: RootDecision,
    },
}

#[derive(Clone)]
enum TerminalReply {
    Completed(SharedTerminalOutcome),
    Replay(SharedTerminalOutcome),
}

enum TxOutcomeError {
    Protocol(TxProtocolError),
    Database(DbError),
    SavepointOpenFailed(DbError),       // savepoint_open_failed
    SavepointReleaseFailed(DbError),    // savepoint_release_failed
    SavepointRollbackFailed(SavepointCleanupFailure),
                                           // savepoint_rollback_failed
    TransactionPoisoned,                // transaction_poisoned
    TransactionCancelled(CancelReason), // transaction_cancelled
    TransactionDeadlineExceeded,        // transaction_deadline_exceeded
    TransactionDetached,                // transaction_detached
    AppIncarnationMismatch,             // app_incarnation_mismatch
    AppDeprovisioned,                   // app_deprovisioned
    TxEpochChanged,                     // tx_epoch_changed, retryable
    CancellationCleanupFailed(DbError), // cancellation_cleanup_failed
    CancellationProtocolMismatch,       // cancellation_protocol_mismatch
    TerminalResultMismatch,             // terminal_result_mismatch
}
~~~

Pre-admission cancellation is not an ordinary mailbox send. The trusted start
handle calls `cancel_start_exact(TxKey,cause)`; that function locks the same
TxEntry reducer mutex used by AdmissionGranted. If the locked state is
WaitingAdmission it synchronously reduces
`Cancel{cause:forced_reason(cause),order:PreAdmission}` before unlocking and
removes the queued admission token. If AdmissionGranted won the lock first, its
single transition has already installed either an armed ReservationHandle or a
Settled outcome. The function then drops the reducer lock: for an armed handle
it calls `publish_cancel` and waits for the resulting actor-derived CancelOrder;
for Settled it replays the outcome. `PreAdmission` is therefore never queued,
can never arrive in Preparing, and cannot strand a newly installed reservation.
An injected/foreign PreAdmission event outside that locked entry point is STC.

`Create` transfers ownership of both continuations into the entry. The reducer
uses only these two helpers; no row may spell an unowned “invoke/reject caller”:

~~~rust
fn publish_ready(entry: &mut TxEntry, root: FrameId) -> ReducerEffect {
    let waiter = entry.start_waiter.take().expect("ready exactly once");
    ReducerEffect::Reply(ReplyEffect::Start {
        request_id: waiter.request_id,
        reply: waiter.reply,
        value: Ok(StartReply::Ready {
            key: entry.key.clone(),
            root,
        }),
    })
}

enum TerminalRetirementProof {
    NoBackendSession,
    BackendConfirmedEnded(BackendEndProof),
    GenerationRetired(Arc<GenerationRetirementProof>),
}

// Private, already-validated evidence consumed by ReservationHandle.  A public
// TerminalOutcome is deliberately insufficient to disarm Drop-cancel.
enum ValidatedReservationRetirement {
    BackendConfirmedEnded(BackendEndProof),
    GenerationRetired(Arc<GenerationRetirementProof>),
}

fn push_unique_fence_job(
    jobs: &mut Vec<DurableFenceJobHandle>,
    job: DurableFenceJobHandle,
) {
    if !jobs.contains(&job) { jobs.push(job); }
}

fn consume_owned_permit_infallible(permit: &Arc<ExplicitHardStopPermit>) {
    // Under the TxEntry reducer lock. Claimed means its durable keyed event or
    // cutoff already exists; changing it to Consumed only prevents a second
    // publisher. Open means this transition superseded it before publication.
    permit.publish_state.store(
        HardStopPermitPublishState::ConsumedByReducer as u8,
        Release,
    );
}

impl TxEntry {
    fn take_registered_terminal_fence_jobs(
        &mut self,
    ) -> Vec<DurableFenceJobHandle> {
        let mut jobs = Vec::new();
        match &mut self.state {
            TxState::Settling { hard_stop, .. } => {
                consume_owned_permit_infallible(&hard_stop.permit);
                push_unique_fence_job(&mut jobs, hard_stop.trigger.job());
            }
            TxState::HardStopping { permit, trigger, .. } => {
                consume_owned_permit_infallible(permit);
                push_unique_fence_job(&mut jobs, trigger.job());
            }
            TxState::Cancelling { phase, .. } => match phase {
                CancelCleanupPhase::Awaiting { hard_stop, .. } => {
                    consume_owned_permit_infallible(&hard_stop.permit);
                    push_unique_fence_job(&mut jobs, hard_stop.trigger.job());
                }
                CancelCleanupPhase::HardStopping { permit, trigger, .. } => {
                    consume_owned_permit_infallible(permit);
                    push_unique_fence_job(&mut jobs, trigger.job());
                }
            },
            TxState::InFlight {
                action: ActiveAction::Data { hard_stop_trigger, .. }, ..
            }
            | TxState::Quiescing {
                action: ActiveAction::Data { hard_stop_trigger, .. }, ..
            } => push_unique_fence_job(&mut jobs, hard_stop_trigger.job()),
            TxState::Quarantining { superseded_fence_jobs, .. } => {
                for job in std::mem::take(superseded_fence_jobs) {
                    push_unique_fence_job(&mut jobs, job);
                }
            }
            _ => {}
        }
        // Reserve-time root/cancel jobs exist even if neither route became the
        // current state. Terminalization retires those unused capacity records
        // as well. OnceLock values remain descriptive until control Forget.
        if let Some(control) = self.backend_reservation.as_ref()
            .map(BackendReservationLease::control)
        {
            if let Some(root) = control.preinstalled_explicit_root_fence.get() {
                push_unique_fence_job(&mut jobs, root.job);
            }
            if let Some(cancel) =
                control.preinstalled_explicit_cancel_fence.get()
            {
                push_unique_fence_job(&mut jobs, cancel.job);
            }
        }
        jobs
    }
}

fn publish_settled(
    entry: &mut TxEntry,
    outcome: TerminalOutcome,
    retirement: TerminalRetirementProof,
)
    -> Vec<ReducerEffect>
{
    let shared = Arc::new(outcome);
    let mut effects = Vec::new();
    // This helper is the sole frame/effect terminalizer. Table rows describe
    // this behavior but must not publish/discard a second time.
    if matches!(shared.as_ref(), TerminalOutcome::Committed(_)) {
        assert_eq!(entry.frames.len(), 1);
        let mut committed_effects = Vec::new();
        for effect in entry.frames[0].effects.drain(..) {
            committed_effects.push(effect);
        }
        if !committed_effects.is_empty() {
            effects.push(ReducerEffect::Publish(committed_effects));
        }
    } else {
        for frame in &mut entry.frames { frame.effects.clear(); }
    }
    entry.frames.clear();

    let expected_terminal_token = match &entry.state {
        TxState::InFlight { token, .. }
        | TxState::Quiescing { token, .. }
        | TxState::Settling { token, .. } => Some(*token),
        TxState::HardStopping { terminal_token, .. } => Some(*terminal_token),
        TxState::Cancelling { .. } => Some(entry.cancellation_token),
        _ => None,
    };
    let session = std::mem::replace(&mut entry.session, SessionOwner::None);
    let mut validated_retirement = match (&retirement, &session) {
        (TerminalRetirementProof::NoBackendSession, SessionOwner::None) => None,
        (TerminalRetirementProof::BackendConfirmedEnded(proof), owner)
            if proof.key == entry.key
                && entry.backend_actor_generation == Some(proof.actor_generation)
                && expected_terminal_token == Some(proof.command_token)
                && !matches!(owner, SessionOwner::None) =>
            Some(ValidatedReservationRetirement::BackendConfirmedEnded(
                proof.clone(),
            )),
        (TerminalRetirementProof::GenerationRetired(proof), owner)
            if proof.key == entry.key
                && entry.backend_actor_generation == Some(proof.actor_generation)
                && !matches!(owner, SessionOwner::None) =>
            Some(ValidatedReservationRetirement::GenerationRetired(
                proof.clone(),
            )),
        _ => panic!("terminal retirement proof/session mismatch"),
    };
    drop(session);
    // Settling/Cancelling register their fence job before the first terminal
    // deadline. A real result may settle before that job is activated; remove
    // the dormant record (or mark an already-finished one collected) before
    // the terminal state drops its only descriptive trigger.
    for job in entry.take_registered_terminal_fence_jobs() {
        effects.push(ReducerEffect::FinishOrCancelFenceJobInfallible(job));
    }
    disarm_explicit_deadline(&mut effects, &entry.deadline_slots);
    if let Some(lease) = entry.backend_reservation.take() {
        entry.backend_reservation = Some(match lease {
            BackendReservationLease::Armed(handle) => {
                // Consumes the private, exact retirement capability as well as
                // checking the selected cutoff.  The public outcome alone can
                // never convert an armed handle into a no-cancel lease.
                BackendReservationLease::Terminal(
                    handle.disarm_after_terminal_proof(
                        shared.clone(),
                        validated_retirement.take()
                            .expect("admitted settlement needs private retirement"),
                    )
                        .expect("Settled requires actor retirement proof"),
                )
            }
            BackendReservationLease::Terminal(lease) =>
                BackendReservationLease::Terminal(lease),
        });
    } else {
        assert!(validated_retirement.is_none(),
            "retirement capability without reservation");
    }
    if let Some(claim) = entry.claim.take() {
        // The effect owns the guard, so changing state cannot orphan or double
        // release admission. Dropping the executed effect releases exactly once.
        effects.push(ReducerEffect::ReleaseClaim(claim));
    }
    // Stop authenticating actor-unavailable events against this settled entry.
    entry.backend_actor_generation = None;
    if let Some(waiter) = entry.start_waiter.take() {
        effects.push(ReducerEffect::Reply(ReplyEffect::Start {
            request_id: waiter.request_id,
            reply: waiter.reply,
            value: Ok(StartReply::Failed(shared.clone())),
        }));
    }
    effects.extend(drain_deferred_replies(entry, &shared));
    for waiter in entry.terminal_waiters.drain(..) {
        effects.push(ReducerEffect::Reply(ReplyEffect::Terminal {
            request_id: waiter.request_id,
            reply: waiter.reply,
            value: Ok(TerminalReply::Completed(shared.clone())),
        }));
    }
    assert!(matches!(entry.session, SessionOwner::None));
    assert!(entry.frames.is_empty());
    entry.state = TxState::Settled { outcome: shared };
    effects
}

// The only path from a terminal result whose backend retirement is not already
// proved. It deliberately does not clear frames/effects, release the claim, or
// wake a success/terminal waiter.
fn begin_generation_retirement(
    entry: &mut TxEntry,
    outcome: TerminalOutcome,
) -> Transition {
    let actor_generation = entry.backend_actor_generation
        .expect("retirement requires an admitted actor generation");
    let token = CommandToken::mint();
    let retirement_id = GenerationRetirementId::mint();
    let settle_attempt = match &entry.state {
        TxState::Settling { attempt_id, .. }
        | TxState::HardStopping { attempt_id, .. } => Some(*attempt_id),
        _ => None,
    };
    let superseded_fence_jobs =
        entry.take_registered_terminal_fence_jobs();
    let at = Instant::now() + entry.terminal_interrupt_grace;
    let generation = mint_never_reused_generation();
    entry.deadline_slots.replace_with_retirement(generation, at)
        .expect("terminal source owns a current non-retirement deadline");
    Transition {
        next: TxState::Quarantining {
            token,
            retirement_id,
            actor_generation,
            pending_outcome: Box::new(outcome),
            settle_attempt,
            retirement_attempt: 0,
            superseded_fence_jobs: superseded_fence_jobs.clone(),
        },
        effects: vec![ReducerEffect::RetireGeneration(RetireGenerationEffect {
            key: entry.key.clone(), token, retirement_id, actor_generation,
            force_physical: false,
            superseded_fence_jobs,
        })],
    }
}

// Effect executor contract (outside the reducer lock): pin the exact keyed
// mailbox; under the supervisor generation/routing gate remove
// (key.app,actor_generation) from every routing index; close or permanently
// isolate both lanes; mint GenerationRetirementProof only after that ordering;
// then enqueue_once(retirement_id, GenerationRetired{...}). Operational close
// failure selects PhysicallyFenced instead of Quarantined, so the executor has
// no unsafe error completion. A RetirementFence retry uses the same
// retirement_id and is therefore idempotent.
fn execute_retire_generation(effect: RetireGenerationEffect) {
    for job in &effect.superseded_fence_jobs {
        supervisor.jobs.finish_or_cancel_infallible(*job);
    }
    let mailbox = registry.pin_exact(&effect.key);
    let proof = if effect.force_physical {
        supervisor.physically_fence_generation_exact(
            &effect.key.app, effect.actor_generation, effect.retirement_id,
        )
    } else {
        // This call attempts orderly close first and itself falls back to a
        // physical fence before returning a proof.  It therefore cannot emit
        // an unsafe error acknowledgement.
        supervisor.retire_generation_exact(
            &effect.key.app, effect.actor_generation, effect.retirement_id,
        )
    };
    mailbox.enqueue_once(effect.retirement_id, RegistryEvent::Routed {
        key: effect.key,
        event: TxEvent::GenerationRetired {
            token: effect.token,
            retirement_id: effect.retirement_id,
            actor_generation: effect.actor_generation,
            proof: Arc::new(proof),
        },
    });
}

fn disarm_explicit_deadline(
    effects: &mut Vec<ReducerEffect>,
    slots: &ExplicitDeadlineSlots,
) {
    let mut state = slots.state.lock();
    let previous = std::mem::replace(&mut *state, ExplicitDeadlineState::Disarmed);
    if let ExplicitDeadlineState::Armed { kind, generation, .. }
        | ExplicitDeadlineState::Fired { kind, generation } = previous
    {
        effects.push(ReducerEffect::DisarmDeadline { kind, generation });
    }
}

fn retain_request_id(
    entry: &mut TxEntry,
    request_id: RequestId,
) -> Result<(), TxProtocolError> {
    if entry.retained_request_ids.insert(request_id) {
        Ok(())
    } else {
        Err(TxProtocolError::DuplicateRequest)
    }
}
~~~

The start and terminal request ids MUST differ and are inserted into the same
per-entry duplicate-id set before admission is enqueued. A successful BEGIN
calls `publish_ready`; every terminal path calls `publish_settled`. Therefore an
admission error, failed/uncertain BEGIN cleanup, pre-BEGIN lifecycle denial, or
execution deadline is observable even when no later `SettleRoot` is ever
constructed. After readiness, the initial terminal waiter is what lets the
transaction wrapper finish when the user callback never returns.
Every other legal reply-bearing request row calls `retain_request_id` before
changing state, appending a waiter, or issuing SQL. A rejected busy/illegal
request is not retained; an accepted id is deliberately never removed until
Forget, even after its response is sent.
`drain_deferred_replies` is the total table in section 6; it consumes every
stored Data request id and FrameWaiter before state becomes Settled. Illegal
requests instead send `Err(TxProtocolError)` on the corresponding typed Result
channel. In particular, Create validation or duplicate-locator rejection sends
the same protocol error to both supplied continuations without inserting an
entry.

RegistryEvent is the literal dispatch type: every event after Create carries the
complete TxKey in Routed, and every asynchronous completion also carries the
fresh token that launched it. LifecycleObserved is deliberately special only in
its payload: its Routed envelope names the old exact TxKey, while observed is
the newly read authority. It is never routed by a new bare app id.

The authority reducer runs this total classifier before any data SQL:

~~~rust
enum AuthorityDisposition {
    Current { ceiling: MaskCeiling },
    ReResolve, // changing or epoch mismatch
    Deny(AuthorityDenyReason),
}

enum AuthorityDenyReason {
    AppIdMismatch,
    DomainMismatch,
    IncarnationMismatch,
    Deprovisioned,
}

fn classify_authority(
    expected_app: &AppAuthority,
    expected_epoch: SchemaEpoch,
    observed: &AuthorityObservation,
) -> AuthorityDisposition {
    if observed.app.app_id != expected_app.app_id {
        return AuthorityDisposition::Deny(AuthorityDenyReason::AppIdMismatch);
    }
    if observed.app.domain != expected_app.domain {
        return AuthorityDisposition::Deny(AuthorityDenyReason::DomainMismatch);
    }
    if observed.app.incarnation != expected_app.incarnation {
        return AuthorityDisposition::Deny(
            AuthorityDenyReason::IncarnationMismatch,
        );
    }
    match observed.state {
        LifecycleState::Deprovisioned => AuthorityDisposition::Deny(
            AuthorityDenyReason::Deprovisioned,
        ),
        LifecycleState::Changing => AuthorityDisposition::ReResolve,
        LifecycleState::Stable if observed.epoch != expected_epoch =>
            AuthorityDisposition::ReResolve,
        LifecycleState::Stable =>
            AuthorityDisposition::Current { ceiling: observed.ceiling },
    }
}
~~~

ReResolve rolls back this transaction attempt and returns retryable
tx_epoch_changed. Deny returns terminal app_incarnation_mismatch (or the more
specific app_deprovisioned audit reason) and never follows the new app. During
each operation, the platform-session ceiling is folded as:

~~~rust
effective_ceiling =
    meet(begin_ceiling, effective_ceiling, newly_read_ceiling);
~~~

A raise is therefore ignored until a new top-level transaction. The authority
session never borrows the tenant data snapshot and never assumes the tenant
role. This directly instantiates Fork B
(docs/proposals/2026-08-26-sc6-ceiling-read-contract.md:113-131).

## 5. Reducer guard order and typed protocol errors

Every reducer invocation performs these checks in this order:

1. Create first rejects an occupied TxLocator as DuplicateTransaction; for an
   absent locator it validates BackendAdmissionScope against the configured
   backend and derives AdmissionKey from TxKey. A routed event first looks up TxLocator. No locator
   returns TransactionNotFound; a locator whose stored AppAuthority differs
   from the routed key returns AppIncarnationMismatch and MUST NOT touch that
   entry's session, actor, timer, or admission.
   DetachRequested additionally requires expected == stored AppAuthority; a
   different domain or incarnation returns AppIncarnationMismatch without
   publishing cancellation or interrupting anything.
2. AdmissionGranted/AdmissionFailed must match the AdmissionToken stored in
   WaitingAdmission. A wrong token returns StaleAdmissionCompletion. Every
   preparation/data/frame/root completion must match the token carried by its
   current TxState action. CancellationCompleted instead matches the single
   TxEntry.cancellation_token minted at Create and installed in the actor
   delivery at reserve. CancellationHardStopCompleted matches only the
   fence_token in CancelCleanupPhase::HardStopping. TerminalHardStopCompleted
   matches root HardStopping.fence_token, while a late TerminalCompleted still
   matches HardStopping.terminal_token. A mismatch returns
   StaleTransactionCompletion.
   A matching PrepareResult::Observed lease must also name the stored actor
   generation, have phase PreBegin, and equal the actor reservation's prepared
   lease. BeginResult::Opened must keep that same lease_id/generation and change
   only phase to Transaction. Any malformed current-token lease is
   StaleTransactionCompletion and cannot be installed.
3. DeadlineFired first performs a **pure** state/event capability preflight while
   holding the reducer lock. It validates only facts available before
   arbitration: the state accepts that DeadlineKind, and it computes the closed
   set of `ForceArbitration` variants legal for that state (for example,
   TerminalCompletionWon is in the set only when InFlight/Quiescing retains the
   matching completion-owned candidate). It does not pretend to know the
   arbitration result yet. Only a preflight-legal event calls
   `deadline_slots.claim_fire(kind,generation)`. A wrong pair is the pure
   StaleTransactionDeadline path. The reducer then calls `arbitrate_force` and
   reduces the returned member of the precomputed set in the same critical
   section. A result outside that set is not reclassified as an illegal event
   after the timer was consumed: it is an authenticated actor protocol fault,
   enters generation retirement with `cancellation_protocol_mismatch`, and
   cannot release admission. ReservationControl holds the same deadline Arc;
   every legal replacement changes Fired to the next Armed state before
   scheduling. Thus no illegal matrix cell claims a timer, while the protocol
   also makes no causally impossible pre-arbitration observation.
4. BackendActorUnavailable must name backend_actor_generation stored from the
   admitted ReservationHandle. A different generation returns
   StaleTransactionCompletion without touching the entry. The matching event
   is still legal only after the supervisor has removed that generation from
   all routing indexes and proved both actor lanes closed.
5. The state/event matrix runs next. Its not-ready/busy/expired error wins when
   that state cannot legally process any frame event.
6. Only inside a state where the matrix marks that frame event potentially
   legal do top/root/depth guards run before mutation. OpenFrame has no
   caller-supplied child id: Registry mints a never-reused FrameId and savepoint
   name from next_frame_sequence. A joinable CloseFrame/SettleRoot/Cancel whose
   request_id is already retained returns DuplicateRequest; a distinct id joins
   the existing SQL/result.

Before an asynchronous InFlight/Quiescing completion is called "matching", the
closed action/stage guard is applied: Authority requires
`ActiveAction::Data + ActiveStage::Authority`; DataCompleted/DataAbortCompleted
requires `ActiveAction::Data + ActiveStage::DataSql`; FrameCommandCompleted
requires FrameControl plus exactly one of OpenSavepoint, ReleaseSavepoint,
RollbackToSavepoint, or ReleaseAfterRollbackTo matching the result subtype. Any
other cross-product is StaleTransactionCompletion and is pure. This is a guard
on the closed sum, not permission to construct arbitrary action/stage pairs.

All request-bearing reply senders are `Reply<Result<_,TxProtocolError>>` (or the
data operation's `TxOutcomeError` wrapper). Therefore every illegal matrix cell
has a type-correct error path; it is not an out-of-band log-only result.

The frame guards are complete: StartOperation naming any frame other than the
current open top returns SavepointNotCurrent (a previously valid but now-closed
frame is not downgraded to generic expiry); OpenFrame with a non-top parent returns
SavepointNotCurrent; a ninth simultaneously open child returns
SavepointDepthExceeded; CloseFrame naming root returns
SavepointRootCannotClose; and any other non-top close returns
SavepointNotCurrent. None mutates the stack.

~~~rust
enum TxProtocolError {
    TransactionNotFound,              // transaction_not_found
    DuplicateTransaction,             // duplicate_transaction
    InvalidTransactionTimeout,         // invalid_transaction_timeout
    InvalidAdmissionScope,             // invalid_admission_scope
    AppIncarnationMismatch,           // app_incarnation_mismatch
    DuplicateRequest,                  // duplicate_request
    StaleAdmissionCompletion,          // stale_admission_completion
    StaleTransactionCompletion,        // stale_transaction_completion
    StaleTransactionDeadline,          // stale_transaction_deadline
    TransactionNotReady,               // transaction_not_ready
    TransactionNotOpen,                // transaction_not_open
    TransactionConnectionBusy,         // transaction_connection_busy
    TransactionScopeExpired,           // transaction_scope_expired
    TransactionPoisoned,               // transaction_poisoned
    TransactionSettling,               // transaction_settling
    TransactionCancelling,             // transaction_cancelling
    TransactionSettleConflict,         // transaction_settle_conflict
    TransactionNotSettled,             // transaction_not_settled
    TerminalRecordReferenced,           // terminal_record_referenced
    SavepointDepthExceeded,             // savepoint_depth_exceeded
    SavepointNotCurrent,                // savepoint_not_current
    SavepointRootCannotClose,           // savepoint_root_cannot_close
    SavepointFrameLeaked,               // savepoint_frame_leaked
    CancellationCauseMismatch,          // cancellation_cause_mismatch
    TerminalRouteMismatch,              // terminal_route_mismatch
    // Typed wrapper preserves the concrete ActorError discriminator and its
    // wire code (ActorSaturated, ActorUnavailable, protocol mismatch, etc.).
    Backend(ActorError),                 // transaction_backend_error
    EpochChanged,                       // tx_epoch_changed, retryable
}

impl TxProtocolError {
    fn from_actor(error: ActorError) -> Self {
        TxProtocolError::Backend(error)
    }
}
~~~

TransactionConnectionBusy and TransactionScopeExpired already have distinct
creator-visible meanings in the current implementation
(crates/zeroship-data-v8/src/exec.rs:124-171). The new reducer preserves those
meanings; it does not collapse protocol errors into an internal string.

Every table row whose next state is `Cancelling(...,Awaiting)` invokes this one
constructor; the abbreviation never means a struct literal:

~~~rust
fn enter_cancelling_awaiting_exact(
    entry: &mut TxEntry,
    control: &Arc<ReservationControl>,
    cause: CleanupCause,
    goal: CleanupGoal,
) -> Result<TxState, TxProtocolError> {
    let actor_cause = latch_sc1_cleanup_intent_exact(control, &cause)?;
    let hard_stop = prepare_sc1_cancel_route_before_arm(
        entry,
        control,
        actor_cause,
        entry.cancellation_token,
    )?;
    // All capacity, route, and mailbox work happened at Reserve. This binding
    // only authenticates the preinstalled job, captures the immutable cause,
    // and retains it before arming the absolute deadline.
    entry.cancel_dormant_superseded_fence_jobs_infallible();
    entry.deadline_slots.replace_with_cancellation_sql_infallible(
        hard_stop.trigger.clone(),
    );
    publish_terminal_route_armed(control);
    Ok(TxState::Cancelling {
        cause,
        cleanup: goal,
        phase: CancelCleanupPhase::Awaiting {
            watchdog_fired: false,
            hard_stop,
        },
    })
}
~~~

`ActorSaturated` is possible only at Reserve, before a control/index entry,
handle, timer, or actor command is published. Once Cancel can be observed,
`prepare_sc1_cancel_route_before_arm` performs no capacity acquisition or
mailbox pin; absence or mismatch of its reserve-installed record is the typed
internal `CancellationProtocolMismatch` and immediately generation-fences the
reservation. After it returns, no allocation or fallible send is needed to
drive either deadline stage.

## 6. Complete legal transition table

Each row below is one match arm. “Issue” means record the new state and token
before sending the command. No await occurs between those two actions.
Destinations such as Settling(Rollback) abbreviate construction of all fields
shown in the closed enum: the reducer mints the token and carries the exact
RootIntent/cause from the event. Event labels omit request_id/reply only as table
shorthand; every accepted request either retains that responder in the declared
action/waiter vector or replies in the same reducer arm. Every destination
written as `Settled(x)` calls `publish_settled(entry, x, proof)` with the exact
proof selected by the retirement table below, including its start,
initial-terminal, joined-terminal, and deferred-response drains; it never merely
assigns the enum discriminant. A row marked `Quarantining(x)` instead calls
`begin_generation_retirement(entry,x)` and cannot release or reply terminally
until `GenerationRetired` supplies its authenticated proof.

The wrapper mints FrameCloseAttemptId or RootSettleAttemptId before moving an
OwnedJsValue/OwnedJsError into the first event. A retry/join carries that same
attempt id with a new request id and no second owned payload; Registry never
compares or hashes JS values/errors. Same target+attempt id is "identical" in
the table. A different attempt id while a close/root intent is retained returns
TransactionSettleConflict (CON) without consuming its payload or issuing SQL.
In first-attempt rows, table shorthand `CloseFrame(kind)` and
`SettleRoot(intent)` means `FrameCloseInput::First` and
`RootSettleInput::First`. Every row containing "join" requires the payload-free
Join variant; Join in a state with no matching retained attempt returns CON.

Retirement selection is a total second discriminator inside each terminal
match arm. `BackendEndProof` and `GenerationRetirementProof` are private
capabilities, not booleans reconstructed by Registry:

| Terminal evidence | Required reducer action and proof |
| --- | --- |
| Admission failure/pre-admission denial before Reserve | `publish_settled(...,NoBackendSession)`; `session`, reservation, and actor generation must all be absent. |
| Matching Data/Frame `TransactionRolledBack` | Require `retirement=Some(BackendEndProof)` matching TxKey, actor generation, and command token; then `publish_settled(...,BackendConfirmedEnded(proof))`. Missing/foreign proof is an authenticated producer fault and enters `Quarantining(AbortedByBackend(backend_retirement_proof_mismatch(...)))`; `TerminalResultMismatch` is root-only. All other Data/Frame results require `retirement=None`; a surprise proof is STC and changes nothing. |
| `DataAbortCompleted` | Exhaust all three variants. `Ended(p)` validates key/generation/token and publishes with `BackendConfirmedEnded(p)`. `GenerationRetired(p)` validates the private seal, key, generation, and cutoff-derived retirement id and publishes with `GenerationRetired(p)`. `NeedsGenerationRetirement` moves the classified abort outcome and every responder into Quarantining without replying. A foreign `Ended` or `GenerationRetired` proof quarantines the typed `backend_retirement_proof_mismatch` outcome. |
| Root `Committed`, `RolledBack`, or `Failed(DefinitelyNotCommitted)` consistent with intent | `Ended(p)` publishes with `BackendConfirmedEnded(p)` after exact validation. `GenerationRetired(p)` publishes with that exact physical proof. `NeedsGenerationRetirement` enters Quarantining with the same immutable outcome. DefinitelyNotCommitted is emitted only after any compensating rollback has confirmed end. |
| Root `Failed(Indeterminate)` or Rollback intent receiving `Committed` | With `Ended` or `NeedsGenerationRetirement`, enter Quarantining with CommitIndeterminate/RollbackFailed/TerminalResultMismatch; contradictory or indeterminate evidence is not authority to reuse a still-routable generation. With an already matching sealed `GenerationRetired(p)`, publish that immutable outcome directly using `TerminalRetirementProof::GenerationRetired(p)`; never launch a redundant second retirement. |
| Cancellation, every CleanupGoal x CancelAck cell | First derive the total cleanup/mismatch outcome. `Ended(p)` permits direct publication only for a semantically consistent `NoTransaction`/`RolledBack` cell; an inconsistent or Indeterminate cell enters Quarantining. A matching `GenerationRetired(p)` directly publishes either the consistent cleanup outcome or the already-fixed mismatch/indeterminate outcome because the generation is physically unreachable. `NeedsGenerationRetirement` always enters Quarantining. No variant falls through. |
| Matching hard-stop completion or BackendActorUnavailable | Validate the carried GenerationRetirementProof's TxKey, actor generation, retirement id (where the state stores one), and private seal; publish with `GenerationRetired(proof)`. These events never synthesize BackendConfirmedEnded. |
| Reducer-discovered unknown health | First use the sole Cancel cleanup route. A proved RolledBack follows the cancellation row above; its mismatch/indeterminate result enters Quarantining. |

No other construction of `TerminalRetirementProof` is legal. In particular,
the reducer cannot turn “the actor said error,” a raw SQLite code, or a numeric
generation into retirement authority.

Before consuming a matching Preparation/Begin/Authority/Data/Frame completion
permit, the reducer **purely** validates the result/retirement shape and every
private proof. Thus a surprise retirement proof can return STC without clearing
the command gate. It then calls `consume_nonterminal_completion_exact`.
`Cleared` applies the ordinary row and permits a later Open gate.

`ForceAfter{cause}` applies only mutations/replies for work SQLite already
finished and MUST emit no follow-on Issue, install no next command gate, call no
`drive_pending`, and expose no operation-accepting intermediate state. In the
same reducer critical section it exhaustively does this:

| Completed arm under ForceAfter | Same-transition action before entering/retaining terminal control |
| --- | --- |
| Prepare Observed(Current) | Retain the prepared session; issue no BEGIN; enter Cancelling(cause,NoTransaction,Awaiting). |
| Begin Opened | Install root and emit the ordinary Ready reply; immediately enter Cancelling(cause,OpenTransaction,Awaiting). |
| Authority Current | Retain the Data responder; issue no data SQL; enter Cancelling(cause,OpenTransaction,Awaiting). |
| Data/Frame result whose ordinary destination is Idle or Poisoned | Apply its completed reply/effect/frame mutation, then enter Cancelling(cause,OpenTransaction,Awaiting) before unlocking. |
| Frame result whose ordinary row would issue recovery ROLLBACK TO or RELEASE | Issue neither; move every still-unresolved close responder and prior error to deferred_replies; enter Cancelling(cause,OpenTransaction,Awaiting). |
| Any completion from Quiescing | Additionally defer a pending Frame; a pending Root is already terminal_waiters. Never drive it. |
| Completion itself proves terminal state or already enters Cancelling/Quarantining | Preserve that stronger result-owned destination and record the later force as audit; do not start competing cleanup. |

Entry to Cancelling replaces Armed/Fired Execution with CancellationSql and
emits the sole reserve-time Cancel. Its actor claim accepts and clears the exact
retained ForceAfter gate only after OWNER_CANCEL, attempt, delivery, and budget
are installed. The already-keyed `Cancel{AfterNonterminalCompletion}` then joins
or audits; it cannot start a second cleanup. There is no mailbox-visible
Idle/Starting/InFlight window between the completion and forced transition.

| Source | Event and guard | Destination | Atomic reducer effects and externally visible result |
| --- | --- | --- | --- |
| no entry | RegistryEvent::Create, locator absent, distinct start/terminal request ids, trusted scope matches configured backend, and execution_timeout, terminal_sql_timeout, and terminal_interrupt_grace are all > 0 | WaitingAdmission(admission_token) | Derive AdmissionKey from TxKey+scope; mint the admission token plus distinct root/cancel cutoff gates; insert the complete key/epoch/durations, gates, and both owned waiters; and enqueue only (AdmissionKey,token). No backend Reserve, claim, root frame, session, or timer exists. A duplicate id returns DuplicateRequest; a mismatched backend scope returns InvalidAdmissionScope; any zero duration returns InvalidTransactionTimeout. The rejected Create replies to both supplied continuations and does not insert. |
| WaitingAdmission(stored_token) | AdmissionGranted(event_token,claim), event_token == stored_token, synchronous reserve succeeds | Preparing | Move the owned ClaimGuard into TxEntry; set deadline_at=now+execution_timeout; call deadline_slots.arm_initial(Execution,fresh_generation,deadline_at); call reserve with the ClaimGuard proof, exact cutoff Arcs, and this exact deadline_slots Arc. Before enqueueing Prepare, copy ReservationHandle.actor_generation into backend_actor_generation, retain Armed(handle), and set session=Acquiring(command token). Reserve performs no authority read/BEGIN, so neither can race these stores. |
| WaitingAdmission(stored_token) | AdmissionGranted(event_token,claim), event_token == stored_token, synchronous reserve fails with ActorError e | Settled(AdmissionFailed(map_reserve_admission_error(e))) | Disarm the just-created execution generation if any, release the claim, and answer both owned continuations. The adapter is total and preserves e as AdmissionError::Reserve(e), including ActorSaturated, ActorUnavailable, and invariant failures. No Prepare, authority read, or BEGIN was sent; backend_actor_generation and backend_reservation remain None. |
| WaitingAdmission(stored_token) | AdmissionFailed(event_token,e), event_token == stored_token | Settled(AdmissionFailed(e)) | Remove the admission waiter. No rollback or claim release is asserted because no claim was won. |
| WaitingAdmission | synchronously routed Cancel{cause,order:PreAdmission} or DetachRequested(exact authority) | Settled(forced_outcome(cause)) or Settled(Detached) | The trusted start handle may construct PreAdmission only under the entry reducer lock while this exact key has no backend generation or ReservationControl. Remove the admission waiter and complete both owned continuations; issue no SQL. Every queued or actor-derived CancelOrder here is StaleTransactionCompletion. |
| WaitingAdmission | LifecycleObserved classified Current | WaitingAdmission | Keep waiting; change no identity or epoch. |
| WaitingAdmission | LifecycleObserved classified ReResolve | Settled(EpochChanged) | Remove the waiter and return retryable tx_epoch_changed; issue no SQL. |
| WaitingAdmission | LifecycleObserved classified Deny(r) | Settled(denial_outcome(r)) | Remove the waiter; terminally deny without SQL. |
| Preparing | PreparationCompleted(Observed(session,authority)) classified Current, matching token | Starting | Store begin_epoch=resolved_epoch, begin/effective ceiling=authority.ceiling; set session=Command(new_token,PreBegin(session)); retain ClaimGuard; issue BEGIN/setup exactly once. |
| Preparing | PreparationCompleted(Observed(session,authority)) classified ReResolve | Cancelling(Forced(EpochChanged),NoTransaction,Awaiting) | Retire the prepared/no-BEGIN reservation through explicit Cancel: replace Execution with CancellationSql and await CancelAck::NoTransaction before releasing the claim or disarming the handle. The caller then receives retryable tx_epoch_changed and re-resolves to a fresh TxKey; this entry never follows an epoch in place. |
| Preparing | PreparationCompleted(Observed(session,authority)) classified Deny(r) | Cancelling(Forced(AuthorityDenied(r)),NoTransaction,Awaiting) | Issue no BEGIN/data SQL. Replace Execution with CancellationSql and retire the actor reservation through CancelAck::NoTransaction before returning the denial or releasing admission. |
| Preparing | PreparationCompleted(Failed(e)) | Cancelling(SetupFailed(e),NoTransaction,Awaiting) | Preserve e, replace Execution with CancellationSql, and retire the actor reservation through explicit Cancel. Only CancelAck::NoTransaction maps to BeginFailed(e); every other ack follows the mismatch/indeterminate matrix. |
| Preparing | Cancel{order:ForceWon/AfterNonterminalCompletion}, DetachRequested(exact authority) after that force, or current Execution DeadlineFired whose inline arbitration returns InlineApplyForce(reason) | Cancelling(Forced(reason),NoTransaction,Awaiting) | Latch the first forced reason; deadline_slots.replace_current(Execution,CancellationSql,fresh_generation,now+terminal_sql_timeout); use the pre-minted cancellation token/cutoff already stored in the actor control; cancel acquisition. Claim release waits for CancellationCompleted or a fenced indeterminate acknowledgement. |
| Starting | BeginCompleted(Opened(tx)), matching token | Idle | Install tx as Registry owner; create one Open root frame with an empty effect buffer; call publish_ready with that root. The initial terminal waiter remains owned by TxEntry while the creator callback runs. |
| Starting | BeginCompleted(NotOpened(e)) | Cancelling(SetupFailed(e),NoTransaction,Awaiting) | Before enqueueing this completion the actor stores ActorPhase::BeginNotOpened. BEGIN is proved absent but the reservation/control permit is still live. Replace Execution with CancellationSql and retire it through explicit Cancel; cancel_proof accepts NoSqlStarted from that phase and only then settles BeginFailed(e). |
| Starting | BeginCompleted(MayHaveOpened(e)) | Cancelling(BeginUncertain(e),AbortIfOpened) | Quarantine ownership and request backend cancellation; do not release admission until the acknowledgement proves cleanup. A proved cleanup yields BeginFailed(e); indeterminate cleanup preserves e as its source. |
| Starting | Cancel{order:ForceWon/AfterNonterminalCompletion}, DetachRequested(exact authority) after that force, or current Execution DeadlineFired whose inline arbitration returns InlineApplyForce(reason) | Cancelling(Forced(reason),AbortIfOpened,Awaiting) | If forcing wins the command gate, deadline_slots.replace_current(Execution,CancellationSql,fresh_generation,now+terminal_sql_timeout), suppress ordinary BeginCompleted, interrupt/cancel BEGIN, and emit one CancellationCompleted. If completion won, it is keyed first and this force is reduced from its resulting state. An opened transaction is rolled back, never exposed to creator data SQL. |
| Preparing or Starting | LifecycleObserved classified ReResolve/Deny | Cancelling(Forced(reason),NoTransaction/AbortIfOpened) | Convert the disposition to a forced cause; replace Execution with CancellationSql in the shared deadline machine, cancel preparation/BEGIN, and wait for cleanup proof. |
| Idle | StartOperation naming current top frame | InFlight(action, Authority) | Before changing state, call `admit_explicit_execute_before_enqueue`; retain its fresh cutoff, private DataDeliveryPermit, and exact registered hard-stop trigger in ActiveAction::Data and copy them into ActiveCommandRef/Execute. A construction error leaves Idle and sends no authority/data SQL. Keep the data session parked; issue authority only on the platform-role session. The same cutoff/permit/trigger lets a terminal data abort recover without Registry re-entry from an actor owner lock. |
| Idle | OpenFrame, parent is current top and open depth below eight | InFlight(OpenSavepoint, FrameControl) | Mint a never-reused FrameId and monotonic savepoint name from next_frame_sequence, retain request_id/reply, insert that child as Opening, transfer session, and issue SAVEPOINT. |
| Idle | CloseFrame(Release), target is current non-root top | InFlight(ReleaseSavepoint, FrameControl) | Build PendingFrameClose with its first waiter, mark Releasing, transfer session, and issue RELEASE. |
| Idle | CloseFrame(RollbackTo), target is current non-root top | InFlight(RollbackToSavepoint, FrameControl) | Retain the waiter with BodyRejected(error), mark RollingBackTo, transfer session, and issue ROLLBACK TO. Do not pop or clear effects yet. |
| Idle | SettleRoot(Commit), root is the only frame | result of `reduce_first_root_settle` | Retain the attempt/waiter, construct the exact root delivery, and call the closed preinstall reducer. Installed transfers the lease, replaces Execution with TerminalSql, and issues one COMMIT. CancelWon enters reserve-time cancellation without root SQL. FenceWon/producer fault enters incarnation-qualified generation retirement without root SQL. |
| Idle | SettleRoot(Rollback), any frame stack | result of `reduce_first_root_settle` | Retain the attempt/waiter and rollback cause, then use the same closed preinstall reducer. Only Installed issues one root ROLLBACK (closing all savepoints); the other branches issue no root SQL and preserve the accepted waiter through cancellation/retirement. Preserve frame effects until terminal classification, then discard for every non-commit outcome. |
| Idle | SettleRoot(Commit), a child remains | result of `reduce_first_root_settle` with forced Rollback intent | Record SavepointFrameLeaked, retain the accepted waiter, and pass Rollback—not Commit—to the preinstall reducer. Only Installed issues root ROLLBACK. A cancel/fence race owns cleanup and still answers the same waiter; COMMIT is never sent over a live child. |
| Idle | Cancel{order:ForceWon/AfterNonterminalCompletion}, DetachRequested(exact authority) after that force, or current Execution DeadlineFired whose inline arbitration returns InlineApplyForce(reason) | Cancelling(Forced(reason),OpenTransaction,Awaiting) | Win/publish the immutable cancel cause, replace Execution with CancellationSql in the shared deadline machine, and send explicit Cancel(ReservationId) with the preinstalled cancellation token/delivery. Issue no root TerminalCompleted path: the actor returns CancellationCompleted only after rollback/cleanup proof. |
| Idle | LifecycleObserved classified ReResolve/Deny | Cancelling(Forced(reason),OpenTransaction,Awaiting) | Convert the authority disposition to EpochChanged or AuthorityDenied, win/publish the same cancel cause, replace Execution with CancellationSql, issue no new data SQL, and await the actor's rollback proof. |
| InFlight(Authority) | AuthorityCompleted(Observed) classified Current | InFlight(same action, DataSql) | Meet effective_ceiling with the observation, transfer the data session to the same token, and issue data SQL. |
| InFlight(Authority) | AuthorityCompleted(Observed) classified ReResolve | Cancelling(Forced(EpochChanged),OpenTransaction,Awaiting) | Resolve the active operation with TxEpochChanged; emit FinishOrCancelFenceJobInfallible for the Data action's dormant hard-stop job because Data SQL never became visible; publish the immutable EpochChanged cancel cause, replace Execution with CancellationSql, issue no data SQL, and await CancellationCompleted rollback proof. |
| InFlight(Authority) | AuthorityCompleted(Observed) classified Deny(r) | Cancelling(Forced(AuthorityDenied(r)),OpenTransaction,Awaiting) | Resolve the active operation with the typed terminal denial; emit FinishOrCancelFenceJobInfallible for the never-used DataAbort job; publish that immutable cancel cause, replace Execution with CancellationSql, issue no data SQL, and await CancellationCompleted rollback proof. |
| InFlight(Authority) | AuthorityCompleted(Unavailable(e)) | Idle | Consume the Data action's endpoint with ReplyEffect::Data(Err(TxOutcomeError::Database(e))) and emit FinishOrCancelFenceJobInfallible(action.hard_stop_trigger.job()); no data SQL, effect, or poison is produced. Restore the data lease to SessionOwner::Registry and logical operation availability. |
| InFlight(DataSql) | DataCompleted(Succeeded(value,effects)) | Idle | Cancel the action's still-dormant data-abort job, return the session to Registry, append effects in order to the current frame, and consume the Data endpoint with ReplyEffect::Data(Ok(value)). |
| InFlight(DataSql) | DataCompleted(Failed(e,Healthy)) | Idle | Cancel the action's still-dormant data-abort job, return the session, append no effects, and consume the Data endpoint with Err(TxOutcomeError::Database(e)); reject only this operation. |
| InFlight(DataSql) | DataCompleted(Failed(e,Poisoned)) | Poisoned | Cancel the action's still-dormant data-abort job, return session, record recovery_frame, append no effects, and consume the Data endpoint with Err(TxOutcomeError::Database(e)). |
| InFlight(DataSql) | DataCompleted(Failed(e,TransactionRolledBack)) plus matching `Some(BackendEndProof)` | Settled(AbortedByBackend(e)) | Before consuming the completion permit or replying, validate proof key, actor generation, command token, and private seal. Only then consume the Data endpoint with Err(TxOutcomeError::Database(e.clone())) and call `publish_settled(...,BackendConfirmedEnded(proof))`. A missing/foreign proof takes the Quarantining mismatch arm and sends no reply before retirement. |
| InFlight(DataSql) | DataAbortCompleted(proof), matching token and retirement=Ended(p) or GenerationRetired(p), exact proof validation succeeds | Settled(AbortedByBackend(proof.error)) | Consume the Data endpoint only in this terminal arm. Ended uses BackendConfirmedEnded(p); GenerationRetired uses that sealed proof. Then discard effects, disarm timers, release claim, and publish the classified terminal error. |
| InFlight(DataSql) | DataAbortCompleted(proof), matching token and retirement=NeedsGenerationRetirement or a foreign/malformed proof | Quarantining(AbortedByBackend(proof.error) or backend_retirement_proof_mismatch) | Move the Data endpoint to deferred_replies; preserve every frame/effect, reservation, session, timer, and claim; start exact generation retirement. No reply is sent before GenerationRetired. |
| InFlight(DataSql) | DataCompleted(Failed(e,Unknown)) | Cancelling(BackendUnknown(e),QuarantineUnknown) | Move the data responder to deferred_replies with original_error=e, quarantine the session, and prove cleanup before releasing admission. |
| InFlight(OpenSavepoint) | FrameCommandCompleted(Opened(Ok)) | Idle | Return session, mark the Opening frame Open, and consume the FrameOpen endpoint with Ok(FrameOpenReply::Ready{frame}). The caller, not Registry, now runs the nested callback and later sends CloseFrame; no callback is hidden in Frame. |
| InFlight(OpenSavepoint) | FrameCommandCompleted(Opened(Err(TxStatementError{error,health:Healthy}))) | Idle | Remove the never-opened frame, return session, and consume the FrameOpen endpoint with Ok(FrameOpenReply::Failed(TxOutcomeError::SavepointOpenFailed(error))). |
| InFlight(OpenSavepoint) | FrameCommandCompleted(Opened(Err(TxStatementError{error,health:Poisoned}))) | Poisoned | Remove the never-opened frame, return session, set the parent as recovery_frame, and consume the FrameOpen endpoint with Ok(FrameOpenReply::Failed(TxOutcomeError::SavepointOpenFailed(error))). |
| InFlight(OpenSavepoint) | FrameCommandCompleted(Opened(Err(TxStatementError{error,health:TransactionRolledBack}))) plus matching `Some(BackendEndProof)` | Settled(AbortedByBackend(error)) | Before consuming the completion permit or responder, validate proof key, generation, command token, and seal. Then consume the FrameOpen endpoint with SavepointOpenFailed and publish with BackendConfirmedEnded(proof). A missing/foreign proof defers that responder and enters Quarantining; it cannot expose the result first. |
| InFlight(OpenSavepoint) | FrameCommandCompleted(Opened(Err(TxStatementError{error,health:Unknown}))) | Cancelling(BackendUnknown(error),QuarantineUnknown) | Move the open responder to deferred_replies with error, remove the never-opened frame, and quarantine cleanup. |
| InFlight(ReleaseSavepoint) | FrameCommandCompleted(Released(Ok)) | Idle | Return session; pop child; append its effects to the parent in order; build one Arc<FrameCompletion::Resolved(value)> from the retained close kind and consume every FrameClose endpoint with a clone. |
| InFlight(ReleaseSavepoint) | FrameCommandCompleted(Released(Err(statement_error @ TxStatementError{health,..}))), matches!(health,Healthy or Poisoned) | InFlight(RollbackToSavepoint) | Preserve the entire statement_error as the prior failure, mark RollingBackTo, and issue recovery ROLLBACK TO. Do not merge effects. |
| InFlight(ReleaseSavepoint) | FrameCommandCompleted(Released(Err(TxStatementError{error,health:TransactionRolledBack}))) plus matching `Some(BackendEndProof)` | Settled(AbortedByBackend(error)) | Validate the exact proof before consuming either the completion permit or any close endpoint. Then reply once with the shared SavepointReleaseFailed result and publish with BackendConfirmedEnded(proof). A missing/foreign proof defers every waiter and enters Quarantining. |
| InFlight(ReleaseSavepoint) | FrameCommandCompleted(Released(Err(TxStatementError{error,health:Unknown}))) | Cancelling(BackendUnknown(error),QuarantineUnknown) | Move every release waiter to deferred_replies with error; quarantine and do not attempt another frame command on an unknown session. |
| InFlight(RollbackToSavepoint) | FrameCommandCompleted(RolledBackTo(Ok)) | InFlight(ReleaseAfterRollbackTo) | Clear child effects immediately, mark ReleasingAfterRollbackTo, and issue RELEASE for the same name. |
| InFlight(RollbackToSavepoint) | FrameCommandCompleted(RolledBackTo(Err(TxStatementError{error,health}))), matches!(health,Healthy or Poisoned) | Poisoned | Return session; retain frame/effects; consume every waiter with one shared Failed(SavepointRollbackFailed{prior: prior(after), cleanup:error}); require root cleanup. |
| InFlight(RollbackToSavepoint) | FrameCommandCompleted(RolledBackTo(Err(TxStatementError{error,health:TransactionRolledBack}))) plus matching `Some(BackendEndProof)` | Settled(AbortedByBackend(error)) | Validate the exact proof before consuming the completion permit or waiters. Then reply with the shared composite SavepointRollbackFailed result and publish with BackendConfirmedEnded(proof). A missing/foreign proof defers every waiter and enters Quarantining. |
| InFlight(RollbackToSavepoint) | FrameCommandCompleted(RolledBackTo(Err(TxStatementError{error,health:Unknown}))) | Cancelling(BackendUnknown(error),QuarantineUnknown) | Move every rollback-to waiter to deferred_replies with error; quarantine and publish nothing. |
| InFlight(ReleaseAfterRollbackTo) | FrameCommandCompleted(ReleasedAfterRollbackTo(Ok)) | Idle | Return session, pop the now-empty child, run finish_after_rollback_to(after,Ok), and send its one shared FrameCompletion to every waiter. The outer transaction is healthy. |
| InFlight(ReleaseAfterRollbackTo) | FrameCommandCompleted(ReleasedAfterRollbackTo(Err(statement_error @ TxStatementError{health,..}))), matches!(health,Healthy or Poisoned) | Poisoned | Return session; leave the empty child present; run finish_after_rollback_to(after,Err(statement_error)) so the reply retains both prior and cleanup errors; send one shared completion to every waiter and require root cleanup. |
| InFlight(ReleaseAfterRollbackTo) | FrameCommandCompleted(ReleasedAfterRollbackTo(Err(statement_error @ TxStatementError{ref error,health:TransactionRolledBack}))) plus matching `Some(BackendEndProof)` | Settled(AbortedByBackend(error.clone())) | Validate the exact proof before consuming the completion permit or waiters. Then run finish_after_rollback_to and publish with BackendConfirmedEnded(proof). A missing/foreign proof defers every waiter and enters Quarantining. |
| InFlight(ReleaseAfterRollbackTo) | FrameCommandCompleted(ReleasedAfterRollbackTo(Err(TxStatementError{error,health:Unknown}))) | Cancelling(BackendUnknown(error),QuarantineUnknown) | Move every retained waiter to deferred_replies with error; quarantine and publish nothing. |
| InFlight(Data action) | CloseFrame, valid current target | Quiescing | Latch one PendingFrameClose with its waiter. Issue no frame SQL until the data completion restores the session. |
| InFlight(any action) | SettleRoot | Quiescing | Latch one RootIntent. Issue no terminal SQL until the active completion restores or retires the session. |
| InFlight(closing frame control) | CloseFrame(Join) has the stored FrameCloseAttemptId, with new request_id | same InFlight | Append the reply to that action’s waiter vector; Join carries no JS payload. The same request_id returns DuplicateRequest; a different attempt id or any First returns TransactionSettleConflict. |
| InFlight(any stage) | Cancel{order:ForceWon/AfterNonterminalCompletion}, DetachRequested(exact authority) after that force, or current Execution DeadlineFired whose inline arbitration returns InlineApplyForce(reason) | Cancelling(Forced(reason),OpenTransaction,Awaiting) | Move active responders to deferred_replies; replace Execution with CancellationSql in deadline_slots; use the already-installed cancel cutoff/token; signal the exact active backend command; accept no new operation. |
| InFlight(any stage) | LifecycleObserved classified ReResolve/Deny | Cancelling(Forced(reason),OpenTransaction,Awaiting) | Move active responders to deferred_replies; replace Execution with CancellationSql in deadline_slots; cancel the active command and replace any normal result with actor-proved cleanup; issue no later data SQL. |
| Quiescing | matching AuthorityCompleted | Quiescing(DataSql), drive_pending, or Cancelling(Forced(reason),OpenTransaction,Awaiting) | Current starts data SQL and retains pending. Unavailable cancels/finishes the never-used DataAbort job, restores the data lease to Registry, then drives pending. ReResolve/Deny likewise finish/cancel that job, resolve the active data responder with the typed authority error, move pending frame responders to deferred_replies (a root responder is already terminal_waiters), publish the immutable cancel cause, replace Execution with CancellationSql, and await actor rollback proof; it never silently discards pending. |
| Quiescing(DataSql with active Data action) | matching DataAbortCompleted(proof), retirement=Ended(p) or GenerationRetired(p), exact proof validation succeeds | Settled(AbortedByBackend(proof.error)) | Only after proof validation reply to the active Data request and pending Frame/Root waiters with their typed projections. Ended uses BackendConfirmedEnded(p); GenerationRetired uses that sealed proof. Discard effects, disarm timers, release claim, and enter Settled. |
| Quiescing(DataSql with active Data action) | matching DataAbortCompleted(proof), retirement=NeedsGenerationRetirement or a foreign/malformed proof | Quarantining(AbortedByBackend(proof.error) or backend_retirement_proof_mismatch) | Move the active Data responder plus every pending frame/root waiter into deferred_replies, preserving frames/effects/session/claim. Start exact generation retirement and send no reply until its proof arrives. |
| Quiescing | matching DataCompleted or FrameCommandCompleted | continue_quiescing(intermediate,pending) | Apply the corresponding InFlight row. If it directly proves transaction end, reply_pending_frame_from_terminal supplies the exact typed pending-frame reply while the active responder receives that InFlight row's typed error and a pending Root remains in terminal_waiters. If it produces another InFlight frame-cleanup command, retain the same pending intent in a new Quiescing state; only Idle/Poisoned/Settled/Cancelling/Settling may reach drive_pending. |
| Quiescing | CloseFrame(Join)/SettleRoot(Join) has the retained attempt id, with new request_id | Quiescing | Append its reply to the retained pending/terminal waiter vector; Join carries no payload and issues no second command. The same request id returns DuplicateRequest; a different attempt id or any First returns TransactionSettleConflict. |
| Quiescing | Cancel{order:ForceWon/AfterNonterminalCompletion}, DetachRequested/LifecycleObserved after that force, or current Execution DeadlineFired whose inline arbitration returns InlineApplyForce(reason) | Cancelling(Forced(reason),OpenTransaction,Awaiting) | Move active/pending-frame responders to deferred_replies; a root responder is already terminal_waiters. Replace the normal intent with actor cleanup; replace Execution with CancellationSql in deadline_slots; signal the active command. Completion-won orders completion then force as for InFlight. |
| Poisoned | CloseFrame(RollbackTo), target is recovery/current top child | InFlight(RollbackToSavepoint) | Issue ROLLBACK TO; on its success clear child effects, RELEASE, pop, and return the parent to Idle. |
| Poisoned | CloseFrame(Release), target is recovery/current top child | InFlight(RollbackToSavepoint) | A poisoned child cannot release. Recover with rollback-to/release, discard child effects, and return transaction_poisoned to that nested caller. |
| Poisoned | SettleRoot(Commit), root only | result of `reduce_first_root_settle` | Legal and reachable, but first exhaust the preinstall race decision. Only Installed issues COMMIT and judges its tag; a ROLLBACK response is CommitRolledBack, never success. CancelWon/fence branches issue no root SQL. |
| Poisoned | SettleRoot(Commit), a child remains | result of `reduce_first_root_settle` with forced Rollback intent | Record SavepointFrameLeaked; only Installed issues root rollback. Every other preinstall decision preserves the waiter and issues no root SQL. |
| Poisoned | SettleRoot(Rollback), any stack | result of `reduce_first_root_settle` | This is normal creator settlement only in Installed, which later claims OWNER_COMPLETE and issues one root ROLLBACK. CancelWon or FenceWon owns the terminal path and no root SQL is issued. |
| Poisoned | Cancel{order:ForceWon/AfterNonterminalCompletion}; DetachRequested/LifecycleObserved after that force; or current Execution DeadlineFired whose inline arbitration returns InlineApplyForce(reason) | Cancelling(Forced(reason),OpenTransaction,Awaiting) | Publish the immutable cancel cause, replace Execution with CancellationSql, and send explicit Cancel(ReservationId). The actor rolls back the poisoned transaction and returns CancellationCompleted; no TerminalCompleted is fabricated. |
| InFlight or Quiescing, with matching retained completion-owned terminal candidate | Cancel{order:TerminalCompletionWon} | same state | Join its waiter and await the already-keyed DataAbort/terminal proof. This order is STC in Preparing, Starting, Idle, or Poisoned because those states have no retained completion-owned terminal candidate. |
| Preparing, Starting, Idle, InFlight, Quiescing, or Poisoned | Cancel{order:TerminalFenceWon} | same state | Join its waiter and await the state-legal generation-fence/retained-cutoff proof. The supervisor already made the generation unreachable. Do not replace a deadline or invent a terminal outcome. |
| Preparing, Starting, InFlight, or Quiescing | current Execution DeadlineFired whose inline arbitration returns InlineDeferredAfterNonterminal | same logical state with deadline_slots=Fired(Execution,event generation) | The actor had already keyed the matching nonterminal completion. Inline arbitration keyed an authenticated Cancel after it. Issue no cleanup and do not consume the active responder now. Reduce the completion normally, including exact command-gate retirement; its later Cancel event then enters Cancelling from the resulting state and replaces this Fired Execution slot with CancellationSql. |
| InFlight or Quiescing, with exact retained completion-owned terminal candidate | current Execution DeadlineFired whose inline arbitration returns InlineObserver(TerminalCompletionWon) | same state with deadline_slots=Fired(Execution,event generation) | Await the already-keyed retained terminal proof; it disarms or replaces the Fired slot. The same result in Preparing, Starting, Idle, or Poisoned is STC/protocol fault because no such candidate can exist; it must be generation-fenced rather than waited on. |
| Preparing, Starting, Idle, InFlight, Quiescing, or Poisoned | current Execution DeadlineFired whose inline arbitration returns InlineObserver(TerminalFenceWon) | same state with deadline_slots=Fired(Execution,event generation) | Await the already-keyed generation-fence proof; it disarms the Fired slot. Do not send Cancel. |
| WaitingAdmission, Preparing, Starting, Idle, InFlight, Quiescing, Poisoned, or Cancelling | LifecycleObserved classified Current | same state | Meet an existing effective ceiling with the new ceiling; never broaden. In Waiting/Preparing it is only an audit observation. |
| Cancelling | Cancel, DetachRequested, or LifecycleObserved classified ReResolve/Deny | Cancelling | The existing CleanupCause remains creator-visible; later causes are audit metadata. Join any supplied terminal waiter and send no second interrupt. Execution DeadlineFired is not in this arm: its cleared generation is SDL in the illegal matrix. |
| Cancelling(Awaiting{watchdog_fired=false,hard_stop}) | current CancellationSql DeadlineFired | Cancelling(Awaiting{watchdog_fired=true,same hard_stop}) | claim_fire already stored Fired(CancellationSql,generation). Interrupt the exact cleanup target once without changing OWNER_CANCEL, then arm CancellationHardStop at now+terminal_interrupt_grace using `hard_stop.trigger`; its route, supervisor job, and mailbox pins were installed before Reserve published the control, and its cause/deadline binding preceded the CancellationSql arm. |
| Cancelling(Awaiting{watchdog_fired=true,hard_stop}) | current CancellationHardStop DeadlineFired | Cancelling(HardStopping{same permit,trigger,fence_token}) | Do not allocate, pin, register, or arbitrate cancel_cutoff in the reducer. Move the retained `RegisteredExplicitHardStop` into HardStopping and emit PublishHardStop(trigger.clone()). Keep deadline Fired plus responders/claim while the durable job performs result-versus-fence arbitration. Every completion consumes the permit and terminal cleanup cancels or finishes its job. |
| Cancelling(HardStopping) | matching CancellationCompleted wins cancel_cutoff before publisher fence | result of cleanup matrix below | Call consume_hard_stop_permit_exact on the stored Arc, then reduce the retained acknowledgement exactly as the Awaiting arm. **Do not disarm first:** retain Fired(CancellationHardStop) through the cleanup discriminator. A direct `publish_settled` disarms it; a Quarantining result atomically replaces it with RetirementFence. No physical fence or fallback is issued by this result-won arm. |
| Cancelling(Awaiting) | CancellationCompleted(ack), token == entry.cancellation_token and cancel cutoff Result won | result of cleanup matrix below | Retain the current CancellationSql/CancellationHardStop deadline through the total cleanup and retirement discriminator. A direct `publish_settled(...,BackendConfirmedEnded/GenerationRetired)` disarms it; `begin_generation_retirement` replaces it with RetirementFence before scheduling retirement. No matching pair or retirement variant falls through or becomes stale. |
| Cancelling(Awaiting{hard_stop,..}) | CancellationHardStopCompleted(hard_stop.fence_token,same actor generation,ack,sealed proof), cancel cutoff is FenceResult for hard_stop.trigger.job | Settled(outcome selected by cleanup matrix) | This is generation death before the grace-stage state transition. Validate the same permit/job/token and GenerationRetirementProof required in HardStopping, consume the permit, and publish directly with GenerationRetired(proof), disarming whichever CancellationSql/CancellationHardStop generation is current. The physical fence—not timer phase—authorizes this early completion. |
| Cancelling(HardStopping) | CancellationHardStopCompleted(fence token,same generation,ack,proof) | Settled(outcome selected by the cleanup matrix) | Call consume_hard_stop_permit_exact on the stored Arc and validate the exact sealed GenerationRetirementProof. The fence proves the actor generation cannot execute later SQL, not that rollback happened: Indeterminate(e) selects CleanupIndeterminate; a semantically inconsistent NoTransaction/RolledBack selects `CleanupProtocolMismatch{goal,acknowledgement:ack.kind()}`; a consistent acknowledgement selects cleanup_outcome. Every case calls `publish_settled(...,GenerationRetired(proof))` directly, which disarms Fired(CancellationHardStop). Never enter Quarantining again for an already physically retired generation. |
| Preparing or Starting | BackendActorUnavailable(actor_generation,e), actor_generation == backend_actor_generation, after supervisor proves both lanes closed | Settled(BeginFailed(e)) | The actor can execute no further SQL and close ended any uncommitted transaction. publish_settled disarms deadline_slots, clears backend_actor_generation, owns/releases the claim, and fails both continuations; it converts the armed handle to a no-cancel terminal lease. |
| Idle, InFlight, Quiescing, or Poisoned | BackendActorUnavailable(actor_generation,e), actor_generation == backend_actor_generation, after supervisor proves both lanes closed | Settled(AbortedByBackend(e)) | Before replacing Quiescing, consume its pending intent: move a pending Frame to deferred_replies with NeverIssuedPending provenance; a pending Root already owns its responder in terminal_waiters. Then discard effects, drain active/deferred responders with Database(e), and call publish_settled, which disarms the one deadline state, clears the actor generation, releases the owned claim, and converts the handle. OWNER_* terminal paths instead use their exact cutoff. |
| Settling(Commit) | TerminalCompleted(Committed), matching token | Settled(Committed(value)) | Atomically detach root effects and enqueue each exactly once in order; clear frames; retire session; disarm watchdog; release claim; resolve value. |
| Settling(Rollback) | TerminalCompleted(Committed) | Quarantining(TerminalResultMismatch) | Publish nothing. Preserve the immutable mismatch, every waiter, frame/effect buffer, and claim; begin exact generation retirement. Never report the rollback cause as satisfied. |
| Settling(Commit) | TerminalCompleted(RolledBack) | Settled(CommitRolledBack) | Discard all effects, clear resources, release claim, and reject commit_rolled_back. |
| Settling(Rollback) | TerminalCompleted(RolledBack) | Settled(RolledBack(original cause)) | Discard effects; clear resources; release claim; propagate original body/forced error. |
| Settling(Commit) | TerminalCompleted(Failed,DefinitelyNotCommitted,retirement=Ended(proof)) | Settled(CommitFailed) | The actor emitted this certainty only after compensating rollback confirmed end. Discard effects and publish with BackendConfirmedEnded(proof). A NeedsGenerationRetirement payload takes the next row instead. |
| Settling(Commit) | TerminalCompleted(Failed,DefinitelyNotCommitted,retirement=NeedsGenerationRetirement) | Quarantining(CommitFailed) | Preserve the outcome but retire/fence the generation before discard, reply, or claim release. |
| Settling(Commit) | TerminalCompleted(Failed,Indeterminate) | Quarantining(CommitIndeterminate) | Preserve all ownership until exact generation retirement, then reject commit_failed_indeterminate; never report rollback. |
| Settling(Rollback) | TerminalCompleted(Failed,any certainty) | Quarantining(RollbackFailed) | Preserve the original cause under rollback_failed and retire/fence the generation before discard, reply, or claim release. |
| Settling(hard_stop,intent) | TerminalHardStopCompleted(hard_stop.fence_token,same actor generation,sealed proof), root cutoff is FenceResult for hard_stop.trigger.job | Settled(CommitIndeterminate(actor_unavailable_during_terminal_handoff)) for Commit; Settled(RollbackFailed{rollback:actor_unavailable_during_terminal_handoff,original}) for Rollback | This is generation death before SC-1 entered HardStopping. Validate and consume the exact permit/job/token plus GenerationRetirementProof, discard effects, and publish with GenerationRetired(proof), disarming the current TerminalSql/TerminalHardStop slot. No ordinary TerminalCompleted is required. |
| Settling | SettleRoot(Join) has the stored RootSettleAttemptId, new request_id | Settling | Append request_id/reply to terminal_waiters; issue no SQL. The same retained request id returns DuplicateRequest; a different attempt id or any First returns TransactionSettleConflict. |
| Settling(any watchdog flag) | Cancel{cause,order: ForceWon/AfterNonterminalCompletion} and actor owner is still Open | Cancelling(Forced(cause),OpenTransaction,Awaiting) | This is the Settle-before-owner-CAS gap. The force publisher already set CANCEL_INTENT and ForceQueued before keyed-enqueueing this event, so root completion cannot now claim. Keep the immutable unused root route for stale hard-stop validation, select the independent reserve-time cancel route, replace TerminalSql or TerminalHardStop with fresh CancellationSql, call publish_terminal_route_armed, and await the sole retained Cancel command. Suppress the root completion if it arrives Lost; do not issue COMMIT/ROLLBACK under root intent. Detach/Lifecycle sources reach this arm through their first, ordered Cancel event. |
| Settling | Cancel{order: TerminalCompletionWon/TerminalFenceWon/Joined} | Settling | Join the retained terminal waiter. TerminalCompletionWon/Joined leaves reality to its retained result. For TerminalFenceWon, the supervisor completes the preinstalled root cutoff and keyed-publishes TerminalHardStopCompleted with a sealed generation proof; the preceding row accepts it even before the grace-stage transition. This deliberately conservative rule never infers whether terminal FFI crossed an unobserved boundary. This Cancel alone invents no outcome. |
| Settling | LifecycleObserved classified Current | Settling | Audit the observation and meet effective_ceiling with the observed ceiling; issue no force event and never broaden authority. |
| Settling | exact DetachRequested or LifecycleObserved classified ReResolve/Deny, received after its ordered Cancel event whose authenticated order did not leave Settling | Settling | Audit/update trusted context only. It carries no independent force decision and cannot reverse the authenticated CancelOrder. If the preceding Cancel left Settling, the corresponding Cancelling audit row—not this match arm—receives the later event. |
| Settling(watchdog_fired=false,hard_stop) | current TerminalSql DeadlineFired | Settling(watchdog_fired=true,same hard_stop) | Interrupt the exact terminal command once, retaining completion ownership; clear TerminalSql and arm TerminalHardStop at now+terminal_interrupt_grace with `hard_stop.trigger`. The route/job was registered before terminal SQL was issued. A result during grace still selects the table's real outcome. |
| Settling(watchdog_fired=true,token=terminal_token,hard_stop) | current TerminalHardStop DeadlineFired | HardStopping(terminal_token,hard_stop.fence_token,intent,resource_generation,hard_stop.permit,hard_stop.trigger,pending_cancel=None) | Do not allocate, pin, register, or arbitrate root cutoff in the reducer. Move the already-retained route into HardStopping and emit PublishHardStop(trigger.clone()). A late TerminalCompleted authenticates with terminal_token; physical completion/preemption authenticates with the distinct fence_token. Keep the deadline Fired and all ownership while the registered job performs the three-way arbitration. |
| HardStopping | matching TerminalCompleted wins root cutoff before publisher fence | result of the corresponding Settling terminal-result row | Call consume_hard_stop_permit_exact on the stored Arc, disarm the deadline, and reduce the real retained root result. No physical fence/fallback is issued. |
| HardStopping | TerminalHardStopCompleted(fence_token,same resource_generation) | Settled(CommitIndeterminate(terminal_deadline_exceeded)) for Commit; Settled(RollbackFailed{rollback: terminal_deadline_exceeded, original}) for Rollback | Call consume_hard_stop_permit_exact on the stored Arc. The acknowledgement proves the generation is no longer routable and can execute no command after its already-issued terminal statement. Discard every effect, disarm both terminal generations, release the claim, store the immutable indeterminate outcome, and wake all waiters. The quarantined statement may have completed, so Commit is never reported definitely failed and Rollback is never reported confirmed. |
| HardStopping | matching TerminalHardStopPreempted(same permit Arc/id and resource generation), and pending_cancel is None or equals event cause | Cancelling(Forced(cause),OpenTransaction,Awaiting) | Call consume_hard_stop_permit_exact on that Arc. The keyed preemption proves OWNER_OPEN+CANCEL_INTENT won before the root cutoff. Drop the consumed root permit, retain the unused root route only as stale evidence, replace Fired(TerminalHardStop) with fresh Armed(CancellationSql), call publish_terminal_route_armed, and let the already-retained sole Cancel command claim OWNER_CANCEL. The separately keyed Cancel event later joins idempotently. No root fence/result was published. |
| HardStopping | matching TerminalHardStopPreempted but pending_cancel is Some(other) and other != event cause | Cancelling(BackendUnknown(cancellation_cause_mismatch_db_error(other,event cause)),QuarantineUnknown,Awaiting) | This is an authenticated actor protocol fault, not a reply-bearing illegal request. Call consume_hard_stop_permit_exact, arm the reserve-time cancellation route/deadline, let the sole Cancel prove rollback or fence, and eventually publish the typed coded `cancellation_cause_mismatch` database error through the cleanup matrix. Never release admission or reuse the generation merely because the two messages disagreed. |
| HardStopping(pending_cancel) | Cancel{cause,order: ForceWon/AfterNonterminalCompletion} arrives before TerminalHardStopPreempted, and pending_cancel is None or equals cause | HardStopping(pending_cancel=Some(cause)) | Retain/join its waiter; do not touch the cutoff, permit, deadline, or actor owner. The already-emitted PublishHardStop effect observes the full OWNER_OPEN+CANCEL_INTENT word and keyed-enqueues TerminalHardStopPreempted. If that preemption arrived first, the same Cancel is instead reduced by Cancelling's idempotent row. |
| HardStopping(pending_cancel=Some(other)) | Cancel{cause,order: ForceWon/AfterNonterminalCompletion}, other != cause | Cancelling(BackendUnknown(cancellation_cause_mismatch_db_error(other,cause)),QuarantineUnknown,Awaiting) | Consume the exact hard-stop permit, arm the reserve-time cancellation route/deadline, and retain all waiters until cleanup proof. Root-preemption claim and keyed enqueue are atomic, so there is either an already-keyed stale preemption or no claim; the root cutoff cannot choose a fence after CANCEL_INTENT. The public result is the typed coded protocol error after cleanup/quarantine. |
| HardStopping | Cancel{order: TerminalCompletionWon/TerminalFenceWon/Joined} or SettleRoot(Join) with the stored RootSettleAttemptId and a new request_id; exact DetachRequested; LifecycleObserved | HardStopping | Join the distinct terminal waiter or append audit metadata. A retained request id returns DuplicateRequest; another attempt id or any First returns TransactionSettleConflict. Do not change owner, send a second interrupt/fence, or weaken the outcome. |
| Quarantining | GenerationRetired(token,retirement_id,actor_generation,proof), all four values and proof seal match state/TxKey | Settled(pending_outcome) | Move the pending outcome out exactly once and call publish_settled with GenerationRetired(proof). Only now clear/discard frames, disarm RetirementFence, convert the reservation lease, drain every waiter, and release the claim. A mismatched token/id/generation/proof is STC and is pure. |
| Quarantining(retirement_attempt=n,superseded_fence_jobs) | current RetirementFence DeadlineFired | Quarantining(retirement_attempt=n+1,same jobs) | After `claim_fire` succeeds, mint `next_generation`, call `rearm_retirement(fired_generation,next_generation,now+terminal_interrupt_grace)` while still holding the reducer lock, increment `retirement_attempt`, and emit `RetireGenerationEffect{same key,token,retirement_id,actor_generation,superseded_fence_jobs:clone,force_physical:true}`. Every retry idempotently finishes/cancels those old jobs before fencing. Rearming precedes effect dispatch, so executor death is retried. `enqueue_once(retirement_id,...)` makes every original/retry one logical GenerationRetired event. Never mint a second retirement id or change the pending outcome. |
| Quarantining | matching BackendActorUnavailable carrying a GenerationRetirementProof for the same retirement_id/generation/key | Settled(pending_outcome) | Treat it exactly as matching GenerationRetired. This closes a concurrent supervisor-death notification without weakening proof requirements. |
| Quarantining(settle_attempt=Some(id)) | SettleRoot(Join(id)) with a new request_id | Quarantining | Retain the terminal waiter. A duplicate request id is DUP; First, a different attempt, or settle_attempt=None is CON. Issue no SQL. |
| Quarantining | Cancel(any authenticated order) | Quarantining | Join an optional waiter to the already-fixed pending terminal result. Do not publish CANCEL_INTENT, send Cancel, replace the pending outcome, or claim cleanup. |
| Quarantining | exact DetachRequested or LifecycleObserved | Quarantining | Record audit metadata only. Authority change cannot retarget or weaken the incarnation-qualified retirement already in progress. |
| Settled | SettleRoot | Settled | Return TerminalReply::Replay(outcome); issue no SQL/effect. |
| Settled | Cancel | Settled | Return TerminalReply::Replay(outcome); never claim a rollback. |
| Settled | LifecycleObserved | Settled | Retain immutable outcome and record the audit observation only. |
| Settled | exact DetachRequested | Settled | Retain immutable outcome and record the audit observation only; no interrupt or SQL. |
| Settled | Forget, `terminal_refs == 0`, and (`backend_reservation=None` or `backend_reservation=Some(Terminal)` whose lease refcount is one) | no entry | If backend_reservation is Some(Terminal), take it; its Drop sends only incarnation-qualified ForgetTerminal. If it is None, require that settlement used NoBackendSession and remove directly. Some(Armed) is a reducer invariant failure. First clear cutoff/permit fields and their detached endpoint pins, then remove the registry record, dedupe keys, and retained_request_ids. Forget never publishes Cancel. |

Cancellation cleanup is this total 4x3 match. The actor owns any required root
rollback through completion; no cancellation adapter returns a live transaction
to Registry. “Mismatch” means enter
Quarantining with `CleanupProtocolMismatch(goal,ack kind)`; do not discard
effects, wake terminal waiters, or release the claim until GenerationRetired.
Every direct Settled cell additionally requires
BackendTerminalRetirement::Ended from CancellationCompleted. An absent or
NeedsGenerationRetirement proof changes that cell to Quarantining with the same
outcome.

| CleanupGoal \ CancelAck | NoTransaction | RolledBack | Indeterminate(e) |
| --- | --- | --- | --- |
| NoTransaction | Settled(cleanup_outcome(cause)); release with Ended proof | Quarantining(CleanupProtocolMismatch); rollback was impossible in this phase | Quarantining(cleanup_indeterminate(cause,e)) |
| AbortIfOpened | Settled(cleanup_outcome(cause)); BEGIN proved absent, Ended proof | Settled(cleanup_outcome(cause)); rollback+Ended proved | Quarantining(cleanup_indeterminate(cause,e)) |
| OpenTransaction | Quarantining(CleanupProtocolMismatch); an enclosing transaction must exist | Settled(cleanup_outcome(cause)); whole transaction rollback+Ended proved | Quarantining(cleanup_indeterminate(cause,e)) |
| QuarantineUnknown | Quarantining(CleanupProtocolMismatch) | Settled(cleanup_outcome(cause)); cleanup+Ended proved | Quarantining(cleanup_indeterminate(cause,e)) |

Every transition into Settling first fixes terminal ownership—Complete for a
normal settle, or the already-won Cancel owner for cancellation cleanup—then
disarms the execution timer and arms a separate terminal-SQL watchdog for
now+terminal_sql_timeout, stores the terminal token/intent, and only then sends
terminal SQL. This includes drive_pending and CancellationCompleted. The
watchdog is disarmed only after a terminal result or connection quarantine. The
Forgotten object here is only a transaction result cache: it is not Fork C's
application-lifecycle tombstone, which is permanent and MUST NOT be cleared.

Every transition into Cancelling uses the one
`TerminalCutoffGate<CancelAck>` created with the entry and already installed in
the backend reservation, disarms Execution, arms CancellationSql for
`now + terminal_sql_timeout`, and
sets `phase=Awaiting{watchdog_fired:false}` before issuing cancel. A backend
cleanup result may publish only through that gate. Cancellation cleanup never
uses `RootFinishResult`, `TerminalCompleted`, or the root
`TerminalHardStopCompleted` event; its only completion forms are
`CancellationCompleted` and `CancellationHardStopCompleted`. This keeps
OWNER_CANCEL cleanup bounded without pretending it is a caller-requested root
settlement.

Cleanup causes map to terminal outcomes and rollback provenance by these total
functions. cleanup_indeterminate constructs a DbError::Coded with code
"cancellation_cleanup_failed" and retains both the original cause/error and the
cleanup error in its message/hint; it never drops the original failure.

~~~rust
fn forced_outcome(reason: ForcedReason) -> TerminalOutcome {
    match reason {
        ForcedReason::Cancel(cause) => TerminalOutcome::Cancelled(cause),
        ForcedReason::DeadlineExceeded => TerminalOutcome::DeadlineExceeded,
        ForcedReason::Detach => TerminalOutcome::Detached,
        ForcedReason::AuthorityDenied(reason) => denial_outcome(reason),
        ForcedReason::EpochChanged(_) => TerminalOutcome::EpochChanged,
    }
}

fn cleanup_outcome(cause: CleanupCause) -> TerminalOutcome {
    match cause {
        CleanupCause::Forced(reason) => forced_outcome(reason),
        CleanupCause::SetupFailed(error) =>
            TerminalOutcome::BeginFailed(error),
        CleanupCause::BeginUncertain(error) =>
            TerminalOutcome::BeginFailed(error),
        CleanupCause::BackendUnknown(error) =>
            TerminalOutcome::AbortedByBackend(error),
    }
}

fn cleanup_rollback_cause(cause: CleanupCause) -> RollbackCause {
    match cause {
        CleanupCause::Forced(reason) => RollbackCause::Forced(reason),
        CleanupCause::SetupFailed(error) =>
            RollbackCause::SetupFailed(error),
        CleanupCause::BeginUncertain(error) =>
            RollbackCause::BeginUncertain(error),
        CleanupCause::BackendUnknown(error) =>
            RollbackCause::BackendUnknown(error),
    }
}

fn denial_outcome(reason: AuthorityDenyReason) -> TerminalOutcome {
    match reason {
        AuthorityDenyReason::Deprovisioned =>
            TerminalOutcome::AppDeprovisioned,
        AuthorityDenyReason::AppIdMismatch
        | AuthorityDenyReason::DomainMismatch
        | AuthorityDenyReason::IncarnationMismatch =>
            TerminalOutcome::IncarnationDenied,
    }
}
~~~

Before any transition into Cancelling, `capture_outstanding` moves, rather than
drops, every **still-unresolved** responder owned by the active action and a
Quiescing pending frame close into TxEntry.deferred_replies. A row such as an
authority ReResolve/Deny first replies to the active Data request and therefore
captures only the pending frame responder; it cannot capture and later answer
the Data endpoint twice. A pending root responder already lives in
terminal_waiters. The final transition to Settled performs exactly one drain:

In the Close column, `Failed(x)` abbreviates
`Ok(Arc::new(FrameCompletion::Failed(x)))`; in the Open column it abbreviates
`Ok(FrameOpenReply::Failed(x))`. Protocol-matrix errors instead use the outer
`Err(TxProtocolError)`. The Data column is already the exact
`Result<DbRows,TxOutcomeError>` value. A grouped FrameClose builds one
SharedFrameCompletion, then consumes every owned endpoint with a clone.

| Final condition | Deferred Data reply | Deferred FrameOpen reply | Deferred FrameClose reply |
| --- | --- | --- | --- |
| CleanupProtocolMismatch or CancellationProtocolMismatch{..} | Err(CancellationProtocolMismatch) | Failed(CancellationProtocolMismatch) | Failed(CancellationProtocolMismatch) |
| CleanupIndeterminate or rollback cleanup failure | Err(CancellationCleanupFailed(cleanup error)) | Failed(CancellationCleanupFailed(cleanup error)) | Failed(CancellationCleanupFailed(cleanup error)) |
| CleanupCause::Forced(reason), cleanup proved | Err(forced_tx_error(reason)) | Failed(forced_tx_error(reason)) | Failed(forced_tx_error(reason)) |
| CleanupCause::BackendUnknown(e), cleanup proved | Err(Database(original e)) | Failed(SavepointOpenFailed(original e)) | Release: Failed(SavepointReleaseFailed(original e)); RollbackTo: Failed(SavepointRollbackFailed{prior: prior(stored after), cleanup: original e}) |
| TerminalOutcome::AbortedByBackend(e) after generation retirement | Err(Database(e)) | Failed(SavepointOpenFailed(e)) | `NeverIssuedPending`: Failed(Database(e)) for either close kind. `CommandIssued`: Release maps to SavepointReleaseFailed(e); RollbackTo/ReleaseAfterRollbackTo maps to SavepointRollbackFailed{prior: prior(stored after), cleanup: e}. Retirement-proof timing therefore cannot change a never-issued close's public result. |
| CleanupCause::SetupFailed or BeginUncertain | impossible before a data responder exists | impossible before a frame responder exists | impossible before a frame responder exists |
| TerminalResultMismatch after cancellation recovery entered root rollback | Err(TerminalResultMismatch) | Failed(TerminalResultMismatch) | Failed(TerminalResultMismatch) |

forced_tx_error is the exhaustive mapping Cancel -> TransactionCancelled,
DeadlineExceeded -> TransactionDeadlineExceeded, Detach ->
TransactionDetached, EpochChanged -> TxEpochChanged, and AuthorityDenied ->
AppDeprovisioned or AppIncarnationMismatch. After that drain, every distinct
terminal_waiter receives a clone of the same
`TerminalReply::Completed(Arc<TerminalOutcome>)` and is removed. Every frame
waiter likewise receives the same `Arc<FrameCompletion>`; the reducer never
clones an owned JS payload or moves the retained terminal outcome out of its
record.
Transitions from CancellationCompleted to Settling retain both collections and
drain only after the root terminal result. Any Settled transition with a
nonempty deferred collection that did not execute this table is a reducer panic
in tests, never a silently dropped reply in production.

The helper used by the Quiescing rows is itself closed:

~~~rust
struct Transition {
    next: TxState,
    effects: Vec<ReducerEffect>,
}

enum BackendCommand {
    Prepare,
    Begin,
    AuthorityRead,
    Data(DbPlan),
    Savepoint(SavepointName),
    Release(SavepointName),
    RollbackTo(SavepointName),
    Root(RootDecision),
    Cancel(CleanupGoal),
    Fence { resource_generation: u64 },
}

struct AuditRecord {
    code: &'static str,
}

impl Transition {
    fn state(next: TxState) -> Self {
        Self { next, effects: Vec::new() }
    }
}

enum ReducerEffect {
    Issue {
        token: CommandToken,
        command: BackendCommand,
        // None only for Prepare, the separate platform-role AuthorityRead, and
        // supervisor Fence. Every data-session command names the same actor
        // lease currently recorded as SessionOwner::Command.
        lease: Option<ActorSessionLease>,
    },
    ArmDeadline { kind: DeadlineKind, generation: u64, at: Instant },
    DisarmDeadline { kind: DeadlineKind, generation: u64 },
    Reply(ReplyEffect),
    Publish(Vec<PendingEffect>),
    ReleaseClaim(ClaimGuard),
    RetireGeneration(RetireGenerationEffect),
    FinishOrCancelFenceJobInfallible(DurableFenceJobHandle),
    // The trigger, not merely its permit, carries the typed cutoff/route/job
    // needed by publish_hard_stop.
    PublishHardStop(HardStopTrigger),
    Audit(AuditRecord),
}

struct RetireGenerationEffect {
    key: TxKey,
    token: CommandToken,
    retirement_id: GenerationRetirementId,
    actor_generation: u64,
    // false on the first orderly-close attempt; true after RetirementFence.
    // Both modes remove routing first and can return only a sealed proof.
    force_physical: bool,
    // Idempotently finished/cancelled by every executor attempt. Keeping the
    // same handles in Quarantining makes executor death retry-safe.
    superseded_fence_jobs: Vec<DurableFenceJobHandle>,
}

enum ReplyEffect {
    Start {
        request_id: RequestId,
        reply: Reply<Result<StartReply, TxProtocolError>>,
        value: Result<StartReply, TxProtocolError>,
    },
    Data {
        request_id: RequestId,
        reply: Reply<Result<DbRows, TxOutcomeError>>,
        value: Result<DbRows, TxOutcomeError>,
    },
    FrameOpen {
        request_id: RequestId,
        reply: Reply<Result<FrameOpenReply, TxProtocolError>>,
        value: Result<FrameOpenReply, TxProtocolError>,
    },
    FrameClose {
        request_id: RequestId,
        reply: Reply<Result<SharedFrameCompletion, TxProtocolError>>,
        value: Result<SharedFrameCompletion, TxProtocolError>,
    },
    Terminal {
        request_id: RequestId,
        reply: Reply<Result<TerminalReply, TxProtocolError>>,
        value: Result<TerminalReply, TxProtocolError>,
    },
}

fn reject_request(event: TxEvent, error: TxProtocolError) -> Option<ReplyEffect> {
    match event {
        TxEvent::StartOperation { request_id, reply, .. } =>
            Some(ReplyEffect::Data {
                request_id,
                reply,
                value: Err(TxOutcomeError::Protocol(error)),
            }),
        TxEvent::OpenFrame { request_id, reply, .. } =>
            Some(ReplyEffect::FrameOpen {
                request_id,
                reply,
                value: Err(error),
            }),
        TxEvent::CloseFrame { waiter, .. } =>
            Some(ReplyEffect::FrameClose {
                request_id: waiter.request_id,
                reply: waiter.reply,
                value: Err(error),
            }),
        TxEvent::SettleRoot { request_id, reply, .. } =>
            Some(ReplyEffect::Terminal { request_id, reply, value: Err(error) }),
        TxEvent::Cancel { waiter: Some(waiter), .. } =>
            Some(ReplyEffect::Terminal {
                request_id: waiter.request_id,
                reply: waiter.reply,
                value: Err(error),
            }),
        // Replyless control/completion events return the typed diagnostic to
        // their internal caller; they own no creator endpoint.
        _ => None,
    }
}

fn capture_pending_frame_into_deferred(
    entry: &mut TxEntry,
    pending: SettleIntent,
) {
    let SettleIntent::Frame(close) = pending else {
        // Root responders were inserted in terminal_waiters when the intent
        // was latched; dropping only the owned intent payload is correct.
        return;
    };
    let (kind, after) = match close.kind {
        FrameCloseKind::Release { .. } =>
            (InterruptedFrameKind::Release, None),
        FrameCloseKind::RollbackTo { error, .. } => (
            InterruptedFrameKind::RollbackTo,
            Some(AfterRollbackTo::BodyRejected(error)),
        ),
    };
    entry.deferred_replies.items.push(InterruptedReply::FrameClose {
        kind,
        waiters: close.waiters,
        provenance: DeferredFrameProvenance::NeverIssuedPending,
        after,
        original_error: None,
    });
}

fn continue_quiescing(
    registry: &TxRegistry,
    entry: &mut TxEntry,
    control: &Arc<ReservationControl>,
    intermediate: TxState,
    pending: SettleIntent,
) -> Transition {
    match intermediate {
        InFlight { token, action, stage } => Transition::state(Quiescing {
            token,
            action,
            stage,
            pending,
        }),
        Idle | Poisoned { .. } =>
            drive_pending(registry, entry, control, intermediate, pending),
        Cancelling { .. } | Settling { .. } | HardStopping { .. } => {
            // Root reply is already in terminal_waiters. A frame reply must
            // survive until the terminal outcome.
            capture_pending_frame_into_deferred(entry, pending);
            Transition::state(intermediate)
        }
        Quarantining { .. } => {
            let mut next = intermediate;
            match pending {
                SettleIntent::Frame(close) =>
                    capture_pending_frame_into_deferred(
                        entry, SettleIntent::Frame(close),
                    ),
                SettleIntent::Root { attempt_id, intent } => {
                    // Root responder already lives in terminal_waiters. Preserve
                    // exact Join identity even though retirement superseded its
                    // unused intent payload.
                    drop(intent);
                    let Quarantining { settle_attempt, .. } = &mut next else {
                        unreachable!()
                    };
                    match settle_attempt {
                        None => *settle_attempt = Some(attempt_id),
                        Some(existing) => assert_eq!(*existing, attempt_id),
                    }
                }
            }
            Transition::state(next)
        }
        Settled { outcome } => {
            // An InFlight completion can reach Settled here only because the
            // backend proved the transaction ended. Root waiters already live
            // in terminal_waiters; a pending frame needs its own typed reply.
            let effects = reply_pending_frame_from_terminal(pending, &outcome);
            Transition { next: Settled { outcome }, effects }
        }
        other => protocol_bug(other),
    }
}

fn reply_pending_frame_from_terminal(
    pending: SettleIntent,
    outcome: &SharedTerminalOutcome,
) -> Vec<ReducerEffect> {
    let close = match pending {
        SettleIntent::Root { .. } => return Vec::new(),
        SettleIntent::Frame(close) => close,
    };
    let error = match outcome.as_ref() {
        TerminalOutcome::AbortedByBackend(error) => error.clone(),
        // No other InFlight completion has a direct Settled arm. Treat adding
        // one without extending this match as a reducer construction bug.
        other => panic!("unmapped quiescing terminal outcome: {other:?}"),
    };
    let completion = Arc::new(FrameCompletion::Failed(
        TxOutcomeError::Database(error),
    ));
    close.waiters.into_iter().map(|waiter| ReducerEffect::Reply(ReplyEffect::FrameClose {
        request_id: waiter.request_id,
        reply: waiter.reply,
        value: Ok(completion.clone()),
    })).collect()
}

fn drive_pending(
    registry: &TxRegistry,
    entry: &mut TxEntry,
    control: &Arc<ReservationControl>,
    intermediate: TxState,
    pending: SettleIntent,
) -> Transition {
    if let SettleIntent::Frame(ref intent) = pending {
        if !intent.names(entry.frames.last().unwrap().id) {
            return issue_root_rollback(RollbackCause::Protocol(
                TxProtocolError::SavepointFrameLeaked,
            ));
        }
    }

    match (intermediate, pending) {
        (Idle, Frame(PendingFrameClose {
            kind: FrameCloseKind::Release { .. }, ..
        })) => issue_release(),
        (Idle, Frame(PendingFrameClose {
            kind: FrameCloseKind::RollbackTo { .. }, ..
        })) => issue_rollback_to(),
        (Idle, Root {
            attempt_id, intent @ RootIntent::Commit { .. },
        }) => {
            let intent = commit_if_only_root_else_leaked_rollback(entry, intent);
            drive_first_root_settle_exact(
                registry, entry, control, attempt_id, intent,
            )
        }
        (Idle, Root { attempt_id, intent @ RootIntent::Rollback { .. } }) =>
            drive_first_root_settle_exact(
                registry, entry, control, attempt_id, intent,
            ),

        (Poisoned { .. }, Frame(_))        => issue_rollback_to_and_reject_poison(),
        (Poisoned { .. }, Root {
            attempt_id, intent @ RootIntent::Commit { .. },
        }) => {
            let intent = commit_if_only_root_else_leaked_rollback(entry, intent);
            drive_first_root_settle_exact(
                registry, entry, control, attempt_id, intent,
            )
        }
        (Poisoned { .. }, Root {
            attempt_id, intent @ RootIntent::Rollback { .. },
        }) => drive_first_root_settle_exact(
            registry, entry, control, attempt_id, intent,
        ),

        (other, _)                          => protocol_bug(other),
    }
}

fn drive_first_root_settle_exact(
    registry: &TxRegistry,
    entry: &mut TxEntry,
    control: &Arc<ReservationControl>,
    attempt_id: RootSettleAttemptId,
    intent: RootIntent,
) -> Transition {
    // Build the descriptive delivery before the reducer borrows entry
    // mutably.  Keeping construction here prevents call sites from taking an
    // immutable entry borrow in the same argument list as the mutable one.
    let delivery = make_explicit_root_delivery(entry, control);
    match reduce_first_root_settle(
        registry, entry, control, attempt_id, intent, delivery,
    ) {
        Ok(transition) => transition,
        Err(error) => begin_generation_retirement(
            entry,
            TerminalOutcome::AbortedByBackend(
                tx_protocol_error_as_db_error(error),
            ),
        ),
    }
}
~~~

## 7. Exhaustive illegal transition matrix

The legal cells below still require the guards stated in section 5. Failing a
guard yields the more specific identity/token/frame error before this matrix.
Abbreviations name the typed wire code:

* DUP = duplicate_transaction
* SAD = stale_admission_completion
* NRD = transaction_not_ready
* NOP = transaction_not_open
* BUS = transaction_connection_busy
* EXP = transaction_scope_expired
* POI = transaction_poisoned
* STG = transaction_settling
* CAN = transaction_cancelling
* CON = transaction_settle_conflict
* NSS = transaction_not_settled
* REF = terminal_record_referenced
* SPC = savepoint_not_current or savepoint_root_cannot_close, selected by target
* STC = stale_transaction_completion
* SDL = stale_transaction_deadline

Request/control events:

`CancelOrder::PreAdmission` is legal only in the WaitingAdmission cell. In
every other state it returns STC even where the unqualified Cancel cell says
legal, observer, idempotent, or replay; those labels apply only to actor-derived
orders.

| State | Create | Admission result | StartOperation | OpenFrame | CloseFrame | SettleRoot | Cancel | DetachRequested | Exec deadline | Cancel deadline | Terminal deadline | Retirement deadline | LifecycleObserved | Forget |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| WaitingAdmission | DUP | legal | NRD | NRD | NRD | NOP | PreAdmission legal; other orders STC | legal for exact authority | SDL | SDL | SDL | SDL | legal | NSS |
| Preparing | DUP | SAD | NRD | NRD | NRD | NOP | order submatrix | legal for exact authority | current legal; stale SDL | SDL | SDL | SDL | legal | NSS |
| Starting | DUP | SAD | NRD | NRD | NRD | NOP | order submatrix | legal for exact authority | current legal; stale SDL | SDL | SDL | SDL | legal | NSS |
| Idle | DUP | SAD | legal only for current open top; otherwise SPC | legal under depth/top guards | legal for non-root top | legal | order submatrix | legal for exact authority | current legal; stale SDL | SDL | SDL | SDL | legal | NSS |
| InFlight(Data) | DUP | SAD | BUS | BUS | legal latch for current top | legal latch | order submatrix | legal forcing event | current legal; stale SDL | SDL | SDL | SDL | legal | NSS |
| InFlight(OpenSavepoint) | DUP | SAD | BUS | BUS | BUS | legal latch | order submatrix | legal forcing event | current legal; stale SDL | SDL | SDL | SDL | legal | NSS |
| InFlight(closing frame) | DUP | SAD | BUS | BUS | identical joins; conflicting CON | legal latch | order submatrix | legal forcing event | current legal; stale SDL | SDL | SDL | SDL | legal | NSS |
| Quiescing | DUP | SAD | STG | STG | identical pending joins; otherwise CON | identical pending joins; otherwise CON | order submatrix | legal escalation | current legal; stale SDL | SDL | SDL | SDL | legal | NSS |
| Poisoned | DUP | SAD | POI | POI | current recovery child legal; otherwise SPC/POI | legal | order submatrix | legal for exact authority | current legal; stale SDL | SDL | SDL | SDL | legal | NSS |
| Cancelling | DUP | SAD | CAN | CAN | CAN | CAN | order submatrix | legal/idempotent | SDL | current legal for matching phase/generation; stale SDL | SDL | SDL | legal/audit | NSS |
| Settling | DUP | SAD | STG | STG | STG | identical joins; otherwise CON | order submatrix | legal observer | SDL | SDL | current legal once; stale SDL | SDL | legal audit | NSS |
| HardStopping | DUP | SAD | STG | STG | STG | identical joins; otherwise CON | order submatrix | legal observer | SDL | SDL | SDL | SDL | legal audit | NSS |
| Quarantining | DUP | SAD | STG | STG | STG | exact retained join; otherwise CON | legal join | legal audit | SDL | SDL | SDL | current legal once; stale SDL | legal audit | NSS |
| Settled | DUP | SAD | EXP | EXP | EXP | legal replay | legal replay | legal audit | SDL | SDL | SDL | SDL | legal audit | legal only unreferenced; otherwise REF |

`ReleaseTerminalRef` is internal and therefore omitted from the creator-event
columns. It is legal only in Settled with `terminal_refs > 0`, where it decrements
exactly once and changes nothing else. In Settled with zero, or in every other
state, it is a pure StaleTransactionCompletion producer fault. Forget's
"unreferenced" guard is exactly `terminal_refs == 0` plus the terminal backend
lease-refcount guard in the legal row; no prose-only external-waiter census is
used.

The `Cancel` cells expand to this closed order submatrix. `InlineApplyForce` is
the typed, non-event result of reducing an already-keyed DeadlineFired; when the
gate said Joined it carries the immutable first cause and is applied immediately
because the earlier publisher's Cancel event may be later in mailbox order.
`InlineDeferredAfterNonterminal` instead means a nonterminal completion is
already keyed and must reduce first.

| Source | PreAdmission | ForceWon / AfterNonterminalCompletion event | TerminalCompletionWon | TerminalFenceWon | Joined event | InlineApplyForce | InlineDeferredAfterNonterminal |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Preparing / Starting / Idle / Poisoned | STC | enter Cancelling | STC | observe matching fence proof | STC | enter Cancelling with first cause | legal only Preparing/Starting; await keyed completion then Cancel |
| InFlight / Quiescing | STC | enter Cancelling (After event is behind its ordinary completion) | legal only with retained completion-owned candidate; otherwise STC | observe matching fence proof | STC | enter Cancelling with first cause | await keyed completion then keyed Cancel |
| Cancelling | STC | Equal cause joins. A later different external cause is audit-only and joins the immutable first cause; it cannot replace the terminal attempt or enter a second cleanup. Only a supposedly same publication carrying a cause that disagrees with the authenticated control latch is a producer fault and enters `CancellationProtocolMismatch{expected:latched,observed:event}`/Quarantining. | STC | legal only as matching physical cancellation fallback | join existing cleanup and audit any later differing external cause | impossible/STC | impossible/STC |
| Settling | STC | settle-before-owner gap enters Cancelling | join retained root completion | join retained root fence fallback | join earlier force/terminal owner | apply the corresponding authenticated force row | impossible/STC |
| HardStopping | STC | pending-cancel rows in legal table | join retained result | join retained fence | join existing path | apply corresponding hard-stop row | impossible/STC |
| Quarantining | STC | join fixed pending outcome | join fixed pending outcome | join fixed pending outcome | join fixed pending outcome | impossible/STC | impossible/STC |
| Settled | STC | replay | replay | replay | replay | impossible/STC | impossible/STC |

Asynchronous completion events:

| State | PreparationCompleted | BeginCompleted | AuthorityCompleted | DataCompleted | DataAbortCompleted | FrameCommandCompleted | TerminalCompleted | TerminalHardStopPreempted | CancellationCompleted | CancellationHardStopCompleted | TerminalHardStopCompleted | BackendActorUnavailable | GenerationRetired |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| WaitingAdmission | STC | STC | STC | STC | STC | STC | STC | STC | STC | STC | STC | STC | STC |
| Preparing | matching prepare legal | STC | STC | STC | STC | STC | STC | STC | STC | STC | STC | matching generation+proof legal | STC |
| Starting | STC | matching begin legal | STC | STC | STC | STC | STC | STC | STC | STC | STC | matching generation+proof legal | STC |
| Idle | STC | STC | STC | STC | STC | STC | STC | STC | STC | STC | STC | matching generation+proof legal | STC |
| InFlight | STC | STC | matching Authority-stage legal | matching DataSql-stage legal | matching DataSql-stage legal | matching FrameControl-stage legal | STC | STC | STC | STC | STC | matching generation+proof legal | STC |
| Quiescing | STC | STC | matching Authority-stage legal | matching DataSql-stage legal | matching DataSql-stage legal | matching FrameControl-stage legal | STC | STC | STC | STC | STC | matching generation+proof legal | STC |
| Poisoned | STC | STC | STC | STC | STC | STC | STC | STC | STC | STC | STC | matching generation+proof legal | STC |
| Cancelling | STC | STC | STC | STC | STC | STC | STC | STC | matching Awaiting/HardStopping legal | matching Awaiting/HardStopping fence result legal | STC | STC | STC |
| Settling | STC | STC | STC | STC | STC | STC | matching terminal legal before cutoff | STC | STC | STC | matching preinstalled job/token/generation+proof legal | STC | STC |
| HardStopping | STC | STC | STC | STC | STC | STC | matching retained root result legal | matching permit/generation legal | STC | STC | matching fence token/generation+proof legal | STC | STC |
| Quarantining | STC | STC | STC | STC | STC | STC | STC | STC | STC | STC | STC | matching retirement proof legal | matching token/id/generation/proof legal |
| Settled | STC | STC | STC | STC | STC | STC | STC | STC | STC | STC | STC | STC | STC |

Each preparation, BEGIN, authority, data, and frame command owns a private
`CommandCompletionGate` and a keyed mailbox delivery id:

~~~rust
enum CommandGateState {
    Open,
    // Actor has won OWNER_COMPLETE for a terminal result arising from this
    // command and promises keyed delivery through cutoff. A force cannot
    // suppress that future completion.
    CompletionPromised { delivery_id: TerminalDeliveryId },
    // Ordinary nonterminal completion was keyed before a racing force. A
    // terminal candidate never enters this variant: CompletionPromised remains
    // durable until the retained terminal outcome retires the control.
    NonterminalCompletionQueued,
    ForceQueued { cause: CleanupCause },
    // The nonterminal result is already keyed ahead of the force. Cancellation
    // must still own the now-open transaction after SC-1 reduces that result.
    ForceAfterNonterminalCompletionQueued { cause: CleanupCause },
}

struct NonterminalCompletionPermit {
    control: Arc<ReservationControl>,
    command_sequence: u64,
    gate: Arc<CommandCompletionGate>,
}

enum NonterminalGateRetirement {
    Cleared,
    ForceAfter { cause: CleanupCause },
}

fn consume_nonterminal_completion_exact(
    permit: &Arc<NonterminalCompletionPermit>,
) -> Result<NonterminalGateRetirement, TxProtocolError> {
    let mut slot = permit.control.active_command_gate.lock();
    let Some(current) = slot.as_ref() else {
        return Err(TxProtocolError::StaleTransactionCompletion);
    };
    if current.command_sequence != permit.command_sequence
        || !Arc::ptr_eq(&current.gate, &permit.gate)
    {
        return Err(TxProtocolError::StaleTransactionCompletion);
    }
    let state = current.gate.state.lock();
    match &*state {
        CommandGateState::NonterminalCompletionQueued => {
            drop(state);
            *slot = None;
            Ok(NonterminalGateRetirement::Cleared)
        }
        CommandGateState::ForceAfterNonterminalCompletionQueued { cause } => {
            // Retain the exact gate until the already-keyed force is reduced and
            // OWNER_CANCEL claims it. CANCEL_INTENT prevents a next command.
            Ok(NonterminalGateRetirement::ForceAfter {
                cause: cause.clone(),
            })
        }
        _ => Err(TxProtocolError::StaleTransactionCompletion),
    }
}

fn restore_registry_after_suppressed_issue_exact(
    entry: &mut TxEntry,
) -> Result<(), TxProtocolError> {
    entry.session = match std::mem::replace(
        &mut entry.session, SessionOwner::None,
    ) {
        SessionOwner::Command { lease, .. } => SessionOwner::Registry(lease),
        SessionOwner::Registry(lease) => SessionOwner::Registry(lease),
        SessionOwner::Acquiring(_) | SessionOwner::None =>
            return Err(TxProtocolError::StaleTransactionCompletion),
        SessionOwner::Quarantined => SessionOwner::Quarantined,
    };
    Ok(())
}

fn defer_pending_close_exact(
    entry: &mut TxEntry,
    close: PendingFrameClose,
    provenance: DeferredFrameProvenance,
    original_error: Option<DbError>,
) {
    let (kind, after) = match close.kind {
        FrameCloseKind::Release { .. } =>
            (InterruptedFrameKind::Release, None),
        FrameCloseKind::RollbackTo { error, .. } => (
            InterruptedFrameKind::RollbackTo,
            Some(AfterRollbackTo::BodyRejected(error)),
        ),
    };
    entry.deferred_replies.items.push(InterruptedReply::FrameClose {
        kind,
        waiters: close.waiters,
        provenance,
        after,
        original_error,
    });
}

fn defer_unissued_action_exact(
    entry: &mut TxEntry,
    action: ActiveAction,
    effects: &mut Vec<ReducerEffect>,
) {
    match action {
        ActiveAction::Data {
            request_id, reply, hard_stop_trigger, ..
        } => {
            entry.deferred_replies.items.push(InterruptedReply::Data {
                request_id,
                reply,
                original_error: None,
            });
            effects.push(
                ReducerEffect::FinishOrCancelFenceJobInfallible(
                    hard_stop_trigger.job(),
                ),
            );
        }
        ActiveAction::OpenSavepoint { request_id, reply, .. } =>
            entry.deferred_replies.items.push(InterruptedReply::FrameOpen {
                waiter: FrameOpenWaiter { request_id, reply },
                original_error: None,
            }),
        ActiveAction::ReleaseSavepoint { close } =>
            defer_pending_close_exact(
                entry, close, DeferredFrameProvenance::CommandIssued, None,
            ),
        ActiveAction::RollbackToSavepoint {
            after, waiters, ..
        }
        | ActiveAction::ReleaseAfterRollbackTo {
            after, waiters, ..
        } => entry.deferred_replies.items.push(
            InterruptedReply::FrameClose {
                kind: InterruptedFrameKind::RollbackTo,
                waiters,
                provenance: DeferredFrameProvenance::CommandIssued,
                after: Some(after),
                original_error: None,
            },
        ),
    }
}

fn capture_force_after_state_exact(
    entry: &mut TxEntry,
    kind: NonterminalCompletionKind,
    next: TxState,
    effects: &mut Vec<ReducerEffect>,
) -> Result<CleanupGoal, TxProtocolError> {
    match (kind, next) {
        (NonterminalCompletionKind::Prepare, TxState::Starting { .. }) => {
            restore_registry_after_suppressed_issue_exact(entry)?;
            Ok(CleanupGoal::NoTransaction)
        }
        (NonterminalCompletionKind::Begin, TxState::Idle) =>
            Ok(CleanupGoal::OpenTransaction),
        (NonterminalCompletionKind::Authority,
         TxState::InFlight { action, .. }) => {
            restore_registry_after_suppressed_issue_exact(entry)?;
            defer_unissued_action_exact(entry, action, effects);
            Ok(CleanupGoal::OpenTransaction)
        }
        (NonterminalCompletionKind::Authority, TxState::Idle) =>
            Ok(CleanupGoal::OpenTransaction),
        (NonterminalCompletionKind::Authority,
         TxState::Quiescing { action, pending, .. }) => {
            restore_registry_after_suppressed_issue_exact(entry)?;
            defer_unissued_action_exact(entry, action, effects);
            capture_pending_frame_into_deferred(entry, pending);
            Ok(CleanupGoal::OpenTransaction)
        }
        (NonterminalCompletionKind::Data | NonterminalCompletionKind::Frame,
         TxState::Idle | TxState::Poisoned { .. }) =>
            Ok(CleanupGoal::OpenTransaction),
        (NonterminalCompletionKind::Data | NonterminalCompletionKind::Frame,
         TxState::InFlight { action, .. }) => {
            restore_registry_after_suppressed_issue_exact(entry)?;
            defer_unissued_action_exact(entry, action, effects);
            Ok(CleanupGoal::OpenTransaction)
        }
        (NonterminalCompletionKind::Data | NonterminalCompletionKind::Frame,
         TxState::Quiescing { action, pending, .. }) => {
            restore_registry_after_suppressed_issue_exact(entry)?;
            defer_unissued_action_exact(entry, action, effects);
            capture_pending_frame_into_deferred(entry, pending);
            Ok(CleanupGoal::OpenTransaction)
        }
        _ => Err(TxProtocolError::StaleTransactionCompletion),
    }
}

// Called under the TxEntry reducer lock after pure result/proof validation.
// `ordinary` has not yet been installed and none of its effects has run.
fn reduce_validated_nonterminal_completion(
    entry: &mut TxEntry,
    permit: &Arc<NonterminalCompletionPermit>,
    kind: NonterminalCompletionKind,
    mut ordinary: Transition,
) -> Result<Transition, TxProtocolError> {
    match consume_nonterminal_completion_exact(permit)? {
        NonterminalGateRetirement::Cleared => Ok(ordinary),
        NonterminalGateRetirement::ForceAfter { cause } => {
            match &ordinary.next {
                // Completion itself already established terminal reality or
                // cleanup.  It is stronger than the later force.
                TxState::Cancelling { .. }
                | TxState::Quarantining { .. }
                | TxState::Settled { .. } => {
                    ordinary.effects.push(ReducerEffect::Audit(AuditRecord {
                        code: "force_after_terminal_completion",
                    }));
                    return Ok(ordinary);
                }
                _ => {}
            }
            let Transition { next, mut effects } = ordinary;
            // Preserve reply/frame/effect mutations earned by the completed
            // statement, but delete every unissued follow-on SQL. Ownership is
            // then restored/captured from `next` by value; no borrowed responder
            // can be dropped and no ghost Command token survives.
            effects.retain(|effect| {
                !matches!(effect, ReducerEffect::Issue { .. })
            });
            let cleanup = capture_force_after_state_exact(
                entry, kind, next, &mut effects,
            )?;
            let mut forced = enter_cancelling_after_completion(
                entry, cause, cleanup,
            )?;
            // `enter_cancelling_after_completion` atomically replaces the
            // Execution deadline and appends exactly one control-plane
            // BackendCommand::Cancel. It appends no ordinary SQL command.
            effects.append(&mut forced.effects);
            Ok(Transition { next: forced.next, effects })
        }
    }
}

enum NonterminalCompletionKind {
    Prepare,
    Begin,
    Authority,
    Data,
    Frame,
}

struct CommandCompletionGate {
    state: Mutex<CommandGateState>,
}

enum ForcePublish {
    ForceWon,       // ordinary completion is suppressed
    CompletionWon,  // force was queued immediately after ordinary completion
    Joined,         // an earlier force owns cause; this caller only joins
}
~~~

The ordinary nonterminal publisher locks the gate. From Open it calls mailbox
`enqueue_once(command_delivery_id, completion)` and sets
NonterminalCompletionQueued before
unlocking. From ForceQueued it hands the raw backend result to cancellation
cleanup as diagnostics and emits no ordinary completion. Every forcing
publisher—explicit/caller-drop Cancel, current Execution deadline, exact
DetachRequested, and a Lifecycle observation already classified ReResolve or
Deny—locks this same gate. From Open it latches the one CleanupCause, keyed-
enqueues the force, sets ForceQueued, and later emits exactly one
CancellationCompleted through the cancel cutoff. From
NonterminalCompletionQueued it
keyed-enqueues the force while still holding the gate, after the already-inserted
completion, and returns CompletionWon; the reducer processes the ordinary result
first, then starts a new rollback/cancel action from the resulting state. From
ForceQueued it joins the first cause and emits no second force.

For a terminal candidate produced by a command—autocommit completion,
BUSY_SNAPSHOT data abort, or a result proving an explicit transaction ended—the
actor locks `CommandCompletionGate` and then `terminal_owner_gate`. If the
command gate is ForceQueued it does not attempt Complete; cancellation owns. If
it is Open and OWNER_OPEN has no intent, the actor stores attempt/delivery,
CASes OWNER_COMPLETE, and changes the command gate to CompletionPromised before
releasing either lock. The retained cutoff Result plus delivery bit is the
completion record; the command gate deliberately remains CompletionPromised
until terminal-control retirement. A forcing publisher that sees
CompletionPromised does
not set a new force, does not suppress the promised result, and treats the
request as late Cancel/audit waiting for AlreadyCompleted. The reverse lock
order is forbidden. Thus OWNER_COMPLETE can never coexist with a still-Open
command gate that a deadline/drop could convert to ForceQueued.

A Current lifecycle observation is not forcing and does not claim the gate; its
publisher may enqueue it as audit/ceiling input. The reducer re-runs the
classifier, so a dishonest publisher cannot label a Deny as Current. This gate
contract applies in Preparing, Starting, InFlight, and Quiescing, closing all
deadline/lifecycle/detach races rather than only caller Cancel. Cross-sender
scheduling therefore cannot place a force ahead of a completion that already
won. `NoTransaction` is accepted only for CleanupGoal::NoTransaction or
AbortIfOpened with a no-BEGIN proof; it can never stand for an already-open
transaction. Open transactions are rolled back inside the backend actor and
produce RolledBack or Indeterminate.

Terminal SQL has one additional shared two-way TerminalCutoffGate. Its producer
first prepares and retains the immutable projection, then calls
`publish_prepared_result`; the gate stores retention before its infallible keyed
mailbox insertion, and both retry paths use its TerminalDeliveryId with
enqueue-once. The TerminalHardStop reducer only
retains a private permit and enters HardStopping; it does not touch the cutoff.
After the reducer lock is released, the permit-authenticated publisher holds
terminal_owner_gate and performs the sole arbitration. A real Result suppresses
the hard stop and is deliverable even if its producer dies in the enqueue gap;
an earlier Open|CANCEL_INTENT emits keyed TerminalHardStopPreempted while leaving
the cutoff Open; otherwise Fence suppresses the later ordinary completion and
makes the outcome indeterminate after physical fencing.
This is why a TerminalCompleted queued before the deadline cannot be discarded
merely because the reducer happened to receive the timer first.

Input/schema validation runs before StartOperation is constructed, so a
validation failure is not a TxEvent. If the caller nevertheless submits a
StartOperation envelope, the request matrix governs it: Idle may accept it,
InFlight returns BUS, Poisoned returns POI, Quiescing/Settling return STG,
Cancelling returns CAN, and Settled returns EXP. No validation path invents SQL
or changes transaction health.

## 8. Independent deadline

AdmissionGranted arms a separate compio timer at the same atomic transition that
creates the ClaimGuard. Queue time in WaitingAdmission does not consume the
transaction execution budget. The timer task owns only
(TxKey, DeadlineKind::Execution, generation, event_sender); it never owns the DB
session and does not depend on callback polling or on a settle future. This
corrects the circular settle-path deadline called out by SC-1
(docs/proposals/2026-08-26-sc1-transaction-protocol.md:140-149).

The root absolute deadline is inherited by every child frame and cannot be
extended. A matching DeadlineFired calls claim_fire and atomically changes the
one shared state from Armed to Fired, so duplicates are stale. Its current event
acts as follows:

* Preparing and Starting: cancel acquisition/BEGIN and wait for proof that no
  transaction remains.
* Idle and Poisoned: publish the immutable cancellation cause and make the actor
  prove whole-transaction cleanup through CancellationCompleted.
* InFlight and Quiescing: signal the active command, override every
  not-yet-issued release/commit with forced root rollback, await cancellation,
  then roll back.
* WaitingAdmission has no execution timer; Settling and HardStopping have
  already disarmed it; Settled treats every timer generation as stale.

All six kinds—Execution, CancellationSql, CancellationHardStop, TerminalSql,
TerminalHardStop, and RetirementFence—use one `Arc<ExplicitDeadlineSlots>` and
one enum state. The Arc
is minted at Create and pointer-identically installed in ReservationControl by
reserve. `replace_current(expected_kind,next_kind,...)` accepts only Armed or
Fired for expected_kind and installs/schedules the successor under the same
mutex. `publish_settled` changes any Armed/Fired state to Disarmed. A timer task
carries kind+generation, so equality with a generation formerly used by
another kind never suffices. There is no second actor-side explicit generation.

Entering Cancelling replaces Execution with a fresh CancellationSql generation
in that state machine even when active work is preparation or BEGIN. Its first
expiry changes Armed to Fired, publishes the
out-of-band interrupt for the exact reservation/command and arms
CancellationHardStop by replacing Fired with a fresh Armed generation for the grace
interval. If a retained CancelAck wins the
cancel cutoff during grace, keyed enqueue-once delivers CancellationCompleted.
If the fence wins, the exact backend generation is removed from routing before
CancellationHardStopCompleted(Indeterminate) is sent. Thus a hung cancellation
ROLLBACK cannot strand the claim, and no cancellation timeout is misrouted as a
root TerminalCompleted event.

Terminal SQL is bounded in two stages rather than becoming immortal after the
execution deadline. Entry to Settling atomically replaces the execution timer
with DeadlineKind::TerminalSql at now+terminal_sql_timeout. That required
runtime limit is positive and finite; the reducer never extends it. A current
TerminalSql event takes the interrupt gate for the exact terminal command,
sets watchdog_fired, interrupts once without changing RootIntent or terminal
completion ownership, and arms DeadlineKind::TerminalHardStop at
now+terminal_interrupt_grace. SQLITE_OK/COMMIT during that grace still means
committed; a confirmed rollback still means rolled back; a failed COMMIT
without proof becomes CommitIndeterminate.

If no result wins the cutoff gate during grace, TerminalHardStop wins it and
the reducer enters HardStopping. The backend fence removes the exact resource
generation from every router before acknowledging. It MUST prevent that
generation from accepting any command after the already-issued terminal SQL;
it need not claim whether that SQL committed. PostgreSQL can fence a connection
generation by closing its transport. The SQLite actor fences the lane/actor
generation and lets the quarantined synchronous call finish only into a
discarded completion; it is never reused. The acknowledgement therefore makes
admission safe to release but deliberately yields CommitIndeterminate or
RollbackFailed. Merely timing out a future or abandoning a still-routable actor
is not a fence.

ClaimGuard is disarmed only after confirmed root end or after the session has
been quarantined so it cannot execute another app command. Thus cancellation
between admission and BEGIN cannot leak the claim, and an absent parked client
can never serve as evidence of settlement.

## 9. Terminal outcome table

| TerminalOutcome | Creator result | Effects | Session | Admission/timer |
| --- | --- | --- | --- | --- |
| Committed(value) | resolve value | publish root exactly once | return or retire clean | release/disarm |
| RolledBack(cause) | reject original body/forced/protocol cause | discard all | prove rollback or quarantine | release/disarm |
| AdmissionFailed(e) | reject e | none | none | no claim/timer |
| BeginFailed(e) | reject begin_failed or preserved typed provisioning error | none | prove no transaction | release/disarm |
| Cancelled | reject transaction_cancelled | discard all | cancellation acknowledgement proves cleanup | release/disarm |
| DeadlineExceeded | reject transaction_deadline_exceeded | discard all | cancellation/rollback acknowledgement | release/disarm |
| Detached | reject transaction_detached | discard all | detach acknowledgement proves cleanup | release/disarm |
| IncarnationDenied | reject app_incarnation_mismatch | discard all | no further data SQL | release/disarm |
| AppDeprovisioned | reject app_deprovisioned | discard all | no further data SQL | release/disarm; tombstone remains |
| EpochChanged | reject retryable tx_epoch_changed | discard all | rollback before retry | release/disarm |
| CommitRolledBack | reject commit_rolled_back | discard all | clean ended transaction | release/disarm |
| CommitFailed | reject commit_failed | discard all | clean or quarantine | release/disarm |
| CommitIndeterminate | reject commit_failed_indeterminate | discard all; never speculate | quarantine | release after quarantine |
| RollbackFailed | reject rollback_failed with original cause | discard all | quarantine/replace | release after quarantine |
| AbortedByBackend(e) | reject mapped e | discard all | actor/driver proves abort | release/disarm |
| CleanupIndeterminate(e) | reject cancellation_cleanup_failed | discard all | quarantine/replace | release after quarantine |
| CleanupProtocolMismatch | reject cancellation_protocol_mismatch | discard all | quarantine | release after quarantine |
| CancellationProtocolMismatch{expected,observed} | reject cancellation_protocol_mismatch with both authenticated causes in diagnostics | discard all | quarantine | release after quarantine |
| TerminalResultMismatch | reject terminal_result_mismatch | discard all; never publish | quarantine | release after quarantine |

The command tag is durability evidence, not decoration. The existing driver
explicitly distinguishes a COMMIT answered as ROLLBACK
(libs/compio-postgres/src/transaction.rs:54-59,165-189), and the native raw
terminal path now performs the same check
(crates/zeroship-data-v8/src/transaction/mod.rs:123-159).

## 10. Property-test invariants

A pure reducer driven by a model backend MUST assert each safety clause after
every generated prefix, including prefixes ending in every illegal event.
Clauses explicitly marked **liveness** are instead asserted only after a fair
drain: every enabled mailbox delivery, deadline, durable fence job, and backend
completion is eventually scheduled. This distinction prevents a healthy live
claim or an armed timer at the end of a finite prefix from failing the suite.

1. Qualified identity: an event for authority A never mutates, interrupts,
   settles, or executes SQL for B. An epoch mismatch emits re-resolution;
   domain/incarnation mismatch and Deprovisioned emit terminal denial.
2. Admission cardinality: at most one nonterminal entry owns an AdmissionKey.
   PostgreSQL keys are runtime instance plus app_id plus incarnation; SQLite
   keys are thread resource plus app_id plus incarnation. Neither includes
   authority domain.
3. Claim balance: for every key and every prefix,
   `grants == live_claims + releases`, every individual claim has at most one
   release, and release occurs only after no routable transaction/reservation
   remains. After a fair drain, `live_claims == 0` and every grant has exactly
   one release. A hard-stopped
   generation may finish only its already-issued terminal statement into a
   discarded result and can never accept another command.
4. Session conservation: after confirmed BEGIN and before terminal cleanup,
   session ownership is exactly one of Registry, the matching command token, or
   Quarantined. It is never silently absent.
5. Single command: at most one active backend token exists per transaction.
   Duplicate/stale completions cannot alter state or effects.
6. Operation serialization: a second operation in InFlight returns
   transaction_connection_busy and is never silently deferred or autocommitted.
7. Authority separation: every data-SQL trace is preceded by a successful
   platform-role lifecycle read for the same AppAuthority. No authority read is
   on the tenant data session.
8. Ceiling monotonicity: effective_ceiling is never broader than begin_ceiling
   or any previously accepted effective value. A mid-transaction raise changes
   nothing; a lower value tightens the next authorization.
9. Frame stack: root is index zero; parent links form one chain; only top acts;
   simultaneous child depth is at most eight; frame sequence/name never repeats.
10. Complete child close: every successfully opened child that the child-close
    subprotocol **successfully closes** has exactly one confirmed-success
    RELEASE. A direct Release trace either succeeds immediately, or has at most
    one failed initial RELEASE followed by recovery ROLLBACK TO and, only when
    that rollback-to succeeds, one RELEASE. A failed ROLLBACK TO ends that trace
    without RELEASE; a failed cleanup RELEASE also ends it without inventing a
    success. A CloseFrame(RollbackTo) trace orders ROLLBACK TO before any
    successful RELEASE. Children ended by a root ROLLBACK are excluded.
11. Effect locality: success appends only to current frame. Release moves the
    exact child sequence into its parent. Confirmed rollback-to discards exactly
    the child sequence and no parent effect.
12. Commit-only publication: no trace publishes unless RootIntent is Commit and
    RootFinishResult is Committed. Committed returned for a Rollback intent is a
    terminal-result mismatch and publishes nothing. A confirmed commit publishes
    every retained effect once, in transaction order.
13. Poison rule: no creator data/open/release command starts from Poisoned.
    Successful rollback-to/release of the recovery child returns the parent to
    Idle. Absent forced cancellation, actor retirement, or backend-unavailable
    fencing, only creator root settlement can end Poisoned.
14. Poisoned commit: COMMIT answered ROLLBACK never yields a resolved creator
    promise or a published effect. The reachable regression case is covered in
    the current live suite
    (crates/zeroship-data-v8/tests/native_transaction.rs:977-1048).
15. Quiescing: settlement while another command owns logical execution issues
    zero frame/terminal SQL until that command returns, then starts at most one
    logical settlement attempt and never more than one SQL statement
    concurrently. A successfully closed RollbackTo frame settlement issues its
    ordered ROLLBACK TO then RELEASE pair. If ROLLBACK TO fails, the legal error
    row stops without RELEASE and retains/poisons/quarantines as classified.
16. Forced precedence: cancellation/deadline/revocation before terminal
    ownership overrides a pending release/commit with root rollback. After
    terminal ownership, caller cancellation/lifecycle notification does not
    interrupt and backend reality wins; only the separately armed terminal-SQL
    watchdog may interrupt, without changing ownership.
17. **Liveness.** Deadline independence: a callback that never settles still produces a
    current Execution deadline trace followed by interrupt/rollback cleanup and
    claim release. If no authenticated cancellation preempts terminal ownership,
    a terminal command that never answers produces one current
    TerminalSql interrupt, one current TerminalHardStop after grace, one
    generation-fence acknowledgement, an indeterminate immutable result, and
    claim release. Deleting either timer arm makes the property fail.
18. Settled immutability: after the first Settled state, arbitrary events cannot
    issue SQL, publish, reacquire admission, or change the outcome.
19. Illegal purity: every illegal matrix cell returns its exact typed error and
    leaves state, session ownership, frames, effects, timer generation, claim,
    and backend-command count unchanged.
20. PITR fence: presenting the same bare app_id and incarnation under a changed
    (system_identifier,timeline_id) cannot execute data SQL or target the old
    transaction.
21. Completion ordering: if force wins a command gate, that command emits no
    ordinary completion and its cleanup emits exactly one keyed terminal form:
    CancellationCompleted if real cleanup beats the fence, XOR matching
    CancellationHardStopCompleted if the physical fence wins.
    If ordinary completion wins, it is enqueued before the forcing event and the
    latter starts cleanup from the post-completion state. No generated trace can
    observe both an ordinary completion and either cancellation terminal form
    for the same command token, both cancellation terminal forms, or
    force-before-completion after NonterminalCompletionQueued.
22. Root rollback completeness: RootIntent::Rollback is legal with any number of
    child frames, issues one root ROLLBACK, publishes nothing, and leaves no frame
    after confirmation. Root commit with any child always becomes forced rollback.
23. Tombstone permanence: a Deprovisioned observation permits no data SQL and
    remains terminal even after every transaction Forget/actor ForgetTerminal.
24. Admission correlation: only the AdmissionGranted/AdmissionFailed carrying
    the token stored in WaitingAdmission can install a ClaimGuard or settle the
    entry; every stale token is pure and returns stale_admission_completion.
25. Request conservation: every accepted request_id appears exactly once in the
    start waiter, initial/joined terminal-waiter vector, active action,
    deferred-reply bundle, or pending-close vector until it receives exactly
    one reply. Its id remains in retained_request_ids through Settled and is
    removed only by Forget. Distinct ids may join identical
    close/root/cancel work without duplicating SQL; an already-retained id is
    rejected purely with duplicate_request. StartOperation/OpenFrame are not
    replay keys: when the state is busy their envelopes receive the matrix error.
    A CallerDrop `Cancel { waiter: None }` is a replyless control notice and has
    no request_id, so it is deliberately outside this property.
26. Cleanup closure: generated CleanupGoal x CancelAck pairs visit all twelve
    matrix cells. Every mismatch quarantines before claim release and none is
    misreported as a stale completion.
27. **Safety plus liveness.** Create owns distinct start and terminal request ids.
    Exactly one Ready or Failed reaches start; exactly one Completed reaches the
    initial terminal waiter. A callback that never emits SettleRoot is ended by
    Execution deadline cleanup. Multiple frame/root waiters receive Arc-shared
    results while Settled retains that same outcome.
28. Terminal delivery dedupe: inject actor death after keyed mailbox insertion
    but before `delivery_enqueued=true`. Recovery retries the same
    TerminalDeliveryId; the reducer observes and applies one completion, not two.
29. **Liveness.** Cancellation timeout: a stuck OWNER_CANCEL cleanup reaches one interrupt,
    then one generation fence, then CancellationHardStopCompleted(Indeterminate);
    TerminalCompleted and root TerminalHardStopCompleted remain illegal in
    Cancelling.
30. Deadline-machine exclusivity: every reachable shared deadline state is
    exactly Disarmed, Armed{kind,generation,at}, or Fired{kind,generation}; the
    pointer in TxEntry equals the pointer in ReservationControl. Its only
    mutators are arm_initial (Disarmed -> Armed), claim_fire on the exact Armed
    pair, replace_current on the exact expected kind,
    replace_with_retirement on exactly one current non-retirement kind, and
    disarm_explicit_deadline on the exact terminal generation. Stale
    kind/generation produces no SQL, reply, claim change,
    actor interrupt, or state mutation.
31. Backend-generation authentication: successful reserve stores its handle and
    actor generation before Prepare is enqueueable. BackendActorUnavailable for
    any other generation is pure. A matching event is accepted only after the
    supervisor's two-lane closure/fence proof; it clears the stored generation
    before Settled and cannot target another authority domain, incarnation, or
    transaction.
32. Quiescing abort conservation: when DataAbortCompleted races a pending frame
    close, the active Data responder receives Err(Database(e)), every pending
    frame waiter receives one shared Failed(Database(e)), every root waiter
    receives the one shared AbortedByBackend(e), no effect is published, and no
    responder remains retained outside the settled replay record.
33. **Safety plus liveness.** Generation-retirement conservation: entry to Quarantining preserves the
    claim, reservation, session owner, every frame/effect, pending outcome, and
    every responder. None is released, discarded, published, or terminally
    replied before a GenerationRetired/BackendActorUnavailable event carries the
    exact token, retirement id, TxKey, actor generation, and private proof seal.
    A matching proof releases exactly once; a stale/foreign proof is pure. The
    every current RetirementFence reissues only the same idempotent retirement
    id, arms a fresh generation before returning, and therefore cannot strand a
    claim merely because a retire-effect executor died.
34. Hard-stop permit conservation: every minted ExplicitHardStopPermit is
    consumed exactly once by one of (a) its retained Result delivery, (b) its
    keyed authenticated cancellation preemption, (c) its physical-fence
    completion, (d) an explicit transition to Quarantining that moves its job
    into `superseded_fence_jobs` before replacing the old state, or (e) a
    pre-install CancelWon/FenceWon/route-mismatch branch that first calls
    `consume_owned_permit_infallible` and then cancels the still-dormant job.
    No trace observes two arms, no HardStopping record reaches Cancelling,
    Quarantining, or Settled with an unconsumed permit, every retirement retry
    idempotently finishes/cancels the moved jobs, and
    ClaimedByPublisher exists only with an already-keyed root-preemption event.
    Ordinary FencePending owns a supervisor job and pinned route while its
    reducer permit may remain Open; either path survives publisher-task death.
35. Action/stage closure: every reachable InFlight/Quiescing state is one of
    Data+Authority, Data+DataSql, or a matching frame action+FrameControl.
    Mutating any action or stage independently makes the completion pure STC;
    no session, frame, reply, or effect changes.
36. Data-abort job conservation: every dormant job registered before explicit
    Execute is either carried unchanged into DataSql/OWNER_COMPLETE, or is
    finished/cancelled exactly once on every pre-Data authority failure,
    denial, re-resolution, force-after suppression, actor retirement, or
    terminal path. Deleting the AuthorityUnavailable cancellation effect leaves
    one live job after a fair drain and must fail this property.

# Artifact 2: SC-2 SQLite cancellation/completion linearization

## 11. Verified actor and SQLite facts

Today Command contains SQL/parameters and a reply sender but no reservation,
owner, incarnation, or cancellation identity
(crates/zeroship-data-v8/src/backend/sqlite/session.rs:155-246). One blocking
actor loop completes run_* and sends the reply before receiving another command
(crates/zeroship-data-v8/src/backend/sqlite/session.rs:403-445). Both the
command documentation and session Drop state that a dropped caller does not
cancel the SQL
(crates/zeroship-data-v8/src/backend/sqlite/session.rs:155-161,641-657).

Cargo.lock resolves rusqlite 0.39.0 and libsqlite3-sys 0.37.0
(Cargo.lock:3104-3114; Cargo.lock:4859-4871). That rusqlite exposes a Send + Sync
InterruptHandle that calls sqlite3_interrupt
(/home/ruiyang/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/rusqlite-0.39.0/src/lib.rs:1015-1020,1268-1284).
SQLite explicitly says a nearly finished operation can complete, interrupted
DML in an explicit transaction can roll back the whole transaction, and an
interrupt invoked while no statement is running is a no-op
(/home/ruiyang/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/libsqlite3-sys-0.37.0/sqlite3/sqlite3.h:2894-2917).

I verified the exact tree command

~~~text
rg -n --fixed-strings SQLITE_INTERRUPT crates libs
~~~

returns exit 1 with zero matches. The mapper names BUSY variants and four
constraint variants, then maps every other SQLite failure to Transient
(crates/zeroship-data-v8/src/backend/sqlite/error.rs:35-43,52-140). Therefore
intentional SQLITE_INTERRUPT has no typed arm today.

## 12. Incarnation-qualified reservations and commands

~~~rust
struct AppActor {
    attached: AppAuthority,
    actor_generation: u64,
    generation_fenced: Arc<AtomicBool>,
    generation_owner_gate: Arc<Mutex<()>>,
    tx_conn: ConnectionSlot, // at most one explicit transaction reservation
    op_conn: ConnectionSlot, // autocommit work and platform-role authority reads
    active_sql: Arc<Mutex<Option<ActiveSqlTarget>>>,
    detach_latch: Arc<DetachLatch>,
    reservations: HashMap<ReservationId, ReservationRecord>,
    terminal_records: HashMap<ReservationId, TerminalRecord>,
}

struct ReservationId {
    app: AppAuthority,
    nonce: u128, // privileged actor-front-end-minted, never reused
}

enum ConnectionLane {
    Tx,
    Op,
}

struct ActiveSqlTarget {
    id: ReservationId,
    lane: ConnectionLane,
    connection_generation: u64,
    command_sequence: u64,
    // Logical ActorCommand sequence used by Cancel. It can differ from the
    // per-statement sequence for Begin's snapshot-marker substep.
    cancellation_sequence: u64,
    statement: SqlStatementClass,
    interrupt: InterruptHandle,
    interrupt_sent: bool,
}

enum SqlStatementClass {
    PrepareAuthority,
    OperationAuthority,
    Begin,
    SnapshotMarker,
    Data,
    FrameControl,
    Commit,
    Rollback,
    // Compensating rollback owned by OWNER_COMPLETE after COMMIT or an
    // autocommit operation failed. It is never cancellation cleanup.
    FailureCleanupRollback,
    CleanupRollback,
    PostRollbackAuthority,
}

struct SqliteSnapshotMarker {
    state: LifecycleState,
    epoch: SchemaEpoch,
    incarnation: AppIncarnationId,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReservationKind {
    Autocommit,
    Transaction,
}

enum ActorCommand {
    Reserve {
        id: ReservationId,
        resolved_epoch: SchemaEpoch,
        kind: ReservationKind,
        reply: Reply<Result<(), ActorError>>, // control registration only; no SQL
    },
    Prepare {
        id: ReservationId,
        command_sequence: u64,
        reply: Reply<Result<PreparedReservation, ActorError>>,
    },
    Begin {
        id: ReservationId,
        command_sequence: u64,
        prepared: PreparedReservation,
        reply: Reply<Result<OpenedReservation, DbError>>,
    },
    Execute {
        id: ReservationId,
        command_sequence: u64,
        operation: DbOperation,
        // Exact authenticated fallback route, installed before this command is
        // enqueueable. Explicit Execute uses ExplicitDataAbort; autocommit uses
        // Autocommit. Any other variant is ForeignReservation.
        terminal_delivery: TerminalDelivery,
        reply: Reply<Result<SharedDbResult, DbError>>,
    },
    Settle {
        id: ReservationId,
        // Exact active command-gate sequence installed by SC-1 together with
        // its root CommandToken. The actor cannot infer it from "latest".
        command_sequence: u64,
        decision: RootDecision,
        // Exact ExplicitRoot descriptor (or Autocommit descriptor for an
        // internal autocommit terminal command), written before OWNER_COMPLETE.
        terminal_delivery: TerminalDelivery,
        reply: Reply<Result<SharedActorTerminalOutcome, ActorError>>,
    },
    Cancel {
        id: ReservationId,
        // Snapshot under active_command_gate before force publication. None is
        // exact for queued/preparing/idle reservations with no active command.
        command_sequence: Option<u64>,
        cause: CancelCause,
        reply: SharedCancelReplySink,
    },
    TerminalWatchdog {
        id: ReservationId,
        command_sequence: u64,
        watchdog_generation: u64,
    },
    Release {
        id: ReservationId,
    },
    DetachApp {
        expected: AppAuthority,
        reply: Reply<Result<(), ActorError>>,
    },
    ForgetTerminal {
        id: ReservationId,
        lease_generation: u64,
    },
}

struct PreparedReservation {
    authority: AuthorityObservation,
    lease: ActorPreparedLease,
}

struct OpenedReservation {
    snapshot: SqliteSnapshotMarker,
    lease: ActorTransactionLease,
}

#[derive(Clone, PartialEq, Eq)]
enum CancelCause {
    CallerDrop,
    Explicit,
    IsolateTeardown,
    Deadline,
    Detach,
    AuthorityDenied(AuthorityDenyReason),
    EpochChanged(SchemaEpoch),
    // Trusted SC-1 reducer-only causes. Creator/external Cancel APIs cannot
    // construct them; they preserve why setup/BEGIN/unknown-health cleanup ran.
    SetupFailed(DbError),
    BeginUncertain(DbError),
    BackendUnknown(DbError),
}

enum CancelResult {
    Cancelled {
        cause: CancelCause,
        cleanup: CleanupDisposition,
    },
    AlreadyCompleted {
        outcome: SharedActorTerminalOutcome,
    },
}

type SharedDbResult = Arc<DbResult>;
type SharedActorTerminalOutcome = Arc<ActorTerminalOutcome>;

#[derive(Clone)]
struct SharedCancelReplySink {
    result: Arc<OnceLock<Result<CancelResult, ActorError>>>,
    waiters: Arc<Mutex<Vec<Waker>>>,
}

impl SharedCancelReplySink {
    fn store_and_wake(&self, result: Result<CancelResult, ActorError>) {
        let _ = self.result.set(result); // duplicates observe the first value
        for waiter in self.waiters.lock().drain(..) { waiter.wake(); }
    }
}

#[derive(PartialEq)]
enum ActorTerminalOutcome {
    // Owns a replayable copy of the exact operation result. In particular,
    // ordinary SQLite errors are not collapsed into "completed".
    Autocommit(Result<SharedDbResult, DbError>),
    AutocommitIndeterminate(DbError),
    Committed,
    RolledBack,
    CommitRolledBack,
    TerminalResultMismatch,
    Cancelled {
        cause: CancelCause,
        cleanup: CleanupDisposition,
    },
    TransactionAborted(DbError),
    CommitFailed(DbError),
    CommitIndeterminate(DbError),
    RollbackFailed(DbError),
    CleanupIndeterminate(DbError),
    ActorUnavailable(DbError),
}

// Written before the owner CAS, so the supervisor can classify actor death
// even if outcome has not yet been stored.
enum TerminalAttempt {
    AutocommitSuccess(SharedDbResult),
    AutocommitFailure(DbError),
    // Stored before OWNER_COMPLETE for raw 517. The classified form replaces
    // it only after proved rollback and a fresh platform-role authority read.
    AutocommitSnapshotAbortPending(DbError),
    AutocommitSnapshotAbort(DbError),
    ExplicitCommit,
    ExplicitRollback,
    ExplicitTransactionAborted(DbError),
    ExplicitSnapshotAbortPending,
    ExplicitSnapshotAbort(DbError),
    Cancellation {
        cause: CancelCause,
        phase_proof: CancelPhaseProof,
        // Some only when the command gate proved that this exact finalized
        // statement is cancellation's predecessor. IDs are equality tokens,
        // never an ordered clock.
        predecessor_sequence: Option<u64>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CancelPhaseProof {
    // Captured only from Queued, Preparing, or BeginNotOpened while the owner
    // and active-SQL gates exclude a concurrent BEGIN target.
    NoTransactionPossible,
    // Captured after the Begin target is published but before a successful
    // snapshot marker proves the transaction open.
    BeginMayHaveOpened,
    TransactionMayExist,
}

enum ReservationCutoff {
    // Both are the exact Arcs stored in the corresponding SC-1 TxEntry.
    Explicit {
        root: Arc<TerminalCutoffGate<RootFinishResult>>,
        cancel: Arc<TerminalCutoffGate<CancelAck>>,
    },
    Autocommit(Arc<TerminalCutoffGate<ActorTerminalOutcome>>),
}

#[derive(Clone)]
enum TerminalDelivery {
    ExplicitRoot {
        key: TxKey,
        cutoff: Arc<TerminalCutoffGate<RootFinishResult>>,
        token: CommandToken,
        registry: TxRegistrySender,
    },
    ExplicitCancel {
        key: TxKey,
        cutoff: Arc<TerminalCutoffGate<CancelAck>>,
        token: CommandToken,
        registry: TxRegistrySender,
    },
    ExplicitDataAbort {
        permit: Arc<DataDeliveryPermit>,
        key: TxKey,
        cutoff: Arc<TerminalCutoffGate<DataAbortProof>>,
        data_token: CommandToken,
        registry: TxRegistrySender,
        // Exact dormant job registered before this Execute became visible.
        hard_stop_trigger: HardStopTrigger,
    },
    Autocommit {
        sink: SharedActorTerminalSink,
    },
}

#[derive(Clone, PartialEq, Eq)]
enum CleanupDisposition {
    NoSqlStarted,
    OutsideTransactionCompleted,
    // BEGIN was attempted but a post-return is_autocommit sample proves it did
    // not leave a transaction open.
    BeginDidNotOpen,
    RolledBack,
    SQLiteAlreadyRolledBack,
}

enum ActorError {
    UnknownReservation,
    ForeignReservation,
    StaleAppIncarnation,
    AppDeprovisioned,
    SchemaEpochChanged,
    CancellationCleanupFailed,
    CancellationProtocolMismatch,
    ActorUnavailable,
    ActorUnavailableWithSource(DbError),
    ActorSaturated,
    TerminalReplyProjectionMismatch,
    Database(DbError),
}

enum OwnerClaimError {
    ActorFenced,
    TerminalRouteNotArmed,
    Lost { observed: u8 },
}

enum ActorControlError {
    UnknownReservation,
    StaleTerminalWatchdog,
    TerminalWatchdogProtocolFault,
    ActorUnavailable,
}

fn unexpected_sqlite_interrupt() -> DbError {
    DbError::Coded {
        code: "unexpected_sqlite_interrupt".into(),
        message: "SQLite returned SQLITE_INTERRUPT without the matching reservation intent or terminal watchdog".into(),
        hint: Some("the connection generation was quarantined".into()),
    }
}

fn terminal_deadline_exceeded() -> DbError {
    DbError::Coded {
        code: "terminal_deadline_exceeded".into(),
        message: "terminal SQL did not finish before interrupt grace expired".into(),
        hint: Some("the backend generation was fenced; commit status may be indeterminate".into()),
    }
}

fn schema_snapshot_stale() -> DbError {
    DbError::Coded {
        code: "schema_snapshot_stale".into(),
        message: "the SQLite snapshot no longer matches trusted app authority".into(),
        hint: Some("re-resolve the binding and retry in a new transaction".into()),
    }
}

fn serialization_conflict() -> DbError {
    DbError::Coded {
        code: "serialization_conflict".into(),
        message: "the SQLite WAL snapshot could not be upgraded to a writer".into(),
        hint: Some("retry the transaction from BEGIN".into()),
    }
}

fn actor_unavailable() -> DbError {
    DbError::Coded {
        code: "actor_unavailable".into(),
        message: "the SQLite actor generation became unreachable".into(),
        hint: Some("retry only after the binding is re-resolved".into()),
    }
}
~~~

`Begin` is not an exception to exact-target cancellation. Its actor arm is this
two-statement subprotocol; `run_statement_exact` is the typed
StartDecision/progress/finalization algorithm in section 14 and installs the
listed ActiveSqlTarget before calling SQLite:

~~~text
validate exact reservation, PreparedReservation, command gate, and Fork-C key
phase := Beginning
begin_run := run_statement_exact(Begin, tx_conn, command_sequence, "BEGIN")
exhaust StatementRun and ClassifiedRawResult through consume_statement_run_exact(Begin)
if barrier says cancellation/fence: do not call SQLite; let the retained Cancel run
if begin_result is Err:
  sample tx_conn.is_autocommit immediately
  if true: phase := BeginNotOpened; classify as NotOpened(error)
  if false: phase := BeginningWithOpenTransaction; classify as MayHaveOpened(error)
  if matching CANCEL_INTENT caused SQLITE_INTERRUPT:
    true -> CancelResult cleanup BeginDidNotOpen
    false -> run exact CleanupRollback and prove RolledBack/Indeterminate
  clear the exact Begin target/progress hook before publishing either result
if begin_result is Ok but is_autocommit is true: protocol-fence the generation

phase := BeginningWithOpenTransaction
marker_sequence := mint_never_reused_command_sequence()
marker_run := run_statement_exact(
  SnapshotMarker, tx_conn, marker_sequence, first snapshot-marker SELECT)
exhaust StatementRun and ClassifiedRawResult through consume_statement_run_exact(SnapshotMarker)
if marker_result is Ok and matches prepared authority/epoch/incarnation:
  phase := Idle; publish OpenedReservation through the Begin command gate
if marker fails/mismatches or matching cancellation interrupts it:
  the transaction is known open; run exact CleanupRollback under the winner
  and publish MayHaveOpened(error) or cause-specific CancelResult only after
  rollback proof; an unexpected SQLITE_INTERRUPT protocol-fences the generation
always remove the exact progress hook and ActiveSqlTarget before the next step
~~~

Consequently Starting cancellation has a representable target both before and
after BEGIN opens. `CancelPhaseProof::BeginMayHaveOpened` plus a post-return
autocommit sample is the only producer of `BeginDidNotOpen`; once the marker
target is installed the phase proof is TransactionMayExist and `NoSqlStarted`
cannot be accepted as a cleanup proof.

These are concrete Coded errors, not undeclared enum variants. The current
DbError contract explicitly provides Coded { code, message, hint } for a code
chosen by another subsystem
(crates/zeroship-data-v8/src/error.rs:145-160).

The actor-to-SC-1 adapters are closed and run before either explicit cutoff is
published:

~~~rust
struct AdaptedProof<T> {
    proof: T,
    fence_before_publish: bool,
}

fn root_proof(outcome: &ActorTerminalOutcome) -> AdaptedProof<RootFinishResult> {
    match outcome {
        ActorTerminalOutcome::Committed => AdaptedProof {
            proof: RootFinishResult::Committed, fence_before_publish: false,
        },
        ActorTerminalOutcome::RolledBack => AdaptedProof {
            proof: RootFinishResult::RolledBack, fence_before_publish: false,
        },
        ActorTerminalOutcome::CommitFailed(error) => AdaptedProof {
            proof: RootFinishResult::Failed {
            error: error.clone(),
            certainty: FinishCertainty::DefinitelyNotCommitted,
            }, fence_before_publish: false,
        },
        ActorTerminalOutcome::CommitIndeterminate(error)
        | ActorTerminalOutcome::RollbackFailed(error) => AdaptedProof {
            proof: RootFinishResult::Failed {
                error: error.clone(),
                certainty: FinishCertainty::Indeterminate,
            }, fence_before_publish: false,
        },
        other => AdaptedProof {
            proof: RootFinishResult::Failed {
                error: actor_protocol_error(
                    "non-root outcome on root delivery", other),
                certainty: FinishCertainty::Indeterminate,
            },
            fence_before_publish: true,
        },
    }
}

fn cancel_proof(
    result: Result<CancelResult, ActorError>,
    phase_proof: CancelPhaseProof,
) -> AdaptedProof<CancelAck> {
    match result {
        Ok(CancelResult::Cancelled {
            cleanup: CleanupDisposition::NoSqlStarted
                | CleanupDisposition::OutsideTransactionCompleted, ..
        }) if phase_proof == CancelPhaseProof::NoTransactionPossible =>
            AdaptedProof { proof: CancelAck::NoTransaction,
                fence_before_publish: false },
        Ok(CancelResult::Cancelled {
            cleanup: CleanupDisposition::BeginDidNotOpen, ..
        }) if phase_proof == CancelPhaseProof::BeginMayHaveOpened =>
            AdaptedProof { proof: CancelAck::NoTransaction,
                fence_before_publish: false },
        Ok(CancelResult::Cancelled {
            cleanup: CleanupDisposition::RolledBack
                | CleanupDisposition::SQLiteAlreadyRolledBack, ..
        }) => AdaptedProof { proof: CancelAck::RolledBack,
            fence_before_publish: false },
        Ok(CancelResult::Cancelled {
            cleanup: CleanupDisposition::NoSqlStarted
                | CleanupDisposition::OutsideTransactionCompleted
                | CleanupDisposition::BeginDidNotOpen, ..
        }) => AdaptedProof {
            proof: CancelAck::Indeterminate(actor_protocol_error(
                "NoSqlStarted after an explicit transaction could be open",
                phase_proof,
            )), fence_before_publish: true,
        },
        Err(error) => AdaptedProof {
            proof: CancelAck::Indeterminate(actor_error_as_db_error(error)),
            fence_before_publish: true,
        },
        Ok(CancelResult::AlreadyCompleted { .. }) => AdaptedProof {
            proof: CancelAck::Indeterminate(actor_protocol_error(
                "AlreadyCompleted on OWNER_CANCEL delivery",
                phase_proof,
            )), fence_before_publish: true,
        },
    }
}
~~~

SQLite and PostgreSQL explicit cancellation perform/observe rollback inside the
backend owner, so no adapter returns a live transaction. `AlreadyCompleted` replays the
real root/data proof that won its own cutoff and never fabricates CancelAck. For
an explicit Settle result the actor calls `root_proof`; explicit cancellation
calls `cancel_proof`. If `fence_before_publish`, the common adapter first
physically fences the exact actor generation, then publishes the typed
Indeterminate proof through the exact cutoff. Otherwise it publishes directly.
There is therefore no Err branch that can clear a timer and strand OWNER_*.
Autocommit publishes
the ActorTerminalOutcome directly. Every closure uses the `TerminalDelivery`
stored before the owner CAS and the gate's keyed enqueue-once id.

SC-2's CancelCause is the canonical actor boundary. SC-1 maps it without
dropping information: CallerDrop/Explicit/IsolateTeardown map identically;
ForcedReason::DeadlineExceeded maps to Deadline, Detach to Detach,
AuthorityDenied(r) to AuthorityDenied(r), and EpochChanged(e) to
EpochChanged(e). The actor returns that same value in Cancelled; SC-1 converts
it to its corresponding terminal outcome. There is no generic "cancel" arm
that can erase deadline, lifecycle, or epoch provenance.

For SQLite, TxEntry does not race an independent cause choice against this
latch. Every SC-1 forced publisher first calls the reservation's cause-latch
operation and uses the returned immutable CancelCause to construct
CleanupCause::Forced; a caller Drop does the same before it enqueues Registry's
Cancel event. Thus a racing DeadlineFired cannot record Deadline in SC-1 while
the actor records CallerDrop, or vice versa. PostgreSQL uses the same logical
first-cause latch in its command cancellation gate, without an SQLite actor.

There is one actor thread per attached app file and it owns both connections.
An open transaction reservation occupies tx_conn but does not occupy op_conn;
the actor may therefore service an autocommit read or an explicit transaction’s
authority read between tx_conn commands. A single synchronous SQLite statement
still occupies the actor thread until that statement returns. This is the
two-connection/one-actor choice in SC-2
(docs/proposals/2026-08-26-sc2-sqlite-actor-protocol.md:55-72,103-108).

AppIncarnationId is not the reservation nonce. The privileged lifecycle
publisher mints the former; the privileged actor front end mints the latter
before queueing Reserve. Reserve, Execute,
Settle, Cancel, Release, DetachApp, the active map, and reservation terminal
records all carry AppAuthority. The actor compares the externally supplied
authority domain plus the trusted AuthorityObservation, then checks the
in-transaction SqliteSnapshotMarker before any creator data SQL. Changing/epoch
mismatch returns ReResolve; domain/incarnation mismatch returns terminal
StaleAppIncarnation; Deprovisioned returns terminal AppDeprovisioned.
Garbage-collecting a reservation terminal record never clears the separate,
permanent application-lifecycle tombstone.

DetachApp(expected) first compares the entire AppAuthority. On a match it rejects
new reservations, cancels queued and running work through the same Cancel
protocol, waits for cleanup acknowledgement, closes tx_conn and op_conn, and
only then acknowledges file replacement. A DetachApp for incarnation A against
an attachment for B cannot interrupt or close B. This implements SC-2’s
close-before-swap rule
(docs/proposals/2026-08-26-sc2-sqlite-actor-protocol.md:160-162;
docs/proposals/2026-08-26-sc2-sqlite-actor-protocol.md:228-229) without losing
Fork C.

Detach is published out of band before its ActorCommand is enqueued, because a
queued command cannot interrupt the synchronous statement that prevents the
actor from receiving it:

~~~rust
fn publish_detach(handle: &AppDetachHandle, expected: AppAuthority) {
    if handle.actor_control.attached != expected {
        return reply(ForeignReservation);
    }
    handle.actor_control.detach_latch.set_exact(expected);
    for control in handle.actor_control.controls_for_exact(expected) {
        publish_cancel(control, CancelCause::Detach);
    }
    // No second interrupt path: publish_cancel reloads OWNER_OPEN while holding
    // active_sql. Detach therefore cannot interrupt completion-owned terminal
    // SQL or cancellation-owned cleanup SQL.
    handle.detach_permit.send(DetachApp { expected, reply });
}
~~~

The latch is keyed by complete AppAuthority and is checked at the same pre-start
barrier as CANCEL_INTENT. Detach for A cannot latch, interrupt, or enqueue a
close against actor attachment B. The ActorCommand performs cleanup, closes both
lanes, and acknowledges; it is not the first notification of detach.

## 13. Actor phase and the actor-owned terminal CAS

Each reservation has a control record shared with its cancellation handles:

~~~rust
const CANCEL_INTENT: u8 = 0b001;
const OWNER_MASK:    u8 = 0b110;
const OWNER_OPEN:    u8 = 0b000;
const OWNER_CANCEL:  u8 = 0b010;
const OWNER_COMPLETE:u8 = 0b100;
const OPEN_WITH_CANCEL_INTENT: u8 = OWNER_OPEN | CANCEL_INTENT;
const CANCEL_WITH_INTENT: u8 = OWNER_CANCEL | CANCEL_INTENT;

fn owner(word: u8) -> u8 { word & OWNER_MASK }
fn has_cancel_intent(word: u8) -> bool { word & CANCEL_INTENT != 0 }
fn is_terminal_owner(value: u8) -> bool {
    value == OWNER_CANCEL || value == OWNER_COMPLETE
}

fn terminal_interrupt_matches(
    control: &ReservationControl,
    command_sequence: u64,
) -> bool {
    let g1 = control.terminal_interrupt_generation.load(Acquire);
    if g1 == 0 {
        return false;
    }
    let sequence = control.terminal_interrupt_sequence.load(Acquire);
    let g2 = control.terminal_interrupt_generation.load(Acquire);
    g1 == g2 && sequence == command_sequence
}

enum ActorPhase {
    Queued,
    Preparing,
    // ActiveSqlTarget::Begin has been published; BEGIN may or may not have
    // opened a transaction until the post-return is_autocommit sample.
    Beginning,
    // BEGIN succeeded and ActiveSqlTarget::SnapshotMarker is published.
    BeginningWithOpenTransaction,
    // BEGIN returned typed NotOpened; no transaction exists, but the control
    // reservation still requires retirement.
    BeginNotOpened,
    Idle,
    Running {
        reservation_id: ReservationId,
        command_sequence: u64,
        lane: ConnectionLane,
        connection_generation: u64,
    },
    BetweenStatementAndCommit,
    CompletionOwned,
    CancellationOwned,
    Cleaning,
    Fenced,
    Terminal,
}

impl ActorPhase {
    fn is_running_exactly(
        &self,
        reservation_id: ReservationId,
        lane: ConnectionLane,
        connection_generation: u64,
        command_sequence: u64,
    ) -> bool {
        matches!(self, ActorPhase::Running {
            reservation_id: stored_id,
            command_sequence: stored_sequence,
            lane: stored_lane,
            connection_generation: stored_generation,
        } if *stored_id == reservation_id
            && *stored_sequence == command_sequence
            && *stored_lane == lane
            && *stored_generation == connection_generation)
    }
}

struct ReservationControl {
    id: ReservationId,
    // Copied from the already-validated ReserveSpec before the control is
    // published. Cancellation cleanup must not infer the physical lane from a
    // transient ActorPhase or ActiveSqlTarget.
    kind: ReservationKind,
    actor_generation: u64,
    execution_deadline_at: Option<Instant>,
    terminal_sql_timeout: Duration,
    terminal_interrupt_grace: Duration,
    tx_key: Option<TxKey>, // Some for SC-1 explicit transactions; exact Fork-C key
    resolved_epoch: SchemaEpoch,
    phase: AtomicPhase,
    terminal: AtomicU8, // starts OWNER_OPEN with no CANCEL_INTENT
    // Some only for explicit transactions and pointer-equal to TxEntry's Arc.
    explicit_deadlines: Option<Arc<ExplicitDeadlineSlots>>,
    // Actor-owned budget only: autocommit or ExplicitDataAbort. Shared explicit
    // root/cancel timing lives solely in explicit_deadlines.
    current_terminal_sequence: AtomicU64,
    terminal_watchdog_armed: AtomicU64,
    terminal_hard_stop_armed: AtomicU64,
    terminal_cutoff: ReservationCutoff,
    // Seqlock-style read by the SQLite progress callback; no mutex/FFI-path
    // allocation. generation==0 means no interrupt intent.
    terminal_interrupt_generation: AtomicU64,
    terminal_interrupt_sequence: AtomicU64,
    // One atomic bundle installed by SC-1 before root Settle is enqueueable.
    // Its route/job were prepared before the owner-gate installation, so a
    // generation-death observer can never see a root descriptor without its
    // durable driver. Cancellation has its reserve-time bundle below.
    preinstalled_root: OnceLock<PreinstalledExplicitRoot>,
    // Chosen copy installed only by the successful actor owner claim.
    terminal_delivery: Mutex<Option<TerminalDelivery>>,
    // Installed synchronously for every reservation, before actor execution.
    cancel_delivery: TerminalDelivery,
    explicit_failure_sink: Option<ExplicitFailureSink>,
    active_command_gate: Mutex<Option<ActiveCommandRef>>,
    generation_fenced: Arc<AtomicBool>, // shared by every control in this actor generation
    terminal_owner_gate: Arc<Mutex<()>>, // same generation gate held by AppActor
    // The first successful publication is immutable and is the value carried
    // by the one explicit Cancel command. OnceLock is deliberately lock-free
    // at every later read, so it cannot invert terminal_owner_gate ordering.
    cancel_cause: OnceLock<CancelCause>,
    cancel_publish: AtomicCancelPublish,
    // Independent terminal retention/replay cell. It owns no cutoff and is the
    // only object captured by prepared-delivery closures.
    retention: Arc<TerminalRetentionCell>,
    terminal_route_latch: TerminalRouteLatch,
    pending_cancel: Mutex<Option<PendingCancel>>,
    pending_root_settle: Mutex<Option<PendingRootSettle>>,
    preclaim_autocommit_cancel_budget:
        OnceLock<Arc<PreclaimAutocommitCancelBudget>>,
    // Reserve installs this only after its dormant supervisor job and typed
    // endpoint route exist; the control is not published until the set wins.
    preclaim_autocommit_fence_trigger: OnceLock<HardStopTrigger>,
    // Some only for explicit reservations. Reserve installs it before index or
    // handle publication, so immediate Drop/Cancel needs no later capacity.
    preinstalled_explicit_cancel_fence:
        OnceLock<PreinstalledExplicitCancelFence>,
    // Explicit root uses a distinct cutoff/job. Reserve installs this beside
    // the cancel fence, before a handle or control-index entry is visible.
    preinstalled_explicit_root_fence:
        OnceLock<PreinstalledExplicitRootFence>,
    preclaim_timers_published: AtomicBool,
    preclaim_watchdog_fired: AtomicBool,
    // Written while clearing an exact target, before phase becomes a generic
    // between-statements value; consumed only by the matching Cancel sequence.
    finalized_cancel_proof: Mutex<Option<FinalizedCancelProof>>,
    control_permit: OwnedControlPermit,
    terminal_timer_permit: OwnedTerminalTimerPermit,
    actor_control: Arc<ActorControlIndex>,
    // Installed before a real cutoff Result is published. Ordinary terminal
    // paths store Ended or NeedsGenerationRetirement; supervisor fence paths
    // construct GenerationRetired from their physical proof.
    terminal_retirement: OnceLock<BackendTerminalRetirement>,
    terminal_attempt: Mutex<Option<TerminalAttempt>>,
    // Some only while OWNER_COMPLETE executes FailureCleanupRollback. The
    // terminal-watchdog interrupt consumer takes this exact context, so it can
    // preserve the original error/result class without guessing from phase.
    failure_cleanup_context: Mutex<Option<FailureCleanupContext>>,
    // Some only while OWNER_COMPLETE executes PostRollbackAuthority. It is
    // installed before the target becomes visible and consumed by either the
    // ordinary classifier or the authenticated terminal-watchdog continuation.
    postrollback_authority_context:
        Mutex<Option<PostRollbackAuthorityContext>>,
    live_terminal_leases: AtomicUsize,
}

#[derive(Clone)]
enum FailureCleanupContext {
    ExplicitCommit {
        terminal: CompletedSqlTargetProof,
        commit_error: DbError,
    },
    AutocommitCommit {
        terminal: CompletedSqlTargetProof,
        commit_error: DbError,
    },
    AutocommitOperation {
        original_error: DbError,
    },
    SnapshotAbort(SnapshotAbortContext),
}

#[derive(Clone)]
enum SnapshotAbortDestination {
    Explicit { permit: Arc<DataDeliveryPermit> },
    Autocommit,
}

#[derive(Clone)]
struct SnapshotAbortContext {
    data_completed: CompletedSqlTargetProof,
    raw_mapped: DbError,
    destination: SnapshotAbortDestination,
}

#[derive(Clone)]
struct PostRollbackAuthorityContext {
    ended: CompletedSqlTargetProof,
    snapshot: SnapshotAbortContext,
}

struct ActiveCommandRef {
    command_sequence: u64,
    gate: Arc<CommandCompletionGate>,
    // Some only for an explicit Execute and pointer-identical to the permit in
    // its ActiveAction and TerminalDelivery::ExplicitDataAbort.
    data_delivery_permit: Option<Arc<DataDeliveryPermit>>,
}

struct TerminalRouteLatch {
    generation: AtomicU64,
    actor_wake: ActorWake,
}

struct PendingCancel {
    command_sequence: Option<u64>,
    cause: CancelCause,
    // Pointer-identical shared sink from the sole ActorCommand::Cancel.
    reply: SharedCancelReplySink,
    last_seen_route_generation: u64,
}

struct PendingRootSettle {
    command_sequence: u64,
    decision: RootDecision,
    delivery: TerminalDelivery,
    reply: Reply<Result<SharedActorTerminalOutcome, ActorError>>,
}

struct FinalizedCancelProof {
    cancellation_sequence: u64,
    phase: CancelPhaseProof,
    evidence: FinalizedCancelEvidence,
}

enum FinalizedCancelEvidence {
    NoStatementStarted(NoSqlStartProof),
    // PrepareAuthority ran on the platform/op lane, but BEGIN cannot yet have
    // run. The sealed post-finalization sample proves clean autocommit without
    // pretending no FFI call occurred.
    CompletedOutsideTransaction(PostFinalizeAutocommitProof),
    Statement(CompletedSqlTargetProof),
}

struct PreclaimAutocommitCancelBudget {
    budget_id: TerminalDeliveryId,
    first_deadline: Instant,
    watchdog_generation: u64,
    hard_stop_generation: u64,
    trigger: HardStopTrigger,
}

struct PreinstalledExplicitCancelFence {
    job: DurableFenceJobHandle,
    fence_token: CommandToken,
    mailbox: PinnedRegistryMailbox,
}

struct PreinstalledExplicitRootFence {
    job: DurableFenceJobHandle,
    fence_token: CommandToken,
    mailbox: PinnedRegistryMailbox,
}

#[derive(Clone)]
struct PreinstalledExplicitRoot {
    delivery: TerminalDelivery,
    decision: RootDecision,
    hard_stop: RegisteredExplicitHardStop,
}

struct ExplicitFailureSink {
    key: TxKey,
    cancellation_token: CommandToken,
    registry: TxRegistrySender,
}

fn cancel_delivery_for_control(
    control: &ReservationControl,
) -> Result<TerminalDelivery, OwnerClaimError> {
    let delivery = control.cancel_delivery.clone();
    if delivery_matches_control(control, &delivery, None) {
        Ok(delivery)
    } else {
        Err(OwnerClaimError::ActorFenced)
    }
}

fn delivery_matches_control(
    control: &ReservationControl,
    delivery: &TerminalDelivery,
    current_data_permit: Option<&Arc<DataDeliveryPermit>>,
) -> bool {
    match (delivery, &control.terminal_cutoff) {
        (TerminalDelivery::ExplicitRoot { key, cutoff, .. },
         ReservationCutoff::Explicit { root, .. }) =>
            control.tx_key.as_ref() == Some(key) && Arc::ptr_eq(cutoff, root),
        (TerminalDelivery::ExplicitCancel { key, cutoff, .. },
         ReservationCutoff::Explicit { cancel, .. }) =>
            control.tx_key.as_ref() == Some(key) && Arc::ptr_eq(cutoff, cancel),
        (TerminalDelivery::ExplicitDataAbort {
             permit, key, cutoff, data_token, hard_stop_trigger, ..
         }, ReservationCutoff::Explicit { .. }) =>
            control.tx_key.as_ref() == Some(key)
                && control.actor_generation == permit.actor_generation
                && permit.key == *key
                && permit.data_token == *data_token
                && permit.cutoff_delivery_id == cutoff.delivery_id
                && hard_stop_trigger.job() == permit.fence_job
                && current_data_permit.is_some_and(|stored| {
                    Arc::ptr_eq(stored, permit)
                }),
        (TerminalDelivery::Autocommit { sink },
         ReservationCutoff::Autocommit(_)) =>
            matches!(&control.cancel_delivery,
                TerminalDelivery::Autocommit { sink: stored }
                    if sink.same_endpoint(stored)),
        _ => false,
    }
}

fn same_terminal_delivery(a: &TerminalDelivery, b: &TerminalDelivery) -> bool {
    match (a, b) {
        (TerminalDelivery::ExplicitRoot {
             key: ak, cutoff: ac, token: at, registry: ar,
         }, TerminalDelivery::ExplicitRoot {
             key: bk, cutoff: bc, token: bt, registry: br,
         }) => ak == bk && at == bt && Arc::ptr_eq(ac, bc)
              && ar.same_endpoint(br),
        (TerminalDelivery::ExplicitCancel {
             key: ak, cutoff: ac, token: at, registry: ar,
         }, TerminalDelivery::ExplicitCancel {
             key: bk, cutoff: bc, token: bt, registry: br,
         }) => ak == bk && at == bt && Arc::ptr_eq(ac, bc)
              && ar.same_endpoint(br),
        (TerminalDelivery::ExplicitDataAbort {
             permit: ap, key: ak, cutoff: ac, data_token: at, registry: ar,
             hard_stop_trigger: ah,
         }, TerminalDelivery::ExplicitDataAbort {
             permit: bp, key: bk, cutoff: bc, data_token: bt, registry: br,
             hard_stop_trigger: bh,
         }) => Arc::ptr_eq(ap, bp) && ak == bk && at == bt
              && Arc::ptr_eq(ac, bc) && ar.same_endpoint(br)
              && ah.job() == bh.job(),
        (TerminalDelivery::Autocommit { sink: a },
         TerminalDelivery::Autocommit { sink: b }) => a.same_endpoint(b),
        _ => false,
    }
}

fn terminal_class_and_delivery_id(
    delivery: &TerminalDelivery,
) -> (TerminalPublicationClass, TerminalDeliveryId) {
    match delivery {
        TerminalDelivery::ExplicitRoot { cutoff, .. } =>
            (TerminalPublicationClass::Root, cutoff.delivery_id),
        TerminalDelivery::ExplicitCancel { cutoff, .. } =>
            (TerminalPublicationClass::Cancel, cutoff.delivery_id),
        TerminalDelivery::ExplicitDataAbort { cutoff, .. } =>
            (TerminalPublicationClass::DataAbort, cutoff.delivery_id),
        TerminalDelivery::Autocommit { .. } =>
            // The autocommit cutoff is stored in ReservationControl, so the
            // caller must use selected_terminal_delivery_id for this arm.
            unreachable!("autocommit delivery id comes from its selected cutoff"),
    }
}

enum RootPreinstallDecision {
    Installed,
    // CANCEL_INTENT/OWNER_CANCEL won under the same owner gate. The root
    // request is retained, but root SQL must not be issued.
    CancelWon { cause: CleanupCause },
    // The supervisor already made the actor generation unreachable. SC-1 may
    // terminalize only through a sealed retirement proof.
    FenceWon { cause: Option<CleanupCause> },
    // Another terminal publisher owns reality. This is impossible for a first
    // root install from Idle/Poisoned and is fenced as a producer fault.
    OtherCompletionWon,
}

enum RootBundleInstallError {
    ProtocolMismatch,
}

fn root_bundle_matches_control(
    control: &ReservationControl,
    delivery: &TerminalDelivery,
    decision: RootDecision,
    hard_stop: &RegisteredExplicitHardStop,
) -> bool {
    let TerminalDelivery::ExplicitRoot {
        key: delivery_key,
        cutoff: delivery_cutoff,
        token: delivery_token,
        registry: delivery_registry,
    } = delivery else { return false; };
    let HardStopTrigger::ExplicitRoot {
        permit,
        decision: trigger_decision,
        key: trigger_key,
        cutoff: trigger_cutoff,
        deadline_slots,
        terminal_token,
        fence_token,
        registry: trigger_registry,
        job,
    } = &hard_stop.trigger else { return false; };
    matches!(&control.terminal_cutoff,
        ReservationCutoff::Explicit { root, .. }
            if Arc::ptr_eq(root, delivery_cutoff))
        && control.tx_key.as_ref() == Some(delivery_key)
        && delivery_key == trigger_key
        && Arc::ptr_eq(delivery_cutoff, trigger_cutoff)
        && delivery_token == terminal_token
        && delivery_registry.same_endpoint(trigger_registry)
        && decision == *trigger_decision
        && control.explicit_deadlines.as_ref().is_some_and(|stored| {
            Arc::ptr_eq(stored, deadline_slots)
        })
        && Arc::ptr_eq(&hard_stop.permit, permit)
        && hard_stop.fence_token == *fence_token
        && permit.fence_job == *job
        && job.delivery_id == delivery_cutoff.delivery_id
}

// SC-1 calls this synchronously while reducing a first root settlement. The
// endpoint and dormant job were installed privately by Reserve; this one owner-
// gate critical section publishes delivery+decision+job as one bundle before
// state, TerminalSql, or Settle can become visible. Cancellation's stable
// bundle was installed by reserve().
fn install_explicit_root_bundle(
    control: &ReservationControl,
    delivery: TerminalDelivery,
    decision: RootDecision,
    hard_stop: &RegisteredExplicitHardStop,
) -> Result<RootPreinstallDecision, RootBundleInstallError> {
    let _owner = control.terminal_owner_gate.lock();
    let owner_word = control.terminal.load(Acquire);
    if !delivery_matches_control(control, &delivery, None)
        || !root_bundle_matches_control(control, &delivery, decision, hard_stop)
    {
        return Err(RootBundleInstallError::ProtocolMismatch);
    }
    let latched = control.cancel_cause.get().cloned().map(cleanup_cause);
    if control.generation_fenced.load(Acquire) {
        return Ok(RootPreinstallDecision::FenceWon { cause: latched });
    }
    match owner(owner_word) {
        OWNER_CANCEL => return Ok(RootPreinstallDecision::CancelWon {
            cause: latched.ok_or(RootBundleInstallError::ProtocolMismatch)?,
        }),
        OWNER_COMPLETE => return Ok(RootPreinstallDecision::OtherCompletionWon),
        OWNER_OPEN if has_cancel_intent(owner_word) =>
            return Ok(RootPreinstallDecision::CancelWon {
                cause: latched.ok_or(RootBundleInstallError::ProtocolMismatch)?,
            }),
        OWNER_OPEN => {}
        _ => return Err(RootBundleInstallError::ProtocolMismatch),
    }
    if let Some(stored) = control.preinstalled_root.get() {
        if stored.decision != decision
            || !same_terminal_delivery(&stored.delivery, &delivery)
            || stored.hard_stop.trigger.job() != hard_stop.trigger.job()
            || stored.hard_stop.fence_token != hard_stop.fence_token
            || !Arc::ptr_eq(&stored.hard_stop.permit, &hard_stop.permit)
        {
            return Err(RootBundleInstallError::ProtocolMismatch);
        }
    } else if control.preinstalled_root.set(PreinstalledExplicitRoot {
        delivery,
        decision,
        hard_stop: hard_stop.clone(),
    }).is_err() {
        return Err(RootBundleInstallError::ProtocolMismatch);
    }
    Ok(RootPreinstallDecision::Installed)
}

// Closed call site used by Idle, Poisoned, and drive_pending Root rows.  The
// caller has already retained attempt_id, request_id, reply, and any rollback
// cause, so every branch continues to own and eventually answer that waiter.
fn reduce_first_root_settle(
    registry: &TxRegistry,
    entry: &mut TxEntry,
    control: &Arc<ReservationControl>,
    attempt_id: RootSettleAttemptId,
    intent: RootIntent,
    delivery: TerminalDelivery,
) -> Result<Transition, TxProtocolError> {
    validate_terminal_sql_deadline_replacement(entry)?;
    let token = delivery.command_token();
    let decision = intent.decision();
    // Reserve already paid every route/capacity cost. This private binding
    // authenticates the selected decision/token against that dormant job; a
    // generation-death worker cannot select it until the atomic install below.
    let hard_stop = prepare_sc1_root_route_before_arm(
        entry, control, &intent, delivery.clone(), token,
    )?;
    match install_explicit_root_bundle(
        control, delivery.clone(), decision, &hard_stop,
    ) {
        Ok(RootPreinstallDecision::Installed) => {
            replace_execution_with_terminal_sql_infallible(entry, token);
            publish_terminal_route_armed(control);
            Ok(Transition {
                next: TxState::Settling {
                    token, attempt_id, intent, watchdog_fired: false,
                    hard_stop,
                },
                effects: vec![issue_root_with_current_lease(
                    control.id, token, decision, delivery,
                )],
            })
        }
        Ok(RootPreinstallDecision::CancelWon { cause }) => {
            consume_owned_permit_infallible(&hard_stop.permit);
            registry.supervisor.jobs.cancel_dormant_infallible(
                hard_stop.trigger.job(),
            );
            // Settle-before-owner-CAS gap: no root route/SQL is exposed. The
            // reserve-time Cancel route owns cleanup and the retained root
            // waiter observes that one immutable outcome.
            enter_cancelling_from_root_preinstall(entry, control, cause)
        }
        Ok(RootPreinstallDecision::FenceWon { cause }) => {
            consume_owned_permit_infallible(&hard_stop.permit);
            registry.supervisor.jobs.cancel_dormant_infallible(
                hard_stop.trigger.job(),
            );
            // Re-prove the already-unroutable generation under SC-1's own
            // incarnation-qualified retirement id. This is idempotent and
            // prevents a bare boolean from authorizing claim release.
            let outcome = cause.map(cleanup_outcome).unwrap_or_else(||
                TerminalOutcome::AbortedByBackend(
                    db_error("root_preinstall_generation_already_fenced"),
                ));
            Ok(begin_generation_retirement(entry, outcome))
        }
        Ok(RootPreinstallDecision::OtherCompletionWon)
        | Err(RootBundleInstallError::ProtocolMismatch) => {
            consume_owned_permit_infallible(&hard_stop.permit);
            registry.supervisor.jobs.cancel_dormant_infallible(
                hard_stop.trigger.job(),
            );
            Ok(begin_generation_retirement(
                entry,
                TerminalOutcome::AbortedByBackend(
                    terminal_route_mismatch_db_error(),
                ),
            ))
        }
    }
}

fn shared_explicit_route_matches(
    control: &ReservationControl,
    delivery: &TerminalDelivery,
) -> bool {
    match delivery {
        TerminalDelivery::ExplicitRoot { .. } =>
            control.preinstalled_root.get().is_some_and(|stored| {
                same_terminal_delivery(&stored.delivery, delivery)
            }),
        TerminalDelivery::ExplicitCancel { .. } =>
            same_terminal_delivery(&control.cancel_delivery, delivery),
        _ => false,
    }
}

// Called only after SC-1 has armed the matching shared deadline. Release makes
// the readiness test and descriptor/deadline writes visible before the actor
// is woken. Notifications coalesce; generation is a latch, not a queue slot.
fn publish_terminal_route_armed(control: &ReservationControl) {
    control.terminal_route_latch.generation
        .store(mint_never_reused_generation(), Release);
    control.terminal_route_latch.actor_wake.wake();
}

#[repr(u8)]
enum CancelPublishState {
    Empty,
    Publishing,
    Enqueued,
    ActorDead,
}

type AtomicCancelPublish = AtomicU8; // stores only CancelPublishState values
~~~

Only the actor changes OWNER_MASK. A caller may only publish CANCEL_INTENT with
fetch_or. Every actor owner CAS shares terminal_owner_gate with the supervisor's
death/hard-stop fence, so OWNER_OPEN cannot change after a fence snapshot:

~~~rust
enum TerminalBudgetArm {
    // SC-1 armed this exact shared state before enqueueing Settle/Cancel.
    SharedExplicit {
        slots: Arc<ExplicitDeadlineSlots>,
        expected_kind: DeadlineKind,
    },
    // Used only by autocommit and ExplicitDataAbort, whose terminalization is
    // discovered inside Execute rather than by an SC-1 state transition.
    ActorOwned {
        first_deadline: Instant,
        trigger: HardStopTrigger,
    },
    // Armed by the caller/supervisor before publishing autocommit CANCEL_INTENT.
    // The actor adopts this exact absolute budget; it never starts a new one.
    PrearmedAutocommitCancel {
        budget: Arc<PreclaimAutocommitCancelBudget>,
    },
}

// Caller holds terminal_owner_gate. Reserve preallocated both timer nodes, so
// installation cannot block or fail; lack of this capacity makes
// Reserve fail ActorSaturated before returning a handle.
fn arm_preclaim_autocommit_cancel_budget_locked(
    control: &ReservationControl,
) -> Arc<PreclaimAutocommitCancelBudget> {
    control.preclaim_autocommit_cancel_budget.get_or_init(|| {
        let ReservationCutoff::Autocommit(cutoff) = &control.terminal_cutoff
            else { unreachable!("explicit cancellation uses SC-1 deadline") };
        let first_deadline = Instant::now() + control.terminal_sql_timeout;
        let budget_id = mint_never_reused_terminal_delivery_id();
        let mut trigger = control.preclaim_autocommit_fence_trigger.get()
            .expect("Reserve registered the durable autocommit fence job")
            .clone();
        let HardStopTrigger::Autocommit {
            preclaim_cancel_budget_id, ..
        } = &mut trigger else {
            unreachable!("explicit cancellation uses SC-1 deadline")
        };
        *preclaim_cancel_budget_id = Some(budget_id);
        let budget = Arc::new(PreclaimAutocommitCancelBudget {
            budget_id,
            first_deadline,
            watchdog_generation: mint_never_reused_generation(),
            hard_stop_generation: mint_never_reused_generation(),
            trigger,
        });
        control.terminal_watchdog_armed.store(
            budget.watchdog_generation, Release,
        );
        control.terminal_hard_stop_armed.store(
            budget.hard_stop_generation, Release,
        );
        budget
    }).clone()
}

// Called under the same terminal_owner_gate only after CANCEL_INTENT and the
// command-gate force state are Release-visible. A callback can therefore never
// observe a scheduled preclaim budget beside bare OWNER_OPEN.
fn publish_preclaim_autocommit_timers_locked(
    control: &ReservationControl,
    budget: &Arc<PreclaimAutocommitCancelBudget>,
) {
    if !control.preclaim_timers_published.swap(true, AcqRel) {
        control.actor_control.schedule_preallocated_terminal_watchdog(
            &control.terminal_timer_permit,
            budget.first_deadline,
            control.id,
            budget.watchdog_generation,
        );
        control.actor_control.schedule_preallocated_hard_stop(
            &control.terminal_timer_permit,
            budget.first_deadline + control.terminal_interrupt_grace,
            control.id,
            control.actor_generation,
            budget.hard_stop_generation,
            budget.trigger.clone(),
        );
    }
}

enum TerminalBudgetCommit {
    AlreadyPublishedOrShared,
    PublishActorOwned {
        first_deadline: Instant,
        watchdog_generation: u64,
        hard_stop_generation: u64,
        trigger: HardStopTrigger,
    },
}

impl TerminalBudgetCommit {
    fn publish_after_owner_claim_locked(
        self,
        control: &ReservationControl,
    ) {
        if let TerminalBudgetCommit::PublishActorOwned {
            first_deadline, watchdog_generation, hard_stop_generation, trigger,
        } = self {
            // Reserve preallocated both nodes. These are infallible publications,
            // and terminal_owner_gate is still held after the owner CAS.
            control.actor_control.schedule_preallocated_terminal_watchdog(
                &control.terminal_timer_permit,
                first_deadline,
                control.id,
                watchdog_generation,
            );
            control.actor_control.schedule_preallocated_hard_stop(
                &control.terminal_timer_permit,
                first_deadline + control.terminal_interrupt_grace,
                control.id,
                control.actor_generation,
                hard_stop_generation,
                trigger,
            );
        }
    }
}

fn validate_or_stage_budget_locked(
    control: &ReservationControl,
    command_sequence: u64,
    budget: TerminalBudgetArm,
) -> Result<TerminalBudgetCommit, OwnerClaimError> {
    match budget {
        TerminalBudgetArm::SharedExplicit { slots, expected_kind } => {
            let stored = control.explicit_deadlines.as_ref()
                .ok_or(OwnerClaimError::ActorFenced)?;
            if !Arc::ptr_eq(&slots, stored) {
                return Err(OwnerClaimError::ActorFenced);
            }
            if !matches!(*slots.state.lock(),
                ExplicitDeadlineState::Armed { kind, .. }
                    if kind == expected_kind)
            {
                return Err(OwnerClaimError::TerminalRouteNotArmed);
            }
            control.current_terminal_sequence.store(command_sequence, Release);
            Ok(TerminalBudgetCommit::AlreadyPublishedOrShared)
        }
        TerminalBudgetArm::ActorOwned { first_deadline, trigger } => {
            let watchdog = mint_never_reused_generation();
            let hard_stop = mint_never_reused_generation();
            control.current_terminal_sequence.store(command_sequence, Release);
            control.terminal_watchdog_armed.store(watchdog, Release);
            control.terminal_hard_stop_armed.store(hard_stop, Release);
            Ok(TerminalBudgetCommit::PublishActorOwned {
                first_deadline,
                watchdog_generation: watchdog,
                hard_stop_generation: hard_stop,
                trigger,
            })
        }
        TerminalBudgetArm::PrearmedAutocommitCancel { budget } => {
            let stored = control.preclaim_autocommit_cancel_budget.get()
                .ok_or(OwnerClaimError::TerminalRouteNotArmed)?;
            let watchdog_state_ok =
                control.terminal_watchdog_armed.load(Acquire)
                    == budget.watchdog_generation
                || (control.preclaim_watchdog_fired.load(Acquire)
                    && control.terminal_watchdog_armed.load(Acquire) == 0);
            if !Arc::ptr_eq(stored, &budget)
                || !watchdog_state_ok
                || control.terminal_hard_stop_armed.load(Acquire)
                    != budget.hard_stop_generation
            {
                return Err(OwnerClaimError::ActorFenced);
            }
            control.current_terminal_sequence.store(command_sequence, Release);
            if control.preclaim_watchdog_fired.load(Acquire) {
                // The absolute first stage already expired. Retain that fact on
                // the newly adopted cleanup sequence; start-barrier/target
                // installation immediately observes and interrupts it. Only the
                // originally scheduled hard-stop remainder remains.
                control.terminal_interrupt_sequence.store(
                    command_sequence, Release,
                );
                control.terminal_interrupt_generation.store(
                    budget.watchdog_generation, Release,
                );
            }
            // Absolute first_deadline is retained; no timer is rescheduled.
            Ok(TerminalBudgetCommit::AlreadyPublishedOrShared)
        }
    }
}

fn actor_claim_completion(
    control: &ReservationControl,
    command_sequence: u64,
    attempt: TerminalAttempt,
    delivery: TerminalDelivery,
    delivery_id: TerminalDeliveryId,
    budget: TerminalBudgetArm,
) -> Result<(), OwnerClaimError> {
    // One unbroken critical section: slot -> command -> owner -> active_sql ->
    // budget. No terminal owner exists without attempt, delivery, and budget.
    let active_slot = control.active_command_gate.lock();
    let command = active_slot.as_ref().filter(|slot| {
        slot.command_sequence == command_sequence
    }).ok_or(OwnerClaimError::Lost { observed: OWNER_OPEN })?;
    let mut command_state = command.gate.state.lock();
    let _owner = control.terminal_owner_gate.lock();
    let _active = control.actor_control.active_sql.lock();
    if control.generation_fenced.load(Acquire)
        || !matches!(*command_state, CommandGateState::Open)
        || control.terminal.load(Acquire) != OWNER_OPEN
        || !delivery_matches_control(
            control, &delivery, command.data_delivery_permit.as_ref(),
        )
    {
        return Err(OwnerClaimError::Lost {
            observed: control.terminal.load(Acquire),
        });
    }
    match &budget {
        TerminalBudgetArm::SharedExplicit { .. } => {
            if !shared_explicit_route_matches(control, &delivery) {
                return Err(OwnerClaimError::TerminalRouteNotArmed);
            }
        }
        TerminalBudgetArm::ActorOwned { .. } => {
            let mut installed = control.terminal_delivery.lock();
            if installed.as_ref().is_some_and(|stored| {
                !same_terminal_delivery(stored, &delivery)
            }) {
                return Err(OwnerClaimError::ActorFenced);
            }
            *installed = Some(delivery.clone());
        }
        TerminalBudgetArm::PrearmedAutocommitCancel { .. } =>
            return Err(OwnerClaimError::ActorFenced),
    }
    let budget_commit =
        validate_or_stage_budget_locked(control, command_sequence, budget)?;
    *control.terminal_attempt.lock() = Some(attempt);
    *control.terminal_delivery.lock() = Some(delivery);
    control.terminal.compare_exchange(
        OWNER_OPEN, OWNER_COMPLETE, AcqRel, Acquire,
    ).expect("all owner writers hold terminal_owner_gate");
    *command_state = CommandGateState::CompletionPromised { delivery_id };
    budget_commit.publish_after_owner_claim_locked(control);
    Ok(())
}

fn actor_claim_cancellation(
    control: &ReservationControl,
    command_sequence: Option<u64>,
    cause: CancelCause,
    budget: TerminalBudgetArm,
) -> Result<CancelPhaseProof, OwnerClaimError> {
    let mut active_slot = control.active_command_gate.lock();
    let mut command_state = active_slot.as_ref().map(|slot| slot.gate.state.lock());
    if command_sequence.is_some_and(|seq| {
        active_slot.as_ref().map(|slot| slot.command_sequence) != Some(seq)
    }) {
        return Err(OwnerClaimError::Lost {
            observed: control.terminal.load(Acquire),
        });
    }
    let _owner = control.terminal_owner_gate.lock();
    let active = control.actor_control.active_sql.lock();
    let expected = OWNER_OPEN | CANCEL_INTENT;
    let delivery = cancel_delivery_for_control(control)?;
    if control.generation_fenced.load(Acquire)
        || control.terminal.load(Acquire) != expected
        || command_state.as_deref().is_some_and(|state| !matches!(state,
            CommandGateState::ForceQueued { .. }
            | CommandGateState::ForceAfterNonterminalCompletionQueued { .. }))
        || !delivery_matches_control(control, &delivery, None)
    {
        return Err(OwnerClaimError::Lost {
            observed: control.terminal.load(Acquire),
        });
    }
    let finalized = control.finalized_cancel_proof.lock();
    let phase_proof = match (
        control.phase.load(Acquire),
        active.as_ref().map(|target| &target.statement),
    ) {
        (ActorPhase::Queued | ActorPhase::Preparing
            | ActorPhase::BeginNotOpened, _) =>
                CancelPhaseProof::NoTransactionPossible,
        // The prepare read is on op_conn before BEGIN. OperationAuthority is
        // deliberately distinct because it runs while the explicit tx is open.
        (_, Some(SqlStatementClass::PrepareAuthority)) =>
                CancelPhaseProof::NoTransactionPossible,
        (_, Some(SqlStatementClass::Begin)) | (ActorPhase::Beginning, _) =>
                CancelPhaseProof::BeginMayHaveOpened,
        (_, None) if command_sequence.is_some_and(|sequence| {
            finalized.as_ref().is_some_and(|proof| {
                proof.cancellation_sequence == sequence
            })
        }) => finalized.as_ref().unwrap().phase,
        _ => CancelPhaseProof::TransactionMayExist,
    };
    // Even a queued/no-SQL cancellation gets a nonzero terminal sequence. Its
    // first-stage interrupt may find no ActiveSqlTarget (a valid no-op), while
    // the independently armed hard stop still bounds actor retirement.
    let terminal_sequence = command_sequence
        .unwrap_or_else(mint_never_reused_command_sequence);
    match &budget {
        TerminalBudgetArm::SharedExplicit { .. } => {
            if !shared_explicit_route_matches(control, &delivery) {
                // Keep Cancel pending and execute no cleanup SQL. SC-1 wakes the
                // control lane after preinstall + deadline arm complete.
                return Err(OwnerClaimError::TerminalRouteNotArmed);
            }
        }
        TerminalBudgetArm::ActorOwned { .. } => {
            let mut installed = control.terminal_delivery.lock();
            if installed.as_ref().is_some_and(|stored| {
                !same_terminal_delivery(stored, &delivery)
            }) {
                return Err(OwnerClaimError::ActorFenced);
            }
            *installed = Some(delivery.clone());
        }
        TerminalBudgetArm::PrearmedAutocommitCancel { budget } => {
            let stored = control.preclaim_autocommit_cancel_budget.get()
                .ok_or(OwnerClaimError::TerminalRouteNotArmed)?;
            if !Arc::ptr_eq(stored, budget)
                || !matches!(delivery, TerminalDelivery::Autocommit { .. })
            {
                return Err(OwnerClaimError::ActorFenced);
            }
            // Common code below installs the same reserve-time delivery with
            // OWNER_CANCEL; validate_or_arm adopts the absolute timers.
        }
    }
    let budget_commit =
        validate_or_stage_budget_locked(control, terminal_sequence, budget)?;
    let predecessor_sequence = command_sequence.filter(|sequence|
        finalized.as_ref().is_some_and(|proof|
            proof.cancellation_sequence == *sequence));
    *control.terminal_attempt.lock() = Some(TerminalAttempt::Cancellation {
        cause,
        phase_proof,
        predecessor_sequence,
    });
    *control.terminal_delivery.lock() = Some(delivery);
    control.terminal.compare_exchange(
        expected, OWNER_CANCEL | CANCEL_INTENT, AcqRel, Acquire,
    ).expect("all owner writers hold terminal_owner_gate");
    let preserve_retained_completion = command_state.as_deref().is_some_and(|s|
        matches!(s,
            CommandGateState::ForceAfterNonterminalCompletionQueued { .. })
    );
    drop(command_state);
    if !preserve_retained_completion {
        *active_slot = None; // only after attempt/delivery/owner are durable
    }
    budget_commit.publish_after_owner_claim_locked(control);
    // ForceAfter keeps the exact slot until SC-1 consumes the already-keyed
    // completion. CANCEL_INTENT forbids a next command; terminal retirement
    // drops the retained slot after the later force/cancellation proof.
    Ok(phase_proof)
}
~~~

Autocommit completion uses ActorOwned with `now + terminal_sql_timeout`.
Autocommit cancellation instead adopts PrearmedAutocommitCancel, installed
before CANCEL_INTENT and never extended by actor delay. ExplicitDataAbort
uses ActorOwned with
`min(execution_deadline_at, now + terminal_sql_timeout)` and its data trigger.
Ordinary explicit root/cancel uses SharedExplicit with the pointer-identical
SC-1 deadline Arc and expected TerminalSql/CancellationSql kind. The combined
claim functions validate and stage the budget before their owner CAS, then
publish the preallocated timer nodes inside the same owner-gate critical
section after attempt, delivery, owner, and command promise are visible. There
is neither an owner-without-watchdog state nor a callback that can observe bare
Open. A real proof is first retained by its cutoff
and successfully keyed-enqueued (or loses to Fence); only then may the actor
disarm an actor-owned budget. A shared explicit deadline is disarmed only by
SC-1 after reducing the keyed completion. Any adapter/enqueue error leaves the
budget armed so hard-stop recovery can retry the retained Result or fence it.

An explicit Settle treats `TerminalRouteNotArmed` as a distinct pre-claim
terminal state, not a generic error. It verifies that the pointer-identical
preinstalled root delivery/decision exist and that the shared deadline is
already Fired(TerminalSql) or Armed/Fired(TerminalHardStop), stores the exact
command in `pending_root_settle`, and issues **no** COMMIT/ROLLBACK. The SC-1
hard-stop task remains responsible for the cutoff. After a physical fence it
uses `preinstalled_root.decision` and publishes
RootFinishResult::Failed(Indeterminate, terminal_deadline_exceeded) for either
decision. Both travel through the
preinstalled root cutoff as keyed TerminalCompleted and store/wake
`control.retention.outcome`; the pending one-shot is only a fast path. A different pending
decision/delivery is ActorFenced. This is the sequence-zero settle timeout arm.

The cancellation publisher also holds `terminal_owner_gate` around its fetch_or
(shown below), so after the explicit equality check the actor CAS cannot lose.
An actor candidate that observes the other ordering returns Lost before writing
attempt or delivery. The actor performs exactly one successful owner CAS, and a
death snapshot can never see an owner without its matching attempt/delivery.

These operations share one modification order:

* caller fetch_or happens first: completion’s expected OWNER_OPEN does not
  match; the actor takes OWNER_CANCEL and rolls back;
* actor’s OWNER_COMPLETE CAS happens first: caller observes completion ownership,
  does not interrupt, and Cancel waits for AlreadyCompleted(outcome);
* the actor never succeeds at both CAS operations, and no caller can succeed at
  either.

There is no exception CAS from Open+Intent to Complete. If CANCEL_INTENT was
published first, cancellation owns the public outcome even when the statement
returns a racing corruption, I/O, or constraint error rather than INTERRUPT;
the actor records that engine error as diagnostic metadata and still performs
cleanup. If the actor accepts the ordinary engine result and CASes Open to
Complete first, that real success/error is the public outcome and the later
Cancel receives AlreadyCompleted. This preserves one modification-order rule
instead of deciding the same race differently by error class.

That paragraph concerns a terminal candidate: an autocommit operation, Settle,
or an engine result that proves the explicit transaction ended. A normal
nonterminal Execute in an explicit transaction does not CAS reservation owner;
its per-command completion gate orders the Execute reply against Cancel, and a
later cancellation still rolls back the open reservation.

Completion ownership is taken after an autocommit statement is finalized and
immediately before its success COMMIT or error cleanup; explicit Settle takes
it immediately before COMMIT or ROLLBACK. It is never taken after reply delivery.
Once it is taken,
cancellation does not call interrupt on that connection; the Cancel reply waits
for terminal SQL’s real outcome. One `Arc<ActorTerminalOutcome>` is stored before
an autocommit operation reply or explicit Settle reply is sent; the original
reply and every late Cancel receive clones of that Arc, so an owned DbResult is
never duplicated or moved out of the terminal record. A successful nonterminal
Execute in an explicit transaction instead restores phase Idle before sending
its reply; dropping that reply before it is polled can still cancel and roll
back the open reservation. Reply send, reply poll, and future Drop are not
terminal linearization points.

OWNER_MASK chooses Cancel versus Complete; TerminalCutoffGate separately
chooses a real terminal result versus SC-1's hard fence. Immediately after
terminal SQL returns, the actor consumes the statement-class-specific private
end capability, prepares an infallible delivery, and calls
`publish_prepared_result` on the selected ReservationCutoff. The gate first
stores Result, then commits the immutable retention cell, then keyed-enqueues
the completion while still holding the gate. If FencePending/FenceResult
already won, the method returns false; the actor suppresses that ordinary
result, drops the generation, and lets the registered supervisor job publish
the sole fenced result. Neither cutoff choice changes OWNER_MASK.

The result publisher's critical path is also fixed, not implicit:

~~~text
terminal SQL/cleanup returns and statement is finalized
classify raw result + is_autocommit into the immutable typed proof
lock active_sql; clear only the matching ActiveSqlTarget
lock the command gate (it must be CompletionPromised or ForceQueued for Cancel)
prepare the typed PreparedTerminalDelivery before cutoff arbitration
  preparation validates projection, pins/reserves the exact endpoint, and
  captures the independent retention cell; any failure occurs while Open
call the selected cutoff.publish_prepared_result(prepared)
  gate stores Result(delivery_enqueued=false)
  gate idempotently commits Arc<ActorTerminalOutcome> in the retention OnceLock
  gate infallibly keyed-enqueues the exact SC-1 event or autocommit reply
  gate changes delivery_enqueued=true before releasing its mutex
if cutoff returned false, FencePending/FenceResult already won: store no
  competing outcome, close this generation, and let the durable job finish
after successful keyed insertion, clear current_terminal_sequence, the exact
  terminal-interrupt generation/sequence, and actor-owned timer generations;
  shared explicit generations remain for SC-1 to disarm while reducing
only after successful keyed insertion wake Settle/Execute/late-Cancel waiters
~~~

Delivery is recovery-safe: `fence_or_ensure_result_delivery` re-runs
`insert_and_commit` if it observes Result with `delivery_enqueued=false`; both
operations are idempotent and the endpoint was reserved before arbitration.
For explicit root, `root_proof + TerminalAttempt` derives the shared actor
outcome; for explicit cancellation `CancelAck + latched cause` does so;
autocommit already carries the full outcome. No reply sender owns the only copy
of a DbResult.

The one-shot `reply` field in Execute/Settle is only the fast path. Every such
future is outcome-aware and registers its real waker in ReservationControl, so
an actor death cannot leave a receiver pending merely because no actor remains
to send the one-shot:

~~~rust
enum TerminalProjection {
    ExecuteAutocommit,
    ExecuteExplicit { data_token: CommandToken },
    SettleExplicit,
}

enum ProjectedReply {
    Execute(Result<SharedDbResult, DbError>),
    Settle(Result<SharedActorTerminalOutcome, ActorError>),
}

fn project_terminal_outcome(
    control: &ReservationControl,
    projection: &TerminalProjection,
    outcome: SharedActorTerminalOutcome,
) -> Result<ProjectedReply, ActorError> {
    match (projection, outcome.as_ref()) {
        (TerminalProjection::ExecuteAutocommit,
         ActorTerminalOutcome::Autocommit(result)) =>
            Ok(ProjectedReply::Execute(result.clone())),
        (TerminalProjection::ExecuteAutocommit,
         ActorTerminalOutcome::AutocommitIndeterminate(error)
         | ActorTerminalOutcome::CleanupIndeterminate(error)
         | ActorTerminalOutcome::ActorUnavailable(error)) =>
            Ok(ProjectedReply::Execute(Err(error.clone()))),
        (TerminalProjection::ExecuteAutocommit,
         ActorTerminalOutcome::Cancelled { cause, .. }) =>
            Ok(ProjectedReply::Execute(Err(cancelled_db_error(cause.clone())))),
        (TerminalProjection::ExecuteExplicit { data_token },
         ActorTerminalOutcome::TransactionAborted(error))
            if matches!(control.terminal_delivery.lock().as_ref(),
                Some(TerminalDelivery::ExplicitDataAbort {
                    data_token: stored, ..
                }) if stored == data_token) =>
            Ok(ProjectedReply::Execute(Err(error.clone()))),
        (TerminalProjection::ExecuteExplicit { .. },
         ActorTerminalOutcome::Cancelled { cause, .. }) =>
            Ok(ProjectedReply::Execute(Err(cancelled_db_error(cause.clone())))),
        (TerminalProjection::ExecuteExplicit { .. },
         ActorTerminalOutcome::CleanupIndeterminate(error)
         | ActorTerminalOutcome::ActorUnavailable(error)) =>
            Ok(ProjectedReply::Execute(Err(error.clone()))),
        (TerminalProjection::SettleExplicit, _) =>
            Ok(ProjectedReply::Settle(Ok(outcome))),
        _ => Err(ActorError::TerminalReplyProjectionMismatch),
    }
}

fn poll_outcome_fallback(
    control: &ReservationControl,
    projection: &TerminalProjection,
    cx: &mut Context<'_>,
) -> Poll<Result<ProjectedReply, ActorError>> {
    if let Some(outcome) = control.retention.outcome.get() {
        return Poll::Ready(project_terminal_outcome(
            control, projection, outcome.clone(),
        ));
    }
    let mut waiters = control.retention.terminal_waiters.lock();
    if let Some(outcome) = control.retention.outcome.get() {
        drop(waiters);
        return Poll::Ready(project_terminal_outcome(
            control, projection, outcome.clone(),
        ));
    }
    if !waiters.iter().any(|w| w.will_wake(cx.waker())) {
        waiters.push(cx.waker().clone());
    }
    Poll::Pending
}

struct OutcomeAwareCommandFuture<R> {
    reply_rx: OneshotReceiver<Result<R, SenderDropped>>,
    reply_fast_path_closed: bool, // initialized false
    control: Arc<ReservationControl>,
    projection: TerminalProjection,
    cancel_guard: ReservationCancelGuard,
    terminal_receipt: TerminalReceipt,
}

impl<R> Future for OutcomeAwareCommandFuture<R> {
    type Output = R;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<R> {
        if !self.reply_fast_path_closed {
            match self.reply_rx.poll_unpin(cx) {
                Poll::Ready(Ok(reply)) => {
                    self.terminal_receipt.observe_fast_path(&reply);
                    self.cancel_guard.disarm();
                    return Poll::Ready(reply);
                }
                Poll::Ready(Err(SenderDropped)) => {
                    // Actor death closes only the optimization. The retained
                    // cutoff outcome remains the authoritative completion.
                    self.reply_fast_path_closed = true;
                }
                Poll::Pending => {}
            }
        }
        if let Poll::Ready(projected) = poll_outcome_fallback(
            &self.control, &self.projection, cx,
        ) {
            let reply = self.projection.downcast(projected);
            self.terminal_receipt.observe_fallback(&reply);
            self.cancel_guard.disarm();
            return Poll::Ready(reply);
        }
        // Close reply-send versus waiter-registration: a fast reply that raced
        // the fallback registration is observed without relying on a wake.
        if !self.reply_fast_path_closed {
            match self.reply_rx.poll_unpin(cx) {
                Poll::Ready(Ok(reply)) => {
                    self.terminal_receipt.observe_fast_path(&reply);
                    self.cancel_guard.disarm();
                    return Poll::Ready(reply);
                }
                Poll::Ready(Err(SenderDropped)) => {
                    self.reply_fast_path_closed = true;
                    // poll_outcome_fallback already registered this waker.
                }
                Poll::Pending => {}
            }
        }
        Poll::Pending
    }
}
~~~

`TerminalReplyProjectionMismatch` is a protocol fault: it fences the generation
and returns no invented success. Normal and hard-stop prepared deliveries first
commit the identical shared outcome, then keyed-enqueue/wake consumers after
ordinary completion or physical fencing. A future therefore completes from
either the actor's fast reply or the retained terminal record, and the receipt
asserts equality if tests deliberately make both ready.

## 14. Explicit Cancel publication and the start-gap fence

Reservation identity and cancellation control exist before the actor can run
authority, BEGIN, or data SQL. The synchronous front end performs:

~~~rust
struct ReserveSpec {
    app: AppAuthority,
    tx_key: Option<TxKey>,
    admission: Option<AdmissionLeaseProof>, // Some for explicit transaction
    resolved_epoch: SchemaEpoch,
    execution_deadline_at: Option<Instant>, // Some for admitted explicit tx
    explicit_deadlines: Option<Arc<ExplicitDeadlineSlots>>,
    cancellation_token: Option<CommandToken>,
    autocommit_sink: Option<SharedActorTerminalSink>,
    terminal_sql_timeout: Duration,
    terminal_interrupt_grace: Duration,
    kind: ReservationKind,
    cutoff: ReservationCutoff,
}

fn reserve(spec: ReserveSpec) -> Result<ReservationHandle, ActorError> {
    let actor = supervisor.actor_exact(spec.app)?;
    let permit = actor.control_capacity.try_acquire_owned()
        .ok_or(ActorError::ActorSaturated)?;
    // One permit owns the preallocated watchdog and hard-stop nodes. Reserve
    // fails synchronously rather than returning a cancel handle whose deadline
    // could later fail to arm.
    let terminal_timer_permit = actor.terminal_timer_capacity
        .try_acquire_pair_owned()
        .ok_or(ActorError::ActorSaturated)?;
    let id = ReservationId {
        app: spec.app,
        nonce: supervisor.secure_monotonic_nonce(),
    };
    let (ready_tx, ready_rx) = oneshot();
    if spec.tx_key.as_ref().is_some_and(|key| key.app != spec.app) {
        return Err(ActorError::ForeignReservation);
    }
    match (&spec.kind, &spec.tx_key, &spec.cutoff) {
        (ReservationKind::Transaction, Some(key),
         ReservationCutoff::Explicit { root, cancel }) => {
            let admission = spec.admission.as_ref()
                .ok_or(ActorError::ForeignReservation)?;
            let entry = tx_registry.entry_exact(key)
                .ok_or(ActorError::ForeignReservation)?;
            if !entry.claim.as_ref().is_some_and(|claim| {
                claim.proves(admission, &entry.admission)
            }) {
                return Err(ActorError::ForeignReservation);
            }
            if !Arc::ptr_eq(root, entry.terminal_cutoff.as_ref().unwrap())
                || !Arc::ptr_eq(cancel, entry.cancel_cutoff.as_ref().unwrap())
                || !spec.explicit_deadlines.as_ref().is_some_and(|slots| {
                    Arc::ptr_eq(slots, &entry.deadline_slots)
                })
                || spec.cancellation_token != Some(entry.cancellation_token)
            {
                return Err(ActorError::ForeignReservation);
            }
        }
        (ReservationKind::Autocommit, None,
         ReservationCutoff::Autocommit(_))
            if spec.admission.is_none()
                && spec.explicit_deadlines.is_none()
                && spec.cancellation_token.is_none()
                && spec.autocommit_sink.is_some() => {}
        _ => return Err(ActorError::ForeignReservation),
    }
    let explicit_failure_sink = spec.tx_key.clone().map(|key| ExplicitFailureSink {
        key,
        cancellation_token: spec.cancellation_token.unwrap(),
        registry: tx_registry.sender(),
    });
    let cancel_delivery = match (&spec.cutoff, explicit_failure_sink.as_ref()) {
        (ReservationCutoff::Explicit { cancel, .. }, Some(sink)) =>
            TerminalDelivery::ExplicitCancel {
                key: sink.key.clone(),
                cutoff: cancel.clone(),
                token: sink.cancellation_token,
                registry: sink.registry.clone(),
            },
        (ReservationCutoff::Autocommit(_), None) =>
            TerminalDelivery::Autocommit {
                sink: spec.autocommit_sink.clone().unwrap(),
            },
        _ => unreachable!("validated reservation shape"),
    };
    let autocommit_fence_inputs = match (
        &spec.cutoff, spec.autocommit_sink.as_ref(),
    ) {
        (ReservationCutoff::Autocommit(cutoff), Some(sink)) =>
            Some((cutoff.clone(), sink.clone())),
        _ => None,
    };
    let control = Arc::new(ReservationControl::queued(
        id,
        spec.kind,
        spec.tx_key.clone(), // Some only for an explicit SC-1 reservation
        spec.cutoff, // exact shared Arc(s), never reconstructed from TxKey
        spec.resolved_epoch,
        actor.actor_generation,
        spec.execution_deadline_at,
        spec.explicit_deadlines,
        spec.terminal_sql_timeout,
        spec.terminal_interrupt_grace,
        explicit_failure_sink,
        cancel_delivery,
        actor.generation_fenced.clone(),
        actor.generation_owner_gate.clone(),
        permit,
        terminal_timer_permit,
    ));
    if let Some((cutoff, sink)) = autocommit_fence_inputs {
        // The control is still private. Failure drops it and its permits; no
        // handle, timer, command, or index entry can observe a missing job.
        install_autocommit_fence_trigger_before_publish(
            &supervisor, &control, cutoff, sink,
        )?;
    }
    if matches!(&control.terminal_cutoff, ReservationCutoff::Explicit { .. }) {
        // Root settlement and Cancel can become visible immediately after the
        // handle/control. Install both independent routes first, and roll back
        // the first job if preparation of the second fails.
        if let Err(error) =
            install_explicit_root_fence_before_publish(&supervisor, &control)
        {
            cancel_unpublished_reservation_fences_infallible(
                &supervisor, &control,
            );
            return Err(error);
        }
        if let Err(error) =
            install_explicit_cancel_fence_before_publish(&supervisor, &control)
        {
            cancel_unpublished_reservation_fences_infallible(
                &supervisor, &control,
            );
            return Err(error);
        }
    }
    if let Err(error) = actor.control_index.insert_exact(id, control.clone()) {
        cancel_unpublished_reservation_fences_infallible(&supervisor, &control);
        return Err(error);
    }
    if actor.data_sender.try_send(ActorCommand::Reserve {
        id,
        resolved_epoch: spec.resolved_epoch,
        kind: spec.kind,
        reply: ready_tx,
    }).is_err() {
        actor.control_index.remove_exact(id);
        cancel_unpublished_reservation_fences_infallible(&supervisor, &control);
        return Err(ActorError::ActorUnavailable);
    }
    Ok(ReservationHandle::armed(id, control, ready_rx))
}
~~~

The SC-1 Create reducer mints its root and cancel gates with distinct
TerminalDeliveryIds, but it does not call `reserve` while WaitingAdmission.
Only AdmissionGranted supplies `AdmissionLeaseProof` and invokes this function;
the actor cannot read authority or issue BEGIN before the ClaimGuard exists. An autocommit
front end mints one fresh ActorTerminalOutcome gate. `ReservationControl::queued`
stores the supplied value verbatim. Pointer equality above is therefore an
executable invariant: an explicit actor can neither substitute an independent
cutoff nor publish a terminal result into another transaction's gate.

Every explicit terminal route has one construction site and one fixed order:

| Route | Descriptor construction and publication order |
| --- | --- |
| ExplicitRoot | Reserve pins the root endpoint/mailbox and registers its distinct dormant job before publishing ReservationControl. The SC-1 SettleRoot reducer binds the chosen decision and terminal token to that job, then under the owner gate atomically installs `PreinstalledExplicitRoot{delivery,decision,hard_stop}`. Only Installed stores the bundle in Settling, arms TerminalSql, and enqueues ActorCommand::Settle. Cancel/Fence wins cancel the unused root job and issue no root SQL. |
| ExplicitCancel | While ReservationControl is still private, Reserve constructs immutable cancel_delivery, pins both endpoints, registers its dormant supervisor job, stores `PreinstalledExplicitCancelFence`, and only then publishes the control. Entry to Cancelling latches CANCEL_INTENT and `create_explicit_cancel_hard_stop` binds that exact cause/deadline to the already-registered job without allocation or capacity acquisition; it stores the permit before arming CancellationSql/HardStop and calling publish_terminal_route_armed. An earlier Cancel remains in pending_cancel and executes no SQL until this succeeds. |
| ExplicitDataAbort | Before Idle-to-InFlight becomes visible, Registry calls `create_data_abort_delivery`. It retains the returned permit Arc and exact registered trigger in ActiveAction, ActiveCommandRef, TerminalDelivery, and ActorCommand::Execute. The actor adopts that same trigger if the exact Execute proves an auto-rollback/BUSY_SNAPSHOT terminal abort; it never reconstructs one after SQL. |
| Autocommit | The front end creates the sink/cutoff and calls `install_autocommit_fence_trigger_before_publish` while ReservationControl is private. Only then does `reserve` insert the control or enqueue Reserve. Completion/cancellation clones that registered trigger and only changes the preclaim budget id. |

`publish_terminal_route_armed` is only a control-lane readiness notification; it
does not claim an owner and contains no mutable descriptor. A pending Cancel
re-runs the combined claim after observing the pointer-identical delivery and
Armed(CancellationSql) state. Timer publication can race after the arm, but by
then the descriptor is already recoverable. Thus a shared first-stage timeout
may legally observe sequence zero, while the corresponding hard stop can still
validate the route and terminalize it.

ReservationHandle is returned already armed with id and cancel control; waiting
for ready_rx is not what creates it. The actor's Reserve arm only validates the
qualified id/admission proof, records the reservation, checks CANCEL_INTENT, and
publishes control readiness; it performs no authority read and no BEGIN. The
SC-1 Preparing command then sends ActorCommand::Prepare, which performs the
platform-role authority read and returns PreparedReservation. SC-1 Starting
sends ActorCommand::Begin exactly once; that arm runs BEGIN and the first
snapshot-marker read, returning OpenedReservation. Prepare and Begin each check
CANCEL_INTENT at their pre-start barrier. If sending readiness finds the receiver gone, the actor invokes
publish_cancel with CallerDrop and cleans an already-open transaction before
retiring (normally it joins the cancellation already published by the handle's
Drop). Thus cancellation while Reserve is queued, executing, or completed but
unpolled always names the same reservation and cannot leak a BEGIN/admission
claim.

There are two actor inputs: the bounded data lane and a separately reserved
cancellation/control lane. Reserve—including a queued admission reservation—
must acquire and embed one OwnedControlPermit before returning a handle; if the
configured maximum of live controls has no permit, Reserve returns
ActorSaturated. The permit is retained through terminal-record Forget and can
deliver exactly one Cancel without waiting for data-lane capacity. Duplicate
explicit callers join the shared terminal waiter. The actor drains the control
lane before starting queued data and between statements. A synchronous SQLite
call can still block the actor thread, so the shared intent and the exact
ActiveSqlTarget are the in-statement path; putting Cancel behind Execute on the
ordinary FIFO would reproduce today’s run-to-completion loop
(crates/zeroship-data-v8/src/backend/sqlite/session.rs:403-445).

Dropping an armed CommandFuture or calling cancel().await executes this exact
publication sequence. The route is part of the validated handle shape; an
autocommit reservation has no SC-1 TxEntry and therefore cannot accidentally
address a registry bridge:

~~~rust
enum ForceRoute {
    Explicit(Sc1ForceBridge), // contains the exact incarnation-qualified TxKey
    Autocommit,
}

struct ReservationCancelHandle {
    id: ReservationId,
    control: Arc<ReservationControl>,
    route: ForceRoute,
    terminal_waiter: TerminalWaiterRegistration,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ForceEventDelivery { Enqueue, InlineReducer }

enum ForceArbitration {
    Enqueued(CancelOrder),
    // Reducer applies the normal force arm now. Inline Joined is converted to
    // this form with the immutable first cause because the earlier external
    // force event may be behind the currently reducing timer event.
    InlineApplyForce { cause: ForcedReason },
    // A nonterminal completion is already keyed behind the timer currently
    // being reduced. The force has now been keyed after that completion and
    // must be applied only after the completion restores the next state.
    InlineDeferredAfterNonterminal,
    // Terminal completion/fence already owns reality. Leave the claimed timer
    // Fired until the keyed proof disarms it.
    InlineObserver(CancelOrder),
}

fn arbitrate_force(
    handle: &ReservationCancelHandle,
    cause: CancelCause,
    delivery: ForceEventDelivery,
) -> ForceArbitration {
    // Required global order: active-command slot -> its gate -> owner gate ->
    // active_sql. Holding the slot prevents an old Arc snapshot from being
    // mistaken for a newly installed command. Cause publication is a OnceLock,
    // not a mutex in this order.
    let active_slot = handle.control.active_command_gate.lock();
    let mut command_state = active_slot.as_ref().map(|slot| slot.gate.state.lock());
    let cancel_command_sequence =
        active_slot.as_ref().map(|slot| slot.command_sequence);
    let latched_cause = handle.control.cancel_cause.get_or_init(|| cause).clone();
    let order = {
        let _owner_gate = handle.control.terminal_owner_gate.lock();
        let word = handle.control.terminal.load(Acquire);
        let decided = if handle.control.generation_fenced.load(Acquire) {
            CancelOrder::TerminalFenceWon
        } else {
          match (owner(word), command_state.as_deref_mut()) {
            (OWNER_COMPLETE, Some(CommandGateState::CompletionPromised { .. })) =>
                CancelOrder::TerminalCompletionWon,
            // Preserve retained-result ordering even if the actor has already
            // claimed cancellation. ForceAfter stays in the slot until the
            // keyed nonterminal completion is consumed.
            (OWNER_CANCEL,
             Some(CommandGateState::ForceAfterNonterminalCompletionQueued { .. })) =>
                CancelOrder::AfterNonterminalCompletion,
            (OWNER_CANCEL, _)
            | (_, Some(CommandGateState::ForceQueued { .. })) =>
                CancelOrder::Joined,
            (OWNER_OPEN, Some(state @ CommandGateState::Open)) => {
                let preclaim = handle.control.explicit_deadlines.is_none().then(||
                    arm_preclaim_autocommit_cancel_budget_locked(&handle.control)
                );
                handle.control.terminal.fetch_or(CANCEL_INTENT, AcqRel);
                *state = CommandGateState::ForceQueued {
                    cause: cleanup_cause(latched_cause.clone()),
                };
                if let Some(budget) = preclaim.as_ref() {
                    publish_preclaim_autocommit_timers_locked(
                        &handle.control, budget,
                    );
                }
                CancelOrder::ForceWon
            }
            (OWNER_OPEN, Some(CommandGateState::NonterminalCompletionQueued)) => {
                let preclaim = handle.control.explicit_deadlines.is_none().then(||
                    arm_preclaim_autocommit_cancel_budget_locked(&handle.control)
                );
                handle.control.terminal.fetch_or(CANCEL_INTENT, AcqRel);
                *command_state.as_deref_mut().unwrap() =
                    CommandGateState::ForceAfterNonterminalCompletionQueued {
                        cause: cleanup_cause(latched_cause.clone()),
                    };
                if let Some(budget) = preclaim.as_ref() {
                    publish_preclaim_autocommit_timers_locked(
                        &handle.control, budget,
                    );
                }
                CancelOrder::AfterNonterminalCompletion
            }
            (OWNER_OPEN, None) => {
                let preclaim = handle.control.explicit_deadlines.is_none().then(||
                    arm_preclaim_autocommit_cancel_budget_locked(&handle.control)
                );
                handle.control.terminal.fetch_or(CANCEL_INTENT, AcqRel);
                if let Some(budget) = preclaim.as_ref() {
                    publish_preclaim_autocommit_timers_locked(
                        &handle.control, budget,
                    );
                }
                CancelOrder::ForceWon
            }
            _ => protocol_fault_cancel_order(),
          }
        };
        let arbitration = if delivery == ForceEventDelivery::Enqueue {
            // This is keyed mailbox insertion only: nonblocking, allocation
            // pre-reserved with the control, and it never runs the reducer.
            // It must precede release of terminal_owner_gate for every order,
            // so a supervisor/result and any publisher-specific audit cannot
            // overtake the authenticated observer/force decision.
            if let ForceRoute::Explicit(bridge) = &handle.route {
                bridge.enqueue_sc1_force_in_gate_order(
                    forced_reason(latched_cause.clone()), decided,
                );
            }
            ForceArbitration::Enqueued(decided)
        } else if !matches!(handle.route, ForceRoute::Explicit(_)) {
            protocol_fault("InlineReducer exists only under an SC-1 reducer");
        } else {
            match decided {
                CancelOrder::ForceWon | CancelOrder::Joined =>
                    ForceArbitration::InlineApplyForce {
                        cause: forced_reason(latched_cause.clone()),
                    },
                CancelOrder::AfterNonterminalCompletion => {
                    // The ordinary completion was keyed before this mutation.
                    // Key the force now, still under owner/command ordering; it
                    // is necessarily after that completion even though the
                    // currently reducing DeadlineFired was before both.
                    let ForceRoute::Explicit(bridge) = &handle.route else {
                        unreachable!()
                    };
                    bridge.enqueue_sc1_force_in_gate_order(
                        forced_reason(latched_cause.clone()), decided,
                    );
                    ForceArbitration::InlineDeferredAfterNonterminal
                }
                CancelOrder::TerminalCompletionWon
                | CancelOrder::TerminalFenceWon =>
                    ForceArbitration::InlineObserver(decided),
                CancelOrder::PreAdmission => unreachable!(),
            }
        };
        (decided, arbitration)
    };

    match order.0 {
        CancelOrder::ForceWon | CancelOrder::AfterNonterminalCompletion => {},
        CancelOrder::TerminalCompletionWon
        | CancelOrder::TerminalFenceWon
        | CancelOrder::Joined =>
            handle.terminal_waiter.join_terminal_or_cancel_waiter(),
        CancelOrder::PreAdmission =>
            unreachable!("an actor ReservationControl cannot publish PreAdmission"),
    }
    drop(command_state);
    drop(active_slot);

    let send_cancel_command = !matches!(
        &order.1, ForceArbitration::InlineObserver(_)
    );
    let interrupt_now = matches!(
        &order.1, ForceArbitration::InlineApplyForce { .. }
    ) || matches!(
        &order.1,
        ForceArbitration::Enqueued(
            CancelOrder::ForceWon | CancelOrder::AfterNonterminalCompletion,
        )
    );

    // Only the caller that changes Empty -> Publishing consumes the permit.
    if send_cancel_command && handle.control.cancel_publish.compare_exchange(
        Empty, Publishing, AcqRel, Acquire
    ).is_ok() {
        let command = Cancel {
            id: handle.id,
            command_sequence: cancel_command_sequence,
            cause: latched_cause,
            reply: handle.control.retention.cancel_reply.clone(),
        };
        match handle.control.control_permit.try_send(command) {
            Ok(()) => handle.control.cancel_publish.store(Enqueued, Release),
            Err(ActorDead) => {
                handle.control.cancel_publish.store(ActorDead, Release);
                handle.control.actor_control.report_dead_actor(handle.id);
            }
        }
    }

    // active_sql is also the interrupt gate. It names the connection actually
    // executing for this reservation—op_conn or tx_conn.
    // This mutex is never held across SQLite execution. Waiting here closes the
    // publication/clear race; silently skipping on try_lock would not.
    let mut active = handle.control.actor_control.active_sql.lock();
    let now = handle.control.terminal.load(Acquire);
    if interrupt_now && owner(now) == OWNER_OPEN
    {
        if let Some(target) = active.as_mut() {
            if target.id == handle.id && !target.interrupt_sent {
                target.interrupt_sent = true;
                target.interrupt.interrupt();
            }
        }
    }
    order.1
}

fn publish_cancel(handle: &ReservationCancelHandle, cause: CancelCause) {
    let _ = arbitrate_force(handle, cause, ForceEventDelivery::Enqueue);
}
~~~

The control-lane receiver passes the command's immutable `command_sequence`
field directly to `accept_cancel_command`; it never resamples the active slot.
The control-lane `Cancel` arm is also closed; receiving the sole command before
SC-1 has armed cancellation does not consume it:

~~~rust
fn accept_cancel_command(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    command_sequence: Option<u64>,
    cause: CancelCause,
    reply: SharedCancelReplySink,
) {
    // A late Cancel needs no cancellation budget. Inspect owner first under the
    // same gate as claim/fence: Complete joins the exact retained result; Cancel
    // joins the existing cleanup. In neither arm may missing preclaim budget be
    // treated as a fault. This is the after-commit/before-or-after-poll path.
    {
        let _owner = control.terminal_owner_gate.lock();
        let word = control.terminal.load(Acquire);
        if control.generation_fenced.load(Acquire) {
            actor.join_fenced_terminal(control, reply);
            return;
        }
        match owner(word) {
            OWNER_COMPLETE => {
                actor.join_stored_completion(control, reply);
                return;
            }
            OWNER_CANCEL => {
                actor.join_cancel_cleanup(control, cause, reply);
                return;
            }
            OWNER_OPEN if has_cancel_intent(word) => {}
            _ => {
                actor.protocol_fault_and_fence(control.id);
                return;
            }
        }
    }
    // Sample before the claim. If readiness publishes while the claim is
    // discovering NotArmed, the post-install comparison below observes it.
    let route_generation_before =
        control.terminal_route_latch.generation.load(Acquire);
    let budget = match (
        control.explicit_deadlines.as_ref(),
        &control.terminal_cutoff,
        &control.cancel_delivery,
    ) {
        (Some(slots), ReservationCutoff::Explicit { .. },
         TerminalDelivery::ExplicitCancel { .. }) =>
            TerminalBudgetArm::SharedExplicit {
                slots: slots.clone(),
                expected_kind: DeadlineKind::CancellationSql,
            },
        (None, ReservationCutoff::Autocommit(cutoff),
         TerminalDelivery::Autocommit { sink }) => {
            let Some(budget) = control.preclaim_autocommit_cancel_budget.get()
                .cloned()
            else {
                return actor.protocol_fault_and_fence(control.id);
            };
            TerminalBudgetArm::PrearmedAutocommitCancel { budget }
        }
        _ => return actor.protocol_fault_and_fence(control.id),
    };
    match actor_claim_cancellation(
        control, command_sequence, cause.clone(), budget,
    ) {
        Ok(phase_proof) => run_cancel_cleanup(actor, control, phase_proof),
        Err(OwnerClaimError::TerminalRouteNotArmed) => {
            let pending = PendingCancel {
                command_sequence,
                cause,
                reply,
                last_seen_route_generation: route_generation_before,
            };
            let mut slot = control.pending_cancel.lock();
            match slot.as_ref() {
                None => *slot = Some(pending),
                Some(existing) if existing.reply.same_endpoint(&pending.reply)
                    && existing.cause == pending.cause => {}
                Some(_) => actor.protocol_fault_and_fence(control.id),
            }
            drop(slot);
            // Closes store-after-notify: if SC-1 published readiness between the
            // first load and pending installation, schedule a retry now.
            let route_is_now_armed = control.explicit_deadlines.as_ref()
                .is_some_and(|slots| matches!(*slots.state.lock(),
                    ExplicitDeadlineState::Armed {
                        kind: DeadlineKind::CancellationSql, ..
                    }));
            if control.terminal_route_latch.generation.load(Acquire)
                    != route_generation_before
                || route_is_now_armed
            {
                control.terminal_route_latch.actor_wake.wake();
            }
        }
        Err(OwnerClaimError::Lost { observed }) if owner(observed) == OWNER_COMPLETE =>
            actor.join_stored_completion(control, reply),
        Err(OwnerClaimError::Lost { observed }) if owner(observed) == OWNER_CANCEL =>
            actor.join_cancel_cleanup(control, cause, reply),
        Err(_) => actor.report_dead_or_fenced(control.id),
    }
}

fn drain_route_armed_cancels(actor: &AppActor) {
    for control in actor.controls_with_pending_cancel() {
        let armed = control.terminal_route_latch.generation.load(Acquire);
        let pending = {
            let mut slot = control.pending_cancel.lock();
            if slot.as_ref().is_some_and(|p| {
                armed != 0 && armed != p.last_seen_route_generation
            }) {
                slot.take()
            } else {
                None
            }
        };
        if let Some(p) = pending {
            accept_cancel_command(
                actor, control, p.command_sequence, p.cause, p.reply,
            );
        }
    }
}
~~~

SC-1 is the only readiness producer. On ordinary entry to Cancelling it uses
the immutable reserve-time `cancel_delivery`, replaces the current shared
deadline with Armed(CancellationSql), and only then calls
`publish_terminal_route_armed`. On root settlement it installs the immutable
root route, arms TerminalSql, and only then enqueues Settle. The actor wake
drains pending controls before starting more data work. `cancel_publish` remains
Enqueued throughout a pending retry: there is one explicit Cancel command, one
owned sink, and no second control-permit send.

For `AfterNonterminalCompletion`, the gate mutation and keyed force enqueue are
the handoff: the already-keyed ordinary completion is first, the force is
second, and only then may the control permit enqueue `Cancel`. The Cancel actor
accepts `ForceAfterNonterminalCompletionQueued` as well as `ForceQueued`, claims
OWNER_CANCEL, and clears the exact active_command_gate pointer only after it has
stored its cancellation attempt/delivery. CANCEL_INTENT prevents any next
statement between those events. The retained ordinary result is therefore
reduced to Idle/Poisoned first, the force moves that state to Cancelling, and
CancellationCompleted follows; the sole Cancel command cannot be consumed as
Lost. By contrast a terminal `CompletionPromised` is always paired with
OWNER_COMPLETE, an immutable attempt/delivery, and an armed terminal budget.
Before the result exists that budget guarantees result-or-fence; after cutoff
Result is retained it guarantees delivery recovery. The gate takes
`TerminalCompletionWon`, is never rewritten, and remains available for
AlreadyCompleted replay. Result
adapters do not acquire the command gate, avoiding an owner-to-command ABBA
edge; retirement drops the whole control only after retained delivery.

cancel_publish changes to Enqueued only after delivery is guaranteed. Every
publisher, including one that wins Empty -> Publishing after another publisher
latched the cause, sends the immutable latched value. Later causes are audit
metadata only. The caller does not forge an owner CAS. Publishing is transient
under the one permit owner, and duplicate publishers only register waiters.
For an explicit SC-1 reservation, `enqueue_sc1_force_in_gate_order` always
keyed-enqueues `TxEvent::Cancel { cause, order, waiter }` first. Detach and
authority/epoch publishers enqueue their incarnation-qualified
`DetachRequested` or trusted forcing `LifecycleObserved` only when the returned
order is ForceWon/AfterNonterminalCompletion. For TerminalCompletionWon,
TerminalFenceWon, or Joined they retain the observation in control audit
metadata and suppress the separate forcing event until the selected terminal
outcome is immutable; it cannot reverse that outcome. A real timer
still sends the exact `DeadlineFired`; while reducing it under the entry lock,
Registry calls `arbitrate_force(...,InlineReducer)`. That mode returns the
typed ForceArbitration. InlineApplyForce is reduced immediately;
InlineObserver waits for the retained proof. Only
InlineDeferredAfterNonterminal keyed-enqueues TxEvent::Cancel, because its
ordinary completion is already keyed behind the timer currently being reduced
and must remain first. External drop/detach uses
Enqueue and receives a distinct keyed event. Both modes publish at most the one
ActorCommand::Cancel controlled by cancel_publish. CompletionPromised emits no forcing event;
it only joins the promised terminal delivery. Thus the Settling table consumes
a typed proof produced inside the decisive critical section, never a racy later
read of OWNER_MASK.

Receiver death and hard stop share one durable generation-fence worker. The
timer callback only selects a cutoff and activates a job that was registered
before the timer was armable; it is never the sole owner of physical cleanup:

~~~text
drive_generation_fence_job(job, bound_snapshot):
  recover the durable job by (actor_generation, job_id, delivery_id)
  lock generation_owner_gate, then routing_execution_gate
  set generation_fenced=true on every control in that exact actor generation
  remove (AppAuthority, actor_generation) from both lane-routing indexes
  close every generation sender and snapshot every ReservationControl
  unlock routing_execution_gate and generation_owner_gate

  request close of tx_conn and op_conn; join the actor thread
  if join/close is incomplete: persist Running and retry; publish nothing
  once both connection generations are unreachable:
    reacquire generation_owner_gate (the actor can no longer contend for it)
    mint one PhysicalGenerationFenceProof containing target ReservationId,
      actor_generation, cutoff delivery id, full owner byte, publication class,
      and public kind copied from bound_snapshot
    call the selected cutoff.publish_physical_fence(job, proof)
      FencePending -> FenceResult -> retention commit -> keyed enqueue
      Result/FenceResult -> only idempotently ensure retained delivery
    for every other snapshotted control in the generation:
      if its cutoff already contains Result, ensure that prepared delivery
      if it owns a terminal attempt, activate its own pre-registered fence job
      if it is explicit OPEN_WITH_CANCEL_INTENT without an owner, bind its
        immutable cause to the reserve-time cancel job and activate that cutoff
      else if it is OWNER_OPEN with an installed root bundle, bind its stored
        decision to the reserve-time root job and activate that cutoff
      else if it is bare OWNER_OPEN explicit, deliver BackendActorUnavailable
        through its preinstalled incarnation-qualified failure sink
      if it is bare Open autocommit, activate its reserve-time autocommit job
        with a separately bound bare-open ActorUnavailable snapshot
    release generation_owner_gate after every control has a durable driver
  retain the lifecycle tombstone, terminal records, jobs, and dedupe keys until
    ForgetTerminal observes zero valid leases
~~~

The “other control” discriminator is executable and ordered; the generic bare
Open arm is deliberately last:

~~~rust
fn drive_unreachable_control_exact(
    supervisor: &FenceJobRegistry,
    control: &Arc<ReservationControl>,
) -> Result<(), ActorError> {
    let _owner = control.terminal_owner_gate.lock();
    if !control.generation_fenced.load(Acquire) {
        return Err(ActorError::CancellationProtocolMismatch);
    }
    if ensure_any_selected_result_delivery(control, supervisor) {
        return Ok(());
    }
    let word = control.terminal.load(Acquire);
    if control.terminal_attempt.lock().is_some() {
        let trigger = selected_registered_trigger_exact(control)?;
        let snapshot = snapshot_terminal_attempt_locked(control, &trigger, word)?;
        select_cutoff_and_bind_infallible(control, supervisor, trigger.job(), snapshot);
        return Ok(());
    }

    match (&control.terminal_cutoff, word) {
        (ReservationCutoff::Explicit { cancel, .. },
         OPEN_WITH_CANCEL_INTENT) => {
            let cause = control.cancel_cause.get().cloned()
                .ok_or(ActorError::CancellationProtocolMismatch)?;
            let route = control.preinstalled_explicit_cancel_fence.get()
                .ok_or(ActorError::CancellationProtocolMismatch)?;
            // A root descriptor may have lost to this intent before its actor
            // owner CAS. It is now permanently superseded.
            if let Some(root) = control.preinstalled_root.get() {
                supervisor.cancel_dormant_infallible(
                    root.hard_stop.trigger.job(),
                );
            }
            let snapshot = TerminalFenceSnapshot {
                id: control.id,
                actor_generation: control.actor_generation,
                full_terminal_word: word,
                delivery_id: cancel.delivery_id,
                class: TerminalPublicationClass::Cancel,
                public_kind: ExplicitHardStopKind::Cancel,
                attempt: TerminalAttemptSnapshot::ExplicitCancel {
                    cause,
                    // OWNER_CANCEL was never acquired; the transaction may
                    // nevertheless have opened before actor death.
                    phase: CancelPhaseProof::TransactionMayExist,
                },
                source: Some(actor_died_before_cancel_claim()),
            };
            cancel.fence_or_ensure_result_delivery(
                supervisor, route.job, snapshot,
            );
            Ok(())
        }
        (ReservationCutoff::Explicit { root, .. }, OWNER_OPEN)
            if control.preinstalled_root.get().is_some() => {
            let bundle = control.preinstalled_root.get().unwrap();
            let attempt = match bundle.decision {
                RootDecision::Commit => TerminalAttemptSnapshot::ExplicitCommit,
                RootDecision::Rollback => TerminalAttemptSnapshot::ExplicitRollback,
            };
            let snapshot = TerminalFenceSnapshot {
                id: control.id,
                actor_generation: control.actor_generation,
                full_terminal_word: word,
                delivery_id: root.delivery_id,
                class: TerminalPublicationClass::Root,
                public_kind: ExplicitHardStopKind::Root,
                attempt,
                source: Some(actor_unavailable_during_terminal_handoff()),
            };
            root.fence_or_ensure_result_delivery(
                supervisor, bundle.hard_stop.trigger.job(), snapshot,
            );
            Ok(())
        }
        (ReservationCutoff::Explicit { .. }, OWNER_OPEN) =>
            deliver_explicit_actor_unavailable_with_sealed_proof(control),
        (ReservationCutoff::Autocommit(cutoff), OWNER_OPEN) => {
            let trigger = control.preclaim_autocommit_fence_trigger.get()
                .ok_or(ActorError::CancellationProtocolMismatch)?;
            let snapshot = snapshot_bare_open_autocommit_after_unroute_locked(
                control,
            )?;
            cutoff.fence_or_ensure_result_delivery(
                supervisor, trigger.job(), snapshot,
            );
            Ok(())
        }
        _ => bind_protocol_fault_cutoff_after_unroute(control, supervisor, word),
    }
}
~~~

The target cutoff is selected under `terminal_owner_gate` by
`fence_or_ensure_result_delivery`. Its `Open -> FencePending` transition is the
logical result-versus-fence linearization point. `FencePending` contains the
complete semantic snapshot and the supervisor-owned job id; killing the timer
publisher at the next instruction cannot strand it. The generation worker
iterates every control sharing the generation, so an autocommit hard stop
cannot strand a queued explicit reservation and fencing one lane cannot leave
the other routable. If Result won just before the fence lock, its already
prepared delivery is replayed; if FencePending won, the actor's later
`publish_prepared_result` returns false and only FenceResult is deliverable.
The join is deliberately outside generation_owner_gate: an SQLite call that is
returning may need that same gate to finalize its target before its actor thread
can exit. Holding the gate across join would be a deterministic deadlock.

After physical unreachability, the snapshot-to-fallback projection is total:

| Bound full word and attempt | Fallback committed through the selected gate |
| --- | --- |
| bare OWNER_OPEN + preinstalled explicit root | RootFinishResult::Failed(Indeterminate, actor_unavailable_during_terminal_handoff) through TerminalHardStopCompleted; retained CommitIndeterminate or RollbackFailed. It never guesses whether terminal FFI began. |
| bare OWNER_OPEN + no terminal explicit attempt | Incarnation-qualified BackendActorUnavailable via ExplicitFailureSink; retained ActorUnavailable wakes command/lease waiters. No owner CAS is fabricated. |
| bare OWNER_OPEN autocommit | ActorTerminalOutcome::ActorUnavailable after the generation proof. |
| OPEN_WITH_CANCEL_INTENT explicit | CancelAck::Indeterminate(actor_died_before_cancel_claim) through the reserve-time cancel cutoff; retained CleanupIndeterminate. The already-keyed force precedes this event. |
| OPEN_WITH_CANCEL_INTENT autocommit | CleanupIndeterminate(actor_died_before_cancel_claim) retaining the immutable cause; never bare ActorUnavailable. |
| OWNER_CANCEL explicit/autocommit | Cause-preserving CleanupIndeterminate(actor_died_during_cancellation); explicit form is keyed CancellationHardStopCompleted. |
| OWNER_COMPLETE + AutocommitSuccess | AutocommitIndeterminate(actor_died_during_commit). |
| OWNER_COMPLETE + AutocommitFailure(e) | CleanupIndeterminate(actor_died_during_error_rollback) retaining e. |
| OWNER_COMPLETE + ExplicitCommit | Root Failed(Indeterminate, actor_died_during_commit); retained CommitIndeterminate. |
| OWNER_COMPLETE + ExplicitRollback | Root Failed(Indeterminate, actor_died_during_rollback); retained RollbackFailed. |
| OWNER_COMPLETE + ExplicitTransactionAborted(e) or ExplicitSnapshotAbort(e) | DataAbortProof(error=e, GenerationRetired) to the exact operation token. |
| OWNER_COMPLETE + ExplicitSnapshotAbortPending | DataAbortProof(actor_unavailable_after_busy_snapshot, GenerationRetired); raw 517 is not relabeled as schema movement because the required authority read did not finish. |
| Missing/mismatched delivery, attempt, owner byte, class, or job | Bind a protocol-fault snapshot, physically fence, and publish the type-appropriate indeterminate failure; alert. Never success and never Stale for a current timer generation. |

The actor writes terminal_attempt and terminal_delivery inside the owner gate
before its owner CAS. The hard-stop publisher binds the **full** terminal byte,
attempt, source, delivery, class, and public projection under that same gate.
`PhysicalGenerationFenceProof` repeats those fields and is rejected if any
differ. The keyed SC-1 proof retires the transaction record, while
`control.retention.outcome` wakes OutcomeAwareCommandFuture instances whose
one-shot actor sender died. A physical fence is minted only after both lane
generations are unreachable. No pre-claim adapter fabricates a cause: it reads
the immutable cause latch and the reserve-time route.

A foreign caller cannot obtain ReservationControl or ActorControlIndex merely
by constructing bytes that look like ReservationId. The actor validates the
full id before acting on the explicit command. A stale/foreign Cancel therefore
never invokes interrupt.

The active_sql mutex is load-bearing. Immediately before sqlite3_step the actor
publishes (ReservationId, lane, connection generation, command sequence,
InterruptHandle) and changes phase to Running under that mutex. Immediately
after step returns it clears that exact target and changes phase to
BetweenStatementAndCommit under the same mutex, before either owner CAS. A
canceller therefore interrupts the exact lane currently serving the reservation
or interrupts nothing; it cannot read stale Running and later hit COMMIT, a
reopened connection, or another reservation. The mutex is never held across
SQLite execution, so Drop may lock it to close the publication/clear race
without waiting for the statement itself.

TerminalWatchdog uses the same out-of-band target path; merely queuing its
ActorCommand would also sit behind a hung COMMIT:

~~~rust
fn publish_terminal_watchdog(
    actor: &ActorControlIndex,
    id: ReservationId,
    watchdog_generation: u64,
) -> Result<(), ActorControlError> {
    let Some(control) = actor.control_exact(id) else {
        return Err(ActorControlError::UnknownReservation);
    };

    // Actor-owned budgets only (autocommit or ExplicitDataAbort). Explicit
    // root/cancel timers are claimed by their shared SC-1 deadline machine.
    let mut active = actor.active_sql.lock();
    // Consume/compare the generation first. A real result clears this slot, so
    // its late callback is Stale even though current_terminal_sequence is 0.
    if control.terminal_watchdog_armed.compare_exchange(
        watchdog_generation, 0, AcqRel, Acquire
    ).is_err() {
        return Err(ActorControlError::StaleTerminalWatchdog);
    }
    let command_sequence = control.current_terminal_sequence.load(Acquire);
    if command_sequence == 0 {
        // Legal pre-claim autocommit-cancel race: the caller armed this exact
        // watchdog before publishing CANCEL_INTENT, but the actor has not yet
        // adopted a terminal sequence. Consuming the first-stage callback is a
        // no-op; the independently prearmed hard stop still bounds retirement.
        let preclaim = control.preclaim_autocommit_cancel_budget.get();
        if control.explicit_deadlines.is_none()
            && control.terminal.load(Acquire) == OPEN_WITH_CANCEL_INTENT
            && preclaim.is_some_and(|budget| {
                budget.watchdog_generation == watchdog_generation
            })
        {
            control.preclaim_watchdog_fired.store(true, Release);
            return Ok(());
        }
        return Err(ActorControlError::TerminalWatchdogProtocolFault);
    }
    if control.terminal_interrupt_generation.load(Acquire) != 0
    {
        return Err(ActorControlError::TerminalWatchdogProtocolFault);
    }
    control.terminal_interrupt_sequence.store(command_sequence, Release);
    control.terminal_interrupt_generation.store(watchdog_generation, Release);

    if let Some(target) = active.as_mut() {
        if target.id == id
            && target.command_sequence == command_sequence
            && matches!(target.statement,
                Commit | Rollback | FailureCleanupRollback
                    | CleanupRollback | PostRollbackAuthority)
            && is_terminal_owner(owner(control.terminal.load(Acquire)))
            && control.terminal_interrupt_generation.load(Acquire)
                == watchdog_generation
            && control.terminal_interrupt_sequence.load(Acquire)
                == command_sequence
            && !target.interrupt_sent
        {
            target.interrupt_sent = true;
            target.interrupt.interrupt();
        }
    }
    drop(active);
    if actor.watchdog_permit.try_send(TerminalWatchdog {
        id, command_sequence, watchdog_generation,
    }).is_err() {
        actor.report_dead_actor(id);
        return Err(ActorControlError::ActorUnavailable);
    }
    Ok(())
}

enum TerminalWatchdogCommandDisposition {
    TargetStillRunning,
    StartBarrierArmed,
    ResultAlreadyRetained,
}

// ActorCommand::TerminalWatchdog dispatches here. The out-of-band publisher is
// the interrupt linearization; this queued command authenticates the latch and
// closes the no-target case. It never changes OWNER_MASK or manufactures a
// result. If terminal SQL has not started, run_statement_exact will observe the
// same latch and take its typed CompletionOwned/Cleaning start-barrier arm.
fn handle_terminal_watchdog_command_exact(
    actor: &AppActor,
    id: ReservationId,
    command_sequence: u64,
    watchdog_generation: u64,
) -> Result<TerminalWatchdogCommandDisposition, ActorError> {
    let control = actor.control_index.control_exact(id)
        .ok_or(ActorError::UnknownReservation)?;
    let _owner = control.terminal_owner_gate.lock();
    let active = actor.active_sql.lock();
    if control.retention.outcome.get().is_some() {
        return Ok(TerminalWatchdogCommandDisposition::ResultAlreadyRetained);
    }
    if control.generation_fenced.load(Acquire)
        || control.actor_generation != actor.actor_generation
        || control.current_terminal_sequence.load(Acquire)
            != command_sequence
        || control.terminal_interrupt_generation.load(Acquire)
            != watchdog_generation
        || control.terminal_interrupt_sequence.load(Acquire)
            != command_sequence
        || !is_terminal_owner(owner(control.terminal.load(Acquire)))
    {
        return Err(ActorError::CancellationProtocolMismatch);
    }
    match active.as_ref() {
        Some(target)
            if target.id == id
                && target.command_sequence == command_sequence
                && target.interrupt_sent =>
            Ok(TerminalWatchdogCommandDisposition::TargetStillRunning),
        None => Ok(TerminalWatchdogCommandDisposition::StartBarrierArmed),
        _ => Err(ActorError::CancellationProtocolMismatch),
    }
}

impl AppActor {
    fn finish_prestart_terminal_timeout(
        &self,
        control: &Arc<ReservationControl>,
        statement: SqlStatementClass,
        sequence: u64,
    ) {
        let _owner = control.terminal_owner_gate.lock();
        let active = self.active_sql.lock();
        let word = control.terminal.load(Acquire);
        let valid = active.is_none()
            && owner(word) == OWNER_COMPLETE
            && sequence == control.current_terminal_sequence.load(Acquire)
            && control.terminal_interrupt_generation.load(Acquire) != 0
            && control.terminal_interrupt_sequence.load(Acquire) == sequence
            && matches!(statement,
                SqlStatementClass::Commit
                    | SqlStatementClass::Rollback
                    | SqlStatementClass::FailureCleanupRollback
                    | SqlStatementClass::PostRollbackAuthority);
        if !valid {
            self.supervisor.quarantine_generation_infallible(
                control.actor_generation,
                prestart_terminal_watchdog_mismatch(),
            );
            return;
        }
        // The absolute hard-stop node was installed with the winning owner
        // claim and remains armed. Recording the barrier is sufficient: no FFI
        // occurred, no capability exists, and only its durable physical fence
        // may now publish an indeterminate terminal result.
        self.supervisor.jobs.note_prestart_watchdog_infallible(
            control.id,
            control.actor_generation,
            sequence,
            selected_terminal_delivery_id(control)
                .expect("validated terminal delivery"),
        );
    }

    fn leave_cancel_cutoff_pending_for_hard_stop(
        &self,
        control: &Arc<ReservationControl>,
    ) {
        let sequence = control.current_terminal_sequence.load(Acquire);
        let _owner = control.terminal_owner_gate.lock();
        let active = self.active_sql.lock();
        if active.is_some()
            || owner(control.terminal.load(Acquire)) != OWNER_CANCEL
            || control.terminal_interrupt_generation.load(Acquire) == 0
            || control.terminal_interrupt_sequence.load(Acquire) != sequence
        {
            self.supervisor.quarantine_generation_infallible(
                control.actor_generation,
                prestart_cancel_cleanup_watchdog_mismatch(),
            );
            return;
        }
        self.supervisor.jobs.note_prestart_watchdog_infallible(
            control.id,
            control.actor_generation,
            sequence,
            selected_terminal_delivery_id(control)
                .expect("validated cancel delivery"),
        );
    }
}

// Relevant actor-loop arms; all other commands use their dedicated functions.
match command {
    ActorCommand::Settle {
        id, command_sequence, decision, terminal_delivery, reply,
    } => {
        let control = actor.control_index.control_exact(id)
            .ok_or(ActorError::UnknownReservation)?;
        let result = claim_and_execute_explicit_root_exact(
            actor, &control, command_sequence, decision, terminal_delivery,
        );
        reply.send(result.map(|_| control.retention.outcome_exact()));
    }
    ActorCommand::TerminalWatchdog {
        id, command_sequence, watchdog_generation,
    } => {
        handle_terminal_watchdog_command_exact(
            actor, id, command_sequence, watchdog_generation,
        )?;
    }
    _ => dispatch_nonterminal_or_cancel_command(command),
}

fn publish_explicit_deadline_interrupt(
    actor: &ActorControlIndex,
    id: ReservationId,
    slots: &Arc<ExplicitDeadlineSlots>,
    kind: DeadlineKind,
    generation: u64,
) -> Result<(), ActorControlError> {
    let control = actor.control_exact(id)
        .ok_or(ActorControlError::UnknownReservation)?;
    if !control.explicit_deadlines.as_ref().is_some_and(|stored| {
        Arc::ptr_eq(stored, slots)
    }) || !matches!(*slots.state.lock(),
        ExplicitDeadlineState::Fired {
            kind: current, generation: current_generation,
        } if current == kind && current_generation == generation)
    {
        return Err(ActorControlError::StaleTerminalWatchdog);
    }
    // Same critical-section order as cleanup retargeting. In particular, do not
    // sample current_terminal_sequence before active_sql: that could publish an
    // interrupt for the predecessor after cleanup installed a fresh sequence.
    let _owner = control.terminal_owner_gate.lock();
    let mut active = actor.active_sql.lock();
    let sequence = control.current_terminal_sequence.load(Acquire);
    if sequence == 0 {
        // The SC-1 absolute timer is allowed to beat actor owner acquisition.
        // Its already-preinstalled delivery lets the next hard-stop fence the
        // generation; there is no SQL target to interrupt yet.
        return Ok(());
    }
    control.terminal_interrupt_sequence.store(sequence, Release);
    control.terminal_interrupt_generation.store(generation, Release);
    if let Some(target) = active.as_mut() {
        let eligible = match kind {
            DeadlineKind::CancellationSql => target.id == id,
            DeadlineKind::TerminalSql => target.id == id
                && matches!(target.statement,
                    Commit | Rollback | FailureCleanupRollback
                        | CleanupRollback),
            _ => false,
        };
        if eligible && target.command_sequence == sequence
            && !target.interrupt_sent
        {
            target.interrupt_sent = true;
            target.interrupt.interrupt();
        }
    }
    Ok(())
}
~~~

The direct interrupt is the low-latency attempt to end the synchronous call;
the later command lets the
actor reject a stale generation and finish terminal bookkeeping. It does not
publish CANCEL_INTENT or change the existing owner. It is valid for both a
completion-owned COMMIT/ROLLBACK and a cancellation-owned cleanup ROLLBACK.
Explicit SC-1 TerminalSql/CancellationSql instead calls
`claim_fire` and then `publish_explicit_deadline_interrupt`; only after that
returns does its reducer replace Fired with the matching HardStop generation.
The same Arc authenticates both sides, closing the reducer/actor wiring gap.
Sequence zero is a legal pre-claim expiry, not a protocol error: the first
stage interrupts nothing, SC-1 advances to hard-stop, and the preinstalled
delivery plus logical generation fence prevents the actor from subsequently
claiming ownership or starting SQL.

SC-1's TerminalHardStop is the second, non-actor-mailbox stage. It uses this
exact publisher rather than the blocked actor mailbox:

~~~rust
enum HardStopPublish {
    Fencing,       // the trigger-specific fenced completion will follow
    ResultWon,     // retained proof is now guaranteed enqueued
    CancelPreempted, // OPEN|CANCEL_INTENT beat a pre-claim root hard stop
    Stale,
}

#[derive(Clone)]
enum HardStopTrigger {
    ExplicitRoot {
        permit: Arc<ExplicitHardStopPermit>,
        decision: RootDecision,
        key: TxKey,
        cutoff: Arc<TerminalCutoffGate<RootFinishResult>>,
        deadline_slots: Arc<ExplicitDeadlineSlots>,
        terminal_token: CommandToken,
        fence_token: CommandToken,
        registry: TxRegistrySender,
        job: DurableFenceJobHandle,
    },
    ExplicitCancel {
        permit: Arc<ExplicitHardStopPermit>,
        cause: CancelCause,
        key: TxKey,
        cutoff: Arc<TerminalCutoffGate<CancelAck>>,
        deadline_slots: Arc<ExplicitDeadlineSlots>,
        cancellation_token: CommandToken,
        fence_token: CommandToken,
        registry: TxRegistrySender,
        job: DurableFenceJobHandle,
    },
    ExplicitDataAbort {
        permit: Arc<DataDeliveryPermit>,
        key: TxKey,
        cutoff: Arc<TerminalCutoffGate<DataAbortProof>>,
        data_token: CommandToken,
        registry: TxRegistrySender,
        job: DurableFenceJobHandle,
    },
    Autocommit {
        app: AppAuthority,
        cutoff: Arc<TerminalCutoffGate<ActorTerminalOutcome>>,
        sink: SharedActorTerminalSink,
        // Some only for caller-armed OPEN_WITH_CANCEL_INTENT cancellation.
        preclaim_cancel_budget_id: Option<TerminalDeliveryId>,
        job: DurableFenceJobHandle,
    },
}

impl HardStopTrigger {
    fn app_authority(&self) -> &AppAuthority {
        match self {
            HardStopTrigger::ExplicitRoot { key, .. }
            | HardStopTrigger::ExplicitCancel { key, .. }
            | HardStopTrigger::ExplicitDataAbort { key, .. } => &key.app,
            HardStopTrigger::Autocommit { app, .. } => app,
        }
    }
}

// These builders are the only way to obtain an armable hard-stop trigger.
// Data-abort builds its typed endpoint/job before publishing Execute. Root,
// cancel, and autocommit bind jobs installed while ReservationControl was
// private. A caller may arm a
// watchdog or publish Execute/Settle/Cancel only after the matching builder
// succeeds. No timer can fire and then discover that recovery lacked capacity.
fn create_explicit_root_hard_stop(
    control: &Arc<ReservationControl>,
    decision: RootDecision,
    key: TxKey,
    cutoff: Arc<TerminalCutoffGate<RootFinishResult>>,
    deadline_slots: Arc<ExplicitDeadlineSlots>,
    terminal_token: CommandToken,
    registry: TxRegistrySender,
) -> Result<(Arc<ExplicitHardStopPermit>, HardStopTrigger), ActorError> {
    let preinstalled = control.preinstalled_explicit_root_fence.get()
        .ok_or(ActorError::CancellationProtocolMismatch)?;
    let job = preinstalled.job;
    let fence_token = preinstalled.fence_token;
    if job.delivery_id != cutoff.delivery_id {
        return Err(ActorError::CancellationProtocolMismatch);
    }
    let permit = Arc::new(ExplicitHardStopPermit {
        permit_id: mint_never_reused_terminal_delivery_id(),
        kind: ExplicitHardStopKind::Root,
        key: key.clone(),
        actor_generation: control.actor_generation,
        deadline_slots: deadline_slots.clone(),
        cutoff_delivery_id: cutoff.delivery_id,
        completion_token: terminal_token,
        fence_token,
        fence_job: job,
        mailbox: preinstalled.mailbox.clone(),
        publish_state: AtomicU8::new(HardStopPermitPublishState::Open as u8),
    });
    let trigger = HardStopTrigger::ExplicitRoot {
        permit: permit.clone(), decision, key, cutoff, deadline_slots,
        terminal_token, fence_token, registry, job,
    };
    Ok((permit, trigger))
}

fn install_explicit_root_fence_before_publish(
    supervisor: &ActorSupervisorIndex,
    control: &Arc<ReservationControl>,
) -> Result<(), ActorError> {
    let (
        ReservationCutoff::Explicit { root: cutoff, .. },
        TerminalDelivery::ExplicitCancel { key, registry, .. },
    ) = (&control.terminal_cutoff, &control.cancel_delivery)
    else { return Err(ActorError::CancellationProtocolMismatch); };
    let mailbox = registry.pin_detached_mailbox(key)?;
    let fence_token = mint_command_token();
    let route = prepare_root_fence_route(
        control, key, cutoff, fence_token, registry,
    )?;
    let job = supervisor.jobs.register_dormant_root(
        control.actor_generation,
        Arc::downgrade(cutoff),
        route,
    ).map_err(|_| ActorError::ActorSaturated)?;
    let preinstalled = PreinstalledExplicitRootFence {
        job,
        fence_token,
        mailbox,
    };
    if control.preinstalled_explicit_root_fence.set(preinstalled).is_err() {
        supervisor.jobs.cancel_dormant_infallible(job);
        Err(ActorError::CancellationProtocolMismatch)
    } else {
        Ok(())
    }
}

fn create_explicit_cancel_hard_stop(
    control: &Arc<ReservationControl>,
    cause: CancelCause,
    key: TxKey,
    cutoff: Arc<TerminalCutoffGate<CancelAck>>,
    deadline_slots: Arc<ExplicitDeadlineSlots>,
    cancellation_token: CommandToken,
    registry: TxRegistrySender,
) -> Result<(Arc<ExplicitHardStopPermit>, HardStopTrigger), ActorError> {
    // Reserve installed this route while the control was still private.  This
    // function only binds the immutable first cause and SC-1 deadline Arc; it
    // cannot allocate, pin a mailbox, or consume supervisor capacity.
    let preinstalled = control.preinstalled_explicit_cancel_fence.get()
        .ok_or(ActorError::CancellationProtocolMismatch)?;
    let job = preinstalled.job;
    let fence_token = preinstalled.fence_token;
    if job.delivery_id != cutoff.delivery_id {
        return Err(ActorError::CancellationProtocolMismatch);
    }
    let permit = Arc::new(ExplicitHardStopPermit {
        permit_id: mint_never_reused_terminal_delivery_id(),
        kind: ExplicitHardStopKind::Cancel,
        key: key.clone(),
        actor_generation: control.actor_generation,
        deadline_slots: deadline_slots.clone(),
        cutoff_delivery_id: cutoff.delivery_id,
        completion_token: cancellation_token,
        fence_token,
        fence_job: job,
        mailbox: preinstalled.mailbox.clone(),
        publish_state: AtomicU8::new(HardStopPermitPublishState::Open as u8),
    });
    let trigger = HardStopTrigger::ExplicitCancel {
        permit: permit.clone(), cause, key, cutoff, deadline_slots,
        cancellation_token, fence_token, registry, job,
    };
    Ok((permit, trigger))
}

fn install_explicit_cancel_fence_before_publish(
    supervisor: &ActorSupervisorIndex,
    control: &Arc<ReservationControl>,
) -> Result<(), ActorError> {
    let TerminalDelivery::ExplicitCancel {
        key, cutoff, registry, ..
    } = control.cancel_delivery.clone()
    else { return Err(ActorError::CancellationProtocolMismatch); };

    // Both independent pins are fallible only while ReservationControl is
    // private. The prepared route owns its keyed completion endpoint; the
    // detached mailbox is the infallible CancelPreempted endpoint retained by
    // ExplicitHardStopPermit.
    let mailbox = registry.pin_detached_mailbox(&key)?;
    let fence_token = mint_command_token();
    let route = prepare_cancel_fence_route(
        control, &key, &cutoff, fence_token, &registry,
    )?;
    let job = supervisor.jobs.register_dormant_cancel(
        control.actor_generation,
        Arc::downgrade(&cutoff),
        route,
    ).map_err(|_| ActorError::ActorSaturated)?;
    let preinstalled = PreinstalledExplicitCancelFence {
        job,
        fence_token,
        mailbox,
    };
    if control.preinstalled_explicit_cancel_fence
        .set(preinstalled).is_err()
    {
        supervisor.jobs.cancel_dormant_infallible(job);
        Err(ActorError::CancellationProtocolMismatch)
    } else {
        Ok(())
    }
}

fn create_data_abort_delivery(
    supervisor: &ActorSupervisorIndex,
    control: &Arc<ReservationControl>,
    permit_id: u128,
    key: TxKey,
    cutoff: Arc<TerminalCutoffGate<DataAbortProof>>,
    data_token: CommandToken,
    registry: TxRegistrySender,
) -> Result<(TerminalDelivery, HardStopTrigger), ActorError> {
    // Called while Registry admits ActiveAction::Data and before Execute is
    // enqueued. The source error is deliberately absent: it is captured under
    // terminal_owner_gate in TerminalFenceSnapshot if this job later wins.
    let route = prepare_data_abort_fence_route(
        control, &key, &cutoff, data_token, &registry,
    )?;
    let job = supervisor.jobs.register_dormant_data_abort(
        control.actor_generation,
        Arc::downgrade(&cutoff),
        route,
    ).map_err(|_| ActorError::ActorSaturated)?;
    let permit = Arc::new(DataDeliveryPermit {
        permit_id,
        key: key.clone(),
        actor_generation: control.actor_generation,
        data_token,
        cutoff_delivery_id: cutoff.delivery_id,
        fence_job: job,
    });
    let trigger = HardStopTrigger::ExplicitDataAbort {
        permit: permit.clone(), key: key.clone(), cutoff: cutoff.clone(),
        data_token, registry: registry.clone(), job,
    };
    let delivery = TerminalDelivery::ExplicitDataAbort {
        permit, key, cutoff, data_token, registry,
        hard_stop_trigger: trigger.clone(),
    };
    Ok((delivery, trigger))
}

fn install_autocommit_fence_trigger_before_publish(
    supervisor: &ActorSupervisorIndex,
    control: &Arc<ReservationControl>,
    cutoff: Arc<TerminalCutoffGate<ActorTerminalOutcome>>,
    sink: SharedActorTerminalSink,
) -> Result<(), ActorError> {
    let route = prepare_autocommit_fence_route(control, &cutoff, &sink)?;
    let job = supervisor.jobs.register_dormant_autocommit(
        control.actor_generation,
        Arc::downgrade(&cutoff),
        route,
    ).map_err(|_| ActorError::ActorSaturated)?;
    let trigger = HardStopTrigger::Autocommit {
        app: control.id.app,
        cutoff,
        sink,
        preclaim_cancel_budget_id: None,
        job,
    };
    if control.preclaim_autocommit_fence_trigger.set(trigger).is_err() {
        supervisor.jobs.cancel_dormant_infallible(job);
        Err(ActorError::CancellationProtocolMismatch)
    } else {
        Ok(())
    }
}

fn cancel_unpublished_reservation_fences_infallible(
    supervisor: &ActorSupervisorIndex,
    control: &ReservationControl,
) {
    if let Some(trigger) = control.preclaim_autocommit_fence_trigger.get() {
        supervisor.jobs.cancel_dormant_infallible(trigger.job());
    }
    if let Some(preinstalled) =
        control.preinstalled_explicit_cancel_fence.get()
    {
        supervisor.jobs.cancel_dormant_infallible(preinstalled.job);
    }
    if let Some(preinstalled) = control.preinstalled_explicit_root_fence.get() {
        supervisor.jobs.cancel_dormant_infallible(preinstalled.job);
    }
}

// Mandatory construction order, asserted by unit and model tests:
//
// * autocommit Reserve: construct Arc<ReservationControl> privately; call
//   install_autocommit_fence_trigger_before_publish; only then insert it in the
//   control index or return ReservationHandle;
// * explicit Reserve: construct Arc<ReservationControl> privately; install its
//   distinct root and cancel fence routes/jobs; only then insert it in the
//   control index or return ReservationHandle;
// * explicit Execute: call create_data_abort_delivery before inserting
//   ActiveAction::Data or enqueueing ActorCommand::Execute; carry the returned
//   trigger unchanged into ActorOwned budget admission;
// * explicit root: bind decision/token/deadline to the reserve-installed root
//   job, atomically install that full bundle, retain it in Settling, then arm;
// * explicit cancel: bind cause/deadline to the reserve-installed cancel job,
//   retain permit+trigger in Cancelling(Awaiting), then arm the first deadline.
//
// Capacity failure is ActorSaturated only at Reserve or explicit Execute,
// before publication/SQL. A post-Reserve root/cancel binding mismatch is the
// typed TerminalRouteMismatch/Backend(protocol mismatch), enters generation
// retirement, sends no SQL, and never arms a terminal timer.

fn prepare_sc1_root_route_before_arm(
    entry: &mut TxEntry,
    control: &Arc<ReservationControl>,
    intent: &RootIntent,
    delivery: TerminalDelivery,
    terminal_token: CommandToken,
) -> Result<RegisteredExplicitHardStop, TxProtocolError> {
    let TerminalDelivery::ExplicitRoot { key, cutoff, registry: sender, .. } =
        delivery else { return Err(TxProtocolError::TerminalRouteMismatch); };
    let (permit, trigger) = create_explicit_root_hard_stop(
        control,
        intent.decision(),
        key,
        cutoff,
        entry.deadline_slots.clone(),
        terminal_token,
        sender,
    ).map_err(TxProtocolError::from_actor)?;
    let fence_token = permit.fence_token;
    Ok(RegisteredExplicitHardStop { permit, trigger, fence_token })
}

fn prepare_sc1_cancel_route_before_arm(
    entry: &mut TxEntry,
    control: &Arc<ReservationControl>,
    cause: CancelCause,
    cancellation_token: CommandToken,
) -> Result<RegisteredExplicitHardStop, TxProtocolError> {
    let TerminalDelivery::ExplicitCancel { key, cutoff, registry: sender, .. } =
        control.cancel_delivery.clone()
    else { return Err(TxProtocolError::TerminalRouteMismatch); };
    let (permit, trigger) = create_explicit_cancel_hard_stop(
        control,
        cause,
        key,
        cutoff,
        entry.deadline_slots.clone(),
        cancellation_token,
        sender,
    ).map_err(TxProtocolError::from_actor)?;
    let fence_token = permit.fence_token;
    Ok(RegisteredExplicitHardStop { permit, trigger, fence_token })
}

fn admit_explicit_execute_before_enqueue(
    registry: &mut TxRegistry,
    entry: &mut TxEntry,
    control: &Arc<ReservationControl>,
    token: CommandToken,
) -> Result<(TerminalDelivery, HardStopTrigger), TxProtocolError> {
    let cutoff = Arc::new(TerminalCutoffGate::fresh(
        mint_never_reused_terminal_delivery_id(),
    ));
    let (delivery, trigger) = create_data_abort_delivery(
        &registry.supervisor,
        control,
        registry.secure_permit_id(),
        entry.key.clone(),
        cutoff,
        token,
        registry.sender(),
    ).map_err(TxProtocolError::from_actor)?;
    entry.install_active_data_route_exact(token, delivery.clone(), trigger.clone());
    // Only this return value may be copied into ActorCommand::Execute.
    Ok((delivery, trigger))
}

fn terminal_outcome_equivalent(
    a: &ActorTerminalOutcome,
    b: &ActorTerminalOutcome,
) -> bool {
    // ActorTerminalOutcome's PartialEq covers the variant and every typed
    // payload, including the canonical DbError and SharedDbResult value.
    a == b
}

// This cell is deliberately separate from ReservationControl. It owns no
// cutoff gate, so a prepared delivery may capture Arc<TerminalRetentionCell>
// without forming control -> cutoff -> closure -> control. ForgetTerminal drops
// the last cutoff/route and then this cell.
struct TerminalRetentionCell {
    id: ReservationId,
    outcome: OnceLock<SharedActorTerminalOutcome>,
    cancel_reply: SharedCancelReplySink,
    terminal_waiters: Mutex<Vec<Waker>>,
    conflict_reporter: TerminalConflictReporter,
}

#[derive(Clone)]
struct OutcomeCommitPlan {
    retention: Arc<TerminalRetentionCell>,
    candidate: SharedActorTerminalOutcome,
    public_kind: ExplicitHardStopKind,
}

impl OutcomeCommitPlan {
    // All authority/projection checks happened before cutoff arbitration and one
    // cutoff/owner/permit winner is the only caller. Same candidate is an
    // idempotent recovery retry; different candidate is an internal assertion
    // covered by the cutoff-exclusivity property.
    fn commit_exact(&self) {
        if self.retention.outcome.set(self.candidate.clone()).is_err() {
            let stored = self.retention.outcome.get().expect("outcome writer");
            assert!(terminal_outcome_equivalent(
                stored.as_ref(), self.candidate.as_ref(),
            ));
        }
        let stored = self.retention.outcome.get().unwrap().clone();
        match (self.public_kind, stored.as_ref()) {
            (ExplicitHardStopKind::Cancel,
             ActorTerminalOutcome::Cancelled { cause, cleanup }) =>
                self.retention.cancel_reply.store_and_wake(
                    Ok(CancelResult::Cancelled {
                        cause: cause.clone(), cleanup: cleanup.clone(),
                    }),
                ),
            (ExplicitHardStopKind::Cancel,
             ActorTerminalOutcome::CleanupIndeterminate(_)) =>
                self.retention.cancel_reply.store_and_wake(
                    Err(ActorError::CancellationCleanupFailed),
                ),
            (ExplicitHardStopKind::Root, _) =>
                self.retention.cancel_reply.store_and_wake(
                    Ok(CancelResult::AlreadyCompleted {
                        outcome: stored.clone(),
                    }),
                ),
            _ => unreachable!("projection validated before cutoff arbitration"),
        }
        for waiter in self.retention.terminal_waiters.lock().drain(..) {
            waiter.wake();
        }
    }
}

struct PhysicalGenerationFenceProof {
    proof_id: TerminalDeliveryId,
    id: ReservationId,
    actor_generation: u64,
    observed_full_word: u8,
    cutoff_delivery_id: TerminalDeliveryId,
    class: TerminalPublicationClass,
    public_kind: ExplicitHardStopKind,
    seal: PhysicalGenerationFenceSeal,
}

fn make_prepared_delivery<T>(
    delivery_id: TerminalDeliveryId,
    proof: Arc<T>,
    endpoint: Arc<dyn Fn(TerminalDeliveryId, Arc<T>) + Send + Sync>,
    outcome: OutcomeCommitPlan,
) -> Arc<PreparedTerminalDelivery<T>>
where
    T: Send + Sync + 'static,
{
    // Captures only the independent retention cell and a pinned endpoint.
    // Neither owns ReservationControl or TerminalCutoffGate.
    let commit = Arc::new(move || outcome.commit_exact());
    Arc::new(PreparedTerminalDelivery {
        delivery_id,
        proof,
        enqueue_once: endpoint,
        commit_retained: commit,
    })
}

fn root_outcome(
    attempt: &TerminalAttempt,
    proof: &RootFinishResult,
) -> Result<SharedActorTerminalOutcome, ActorError> {
    Ok(Arc::new(match (attempt, proof) {
        (TerminalAttempt::ExplicitCommit, RootFinishResult::Committed) =>
            ActorTerminalOutcome::Committed,
        (TerminalAttempt::ExplicitCommit, RootFinishResult::RolledBack) =>
            ActorTerminalOutcome::CommitRolledBack,
        (TerminalAttempt::ExplicitRollback, RootFinishResult::Committed) =>
            ActorTerminalOutcome::TerminalResultMismatch,
        (TerminalAttempt::ExplicitRollback, RootFinishResult::RolledBack) =>
            ActorTerminalOutcome::RolledBack,
        (TerminalAttempt::ExplicitCommit,
         RootFinishResult::Failed {
             error, certainty: FinishCertainty::DefinitelyNotCommitted,
         }) => ActorTerminalOutcome::CommitFailed(error.clone()),
        (TerminalAttempt::ExplicitCommit,
         RootFinishResult::Failed { error, .. }) =>
            ActorTerminalOutcome::CommitIndeterminate(error.clone()),
        (TerminalAttempt::ExplicitRollback,
         RootFinishResult::Failed { error, .. }) =>
            ActorTerminalOutcome::RollbackFailed(error.clone()),
        _ => return Err(ActorError::TerminalReplyProjectionMismatch),
    }))
}

fn sc1_fence_retirement(
    control_id: ReservationId,
    actor_generation: u64,
    key: TxKey,
    retirement_id: GenerationRetirementId,
    physical: Arc<PhysicalGenerationFenceProof>,
) -> Arc<GenerationRetirementProof> {
    assert_eq!(physical.id, control_id);
    assert_eq!(physical.actor_generation, actor_generation);
    Arc::new(GenerationRetirementProof {
        key,
        actor_generation,
        retirement_id,
        kind: GenerationRetirementKind::PhysicallyFenced,
        seal: GenerationRetirementSeal::from_physical(physical),
    })
}
// Ordinary terminal retirement is authorized only by a private capability
// minted at the exact finalization barrier. CompletedSqlTargetProof is diagnostic
// data and can never by itself retire a reservation.
enum TerminalEndCapability {
    Root(RootEndCapability),
    Cancel(CancelEndCapability),
    DataAbort(DataAbortEndCapability),
    Uncertain(UncertainEndCapability),
}

struct RootEndCapability {
    id: ReservationId,
    actor_generation: u64,
    delivery_id: TerminalDeliveryId,
    decision: RootDecision,
    terminal: CompletedSqlTargetProof,
    // Some only when a failed COMMIT was followed by a confirmed compensating
    // FailureCleanupRollback. Both statements are bound into one private seal.
    cleanup: Option<CompletedSqlTargetProof>,
    observed_owner_word: u8,
    seal: RootEndSeal, // non-Clone; minted only by classify_root_finish_exact
}

struct CancelEndCapability {
    id: ReservationId,
    actor_generation: u64,
    delivery_id: TerminalDeliveryId,
    cause: CancelCause,
    fact: CancelEndFact,
    observed_owner_word: u8,
    seal: CancelEndSeal, // non-Clone; minted only by cancel cleanup finalizer
}

enum CancelEndFact {
    // Minted under terminal_owner_gate+active_sql when the start barrier
    // suppresses Prepare/BEGIN before any FFI call.
    NoStatementStarted(NoSqlStartProof),
    // A platform-role PrepareAuthority statement completed/interrupted before
    // BEGIN; the exact op lane was finalized, idle, and autocommit.
    CompletedOutsideTransaction(PostFinalizeAutocommitProof),
    // BEGIN may have run, but the exact reservation lane was sampled clean and
    // autocommit after all statements were finalized.
    BeginDidNotOpen(PostFinalizeAutocommitProof),
    // Exact CleanupRollback completed and the same barrier sampled autocommit.
    RolledBack(PostFinalizeAutocommitProof),
    // The interrupted statement itself caused SQLite to roll back; the same
    // finalization barrier sampled autocommit before another command could run.
    SQLiteAlreadyRolledBack(PostFinalizeAutocommitProof),
}

struct NoSqlStartProof {
    id: ReservationId,
    actor_generation: u64,
    cancellation_sequence: u64,
    lane: ConnectionLane,
    connection_generation: u64,
    phase: CancelPhaseProof, // necessarily NoTransactionPossible
    observed_owner_word: u8, // necessarily OWNER_CANCEL
    is_busy_after: bool,       // necessarily false
    is_autocommit_after: bool, // necessarily true
    seal: NoSqlStartSeal,
}

struct PostFinalizeAutocommitProof {
    source: PostFinalizeSource,
    is_busy_after: bool,       // necessarily false
    is_autocommit_after: bool, // necessarily true
    seal: PostFinalizeEndSeal,
}

enum PostFinalizeSource {
    Statement(CompletedSqlTargetProof),
    // Used when cancellation claimed an actor-idle reservation after the last
    // ordinary command was already reduced. The sample and private seal bind
    // the terminal sequence, lease, lane generation, phase, and engine state;
    // it is not inferred merely from a thread-local phase enum.
    ActorIdle {
        id: ReservationId,
        actor_generation: u64,
        terminal_sequence: u64,
        lease_id: u128,
        lane: ConnectionLane,
        connection_generation: u64,
        phase: ActorPhase,
        is_busy_after: bool,
        is_autocommit_after: bool,
        seal: ActorIdleEndSeal,
    },
}

struct DataAbortEndCapability {
    id: ReservationId,
    actor_generation: u64,
    delivery_id: TerminalDeliveryId,
    data_permit: Arc<DataDeliveryPermit>,
    completed_end: CompletedSqlTargetProof,
    // Some only for BUSY_SNAPSHOT: the platform-role classification ran after
    // completed_end had already proved rollback. It is the current terminal
    // sequence but never substitutes for the transaction-end proof.
    postrollback_authority: Option<CompletedSqlTargetProof>,
    observed_owner_word: u8,
    seal: DataAbortEndSeal,
}

struct UncertainEndCapability {
    id: ReservationId,
    actor_generation: u64,
    delivery_id: TerminalDeliveryId,
    class: TerminalPublicationClass,
    sequence: Option<u64>,
    observed_owner_word: u8,
    seal: UncertainEndSeal,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TerminalPublicationClass { Root, Cancel, DataAbort, Autocommit }

fn validate_completed_target(
    actor: &AppActor,
    control: &ReservationControl,
    completed: &CompletedSqlTargetProof,
    expected_lane: ConnectionLane,
    expected_statement: impl Fn(SqlStatementClass) -> bool,
) -> bool {
    completed.id == control.id
        && completed.lane == expected_lane
        && completed.connection_generation == actor.generation_for(expected_lane)
        && completed.command_sequence
            == control.current_terminal_sequence.load(Acquire)
        && expected_statement(completed.statement)
        && !actor.connection(expected_lane).is_busy()
        && actor.connection(expected_lane).is_autocommit()
}

fn validate_root_end_chain_exact(
    actor: &AppActor,
    control: &ReservationControl,
    cap: &RootEndCapability,
) -> bool {
    let terminal_ok = cap.terminal.id == control.id
        && cap.terminal.lane == ConnectionLane::Tx
        && cap.terminal.connection_generation
            == actor.generation_for(ConnectionLane::Tx)
        && matches!((cap.decision, cap.terminal.statement),
            (RootDecision::Commit, SqlStatementClass::Commit)
            | (RootDecision::Rollback, SqlStatementClass::Rollback));
    let sequence_ok = match &cap.cleanup {
        None => cap.terminal.command_sequence
            == control.current_terminal_sequence.load(Acquire),
        Some(cleanup) => cleanup.id == control.id
            && cleanup.lane == ConnectionLane::Tx
            && cleanup.connection_generation
                == actor.generation_for(ConnectionLane::Tx)
            && cleanup.statement == SqlStatementClass::FailureCleanupRollback
            && cleanup.command_sequence
                == control.current_terminal_sequence.load(Acquire),
    };
    terminal_ok && sequence_ok
        && !actor.tx_conn.is_busy() && actor.tx_conn.is_autocommit()
}

fn validate_data_abort_chain_exact(
    actor: &AppActor,
    control: &ReservationControl,
    cap: &DataAbortEndCapability,
) -> bool {
    validate_data_abort_chain_parts(
        actor, control, &cap.completed_end,
        cap.postrollback_authority.as_ref(),
    )
}

fn validate_data_abort_chain_parts(
    actor: &AppActor,
    control: &ReservationControl,
    end: &CompletedSqlTargetProof,
    postrollback_authority: Option<&CompletedSqlTargetProof>,
) -> bool {
    let end_ok = end.id == control.id
        && end.lane == ConnectionLane::Tx
        && end.connection_generation == actor.generation_for(ConnectionLane::Tx)
        && matches!(end.statement,
            SqlStatementClass::Data
                | SqlStatementClass::FailureCleanupRollback)
        && !actor.connection(ConnectionLane::Tx).is_busy()
        && actor.connection(ConnectionLane::Tx).is_autocommit();
    if !end_ok { return false; }
    match postrollback_authority {
        None => end.command_sequence
            == control.current_terminal_sequence.load(Acquire),
        Some(authority) => authority.id == control.id
            && authority.lane == ConnectionLane::Op
            && authority.connection_generation
                == actor.generation_for(ConnectionLane::Op)
            && authority.statement == SqlStatementClass::PostRollbackAuthority
            && authority.command_sequence
                == control.current_terminal_sequence.load(Acquire)
            && !actor.connection(ConnectionLane::Op).is_busy(),
    }
}

// Consumes the non-Clone capability. It validates the exact selected delivery,
// owner, statement class, sequence, and private seal before stamping the SC-1
// token into BackendEndProof. An old Data/authority proof cannot be replayed as
// root or cancellation retirement.
fn install_terminal_retirement_exact(
    actor: &AppActor,
    control: &ReservationControl,
    capability: TerminalEndCapability,
) -> Result<BackendTerminalRetirement, ActorError> {
    let _owner = control.terminal_owner_gate.lock();
    let active = actor.active_sql.lock();
    if active.is_some() || control.terminal_retirement.get().is_some() {
        return Err(ActorError::CancellationProtocolMismatch);
    }
    let delivery = control.terminal_delivery.lock().clone()
        .ok_or(ActorError::CancellationProtocolMismatch)?;
    let word = control.terminal.load(Acquire);

    let ended: Option<(TxKey, CommandToken)> = match (capability, &delivery) {
        (TerminalEndCapability::Root(cap),
         TerminalDelivery::ExplicitRoot { key, token, cutoff, .. })
            if cap.id == control.id
                && cap.actor_generation == control.actor_generation
                && cap.delivery_id == cutoff.delivery_id
                && cap.observed_owner_word == word
                && owner(word) == OWNER_COMPLETE
                && control.terminal_attempt.lock().as_ref().is_some_and(|a|
                    matches!((a, cap.decision),
                        (TerminalAttempt::ExplicitCommit, RootDecision::Commit)
                        | (TerminalAttempt::ExplicitRollback, RootDecision::Rollback)))
                && validate_root_end_chain_exact(actor, control, &cap)
                && cap.seal.authenticates(
                    &cap.terminal, cap.cleanup.as_ref(), cap.delivery_id, word,
                ) =>
            Some((key.clone(), *token)),

        (TerminalEndCapability::Cancel(cap),
         TerminalDelivery::ExplicitCancel { key, token, cutoff, .. })
            if cap.id == control.id
                && cap.actor_generation == control.actor_generation
                && cap.delivery_id == cutoff.delivery_id
                && cap.observed_owner_word == word
                && owner(word) == OWNER_CANCEL
                && control.terminal_attempt.lock().as_ref().is_some_and(|a|
                    matches!(a, TerminalAttempt::Cancellation { cause, .. }
                        if cause == &cap.cause))
                && validate_cancel_end_fact_exact(actor, control, &cap.fact)
                && cap.seal.authenticates(
                    control.id, cap.delivery_id, &cap.cause, &cap.fact, word,
                ) => Some((key.clone(), *token)),

        (TerminalEndCapability::DataAbort(cap),
         TerminalDelivery::ExplicitDataAbort {
             permit, key, data_token, cutoff, ..
         }) if cap.id == control.id
                && cap.actor_generation == control.actor_generation
                && cap.delivery_id == cutoff.delivery_id
                && Arc::ptr_eq(&cap.data_permit, permit)
                && cap.observed_owner_word == word
                && owner(word) == OWNER_COMPLETE
                && matches!(cap.completed_end.statement,
                    SqlStatementClass::Data
                    | SqlStatementClass::FailureCleanupRollback)
                && validate_data_abort_chain_exact(actor, control, &cap)
                && cap.seal.authenticates(
                    &cap.completed_end,
                    cap.postrollback_authority.as_ref(),
                    cap.delivery_id,
                    permit.permit_id,
                ) => Some((key.clone(), *data_token)),

        (TerminalEndCapability::Uncertain(cap), selected)
            if cap.id == control.id
                && cap.actor_generation == control.actor_generation
                && cap.observed_owner_word == word
                && terminal_class_and_delivery_id(selected)
                    == (cap.class, cap.delivery_id)
                && cap.seal.authenticates(
                    control.id, cap.delivery_id, cap.class, word,
                ) => None,

        _ => return Err(actor.protocol_fault_and_fence_with_source(
            control, terminal_end_capability_mismatch(),
        )),
    };

    let retirement = match ended {
        Some((key, command_token)) if key.app == control.id.app
            && control.tx_key.as_ref() == Some(&key) =>
            BackendTerminalRetirement::Ended(BackendEndProof {
                key,
                actor_generation: control.actor_generation,
                command_token,
                seal: BackendEndSeal::mint_from_consumed_terminal_capability(
                    control.id, control.actor_generation, command_token,
                ),
            }),
        Some(_) => return Err(ActorError::StaleAppIncarnation),
        None => BackendTerminalRetirement::NeedsGenerationRetirement,
    };
    control.terminal_retirement.set(retirement.clone())
        .map_err(|_| ActorError::CancellationProtocolMismatch)?;
    Ok(retirement)
}

fn validate_cancel_end_fact_exact(
    actor: &AppActor,
    control: &ReservationControl,
    fact: &CancelEndFact,
) -> bool {
    match fact {
        CancelEndFact::NoStatementStarted(p) =>
            p.id == control.id
                && p.actor_generation == control.actor_generation
                && p.connection_generation == actor.generation_for(p.lane)
                && p.phase == CancelPhaseProof::NoTransactionPossible
                && owner(p.observed_owner_word) == OWNER_CANCEL
                && !p.is_busy_after
                && p.is_autocommit_after
                && !actor.connection(p.lane).is_busy()
                && actor.connection(p.lane).is_autocommit()
                && p.seal.authenticates(
                    p.id, p.cancellation_sequence, p.lane,
                    p.connection_generation, p.observed_owner_word,
                    p.is_busy_after, p.is_autocommit_after,
                ),
        CancelEndFact::CompletedOutsideTransaction(p) =>
            !p.is_busy_after && p.is_autocommit_after
                && post_finalize_source_matches(
                    actor, control, &p.source,
                    |s| s == SqlStatementClass::PrepareAuthority,
                    |_| false,
                )
                && p.seal.authenticates(&p.source),
        CancelEndFact::BeginDidNotOpen(p) =>
            !p.is_busy_after && p.is_autocommit_after
                && post_finalize_source_matches(
                    actor, control, &p.source,
                    |s| s == SqlStatementClass::Begin,
                    |phase| phase == ActorPhase::BeginNotOpened,
                )
                && p.seal.authenticates(&p.source),
        CancelEndFact::RolledBack(p) =>
            !p.is_busy_after && p.is_autocommit_after
                && post_finalize_source_matches(
                    actor, control, &p.source,
                    |s| s == SqlStatementClass::CleanupRollback,
                    |_| false,
                )
                && p.seal.authenticates(&p.source),
        CancelEndFact::SQLiteAlreadyRolledBack(p) =>
            !p.is_busy_after && p.is_autocommit_after
                && post_finalize_source_matches(
                    actor, control, &p.source,
                    |s| matches!(s,
                        SqlStatementClass::Begin
                        | SqlStatementClass::SnapshotMarker
                        | SqlStatementClass::OperationAuthority
                        | SqlStatementClass::Data
                        | SqlStatementClass::FrameControl
                        | SqlStatementClass::CleanupRollback),
                    |phase| matches!(phase,
                        ActorPhase::Idle
                        | ActorPhase::BetweenStatementAndCommit),
                )
                && p.seal.authenticates(&p.source),
    }
}

fn post_finalize_source_matches(
    actor: &AppActor,
    control: &ReservationControl,
    source: &PostFinalizeSource,
    statement_allowed: impl Fn(SqlStatementClass) -> bool,
    idle_phase_allowed: impl Fn(ActorPhase) -> bool,
) -> bool {
    match source {
        PostFinalizeSource::Statement(completed) =>
            completed.id == control.id
                && completed.connection_generation
                    == actor.generation_for(completed.lane)
                && completed.logical_cancellation_sequence
                    == control.current_terminal_sequence.load(Acquire)
                && statement_allowed(completed.statement),
        PostFinalizeSource::ActorIdle {
            id, actor_generation, terminal_sequence, lease_id, lane,
            connection_generation, phase, is_busy_after,
            is_autocommit_after, seal,
        } => *id == control.id
            && *actor_generation == control.actor_generation
            && *terminal_sequence
                == control.current_terminal_sequence.load(Acquire)
            && actor.session_lease_exact(control.id)
                .is_some_and(|lease| lease.lease_id == *lease_id)
            && *connection_generation == actor.generation_for(*lane)
            && !*is_busy_after
            && *is_autocommit_after
            && !actor.connection(*lane).is_busy()
            && actor.connection(*lane).is_autocommit()
            && idle_phase_allowed(*phase)
            && seal.authenticates(
                *id, *terminal_sequence, *lease_id, *lane,
                *connection_generation, *phase,
                *is_busy_after, *is_autocommit_after,
            ),
    }
}

fn ordinary_retirement(
    control: &ReservationControl,
) -> Result<BackendTerminalRetirement, ActorError> {
    control.terminal_retirement.get().cloned()
        .ok_or(ActorError::CancellationProtocolMismatch)
}

// Preparation is side-effect-free with respect to retained outcome. It validates,
// derives the exact event, pins/reserves its endpoint, and returns a value owned
// solely by the cutoff candidate. A losing candidate is simply dropped.
fn prepare_root_result(
    control: &Arc<ReservationControl>,
    proof: RootFinishResult,
) -> Result<Arc<PreparedTerminalDelivery<RootFinishResult>>, ActorError> {
    let (delivery_id, key, token, pinned, attempt, retirement) = {
        let _owner = control.terminal_owner_gate.lock();
        if control.generation_fenced.load(Acquire)
            || owner(control.terminal.load(Acquire)) != OWNER_COMPLETE
        {
            return Err(ActorError::ActorUnavailable);
        }
        let TerminalDelivery::ExplicitRoot {
            key, token, cutoff, registry,
        } = control.terminal_delivery.lock().clone()
            .ok_or(ActorError::CancellationProtocolMismatch)?
        else {
            return Err(ActorError::CancellationProtocolMismatch);
        };
        let pinned = registry.pin_and_reserve_exact(&key, cutoff.delivery_id)
            .map_err(|_| ActorError::ActorUnavailable)?;
        let attempt = control.terminal_attempt.lock().clone()
            .ok_or(ActorError::CancellationProtocolMismatch)?;
        (cutoff.delivery_id, key, token, pinned, attempt,
         ordinary_retirement(control)?)
    };
    let proof = Arc::new(proof);
    let candidate = root_outcome(&attempt, &proof)?;
    let endpoint = Arc::new(move |id, retained: Arc<RootFinishResult>| {
        pinned.enqueue_once_infallible(id, RegistryEvent::Routed {
            key: key.clone(),
            event: TxEvent::TerminalCompleted {
                token,
                result: (*retained).clone(),
                retirement: retirement.clone(),
            },
        });
    });
    Ok(make_prepared_delivery(
        delivery_id, proof, endpoint,
        OutcomeCommitPlan {
            retention: control.retention.clone(),
            candidate,
            public_kind: ExplicitHardStopKind::Root,
        },
    ))
}

fn prepare_cancel_result(
    control: &Arc<ReservationControl>,
    proof: CancelAck,
    candidate: ActorTerminalOutcome,
) -> Result<Arc<PreparedTerminalDelivery<CancelAck>>, ActorError> {
    let (delivery_id, key, token, pinned, cause, retirement) = {
        let _owner = control.terminal_owner_gate.lock();
        if control.generation_fenced.load(Acquire)
            || owner(control.terminal.load(Acquire)) != OWNER_CANCEL
        {
            return Err(ActorError::ActorUnavailable);
        }
        let TerminalDelivery::ExplicitCancel {
            key, token, cutoff, registry,
        } = control.terminal_delivery.lock().clone()
            .ok_or(ActorError::CancellationProtocolMismatch)?
        else {
            return Err(ActorError::CancellationProtocolMismatch);
        };
        let cause = match control.terminal_attempt.lock().as_ref() {
            Some(TerminalAttempt::Cancellation { cause, .. }) => cause.clone(),
            _ => return Err(ActorError::CancellationProtocolMismatch),
        };
        let pinned = registry.pin_and_reserve_exact(&key, cutoff.delivery_id)
            .map_err(|_| ActorError::ActorUnavailable)?;
        (cutoff.delivery_id, key, token, pinned, cause,
         ordinary_retirement(control)?)
    };
    let projection_matches = match (&proof, &candidate) {
        (CancelAck::NoTransaction,
         ActorTerminalOutcome::Cancelled {
             cause: stored,
             cleanup: CleanupDisposition::NoSqlStarted
                 | CleanupDisposition::OutsideTransactionCompleted
                 | CleanupDisposition::BeginDidNotOpen,
         }) => stored == &cause,
        (CancelAck::RolledBack,
         ActorTerminalOutcome::Cancelled {
             cause: stored,
             cleanup: CleanupDisposition::RolledBack
                 | CleanupDisposition::SQLiteAlreadyRolledBack,
         }) => stored == &cause,
        (CancelAck::Indeterminate(a),
         ActorTerminalOutcome::CleanupIndeterminate(b)) => a == b,
        _ => false,
    };
    if !projection_matches {
        return Err(ActorError::TerminalReplyProjectionMismatch);
    }
    let candidate = Arc::new(candidate);
    let proof = Arc::new(proof);
    let endpoint = Arc::new(move |id, retained: Arc<CancelAck>| {
        pinned.enqueue_once_infallible(id, RegistryEvent::Routed {
            key: key.clone(),
            event: TxEvent::CancellationCompleted {
                token,
                result: retained,
                retirement: retirement.clone(),
            },
        });
    });
    Ok(make_prepared_delivery(
        delivery_id, proof, endpoint,
        OutcomeCommitPlan {
            retention: control.retention.clone(),
            candidate,
            public_kind: ExplicitHardStopKind::Cancel,
        },
    ))
}

fn prepare_data_abort_result(
    control: &Arc<ReservationControl>,
    proof: DataAbortProof,
) -> Result<Arc<PreparedTerminalDelivery<DataAbortProof>>, ActorError> {
    let (delivery_id, key, data_token, pinned) = {
        let _owner = control.terminal_owner_gate.lock();
        if control.generation_fenced.load(Acquire)
            || owner(control.terminal.load(Acquire)) != OWNER_COMPLETE
        {
            return Err(ActorError::ActorUnavailable);
        }
        let TerminalDelivery::ExplicitDataAbort {
            key, data_token, cutoff, registry, ..
        } = control.terminal_delivery.lock().clone()
            .ok_or(ActorError::CancellationProtocolMismatch)?
        else {
            return Err(ActorError::CancellationProtocolMismatch);
        };
        let pinned = registry.pin_and_reserve_exact(&key, cutoff.delivery_id)
            .map_err(|_| ActorError::ActorUnavailable)?;
        (cutoff.delivery_id, key, data_token, pinned)
    };
    let candidate = Arc::new(
        ActorTerminalOutcome::TransactionAborted(proof.error.clone()),
    );
    let proof = Arc::new(proof);
    let endpoint = Arc::new(move |id, retained: Arc<DataAbortProof>| {
        pinned.enqueue_once_infallible(id, RegistryEvent::Routed {
            key: key.clone(),
            event: TxEvent::DataAbortCompleted {
                token: data_token,
                proof: retained,
            },
        });
    });
    Ok(make_prepared_delivery(
        delivery_id, proof, endpoint,
        OutcomeCommitPlan {
            retention: control.retention.clone(),
            candidate,
            public_kind: ExplicitHardStopKind::Root,
        },
    ))
}

fn prepare_autocommit_result(
    control: &Arc<ReservationControl>,
    proof: ActorTerminalOutcome,
) -> Result<Arc<PreparedTerminalDelivery<ActorTerminalOutcome>>, ActorError> {
    let (delivery_id, pinned, public_kind) = {
        let _owner = control.terminal_owner_gate.lock();
        let word = control.terminal.load(Acquire);
        if control.generation_fenced.load(Acquire)
            || !is_terminal_owner(owner(word))
        {
            return Err(ActorError::ActorUnavailable);
        }
        let TerminalDelivery::Autocommit { sink } =
            control.terminal_delivery.lock().clone()
                .ok_or(ActorError::CancellationProtocolMismatch)?
        else {
            return Err(ActorError::CancellationProtocolMismatch);
        };
        let ReservationCutoff::Autocommit(cutoff) = &control.terminal_cutoff
        else {
            return Err(ActorError::CancellationProtocolMismatch);
        };
        let pinned = sink.pin_and_reserve(cutoff.delivery_id)
            .map_err(|_| ActorError::ActorUnavailable)?;
        let kind = if owner(word) == OWNER_CANCEL {
            ExplicitHardStopKind::Cancel
        } else {
            ExplicitHardStopKind::Root
        };
        (cutoff.delivery_id, pinned, kind)
    };
    let proof = Arc::new(proof);
    let candidate = proof.clone();
    let endpoint = Arc::new(move |id, retained| {
        pinned.enqueue_once_infallible(id, retained);
    });
    Ok(make_prepared_delivery(
        delivery_id, proof, endpoint,
        OutcomeCommitPlan {
            retention: control.retention.clone(),
            candidate,
            public_kind,
        },
    ))
}

// These are the only ordinary terminal publishers. The caller must supply a
// class-specific, single-use terminal capability minted by the exact statement
// finalizer. Preparation performs every fallible endpoint check before cutoff
// selection; a winning cutoff commits retention before keyed visibility.
fn publish_root_terminal_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    end: TerminalEndCapability, // Root or class-matching Uncertain only
    result: RootFinishResult,
) -> Result<(), ActorError> {
    let budget_sequence = terminal_capability_sequence(&end);
    if !matches!(&end,
        TerminalEndCapability::Root(_)
        | TerminalEndCapability::Uncertain(UncertainEndCapability {
            class: TerminalPublicationClass::Root, ..
        }))
    {
        return Err(ActorError::CancellationProtocolMismatch);
    }
    install_terminal_retirement_exact(actor, control, end)?;
    let prepared = prepare_root_result(control, result)?;
    let ReservationCutoff::Explicit { root, .. } = &control.terminal_cutoff
    else { return Err(ActorError::CancellationProtocolMismatch); };
    if root.publish_prepared_result(prepared) {
        actor.disarm_terminal_budget_after_retention_exact(
            control, budget_sequence,
        );
    }
    Ok(())
}

enum CancelTerminalConclusion {
    Clean(CancelEndCapability),
    Indeterminate {
        end: UncertainEndCapability,
        error: DbError,
    },
}

fn project_cancel_conclusion(
    conclusion: &CancelTerminalConclusion,
) -> Result<(CancelAck, ActorTerminalOutcome), ActorError> {
    match conclusion {
        CancelTerminalConclusion::Clean(cap) => {
            let cleanup = match &cap.fact {
                CancelEndFact::NoStatementStarted(_) =>
                    CleanupDisposition::NoSqlStarted,
                CancelEndFact::CompletedOutsideTransaction(_) =>
                    CleanupDisposition::OutsideTransactionCompleted,
                CancelEndFact::BeginDidNotOpen(_) =>
                    CleanupDisposition::BeginDidNotOpen,
                CancelEndFact::RolledBack(_) =>
                    CleanupDisposition::RolledBack,
                CancelEndFact::SQLiteAlreadyRolledBack(_) =>
                    CleanupDisposition::SQLiteAlreadyRolledBack,
            };
            let ack = match cleanup {
                CleanupDisposition::NoSqlStarted
                | CleanupDisposition::OutsideTransactionCompleted
                | CleanupDisposition::BeginDidNotOpen =>
                    CancelAck::NoTransaction,
                CleanupDisposition::RolledBack
                | CleanupDisposition::SQLiteAlreadyRolledBack =>
                    CancelAck::RolledBack,
            };
            Ok((ack, ActorTerminalOutcome::Cancelled {
                cause: cap.cause.clone(), cleanup,
            }))
        }
        CancelTerminalConclusion::Indeterminate { error, .. } =>
            Ok((
                CancelAck::Indeterminate(error.clone()),
                ActorTerminalOutcome::CleanupIndeterminate(error.clone()),
            )),
    }
}

fn publish_cancel_terminal_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    conclusion: CancelTerminalConclusion,
) -> Result<(), ActorError> {
    let (ack, exact_outcome) = project_cancel_conclusion(&conclusion)?;
    let budget_sequence = match &conclusion {
        CancelTerminalConclusion::Clean(cap) =>
            cancel_end_sequence(&cap.fact),
        CancelTerminalConclusion::Indeterminate { end, .. } =>
            uncertain_end_sequence(end),
    };
    let capability = match conclusion {
        CancelTerminalConclusion::Clean(cap) =>
            TerminalEndCapability::Cancel(cap),
        CancelTerminalConclusion::Indeterminate { end, .. } => {
            if end.class != TerminalPublicationClass::Cancel {
                return Err(ActorError::CancellationProtocolMismatch);
            }
            TerminalEndCapability::Uncertain(end)
        }
    };
    install_terminal_retirement_exact(actor, control, capability)?;
    let prepared = prepare_cancel_result(control, ack, exact_outcome)?;
    let ReservationCutoff::Explicit { cancel, .. } = &control.terminal_cutoff
    else { return Err(ActorError::CancellationProtocolMismatch); };
    if cancel.publish_prepared_result(prepared) {
        actor.disarm_terminal_budget_after_retention_exact(
            control, budget_sequence,
        );
    }
    Ok(())
}

fn publish_data_abort_terminal_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    cap: DataAbortEndCapability,
    error: DbError,
) -> Result<(), ActorError> {
    let budget_sequence = cap.postrollback_authority.as_ref()
        .map(|proof| proof.command_sequence)
        .unwrap_or(cap.completed_end.command_sequence);
    let retirement = install_terminal_retirement_exact(
        actor, control, TerminalEndCapability::DataAbort(cap),
    )?;
    let prepared = prepare_data_abort_result(
        control, DataAbortProof { error, retirement },
    )?;
    let cutoff = match control.terminal_delivery.lock().as_ref() {
        Some(TerminalDelivery::ExplicitDataAbort { cutoff, .. }) =>
            cutoff.clone(),
        _ => return Err(ActorError::CancellationProtocolMismatch),
    };
    if cutoff.publish_prepared_result(prepared) {
        actor.disarm_terminal_budget_after_retention_exact(
            control, Some(budget_sequence),
        );
    }
    Ok(())
}

enum AutocommitTerminalConclusion {
    CommitConfirmed {
        completed: CompletedSqlTargetProof,
        value: SharedDbResult,
    },
    CommitDefinitelyFailed {
        terminal: CompletedSqlTargetProof,
        cleanup: CompletedSqlTargetProof,
        error: DbError,
    },
    OperationFailedCleanupConfirmed {
        completed: CompletedSqlTargetProof,
        error: DbError,
    },
    SnapshotAbortClassified {
        // `ended` is Data when SQLite had already auto-rolled back, otherwise
        // the exact OWNER_COMPLETE FailureCleanupRollback proof.
        ended: CompletedSqlTargetProof,
        // Always a fresh read on op_conn's platform-role session and always
        // sequenced after `ended`; it never traverses the data snapshot.
        authority: CompletedSqlTargetProof,
        error: DbError,
        seal: SnapshotAbortEndSeal,
    },
    CancellationConfirmed {
        end: CancelEndCapability,
    },
    CommitIndeterminate {
        sequence: u64,
        error: DbError,
    },
    CleanupIndeterminate {
        sequence: u64,
        error: DbError,
    },
}

fn derive_autocommit_outcome_exact(
    control: &ReservationControl,
    conclusion: &AutocommitTerminalConclusion,
) -> Result<ActorTerminalOutcome, ActorError> {
    let word = control.terminal.load(Acquire);
    let attempt = control.terminal_attempt.lock();
    match (owner(word), attempt.as_ref(), conclusion) {
        (OWNER_COMPLETE, Some(TerminalAttempt::AutocommitSuccess(expected)),
         AutocommitTerminalConclusion::CommitConfirmed { value, .. })
            if Arc::ptr_eq(expected, value) =>
            Ok(ActorTerminalOutcome::Autocommit(Ok(value.clone()))),
        (OWNER_COMPLETE, Some(TerminalAttempt::AutocommitFailure(expected)),
         AutocommitTerminalConclusion::OperationFailedCleanupConfirmed {
             error, ..
         }) if expected == error =>
            Ok(ActorTerminalOutcome::Autocommit(Err(error.clone()))),
        (OWNER_COMPLETE,
         Some(TerminalAttempt::AutocommitSnapshotAbort(expected)),
         AutocommitTerminalConclusion::SnapshotAbortClassified {
             error, ..
         }) if expected == error =>
            Ok(ActorTerminalOutcome::Autocommit(Err(error.clone()))),
        (OWNER_COMPLETE, Some(TerminalAttempt::AutocommitSuccess(_)),
         AutocommitTerminalConclusion::CommitDefinitelyFailed { error, .. }) =>
            Ok(ActorTerminalOutcome::Autocommit(Err(error.clone()))),
        (OWNER_CANCEL, Some(TerminalAttempt::Cancellation { cause, .. }),
         AutocommitTerminalConclusion::CancellationConfirmed { end })
            if cause == &end.cause =>
            Ok(ActorTerminalOutcome::Cancelled {
                cause: end.cause.clone(),
                cleanup: match &end.fact {
                    CancelEndFact::NoStatementStarted(_) =>
                        CleanupDisposition::NoSqlStarted,
                    CancelEndFact::CompletedOutsideTransaction(_) =>
                        CleanupDisposition::OutsideTransactionCompleted,
                    CancelEndFact::BeginDidNotOpen(_) =>
                        CleanupDisposition::BeginDidNotOpen,
                    CancelEndFact::RolledBack(_) =>
                        CleanupDisposition::RolledBack,
                    CancelEndFact::SQLiteAlreadyRolledBack(_) =>
                        CleanupDisposition::SQLiteAlreadyRolledBack,
                },
            }),
        (OWNER_COMPLETE, _, AutocommitTerminalConclusion::CommitIndeterminate {
             error, ..
         }) => Ok(ActorTerminalOutcome::AutocommitIndeterminate(error.clone())),
        (OWNER_COMPLETE | OWNER_CANCEL, _,
         AutocommitTerminalConclusion::CleanupIndeterminate { error, .. }) =>
            Ok(ActorTerminalOutcome::CleanupIndeterminate(error.clone())),
        _ => Err(ActorError::TerminalReplyProjectionMismatch),
    }
}

fn publish_autocommit_terminal_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    conclusion: AutocommitTerminalConclusion,
) -> Result<(), ActorError> {
    let sequence = autocommit_conclusion_sequence(&conclusion);
    validate_autocommit_end_capability_exact(actor, control, &conclusion)?;
    let result = derive_autocommit_outcome_exact(control, &conclusion)?;
    let prepared = prepare_autocommit_result(control, result)?;
    let ReservationCutoff::Autocommit(cutoff) = &control.terminal_cutoff
    else { return Err(ActorError::CancellationProtocolMismatch); };
    if cutoff.publish_prepared_result(prepared) {
        consume_autocommit_end_capability(conclusion);
        actor.disarm_terminal_budget_after_retention_exact(control, sequence);
    }
    Ok(())
}

// Disarm occurs only after the selected cutoff contains a retained result. The
// exact sequence must equal current_terminal_sequence; sequence=None is legal
// only for a NoStatementStarted cancellation capability.
fn disarm_terminal_budget_after_retention_exact(
    &self,
    control: &ReservationControl,
    sequence: Option<u64>,
) {
    assert!(control.retention.outcome.get().is_some());
    if let Some(sequence) = sequence {
        assert_eq!(control.current_terminal_sequence.load(Acquire), sequence);
    }
    control.terminal_interrupt_generation.store(0, Release);
    control.terminal_interrupt_sequence.store(0, Release);
    control.terminal_watchdog_armed.store(0, Release);
    control.terminal_hard_stop_armed.store(0, Release);
}

fn selected_terminal_delivery_id(
    control: &ReservationControl,
) -> Result<TerminalDeliveryId, ActorError> {
    let delivery = control.terminal_delivery.lock();
    match (delivery.as_ref(), &control.terminal_cutoff) {
        (Some(TerminalDelivery::ExplicitRoot { cutoff, .. }),
         ReservationCutoff::Explicit { root, .. })
            if Arc::ptr_eq(cutoff, root) => Ok(cutoff.delivery_id),
        (Some(TerminalDelivery::ExplicitCancel { cutoff, .. }),
         ReservationCutoff::Explicit { cancel, .. })
            if Arc::ptr_eq(cutoff, cancel) => Ok(cutoff.delivery_id),
        (Some(TerminalDelivery::ExplicitDataAbort { cutoff, .. }),
         ReservationCutoff::Explicit { .. }) => Ok(cutoff.delivery_id),
        (Some(TerminalDelivery::Autocommit { .. }),
         ReservationCutoff::Autocommit(cutoff)) => Ok(cutoff.delivery_id),
        _ => Err(ActorError::CancellationProtocolMismatch),
    }
}

fn terminal_capability_sequence(end: &TerminalEndCapability) -> Option<u64> {
    match end {
        TerminalEndCapability::Root(cap) => Some(
            cap.cleanup.as_ref()
                .map(|proof| proof.command_sequence)
                .unwrap_or(cap.terminal.command_sequence),
        ),
        TerminalEndCapability::Cancel(cap) => cancel_end_sequence(&cap.fact),
        TerminalEndCapability::DataAbort(cap) => Some(
            cap.postrollback_authority.as_ref()
                .map(|proof| proof.command_sequence)
                .unwrap_or(cap.completed_end.command_sequence),
        ),
        TerminalEndCapability::Uncertain(cap) => cap.sequence,
    }
}

fn cancel_end_sequence(fact: &CancelEndFact) -> Option<u64> {
    match fact {
        CancelEndFact::NoStatementStarted(_) => None,
        CancelEndFact::CompletedOutsideTransaction(p)
        | CancelEndFact::BeginDidNotOpen(p)
        | CancelEndFact::RolledBack(p)
        | CancelEndFact::SQLiteAlreadyRolledBack(p) => match &p.source {
            PostFinalizeSource::Statement(completed) =>
                Some(completed.command_sequence),
            PostFinalizeSource::ActorIdle { terminal_sequence, .. } =>
                Some(*terminal_sequence),
        },
    }
}

fn uncertain_end_sequence(end: &UncertainEndCapability) -> Option<u64> {
    end.sequence
}

fn autocommit_conclusion_sequence(
    conclusion: &AutocommitTerminalConclusion,
) -> Option<u64> {
    match conclusion {
        AutocommitTerminalConclusion::CommitConfirmed { completed, .. }
        | AutocommitTerminalConclusion::OperationFailedCleanupConfirmed {
            completed, ..
        } => Some(completed.command_sequence),
        AutocommitTerminalConclusion::SnapshotAbortClassified {
            authority, ..
        } => Some(authority.command_sequence),
        AutocommitTerminalConclusion::CommitDefinitelyFailed {
            cleanup, ..
        } => Some(cleanup.command_sequence),
        AutocommitTerminalConclusion::CancellationConfirmed { end } =>
            cancel_end_sequence(&end.fact),
        AutocommitTerminalConclusion::CommitIndeterminate { sequence, .. }
        | AutocommitTerminalConclusion::CleanupIndeterminate {
            sequence, ..
        } => Some(*sequence),
    }
}

fn validate_autocommit_end_capability_exact(
    actor: &AppActor,
    control: &ReservationControl,
    conclusion: &AutocommitTerminalConclusion,
) -> Result<(), ActorError> {
    let _owner = control.terminal_owner_gate.lock();
    if actor.active_sql.lock().is_some()
        || control.generation_fenced.load(Acquire)
        || !matches!(control.terminal_cutoff, ReservationCutoff::Autocommit(_))
    {
        return Err(ActorError::CancellationProtocolMismatch);
    }
    let word = control.terminal.load(Acquire);
    let valid = match conclusion {
        AutocommitTerminalConclusion::CommitConfirmed { completed, .. } =>
            owner(word) == OWNER_COMPLETE
                && completed.statement == SqlStatementClass::Commit
                && validate_completed_target(
                    actor, control, completed, ConnectionLane::Op,
                    |s| s == SqlStatementClass::Commit,
                ),
        AutocommitTerminalConclusion::CommitDefinitelyFailed {
            terminal, cleanup, ..
        } => owner(word) == OWNER_COMPLETE
            && terminal.id == control.id
            && terminal.lane == ConnectionLane::Op
            && terminal.connection_generation
                == actor.generation_for(ConnectionLane::Op)
            && terminal.statement == SqlStatementClass::Commit
            && cleanup.statement == SqlStatementClass::FailureCleanupRollback
            && validate_completed_target(
                actor, control, cleanup, ConnectionLane::Op,
                |s| s == SqlStatementClass::FailureCleanupRollback,
            ),
        AutocommitTerminalConclusion::OperationFailedCleanupConfirmed {
            completed, ..
        } => owner(word) == OWNER_COMPLETE
            && matches!(completed.statement,
                SqlStatementClass::Data
                    | SqlStatementClass::FailureCleanupRollback)
            && validate_completed_target(
                actor, control, completed, ConnectionLane::Op,
                |s| matches!(s, SqlStatementClass::Data
                    | SqlStatementClass::FailureCleanupRollback),
            ),
        AutocommitTerminalConclusion::SnapshotAbortClassified {
            ended, authority, seal, ..
        } => owner(word) == OWNER_COMPLETE
            && ended.id == control.id
            && ended.lane == ConnectionLane::Op
            && ended.connection_generation
                == actor.generation_for(ConnectionLane::Op)
            && matches!(ended.statement,
                SqlStatementClass::Data
                    | SqlStatementClass::FailureCleanupRollback)
            && authority.id == control.id
            && authority.lane == ConnectionLane::Op
            && authority.connection_generation
                == actor.generation_for(ConnectionLane::Op)
            && authority.statement
                == SqlStatementClass::PostRollbackAuthority
            && authority.command_sequence
                == control.current_terminal_sequence.load(Acquire)
            && !actor.op_conn.is_busy()
            && actor.op_conn.is_autocommit()
            && seal.authenticates_exact_chain(
                control.id, ended, authority,
            ),
        AutocommitTerminalConclusion::CancellationConfirmed { end } =>
            owner(word) == OWNER_CANCEL
                && end.id == control.id
                && end.actor_generation == control.actor_generation
                && end.delivery_id == selected_terminal_delivery_id(control)?
                && end.observed_owner_word == word
                && validate_cancel_end_fact_exact(actor, control, &end.fact)
                && end.seal.authenticates(
                    end.id, end.delivery_id, &end.cause, &end.fact, word,
                ),
        AutocommitTerminalConclusion::CommitIndeterminate { sequence, .. } =>
            owner(word) == OWNER_COMPLETE
                && *sequence == control.current_terminal_sequence.load(Acquire),
        AutocommitTerminalConclusion::CleanupIndeterminate { sequence, .. } =>
            is_terminal_owner(owner(word))
                && *sequence == control.current_terminal_sequence.load(Acquire),
    };
    if valid { Ok(()) } else { Err(ActorError::TerminalReplyProjectionMismatch) }
}

fn consume_autocommit_end_capability(_: AutocommitTerminalConclusion) {
    // Taking the non-Clone enum by value is the consumption event. Its private
    // seals cannot be reused for another reservation or terminal attempt.
}

// Fence-route constructors run before logical fence arbitration. Each validates
// the full trigger and pins/reserves the endpoint, then captures only plain
// identity/value data, the independent retention cell, and that endpoint.
// Completing it with a physical proof has no failure arm.
//
// | Trigger | Proof/outcome built from physical proof | Keyed endpoint |
// | --- | --- | --- |
// | ExplicitRoot | Failed(terminal_deadline_exceeded,Indeterminate); CommitIndeterminate/RollbackFailed | TerminalHardStopCompleted |
// | ExplicitCancel | Indeterminate(cleanup timeout with cause); CleanupIndeterminate | CancellationHardStopCompleted |
// | ExplicitDataAbort | DataAbortProof with GenerationRetired; TransactionAborted(source) | DataAbortCompleted |
// | Autocommit | AutocommitIndeterminate or CleanupIndeterminate retaining original/cause | actor terminal sink |
// | Generation death before SC-1 grace transition | same typed indeterminate form from reserve-time route | TerminalHardStopCompleted/CancellationHardStopCompleted, accepted in Settling/Awaiting with GenerationRetired |
// | Unexpected raw SQLITE_INTERRUPT | unexpected_sqlite_interrupt for this target | selected target cutoff/sink; other controls get actor_unavailable |
//
fn prepare_root_fence_route(
    control: &Arc<ReservationControl>,
    key: &TxKey,
    cutoff: &Arc<TerminalCutoffGate<RootFinishResult>>,
    fence_token: CommandToken,
    registry: &TxRegistrySender,
) -> Result<Arc<PreparedFenceRoute<RootFinishResult>>, ActorError> {
    let pinned = registry.pin_and_reserve_exact(key, cutoff.delivery_id)
        .map_err(|_| ActorError::ActorUnavailable)?;
    let id = control.id;
    let actor_generation = control.actor_generation;
    let key = key.clone();
    let retention = control.retention.clone();
    let delivery_id = cutoff.delivery_id;
    Ok(Arc::new(PreparedFenceRoute {
        delivery_id,
        complete: Arc::new(move |snapshot, physical| {
            assert_eq!(physical.id, id);
            assert_eq!(physical.actor_generation, actor_generation);
            assert_eq!(physical.cutoff_delivery_id, delivery_id);
            assert_eq!(snapshot.delivery_id, delivery_id);
            let snapshot_decision = match snapshot.attempt {
                TerminalAttemptSnapshot::ExplicitCommit => RootDecision::Commit,
                TerminalAttemptSnapshot::ExplicitRollback => RootDecision::Rollback,
                _ => panic!("root fence bound to non-root attempt"),
            };
            let error = snapshot.source.clone()
                .unwrap_or_else(terminal_deadline_exceeded);
            let proof = Arc::new(RootFinishResult::Failed {
                error: error.clone(),
                certainty: FinishCertainty::Indeterminate,
            });
            let candidate = Arc::new(match snapshot_decision {
                RootDecision::Commit =>
                    ActorTerminalOutcome::CommitIndeterminate(error),
                RootDecision::Rollback =>
                    ActorTerminalOutcome::RollbackFailed(error),
            });
            let retirement = sc1_fence_retirement(
                id, actor_generation, key.clone(),
                GenerationRetirementId(delivery_id.0), physical,
            );
            let endpoint = {
                let pinned = pinned.clone();
                let key = key.clone();
                Arc::new(move |event_id, _retained: Arc<RootFinishResult>| {
                    pinned.enqueue_once_infallible(event_id,
                        RegistryEvent::Routed {
                            key: key.clone(),
                            event: TxEvent::TerminalHardStopCompleted {
                                token: fence_token,
                                resource_generation: actor_generation,
                                proof: retirement.clone(),
                            },
                        });
                })
            };
            make_prepared_delivery(
                delivery_id, proof, endpoint,
                OutcomeCommitPlan {
                    retention: retention.clone(),
                    candidate,
                    public_kind: ExplicitHardStopKind::Root,
                },
            )
        }),
    }))
}

// The remaining constructors implement their table rows with the same complete
// algorithm as prepare_root_fence_route. No declaration is optional: the
// HardStopTrigger match below invokes exactly one and cannot use a wildcard.
fn prepare_cancel_fence_route(
    control: &Arc<ReservationControl>,
    key: &TxKey,
    cutoff: &Arc<TerminalCutoffGate<CancelAck>>,
    fence_token: CommandToken,
    registry: &TxRegistrySender,
) -> Result<Arc<PreparedFenceRoute<CancelAck>>, ActorError> {
    let pinned = registry.pin_and_reserve_exact(key, cutoff.delivery_id)
        .map_err(|_| ActorError::ActorUnavailable)?;
    let id = control.id;
    let actor_generation = control.actor_generation;
    let key = key.clone();
    let retention = control.retention.clone();
    let delivery_id = cutoff.delivery_id;
    Ok(Arc::new(PreparedFenceRoute {
        delivery_id,
        complete: Arc::new(move |snapshot, physical| {
            assert_eq!(physical.id, id);
            assert_eq!(physical.actor_generation, actor_generation);
            assert_eq!(physical.cutoff_delivery_id, delivery_id);
            let snapshot_cause = match &snapshot.attempt {
                TerminalAttemptSnapshot::ExplicitCancel { cause, .. } =>
                    cause.clone(),
                _ => panic!("cancel fence bound to non-cancel attempt"),
            };
            let error = snapshot.source.clone().unwrap_or_else(||
                cleanup_timeout_with_cause(snapshot_cause));
            let proof = Arc::new(CancelAck::Indeterminate(error.clone()));
            let candidate = Arc::new(
                ActorTerminalOutcome::CleanupIndeterminate(error),
            );
            let retirement = sc1_fence_retirement(
                id, actor_generation, key.clone(),
                GenerationRetirementId(delivery_id.0), physical,
            );
            let endpoint = {
                let pinned = pinned.clone();
                let key = key.clone();
                Arc::new(move |event_id, retained: Arc<CancelAck>| {
                    pinned.enqueue_once_infallible(event_id,
                        RegistryEvent::Routed {
                            key: key.clone(),
                            event: TxEvent::CancellationHardStopCompleted {
                                token: fence_token,
                                resource_generation: actor_generation,
                                result: retained,
                                proof: retirement.clone(),
                            },
                        });
                })
            };
            make_prepared_delivery(
                delivery_id, proof, endpoint,
                OutcomeCommitPlan {
                    retention: retention.clone(),
                    candidate,
                    public_kind: ExplicitHardStopKind::Cancel,
                },
            )
        }),
    }))
}

fn prepare_data_abort_fence_route(
    control: &Arc<ReservationControl>,
    key: &TxKey,
    cutoff: &Arc<TerminalCutoffGate<DataAbortProof>>,
    data_token: CommandToken,
    registry: &TxRegistrySender,
) -> Result<Arc<PreparedFenceRoute<DataAbortProof>>, ActorError> {
    let pinned = registry.pin_and_reserve_exact(key, cutoff.delivery_id)
        .map_err(|_| ActorError::ActorUnavailable)?;
    let id = control.id;
    let actor_generation = control.actor_generation;
    let key = key.clone();
    let retention = control.retention.clone();
    let delivery_id = cutoff.delivery_id;
    Ok(Arc::new(PreparedFenceRoute {
        delivery_id,
        complete: Arc::new(move |snapshot, physical| {
            assert_eq!(physical.id, id);
            assert_eq!(physical.actor_generation, actor_generation);
            let snapshot_source = match &snapshot.attempt {
                TerminalAttemptSnapshot::ExplicitDataAbort { source } =>
                    source.clone(),
                _ => panic!("data-abort fence bound to another attempt"),
            };
            assert_eq!(snapshot.source.as_ref(), Some(&snapshot_source));
            let retirement = BackendTerminalRetirement::GenerationRetired(
                sc1_fence_retirement(
                    id, actor_generation, key.clone(),
                    GenerationRetirementId(delivery_id.0), physical,
                ),
            );
            let proof = Arc::new(DataAbortProof {
                error: snapshot_source.clone(), retirement,
            });
            let candidate = Arc::new(
                ActorTerminalOutcome::TransactionAborted(snapshot_source),
            );
            let endpoint = {
                let pinned = pinned.clone();
                let key = key.clone();
                Arc::new(move |event_id, retained: Arc<DataAbortProof>| {
                    pinned.enqueue_once_infallible(event_id,
                        RegistryEvent::Routed {
                            key: key.clone(),
                            event: TxEvent::DataAbortCompleted {
                                token: data_token,
                                proof: retained,
                            },
                        });
                })
            };
            make_prepared_delivery(
                delivery_id, proof, endpoint,
                OutcomeCommitPlan {
                    retention: retention.clone(),
                    candidate,
                    public_kind: ExplicitHardStopKind::Root,
                },
            )
        }),
    }))
}

fn prepare_autocommit_fence_route(
    control: &Arc<ReservationControl>,
    cutoff: &Arc<TerminalCutoffGate<ActorTerminalOutcome>>,
    sink: &SharedActorTerminalSink,
) -> Result<Arc<PreparedFenceRoute<ActorTerminalOutcome>>, ActorError> {
    let pinned = sink.pin_and_reserve(cutoff.delivery_id)
        .map_err(|_| ActorError::ActorUnavailable)?;
    let id = control.id;
    let actor_generation = control.actor_generation;
    let retention = control.retention.clone();
    let delivery_id = cutoff.delivery_id;
    Ok(Arc::new(PreparedFenceRoute {
        delivery_id,
        complete: Arc::new(move |snapshot, physical| {
            assert_eq!(physical.id, id);
            assert_eq!(physical.actor_generation, actor_generation);
            assert_eq!(physical.cutoff_delivery_id, delivery_id);
            let (fallback, public_kind) = match &snapshot.attempt {
                TerminalAttemptSnapshot::AutocommitSuccess => (
                    ActorTerminalOutcome::AutocommitIndeterminate(
                        terminal_deadline_exceeded(),
                    ),
                    ExplicitHardStopKind::Root,
                ),
                TerminalAttemptSnapshot::AutocommitFailure { source } => (
                    ActorTerminalOutcome::CleanupIndeterminate(
                        cleanup_timeout_with_original(source.clone()),
                    ),
                    ExplicitHardStopKind::Root,
                ),
                TerminalAttemptSnapshot::AutocommitCancel { cause, .. } => (
                    ActorTerminalOutcome::CleanupIndeterminate(
                        cleanup_timeout_with_cause(cause.clone()),
                    ),
                    ExplicitHardStopKind::Cancel,
                ),
                TerminalAttemptSnapshot::AutocommitActorUnavailable => (
                    ActorTerminalOutcome::ActorUnavailable(
                        actor_unavailable_before_terminal_claim(),
                    ),
                    ExplicitHardStopKind::Root,
                ),
                _ => panic!("autocommit fence bound to explicit attempt"),
            };
            let proof = Arc::new(fallback.clone());
            let candidate = proof.clone();
            let endpoint = {
                let pinned = pinned.clone();
                Arc::new(move |event_id, retained| {
                    pinned.enqueue_once_infallible(event_id, retained);
                })
            };
            make_prepared_delivery(
                delivery_id, proof, endpoint,
                OutcomeCommitPlan {
                    retention: retention.clone(),
                    candidate,
                    public_kind,
                },
            )
        }),
    }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HardStopCutoffWinner { Fence, Result }

fn erase_cutoff_decision<T>(decision: CutoffDecision<T>)
    -> HardStopCutoffWinner
{
    match decision {
        CutoffDecision::FenceWon => HardStopCutoffWinner::Fence,
        CutoffDecision::ResultWon(_) => HardStopCutoffWinner::Result,
    }
}

fn hard_stop_owner_word_is_legal(trigger: &HardStopTrigger, word: u8) -> bool {
    match trigger {
        HardStopTrigger::ExplicitRoot { .. } => matches!(word,
            OWNER_OPEN | OWNER_COMPLETE | OPEN_WITH_CANCEL_INTENT
                | CANCEL_WITH_INTENT),
        HardStopTrigger::ExplicitCancel { .. } => matches!(word,
            OPEN_WITH_CANCEL_INTENT | CANCEL_WITH_INTENT),
        HardStopTrigger::ExplicitDataAbort { .. } => word == OWNER_COMPLETE,
        HardStopTrigger::Autocommit {
            preclaim_cancel_budget_id: Some(_), ..
        } => matches!(word, OPEN_WITH_CANCEL_INTENT | CANCEL_WITH_INTENT),
        HardStopTrigger::Autocommit {
            preclaim_cancel_budget_id: None, ..
        } => matches!(word, OWNER_COMPLETE | CANCEL_WITH_INTENT),
    }
}

fn validate_trigger_identity_and_deadline(
    control: &ReservationControl,
    trigger: &HardStopTrigger,
    generation: u64,
) -> bool {
    match (trigger, &control.terminal_cutoff) {
        (HardStopTrigger::ExplicitRoot {
            permit, decision, key, cutoff, deadline_slots, terminal_token,
            fence_token, registry, job,
        }, ReservationCutoff::Explicit { root, .. }) => {
            let bundle_ok = control.preinstalled_root.get()
                .is_some_and(|stored| stored.decision == *decision
                    && Arc::ptr_eq(&stored.hard_stop.permit, permit)
                    && stored.hard_stop.fence_token == *fence_token
                    && stored.hard_stop.trigger.job() == *job
                    && matches!(&stored.delivery,
                    TerminalDelivery::ExplicitRoot {
                        key: stored_key, cutoff: stored_cutoff, token,
                        registry: stored_registry,
                    } if stored_key == key
                        && token == terminal_token
                        && Arc::ptr_eq(stored_cutoff, cutoff)
                        && registry.same_endpoint(stored_registry)));
            let reserve_job_ok = control.preinstalled_explicit_root_fence.get()
                .is_some_and(|stored|
                    stored.job == *job && stored.fence_token == *fence_token);
            control.tx_key.as_ref() == Some(key)
                && Arc::ptr_eq(root, cutoff)
                && control.explicit_deadlines.as_ref()
                    .is_some_and(|p| Arc::ptr_eq(p, deadline_slots))
                && matches!(*deadline_slots.state.lock(),
                    ExplicitDeadlineState::Fired {
                        kind: DeadlineKind::TerminalHardStop,
                        generation: g,
                    } if g == generation)
                && permit.kind == ExplicitHardStopKind::Root
                && permit.key == *key
                && permit.actor_generation == control.actor_generation
                && permit.cutoff_delivery_id == cutoff.delivery_id
                && permit.completion_token == *terminal_token
                && permit.fence_token == *fence_token
                && permit.fence_job == *job
                && job.delivery_id == cutoff.delivery_id
                && bundle_ok
                && reserve_job_ok
        }
        (HardStopTrigger::ExplicitCancel {
            permit, key, cutoff, deadline_slots, cancellation_token,
            fence_token, registry, job,
        }, ReservationCutoff::Explicit { cancel, .. }) => {
            let delivery_ok = matches!(&control.cancel_delivery,
                TerminalDelivery::ExplicitCancel {
                    key: stored_key, cutoff: stored_cutoff, token,
                    registry: stored_registry,
                } if stored_key == key
                    && token == cancellation_token
                    && Arc::ptr_eq(stored_cutoff, cutoff)
                    && registry.same_endpoint(stored_registry));
            let preinstalled_ok = control.preinstalled_explicit_cancel_fence
                .get().is_some_and(|stored|
                    stored.job == *job && stored.fence_token == *fence_token);
            control.tx_key.as_ref() == Some(key)
                && Arc::ptr_eq(cancel, cutoff)
                && control.explicit_deadlines.as_ref()
                    .is_some_and(|p| Arc::ptr_eq(p, deadline_slots))
                && matches!(*deadline_slots.state.lock(),
                    ExplicitDeadlineState::Fired {
                        kind: DeadlineKind::CancellationHardStop,
                        generation: g,
                    } if g == generation)
                && permit.kind == ExplicitHardStopKind::Cancel
                && permit.key == *key
                && permit.actor_generation == control.actor_generation
                && permit.cutoff_delivery_id == cutoff.delivery_id
                && permit.completion_token == *cancellation_token
                && permit.fence_token == *fence_token
                && permit.fence_job == *job
                && job.delivery_id == cutoff.delivery_id
                && delivery_ok
                && preinstalled_ok
        }
        (HardStopTrigger::ExplicitDataAbort {
            permit, key, cutoff, data_token, registry, job,
        }, _) => control.tx_key.as_ref() == Some(key)
            && permit.key == *key
            && permit.actor_generation == control.actor_generation
            && permit.data_token == *data_token
            && permit.cutoff_delivery_id == cutoff.delivery_id
            && permit.fence_job == *job
            && job.delivery_id == cutoff.delivery_id
            && control.terminal_hard_stop_armed.load(Acquire) == generation
            && matches!(control.terminal_delivery.lock().as_ref(),
                Some(TerminalDelivery::ExplicitDataAbort {
                    permit: p, key: k, cutoff: c, data_token: t,
                    registry: r, ..
                }) if Arc::ptr_eq(p, permit) && k == key
                    && Arc::ptr_eq(c, cutoff) && t == data_token
                    && r.same_endpoint(registry)),
        (HardStopTrigger::Autocommit {
            cutoff, sink, preclaim_cancel_budget_id, job, ..
        }, ReservationCutoff::Autocommit(stored)) =>
            Arc::ptr_eq(cutoff, stored)
                && job.delivery_id == cutoff.delivery_id
                && control.terminal_hard_stop_armed.load(Acquire) == generation
                && matches!(&control.cancel_delivery,
                    TerminalDelivery::Autocommit { sink: s }
                        if s.same_endpoint(sink))
                && preclaim_cancel_budget_id.as_ref().map_or(true, |id|
                    control.preclaim_autocommit_cancel_budget.get()
                        .is_some_and(|b| b.budget_id == *id
                            && b.hard_stop_generation == generation)),
        _ => false,
    }
}

fn snapshot_terminal_attempt_locked(
    control: &ReservationControl,
    trigger: &HardStopTrigger,
    word: u8,
) -> Result<TerminalFenceSnapshot, ActorError> {
    let (delivery_id, class, public_kind, attempt, source) = match trigger {
        HardStopTrigger::ExplicitRoot { decision, cutoff, .. } => {
            let attempt = match decision {
                RootDecision::Commit => TerminalAttemptSnapshot::ExplicitCommit,
                RootDecision::Rollback => TerminalAttemptSnapshot::ExplicitRollback,
            };
            (cutoff.delivery_id, TerminalPublicationClass::Root,
             ExplicitHardStopKind::Root, attempt, None)
        }
        HardStopTrigger::ExplicitCancel { cause, cutoff, .. } => {
            let phase = match control.terminal_attempt.lock().as_ref() {
                Some(TerminalAttempt::Cancellation {
                    cause: stored, phase_proof, ..
                }) if stored == cause => *phase_proof,
                None if word == OPEN_WITH_CANCEL_INTENT =>
                    CancelPhaseProof::NoTransactionPossible,
                _ => return Err(ActorError::CancellationProtocolMismatch),
            };
            (cutoff.delivery_id, TerminalPublicationClass::Cancel,
             ExplicitHardStopKind::Cancel,
             TerminalAttemptSnapshot::ExplicitCancel {
                 cause: cause.clone(), phase,
             }, None)
        }
        HardStopTrigger::ExplicitDataAbort { cutoff, .. } => {
            let source = match control.terminal_attempt.lock().as_ref() {
                Some(TerminalAttempt::ExplicitTransactionAborted(error))
                | Some(TerminalAttempt::ExplicitSnapshotAbort(error)) =>
                    error.clone(),
                Some(TerminalAttempt::ExplicitSnapshotAbortPending) =>
                    actor_unavailable(),
                _ => return Err(ActorError::CancellationProtocolMismatch),
            };
            (cutoff.delivery_id, TerminalPublicationClass::DataAbort,
             ExplicitHardStopKind::Root,
             TerminalAttemptSnapshot::ExplicitDataAbort {
                 source: source.clone(),
             }, Some(source))
        }
        HardStopTrigger::Autocommit { cutoff, .. } => {
            let (attempt, kind, source) =
                match control.terminal_attempt.lock().as_ref() {
                    Some(TerminalAttempt::AutocommitSuccess(_)) => (
                        TerminalAttemptSnapshot::AutocommitSuccess,
                        ExplicitHardStopKind::Root, None,
                    ),
                    Some(TerminalAttempt::AutocommitFailure(error)) => (
                        TerminalAttemptSnapshot::AutocommitFailure {
                            source: error.clone(),
                        },
                        ExplicitHardStopKind::Root, Some(error.clone()),
                    ),
                    Some(TerminalAttempt::AutocommitSnapshotAbortPending(_)) => {
                        let source =
                            actor_unavailable_after_busy_snapshot();
                        (
                            TerminalAttemptSnapshot::AutocommitFailure {
                                source: source.clone(),
                            },
                            ExplicitHardStopKind::Root,
                            Some(source),
                        )
                    }
                    Some(TerminalAttempt::AutocommitSnapshotAbort(error)) => (
                        TerminalAttemptSnapshot::AutocommitFailure {
                            source: error.clone(),
                        },
                        ExplicitHardStopKind::Root,
                        Some(error.clone()),
                    ),
                    Some(TerminalAttempt::Cancellation {
                        cause, phase_proof, ..
                    }) => (
                        TerminalAttemptSnapshot::AutocommitCancel {
                            cause: cause.clone(), phase: *phase_proof,
                        },
                        ExplicitHardStopKind::Cancel, None,
                    ),
                    None if word == OPEN_WITH_CANCEL_INTENT => {
                        let cause = control.cancel_cause.get().cloned()
                            .ok_or(ActorError::CancellationProtocolMismatch)?;
                        (
                            TerminalAttemptSnapshot::AutocommitCancel {
                                cause, phase: CancelPhaseProof::NoTransactionPossible,
                            },
                            ExplicitHardStopKind::Cancel, None,
                        )
                    }
                    _ => return Err(ActorError::CancellationProtocolMismatch),
                };
            (cutoff.delivery_id, TerminalPublicationClass::Autocommit,
             kind, attempt, source)
        }
    };
    Ok(TerminalFenceSnapshot {
        id: control.id,
        actor_generation: control.actor_generation,
        full_terminal_word: word,
        delivery_id,
        class,
        public_kind,
        attempt,
        source,
    })
}

// Generation-death recovery uses this only after it set generation_fenced and
// removed the actor from routing under generation_owner_gate. The still-Open
// word therefore cannot acquire an actor owner while this snapshot is bound.
fn snapshot_bare_open_autocommit_after_unroute_locked(
    control: &ReservationControl,
) -> Result<TerminalFenceSnapshot, ActorError> {
    let word = control.terminal.load(Acquire);
    let ReservationCutoff::Autocommit(cutoff) = &control.terminal_cutoff
    else { return Err(ActorError::CancellationProtocolMismatch); };
    if !control.generation_fenced.load(Acquire)
        || word != OWNER_OPEN
        || control.terminal_attempt.lock().is_some()
    {
        return Err(ActorError::CancellationProtocolMismatch);
    }
    Ok(TerminalFenceSnapshot {
        id: control.id,
        actor_generation: control.actor_generation,
        full_terminal_word: word,
        delivery_id: cutoff.delivery_id,
        class: TerminalPublicationClass::Autocommit,
        public_kind: ExplicitHardStopKind::Root,
        attempt: TerminalAttemptSnapshot::AutocommitActorUnavailable,
        source: Some(actor_unavailable_before_terminal_claim()),
    })
}

impl HardStopTrigger {
    fn job(&self) -> DurableFenceJobHandle {
        match self {
            HardStopTrigger::ExplicitRoot { job, .. }
            | HardStopTrigger::ExplicitCancel { job, .. }
            | HardStopTrigger::ExplicitDataAbort { job, .. }
            | HardStopTrigger::Autocommit { job, .. } => *job,
        }
    }
}

fn observe_trigger_cutoff(
    supervisor: &FenceJobRegistry,
    trigger: &HardStopTrigger,
) -> Option<HardStopPublish> {
    let job = trigger.job();
    match trigger {
        HardStopTrigger::ExplicitRoot { cutoff, .. } =>
            cutoff.observe_late_hard_stop(supervisor, job),
        HardStopTrigger::ExplicitCancel { cutoff, .. } =>
            cutoff.observe_late_hard_stop(supervisor, job),
        HardStopTrigger::ExplicitDataAbort { cutoff, .. } =>
            cutoff.observe_late_hard_stop(supervisor, job),
        HardStopTrigger::Autocommit { cutoff, .. } =>
            cutoff.observe_late_hard_stop(supervisor, job),
    }
}

fn hard_stop_generation_is_current(
    control: &ReservationControl,
    trigger: &HardStopTrigger,
    generation: u64,
) -> bool {
    match trigger {
        HardStopTrigger::ExplicitRoot { deadline_slots, .. } =>
            matches!(*deadline_slots.state.lock(),
                ExplicitDeadlineState::Fired {
                    kind: DeadlineKind::TerminalHardStop,
                    generation: current,
                } if current == generation),
        HardStopTrigger::ExplicitCancel { deadline_slots, .. } =>
            matches!(*deadline_slots.state.lock(),
                ExplicitDeadlineState::Fired {
                    kind: DeadlineKind::CancellationHardStop,
                    generation: current,
                } if current == generation),
        HardStopTrigger::ExplicitDataAbort { .. }
        | HardStopTrigger::Autocommit { .. } =>
            control.terminal_hard_stop_armed.load(Acquire) == generation,
    }
}

fn conservative_protocol_fault_snapshot(
    control: &ReservationControl,
    trigger: &HardStopTrigger,
    word: u8,
    source: DbError,
) -> TerminalFenceSnapshot {
    let (delivery_id, class, public_kind, attempt) = match trigger {
        HardStopTrigger::ExplicitRoot { cutoff, decision, .. } => (
            cutoff.delivery_id,
            TerminalPublicationClass::Root,
            ExplicitHardStopKind::Root,
            match decision {
                RootDecision::Commit => TerminalAttemptSnapshot::ExplicitCommit,
                RootDecision::Rollback => TerminalAttemptSnapshot::ExplicitRollback,
            },
        ),
        HardStopTrigger::ExplicitCancel { cutoff, cause, .. } => (
            cutoff.delivery_id,
            TerminalPublicationClass::Cancel,
            ExplicitHardStopKind::Cancel,
            TerminalAttemptSnapshot::ExplicitCancel {
                cause: cause.clone(),
                phase: CancelPhaseProof::TransactionMayExist,
            },
        ),
        HardStopTrigger::ExplicitDataAbort { cutoff, .. } => (
            cutoff.delivery_id,
            TerminalPublicationClass::DataAbort,
            ExplicitHardStopKind::Root,
            TerminalAttemptSnapshot::ExplicitDataAbort {
                source: source.clone(),
            },
        ),
        HardStopTrigger::Autocommit { cutoff, .. } => (
            cutoff.delivery_id,
            TerminalPublicationClass::Autocommit,
            ExplicitHardStopKind::Root,
            TerminalAttemptSnapshot::AutocommitActorUnavailable,
        ),
    };
    TerminalFenceSnapshot {
        id: control.id,
        actor_generation: control.actor_generation,
        full_terminal_word: word,
        delivery_id,
        class,
        public_kind,
        attempt,
        source: Some(source),
    }
}

fn claim_protocol_fault_cutoff_locked(
    supervisor: &ActorSupervisorIndex,
    control: &ReservationControl,
    trigger: &HardStopTrigger,
    source: DbError,
) -> HardStopPublish {
    let job = trigger.job();
    let snapshot = conservative_protocol_fault_snapshot(
        control, trigger, control.terminal.load(Acquire), source,
    );
    let winner = match trigger {
        HardStopTrigger::ExplicitRoot { cutoff, .. } =>
            erase_cutoff_decision(cutoff.fence_or_ensure_result_delivery(
                &supervisor.jobs, job, snapshot,
            )),
        HardStopTrigger::ExplicitCancel { cutoff, .. } =>
            erase_cutoff_decision(cutoff.fence_or_ensure_result_delivery(
                &supervisor.jobs, job, snapshot,
            )),
        HardStopTrigger::ExplicitDataAbort { cutoff, .. } =>
            erase_cutoff_decision(cutoff.fence_or_ensure_result_delivery(
                &supervisor.jobs, job, snapshot,
            )),
        HardStopTrigger::Autocommit { cutoff, .. } =>
            erase_cutoff_decision(cutoff.fence_or_ensure_result_delivery(
                &supervisor.jobs, job, snapshot,
            )),
    };
    if winner == HardStopCutoffWinner::Result {
        HardStopPublish::ResultWon
    } else {
        control.generation_fenced.store(true, Release);
        HardStopPublish::Fencing
    }
}

// Trigger construction has already pinned the endpoint, built PreparedFenceRoute,
// and registered its dormant supervisor job. Consequently every current-timer
// branch below is infallible after an explicit permit claim and is recoverable
// after publisher death.
fn publish_hard_stop(
    supervisor: &ActorSupervisorIndex,
    id: ReservationId,
    actor_generation: u64,
    hard_stop_generation: u64,
    trigger: HardStopTrigger,
) -> HardStopPublish {
    if trigger.app_authority() != id.app {
        return HardStopPublish::Stale;
    }
    let Some(control) = supervisor.control_exact(id, actor_generation) else {
        return HardStopPublish::Stale;
    };
    if control.actor_generation != actor_generation {
        return HardStopPublish::Stale;
    }
    let job = trigger.job();

    // A real result or already-selected fence wins before timer-generation
    // validation. Ordinary result publication disarms the generation, so a
    // late callback is ResultWon/Stale, never a protocol fault.
    if let Some(done) = observe_trigger_cutoff(&supervisor.jobs, &trigger) {
        return done;
    }
    if !hard_stop_generation_is_current(
        &control, &trigger, hard_stop_generation,
    ) {
        return HardStopPublish::Stale;
    }

    // A current deadline with a malformed trigger is an internal protocol fault,
    // not a stale callback. Claim its typed cutoff before activating the job.
    if !validate_trigger_identity_and_deadline(
        &control, &trigger, hard_stop_generation,
    ) {
        let _owner = control.terminal_owner_gate.lock();
        return claim_protocol_fault_cutoff_locked(
            supervisor, &control, &trigger, hard_stop_trigger_mismatch(),
        );
    }

    let _owner = control.terminal_owner_gate.lock();
    let word = control.terminal.load(Acquire);
    if !hard_stop_owner_word_is_legal(&trigger, word) {
        return claim_protocol_fault_cutoff_locked(
            supervisor, &control, &trigger, hard_stop_owner_mismatch(),
        );
    }

    if matches!(trigger,
        HardStopTrigger::ExplicitDataAbort { .. }
        | HardStopTrigger::Autocommit { .. })
        && control.terminal_hard_stop_armed.compare_exchange(
            hard_stop_generation, 0, AcqRel, Acquire,
        ).is_err()
    {
        return observe_trigger_cutoff(&supervisor.jobs, &trigger)
            .unwrap_or(HardStopPublish::Stale);
    }

    if let HardStopTrigger::ExplicitRoot { permit, key, .. } = &trigger {
        if word == OPEN_WITH_CANCEL_INTENT || word == CANCEL_WITH_INTENT {
            let cause = control.cancel_cause.get().cloned()
                .expect("intent and immutable cause are one publication");
            if !permit.claim_and_enqueue_preemption_infallible(
                RegistryEvent::Routed {
                    key: key.clone(),
                    event: TxEvent::TerminalHardStopPreempted {
                        permit: permit.clone(),
                        resource_generation: actor_generation,
                        cause: forced_reason(cause),
                    },
                },
            ) {
                return claim_protocol_fault_cutoff_locked(
                    supervisor, &control, &trigger,
                    hard_stop_permit_mismatch(),
                );
            }
            supervisor.jobs.cancel_dormant_infallible(job);
            return HardStopPublish::CancelPreempted;
        }
    }

    // This is the semantic snapshot: full owner byte, exact attempt, source, and
    // delivery are read while the owner gate still excludes actor CAS/death.
    let snapshot = match snapshot_terminal_attempt_locked(&control, &trigger, word) {
        Ok(snapshot) => snapshot,
        Err(_) => {
            return claim_protocol_fault_cutoff_locked(
                supervisor, &control, &trigger,
                hard_stop_attempt_mismatch(),
            );
        }
    };
    let decision = match &trigger {
        HardStopTrigger::ExplicitRoot { cutoff, .. } =>
            erase_cutoff_decision(cutoff.fence_or_ensure_result_delivery(
                &supervisor.jobs, job, snapshot,
            )),
        HardStopTrigger::ExplicitCancel { cutoff, .. } =>
            erase_cutoff_decision(cutoff.fence_or_ensure_result_delivery(
                &supervisor.jobs, job, snapshot,
            )),
        HardStopTrigger::ExplicitDataAbort { cutoff, .. } =>
            erase_cutoff_decision(cutoff.fence_or_ensure_result_delivery(
                &supervisor.jobs, job, snapshot,
            )),
        HardStopTrigger::Autocommit { cutoff, .. } =>
            erase_cutoff_decision(cutoff.fence_or_ensure_result_delivery(
                &supervisor.jobs, job, snapshot,
            )),
    };
    if decision == HardStopCutoffWinner::Result {
        return HardStopPublish::ResultWon;
    }
    control.generation_fenced.store(true, Release);
    // The already-active supervisor job owns physical fencing and final
    // publication; recovery no longer depends on this publisher task.
    HardStopPublish::Fencing
}
~~~

The four trigger variants are deliberately separate at the type boundary. A
root hard stop emits exactly one keyed result to SC-1 HardStopping:
TerminalCompleted when the retained operation result wins,
TerminalHardStopPreempted when cancellation intent wins, or
TerminalHardStopCompleted when the physical fence wins. Explicit cancellation
similarly emits CancellationCompleted when its retained result wins or
CancellationHardStopCompleted when its physical fence wins, both only to
Cancelling(HardStopping). Explicit snapshot abort emits keyed DataAbortCompleted only
after the physical fence; autocommit stores and wakes an ActorTerminalOutcome.
All four first arbitrate their exact shared cutoff and all four fence the whole
actor generation. `prepare_autocommit_fence_route` exhausts the bound snapshot:
AutocommitSuccess becomes AutocommitIndeterminate; error cleanup or Cancel
becomes CleanupIndeterminate retaining the original/cause; bare Open becomes
ActorUnavailable; every malformed current trigger first binds a conservative
typed protocol-fault snapshot. An
already-absent exact generation is fence-complete, but every control is still
terminalized through the supervisor's retained control index. Fence completion
asserts only unreachability, never what an already-running COMMIT did.

InterruptHandle alone is not a start fence because SQLite says an interrupt
with zero running statements has no effect on the next one
(/home/ruiyang/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/libsqlite3-sys-0.37.0/sqlite3/sqlite3.h:2908-2917).
The actor therefore installs a per-command progress handler before marking the
phase Running. `run_statement_exact` receives both `statement_sequence` and
`logical_cancellation_sequence`: they are equal for an ordinary command, but
BEGIN's SnapshotMarker uses a fresh statement sequence while retaining the
BEGIN command sequence as its logical cancellation sequence. CleanupRollback
uses its newly minted terminal cleanup sequence for both:

~~~rust
const CANCEL_PROGRESS_OPS: i32 = 1024;

type RusqliteError = rusqlite::Error;

// rusqlite::Error is an enum. Numeric extended codes exist only on its
// SqliteFailure payload; non-SQLite variants return None and never enter a
// numeric SQLite classifier.
fn sqlite_extended_code(error: &RusqliteError) -> Option<i32> {
    error.sqlite_error().map(|ffi_error| ffi_error.extended_code)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StartDecision {
    Fenced,
    CancellationOwned,
    CompletionOwned,
    Cleaning,
}

#[derive(Clone)]
struct CompletedSqlTargetProof {
    id: ReservationId,
    lane: ConnectionLane,
    connection_generation: u64,
    command_sequence: u64,
    logical_cancellation_sequence: u64,
    statement: SqlStatementClass,
    cancel_phase: CancelPhaseProof,
    observed_terminal_word: u8,
    observed_interrupt_generation: u64,
    observed_interrupt_sequence: u64,
}

enum ClassifiedRawResult<T> {
    Ok(T),
    // The only error variant which may enter the ordinary SQLite mapper.
    SqlError(RusqliteError),
    CancellationInterrupt {
        raw: RusqliteError,
        cause: CancelCause,
        phase: CancelPhaseProof,
    },
    TerminalWatchdogInterrupt {
        raw: RusqliteError,
        owner: u8,
        statement: SqlStatementClass,
    },
    // The helper has already physically fenced this target and published its
    // type-appropriate keyed terminal failure. The caller must return only the
    // retained outcome; it may not run generic mapping or send a second reply.
    UnexpectedInterruptFenced {
        raw: RusqliteError,
        source: DbError,
    },
}

enum StatementRun<T> {
    Ran {
        result: ClassifiedRawResult<T>,
        completed: CompletedSqlTargetProof,
    },
    Suppressed(StartDecision),
}

// Installed before any fallible start-barrier work. remove_checked consumes the
// live flag; Drop is the backstop for every `?`/early return and fences the lane
// if SQLite refuses callback removal. No next reservation can inherit it.
struct CheckedProgressGuard<'a> {
    actor: &'a AppActor,
    control: Arc<ReservationControl>,
    conn: &'a Connection,
    live: bool,
}

impl CheckedProgressGuard<'_> {
    fn remove_checked(&mut self) -> Result<(), ActorError> {
        if self.live {
            clear_progress_handler(self.conn).map_err(|_|
                self.actor.protocol_fault_and_fence_with_source(
                    &self.control, progress_handler_remove_failed(),
                ))?;
            self.live = false;
        }
        Ok(())
    }
}

impl Drop for CheckedProgressGuard<'_> {
    fn drop(&mut self) {
        if self.live && clear_progress_handler(self.conn).is_err() {
            self.actor.supervisor.quarantine_generation_infallible(
                self.control.actor_generation,
                progress_handler_remove_failed(),
            );
        }
    }
}

fn cancel_phase_for(statement: SqlStatementClass) -> CancelPhaseProof {
    match statement {
        SqlStatementClass::PrepareAuthority =>
            CancelPhaseProof::NoTransactionPossible,
        SqlStatementClass::Begin => CancelPhaseProof::BeginMayHaveOpened,
        SqlStatementClass::SnapshotMarker
        | SqlStatementClass::OperationAuthority
        | SqlStatementClass::Data
        | SqlStatementClass::FrameControl
        | SqlStatementClass::Commit
        | SqlStatementClass::Rollback
        | SqlStatementClass::FailureCleanupRollback
        | SqlStatementClass::CleanupRollback
        | SqlStatementClass::PostRollbackAuthority =>
            CancelPhaseProof::TransactionMayExist,
    }
}

// This is the sole SQLite statement runner for PrepareAuthority, Begin,
// SnapshotMarker, OperationAuthority, Data, FrameControl, Commit, Rollback,
// FailureCleanupRollback, CleanupRollback, and PostRollbackAuthority. Its return type makes it
// impossible to reach generic error mapping without first classifying raw 9.
// It also owns the one and only target/progress finalization.
fn run_statement_exact<T>(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    conn: &Connection,
    lane: ConnectionLane,
    statement_sequence: u64,
    logical_cancellation_sequence: u64,
    statement: SqlStatementClass,
    run_one_statement: impl FnOnce() -> Result<T, RusqliteError>,
) -> Result<StatementRun<T>, ActorError> {
    let id = control.id;
    let generation = actor.generation_for(lane);
    let progress_control = Arc::downgrade(control);
    conn.progress_handler(CANCEL_PROGRESS_OPS, Some(move || {
        let Some(control) = progress_control.upgrade() else { return true; };
        let word = control.terminal.load(Acquire);
        let g1 = control.terminal_interrupt_generation.load(Acquire);
        let sequence = control.terminal_interrupt_sequence.load(Acquire);
        let g2 = control.terminal_interrupt_generation.load(Acquire);
        control.generation_fenced.load(Acquire)
            || (owner(word) == OWNER_OPEN && has_cancel_intent(word))
            || (g1 != 0 && g1 == g2
                && is_terminal_owner(owner(word))
                && sequence == statement_sequence)
    })).map_err(|_| actor.protocol_fault_and_fence_with_source(
        control, progress_handler_install_failed()))?;
    let mut progress = CheckedProgressGuard {
        actor,
        control: control.clone(),
        conn,
        live: true,
    };

    let start = {
        let _owner = control.terminal_owner_gate.lock();
        let mut active = actor.active_sql.lock();
        let word = control.terminal.load(Acquire);
        let g1 = control.terminal_interrupt_generation.load(Acquire);
        let interrupt_sequence =
            control.terminal_interrupt_sequence.load(Acquire);
        let g2 = control.terminal_interrupt_generation.load(Acquire);
        if g1 != g2 {
            return Err(actor.protocol_fault_and_fence_with_source(
                control, torn_interrupt_latch()));
        }
        let decision = if control.generation_fenced.load(Acquire) {
            Some(StartDecision::Fenced)
        } else if owner(word) == OWNER_OPEN && has_cancel_intent(word) {
            Some(StartDecision::CancellationOwned)
        } else if g1 != 0 && is_terminal_owner(owner(word))
            && interrupt_sequence == statement_sequence
        {
            Some(if owner(word) == OWNER_COMPLETE {
                StartDecision::CompletionOwned
            } else {
                StartDecision::Cleaning
            })
        } else {
            None
        };
        if decision.is_none() {
            if active.is_some() {
                return Err(actor.protocol_fault_and_fence_with_source(
                    control, overlapping_sql_target()));
            }
            *active = Some(ActiveSqlTarget {
                id,
                lane,
                connection_generation: generation,
                command_sequence: statement_sequence,
                cancellation_sequence: logical_cancellation_sequence,
                statement,
                interrupt: conn.get_interrupt_handle(),
                interrupt_sent: false,
            });
            control.phase.store(ActorPhase::Running {
                reservation_id: id,
                command_sequence: statement_sequence,
                lane,
                connection_generation: generation,
            }, Release);
        }
        decision.map(|decision| (decision, word))
    };

    if let Some((decision, start_word)) = start {
        // No ActiveSqlTarget was installed. This is the only suppressed arm.
        progress.remove_checked()?;
        match decision {
            StartDecision::Fenced =>
                actor.drop_connections_and_exit_generation(control.actor_generation),
            StartDecision::CancellationOwned => {
                // No FFI call occurred. A suppressed BEGIN therefore proves
                // absence, not BeginMayHaveOpened. SnapshotMarker and later
                // classes retain TransactionMayExist because their BEGIN is an
                // earlier completed statement.
                let phase = match statement {
                    SqlStatementClass::PrepareAuthority
                    | SqlStatementClass::Begin =>
                        CancelPhaseProof::NoTransactionPossible,
                    _ => cancel_phase_for(statement),
                };
                {
                    let _owner = control.terminal_owner_gate.lock();
                    let active = actor.active_sql.lock();
                    assert!(active.is_none());
                    if phase == CancelPhaseProof::NoTransactionPossible {
                        let is_busy_after = conn.is_busy();
                        let is_autocommit_after = conn.is_autocommit();
                        if is_busy_after || !is_autocommit_after {
                            return Err(actor.protocol_fault_and_fence_with_source(
                                control, no_sql_start_engine_state_mismatch(),
                            ));
                        }
                        let seal = NoSqlStartSeal::mint_under_locked_barrier(
                            id, logical_cancellation_sequence, lane, generation,
                            start_word, is_busy_after, is_autocommit_after,
                        );
                        *control.finalized_cancel_proof.lock() =
                            Some(FinalizedCancelProof {
                                cancellation_sequence:
                                    logical_cancellation_sequence,
                                phase,
                                evidence:
                                    FinalizedCancelEvidence::NoStatementStarted(
                                        NoSqlStartProof {
                                            id,
                                            actor_generation:
                                                control.actor_generation,
                                            cancellation_sequence:
                                                logical_cancellation_sequence,
                                            lane,
                                            connection_generation: generation,
                                            phase,
                                            observed_owner_word: start_word,
                                            is_busy_after,
                                            is_autocommit_after,
                                            seal,
                                        },
                                    ),
                            });
                    } else {
                        // BEGIN already exists. Suppressing a later statement
                        // proves only that statement did not start, not that the
                        // transaction ended; cleanup must inspect/ROLLBACK.
                        *control.finalized_cancel_proof.lock() = None;
                    }
                    if statement == SqlStatementClass::Begin {
                        control.phase.store(ActorPhase::BeginNotOpened, Release);
                    } else {
                        control.phase.store(
                            ActorPhase::BetweenStatementAndCommit, Release,
                        );
                    }
                }
                let cause = control.cancel_cause.get().cloned()
                    .expect("intent publication precedes suppression");
                accept_cancel_command(
                    actor,
                    control,
                    Some(logical_cancellation_sequence),
                    cause,
                    control.retention.cancel_reply.clone(),
                );
            }
            StartDecision::CompletionOwned => {
                // The terminal watchdog fired before COMMIT/ROLLBACK began.
                // Preserve OWNER_COMPLETE and the original absolute budget;
                // the closed pre-start terminal classifier may try bounded
                // rollback but never reports the unexecuted terminal SQL as OK.
                actor.finish_prestart_terminal_timeout(
                    control, statement, statement_sequence,
                );
            }
            StartDecision::Cleaning => {
                // CancellationSql already fired before cleanup ROLLBACK began.
                // Do not publish or wake while a transaction may remain open.
                // The prearmed hard stop physically retires this generation and
                // publishes the sole fenced cancellation outcome.
                actor.leave_cancel_cutoff_pending_for_hard_stop(control);
            }
        }
        return Ok(StatementRun::Suppressed(decision));
    }

    let raw_result = run_one_statement(); // no actor mutex is held across FFI
    let handler_clear = progress.remove_checked();

    // Classification occurs while the exact target still exists. The one
    // critical section snapshots the owner/interrupt latch, authenticates raw
    // SQLITE_INTERRUPT, records the cancellation phase proof, and removes the
    // target exactly once. No outer helper is allowed to finalize it again.
    let (completed, route) = {
        let _owner = control.terminal_owner_gate.lock();
        let mut active = actor.active_sql.lock();
        let Some(target) = active.as_ref() else {
            return Err(actor.protocol_fault_and_fence_with_source(
                control, missing_sql_target()));
        };
        if target.id != id || target.lane != lane
            || target.connection_generation != generation
            || target.command_sequence != statement_sequence
            || target.cancellation_sequence != logical_cancellation_sequence
            || target.statement != statement
        {
            return Err(actor.protocol_fault_and_fence_with_source(
                control, foreign_sql_target()));
        }
        let word = control.terminal.load(Acquire);
        let g1 = control.terminal_interrupt_generation.load(Acquire);
        let interrupt_sequence =
            control.terminal_interrupt_sequence.load(Acquire);
        let g2 = control.terminal_interrupt_generation.load(Acquire);
        if g1 != g2 {
            return Err(actor.protocol_fault_and_fence_with_source(
                control, torn_interrupt_latch()));
        }
        let phase = cancel_phase_for(statement);
        let route = raw_result.as_ref().err().and_then(|raw|
            classify_interrupt_from_locked_target(
                control, target, raw, word, g1, interrupt_sequence,
            )
        );
        let completed = CompletedSqlTargetProof {
            id,
            lane,
            connection_generation: generation,
            command_sequence: statement_sequence,
            logical_cancellation_sequence,
            statement,
            cancel_phase: phase,
            observed_terminal_word: word,
            observed_interrupt_generation: g1,
            observed_interrupt_sequence: interrupt_sequence,
        };
        if statement != SqlStatementClass::PrepareAuthority {
            *control.finalized_cancel_proof.lock() = Some(FinalizedCancelProof {
                cancellation_sequence: logical_cancellation_sequence,
                phase,
                evidence: FinalizedCancelEvidence::Statement(completed.clone()),
            });
        }
        *active = None;
        control.phase.store(BetweenStatementAndCommit, Release);
        (completed, route)
    };

    if handler_clear.is_err() {
        return Err(actor.protocol_fault_and_fence_with_source(
            control, progress_handler_remove_failed()));
    }
    assert!(!conn.is_busy());
    if statement == SqlStatementClass::PrepareAuthority {
        let is_busy_after = conn.is_busy();
        let is_autocommit_after = conn.is_autocommit();
        if is_busy_after || !is_autocommit_after {
            return Err(actor.protocol_fault_and_fence_with_source(
                control, prepare_authority_end_state_mismatch(),
            ));
        }
        let source = PostFinalizeSource::Statement(completed.clone());
        let proof = PostFinalizeAutocommitProof {
            seal: PostFinalizeEndSeal::mint_at_connection_sample(
                &source, is_busy_after, is_autocommit_after,
            ),
            source,
            is_busy_after,
            is_autocommit_after,
        };
        let _owner = control.terminal_owner_gate.lock();
        if actor.active_sql.lock().is_some() {
            return Err(actor.protocol_fault_and_fence_with_source(
                control, prepare_authority_end_state_mismatch(),
            ));
        }
        *control.finalized_cancel_proof.lock() = Some(FinalizedCancelProof {
            cancellation_sequence: logical_cancellation_sequence,
            phase: CancelPhaseProof::NoTransactionPossible,
            evidence: FinalizedCancelEvidence::CompletedOutsideTransaction(proof),
        });
    }

    let result = match (raw_result, route) {
        (Ok(value), None) => ClassifiedRawResult::Ok(value),
        (Err(raw), None) => ClassifiedRawResult::SqlError(raw),
        (Err(raw), Some(InterruptRoute::Cancellation { cause, phase })) =>
            ClassifiedRawResult::CancellationInterrupt { raw, cause, phase },
        (Err(raw), Some(InterruptRoute::TerminalWatchdog { owner, statement })) =>
            ClassifiedRawResult::TerminalWatchdogInterrupt {
                raw, owner, statement,
            },
        (Err(raw), Some(InterruptRoute::Unexpected)) => {
            let source = unexpected_sqlite_interrupt();
            // The supervisor fences every lane/routing entry, but this exact
            // control is not skipped. It receives a source-preserving prepared
            // cutoff fallback before this function returns. Other controls in
            // the generation receive generic actor_unavailable.
            actor.supervisor.fence_generation_with_target_failure(
                control.actor_generation,
                control.id,
                TargetFenceFailure::UnexpectedInterrupt {
                    source: source.clone(),
                    completed: completed.clone(),
                },
            );
            ClassifiedRawResult::UnexpectedInterruptFenced { raw, source }
        }
        (Ok(_), Some(_)) => unreachable!("only raw SQLITE_INTERRUPT has a route"),
    };
    Ok(StatementRun::Ran { result, completed })
}
~~~

The direct interrupt is the low-latency path for a statement already stepping.
The progress latch is the deterministic backstop when interrupt landed in the
marked-running/before-step gap. Rusqlite documents that the callback runs every
approximately num_ops VM instructions and a true result interrupts the
operation
(/home/ruiyang/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/rusqlite-0.39.0/src/hooks/mod.rs:425-439).
The workspace already enables preupdate_hook (Cargo.toml:90-95), and rusqlite
0.39 defines that feature to include hooks
(/home/ruiyang/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/rusqlite-0.39.0/Cargo.toml:149-152),
so no feature change is needed. Registration’s Result remains checked as shown.

Short work that finishes before a progress callback still cannot become durable
after an earlier cancel: the actor arbitrates the shared terminal word
immediately before COMMIT. Handler removal is checked before the active target
is cleared; failure quarantines that lane, so the callback never captures the
next reservation.

The terminal interrupt intent is command-sequence scoped. When an interrupted
COMMIT/ROLLBACK returns, statement finalization preserves a matching fired
generation/sequence latch while its closed classifier runs. If that classifier
starts cleanup or authority work, it atomically retargets the same generation
from the completed sequence to the fresh sequence under terminal_owner_gate plus
active_sql, then invokes
`run_statement_exact(PostRollbackAuthority, ...)`; it never hand-clears or
hand-installs an ActiveSqlTarget. The callback's
generation/sequence/generation read rejects a torn retarget.
The actor does **not** disarm or extend the SC-1
terminal/cancellation absolute deadline or its already-armed hard-stop grace. If
the COMMIT classifier requires a cleanup ROLLBACK, the actor mints a new command
sequence, stores it in current_terminal_sequence, installs the ROLLBACK target,
and executes it under the remaining original budget. A timer publisher resolves
the current sequence from the control record rather than freezing the first
COMMIT sequence. Only retention of the selected cutoff Result clears generation
and sequence, in `disarm_terminal_budget_after_retention_exact`; the original
absolute bound therefore still fences a cleanup that hangs.

## 15. Actor terminal algorithm

Autocommit reservation:

~~~text
Reserve on op_conn
outside a data transaction, run the trusted platform-authority prepare read
classify AppAuthority/state/epoch and capture the ceiling
if ReResolve or Deny: return without data BEGIN
BEGIN DEFERRED on op_conn
as the first in-transaction statement, read the app-file snapshot marker
compare marker state/epoch/incarnation with the prepared observation
if mismatch: ROLLBACK and return ReResolve or terminal Deny
execute one DbOperation
finalize/drop the statement and record its exact Result as terminal_attempt
arbitrate terminal owner
if cancellation owns: ROLLBACK/verify auto-rollback, retire, acknowledge
if completion owns and operation succeeded: COMMIT, classify using COMMIT result
    plus is_autocommit, store Autocommit(Ok(value)) only on confirmed commit
if completion owns and operation failed: NEVER COMMIT; inspect is_autocommit,
    ROLLBACK if still live, store Autocommit(Err(original)) only after cleanup,
    otherwise store CleanupIndeterminate and quarantine
store the immutable ActorTerminalOutcome before sending the Execute reply
~~~

Explicit transaction reservation:

~~~text
Reserve control record (no SQL), then Prepare outside the data snapshot: read
trusted authority and capture the initial ceiling on platform-role op_conn
SC-1 consumes PreparationCompleted and sends exactly one Begin command
Begin on tx_conn; tenant-role/data work starts only after that authority result
first statement in that same Begin command: read/pin the app-file snapshot marker; compare it
with the prepared state/epoch/incarnation before creator data SQL
for each Execute:
    read fresh trusted authority on the platform-role op_conn
    terminally deny domain/incarnation/deprovision; re-resolve epoch/changing
    intersect ceiling with the BEGIN value
    execute one statement on tx_conn
Settle(COMMIT or ROLLBACK), or Cancel
terminal CAS, terminal SQL/cleanup, retire, reply
~~~

The app-file snapshot marker is a coherence assertion, not the authority source:
it contains lifecycle state, epoch, and incarnation but no decision may broaden
the trusted prepared ceiling. For autocommit, trusted authority preparation
precedes BEGIN. For an explicit transaction, trusted authority never uses
tx_conn after that connection assumes tenant/data work: op_conn performs the
initial and per-operation reads. A later ceiling is intersected with, never
substituted for, the value captured immediately before BEGIN. This preserves
both SC-2’s first-statement coherent snapshot rule
(docs/proposals/2026-08-26-sc2-sqlite-actor-protocol.md:164-175;
docs/proposals/2026-08-26-sc2-sqlite-actor-protocol.md:230-242) and Fork B
(docs/proposals/2026-08-26-sc6-ceiling-read-contract.md:113-125).

Interrupting an explicit transaction’s op_conn authority read cancels the whole
reservation, not merely that auxiliary read: after op_conn returns, the actor
uses tx_conn to roll back (or proves SQLite already ended it) before
acknowledging Cancel. ActiveSqlTarget selects where to interrupt;
ReservationId selects everything that must be cleaned.

The actor applies these terminal rules:

| Actor observation | Owner CAS and outcome |
| --- | --- |
| CANCEL_INTENT before Reserve/BEGIN/data SQL | CAS Open+Intent to Cancel; remove queued work; no SQL if no transaction exists; otherwise rollback; then Cancelled. |
| CANCEL_INTENT after BEGIN but before a statement | CAS to Cancel; rollback; retire; then acknowledge. |
| Running statement returns matching SQLITE_INTERRUPT | CAS to Cancel; finalize statement; verify/perform rollback; retire; then acknowledge. |
| Statement returns success and cancel won before pre-COMMIT CAS | Discard result; CAS to Cancel; rollback; acknowledge. |
| Actor CASes Open to Complete before COMMIT | Set phase CompletionOwned, issue COMMIT, and let its result determine Committed, RolledBack, failed, or indeterminate. Later Cancel cannot interrupt. |
| Successful autocommit operation is accepted while owner is Open and no intent exists | Store AutocommitSuccess(value), CAS Open to Complete, COMMIT, and store Autocommit(Ok(value)) only after confirmed success; a later Cancel replays that exact value. |
| Failed autocommit operation is accepted while owner is Open and no intent exists | Store AutocommitFailure(error), CAS Open to Complete, never COMMIT, and rollback/verify auto-rollback. Store Autocommit(Err(error)) only after cleanup; otherwise CleanupIndeterminate. |
| Terminal engine fault races an earlier CANCEL_INTENT | CAS Open+Intent to Cancel; cancellation is public winner, ordinary fault is retained as diagnostic metadata, and cleanup still must complete. |
| Nonterminal explicit Execute completes | Use its command completion gate, not OWNER_MASK. Restore Idle/Poisoned as SC-1 requires; a later Cancel still ends the reservation. |
| TerminalSql watchdog fires after Complete or Cancel ownership | Interrupt only the exact terminal ActiveSqlTarget; owner is unchanged. SQLITE_OK remains committed; terminal error is classified as failed/indeterminate from engine state. |
| Cancellation cleanup cannot prove autocommit | Store CancellationCleanupFailed, quarantine that lane generation, wake every waiter after it is unreachable, and reopen only as later actor recovery. |

Cleanup is engine-state based:

~~~rust
fn finalize_interrupted_target_exact(
    actor: &AppActor,
    control: &ReservationControl,
    phase_proof: CancelPhaseProof,
) -> Result<(), ActorError> {
    let _owner = control.terminal_owner_gate.lock();
    let active = actor.active_sql.lock();
    match active.as_ref() {
        // Actor commands are serialized. A running statement is finalized only
        // by run_statement_exact when its FFI call returns; cleanup must never
        // steal or double-clear that target.
        Some(_) => Err(ActorError::CancellationProtocolMismatch),
        None if phase_proof == CancelPhaseProof::NoTransactionPossible
            && matches!(control.phase.load(Acquire),
                ActorPhase::Queued
                | ActorPhase::Preparing
                | ActorPhase::BeginNotOpened) => Ok(()),
        None if phase_proof == CancelPhaseProof::TransactionMayExist
            && owner(control.terminal.load(Acquire)) == OWNER_CANCEL
            && matches!(control.phase.load(Acquire),
                ActorPhase::Idle | ActorPhase::BetweenStatementAndCommit) =>
            Ok(()),
        None if control.finalized_cancel_proof.lock().as_ref().is_some_and(|p| {
            p.cancellation_sequence
                == control.current_terminal_sequence.load(Acquire)
                && p.phase == phase_proof
        }) => Ok(()),
        None => Err(ActorError::CancellationProtocolMismatch),
    }
}

enum RollbackEngineResult {
    Succeeded,
    Failed(RusqliteError),
    WatchdogInterrupted(RusqliteError),
}

struct CleanupRollbackObservation {
    engine: RollbackEngineResult,
    end: PostFinalizeAutocommitProof,
}

enum CleanupRollbackRun {
    Observed(CleanupRollbackObservation),
    SuppressedByExpiredWatchdog,
    TargetAlreadyTerminalized,
}

fn execute_cancel_cleanup_rollback(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    lane: ConnectionLane,
) -> Result<CleanupRollbackRun, ActorError> {
    let fresh = mint_never_reused_command_sequence();
    {
        let _owner = control.terminal_owner_gate.lock();
        let active = actor.active_sql.lock();
        if owner(control.terminal.load(Acquire)) != OWNER_CANCEL
            || active.is_some()
            || control.generation_fenced.load(Acquire)
        {
            return Err(ActorError::CancellationProtocolMismatch);
        }
        // Retarget, never reset, a fired watchdog. Absolute time/generation do
        // not change, so cleanup cannot buy a fresh budget.
        let old = control.current_terminal_sequence.load(Acquire);
        let g1 = control.terminal_interrupt_generation.load(Acquire);
        let armed = control.terminal_interrupt_sequence.load(Acquire);
        let g2 = control.terminal_interrupt_generation.load(Acquire);
        if g1 != g2 || (g1 != 0 && armed != old) {
            return Err(ActorError::CancellationProtocolMismatch);
        }
        if g1 != 0 {
            control.terminal_interrupt_sequence.store(fresh, Release);
        }
        control.current_terminal_sequence.store(fresh, Release);
    }

    let conn = actor.connection(lane);
    let run = run_statement_exact(
        actor, control, conn, lane, fresh, fresh,
        SqlStatementClass::CleanupRollback,
        || conn.execute_batch("ROLLBACK"),
    )?;
    let (engine, completed) = match run {
        StatementRun::Suppressed(StartDecision::Cleaning) =>
            return Ok(CleanupRollbackRun::SuppressedByExpiredWatchdog),
        StatementRun::Suppressed(StartDecision::Fenced) =>
            return Ok(CleanupRollbackRun::TargetAlreadyTerminalized),
        StatementRun::Suppressed(
            StartDecision::CancellationOwned | StartDecision::CompletionOwned,
        ) => return Err(ActorError::CancellationProtocolMismatch),
        StatementRun::Ran {
            result: ClassifiedRawResult::Ok(()), completed,
        } => (RollbackEngineResult::Succeeded, completed),
        StatementRun::Ran {
            result: ClassifiedRawResult::SqlError(raw), completed,
        } => (RollbackEngineResult::Failed(raw), completed),
        StatementRun::Ran {
            result: ClassifiedRawResult::TerminalWatchdogInterrupt {
                raw,
                owner: OWNER_CANCEL,
                statement: SqlStatementClass::CleanupRollback,
            },
            completed,
        } => (RollbackEngineResult::WatchdogInterrupted(raw), completed),
        StatementRun::Ran {
            result: ClassifiedRawResult::UnexpectedInterruptFenced { .. }, ..
        } => return Ok(CleanupRollbackRun::TargetAlreadyTerminalized),
        StatementRun::Ran {
            result: ClassifiedRawResult::CancellationInterrupt { .. }
                | ClassifiedRawResult::TerminalWatchdogInterrupt { .. },
            ..
        } => return Err(ActorError::CancellationProtocolMismatch),
    };

    // Actor command execution is serialized; no next command can enter between
    // run_statement_exact's target finalization and this sample.
    let source = PostFinalizeSource::Statement(completed);
    let is_busy_after = conn.is_busy();
    let is_autocommit_after = conn.is_autocommit();
    let seal = PostFinalizeEndSeal::mint_at_connection_sample(
        &source, is_busy_after, is_autocommit_after,
    );
    Ok(CleanupRollbackRun::Observed(CleanupRollbackObservation {
        engine,
        end: PostFinalizeAutocommitProof {
            source, is_busy_after, is_autocommit_after, seal,
        },
    }))
}

fn take_finalized_cancel_evidence(
    control: &ReservationControl,
    expected_phase: CancelPhaseProof,
    expected_predecessor_sequence: Option<u64>,
) -> Result<Option<FinalizedCancelEvidence>, ActorError> {
    let mut slot = control.finalized_cancel_proof.lock();
    match (slot.take(), expected_predecessor_sequence) {
        (Some(proof), Some(expected))
            if proof.phase == expected_phase
                && proof.cancellation_sequence == expected =>
                    Ok(Some(proof.evidence)),
        (Some(_), Some(_)) => Err(ActorError::CancellationProtocolMismatch),
        // IDs are opaque equality tokens. Without a command-gate predecessor
        // edge, an old finalized proof is diagnostic only and is discarded.
        (Some(_), None) | (None, _) => Ok(None),
    }
}

fn mint_idle_post_finalize_proof(
    actor: &AppActor,
    control: &ReservationControl,
    lane: ConnectionLane,
    evidence: Option<FinalizedCancelEvidence>,
) -> Result<PostFinalizeAutocommitProof, ActorError> {
    let _owner = control.terminal_owner_gate.lock();
    let active = actor.active_sql.lock();
    if owner(control.terminal.load(Acquire)) != OWNER_CANCEL || active.is_some() {
        return Err(ActorError::CancellationProtocolMismatch);
    }
    let conn = actor.connection(lane);
    let is_busy_after = conn.is_busy();
    let is_autocommit_after = conn.is_autocommit();
    let source = match evidence {
        Some(FinalizedCancelEvidence::Statement(completed)) =>
            PostFinalizeSource::Statement(completed),
        Some(FinalizedCancelEvidence::CompletedOutsideTransaction(_)) =>
            return Err(ActorError::CancellationProtocolMismatch),
        Some(FinalizedCancelEvidence::NoStatementStarted(_)) | None => {
            let terminal_sequence =
                control.current_terminal_sequence.load(Acquire);
            let lease = actor.session_lease_exact(control.id)
                .ok_or(ActorError::CancellationProtocolMismatch)?;
            let phase = control.phase.load(Acquire);
            let connection_generation = actor.generation_for(lane);
            let seal = ActorIdleEndSeal::mint_under_locked_barrier(
                control.id, terminal_sequence, lease.lease_id, lane,
                connection_generation, phase,
                is_busy_after, is_autocommit_after,
            );
            PostFinalizeSource::ActorIdle {
                id: control.id,
                actor_generation: control.actor_generation,
                terminal_sequence,
                lease_id: lease.lease_id,
                lane,
                connection_generation,
                phase,
                is_busy_after,
                is_autocommit_after,
                seal,
            }
        }
    };
    let seal = PostFinalizeEndSeal::mint_at_connection_sample(
        &source, is_busy_after, is_autocommit_after,
    );
    Ok(PostFinalizeAutocommitProof {
        source, is_busy_after, is_autocommit_after, seal,
    })
}

fn seal_cancel_end_capability(
    actor: &AppActor,
    control: &ReservationControl,
    cause: CancelCause,
    fact: CancelEndFact,
) -> Result<CancelEndCapability, ActorError> {
    let _owner = control.terminal_owner_gate.lock();
    let active = actor.active_sql.lock();
    let word = control.terminal.load(Acquire);
    if owner(word) != OWNER_CANCEL || active.is_some() {
        return Err(ActorError::CancellationProtocolMismatch);
    }
    let delivery_id = selected_terminal_delivery_id(control)?;
    let seal = CancelEndSeal::mint_from_exact_fact(
        control.id, delivery_id, &cause, &fact, word,
    );
    Ok(CancelEndCapability {
        id: control.id,
        actor_generation: control.actor_generation,
        delivery_id,
        cause,
        fact,
        observed_owner_word: word,
        seal,
    })
}

fn mint_no_sql_start_proof_under_owner_gate(
    actor: &AppActor,
    control: &ReservationControl,
) -> Result<NoSqlStartProof, ActorError> {
    let _owner = control.terminal_owner_gate.lock();
    let active = actor.active_sql.lock();
    let word = control.terminal.load(Acquire);
    let sequence = control.current_terminal_sequence.load(Acquire);
    let lane = match control.kind {
        ReservationKind::Transaction => ConnectionLane::Tx,
        ReservationKind::Autocommit => ConnectionLane::Op,
    };
    let conn = actor.connection(lane);
    let connection_generation = actor.generation_for(lane);
    let is_busy_after = conn.is_busy();
    let is_autocommit_after = conn.is_autocommit();
    if owner(word) != OWNER_CANCEL
        || active.is_some()
        || is_busy_after
        || !is_autocommit_after
        || !matches!(control.phase.load(Acquire),
            ActorPhase::Queued | ActorPhase::Preparing
            | ActorPhase::BeginNotOpened)
    {
        return Err(ActorError::CancellationProtocolMismatch);
    }
    Ok(NoSqlStartProof {
        id: control.id,
        actor_generation: control.actor_generation,
        cancellation_sequence: sequence,
        lane,
        connection_generation,
        phase: CancelPhaseProof::NoTransactionPossible,
        observed_owner_word: word,
        is_busy_after,
        is_autocommit_after,
        seal: NoSqlStartSeal::mint_under_locked_barrier(
            control.id, sequence, lane, connection_generation, word,
            is_busy_after, is_autocommit_after,
        ),
    })
}

fn publish_clean_cancel_end(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    cap: CancelEndCapability,
) -> Result<(), ActorError> {
    match &control.terminal_cutoff {
        ReservationCutoff::Explicit { .. } =>
            publish_cancel_terminal_exact(
                actor, control, CancelTerminalConclusion::Clean(cap),
            ),
        ReservationCutoff::Autocommit(_) =>
            publish_autocommit_terminal_exact(
                actor, control,
                AutocommitTerminalConclusion::CancellationConfirmed { end: cap },
            ),
    }
}

fn publish_cancel_cleanup_failure(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    error: DbError,
) {
    actor.quarantine_generation_before_wake(control.actor_generation);
    let attempt = {
        let _owner = control.terminal_owner_gate.lock();
        let active = actor.active_sql.lock();
        if active.is_some() {
            None
        } else {
            let word = control.terminal.load(Acquire);
            let delivery_id = selected_terminal_delivery_id(control).ok();
            delivery_id.map(|delivery_id| UncertainEndCapability {
                id: control.id,
                actor_generation: control.actor_generation,
                delivery_id,
                class: TerminalPublicationClass::Cancel,
                sequence: Some(
                    control.current_terminal_sequence.load(Acquire),
                ),
                observed_owner_word: word,
                seal: UncertainEndSeal::mint_under_owner_gate(
                    control.id, delivery_id,
                    TerminalPublicationClass::Cancel, word,
                ),
            })
        }
    };
    let published = match (&control.terminal_cutoff, attempt) {
        (ReservationCutoff::Explicit { .. }, Some(end)) =>
            publish_cancel_terminal_exact(
                actor, control, CancelTerminalConclusion::Indeterminate {
                    end, error: error.clone(),
                },
            ),
        (ReservationCutoff::Autocommit(_), _) =>
            publish_autocommit_terminal_exact(
                actor, control,
                AutocommitTerminalConclusion::CleanupIndeterminate {
                    sequence: control.current_terminal_sequence.load(Acquire),
                    error: error.clone(),
                },
            ),
        _ => Err(ActorError::CancellationProtocolMismatch),
    };
    if published.is_err() {
        // Total last resort: supervisor owns and durably registers a physical
        // fence job before returning. Its exact cancel cutoff route publishes
        // the same source-preserving indeterminate outcome; no actor-local store
        // or wake bypass exists.
        actor.supervisor.register_and_drive_cancel_fence_infallible(
            control.clone(), error,
        );
    }
    actor.schedule_lane_reopen_after_terminal_record(control.actor_generation);
}

fn run_cancel_cleanup(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    phase_proof: CancelPhaseProof,
) {
    if let Err(error) = finalize_interrupted_target_exact(
        actor, control, phase_proof,
    ) {
        publish_cancel_cleanup_failure(
            actor, control, actor_error_as_db_error(error),
        );
        return;
    }

    let (cause, predecessor_sequence) =
        match control.terminal_attempt.lock().as_ref() {
        Some(TerminalAttempt::Cancellation {
            cause, phase_proof: stored, predecessor_sequence,
        }) if *stored == phase_proof =>
            (cause.clone(), *predecessor_sequence),
        _ => {
            publish_cancel_cleanup_failure(
                actor, control, cancellation_protocol_db_error(),
            );
            return;
        }
    };
    let initial = match take_finalized_cancel_evidence(
        control, phase_proof, predecessor_sequence,
    ) {
        Ok(value) => value,
        Err(error) => {
            publish_cancel_cleanup_failure(
                actor, control, actor_error_as_db_error(error),
            );
            return;
        }
    };
    let lane = match control.kind {
        ReservationKind::Transaction => ConnectionLane::Tx,
        ReservationKind::Autocommit => ConnectionLane::Op,
    };
    let conn = actor.connection(lane);
    drop_or_reset_all_statements(conn);

    let fact: Result<CancelEndFact, DbError> = match phase_proof {
        CancelPhaseProof::NoTransactionPossible => match initial {
            Some(FinalizedCancelEvidence::NoStatementStarted(proof)) =>
                Ok(CancelEndFact::NoStatementStarted(proof)),
            Some(FinalizedCancelEvidence::CompletedOutsideTransaction(proof)) =>
                Ok(CancelEndFact::CompletedOutsideTransaction(proof)),
            None => mint_no_sql_start_proof_under_owner_gate(
                actor, control,
            ).map(CancelEndFact::NoStatementStarted)
             .map_err(actor_error_as_db_error),
            Some(FinalizedCancelEvidence::Statement(_)) =>
                Err(cancellation_protocol_db_error()),
        },

        CancelPhaseProof::BeginMayHaveOpened if conn.is_autocommit() =>
            mint_idle_post_finalize_proof(actor, control, lane, initial)
                .map(CancelEndFact::BeginDidNotOpen)
                .map_err(actor_error_as_db_error),

        CancelPhaseProof::BeginMayHaveOpened
        | CancelPhaseProof::TransactionMayExist if !conn.is_autocommit() =>
            match execute_cancel_cleanup_rollback(actor, control, lane) {
                Ok(CleanupRollbackRun::Observed(observation))
                    if observation.end.is_autocommit_after
                        && !observation.end.is_busy_after => {
                    let fact = match observation.engine {
                        RollbackEngineResult::Succeeded =>
                            CancelEndFact::RolledBack(observation.end),
                        RollbackEngineResult::Failed(raw)
                        | RollbackEngineResult::WatchdogInterrupted(raw) => {
                            actor.record_cancel_cleanup_diagnostic(
                                control.id,
                                map_terminal_sql_error_exact(
                                    SqlStatementClass::CleanupRollback,
                                    &raw,
                                ),
                            );
                            CancelEndFact::SQLiteAlreadyRolledBack(
                                observation.end,
                            )
                        }
                    };
                    Ok(fact)
                }
                Ok(CleanupRollbackRun::SuppressedByExpiredWatchdog)
                | Ok(CleanupRollbackRun::TargetAlreadyTerminalized) => return,
                Ok(CleanupRollbackRun::Observed(observation)) => {
                    let engine_error = match observation.engine {
                        RollbackEngineResult::Succeeded =>
                            cancellation_cleanup_db_error(),
                        RollbackEngineResult::Failed(raw)
                        | RollbackEngineResult::WatchdogInterrupted(raw) =>
                            map_terminal_sql_error_exact(
                                SqlStatementClass::CleanupRollback, &raw,
                            ),
                    };
                    Err(cancellation_cleanup_with_engine_error(engine_error))
                }
                Err(error) => Err(actor_error_as_db_error(error)),
            },

        CancelPhaseProof::TransactionMayExist => {
            // SQLite may have auto-rolled the transaction back on INTERRUPT.
            mint_idle_post_finalize_proof(actor, control, lane, initial)
                .map(CancelEndFact::SQLiteAlreadyRolledBack)
                .map_err(actor_error_as_db_error)
        }

        CancelPhaseProof::BeginMayHaveOpened =>
            unreachable!("non-autocommit branch handled above"),
    };

    match fact.and_then(|fact| {
        seal_cancel_end_capability(actor, control, cause, fact)
            .map_err(actor_error_as_db_error)
    }) {
        Ok(cap) => {
            if let Err(error) = publish_clean_cancel_end(actor, control, cap) {
                publish_cancel_cleanup_failure(
                    actor, control, actor_error_as_db_error(error),
                );
            }
        }
        Err(error) => publish_cancel_cleanup_failure(actor, control, error),
    }
}
~~~

SQLite documents that errors including INTERRUPT can automatically roll back a
multi-statement transaction and that sqlite3_get_autocommit is the only way to
know
(/home/ruiyang/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/libsqlite3-sys-0.37.0/sqlite3/sqlite3.h:6830-6851).
Rusqlite exposes both is_autocommit and is_busy
(/home/ruiyang/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/rusqlite-0.39.0/src/lib.rs:1047-1057).
The actor scopes/drops the active statement, asserts !is_busy(), then runs the
cleanup above. A ROLLBACK error is benign only when is_autocommit is true; the
implementation MUST NOT string-match “no transaction is active”.

Reopening is deliberately outside cleanup’s result path. Failure to reopen can
degrade later availability but cannot skip the cancelled reservation’s terminal
record or strand its waiters.

Cancellation acknowledgement is sent only after the reservation is retired and
the connection is demonstrably clean or quarantined. Transaction state never
lives in the caller future.

## 16. Exact required interleavings

### Cancel arrives before execution starts

Execution start is the actor’s transition to Running, not an unknowable first
sqlite3_step instruction.

1. Caller sets CANCEL_INTENT in the shared terminal word.
2. Caller enqueues explicit `Cancel(ReservationId)`.
3. Actor sees the intent in its pre-start check and CASes Open+Intent to Cancel.
4. If nothing began, it removes the queued operation, retires the actor
   reservation, and acknowledges Cancelled(NoSqlStarted). SC-1 releases its
   ClaimGuard/admission only after reducing that proof. No BEGIN or data SQL is
   issued.
5. If BEGIN had already succeeded, it performs/validates rollback before the
   acknowledgement.

If the actor stores Running before the caller publishes intent, this is the
“during execution” interleaving below, even when SQLite has not reached its first
step. The progress latch covers that narrow physical gap.

### Cancel arrives during a long statement

1. Caller sets CANCEL_INTENT and enqueues `Cancel(ReservationId)`. Under active_sql it finds
   the exact (id, lane, connection generation, command sequence), marks that
   target interrupted, and invokes that target’s InterruptHandle. An authority
   read therefore targets op_conn; data/terminal SQL targets its actual lane.
2. Direct interrupt stops an already-running SQLite operation at its earliest
   opportunity. If it hit before stepping, the progress handler observes the
   same intent during the long statement.
3. On matching SQLITE_INTERRUPT the actor finalizes the statement, CASes
   Open+Intent to Cancel, and checks is_autocommit.
4. It performs rollback only if still needed, retires the reservation, removes
   the progress handler, then acknowledges the cause-specific cancellation.
5. If SQLite was too near completion and returns success, the shared
   pre-COMMIT CAS still lets the earlier intent win; the actor rolls back rather
   than committing.

SQLite expressly allows a nearly finished operation to complete despite an
interrupt
(/home/ruiyang/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/libsqlite3-sys-0.37.0/sqlite3/sqlite3.h:2899-2906);
the post-statement arbitration is what turns that permitted engine result into a
safe reservation outcome.

### Cancel arrives after commit but before the reply is polled

1. Before COMMIT, the actor already CASed Open to Complete.
2. COMMIT returned SQLITE_OK.
3. For an autocommit command the actor stored
   ActorTerminalOutcome::Autocommit(Ok(value)); for an explicit Settle it stored
   ActorTerminalOutcome::Committed. It changed phase to Terminal and only then
   sent the reply.
4. Dropping the still-unpolled future takes the command/owner gates, observes
   Complete+CompletionPromised, sets no CANCEL_INTENT, and sends/joins
   Cancel only as a terminal waiter. It does not call interrupt.
5. The actor returns AlreadyCompleted with that exact stored outcome. No
   ROLLBACK is sent or claimed; the write remains durable.

The reply channel has no bearing on durability today either: the source says SQL
has already committed or rolled back before a send to a dropped receiver
(crates/zeroship-data-v8/src/backend/sqlite/session.rs:155-161). The new
protocol makes that fact typed instead of silent.

### Cancel arrives after the reply is polled

OutcomeAwareCommandFuture::poll performs this order (the full implementation is
in section 13):

~~~rust
poll reply_rx fast path
if Pending, poll control.retention.outcome and register the real waker with a recheck
poll reply_rx once more to close the registration race
on either Ready path:
    project/validate the typed reply
    terminal_receipt.observe(reply)
    disarm drop_cancel_guard immediately before returning Ready
    return Ready(reply)
~~~

Dropping that future after Poll::Ready emits no Cancel. A retained explicit
ReservationCancelHandle keeps a terminal lease. For an autocommit or Settle
reply, Cancel(ReservationId) through that handle returns
AlreadyCompleted(outcome). If the
polled reply was only a nonterminal Execute in an explicit transaction, the Drop
still emits nothing but a later explicit Cancel is allowed to win and roll back
the still-open reservation. When the last future/cancel handle releases its
lease, ForgetTerminal is processed only after the actor sees zero refs and zero
queued control-command leases, allowing bounded terminal retention. A raw id
used after every valid lease is gone returns UnknownReservation; that is outside
the requested valid-handle interleaving.

**CAS-boundary note, part of the long-statement interleaving.**

This is the decisive sub-interleaving:

* CANCEL_INTENT fetch_or linearizes before the actor’s Open-to-Complete CAS:
  that CAS fails, actor CASes cancellation ownership, and rolls back.
* Open-to-Complete CAS linearizes first: caller observes Complete, does not
  interrupt, and eventually receives AlreadyCompleted with the COMMIT result.

No scheduling statement about reply send/poll is needed to decide the winner.

**Watchdog note (the bounded terminal algorithm, not a fifth required
interleaving).**

This is not a new caller cancellation. Once the actor owns Complete, the
independently armed TerminalSql watchdog may locate and interrupt the exact
COMMIT or ROLLBACK target. Once it owns Cancel, that same watchdog may interrupt
the cancellation cleanup ROLLBACK. It never changes either owner. If COMMIT had
already returned SQLITE_OK, the target is already cleared and the watchdog is
stale; the outcome is Committed (or Autocommit(Ok(value))). If an interrupt
makes terminal SQL return an error, the actor applies the result/autocommit
classifier below. It wakes settle and late-Cancel waiters with the same stored
outcome; it never rewrites Complete as cancellation merely because the raw code
is SQLITE_INTERRUPT.

## 17. Exact SQLite result-code mapping

SQLite assigns INTERRUPT primary code 9, BUSY 5, LOCKED 6,
ABORT_ROLLBACK 516, BUSY_RECOVERY 261, BUSY_SNAPSHOT 517, and BUSY_TIMEOUT 773
(/home/ruiyang/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/libsqlite3-sys-0.37.0/sqlite3/sqlite3.h:451-459,536-562).
ROW is 100 and DONE is 101
(/home/ruiyang/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/libsqlite3-sys-0.37.0/sqlite3/sqlite3.h:479-480);
LOCKED_SHAREDCACHE is 262, while
CONSTRAINT_CHECK/FOREIGNKEY/NOTNULL/UNIQUE are respectively 275/787/1299/2067
(/home/ruiyang/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/libsqlite3-sys-0.37.0/sqlite3/sqlite3.h:542-570).
Libsqlite maps primary INTERRUPT to OperationInterrupted
(/home/ruiyang/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/libsqlite3-sys-0.37.0/src/error.rs:6-25;
/home/ruiyang/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/libsqlite3-sys-0.37.0/src/error.rs:66-78).

The actor MUST retain the raw rusqlite error until it knows ReservationId,
terminal owner, cancellation intent, and the exact ActiveSqlTarget lane/
generation/command sequence. Mapping inside run_exec/run_query as today would
erase that context before the actor decides
(crates/zeroship-data-v8/src/backend/sqlite/session.rs:415-430;
crates/zeroship-data-v8/src/backend/sqlite/error.rs:52-140).

Raw codes never decide a terminal COMMIT/ROLLBACK by themselves. After dropping
the statement and proving !is_busy(), the actor samples is_autocommit and runs
this closed classifier. “Autocommit success/error” below means
ActorTerminalOutcome::Autocommit(Ok(value)/Err(original)); the explicit forms
are the adjacent named outcomes.

| Terminal statement | Raw result | is_autocommit immediately after | Required next step and immutable outcome |
| --- | --- | --- | --- |
| COMMIT after successful autocommit operation or explicit Commit | Ok | true | Autocommit success or Committed. This is the only confirmed-commit arm. |
| COMMIT | Ok | false | Protocol contradiction: quarantine and return AutocommitIndeterminate or CommitIndeterminate. Never publish success. |
| COMMIT | Err(e), including SQLITE_INTERRUPT/BUSY/LOCKED | false | COMMIT did not finish. Issue one ROLLBACK. If the post-rollback sample is true, return Autocommit(Err(commit_failed(e))) or CommitFailed(e); if still false, quarantine and return AutocommitIndeterminate or CommitIndeterminate. |
| COMMIT | Err(e), including SQLITE_INTERRUPT/BUSY/LOCKED | true | The transaction ended, but the error does not prove commit versus SQLite auto-rollback. Return AutocommitIndeterminate or CommitIndeterminate and quarantine. No listed SQLite code alone upgrades this to definitely rolled back. |
| ROLLBACK for explicit Rollback | Ok | true | RolledBack. |
| ROLLBACK for cancellation | Ok | true | Cancelled with cleanup RolledBack and the immutable latched cause. |
| ROLLBACK after an autocommit operation error | Ok | true | Autocommit error preserving the original operation error. |
| ROLLBACK | Err(e), including SQLITE_INTERRUPT | true | SQLite already ended the transaction. Explicit rollback is RolledBack; cancellation is Cancelled(SQLiteAlreadyRolledBack); failed autocommit is Autocommit(Err(original)). Retain e only as diagnostics. |
| ROLLBACK | Ok or Err(e) | false | Cleanup is not proved. Quarantine; explicit rollback returns RollbackFailed(e), cancellation or a failed autocommit returns CleanupIndeterminate(e). |

An autocommit DbOperation error is terminalized before this table: finalize the
statement, store TerminalAttempt::AutocommitFailure(original), and arbitrate the
owner. Cancel ownership uses cancellation cleanup. Complete ownership MUST NOT
send COMMIT: if is_autocommit is already true it stores Autocommit(Err(original));
otherwise it uses the ROLLBACK rows above. Thus a constraint, interrupt, or I/O
failure can never be followed by an accidental commit.

Every explicit Execute error first finalizes its statement, proves `!is_busy`,
and samples `is_autocommit`. The following is the single terminalization helper;
it covers 516 and every other SQLite auto-rollback, with 517 adding the required
fresh authority classification:

~~~rust
enum ExplicitAbortReason {
    // is_autocommit was already true after the failed data statement.
    BackendAutoRollback { mapped_error: DbError },
    // Extended result code 517; end the snapshot even if it remains live.
    BusySnapshot,
}

enum InterruptRoute {
    Cancellation {
        cause: CancelCause,
        phase: CancelPhaseProof,
    },
    TerminalWatchdog { owner: u8, statement: SqlStatementClass },
    Unexpected,
}

// Called only from run_statement_exact while terminal_owner_gate and active_sql
// are held and `target` is the exact live entry in actor.active_sql. It neither
// locks nor follows a borrowed target after that entry is removed.
fn classify_interrupt_from_locked_target(
    control: &ReservationControl,
    target: &ActiveSqlTarget,
    raw: &RusqliteError,
    word: u8,
    interrupt_generation: u64,
    interrupt_sequence: u64,
) -> Option<InterruptRoute> {
    if sqlite_extended_code(raw) != Some(SQLITE_INTERRUPT) { return None; }
    if owner(word) == OWNER_OPEN && has_cancel_intent(word)
        && matches!(target.statement,
            SqlStatementClass::PrepareAuthority
            | SqlStatementClass::Begin
            | SqlStatementClass::SnapshotMarker
            | SqlStatementClass::OperationAuthority
            | SqlStatementClass::Data
            | SqlStatementClass::FrameControl)
    {
        return Some(InterruptRoute::Cancellation {
            cause: control.cancel_cause.get().cloned()
                .expect("intent stores cause first"),
            phase: cancel_phase_for(target.statement),
        });
    }
    if interrupt_generation != 0
        && is_terminal_owner(owner(word))
        && interrupt_sequence == target.command_sequence
        && matches!(target.statement,
            SqlStatementClass::Commit
            | SqlStatementClass::Rollback
            | SqlStatementClass::FailureCleanupRollback
            | SqlStatementClass::CleanupRollback
            | SqlStatementClass::PostRollbackAuthority)
    {
        return Some(InterruptRoute::TerminalWatchdog {
            owner: owner(word), statement: target.statement,
        });
    }
    Some(InterruptRoute::Unexpected)
}

// This is the closed consumer for an authenticated raw 9. Cancellation claims
// or joins OWNER_CANCEL and then uses the total cleanup routine. A terminal
// watchdog never changes owner and is classified by statement plus the
// post-finalization autocommit sample. No arm reaches the generic mapper.
fn finish_authenticated_interrupt<T>(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    completed: CompletedSqlTargetProof,
    result: ClassifiedRawResult<T>,
) -> Result<(), ActorError> {
    match result {
        ClassifiedRawResult::CancellationInterrupt { raw, cause, phase } => {
            assert_eq!(phase, completed.cancel_phase);
            {
                let _owner = control.terminal_owner_gate.lock();
                let word = control.terminal.load(Acquire);
                if control.generation_fenced.load(Acquire) {
                    return Ok(()); // physical-fence fallback owns publication
                }
                match (word, control.cancel_cause.get()) {
                    (OPEN_WITH_CANCEL_INTENT, Some(stored)) if stored == &cause
                    | (CANCEL_WITH_INTENT, Some(stored)) if stored == &cause =>
                        actor.record_cancel_interrupt_diagnostic(
                            control.id, completed.clone(), raw,
                        ),
                    _ => return actor.protocol_fault_and_fence_with_source(
                        control, cancellation_owner_mismatch()),
                }
            }
            // Use the one combined claim path. It validates/adopts the terminal
            // budget, installs delivery/attempt, retires the command gate, and
            // performs the sole owner CAS. The queued Cancel later only joins.
            accept_cancel_command(
                actor,
                control,
                Some(completed.logical_cancellation_sequence),
                cause,
                control.retention.cancel_reply.clone(),
            );
            Ok(())
        }
        ClassifiedRawResult::TerminalWatchdogInterrupt {
            raw, owner: observed_owner, statement,
        } => {
            let word = control.terminal.load(Acquire);
            if owner(word) != observed_owner
                || !is_terminal_owner(observed_owner)
                || statement != completed.statement
            {
                return actor.protocol_fault_and_fence_with_source(
                    control, terminal_watchdog_owner_mismatch());
            }
            // Owner is preserved. These five arms are deliberately exhaustive.
            match (observed_owner, statement) {
                (OWNER_COMPLETE, SqlStatementClass::Commit) =>
                    actor.finish_interrupted_commit(control, completed, raw),
                (OWNER_COMPLETE, SqlStatementClass::Rollback) =>
                    actor.finish_interrupted_explicit_rollback(
                        control, completed, raw,
                    ),
                (OWNER_COMPLETE, SqlStatementClass::FailureCleanupRollback) =>
                    actor.finish_interrupted_failure_cleanup(
                        control, completed, raw,
                    ),
                (OWNER_CANCEL, SqlStatementClass::CleanupRollback) => {
                    actor.finish_interrupted_cancel_rollback(
                        control, completed, raw,
                    );
                    Ok(())
                }
                (OWNER_COMPLETE, SqlStatementClass::PostRollbackAuthority) =>
                    actor.finish_interrupted_postrollback_authority(
                        control, completed, raw,
                    ),
                _ => actor.protocol_fault_and_fence_with_source(
                    control, terminal_watchdog_statement_mismatch()),
            }
        }
        ClassifiedRawResult::UnexpectedInterruptFenced { .. } => Ok(()),
        ClassifiedRawResult::Ok(_) | ClassifiedRawResult::SqlError(_) =>
            Err(ActorError::CancellationProtocolMismatch),
    }
}

// No ordinary data/authority callsite and no completion-owned failure-cleanup
// callsite may match StatementRun directly. This generic gate consumes raw 9
// while preserving T for Ok; only a non-9 SqlError may reach a statement-
// specific ordinary classifier. OWNER_CANCEL CleanupRollback is the sole
// exception: execute_cancel_cleanup_rollback owns a closed, specialized
// converter because it must classify rollback result and post-return
// autocommit state as one indivisible cancellation capability.
fn consume_statement_run_exact<T>(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    run: StatementRun<T>,
) -> Result<Option<(Result<T, RusqliteError>, CompletedSqlTargetProof)>,
            ActorError> {
    match run {
        StatementRun::Suppressed(_) => Ok(None),
        StatementRun::Ran {
            result: ClassifiedRawResult::Ok(value), completed,
        } => Ok(Some((Ok(value), completed))),
        StatementRun::Ran {
            result: ClassifiedRawResult::SqlError(raw), completed,
        } => {
            assert_ne!(sqlite_extended_code(&raw), Some(SQLITE_INTERRUPT));
            Ok(Some((Err(raw), completed)))
        }
        StatementRun::Ran {
            result @ (ClassifiedRawResult::CancellationInterrupt { .. }
                | ClassifiedRawResult::TerminalWatchdogInterrupt { .. }
                | ClassifiedRawResult::UnexpectedInterruptFenced { .. }),
            completed,
        } => {
            finish_authenticated_interrupt(actor, control, completed, result)?;
            Ok(None)
        }
    }
}

// The only explicit COMMIT/ROLLBACK callsite. classify_root_finish_exact samples
// !is_busy/is_autocommit under the finalization barrier, performs the bounded
// error-cleanup arm from the result table, and mints either RootEndCapability or
// class-matching UncertainEndCapability; callers cannot construct either seal.
fn execute_explicit_root_terminal_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    decision: RootDecision,
) -> Result<(), ActorError> {
    let sequence = control.current_terminal_sequence.load(Acquire);
    let statement = match decision {
        RootDecision::Commit => SqlStatementClass::Commit,
        RootDecision::Rollback => SqlStatementClass::Rollback,
    };
    let sql = match decision {
        RootDecision::Commit => "COMMIT",
        RootDecision::Rollback => "ROLLBACK",
    };
    let run = run_statement_exact(
        actor, control, &actor.tx_conn, ConnectionLane::Tx,
        sequence, sequence, statement,
        || actor.tx_conn.execute_batch(sql),
    )?;
    let Some((raw, completed)) =
        consume_statement_run_exact(actor, control, run)?
    else { return Ok(()); };
    let Some((capability, result)) = classify_root_finish_exact(
        actor, control, decision, completed, raw,
    )? else { return Ok(()); };
    publish_root_terminal_exact(actor, control, capability, result)
}

// The only autocommit COMMIT callsite. Failed-operation rollback has the
// separate OWNER_COMPLETE FailureCleanupRollback helper below, so a watchdog
// cannot confuse it with OWNER_CANCEL cleanup.
fn execute_autocommit_commit_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    value: SharedDbResult,
) -> Result<(), ActorError> {
    let statement = SqlStatementClass::Commit;
    let sequence = control.current_terminal_sequence.load(Acquire);
    let run = run_statement_exact(
        actor, control, &actor.op_conn, ConnectionLane::Op,
        sequence, sequence, statement,
        || actor.op_conn.execute_batch("COMMIT"),
    )?;
    let Some((raw, completed)) =
        consume_statement_run_exact(actor, control, run)?
    else { return Ok(()); };
    let Some(conclusion) = classify_autocommit_commit_exact(
        actor, control, value, completed, raw,
    )? else { return Ok(()); };
    publish_autocommit_terminal_exact(actor, control, conclusion)
}

// The following closed classifiers are part of the protocol, not illustrative
// pseudocode. Every terminal SQLite return reaches exactly one of them after
// run_statement_exact has removed the progress handler and active target.
enum FailureCleanupRun {
    Confirmed {
        completed: CompletedSqlTargetProof,
        context: FailureCleanupContext,
    },
    Indeterminate {
        sequence: u64,
        context: FailureCleanupContext,
        error: DbError,
    },
    // An authenticated watchdog interrupt or a physical fence already owns
    // publication. The caller must not publish again.
    Terminalized,
}

fn map_terminal_sql_error_exact(
    statement: SqlStatementClass,
    raw: &RusqliteError,
) -> DbError {
    if sqlite_extended_code(raw) == Some(SQLITE_INTERRUPT) {
        // Authenticated terminal-watchdog raw 9 never enters the ordinary
        // mapper. Statement class supplies its typed diagnostic source.
        terminal_watchdog_interrupted_db_error(statement)
    } else {
        map_sqlite_error(raw)
    }
}

// Private proof type: only the constructor below can mint it, while the owner
// and active-target gates bind both sequential statements to one reservation.
struct SnapshotAbortEndSeal {
    id: ReservationId,
    ended_sequence: u64,
    authority_sequence: u64,
}

impl SnapshotAbortEndSeal {
    fn mint_under_owner_gate(
        id: ReservationId,
        ended: &CompletedSqlTargetProof,
        authority: &CompletedSqlTargetProof,
    ) -> Self {
        Self {
            id,
            ended_sequence: ended.command_sequence,
            authority_sequence: authority.command_sequence,
        }
    }

    fn authenticates_exact_chain(
        &self,
        id: ReservationId,
        ended: &CompletedSqlTargetProof,
        authority: &CompletedSqlTargetProof,
    ) -> bool {
        self.id == id
            && self.ended_sequence == ended.command_sequence
            && self.authority_sequence == authority.command_sequence
            && ended.id == id
            && authority.id == id
            && ended.command_sequence != authority.command_sequence
    }
}

fn failure_cleanup_error_exact(
    context: &FailureCleanupContext,
    cleanup: Option<DbError>,
) -> DbError {
    match context {
        FailureCleanupContext::ExplicitCommit { commit_error, .. }
        | FailureCleanupContext::AutocommitCommit { commit_error, .. } =>
            commit_cleanup_indeterminate(commit_error.clone(), cleanup),
        FailureCleanupContext::AutocommitOperation { original_error } =>
            operation_cleanup_indeterminate(original_error.clone(), cleanup),
        FailureCleanupContext::SnapshotAbort(snapshot) =>
            snapshot_cleanup_indeterminate(
                snapshot.raw_mapped.clone(), cleanup,
            ),
    }
}

fn mint_root_end_capability_exact(
    actor: &AppActor,
    control: &ReservationControl,
    decision: RootDecision,
    terminal: CompletedSqlTargetProof,
    cleanup: Option<CompletedSqlTargetProof>,
) -> Result<RootEndCapability, ActorError> {
    let _owner = control.terminal_owner_gate.lock();
    let active = actor.active_sql.lock();
    let word = control.terminal.load(Acquire);
    let delivery_id = selected_terminal_delivery_id(control)?;
    let terminal_ok = terminal.id == control.id
        && terminal.lane == ConnectionLane::Tx
        && terminal.connection_generation
            == actor.generation_for(ConnectionLane::Tx)
        && matches!((decision, terminal.statement),
            (RootDecision::Commit, SqlStatementClass::Commit)
                | (RootDecision::Rollback, SqlStatementClass::Rollback));
    let sequence_ok = cleanup.as_ref().map_or_else(
        || terminal.command_sequence
            == control.current_terminal_sequence.load(Acquire),
        |proof| proof.id == control.id
            && proof.lane == ConnectionLane::Tx
            && proof.connection_generation
                == actor.generation_for(ConnectionLane::Tx)
            && proof.statement == SqlStatementClass::FailureCleanupRollback
            && proof.command_sequence
                == control.current_terminal_sequence.load(Acquire),
    );
    if active.is_some()
        || owner(word) != OWNER_COMPLETE
        || !terminal_ok
        || !sequence_ok
        || actor.tx_conn.is_busy()
        || !actor.tx_conn.is_autocommit()
    {
        return Err(ActorError::CancellationProtocolMismatch);
    }
    let seal = RootEndSeal::mint_from_exact_chain(
        &terminal, cleanup.as_ref(), delivery_id, word,
    );
    Ok(RootEndCapability {
        id: control.id,
        actor_generation: control.actor_generation,
        delivery_id,
        decision,
        terminal,
        cleanup,
        observed_owner_word: word,
        seal,
    })
}

fn mint_uncertain_end_capability_exact(
    actor: &AppActor,
    control: &ReservationControl,
    class: TerminalPublicationClass,
    sequence: u64,
) -> Result<UncertainEndCapability, ActorError> {
    let _owner = control.terminal_owner_gate.lock();
    let active = actor.active_sql.lock();
    let word = control.terminal.load(Acquire);
    let delivery_id = selected_terminal_delivery_id(control)?;
    if active.is_some()
        || control.generation_fenced.load(Acquire)
        || !is_terminal_owner(owner(word))
        || sequence != control.current_terminal_sequence.load(Acquire)
        || terminal_class_and_delivery_id(
            control.terminal_delivery.lock().as_ref()
                .ok_or(ActorError::CancellationProtocolMismatch)?,
        ) != (class, delivery_id)
    {
        return Err(ActorError::CancellationProtocolMismatch);
    }
    Ok(UncertainEndCapability {
        id: control.id,
        actor_generation: control.actor_generation,
        delivery_id,
        class,
        sequence: Some(sequence),
        observed_owner_word: word,
        seal: UncertainEndSeal::mint_under_owner_gate(
            control.id, delivery_id, class, word,
        ),
    })
}

fn execute_failure_cleanup_rollback_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    lane: ConnectionLane,
    context: FailureCleanupContext,
) -> Result<FailureCleanupRun, ActorError> {
    let predecessor = control.current_terminal_sequence.load(Acquire);
    let sequence = mint_never_reused_command_sequence();
    retarget_terminal_sequence_exact(actor, control, predecessor, sequence)?;
    {
        let _owner = control.terminal_owner_gate.lock();
        if owner(control.terminal.load(Acquire)) != OWNER_COMPLETE
            || actor.active_sql.lock().is_some()
            || control.failure_cleanup_context.lock().is_some()
        {
            return Err(ActorError::CancellationProtocolMismatch);
        }
        *control.failure_cleanup_context.lock() = Some(context.clone());
    }

    let conn = actor.connection(lane);
    let run = run_statement_exact(
        actor, control, conn, lane, sequence, sequence,
        SqlStatementClass::FailureCleanupRollback,
        || conn.execute_batch("ROLLBACK"),
    )?;
    let Some((raw, completed)) =
        consume_statement_run_exact(actor, control, run)?
    else {
        // The watchdog continuation consumes the context before publication.
        // A physical fence may leave it descriptive until generation teardown.
        debug_assert!(control.failure_cleanup_context.lock().is_none()
            || control.generation_fenced.load(Acquire));
        return Ok(FailureCleanupRun::Terminalized);
    };
    let stored = control.failure_cleanup_context.lock().take()
        .ok_or(ActorError::CancellationProtocolMismatch)?;
    if let Err(ref error) = raw {
        actor.record_terminal_cleanup_diagnostic(
            control.id, completed.clone(), map_sqlite_error(error),
        );
    }
    if !conn.is_busy() && conn.is_autocommit() {
        Ok(FailureCleanupRun::Confirmed {
            completed,
            context: stored,
        })
    } else {
        actor.quarantine_generation_before_wake(control.actor_generation);
        Ok(FailureCleanupRun::Indeterminate {
            sequence,
            error: failure_cleanup_error_exact(
                &stored,
                raw.as_ref().err().map(map_sqlite_error),
            ),
            context: stored,
        })
    }
}

fn classify_root_finish_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    decision: RootDecision,
    terminal: CompletedSqlTargetProof,
    raw: Result<(), RusqliteError>,
) -> Result<Option<(TerminalEndCapability, RootFinishResult)>, ActorError> {
    if actor.tx_conn.is_busy() {
        return actor.protocol_fault_and_fence_with_source(
            control, terminal_statement_still_busy(),
        ).map(|_| None);
    }
    let autocommit = actor.tx_conn.is_autocommit();
    let mapped = raw.as_ref().err().map(|error|
        map_terminal_sql_error_exact(
            match decision {
                RootDecision::Commit => SqlStatementClass::Commit,
                RootDecision::Rollback => SqlStatementClass::Rollback,
            },
            error,
        ));
    match (decision, raw, autocommit) {
        (RootDecision::Commit, Ok(()), true) => {
            let cap = mint_root_end_capability_exact(
                actor, control, decision, terminal, None,
            )?;
            Ok(Some((TerminalEndCapability::Root(cap),
                RootFinishResult::Committed)))
        }
        (RootDecision::Commit, Err(_), false) => {
            let commit_error = mapped.expect("Err mapped above");
            match execute_failure_cleanup_rollback_exact(
                actor, control, ConnectionLane::Tx,
                FailureCleanupContext::ExplicitCommit {
                    terminal: terminal.clone(),
                    commit_error: commit_error.clone(),
                },
            )? {
                FailureCleanupRun::Confirmed { completed, .. } => {
                    let cap = mint_root_end_capability_exact(
                        actor, control, decision, terminal, Some(completed),
                    )?;
                    Ok(Some((TerminalEndCapability::Root(cap),
                        RootFinishResult::Failed {
                            error: commit_error,
                            certainty: FinishCertainty::DefinitelyNotCommitted,
                        })))
                }
                FailureCleanupRun::Indeterminate {
                    sequence, error, ..
                } => {
                    let cap = mint_uncertain_end_capability_exact(
                        actor, control, TerminalPublicationClass::Root,
                        sequence,
                    )?;
                    Ok(Some((TerminalEndCapability::Uncertain(cap),
                        RootFinishResult::Failed {
                            error,
                            certainty: FinishCertainty::Indeterminate,
                        })))
                }
                FailureCleanupRun::Terminalized => Ok(None),
            }
        }
        (RootDecision::Commit, result, _) => {
            let error = commit_outcome_indeterminate(
                result.err().and_then(|_| mapped), autocommit,
            );
            let sequence = control.current_terminal_sequence.load(Acquire);
            let cap = mint_uncertain_end_capability_exact(
                actor, control, TerminalPublicationClass::Root, sequence,
            )?;
            Ok(Some((TerminalEndCapability::Uncertain(cap),
                RootFinishResult::Failed {
                    error,
                    certainty: FinishCertainty::Indeterminate,
                })))
        }
        (RootDecision::Rollback, result, true) => {
            if let Err(error) = result {
                actor.record_terminal_cleanup_diagnostic(
                    control.id, terminal.clone(),
                    map_terminal_sql_error_exact(
                        SqlStatementClass::Rollback, &error,
                    ),
                );
            }
            let cap = mint_root_end_capability_exact(
                actor, control, decision, terminal, None,
            )?;
            Ok(Some((TerminalEndCapability::Root(cap),
                RootFinishResult::RolledBack)))
        }
        (RootDecision::Rollback, result, false) => {
            let error = rollback_outcome_indeterminate(
                result.err().map(|error| map_terminal_sql_error_exact(
                    SqlStatementClass::Rollback, &error,
                )),
            );
            let sequence = control.current_terminal_sequence.load(Acquire);
            let cap = mint_uncertain_end_capability_exact(
                actor, control, TerminalPublicationClass::Root, sequence,
            )?;
            Ok(Some((TerminalEndCapability::Uncertain(cap),
                RootFinishResult::Failed {
                    error,
                    certainty: FinishCertainty::Indeterminate,
                })))
        }
    }
}

fn classify_autocommit_commit_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    value: SharedDbResult,
    terminal: CompletedSqlTargetProof,
    raw: Result<(), RusqliteError>,
) -> Result<Option<AutocommitTerminalConclusion>, ActorError> {
    if actor.op_conn.is_busy() {
        return actor.protocol_fault_and_fence_with_source(
            control, terminal_statement_still_busy(),
        ).map(|_| None);
    }
    let autocommit = actor.op_conn.is_autocommit();
    match (raw, autocommit) {
        (Ok(()), true) => Ok(Some(
            AutocommitTerminalConclusion::CommitConfirmed {
                completed: terminal,
                value,
            },
        )),
        (Err(raw), false) => {
            let commit_error = map_terminal_sql_error_exact(
                SqlStatementClass::Commit, &raw,
            );
            match execute_failure_cleanup_rollback_exact(
                actor, control, ConnectionLane::Op,
                FailureCleanupContext::AutocommitCommit {
                    terminal: terminal.clone(),
                    commit_error: commit_error.clone(),
                },
            )? {
                FailureCleanupRun::Confirmed { completed, .. } => Ok(Some(
                    AutocommitTerminalConclusion::CommitDefinitelyFailed {
                        terminal,
                        cleanup: completed,
                        error: commit_error,
                    },
                )),
                FailureCleanupRun::Indeterminate {
                    sequence, error, ..
                } => Ok(Some(
                    AutocommitTerminalConclusion::CommitIndeterminate {
                        sequence,
                        error,
                    },
                )),
                FailureCleanupRun::Terminalized => Ok(None),
            }
        }
        (result, observed_autocommit) => Ok(Some(
            AutocommitTerminalConclusion::CommitIndeterminate {
                sequence: terminal.command_sequence,
                error: commit_outcome_indeterminate(
                    result.err().map(|error| map_terminal_sql_error_exact(
                        SqlStatementClass::Commit, &error,
                    )),
                    observed_autocommit,
                ),
            },
        )),
    }
}

// ActorCommand::Settle dispatches here. This is the missing root owner-CAS
// callsite: no COMMIT/ROLLBACK starts until SC-1's exact shared route and
// TerminalSql deadline are authenticated by actor_claim_completion.
fn claim_and_execute_explicit_root_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    command_sequence: u64,
    decision: RootDecision,
    delivery: TerminalDelivery,
) -> Result<(), ActorError> {
    let TerminalDelivery::ExplicitRoot { cutoff, .. } = &delivery else {
        return Err(ActorError::ForeignReservation);
    };
    let slots = control.explicit_deadlines.clone()
        .ok_or(ActorError::ForeignReservation)?;
    let attempt = match decision {
        RootDecision::Commit => TerminalAttempt::ExplicitCommit,
        RootDecision::Rollback => TerminalAttempt::ExplicitRollback,
    };
    match actor_claim_completion(
        control,
        command_sequence,
        attempt,
        delivery,
        cutoff.delivery_id,
        TerminalBudgetArm::SharedExplicit {
            slots,
            expected_kind: DeadlineKind::TerminalSql,
        },
    ) {
        Ok(()) => execute_explicit_root_terminal_exact(
            actor, control, decision,
        ),
        Err(OwnerClaimError::Lost { observed })
            if observed == OPEN_WITH_CANCEL_INTENT
                || owner(observed) == OWNER_CANCEL => Ok(()),
        Err(error) => actor.handle_terminal_claim_failure(control.id, error),
    }
}

fn finish_autocommit_operation_failure_after_claim_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    completed: CompletedSqlTargetProof,
    original_error: DbError,
) -> Result<(), ActorError> {
    if !actor.op_conn.is_busy() && actor.op_conn.is_autocommit() {
        return publish_autocommit_terminal_exact(
            actor,
            control,
            AutocommitTerminalConclusion::OperationFailedCleanupConfirmed {
                completed,
                error: original_error,
            },
        );
    }
    match execute_failure_cleanup_rollback_exact(
        actor,
        control,
        ConnectionLane::Op,
        FailureCleanupContext::AutocommitOperation {
            original_error: original_error.clone(),
        },
    )? {
        FailureCleanupRun::Confirmed { completed, .. } =>
            publish_autocommit_terminal_exact(
                actor,
                control,
                AutocommitTerminalConclusion::OperationFailedCleanupConfirmed {
                    completed,
                    error: original_error,
                },
            ),
        FailureCleanupRun::Indeterminate {
            sequence, error, ..
        } => publish_autocommit_terminal_exact(
            actor,
            control,
            AutocommitTerminalConclusion::CleanupIndeterminate {
                sequence,
                error,
            },
        ),
        FailureCleanupRun::Terminalized => Ok(()),
    }
}

// The exact post-Data completion callsite for autocommit. This is the terminal
// linearization CAS for both success and failure; caller cancellation can win
// only before this function's actor_claim_completion CAS.
fn terminalize_autocommit_operation_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    completed: CompletedSqlTargetProof,
    result: Result<SharedDbResult, RusqliteError>,
) -> Result<(), ActorError> {
    let delivery = control.cancel_delivery.clone();
    let ReservationCutoff::Autocommit(cutoff) = &control.terminal_cutoff else {
        return Err(ActorError::ForeignReservation);
    };
    let trigger = control.preclaim_autocommit_fence_trigger.get()
        .ok_or(ActorError::CancellationProtocolMismatch)?
        .clone();
    let (attempt, ordinary) = match result {
        Ok(value) => (
            TerminalAttempt::AutocommitSuccess(value.clone()),
            Ok(value),
        ),
        Err(raw) => {
            assert_ne!(sqlite_extended_code(&raw), Some(SQLITE_INTERRUPT));
            let mapped = map_sqlite_error(&raw);
            if sqlite_extended_code(&raw) == Some(SQLITE_BUSY_SNAPSHOT) {
                (TerminalAttempt::AutocommitSnapshotAbortPending(
                    mapped.clone(),
                ), Err((raw, mapped)))
            } else {
                (TerminalAttempt::AutocommitFailure(mapped.clone()),
                 Err((raw, mapped)))
            }
        }
    };
    match actor_claim_completion(
        control,
        completed.command_sequence,
        attempt,
        delivery,
        cutoff.delivery_id,
        TerminalBudgetArm::ActorOwned {
            first_deadline: min(
                control.execution_deadline_at
                    .unwrap_or_else(|| Instant::now()
                        + control.terminal_sql_timeout),
                Instant::now() + control.terminal_sql_timeout,
            ),
            trigger,
        },
    ) {
        Ok(()) => {}
        Err(OwnerClaimError::Lost { observed })
            if observed == OPEN_WITH_CANCEL_INTENT
                || owner(observed) == OWNER_CANCEL => {
            actor.record_cancel_race_diagnostic_from_operation(
                control.id, completed,
            );
            return Ok(());
        }
        Err(error) =>
            return actor.handle_terminal_claim_failure(control.id, error),
    }
    match ordinary {
        Ok(value) => execute_autocommit_commit_exact(
            actor, control, value,
        ),
        Err((raw, mapped))
            if sqlite_extended_code(&raw) == Some(SQLITE_BUSY_SNAPSHOT) =>
            finish_busy_snapshot_after_claim_exact(
                actor,
                control,
                SnapshotAbortContext {
                    data_completed: completed,
                    raw_mapped: mapped,
                    destination: SnapshotAbortDestination::Autocommit,
                },
            ),
        Err((_raw, mapped)) =>
            finish_autocommit_operation_failure_after_claim_exact(
                actor, control, completed, mapped,
            ),
    }
}

// Concrete ordinary callsites. They demonstrate the mandatory ordering; raw 9
// cannot be mapped by either data or authority code because the generic gate
// consumes it first.
fn execute_autocommit_data_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    command_sequence: u64,
    operation: &DbOperation,
) -> Result<(), ActorError> {
    let run = run_statement_exact(
        actor, control, &actor.op_conn, ConnectionLane::Op,
        command_sequence, command_sequence, SqlStatementClass::Data,
        || actor.op_conn.execute_operation(operation),
    )?;
    let Some((result, completed)) =
        consume_statement_run_exact(actor, control, run)?
    else { return Ok(()); };
    terminalize_autocommit_operation_exact(actor, control, completed, result)
}

fn execute_platform_authority_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    command_sequence: u64,
    class: SqlStatementClass,
) -> Result<Option<(AuthorityObservation, CompletedSqlTargetProof)>,
            ActorError> {
    assert!(matches!(class,
        SqlStatementClass::PrepareAuthority
            | SqlStatementClass::OperationAuthority
            | SqlStatementClass::PostRollbackAuthority));
    let run = run_statement_exact(
        actor, control, &actor.op_conn, ConnectionLane::Op,
        command_sequence, command_sequence, class,
        || actor.op_conn.read_authority_platform_role(control.id.app),
    )?;
    let Some((result, completed)) =
        consume_statement_run_exact(actor, control, run)?
    else { return Ok(None); };
    match result {
        Ok(observation) => Ok(Some((observation, completed))),
        Err(raw) => Err(ActorError::Database(map_sqlite_error(&raw))),
    }
}

// Begin, SnapshotMarker, OperationAuthority, Data, FrameControl, and both
// authority-read variants use the same shape: run_statement_exact followed by
// this same generic consumer. A compile-fail test makes direct Connection
// execution unavailable outside this module; a mutation test that bypasses a
// consumer leaves raw SQLITE_INTERRUPT unmapped and must fail the matrix gate.

// Every call site must exhaust this matrix. `SqlError` is the sole input to
// map_sqlite_error. Suppressed and either interrupt variant never enter it.
//
// | Runner statement | CancellationInterrupt | TerminalWatchdogInterrupt |
// | --- | --- | --- |
// | PrepareAuthority | claim/join cancel, NoTransactionPossible cleanup | protocol-fence (unreachable class) |
// | Begin | claim/join cancel, BeginMayHaveOpened cleanup | protocol-fence |
// | SnapshotMarker, OperationAuthority, Data, FrameControl | claim/join cancel, TransactionMayExist cleanup | protocol-fence |
// | Commit | protocol-fence | preserve Complete; COMMIT classifier |
// | Rollback | protocol-fence | preserve Complete; explicit rollback classifier |
// | FailureCleanupRollback | protocol-fence | preserve Complete; commit/autocommit failure-cleanup classifier |
// | CleanupRollback | protocol-fence | preserve Cancel; total cancellation cleanup classifier |
// | PostRollbackAuthority | protocol-fence | preserve Complete; return indeterminate snapshot-abort outcome |

fn terminalize_explicit_execute_error(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    completed: CompletedSqlTargetProof,
    raw: RusqliteError,
    delivery: TerminalDelivery,
) -> Result<(), ActorError> {
    // Only ClassifiedRawResult::SqlError reaches this helper, so raw 9 cannot
    // be erased by today's mapper. run_statement_exact already finalized the
    // exact target and progress hook exactly once.
    assert_ne!(sqlite_extended_code(&raw), Some(SQLITE_INTERRUPT));
    let command_sequence = completed.command_sequence;
    let mapped_error = map_sqlite_error(&raw);
    if actor.tx_conn.is_busy() {
        return actor.protocol_fault_and_fence(control.id);
    }
    let auto_rolled_back = actor.tx_conn.is_autocommit();
    let reason = if sqlite_extended_code(&raw) == Some(SQLITE_BUSY_SNAPSHOT) {
        ExplicitAbortReason::BusySnapshot
    } else if auto_rolled_back {
        ExplicitAbortReason::BackendAutoRollback {
            mapped_error: mapped_error.clone(),
        }
    } else if sqlite_extended_code(&raw) == Some(SQLITE_ABORT_ROLLBACK) {
        // 516 says a statement was aborted because a rollback occurred; with
        // autocommit still false it does not prove which transaction state is
        // reusable. Publish Unknown so SC-1 enters QuarantineUnknown and drives
        // its sole explicit Cancel; never return an ordinary reusable session.
        return actor.publish_nonterminal_data_error_with_health(
            control.id,
            command_sequence,
            mapped_error,
            HealthImpact::Unknown,
        );
    } else {
        // Healthy/Poisoned is obtained from the existing statement classifier.
        // This is a nonterminal explicit Execute and never touches OWNER_MASK.
        return actor.publish_nonterminal_data_error(
            control.id, command_sequence, mapped_error,
        );
    };

    let TerminalDelivery::ExplicitDataAbort {
        permit, key, cutoff, data_token, registry, hard_stop_trigger,
    } = delivery.clone() else {
        return actor.protocol_fault_and_fence(control.id);
    };
    // These checks are repeated before the combined claim; no lookup into
    // TxRegistry occurs while actor locks are held.
    if key.app != control.id.app
        || control.tx_key.as_ref() != Some(&key)
        || permit.key != key
        || permit.actor_generation != control.actor_generation
        || permit.data_token != data_token
        || permit.cutoff_delivery_id != cutoff.delivery_id
        || hard_stop_trigger.job() != permit.fence_job
    {
        return actor.protocol_fault_and_fence(control.id);
    }

    let attempt = match &reason {
        ExplicitAbortReason::BackendAutoRollback { mapped_error } =>
            TerminalAttempt::ExplicitTransactionAborted(mapped_error.clone()),
        ExplicitAbortReason::BusySnapshot =>
            TerminalAttempt::ExplicitSnapshotAbortPending,
    };
    let budget = TerminalBudgetArm::ActorOwned {
        first_deadline: min(
            control.execution_deadline_at.expect("admitted explicit tx"),
            Instant::now() + control.terminal_sql_timeout,
        ),
        trigger: hard_stop_trigger,
    };
    match actor_claim_completion(
        control,
        command_sequence,
        attempt,
        delivery,
        cutoff.delivery_id,
        budget,
    ) {
        Ok(()) => {}
        Err(OwnerClaimError::Lost { observed })
            if observed == (OWNER_OPEN | CANCEL_INTENT)
                || owner(observed) == OWNER_CANCEL => {
            actor.record_cancel_race_diagnostic(control.id, raw);
            // The sole explicit Cancel owns/awaits cleanup. Do not publish an
            // ordinary DataCompleted or a competing DataAbortCompleted.
            return Ok(());
        }
        Err(error) => return actor.handle_terminal_claim_failure(control.id, error),
    }

    let (terminal_error, completed_end, postrollback_authority) = match reason {
        ExplicitAbortReason::BackendAutoRollback { mapped_error } => {
            // run_statement_exact finalized Data and the immediate sample
            // proved SQLite had ended this exact transaction.
            debug_assert!(actor.tx_conn.is_autocommit());
            (mapped_error, completed, None)
        }
        ExplicitAbortReason::BusySnapshot => {
            finish_busy_snapshot_after_claim_exact(
                actor,
                control,
                SnapshotAbortContext {
                    data_completed: completed,
                    raw_mapped: mapped_error,
                    destination: SnapshotAbortDestination::Explicit {
                        permit,
                    },
                },
            )?;
            return Ok(());
        }
    };

    {
        let _owner = control.terminal_owner_gate.lock();
        if control.generation_fenced.load(Acquire)
            || owner(control.terminal.load(Acquire)) != OWNER_COMPLETE
        {
            return Ok(()); // hard-stop owns the cutoff/fallback
        }
        *control.terminal_attempt.lock() = Some(match &postrollback_authority {
            Some(_) =>
                TerminalAttempt::ExplicitSnapshotAbort(terminal_error.clone()),
            _ => TerminalAttempt::ExplicitTransactionAborted(
                terminal_error.clone(),
            ),
        });
    }
    let cap = seal_data_abort_end_capability(
        actor,
        control,
        permit,
        completed_end,
        postrollback_authority,
    )?;
    publish_data_abort_terminal_exact(actor, control, cap, terminal_error)
}

fn retarget_terminal_sequence_exact(
    actor: &AppActor,
    control: &ReservationControl,
    expected: u64,
    next: u64,
) -> Result<(), ActorError> {
    let _owner = control.terminal_owner_gate.lock();
    let active = actor.active_sql.lock();
    let g1 = control.terminal_interrupt_generation.load(Acquire);
    let armed_sequence = control.terminal_interrupt_sequence.load(Acquire);
    let g2 = control.terminal_interrupt_generation.load(Acquire);
    if active.is_some()
        || control.generation_fenced.load(Acquire)
        || owner(control.terminal.load(Acquire)) != OWNER_COMPLETE
        || control.current_terminal_sequence.load(Acquire) != expected
        || g1 != g2
        || (g1 != 0 && armed_sequence != expected)
    {
        return Err(ActorError::CancellationProtocolMismatch);
    }
    if g1 != 0 {
        // Retarget the fired absolute watchdog; never clear or extend it.
        control.terminal_interrupt_sequence.store(next, Release);
    }
    control.current_terminal_sequence.store(next, Release);
    Ok(())
}

fn force_data_abort_fence_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    error: DbError,
) -> Result<(), ActorError> {
    let trigger = match control.terminal_delivery.lock().as_ref() {
        Some(TerminalDelivery::ExplicitDataAbort {
            hard_stop_trigger, ..
        }) => hard_stop_trigger.clone(),
        _ => return Err(ActorError::CancellationProtocolMismatch),
    };
    let _owner = control.terminal_owner_gate.lock();
    if control.generation_fenced.load(Acquire) {
        return Ok(());
    }
    *control.terminal_attempt.lock() =
        Some(TerminalAttempt::ExplicitSnapshotAbort(error.clone()));
    let _ = claim_protocol_fault_cutoff_locked(
        &actor.supervisor, control, &trigger, error,
    );
    Ok(())
}

fn publish_snapshot_cleanup_indeterminate_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    snapshot: SnapshotAbortContext,
    sequence: u64,
    error: DbError,
) -> Result<(), ActorError> {
    actor.quarantine_generation_before_wake(control.actor_generation);
    match snapshot.destination {
        SnapshotAbortDestination::Explicit { .. } => {
            // Explicit DataAbort has a registered, permit-authenticated physical
            // fence. It is the only publisher when rollback cannot prove end.
            force_data_abort_fence_exact(actor, control, error)
        }
        SnapshotAbortDestination::Autocommit =>
            publish_autocommit_terminal_exact(
                actor,
                control,
                AutocommitTerminalConclusion::CleanupIndeterminate {
                    sequence,
                    error,
                },
            ),
    }
}

fn publish_snapshot_abort_classification_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    context: PostRollbackAuthorityContext,
    authority: CompletedSqlTargetProof,
    classified: DbError,
) -> Result<(), ActorError> {
    let PostRollbackAuthorityContext { ended, snapshot } = context;
    match snapshot.destination {
        SnapshotAbortDestination::Explicit { permit } => {
            {
                let _owner = control.terminal_owner_gate.lock();
                if owner(control.terminal.load(Acquire)) != OWNER_COMPLETE
                    || actor.active_sql.lock().is_some()
                    || authority.command_sequence
                        != control.current_terminal_sequence.load(Acquire)
                {
                    return Err(ActorError::CancellationProtocolMismatch);
                }
                *control.terminal_attempt.lock() = Some(
                    TerminalAttempt::ExplicitSnapshotAbort(classified.clone()),
                );
            }
            let cap = seal_data_abort_end_capability(
                actor,
                control,
                permit,
                ended,
                Some(authority),
            )?;
            publish_data_abort_terminal_exact(actor, control, cap, classified)
        }
        SnapshotAbortDestination::Autocommit => {
            let seal = {
                let _owner = control.terminal_owner_gate.lock();
                if owner(control.terminal.load(Acquire)) != OWNER_COMPLETE
                    || actor.active_sql.lock().is_some()
                    || authority.command_sequence
                        != control.current_terminal_sequence.load(Acquire)
                {
                    return Err(ActorError::CancellationProtocolMismatch);
                }
                *control.terminal_attempt.lock() = Some(
                    TerminalAttempt::AutocommitSnapshotAbort(
                        classified.clone(),
                    ),
                );
                SnapshotAbortEndSeal::mint_under_owner_gate(
                    control.id, &ended, &authority,
                )
            };
            publish_autocommit_terminal_exact(
                actor,
                control,
                AutocommitTerminalConclusion::SnapshotAbortClassified {
                    ended,
                    authority,
                    error: classified,
                    seal,
                },
            )
        }
    }
}

// Fork B is encoded here: after transaction end, the authority statement runs
// only through the platform-role method. It is sequenced after the end proof
// and its context is installed before the target can become visible.
fn continue_snapshot_authority_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    ended: CompletedSqlTargetProof,
    snapshot: SnapshotAbortContext,
) -> Result<(), ActorError> {
    let sequence = mint_never_reused_command_sequence();
    retarget_terminal_sequence_exact(
        actor, control, ended.command_sequence, sequence,
    )?;
    let context = PostRollbackAuthorityContext {
        ended,
        snapshot,
    };
    {
        let _owner = control.terminal_owner_gate.lock();
        if owner(control.terminal.load(Acquire)) != OWNER_COMPLETE
            || actor.active_sql.lock().is_some()
            || control.postrollback_authority_context.lock().is_some()
        {
            return Err(ActorError::CancellationProtocolMismatch);
        }
        *control.postrollback_authority_context.lock() = Some(context.clone());
    }
    let run = run_statement_exact(
        actor,
        control,
        &actor.op_conn,
        ConnectionLane::Op,
        sequence,
        sequence,
        SqlStatementClass::PostRollbackAuthority,
        || actor.op_conn.read_authority_platform_role(control.id.app),
    )?;
    let Some((raw, authority)) =
        consume_statement_run_exact(actor, control, run)?
    else {
        debug_assert!(control.postrollback_authority_context.lock().is_none()
            || control.generation_fenced.load(Acquire));
        return Ok(());
    };
    let context = control.postrollback_authority_context.lock().take()
        .ok_or(ActorError::CancellationProtocolMismatch)?;
    let classified = match raw {
        Ok(observation) => match classify_authority(
            control.id.app,
            control.resolved_epoch,
            observation,
        ) {
            AuthorityDisposition::Deny(reason) => denial_db_error(reason),
            AuthorityDisposition::ReResolve => schema_snapshot_stale(),
            AuthorityDisposition::Current => serialization_conflict(),
        },
        Err(raw) =>
            actor.authority_unavailable_after_proved_rollback(raw),
    };
    publish_snapshot_abort_classification_exact(
        actor, control, context, authority, classified,
    )
}

// Shared by explicit DataAbort and autocommit. OWNER_COMPLETE already won.
// A non-auto-ended 517 uses FailureCleanupRollback, never cancellation's
// CleanupRollback; its raw result goes through the same generic consumer.
fn finish_busy_snapshot_after_claim_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    snapshot: SnapshotAbortContext,
) -> Result<(), ActorError> {
    let lane = match &snapshot.destination {
        SnapshotAbortDestination::Explicit { .. } => ConnectionLane::Tx,
        SnapshotAbortDestination::Autocommit => ConnectionLane::Op,
    };
    let conn = actor.connection(lane);
    if conn.is_busy() {
        let error = snapshot_cleanup_indeterminate(
            snapshot.raw_mapped.clone(), None,
        );
        return publish_snapshot_cleanup_indeterminate_exact(
            actor,
            control,
            snapshot,
            control.current_terminal_sequence.load(Acquire),
            error,
        );
    }
    if conn.is_autocommit() {
        return continue_snapshot_authority_exact(
            actor,
            control,
            snapshot.data_completed.clone(),
            snapshot,
        );
    }
    match execute_failure_cleanup_rollback_exact(
        actor,
        control,
        lane,
        FailureCleanupContext::SnapshotAbort(snapshot),
    )? {
        FailureCleanupRun::Confirmed {
            completed,
            context: FailureCleanupContext::SnapshotAbort(snapshot),
        } => continue_snapshot_authority_exact(
            actor, control, completed, snapshot,
        ),
        FailureCleanupRun::Indeterminate {
            sequence,
            context: FailureCleanupContext::SnapshotAbort(snapshot),
            error,
        } => publish_snapshot_cleanup_indeterminate_exact(
            actor, control, snapshot, sequence, error,
        ),
        FailureCleanupRun::Terminalized => Ok(()),
        _ => actor.protocol_fault_and_fence_with_source(
            control, failure_cleanup_context_mismatch(),
        ),
    }
}

fn publish_failure_cleanup_indeterminate_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    context: FailureCleanupContext,
    sequence: u64,
    error: DbError,
) -> Result<(), ActorError> {
    match context {
        FailureCleanupContext::ExplicitCommit { .. } => {
            let end = mint_uncertain_end_capability_exact(
                actor,
                control,
                TerminalPublicationClass::Root,
                sequence,
            )?;
            publish_root_terminal_exact(
                actor,
                control,
                TerminalEndCapability::Uncertain(end),
                RootFinishResult::Failed {
                    error,
                    certainty: FinishCertainty::Indeterminate,
                },
            )
        }
        FailureCleanupContext::AutocommitCommit { .. } =>
            publish_autocommit_terminal_exact(
                actor,
                control,
                AutocommitTerminalConclusion::CommitIndeterminate {
                    sequence,
                    error,
                },
            ),
        FailureCleanupContext::AutocommitOperation { .. } =>
            publish_autocommit_terminal_exact(
                actor,
                control,
                AutocommitTerminalConclusion::CleanupIndeterminate {
                    sequence,
                    error,
                },
            ),
        FailureCleanupContext::SnapshotAbort(snapshot) =>
            publish_snapshot_cleanup_indeterminate_exact(
                actor, control, snapshot, sequence, error,
            ),
    }
}

fn finish_failure_cleanup_observation_exact(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    completed: CompletedSqlTargetProof,
    context: FailureCleanupContext,
    diagnostic: Option<DbError>,
) -> Result<(), ActorError> {
    if let Some(error) = diagnostic.clone() {
        actor.record_terminal_cleanup_diagnostic(
            control.id, completed.clone(), error,
        );
    }
    let lane = match &context {
        FailureCleanupContext::ExplicitCommit { .. }
        | FailureCleanupContext::SnapshotAbort(SnapshotAbortContext {
            destination: SnapshotAbortDestination::Explicit { .. }, ..
        }) => ConnectionLane::Tx,
        _ => ConnectionLane::Op,
    };
    let conn = actor.connection(lane);
    if conn.is_busy() || !conn.is_autocommit() {
        actor.quarantine_generation_before_wake(control.actor_generation);
        let sequence = completed.command_sequence;
        let error = failure_cleanup_error_exact(&context, diagnostic);
        return publish_failure_cleanup_indeterminate_exact(
            actor, control, context, sequence, error,
        );
    }
    match context {
        FailureCleanupContext::ExplicitCommit {
            terminal, commit_error,
        } => {
            let cap = mint_root_end_capability_exact(
                actor,
                control,
                RootDecision::Commit,
                terminal,
                Some(completed),
            )?;
            publish_root_terminal_exact(
                actor,
                control,
                TerminalEndCapability::Root(cap),
                RootFinishResult::Failed {
                    error: commit_error,
                    certainty: FinishCertainty::DefinitelyNotCommitted,
                },
            )
        }
        FailureCleanupContext::AutocommitCommit {
            terminal, commit_error,
        } => publish_autocommit_terminal_exact(
            actor,
            control,
            AutocommitTerminalConclusion::CommitDefinitelyFailed {
                terminal,
                cleanup: completed,
                error: commit_error,
            },
        ),
        FailureCleanupContext::AutocommitOperation { original_error } =>
            publish_autocommit_terminal_exact(
                actor,
                control,
                AutocommitTerminalConclusion::OperationFailedCleanupConfirmed {
                    completed,
                    error: original_error,
                },
            ),
        FailureCleanupContext::SnapshotAbort(snapshot) =>
            continue_snapshot_authority_exact(
                actor, control, completed, snapshot,
            ),
    }
}

impl AppActor {
    fn finish_interrupted_commit(
        &self,
        control: &Arc<ReservationControl>,
        completed: CompletedSqlTargetProof,
        raw: RusqliteError,
    ) -> Result<(), ActorError> {
        debug_assert_eq!(sqlite_extended_code(&raw), Some(SQLITE_INTERRUPT));
        match control.kind {
            ReservationKind::Transaction => {
                let Some((capability, result)) = classify_root_finish_exact(
                    self,
                    control,
                    RootDecision::Commit,
                    completed,
                    Err(raw),
                )? else { return Ok(()); };
                publish_root_terminal_exact(self, control, capability, result)
            }
            ReservationKind::Autocommit => {
                let value = match control.terminal_attempt.lock().as_ref() {
                    Some(TerminalAttempt::AutocommitSuccess(value)) =>
                        value.clone(),
                    _ => return Err(ActorError::CancellationProtocolMismatch),
                };
                let Some(conclusion) = classify_autocommit_commit_exact(
                    self, control, value, completed, Err(raw),
                )? else { return Ok(()); };
                publish_autocommit_terminal_exact(self, control, conclusion)
            }
        }
    }

    fn finish_interrupted_explicit_rollback(
        &self,
        control: &Arc<ReservationControl>,
        completed: CompletedSqlTargetProof,
        raw: RusqliteError,
    ) -> Result<(), ActorError> {
        if control.kind != ReservationKind::Transaction {
            return Err(ActorError::CancellationProtocolMismatch);
        }
        let Some((capability, result)) = classify_root_finish_exact(
            self,
            control,
            RootDecision::Rollback,
            completed,
            Err(raw),
        )? else { return Ok(()); };
        publish_root_terminal_exact(self, control, capability, result)
    }

    fn finish_interrupted_failure_cleanup(
        &self,
        control: &Arc<ReservationControl>,
        completed: CompletedSqlTargetProof,
        raw: RusqliteError,
    ) -> Result<(), ActorError> {
        debug_assert_eq!(sqlite_extended_code(&raw), Some(SQLITE_INTERRUPT));
        let context = control.failure_cleanup_context.lock().take()
            .ok_or(ActorError::CancellationProtocolMismatch)?;
        finish_failure_cleanup_observation_exact(
            self,
            control,
            completed,
            context,
            Some(terminal_watchdog_interrupted_db_error(
                SqlStatementClass::FailureCleanupRollback,
            )),
        )
    }

    fn finish_interrupted_cancel_rollback(
        &self,
        control: &Arc<ReservationControl>,
        completed: CompletedSqlTargetProof,
        raw: RusqliteError,
    ) {
        debug_assert_eq!(sqlite_extended_code(&raw), Some(SQLITE_INTERRUPT));
        self.record_cancel_cleanup_diagnostic(
            control.id,
            terminal_watchdog_interrupted_db_error(
                SqlStatementClass::CleanupRollback,
            ),
        );
        let conn = self.connection(completed.lane);
        if conn.is_busy() || !conn.is_autocommit() {
            publish_cancel_cleanup_failure(
                self,
                control,
                cancellation_cleanup_with_engine_error(
                    terminal_watchdog_interrupted_db_error(
                        SqlStatementClass::CleanupRollback,
                    ),
                ),
            );
            return;
        }
        let source = PostFinalizeSource::Statement(completed);
        let end = PostFinalizeAutocommitProof {
            seal: PostFinalizeEndSeal::mint_at_connection_sample(
                &source, false, true,
            ),
            source,
            is_busy_after: false,
            is_autocommit_after: true,
        };
        let (cause, _) = match control.terminal_attempt.lock().as_ref() {
            Some(TerminalAttempt::Cancellation { cause, phase_proof, .. }) =>
                (cause.clone(), *phase_proof),
            _ => {
                publish_cancel_cleanup_failure(
                    self, control, cancellation_protocol_db_error(),
                );
                return;
            }
        };
        match seal_cancel_end_capability(
            self,
            control,
            cause,
            CancelEndFact::SQLiteAlreadyRolledBack(end),
        ).and_then(|cap| publish_clean_cancel_end(self, control, cap)) {
            Ok(()) => {}
            Err(error) => publish_cancel_cleanup_failure(
                self, control, actor_error_as_db_error(error),
            ),
        }
    }

    fn finish_interrupted_postrollback_authority(
        &self,
        control: &Arc<ReservationControl>,
        completed: CompletedSqlTargetProof,
        raw: RusqliteError,
    ) -> Result<(), ActorError> {
        debug_assert_eq!(sqlite_extended_code(&raw), Some(SQLITE_INTERRUPT));
        let context = control.postrollback_authority_context.lock().take()
            .ok_or(ActorError::CancellationProtocolMismatch)?;
        let classified = self.authority_unavailable_after_proved_rollback(raw);
        publish_snapshot_abort_classification_exact(
            self, control, context, completed, classified,
        )
    }
}

fn seal_data_abort_end_capability(
    actor: &AppActor,
    control: &Arc<ReservationControl>,
    permit: Arc<DataDeliveryPermit>,
    completed_end: CompletedSqlTargetProof,
    postrollback_authority: Option<CompletedSqlTargetProof>,
) -> Result<DataAbortEndCapability, ActorError> {
    let _owner = control.terminal_owner_gate.lock();
    let active = actor.active_sql.lock();
    let word = control.terminal.load(Acquire);
    let delivery_id = selected_terminal_delivery_id(control)?;
    if active.is_some()
        || owner(word) != OWNER_COMPLETE
        || !validate_data_abort_chain_parts(
            actor, control, &completed_end,
            postrollback_authority.as_ref(),
        )
    {
        return Err(ActorError::CancellationProtocolMismatch);
    }
    let seal = DataAbortEndSeal::mint_from_exact_chain(
        &completed_end,
        postrollback_authority.as_ref(),
        delivery_id,
        permit.permit_id,
        word,
    );
    Ok(DataAbortEndCapability {
        id: control.id,
        actor_generation: control.actor_generation,
        delivery_id,
        data_permit: permit,
        completed_end,
        postrollback_authority,
        observed_owner_word: word,
        seal,
    })
}
~~~

The helper is invoked for every explicit failure after the mandatory
`is_autocommit` sample. Thus `SQLITE_ABORT_ROLLBACK` with autocommit true becomes
terminal TransactionAborted, while 516 with autocommit false is published only
as HealthImpact::Unknown and forces SC-1 QuarantineUnknown cleanup; it can never
return a reusable ordinary session. An unlisted error cannot fall through after
SQLite already ended the transaction. `SQLITE_BUSY_SNAPSHOT` is the only non-auto-ended ordinary error
that deliberately enters the same terminal helper, because retaining that stale
snapshot would guarantee repeated write-upgrade failure.

An authority-read failure after proved rollback maps to a fail-closed coded
authority-unavailable error, still with `TransactionRolledBack`; it never falls
back to generic lock contention. Actor death while the attempt is Pending uses
the actor-unavailable fallback in the generation table, because no fresh read
has classified raw 517. The autocommit version uses the same Cancel-vs-Complete
CAS and rollback/read classifier, then publishes
`Autocommit(Err(classified_error))` (or CleanupIndeterminate) through its
autocommit cutoff rather than DataCompleted.

With that stateful classifier fixed, primary/extended codes map as follows:

| SQLite code/result | Required context | Typed actor/wire outcome |
| --- | --- | --- |
| SQLITE_OK from COMMIT | Complete owner, matching Commit target, post-sample true | Explicit Committed or Autocommit(Ok(value)); later Cancel replays the exact stored outcome. |
| SQLITE_ROW (100)/DONE (101) then operation success | CANCEL_INTENT won before COMMIT ownership | Discard result, rollback, and return Cancelled with the exact latched CallerDrop/Explicit/IsolateTeardown/Deadline/Detach/AuthorityDenied/EpochChanged cause. SC-1 maps that cause to its corresponding wire result. |
| SQLITE_INTERRUPT (9) | matching cancellable nonterminal target (Prepare/operation Authority, Begin, SnapshotMarker, Data, or FrameControl) and Open+Intent wins Cancel ownership | Cause-specific Cancelled after the target-specific no-transaction or ROLLBACK classifier proves cleanup; never Transient. BeginDidNotOpen is legal only for a Begin target whose post-return autocommit sample is true; SnapshotMarker/Data/FrameControl in an explicit transaction require rollback proof. PostRollbackAuthority cannot enter this arm because it exists only after Complete ownership. |
| SQLITE_INTERRUPT (9) | matching Rollback target with Cancel ownership and a terminal watchdog | Preserve Cancel ownership. is_autocommit true yields Cancelled(SQLiteAlreadyRolledBack); false yields CleanupIndeterminate and quarantine. |
| SQLITE_INTERRUPT (9) | matching Commit target with Complete ownership and a terminal watchdog | Preserve Complete ownership and apply the COMMIT classifier: false then confirmed rollback is definitely CommitFailed; true is CommitIndeterminate; failed cleanup is indeterminate. |
| SQLITE_INTERRUPT (9) | matching Rollback target with Complete ownership and a terminal watchdog | Preserve Complete ownership and apply the ROLLBACK classifier: true confirms the requested rollback or original autocommit error; false is RollbackFailed/CleanupIndeterminate. |
| SQLITE_INTERRUPT (9) | matching PostRollbackAuthority target with Complete ownership and its carried terminal-watchdog generation | Preserve Complete. Return the authority_unavailable_after_proved_rollback classified error through DataAbort/Autocommit terminal delivery; never relabel it cancellation or generic Transient. |
| SQLITE_INTERRUPT (9) | no matching eligible intent/watchdog or foreign/stale target | unexpected_sqlite_interrupt; quarantine that lane generation. It is a protocol fault, never user cancellation. |
| SQLITE_ABORT_ROLLBACK (516) | same reservation has an earlier winning cancellation intent | Same cause-specific public cancellation. Cleanup is SQLiteAlreadyRolledBack only if is_autocommit is true; code 516 alone proves nothing. |
| SQLITE_ABORT_ROLLBACK (516) | no matching winning cancellation | If is_autocommit is false, quarantine: 516 alone proves nothing. If true, explicit Execute publishes TransactionAborted; autocommit preserves the original operation error as Autocommit(Err(original)) after the same end proof. Crossing reservation/generation boundaries is a protocol fault. |
| SQLITE_BUSY_SNAPSHOT (517) | WAL snapshot write upgrade fails; Complete wins the CAS | Run the terminal snapshot-abort algorithm above. Fresh `classify_authority` Deny is terminal denial; ReResolve (Changing or epoch mismatch) is retryable schema_snapshot_stale; Current alone is retryable serialization_conflict. Publish explicit DataAbortCompleted or autocommit terminal outcome only after rollback proof. Code 517 alone proves no schema movement. SC-2 requires the distinct arm (docs/proposals/2026-08-26-sc2-sqlite-actor-protocol.md:234-242). |
| SQLITE_BUSY (5), BUSY_RECOVERY (261), BUSY_TIMEOUT (773) | ordinary operation, cancellation did not own | LockContention/lock_not_available. The present mapper already groups these (crates/zeroship-data-v8/src/backend/sqlite/error.rs:35-38,58-70). If emitted by terminal SQL, use the terminal classifier above instead. |
| SQLITE_LOCKED (6), LOCKED_SHAREDCACHE (262), LOCKED_VTAB (518) | ordinary operation, cancellation did not own | LockContention/lock_not_available; add all three instead of falling through to Transient. The bundled header defines both extended LOCKED codes (/home/ruiyang/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/libsqlite3-sys-0.37.0/sqlite3/sqlite3.h:542-543). If emitted by terminal SQL, use the terminal classifier. |
| SQLITE_CONSTRAINT_CHECK (275), FOREIGNKEY (787), NOTNULL (1299), UNIQUE (2067), PRIMARYKEY (1555), ROWID (2579) | ordinary operation, no matching cancel | Preserve check_violation, fk_violation, or not_null_violation; map UNIQUE, PRIMARYKEY, and ROWID to unique_violation. The current mapper names only the first four constraint subcodes (crates/zeroship-data-v8/src/backend/sqlite/error.rs:40-43,72-115), while the bundled header defines PRIMARYKEY, UNIQUE, and ROWID separately (/home/ruiyang/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/libsqlite3-sys-0.37.0/sqlite3/sqlite3.h:563-572). Autocommit cleanup then stores Autocommit(Err(original)); explicit Execute remains nonterminal unless SQLite ended the transaction. |
| Any other operation failure | CANCEL_INTENT linearized first while owner remained Open | Cancellation wins publicly; retain the engine error as diagnostic metadata, clean/quarantine, and never claim the error caused cancellation. |
| Any other autocommit failure | Complete CAS won first | Never COMMIT. Apply the autocommit-error algorithm and replay Autocommit(Err(original)) or CleanupIndeterminate. |
| Any other nonterminal explicit-Execute failure | per-command completion gate won first | Return the mapped operation error and health impact; if is_autocommit says SQLite ended the transaction, store TransactionAborted, otherwise a later Cancel still rolls back the open reservation. |

## 18. SC-2 model/property tests

1. Every actor-arbitrated terminal trace has exactly one actor CAS changing
   OWNER_MASK from Open and one immutable outcome. Confirmed actor death while
   OWNER_OPEN is the explicit zero-CAS exception: the supervisor first fences
   the generation, then stores ActorUnavailable/BackendActorUnavailable; it
   never pretends the dead actor selected Cancel or Complete.
2. A caller never changes OWNER_MASK. Setting CANCEL_INTENT after Complete
   cannot change outcome or invoke interrupt.
3. Reply send, poll, and Drop events cannot change terminal ownership.
4. No cancellation acknowledgement precedes verified rollback, verified
   autocommit, or connection quarantine.
5. A late Cancel for reservation A never increments the interrupt counter for
   reservation B, including the same app_id under a new incarnation. A target
   from an old connection generation is cleared before that lane is reopened.
6. A cancel-before-Running barrier produces no BEGIN/data SQL, retires the
   reservation, and lets the next queued top-level transaction succeed.
7. A recursive long query cancelled after Running returns the cause-specific
   code, leaves is_autocommit true, and the same lane then executes SELECT 1.
8. A start-gap barrier cancels after phase Running but before sqlite3_step. The
   progress latch must end a long query; removing it makes the test fail.
9. A post-statement/pre-COMMIT barrier makes CANCEL_INTENT win and proves the
   would-be write is absent.
10. A post-COMMIT/pre-reply barrier makes completion win, proves the row durable,
    yields AlreadyCompleted, and records zero rollback/next-reservation
    interrupts.
11. A reply-poll barrier proves the Drop guard is disarmed before Poll::Ready and
    subsequent Drop emits zero Cancel commands.
12. An authenticated SQLITE_INTERRUPT on a cancellable nonterminal target while
    the word is OWNER_OPEN+CANCEL_INTENT maps to cancellation. A matching
    terminal-watchdog interrupt instead preserves OWNER_COMPLETE/OWNER_CANCEL
    and uses its terminal classifier. Synthetic INTERRUPT with neither eligible
    intent nor watchdog maps to DbError::Coded("unexpected_sqlite_interrupt")
    and connection replacement.
13. A live-actor BUSY_SNAPSHOT completion runs the Complete-vs-Cancel CAS,
    forces rollback, and performs a fresh platform-role authority read.
    `classify_authority` Deny
    maps to terminal denial; ReResolve (Changing or epoch mismatch) maps to the
    concrete `DbError::Coded("schema_snapshot_stale")`; Current alone maps to
    `DbError::Coded("serialization_conflict")`. Explicit completion is a keyed
    DataAbortCompleted (whose SC-1 arm supplies TransactionRolledBack semantics);
    cancellation is the only competing
    public outcome. Actor death while classification is Pending is the sole
    exception: physical close proves rollback and the keyed result is
    actor_unavailable, never a guessed schema classification.
14. Detach for AppAuthority A cannot cancel, close, or swap an actor attached as
    B. Matching Detach waits for both connections and all reservations to retire.
15. A PITR-domain change with the same bare token value terminally rejects the
    old Execute/Cancel/Detach handles before SQL or interrupt.
16. Cancellation barriers during op_conn authority SQL and tx_conn data SQL
    each interrupt only the recorded lane/generation/sequence, then clean the
    entire reservation as required.
17. A detach barrier while SQLite is inside a long statement proves the
    incarnation-qualified out-of-band latch interrupts it before the queued
    DetachApp command can be received; detach A records zero interrupts for B.
18. A control-lane saturation test proves every returned reservation owns a
    Cancel permit; the next Reserve fails ActorSaturated rather than returning
    an uncancellable handle. After actor death and generation fencing, Open
    returns ActorUnavailable, Cancel-owned returns CleanupIndeterminate, and
    Complete-owned returns the attempt-specific indeterminate outcome.
19. A fault/cancel CAS model proves one rule for every engine result: intent
    first yields cancellation plus diagnostic fault; actor completion CAS first
    yields AlreadyCompleted with the exact stored Autocommit or transaction
    outcome.
20. Terminal-watchdog barriers interrupt stuck COMMIT, explicit ROLLBACK, and
    cancellation-cleanup ROLLBACK targets without changing OWNER_COMPLETE or
    OWNER_CANCEL; each outcome follows the result/is_autocommit table.
21. Deprovisioned snapshot/platform observations issue no creator data SQL and
    cannot be erased by ForgetTerminal.
22. Two concurrent cancellation causes prove the first cancel-latch acquisition
    supplies both CANCEL_INTENT's visible cause and the explicit Cancel command;
    every waiter observes that one cause.
23. Every autocommit operation error trace contains zero COMMIT commands. It
    stores the exact original error only after autocommit/rollback proof, and a
    cleanup failure stores CleanupIndeterminate instead.
24. For every COMMIT/ROLLBACK raw result x post-is_autocommit pair, generated
    tests hit the corresponding classifier row. In particular, INTERRUPT alone
    never establishes commit or rollback.
25. Kill the hard-stop publisher immediately after Open -> FencePending. The
    supervisor job registry alone closes/joins both lanes, authenticates the
    physical proof, publishes FenceResult, and wakes every waiter. Restart its
    worker at each instruction; no trace remains FencePending forever.
26. Publish and retain a real Result, disarm its actor-owned generation, then
    deliver the old hard-stop callback. It returns ResultWon/Stale, replays the
    retained delivery if necessary, and never activates a protocol fence.
27. Mutate any one of physical proof id, job id, ReservationId/AppAuthority,
    actor generation, full owner byte, class, public kind, cutoff id, or seal.
    The cutoff stays FencePending, no admission/claim is released, and the
    durable worker re-proves closure; the unmodified proof completes exactly
    once.
28. Fail each endpoint pin and dormant-job registration site. Autocommit and
    explicit-cancel Reserve fail before control-index/handle publication;
    explicit Execute and the root transition fail before SQL or timer arm.
    Every already-created unpublished job is cancelled; no orphan remains.
29. Corrupt phase to NoTransactionPossible while tx_conn reports busy or
    !is_autocommit. The no-SQL capability is rejected and cancellation fences;
    it can never acknowledge merely from Queued/Preparing thread-local state.
30. Instrument every SQLite Connection execution entry. CleanupRollback and
    PostRollbackAuthority in the BUSY_SNAPSHOT trace both pass through
    run_statement_exact; bypassing it, deleting the end-to-authority proof
    chain, or letting raw 9 reach map_sqlite_error makes the mutation gate fail.

## 19. Argument against the most consequential choice

The actor-owned CAS is required by this task; the most consequential
discretionary choice is installing a progress callback on every SQLite
statement. The strongest case against it is performance: even with a
1024-opcode cadence it adds branches plus the generation/sequence/generation
atomic loads throughout every long
query, per-command installation/removal adds FFI work, and a removal bug could
leak cancellation into the next reservation. It still cannot preempt time
inside one extension callback that does not return to the SQLite VM. Direct
InterruptHandle plus post-statement/pre-COMMIT arbitration protects durability;
the progress hook backstops an interrupt lost in the marked-Running/before-step
gap once VM execution begins.

I would switch to no progress handler if either condition becomes true:

1. SQLite/rusqlite exposes a documented current-or-next-statement interrupt whose
   no-statement case is not a no-op; or
2. a different handshake has a deterministic test that cancels after Running
   but before sqlite3_step and still interrupts a long read, while the
   interrupt-arm latency bound and a representative SQLite throughput benchmark
   both pass.

I would not switch merely because an average-case benchmark cannot hit the gap:
SQLite explicitly documents that the raw interrupt is a no-op there
(/home/ruiyang/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/libsqlite3-sys-0.37.0/sqlite3/sqlite3.h:2915-2917).

## 20. Acceptance arm that cannot fail on today’s code

SC-1’s acceptance arm says a second same-app top-level begin waits, then
succeeds, with both writes durable
(docs/proposals/2026-08-26-sc1-transaction-protocol.md:296-310). That arm cannot
fail as a discriminator for the missing registry/state-machine work: it already
passes on today’s code. The current implementation explicitly waits on AwaitTxClaim
(crates/zeroship-data-v8/src/transaction/mod.rs:354-378;
crates/zeroship-data-v8/src/transaction/mod.rs:441-473), and the
existing probe starts the pair in one Promise.all
(examples/db-todos/src/index.ts:569-599); the dev-vs-deployed gate invokes it and
asserts both legs and both durable rows
(tests/e2e_dev_vs_deployed_db.sh:438-466;
tests/e2e_dev_vs_deployed_db.sh:1205-1217).

Keep that arm as a preservation property, but it needs a discriminating partner:
inspect the new TxKey/state reducer or mutation-delete the reducer’s admission
guard and prove the test fails. The PostgreSQL cross-isolate non-contention arm
is also discriminating; the bare “waits and succeeds” arm is not.

SC-5 names a second cannot-fail trap explicitly: “two isolates racing their
first DB operation on one thread enter one initialization” is already satisfied
by the naive implementation because both isolates reach the same OS-thread
local, not because a thread resource owns a real singleflight
(docs/proposals/2026-08-26-sc5-service-ownership.md:140-158,239-242).
I verified that `ISOLATE_CTX` is declared with `thread_local!` while its doc calls
it the per-isolate context
(crates/zeroship-data-v8/src/context.rs:953-957). A passing two-isolate/
one-thread sharing arm therefore cannot fail on today’s code. Its discriminating
companion must use two OS threads and assert exactly one service/cache per
thread-resource key, or mutation-delete the proposed owner/singleflight and
prove the arm turns red.

SC-3 supplies another arm that cannot reliably fail: “the same plan shape
renders byte-identically twice” (docs/proposals/2026-08-26-sc3-dbplan-ir-and-ledger.md:629-631).
Rendering the same in-memory unordered map twice can preserve that instance's
iteration order, so a non-canonical implementation may pass; fresh randomized
maps make the arm probabilistic rather than discriminating. Replace it with two
semantically identical plans built using opposite insertion permutations and
assert both against one canonical SQL/parameter fixture, plus mutation-delete
the canonical sort and prove the gate turns red.
