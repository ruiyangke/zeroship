//! The SC-1 transaction reducer.
//!
//! `docs/proposals/2026-08-26-sc1-transaction-protocol.md` states the contract;
//! this is the state machine it becomes. The reducer is **pure**: it owns no
//! session, no client, no timer and no future. It takes an event and returns
//! the actions a driver must perform, which is what makes SC-1's invariants
//! checkable without a database.
//!
//! ## The single most important property
//!
//! **Forced cleanup does not route through [`crate::transaction::reducer::TxState::Poisoned`].** Invariant
//! 13 makes `Poisoned` recoverable by construction - a successful
//! `ROLLBACK TO` of the recovery child returns the parent to `Idle`. So a
//! transaction parked in `Poisoned` by an expired deadline or by a
//! `Deny(AuthorityDomainMismatch)` verdict could be walked back to `Idle` by
//! the creator's next `rollbackTo` and resume issuing data SQL - under an
//! authority the classifier terminally denied, past a deadline that already
//! fired. That is a privilege defect.
//!
//! Every force therefore enters [`crate::transaction::reducer::TxState::Cancelling`], which is terminal by
//! invariant 16: no path leads back to any state that can issue creator data
//! SQL, exactly one cleanup cause is ever latched, and every exit is
//! `Settled`. `a_force_never_routes_through_poisoned` and
//! `a_forced_transaction_cannot_be_resurrected_by_rollback_to` are the arms
//! that fail if anyone reintroduces the route.
//!
//! ## What this module deliberately does not build
//!
//! No supervisor, no `FenceJobRegistry`, no durable fence jobs, no
//! `Quarantining` state. SC-1 declines all of it: terminal delivery is an
//! in-memory gate, and the answer to unknown backend health is
//! [`crate::transaction::reducer::SessionOwnership::Withdrawn`] - the physical connection is destroyed
//! rather than returned, which needs no supervisor because closing the
//! connection *is* the proof that nothing further can run on it.

pub mod deadline;
pub mod frames;
pub mod identity;

use std::time::{Duration, Instant};

use deadline::{
    DeadlineGeneration, DeadlineGenerations, DeadlineKind, DeadlineSlot, ScheduleTimer,
};
use frames::{Effect, FrameClose, FrameError, FrameId, FrameStack};
use identity::{DenyReason, ExpectedAuthority, MaskCeiling, ObservedAuthority, Verdict, classify};

/// The nine states.
///
/// `Poisoned`, `Preparing`, `Quiescing` and `Cancelling` are why five labels
/// were not enough. Each names a condition a shorter list has to fake somewhere
/// else: as a bookkeeping flag beside the state, as an early `Starting` that
/// has already issued `BEGIN` on an authority nobody checked, as an empty
/// client slot, or as a `Settling` that never issued the SQL its own definition
/// promises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TxState {
    /// Admission is granted, the RAII claim guard is held and the execution
    /// deadline is armed, and the platform-role authority read is in flight.
    /// **No `BEGIN` has been sent.**
    Preparing,
    /// The authority read returned `Current`; `BEGIN` and session setup are in
    /// flight, and no operation can see the session yet.
    Starting,
    /// Transaction open, no operation owns the client.
    Idle,
    /// An operation owns the client. Settlement may arrive here, and moves the
    /// transaction to `Quiescing` rather than to `Settling`.
    InFlight,
    /// A settlement was requested while a command still owns logical
    /// execution. The intent is latched, and **no frame or terminal SQL is
    /// issued until the active command returns**.
    ///
    /// Distinct from `Settling` because `Settling` *promises terminal SQL has
    /// already been issued*, and this promises it has not.
    Quiescing,
    /// A statement errored. PostgreSQL refuses every further *data* statement
    /// until the transaction ends, so this is a real server-side state, not a
    /// bookkeeping flag.
    ///
    /// **Recoverable by construction** (invariant 13), which is exactly why no
    /// forced cleanup may route through it.
    Poisoned,
    /// The transaction is being ended by a route that is not terminal SQL. The
    /// single cleanup cause is latched and a cleanup goal is fixed from the
    /// state that was interrupted. No creator request is accepted here and no
    /// creator data SQL is ever issued again.
    Cancelling,
    /// Terminal SQL has been issued and not yet answered.
    Settling,
    /// Terminal, with a recorded outcome.
    Settled,
}

impl TxState {
    /// May a creator's data SQL, frame open or frame close start from here?
    ///
    /// `Poisoned` is false by invariant 13's first sentence; it is `Idle`
    /// alone that admits creator work, because `InFlight` already has an owner
    /// (invariant 6).
    #[must_use]
    pub const fn admits_creator_sql(self) -> bool {
        matches!(self, Self::Idle)
    }

    /// Is this a state a force can claim? Every state before terminal SQL is
    /// issued, which is the six SC-1 names.
    ///
    /// `Settling` is deliberately absent: terminal SQL has been issued exactly
    /// once and the backend owes an answer, so a force arriving now would be
    /// starting a competing cleanup on a session that is already ending.
    #[must_use]
    pub const fn is_forceable(self) -> bool {
        matches!(
            self,
            Self::Preparing
                | Self::Starting
                | Self::Idle
                | Self::InFlight
                | Self::Quiescing
                | Self::Poisoned
        )
    }

    /// Every forceable state, for an arm that rules on the whole set rather
    /// than on the states it remembered.
    pub const FORCEABLE: [Self; 6] = [
        Self::Preparing,
        Self::Starting,
        Self::Idle,
        Self::InFlight,
        Self::Quiescing,
        Self::Poisoned,
    ];
}

/// Where cleanup must get to, fixed from the state the force interrupted.
///
/// Read **at entry to `Cancelling`**, not at the moment the force was
/// published: when an ordinary completion won the gate, the reducer processes
/// that result first and the interrupted state is the one that results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupGoal {
    /// `BEGIN` was never sent. Proved by the session reporting no open
    /// transaction.
    NoTransaction,
    /// `BEGIN` may or may not have opened, or backend health is unknown.
    /// Proved by either "no open transaction" or "rolled back".
    ///
    /// **This absorbs the unknown-health case**, which is why there is no
    /// fourth goal: the artifact's `QuarantineUnknown` says "we do not know
    /// whether a transaction is open", which is what this already says.
    AbortIfOpened,
    /// `BEGIN` was confirmed. Proved by "rolled back".
    OpenTransaction,
}

impl CleanupGoal {
    /// The goal the interrupted state fixes.
    ///
    /// The `Starting` arm and the not-forceable arm below return the same
    /// value for entirely different reasons - one is a real disposition, the
    /// other is a defensive default for states `force` never reaches - so they
    /// stay separate.
    #[must_use]
    #[allow(
        clippy::match_same_arms,
        reason = "merging the real Starting disposition with the unreachable \
                  defensive arm would claim they are the same case"
    )]
    pub const fn for_state(state: TxState) -> Self {
        match state {
            TxState::Preparing => Self::NoTransaction,
            TxState::Starting => Self::AbortIfOpened,
            TxState::Idle | TxState::InFlight | TxState::Quiescing | TxState::Poisoned => {
                Self::OpenTransaction
            }
            // Not forceable; `force` never reaches here.
            TxState::Cancelling | TxState::Settling | TxState::Settled => Self::AbortIfOpened,
        }
    }

    /// Does `ack` prove this goal?
    #[must_use]
    pub fn is_proved_by(self, ack: CleanupAck) -> bool {
        match self {
            Self::NoTransaction => ack == CleanupAck::NoOpenTransaction,
            Self::AbortIfOpened => {
                matches!(ack, CleanupAck::NoOpenTransaction | CleanupAck::RolledBack)
            }
            Self::OpenTransaction => ack == CleanupAck::RolledBack,
        }
    }
}

/// What the backend's health oracle said after forced cleanup.
///
/// Both backends expose it and it is the same oracle the terminal classifier
/// uses: PostgreSQL's `transaction_status()`, whose `None` means
/// *indeterminate* and is documented as such
/// (`libs/compio-postgres/src/client.rs:3170-3188`), and SQLite's
/// `is_autocommit` sample.
///
/// Defined in data-core beside [`SettleIntent`] and [`TerminalResult`], for the
/// same reason: a backend that had to name a reducer type to answer a reducer
/// question would depend upward on the protocol it serves.
pub use zeroship_data_orm::error::CleanupAck;

/// Why a transaction is being force-ended.
///
/// **Exactly one is ever latched**, so the reason a transaction ended is
/// deterministic rather than last-writer-wins - which matters because that
/// cause is what the creator is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupCause {
    /// An explicit `Cancel`, or a caller-drop cancel.
    Cancelled,
    /// A deadline of this kind fired.
    DeadlineExpired(DeadlineKind),
    /// An exact `DetachRequested`.
    Detached,
    /// The classifier ruled the observation `ReResolve`.
    EpochChanged,
    /// The classifier ruled the observation `Deny`.
    Denied(DenyReason),
    /// Session setup produced a creator-facing classification that must be
    /// preserved from the driver's exact error detail.
    SessionSetupFailed,
    /// `BEGIN` failed outright.
    BeginFailed,
    /// The reducer itself discovered backend health to be unknown.
    BackendHealthUnknown,
}

impl CleanupCause {
    /// The creator-visible code for this cause.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Cancelled => "transaction_cancelled",
            Self::DeadlineExpired(_) => "transaction_deadline_expired",
            Self::Detached => "transaction_detached",
            Self::EpochChanged => "SCHEMA_EPOCH_CHANGED",
            Self::Denied(reason) => reason.code(),
            Self::SessionSetupFailed => "session_setup_failed",
            Self::BeginFailed => "begin_failed",
            Self::BackendHealthUnknown => "transaction_health_unknown",
        }
    }

    /// Only `EpochChanged` is retryable; every denial and every deadline is
    /// terminal.
    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(self, Self::EpochChanged)
    }
}

/// The latched cleanup: one cause, one goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatchedCleanup {
    pub cause: CleanupCause,
    pub goal: CleanupGoal,
}

/// Who owns the session between confirmed `BEGIN` and terminal cleanup.
/// Invariant 4: exactly one of three, **never silently absent**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOwnership {
    /// No session yet, or it has been finally disposed of.
    None,
    /// The registry holds it.
    Registry,
    /// The matching command token holds it.
    Command(CommandToken),
    /// **Not a transaction state.** The session is never returned to the
    /// registry, never leased to another command, and its physical connection
    /// is destroyed at terminal cleanup rather than reused.
    ///
    /// This is the whole of SC-1's answer to unknown backend health. It
    /// requires no supervisor, no signed retirement proof and no
    /// `Quarantining` state, because closing the connection *is* the proof
    /// that nothing further can run on it.
    Withdrawn,
}

/// A token minted per backend command. Guard order step 2: every completion
/// must carry the token its current action minted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommandToken(u64);

impl CommandToken {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The backend generation the admitted handle stored. Guard order step 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BackendGeneration(pub u64);

/// The settle vocabulary, which this module USES but does not OWN.
///
/// `SettleIntent` and `TerminalResult` moved to `zeroship-data-core` on
/// 2026-09-02. They are the two types that cross the backend seam - the intent
/// goes down to a lane, the result comes back - so a lane must be able to name
/// them without depending on the protocol that interprets them. Leaving them
/// here would make every vendor backend reach UP into the reducer, which is the
/// inversion `DenyReason` was moved down to fix (#103): the engine keeps the
/// logic, the core keeps the vocabulary.
///
/// Re-exported rather than re-pathed at ~40 call sites, and that is not a
/// compatibility shim: `reducer::SettleIntent` is the name the protocol reads
/// in, and it is the same type.
pub use zeroship_data_orm::error::{SettleIntent, TerminalResult};

/// How a transaction ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalOutcome {
    /// Root intent was commit AND the root finish result was committed. The
    /// only outcome that publishes.
    Committed,
    /// Rolled back, by the creator's intent or by the backend.
    RolledBack,
    /// Forced cleanup completed and proved its goal.
    Cancelled(CleanupCause),
    /// The backend never proved the outcome. The session is withdrawn.
    Indeterminate(CleanupCause),
    /// A terminal result that contradicts the intent - a `Committed` answer to
    /// a `Rollback` request. **Publishes nothing.**
    ResultMismatch,
}

impl TerminalOutcome {
    /// Only a confirmed commit publishes.
    #[must_use]
    pub const fn publishes(self) -> bool {
        matches!(self, Self::Committed)
    }
}

/// The routed authority an event carries, for guard order step 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventAuthority {
    pub identity: identity::AuthorityIdentity,
    pub domain: identity::AuthorityDomain,
}

/// The complete result of acquiring a session, issuing `BEGIN`, and applying
/// its per-app setup.
///
/// Keeping the generation inside `Opened` makes success impossible to express
/// without its identity. The failure variants keep semantic classification on
/// the event instead of collapsing it to a boolean while the exact
/// `DbError` stays with the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeginOutcome {
    /// `BEGIN` and session setup both succeeded.
    Opened(BackendGeneration),
    /// Setup produced a creator-facing error that should survive unchanged.
    SetupFailed,
    /// Setup says this attempt must re-resolve its authority.
    ReResolve,
    /// Setup produced a specific terminal denial.
    Denied(DenyReason),
    /// Acquisition, `BEGIN`, or unclassified setup failed.
    Failed,
}

/// Every event the reducer accepts. Closed.
#[derive(Debug, Clone)]
pub enum TxEvent {
    /// An authority observation, from any of the three sources: the read
    /// `Preparing` waits on, the read an operation takes before its own data
    /// SQL, or an unsolicited publisher observation.
    ///
    /// **The reducer re-runs the classifier on the observation itself** rather
    /// than trusting any verdict a publisher attached, so labelling a `Deny` as
    /// `Current` buys nothing: the label is an input, never a verdict.
    AuthorityObserved {
        authority: EventAuthority,
        observed: Box<ObservedAuthority>,
    },
    /// `BEGIN` answered.
    BeginCompleted {
        token: CommandToken,
        outcome: BeginOutcome,
    },
    /// A creator data statement is starting. Takes the session.
    OperationRequested,
    /// A creator data statement answered.
    OperationCompleted { token: CommandToken, errored: bool },
    /// Open a child frame. Takes no caller-supplied id: the registry mints a
    /// never-reused frame id and savepoint name from its own sequence.
    OpenFrame,
    /// `SAVEPOINT` answered.
    OpenFrameCompleted {
        token: CommandToken,
        frame: FrameId,
        ok: bool,
    },
    /// Close a child frame.
    CloseFrame { frame: FrameId, close: FrameClose },
    /// `ROLLBACK TO` or `RELEASE` answered.
    CloseFrameCompleted {
        token: CommandToken,
        frame: FrameId,
        close: FrameClose,
        ok: bool,
    },
    /// The creator's callback settled.
    SettleRequested { intent: SettleIntent },
    /// Terminal SQL answered.
    TerminalCompleted {
        token: CommandToken,
        result: TerminalResult,
    },
    /// An explicit cancel, or a caller-drop cancel. A forcing publisher.
    Cancel { authority: EventAuthority },
    /// An exact detach. A forcing publisher; the expected authority must equal
    /// the stored one, and a mismatch interrupts nothing.
    DetachRequested { authority: EventAuthority },
    /// A timer expired. A forcing publisher when it is the `Execution` kind.
    DeadlineFired {
        kind: DeadlineKind,
        generation: DeadlineGeneration,
    },
    /// Forced cleanup answered, carrying what the health oracle said.
    CancellationAcknowledged {
        token: CommandToken,
        ack: CleanupAck,
    },
}

/// What the driver must do. The reducer performs no I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Spawn a timer task carrying only these fields plus the transaction key
    /// and the event sender - no session, no client, no settle future.
    ScheduleTimer(ScheduleTimer),
    /// Send `BEGIN` and session setup.
    IssueBegin { token: CommandToken },
    /// Send a creator data statement.
    IssueDataSql { token: CommandToken },
    /// Send `SAVEPOINT <name>`.
    IssueSavepoint { token: CommandToken, name: Box<str> },
    /// Send `ROLLBACK TO SAVEPOINT <name>`.
    IssueRollbackTo { token: CommandToken, name: Box<str> },
    /// Send `RELEASE SAVEPOINT <name>`.
    IssueRelease { token: CommandToken, name: Box<str> },
    /// Send terminal SQL.
    IssueTerminal {
        token: CommandToken,
        intent: SettleIntent,
    },
    /// Delegate cleanup to the backend as a cancellation.
    IssueCancellation {
        token: CommandToken,
        goal: CleanupGoal,
    },
    /// Destroy the physical connection rather than returning it, so no later
    /// user can inherit it.
    WithdrawSession,
    /// Return the session to the pool.
    ReleaseSession,
    /// Release the admission claim, freeing the connection slot.
    ReleaseAdmission,
    /// Publish every retained effect exactly once, in order.
    PublishEffects(Vec<Effect>),
    /// Drop every frame buffer without firing any events.
    DiscardEffects,
    /// Answer the caller.
    Reply(Result<TxReply, TxProtocolError>),
}

impl Action {
    /// Would this action put creator data SQL on the wire?
    ///
    /// Invariant 16 asserts this is false for every action emitted at or after
    /// entry to [`TxState::Cancelling`].
    #[must_use]
    pub const fn is_creator_data_sql(&self) -> bool {
        matches!(
            self,
            Self::IssueDataSql { .. }
                | Self::IssueSavepoint { .. }
                | Self::IssueRollbackTo { .. }
                | Self::IssueRelease { .. }
                | Self::IssueBegin { .. }
        )
    }
}

/// A successful reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxReply {
    Began,
    FrameOpened(FrameId),
    FrameClosed(FrameId),
    OperationAccepted,
    Settled(TerminalOutcome),
}

/// Every rejection.
///
/// Reply channels carry `Result<_, TxProtocolError>`, so a rejection is a value
/// the caller receives - never an out-of-band log line. A protocol whose
/// illegal transitions are only observable in a worker log is not executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxProtocolError {
    /// Guard 1. The event's authority does not match the stored one. **Must
    /// not touch that entry's session, actor, timer or admission** - not even
    /// to cancel it.
    AppIncarnationMismatch,
    /// Guard 2. A completion carrying a token its current action did not mint.
    StaleTransactionCompletion,
    /// Guard 3. A deadline delivery that did not win the atomic claim.
    StaleTransactionDeadline,
    /// Guard 4. A completion naming a stale backend generation.
    StaleBackendGeneration,
    /// Guard 5. The state cannot legally process this event at all.
    TransactionNotReady,
    /// Invariant 6. A second operation while one is in flight.
    TransactionConnectionBusy,
    /// The transaction already reached `Settled`.
    TransactionAlreadySettled,
    /// A second settle naming a different attempt.
    SettleConflict,
    /// Guard 6. A frame guard refused.
    Frame(FrameError),
    /// The classifier ruled `ReResolve`. Retryable; the caller re-resolves to
    /// a fresh key.
    EpochChanged,
    /// The classifier ruled `Deny`. Terminal, carrying the *specific* denial.
    Denied(DenyReason),
    /// The transaction is ending under a latched cause, and this request
    /// arrived too late to change that.
    Cleanup(CleanupCause),
}

impl TxProtocolError {
    /// The creator-visible code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::AppIncarnationMismatch => "app_incarnation_mismatch",
            Self::StaleTransactionCompletion => "stale_transaction_completion",
            Self::StaleTransactionDeadline => "stale_transaction_deadline",
            Self::StaleBackendGeneration => "stale_backend_generation",
            Self::TransactionNotReady => "transaction_not_ready",
            Self::TransactionConnectionBusy => "transaction_connection_busy",
            Self::TransactionAlreadySettled => "transaction_already_settled",
            Self::SettleConflict => "settle_conflict",
            Self::Frame(error) => error.code(),
            Self::EpochChanged => "SCHEMA_EPOCH_CHANGED",
            Self::Denied(reason) => reason.code(),
            Self::Cleanup(cause) => cause.code(),
        }
    }
}

/// The time budgets each deadline kind enforces.
#[derive(Debug, Clone, Copy)]
pub struct TxBudgets {
    /// Bounds admission-to-terminal: everything a caller can see.
    pub execution: Duration,
    /// Bounds forced cleanup, from the force winning the gate to the
    /// acknowledgement.
    pub cancellation_sql: Duration,
    /// Bounds terminal SQL, from issue to answer.
    pub terminal_sql: Duration,
}

impl Default for TxBudgets {
    fn default() -> Self {
        Self {
            execution: Duration::from_secs(30),
            cancellation_sql: Duration::from_secs(5),
            terminal_sql: Duration::from_secs(10),
        }
    }
}

/// One transaction's reducer.
#[derive(Debug)]
pub struct TxReducer {
    state: TxState,
    expected: ExpectedAuthority,
    budgets: TxBudgets,

    /// The ceiling captured at `BEGIN`, and the running fold. Invariant 8.
    begin_ceiling: Option<MaskCeiling>,
    effective_ceiling: Option<MaskCeiling>,

    session: SessionOwnership,
    generation: Option<BackendGeneration>,

    frames: FrameStack,
    slot: DeadlineSlot,
    generations: DeadlineGenerations,

    next_token: u64,
    /// The token of the single active backend command. Invariant 5: at most
    /// one.
    active: Option<CommandToken>,
    /// The token forced cleanup is waiting on.
    cancellation_token: Option<CommandToken>,

    /// **Exactly one cleanup cause is ever latched.** `Some` is what makes the
    /// gate's "Joined" outcome deterministic.
    cleanup: Option<LatchedCleanup>,
    /// The intent `Quiescing` latched, or the one `Settling` is running.
    settle_intent: Option<SettleIntent>,
    /// Rule 2 of the gate: once a terminal completion is promised, a later
    /// forcing publisher neither sets a new force nor suppresses the promised
    /// result.
    terminal_promised: bool,
    outcome: Option<TerminalOutcome>,
}

/// The gate's three outcomes. The middle one is the one a hand-rolled
/// implementation gets wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOutcome {
    /// The gate was open; the force latches the single cleanup cause.
    ForceWon,
    /// An earlier force already owns the cause. The second joins it and emits
    /// nothing.
    Joined,
    /// A terminal completion was already promised. Treated as a late cancel
    /// awaiting `AlreadyCompleted`; it does not turn a succeeded commit into a
    /// cancellation.
    LateCancel,
}

impl TxReducer {
    /// Admit a transaction: hold the claim, arm the execution deadline, and
    /// wait for the platform-role authority read.
    ///
    /// The deadline is armed on **this** transition - the same one that grants
    /// admission and creates the claim guard - so queue time does not consume
    /// the transaction's execution budget.
    ///
    /// # Panics
    ///
    /// Never in practice: the only fallible step is arming a slot this call
    /// just constructed, which is `Disarmed` by definition.
    #[must_use]
    pub fn admit(
        expected: ExpectedAuthority,
        budgets: TxBudgets,
        now: Instant,
        max_depth: u32,
    ) -> (Self, Vec<Action>) {
        let mut reducer = Self {
            state: TxState::Preparing,
            expected,
            budgets,
            begin_ceiling: None,
            effective_ceiling: None,
            session: SessionOwnership::None,
            generation: None,
            frames: FrameStack::new(max_depth),
            slot: DeadlineSlot::new(),
            generations: DeadlineGenerations::default(),
            next_token: 0,
            active: None,
            cancellation_token: None,
            cleanup: None,
            settle_intent: None,
            terminal_promised: false,
            outcome: None,
        };
        let generation = reducer.generations.mint();
        let scheduled = reducer
            .slot
            .arm_initial(
                DeadlineKind::Execution,
                generation,
                now + reducer.budgets.execution,
            )
            .expect("a fresh slot is Disarmed");
        (reducer, vec![Action::ScheduleTimer(scheduled)])
    }

    #[must_use]
    pub const fn state(&self) -> TxState {
        self.state
    }

    /// The authority this transaction was admitted under.
    ///
    /// For the driver, which has to stamp it onto every forcing publisher's
    /// event so guard order step 1 has something to compare. Read-only: the
    /// expectation is fixed at admission and an entry never follows a new one
    /// in place - a changed authority re-resolves to a fresh handle.
    #[must_use]
    pub const fn expected(&self) -> &ExpectedAuthority {
        &self.expected
    }

    #[must_use]
    pub const fn session(&self) -> SessionOwnership {
        self.session
    }

    #[must_use]
    pub const fn cleanup(&self) -> Option<LatchedCleanup> {
        self.cleanup
    }

    /// The token forced cleanup is waiting on, if a force has claimed the gate.
    ///
    /// For a driver whose cleanup must survive an `await`: together with
    /// [`Self::generation`] it names *which* cleanup of *which* session a
    /// resumed future belongs to, so a cleanup that outlived its transaction
    /// cannot act on the next one's session.
    #[must_use]
    pub const fn cancellation_token(&self) -> Option<CommandToken> {
        self.cancellation_token
    }

    /// The backend generation of the session this transaction confirmed a
    /// `BEGIN` on. `None` before `BeginCompleted`.
    ///
    /// Monotonic for the life of the thread and never reset
    /// (`transaction::driver`'s private `next_backend_generation`), which is
    /// what makes it usable as a session identity rather than only as a
    /// staleness counter: a later transaction can never mint a value an earlier
    /// one already held.
    #[must_use]
    pub const fn generation(&self) -> Option<BackendGeneration> {
        self.generation
    }

    #[must_use]
    pub const fn outcome(&self) -> Option<TerminalOutcome> {
        self.outcome
    }

    #[must_use]
    pub const fn frames(&self) -> &FrameStack {
        &self.frames
    }

    #[must_use]
    pub const fn deadline(&self) -> &DeadlineSlot {
        &self.slot
    }

    /// The running ceiling fold. `None` until `BEGIN` captured one.
    #[must_use]
    pub const fn effective_ceiling(&self) -> Option<&MaskCeiling> {
        self.effective_ceiling.as_ref()
    }

    #[must_use]
    pub const fn begin_ceiling(&self) -> Option<&MaskCeiling> {
        self.begin_ceiling.as_ref()
    }

    /// Apply one event, in SC-1's normative guard order.
    pub fn apply(&mut self, event: TxEvent, now: Instant) -> Vec<Action> {
        // --- Guard 1: identity ---
        //
        // A mismatch must not touch that entry's session, actor, timer or
        // admission - not even to cancel it. So this runs before everything,
        // and returns without recording anything.
        if let Some(authority) = event_authority(&event) {
            if authority.identity != self.expected.identity
                || authority.domain != self.expected.domain
            {
                return vec![Action::Reply(Err(TxProtocolError::AppIncarnationMismatch))];
            }
        }

        // --- Guard 5 (partial): a settled transaction accepts nothing ---
        //
        // Only a state of `Settled` ends a request early. An absent client
        // never does, which is rule 1.
        if self.state == TxState::Settled {
            return match event {
                // A late force joins the recorded outcome rather than starting
                // a competing cleanup.
                TxEvent::Cancel { .. }
                | TxEvent::DetachRequested { .. }
                | TxEvent::DeadlineFired { .. } => vec![],
                _ => vec![Action::Reply(Err(
                    TxProtocolError::TransactionAlreadySettled,
                ))],
            };
        }

        match event {
            TxEvent::AuthorityObserved { observed, .. } => self.on_authority(&observed, now),
            TxEvent::BeginCompleted { token, outcome } => {
                self.on_begin_completed(token, outcome, now)
            }
            TxEvent::OperationRequested => self.on_operation_requested(),
            TxEvent::OperationCompleted { token, errored } => {
                self.on_operation_completed(token, errored, now)
            }
            TxEvent::OpenFrame => self.on_open_frame(),
            TxEvent::OpenFrameCompleted { token, frame, ok } => {
                self.on_open_frame_completed(token, frame, ok, now)
            }
            TxEvent::CloseFrame { frame, close } => self.on_close_frame(frame, close),
            TxEvent::CloseFrameCompleted {
                token,
                frame,
                close,
                ok,
            } => self.on_close_frame_completed(token, frame, close, ok, now),
            TxEvent::SettleRequested { intent } => self.on_settle_requested(intent, now),
            TxEvent::TerminalCompleted { token, result } => {
                self.on_terminal_completed(token, result)
            }
            TxEvent::Cancel { .. } => self.force(CleanupCause::Cancelled, now).1,
            TxEvent::DetachRequested { .. } => self.force(CleanupCause::Detached, now).1,
            TxEvent::DeadlineFired { kind, generation } => {
                self.on_deadline_fired(kind, generation, now)
            }
            TxEvent::CancellationAcknowledged { token, ack } => {
                self.on_cancellation_acknowledged(token, ack)
            }
        }
    }

    // -----------------------------------------------------------------
    // The gate. One gate for every forcing publisher, not just caller
    // cancellation.
    // -----------------------------------------------------------------

    /// Claim the gate and enter `Cancelling`.
    ///
    /// **This is the only route a force takes.** It never passes through
    /// `Poisoned` and never through `Settling`; see the module docs for the
    /// three independent reasons, the first of which is a privilege defect.
    fn force(&mut self, cause: CleanupCause, now: Instant) -> (GateOutcome, Vec<Action>) {
        // Rule 2: once a terminal completion is promised, a later forcing
        // publisher neither sets a new force nor suppresses the promised
        // result. A deadline that fires microseconds after a commit succeeded
        // must not turn that commit into a cancellation.
        if self.terminal_promised || self.state == TxState::Settling {
            return (GateOutcome::LateCancel, vec![]);
        }
        // Joined: an earlier force already owns the cause. Exactly one cleanup
        // cause is ever latched, so the reason a transaction ended is
        // deterministic rather than last-writer-wins.
        if self.cleanup.is_some() {
            return (GateOutcome::Joined, vec![]);
        }
        debug_assert!(
            self.state.is_forceable(),
            "force reached a state the gate cannot claim: {:?}",
            self.state
        );

        // The goal is read AT ENTRY, from the state the force interrupted.
        let goal = CleanupGoal::for_state(self.state);
        self.cleanup = Some(LatchedCleanup { cause, goal });
        self.state = TxState::Cancelling;

        let mut actions = Vec::new();

        // Cleanup is delegated to the backend as a cancellation, and the state
        // waits for the acknowledgement that says what the backend actually
        // did. The admission claim is held until that arrives, or until the
        // CancellationSql deadline expires and the session is withdrawn.
        //
        // **`NoTransaction` waits too, and that is deliberate.** It is
        // tempting to settle a `Preparing` force immediately, since `BEGIN` was
        // never sent and the goal looks self-proving. SC-2 case 1 forbids it:
        // the actor "removes the queued operation, retires the reservation and
        // acknowledges `Cancelled(NoSqlStarted)` ... and SC-1 releases its
        // admission only after reducing that proof". Releasing the claim
        // before the backend has retired the reservation is what lets the next
        // transaction be admitted onto a lane the previous one has not left.
        let token = self.mint_token();
        self.cancellation_token = Some(token);
        // Every call site names `Execution` as its expected kind, so a second
        // force arriving in `Cancelling` finds `CancellationSql` current and
        // fails the expectation.
        let next = self.generations.mint();
        if let Ok(scheduled) = self.slot.replace_current(
            DeadlineKind::Execution,
            DeadlineKind::CancellationSql,
            next,
            now + self.budgets.cancellation_sql,
        ) {
            actions.push(Action::ScheduleTimer(scheduled));
        }
        actions.push(Action::IssueCancellation { token, goal });
        (GateOutcome::ForceWon, actions)
    }

    // -----------------------------------------------------------------
    // Event handlers
    // -----------------------------------------------------------------

    fn on_authority(&mut self, observed: &ObservedAuthority, now: Instant) -> Vec<Action> {
        // The reducer re-runs the classifier itself; a publisher cannot smuggle
        // a Deny through by labelling it Current.
        let verdict = classify(observed, &self.expected);
        self.on_verdict(verdict, now)
    }

    /// Apply one already-classified semantic outcome. Authority observations
    /// reach this only after the reducer ran `classify` itself; begin outcomes
    /// reach only the forcing variants because a successful setup is `Opened`.
    fn on_verdict(&mut self, verdict: Verdict, now: Instant) -> Vec<Action> {
        match verdict {
            Verdict::Current { ceiling } => {
                // Not forcing: it never claims the gate. The ceiling folds by
                // meet, so it can only tighten (invariant 8).
                self.effective_ceiling =
                    Some(match (&self.begin_ceiling, &self.effective_ceiling) {
                        (Some(begin), Some(effective)) => begin.meet(effective).meet(&ceiling),
                        _ => ceiling.clone(),
                    });
                match self.state {
                    TxState::Preparing => {
                        // Capture the BEGIN ceiling from this read and proceed.
                        self.begin_ceiling = Some(ceiling);
                        self.state = TxState::Starting;
                        let token = self.mint_token();
                        self.active = Some(token);
                        vec![Action::IssueBegin { token }]
                    }
                    // An operation's pre-SQL read, or an unsolicited Current
                    // observation. Nothing to do; the fold above is the effect.
                    _ => vec![],
                }
            }
            Verdict::ReResolve => {
                let (_, mut actions) = self.force(CleanupCause::EpochChanged, now);
                actions.push(Action::Reply(Err(TxProtocolError::EpochChanged)));
                actions
            }
            Verdict::Deny(reason) => {
                let (_, mut actions) = self.force(CleanupCause::Denied(reason), now);
                actions.push(Action::Reply(Err(TxProtocolError::Denied(reason))));
                actions
            }
        }
    }

    fn on_begin_completed(
        &mut self,
        token: CommandToken,
        outcome: BeginOutcome,
        now: Instant,
    ) -> Vec<Action> {
        if let Some(reply) = self.check_token(token) {
            return reply;
        }
        if self.state != TxState::Starting {
            return vec![Action::Reply(Err(TxProtocolError::TransactionNotReady))];
        }
        self.active = None;
        match outcome {
            BeginOutcome::Opened(generation) => {
                self.generation = Some(generation);
                self.session = SessionOwnership::Registry;
                self.frames.open_root();
                self.state = TxState::Idle;
                vec![Action::Reply(Ok(TxReply::Began))]
            }
            BeginOutcome::SetupFailed => self.force(CleanupCause::SessionSetupFailed, now).1,
            BeginOutcome::ReResolve => self.on_verdict(Verdict::ReResolve, now),
            BeginOutcome::Denied(reason) => self.on_verdict(Verdict::Deny(reason), now),
            BeginOutcome::Failed => {
                // A failed BEGIN reaches `Cancelling` directly. It is found
                // under the reducer lock with no publisher to arbitrate
                // against, so it takes the state without claiming the gate -
                // but it still goes through `force`, which keeps exactly one
                // cause true.
                self.force(CleanupCause::BeginFailed, now).1
            }
        }
    }

    fn on_operation_requested(&mut self) -> Vec<Action> {
        if let Some(reply) = self.refuse_if_cleaning_up() {
            return reply;
        }
        match self.state {
            TxState::Idle => {
                let token = self.mint_token();
                self.active = Some(token);
                self.session = SessionOwnership::Command(token);
                self.state = TxState::InFlight;
                vec![Action::IssueDataSql { token }]
            }
            // Invariant 6: never silently deferred, never silently
            // autocommitted.
            TxState::InFlight | TxState::Quiescing => vec![Action::Reply(Err(
                TxProtocolError::TransactionConnectionBusy,
            ))],
            // Invariant 13: no data command may start from `Poisoned`.
            _ => vec![Action::Reply(Err(TxProtocolError::TransactionNotReady))],
        }
    }

    fn on_operation_completed(
        &mut self,
        token: CommandToken,
        errored: bool,
        now: Instant,
    ) -> Vec<Action> {
        if let Some(reply) = self.check_token(token) {
            return reply;
        }
        if self.state == TxState::Cancelling {
            return self.command_returned_during_cleanup();
        }
        self.active = None;
        self.session = SessionOwnership::Registry;
        let quiescing = self.state == TxState::Quiescing;
        self.state = if errored {
            TxState::Poisoned
        } else {
            TxState::Idle
        };
        let mut actions = vec![Action::Reply(if errored {
            Err(TxProtocolError::TransactionNotReady)
        } else {
            Ok(TxReply::OperationAccepted)
        })];
        if quiescing {
            // Invariant 15: the latched intent starts now, and at most one
            // logical settlement attempt.
            let intent = self
                .settle_intent
                .expect("Quiescing latches exactly one intent");
            actions.extend(self.begin_terminal(intent, now));
        }
        actions
    }

    fn on_open_frame(&mut self) -> Vec<Action> {
        if let Some(reply) = self.refuse_if_cleaning_up() {
            return reply;
        }
        if !self.state.admits_creator_sql() {
            return vec![Action::Reply(Err(match self.state {
                TxState::InFlight | TxState::Quiescing => {
                    TxProtocolError::TransactionConnectionBusy
                }
                _ => TxProtocolError::TransactionNotReady,
            }))];
        }
        match self.frames.open_child() {
            Ok((frame, name)) => {
                let token = self.mint_token();
                self.active = Some(token);
                self.session = SessionOwnership::Command(token);
                self.state = TxState::InFlight;
                let _ = frame;
                vec![Action::IssueSavepoint { token, name }]
            }
            Err(error) => vec![Action::Reply(Err(TxProtocolError::Frame(error)))],
        }
    }

    fn on_open_frame_completed(
        &mut self,
        token: CommandToken,
        frame: FrameId,
        ok: bool,
        now: Instant,
    ) -> Vec<Action> {
        if let Some(reply) = self.check_token(token) {
            return reply;
        }
        if self.state == TxState::Cancelling {
            return self.command_returned_during_cleanup();
        }
        self.active = None;
        self.session = SessionOwnership::Registry;
        if ok {
            let _ = self.frames.confirm_child_open(frame);
            self.state = TxState::Idle;
            vec![Action::Reply(Ok(TxReply::FrameOpened(frame)))]
        } else {
            let _ = self.frames.abandon_opening_child(frame);
            self.state = TxState::Poisoned;
            let _ = now;
            vec![Action::Reply(Err(TxProtocolError::TransactionNotReady))]
        }
    }

    fn on_close_frame(&mut self, frame: FrameId, close: FrameClose) -> Vec<Action> {
        if let Some(reply) = self.refuse_if_cleaning_up() {
            return reply;
        }
        // A close is the ONE creator command legal from `Poisoned`: invariant
        // 13's recovery arc, where a successful rollback-to returns the parent
        // to `Idle`. That arc is exactly why a force must never park here.
        let recovering = self.state == TxState::Poisoned && close == FrameClose::RolledBackTo;
        if !self.state.admits_creator_sql() && !recovering {
            return vec![Action::Reply(Err(match self.state {
                TxState::InFlight | TxState::Quiescing => {
                    TxProtocolError::TransactionConnectionBusy
                }
                _ => TxProtocolError::TransactionNotReady,
            }))];
        }
        let Some(top) = self.frames.top() else {
            return vec![Action::Reply(Err(TxProtocolError::Frame(
                FrameError::NoOpenFrame,
            )))];
        };
        if top.id() != frame {
            return vec![Action::Reply(Err(TxProtocolError::Frame(
                FrameError::SavepointNotCurrent,
            )))];
        }
        if top.is_root() {
            return vec![Action::Reply(Err(TxProtocolError::Frame(
                FrameError::SavepointRootCannotClose,
            )))];
        }
        let name: Box<str> = top
            .savepoint()
            .expect("a non-root frame always has a savepoint name")
            .into();
        let token = self.mint_token();
        self.active = Some(token);
        self.session = SessionOwnership::Command(token);
        self.state = TxState::InFlight;
        vec![match close {
            FrameClose::Released => Action::IssueRelease { token, name },
            FrameClose::RolledBackTo => Action::IssueRollbackTo { token, name },
        }]
    }

    fn on_close_frame_completed(
        &mut self,
        token: CommandToken,
        frame: FrameId,
        close: FrameClose,
        ok: bool,
        now: Instant,
    ) -> Vec<Action> {
        if let Some(reply) = self.check_token(token) {
            return reply;
        }
        if self.state == TxState::Cancelling {
            return self.command_returned_during_cleanup();
        }
        self.active = None;
        self.session = SessionOwnership::Registry;
        if !ok {
            // The legal error row stops WITHOUT `RELEASE` and retains the
            // frame's effects. The disposition is invariant 15's, chosen by the
            // health oracle - which the driver reports as a
            // `CancellationAcknowledged`-shaped health sample. Absent one, the
            // conservative arm is `Poisoned`, which invariant 13 governs.
            self.state = TxState::Poisoned;
            return vec![Action::Reply(Err(TxProtocolError::TransactionNotReady))];
        }
        // The fate is applied AFTER the statement succeeded, never before.
        match close {
            FrameClose::RolledBackTo => {
                let _ = self.frames.mark_rolled_back(frame);
                // A rolled-back frame is not closed until its `RELEASE` lands,
                // so the transaction is NOT `Idle` yet.
                let name: Box<str> = self
                    .frames
                    .top()
                    .and_then(frames::Frame::savepoint)
                    .expect("the rolled-back frame is still on the stack")
                    .into();
                let release = self.mint_token();
                self.active = Some(release);
                self.session = SessionOwnership::Command(release);
                self.state = TxState::InFlight;
                let _ = now;
                vec![Action::IssueRelease {
                    token: release,
                    name,
                }]
            }
            FrameClose::Released => {
                let closed = self.frames.close_top(frame, FrameClose::Released);
                match closed {
                    Ok(id) => {
                        self.state = TxState::Idle;
                        vec![Action::Reply(Ok(TxReply::FrameClosed(id)))]
                    }
                    Err(error) => {
                        // The rolled-back frame's RELEASE. Its effects stay
                        // discarded and the frame leaves the stack.
                        let _ = error;
                        let top = self.frames.top().map(frames::Frame::id);
                        if let Some(id) = top {
                            let _ = self.frames.close_top(id, FrameClose::RolledBackTo);
                            self.state = TxState::Idle;
                            return vec![Action::Reply(Ok(TxReply::FrameClosed(id)))];
                        }
                        vec![Action::Reply(Err(TxProtocolError::Frame(error)))]
                    }
                }
            }
        }
    }

    fn on_settle_requested(&mut self, intent: SettleIntent, now: Instant) -> Vec<Action> {
        if let Some(reply) = self.refuse_if_cleaning_up() {
            return reply;
        }
        match self.state {
            // A poisoned transaction settles by rollback on every backend.
            TxState::Idle | TxState::Poisoned => self.begin_terminal(intent, now),
            // From `InFlight` it goes to `Quiescing` first and waits there for
            // the operation to return the client, rather than treating the
            // empty slot as proof that terminal SQL ran.
            TxState::InFlight => {
                self.state = TxState::Quiescing;
                self.settle_intent = Some(intent);
                // ZERO frame or terminal SQL until the active command returns.
                vec![]
            }
            TxState::Quiescing | TxState::Settling => {
                // Exactly one intent is latched: a second request naming the
                // same attempt joins the waiter already there and issues no
                // second command; a different attempt is a settle conflict.
                if self.settle_intent == Some(intent) {
                    vec![]
                } else {
                    vec![Action::Reply(Err(TxProtocolError::SettleConflict))]
                }
            }
            _ => vec![Action::Reply(Err(TxProtocolError::TransactionNotReady))],
        }
    }

    /// Issue terminal SQL exactly once and enter `Settling`.
    fn begin_terminal(&mut self, intent: SettleIntent, now: Instant) -> Vec<Action> {
        debug_assert!(self.active.is_none(), "invariant 5: one active token");
        let command = if self.state == TxState::Poisoned {
            SettleIntent::Rollback
        } else {
            intent
        };
        self.settle_intent = Some(intent);
        self.state = TxState::Settling;
        let token = self.mint_token();
        self.active = Some(token);
        self.session = SessionOwnership::Command(token);
        let mut actions = Vec::new();
        let next = self.generations.mint();
        if let Ok(scheduled) = self.slot.replace_current(
            DeadlineKind::Execution,
            DeadlineKind::TerminalSql,
            next,
            now + self.budgets.terminal_sql,
        ) {
            actions.push(Action::ScheduleTimer(scheduled));
        }
        actions.push(Action::IssueTerminal {
            token,
            intent: command,
        });
        actions
    }

    fn on_terminal_completed(
        &mut self,
        token: CommandToken,
        result: TerminalResult,
    ) -> Vec<Action> {
        if let Some(reply) = self.check_token(token) {
            return reply;
        }
        if self.state != TxState::Settling {
            return vec![Action::Reply(Err(TxProtocolError::TransactionNotReady))];
        }
        self.active = None;
        self.terminal_promised = true;
        let intent = self
            .settle_intent
            .expect("Settling always carries its intent");

        // Invariant 12: nothing publishes unless the root INTENT was commit AND
        // the root finish result was committed. A `Committed` result against a
        // rollback intent is a terminal-result mismatch, not a commit.
        //
        // The two `RolledBack` arms stay separate deliberately: one is the L8
        // case - a COMMIT the server refused - and the other is an ordinary
        // rollback succeeding. Merging them would detach the L8 comment from
        // the case it explains, and that case is the one that used to report
        // success for writes that never landed.
        #[allow(
            clippy::match_same_arms,
            reason = "the L8 arm and the ordinary rollback arm agree on the \
                      outcome for entirely different reasons"
        )]
        let outcome = match (intent, result) {
            (SettleIntent::Commit, TerminalResult::Committed) => TerminalOutcome::Committed,
            // The L8 case: a COMMIT PostgreSQL answered with the tag ROLLBACK
            // is a FAILED transaction, and it publishes nothing.
            (SettleIntent::Commit, TerminalResult::RolledBack) => TerminalOutcome::RolledBack,
            (SettleIntent::Rollback, TerminalResult::RolledBack) => TerminalOutcome::RolledBack,
            (SettleIntent::Rollback, TerminalResult::Committed) => TerminalOutcome::ResultMismatch,
            (_, TerminalResult::Indeterminate) => {
                TerminalOutcome::Indeterminate(CleanupCause::BackendHealthUnknown)
            }
        };
        let withdraw = matches!(outcome, TerminalOutcome::Indeterminate(_));
        self.slot.disarm();
        self.settle_now(outcome, withdraw)
    }

    fn on_deadline_fired(
        &mut self,
        kind: DeadlineKind,
        generation: DeadlineGeneration,
        now: Instant,
    ) -> Vec<Action> {
        // --- Guard 3: deadline preflight, BEFORE claiming the timer ---
        //
        // A pure state/event capability check runs first, and only a
        // preflight-legal event is allowed to claim the fire. Ordering it this
        // way is what stops an illegal cell from claiming a timer or queueing a
        // forced settlement as a side effect of being rejected.
        let preflight_legal = match kind {
            DeadlineKind::Execution => self.state.is_forceable(),
            DeadlineKind::CancellationSql => self.state == TxState::Cancelling,
            DeadlineKind::TerminalSql => self.state == TxState::Settling,
        };
        if !preflight_legal {
            return vec![];
        }
        // Only now the atomic claim.
        if self.slot.claim_fire(kind, generation).is_err() {
            // A pure diagnostic: no SQL, no reply, no claim change, no
            // interrupt and no state mutation.
            return vec![];
        }
        match kind {
            DeadlineKind::Execution => self.force(CleanupCause::DeadlineExpired(kind), now).1,
            // The second-stage deadline. The backend did not answer within its
            // grace. There is no third timer and no escalation: the session is
            // WITHDRAWN - the physical connection destroyed rather than
            // returned - and the transaction settles as indeterminate,
            // carrying the latched cause. That closes invariant 3: every fired
            // deadline has a bounded path to `Settled` requiring no
            // cooperation from the backend.
            DeadlineKind::CancellationSql | DeadlineKind::TerminalSql => {
                let cause = self
                    .cleanup
                    .map_or(CleanupCause::DeadlineExpired(kind), |latched| latched.cause);
                self.slot.disarm();
                self.settle_now(TerminalOutcome::Indeterminate(cause), true)
            }
        }
    }

    fn on_cancellation_acknowledged(
        &mut self,
        token: CommandToken,
        ack: CleanupAck,
    ) -> Vec<Action> {
        // Cancellation completions match the entry's single cancellation
        // token, not the command token.
        if self.cancellation_token != Some(token) {
            return vec![Action::Reply(Err(
                TxProtocolError::StaleTransactionCompletion,
            ))];
        }
        if self.state != TxState::Cancelling {
            return vec![Action::Reply(Err(TxProtocolError::TransactionNotReady))];
        }
        let latched = self.cleanup.expect("Cancelling always carries its cause");
        self.slot.disarm();
        if latched.goal.is_proved_by(ack) {
            self.settle_now(TerminalOutcome::Cancelled(latched.cause), false)
        } else {
            // Anything else - an acknowledgement that contradicts the goal, or
            // one that is indeterminate - settles as indeterminate and
            // withdraws the session: the connection is destroyed rather than
            // returned, so no later user can inherit it.
            self.settle_now(TerminalOutcome::Indeterminate(latched.cause), true)
        }
    }

    // -----------------------------------------------------------------
    // Shared helpers
    // -----------------------------------------------------------------

    /// Reach `Settled`, applying the effect fate and disposing of the session.
    fn settle_now(&mut self, outcome: TerminalOutcome, withdraw: bool) -> Vec<Action> {
        self.state = TxState::Settled;
        self.outcome = Some(outcome);
        self.active = None;
        self.slot.disarm();

        let mut actions = Vec::new();
        if outcome.publishes() {
            let effects = self.frames.take_effects_for_confirmed_commit();
            actions.push(Action::PublishEffects(effects));
        } else {
            self.frames.discard_all_effects();
            actions.push(Action::DiscardEffects);
        }
        if withdraw {
            self.session = SessionOwnership::Withdrawn;
            actions.push(Action::WithdrawSession);
        } else {
            self.session = SessionOwnership::None;
            actions.push(Action::ReleaseSession);
        }
        // Invariant 3: every granted admission has exactly one release.
        actions.push(Action::ReleaseAdmission);
        actions.push(Action::Reply(Ok(TxReply::Settled(outcome))));
        actions
    }

    /// A backend command that forced cleanup interrupted has answered.
    ///
    /// **Only reachable because cleanup can now CANCEL rather than withdraw.**
    /// Before that, a force landing on a command-owned session proved no goal,
    /// settled indeterminate and reached `Settled` inside the same
    /// `apply` - so the command's completion always arrived at a `Settled`
    /// reducer and guard 5 answered it. Cancelling instead means the cleanup
    /// waits for the cancelled statement, and its completion arrives here, in
    /// `Cancelling`.
    ///
    /// The state must NOT move. `on_operation_completed` would have parked it in
    /// `Idle` or `Poisoned`, and both are states that issue creator data SQL -
    /// `Poisoned` because invariant 13 makes it recoverable, which is the whole
    /// reason invariant 16 forbids a force from routing through it. A cancelled
    /// statement's own error must not be the thing that walks a terminally
    /// forced transaction back onto the data path.
    ///
    /// What it DOES do is hand the session back to the registry, because that
    /// is now true: the guard that was holding it has returned it, and the
    /// cleanup `ROLLBACK` is what runs next.
    fn command_returned_during_cleanup(&mut self) -> Vec<Action> {
        self.active = None;
        self.session = SessionOwnership::Registry;
        let latched = self.cleanup.expect("Cancelling always carries its cause");
        vec![Action::Reply(Err(TxProtocolError::Cleanup(latched.cause)))]
    }

    /// Invariant 16: once `Cancelling` is entered, no creator request is
    /// accepted and no path leads back to a state that can issue data SQL.
    ///
    /// The caller receives the **latched cleanup cause**, not a generic
    /// rejection: a rollback that ends the transaction for the *wrong* reason
    /// is what the single-latched cause exists to prevent.
    fn refuse_if_cleaning_up(&self) -> Option<Vec<Action>> {
        let latched = self.cleanup?;
        Some(vec![Action::Reply(Err(TxProtocolError::Cleanup(
            latched.cause,
        )))])
    }

    /// Guard 2: every completion must carry the token its current action
    /// minted; a wrong one changes nothing.
    fn check_token(&self, token: CommandToken) -> Option<Vec<Action>> {
        if self.active == Some(token) {
            None
        } else {
            Some(vec![Action::Reply(Err(
                TxProtocolError::StaleTransactionCompletion,
            ))])
        }
    }

    const fn mint_token(&mut self) -> CommandToken {
        self.next_token += 1;
        CommandToken(self.next_token)
    }

    /// Queue an effect against the current frame. Exposed for the driver's
    /// write path, and for the arms that rule on effect locality.
    ///
    /// # Errors
    ///
    /// Returns the refusal this operation's guard produced. Every one is a
    /// value the caller receives, never an out-of-band log line.
    pub fn queue_effect(&mut self, effect: Effect) -> Result<(), FrameError> {
        self.frames.queue_effect(effect)
    }
}

const fn event_authority(event: &TxEvent) -> Option<&EventAuthority> {
    match event {
        TxEvent::AuthorityObserved { authority, .. }
        | TxEvent::Cancel { authority }
        | TxEvent::DetachRequested { authority } => Some(authority),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
