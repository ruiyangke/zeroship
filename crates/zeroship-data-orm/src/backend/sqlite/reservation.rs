//! Reservation, cancellation and terminal-classification protocol for the
//! SQLite actor (SC-2, `docs/proposals/2026-08-26-sc2-sqlite-actor-protocol.md`).
//!
//! ## What this module owns
//!
//! Three things the actor could not express before it existed:
//!
//! 1. **Who a command belongs to.** Every data command names a
//!    [`Reservation`]; the actor refuses one whose reservation is not the
//!    current owner of the connection it would run on, instead of running it
//!    on whatever connection is free.
//! 2. **Whether a cancellation won.** A caller and the actor race for one
//!    word, [`Reservation::terminal`], and exactly one of them claims it. The
//!    loser is told what the winner decided; it never acts on its own guess.
//! 3. **What a terminal statement actually did.** [`classify_commit`] and
//!    [`classify_rollback`] sample `is_autocommit` and let it - not the result
//!    code - decide. A `COMMIT` that returned `Err` is not proof of rollback.
//!
//! ## The definition that makes cancellation decidable
//!
//! The platform cannot observe when SQLite reaches its first `sqlite3_step`,
//! so "in flight" is defined at a state we own:
//!
//! > **Execution start is the actor's transition to `Running`.**
//!
//! `Running` is [`Reservation::running_seq`] holding a non-zero command
//! sequence. The handshake between a cancelling caller and the executing actor
//! is Dekker's, on `SeqCst`:
//!
//! ```text
//! caller: terminal.store(CancelIntent)  ; then load(running_seq)
//! actor : running_seq.store(seq)        ; then load(terminal)
//! ```
//!
//! `SeqCst` gives a single total order over those four operations, so at least
//! one side observes the other. If the actor sees the intent it never starts
//! (or never continues); if the caller sees a running sequence it interrupts.
//! Neither outcome depends on which thread was scheduled first, which is what
//! makes the four interleavings in SC-2 decidable rather than fuzzy.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use rusqlite::Connection;

use zeroship_data_orm::error::DbError;

/// Opaque identifier for one app's transaction lane.
///
/// Assigned by the session when an app first reserves a transaction and reused
/// for that app's whole life on the session. It is NOT the app id: the actor
/// and the caller both need a `Copy` key they can put inside a [`Lane`], and an
/// app id is a `String`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct TxLaneId(pub(crate) u32);

/// Which connection a command runs on.
///
/// SC-2 Decision 1: a transaction connection is reserved to at most one
/// explicit creator transaction, `op_conn` serves autocommit work.
///
/// **The transaction half is per app, and that is the admission key.** SC-1
/// admits one top-level transaction per `(runtime_instance_id, app_id)`; a
/// single shared transaction connection enforced one per
/// `(runtime_instance_id, session)` instead, so app B's `db.transaction()` was
/// refused while app A held one (defect L22b). Carrying the lane id here is
/// what makes the two keys the same key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Lane {
    /// `op_conn` - autocommit operations, each wrapped in
    /// `BEGIN DEFERRED ... COMMIT` where the statement permits it. One
    /// connection serves every app: an autocommit reservation is minted per
    /// command and settles at that command's completion, so there is no
    /// cross-command state for two apps to share.
    Op,
    /// One app's transaction connection - at most one explicit creator
    /// transaction at a time, and that "one" is per app.
    Tx(TxLaneId),
}

impl Lane {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Op => "op_conn",
            Self::Tx(_) => "tx_conn",
        }
    }

    /// The transaction lane id, if this is a transaction lane.
    pub(crate) fn tx_id(self) -> Option<TxLaneId> {
        match self {
            Self::Op => None,
            Self::Tx(id) => Some(id),
        }
    }

    pub(crate) fn is_tx(self) -> bool {
        matches!(self, Self::Tx(_))
    }
}

/// Nothing has claimed this reservation's terminal yet.
const TERMINAL_PENDING: u8 = 0;
/// A caller has asked to cancel; the actor has not yet acted on it.
const TERMINAL_CANCEL_INTENT: u8 = 1;
/// The actor claimed the terminal as a cancellation.
const TERMINAL_CLAIMED_CANCELLED: u8 = 2;
/// The actor claimed the terminal as a completion. Set **before** the
/// terminal SQL is sent, so a cancel arriving afterwards is answered
/// `AlreadyCompleted` and never turns into a `ROLLBACK`.
const TERMINAL_CLAIMED_COMPLETED: u8 = 3;

/// Shared per-reservation state. One `Arc` lives on the caller side (inside
/// the lease / cancel handle) and clones ride on each queued command, so the
/// actor thread and the caller thread read and write the same words.
#[derive(Debug)]
pub struct Reservation {
    id: u64,
    lane: Lane,
    kind: ReservationKind,
    /// The lane connection generation this reservation is bound to. A
    /// quarantined connection is recycled and its generation bumped, so an
    /// interrupt aimed at the old connection cannot land on its replacement.
    ///
    /// Atomic because the caller's mint-time reading is a **guess** for a
    /// transaction lane: the caller mints before the actor has opened (or
    /// reopened) the connection, so only the actor knows which incarnation the
    /// reservation actually got. It stamps the truth in
    /// [`Self::adopt_generation`] before it acknowledges the reservation, which
    /// is strictly before the caller can issue a command or a cancellation on
    /// it.
    generation: AtomicU64,
    terminal: AtomicU8,
    /// Non-zero while the actor is executing a command for this reservation.
    /// The value is that command's sequence number.
    running_seq: AtomicU64,
    /// The sequence a caller has asked to interrupt. Read by the actor's
    /// progress latch, which covers the window where the actor has stored
    /// `Running` but SQLite has not yet stepped - exactly the gap a raw
    /// `sqlite3_interrupt` is documented to no-op in.
    cancel_seq: AtomicU64,
    /// Set by the actor the moment it issues any SQL for this reservation.
    /// Distinguishes SC-2 case 1's `Cancelled(NoSqlStarted)` - where no
    /// `BEGIN` and no data SQL was ever issued - from a cancellation that has
    /// something to roll back.
    began: AtomicBool,
    /// Set by the actor once this reservation has run one command. Only the
    /// autocommit lane reads it, where "one reservation, one command" is the
    /// whole of ownership - see [`Reservation::used`].
    used: AtomicBool,
    outcome: Mutex<Option<TerminalOutcome>>,
}

/// What a reservation is for. Autocommit reservations retire at command
/// completion; transaction reservations at `Settle` or cancellation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ReservationKind {
    Autocommit,
    Transaction,
}

impl Reservation {
    pub(crate) fn new(id: u64, lane: Lane, kind: ReservationKind, generation: u64) -> Self {
        Self {
            id,
            lane,
            kind,
            generation: AtomicU64::new(generation),
            terminal: AtomicU8::new(TERMINAL_PENDING),
            running_seq: AtomicU64::new(0),
            cancel_seq: AtomicU64::new(0),
            began: AtomicBool::new(false),
            used: AtomicBool::new(false),
            outcome: Mutex::new(None),
        }
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn lane(&self) -> Lane {
        self.lane
    }

    pub(crate) fn kind(&self) -> ReservationKind {
        self.kind
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Actor side: record the generation of the connection this reservation was
    /// actually bound to.
    ///
    /// Called from the `Reserve` handler before it acknowledges, so the value a
    /// canceller reads is the actor's, never the caller's mint-time guess. It
    /// closes a race the guess alone cannot: an idle lane evicted between the
    /// mint and the bind is reopened under a bumped generation, and a
    /// reservation left carrying the pre-eviction reading would aim its
    /// interrupt at - and have its cancellation cleanup silently skipped on -
    /// the wrong incarnation.
    pub(crate) fn adopt_generation(&self, generation: u64) {
        self.generation.store(generation, Ordering::SeqCst);
    }

    pub(crate) fn began(&self) -> bool {
        self.began.load(Ordering::SeqCst)
    }

    pub(crate) fn mark_began(&self) {
        self.began.store(true, Ordering::SeqCst);
    }

    // -- caller side ------------------------------------------------------

    /// Ask to cancel. Returns the sequence the caller must interrupt, if any.
    ///
    /// `Some(seq)` means the actor was already `Running` that command when the
    /// intent landed: the caller interrupts the lane's connection. `None` with
    /// [`CancelIntent::Set`] means the actor had not started, and its
    /// pre-start check will see the intent - no interrupt is needed and
    /// issuing one would target an unrelated later statement.
    pub(crate) fn request_cancel(&self) -> CancelIntent {
        // The store half of the Dekker handshake. It must land before the
        // load below, which is what `SeqCst` buys and what `Relaxed` would
        // not: with a weaker ordering both sides can read stale words and
        // neither acts.
        match self.terminal.compare_exchange(
            TERMINAL_PENDING,
            TERMINAL_CANCEL_INTENT,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => {}
            Err(TERMINAL_CLAIMED_COMPLETED) => return CancelIntent::AlreadyCompleted,
            Err(_) => return CancelIntent::AlreadyCancelling,
        }
        let running = self.running_seq.load(Ordering::SeqCst);
        if running == 0 {
            CancelIntent::Set
        } else {
            self.cancel_seq.store(running, Ordering::SeqCst);
            CancelIntent::Interrupt(running)
        }
    }

    // -- actor side -------------------------------------------------------

    /// Transition to `Running` for `seq`, then check whether a cancellation
    /// beat us to it. `false` means do not execute.
    pub(crate) fn enter_running(&self, seq: u64) -> bool {
        debug_assert!(seq != 0, "command sequence 0 is the not-running sentinel");
        self.running_seq.store(seq, Ordering::SeqCst);
        // The load half of the handshake, and also the progress latch: a
        // caller that set `cancel_seq` for this very sequence between the
        // store above and this load is honoured here rather than losing its
        // interrupt into the pre-step window.
        self.terminal.load(Ordering::SeqCst) == TERMINAL_PENDING
            && self.cancel_seq.load(Ordering::SeqCst) != seq
    }

    pub(crate) fn leave_running(&self) {
        self.running_seq.store(0, Ordering::SeqCst);
    }

    /// Claim the terminal as a completion. Called **before** the terminal SQL
    /// is sent (SC-2 case 3): once this returns `true` a later drop-cancel is
    /// answered `AlreadyCompleted` and no `ROLLBACK` is ever issued, so the
    /// write stays durable.
    pub(crate) fn claim_completed(&self) -> bool {
        self.terminal
            .compare_exchange(
                TERMINAL_PENDING,
                TERMINAL_CLAIMED_COMPLETED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Claim the terminal as a cancellation. `false` means the terminal is
    /// already claimed - by a completion, or by an earlier cancellation - and
    /// this caller must not act on its own.
    ///
    /// **The second-claim arm is load-bearing, not tidiness.** It returned
    /// `true` until 2026-08-27, so a reservation whose terminal already read
    /// `CLAIMED_CANCELLED` handed a *second* caller the right to run cleanup.
    /// The actor's cleanup is a `ROLLBACK` on the reservation's lane, and by
    /// the time a duplicate arrives that lane can belong to somebody else, so
    /// "claim an already-claimed terminal" is a licence to destroy a stranger's
    /// open transaction. Exactly one claim wins; every later one is told so.
    pub(crate) fn claim_cancelled(&self) -> bool {
        loop {
            let current = self.terminal.load(Ordering::SeqCst);
            match current {
                TERMINAL_CLAIMED_COMPLETED | TERMINAL_CLAIMED_CANCELLED => return false,
                _ => {
                    if self
                        .terminal
                        .compare_exchange(
                            current,
                            TERMINAL_CLAIMED_CANCELLED,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_ok()
                    {
                        return true;
                    }
                }
            }
        }
    }

    /// Has this reservation already executed a command?
    ///
    /// The autocommit lane's ownership rule is built on this: SC-2 mints an
    /// autocommit reservation per command and settles it at that command's
    /// completion, so a *second* command naming one is a stale reservation and
    /// not a re-use. See [`super::session`]'s `check_owner`.
    pub(crate) fn used(&self) -> bool {
        self.used.load(Ordering::SeqCst)
    }

    pub(crate) fn mark_used(&self) {
        self.used.store(true, Ordering::SeqCst);
    }

    pub(crate) fn store_outcome(&self, outcome: TerminalOutcome) {
        *self
            .outcome
            .lock()
            .expect("sqlite reservation outcome mutex poisoned") = Some(outcome);
    }

    pub(crate) fn stored_outcome(&self) -> Option<TerminalOutcome> {
        self.outcome
            .lock()
            .expect("sqlite reservation outcome mutex poisoned")
            .clone()
    }
}

/// What [`Reservation::request_cancel`] decided.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CancelIntent {
    /// Intent recorded; the actor had not started, so do not interrupt.
    Set,
    /// Intent recorded and the actor is running this command sequence:
    /// interrupt the lane's connection.
    Interrupt(u64),
    /// Another caller is already cancelling this reservation.
    AlreadyCancelling,
    /// The outcome was already decided as a completion. SC-2: *a cancellation
    /// that arrives after the outcome is decided is a question, not a
    /// command.* Do not interrupt, do not roll back.
    AlreadyCompleted,
}

/// What cleanup a cancellation performed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CancelCleanup {
    /// SC-2 case 1: no `BEGIN`, no data SQL, ever issued.
    NoSqlStarted,
    /// A `ROLLBACK` ran and `is_autocommit` proved the transaction ended.
    RolledBack,
    /// `ROLLBACK` errored because SQLite had already ended the transaction
    /// itself - the ordinary result of interrupting a write. Not a failure.
    AlreadyRolledBack,
    /// The reservation no longer owned the connection it names, so this
    /// cancellation cleaned up **nothing**: whoever retired the reservation
    /// (`Release`/`Reserve`'s `unbind_tx`, or its own `Settle`) already did.
    ///
    /// It exists so the actor never has to choose between issuing a `ROLLBACK`
    /// on a lane a stranger now owns and *reporting* a rollback it did not
    /// perform. Both are lies; this names the state instead.
    AlreadyRetired,
}

/// The terminal fate of a reservation, as classified by `is_autocommit`.
///
/// `CommitIndeterminate` is a first-class outcome and not an error. Collapsing
/// it into failure is defect L8's mistake in the other direction: there the
/// plugin believed a `COMMIT` that PostgreSQL had answered with a `ROLLBACK`
/// tag; here the temptation is to report a loss that may not have happened.
/// Both resolve an uncertainty by assumption. This type represents it instead.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TerminalOutcome {
    /// The only confirmed-commit arm: `COMMIT` returned `Ok` **and**
    /// `is_autocommit` is true afterwards.
    Committed,
    /// The commit did not finish and a following `ROLLBACK` proved the
    /// transaction ended. The writes are gone; that much is known.
    CommitFailed {
        message: String,
    },
    /// Nothing available proves commit versus rollback. The connection is
    /// quarantined and recycled. **Never reported as success.**
    CommitIndeterminate {
        message: String,
    },
    RolledBack,
    /// A `ROLLBACK` we asked for left the connection still inside a
    /// transaction. Cleanup is unproved; quarantine.
    RollbackFailed {
        message: String,
    },
    /// A cancellation's `ROLLBACK` left the connection still inside a
    /// transaction. Quarantine.
    CleanupIndeterminate {
        message: String,
    },
    Cancelled {
        cleanup: CancelCleanup,
        /// The original rusqlite error latched at the point of cancellation,
        /// kept as diagnostics. Never used to classify.
        cause: Option<String>,
    },
    /// The outcome was already decided before this cancellation arrived.
    AlreadyCompleted(Box<TerminalOutcome>),
}

/// The SQLite half of SC-1's forced cleanup.
///
/// Makes the same judgement as the PostgreSQL arm on different evidence: there
/// is no command tag, so the authority is `is_autocommit` sampled inside the
/// actor **after** the statement - the same "sample after, never before" rule,
/// enforced by SC-2's own terminal classifier via [`terminal_result`] rather
/// than restated here.
pub async fn cleanup(
    handle: &super::session::SqliteSessionHandle,
) -> zeroship_data_orm::error::CleanupAck {
    use zeroship_data_orm::error::{CleanupAck, TerminalResult};
    match handle
        .settle(super::session::TerminalIntent::Rollback)
        .await
    {
        Ok(outcome) => match terminal_result(&outcome).0 {
            TerminalResult::RolledBack => CleanupAck::RolledBack,
            TerminalResult::Committed | TerminalResult::Indeterminate => CleanupAck::Indeterminate,
        },
        Err(_) => CleanupAck::Indeterminate,
    }
}

/// Project SC-2's classified SQLite terminal outcome onto SC-1's.
///
/// The vendor decides what happened; SC-1 decides what it means. Lives beside
/// the enum it reads rather than in the driver, so the protocol never has to
/// name a SQLite type to learn its own answer.
pub fn terminal_result(
    outcome: &TerminalOutcome,
) -> (
    zeroship_data_orm::error::TerminalResult,
    Option<zeroship_data_orm::error::DbError>,
) {
    use zeroship_data_orm::error::TerminalResult;
    match outcome {
        TerminalOutcome::Committed => (TerminalResult::Committed, None),
        TerminalOutcome::RolledBack | TerminalOutcome::Cancelled { .. } => {
            (TerminalResult::RolledBack, None)
        }
        TerminalOutcome::CommitFailed { message } => (
            TerminalResult::RolledBack,
            Some(DbError::internal(message.clone())),
        ),
        TerminalOutcome::CommitIndeterminate { message }
        | TerminalOutcome::RollbackFailed { message }
        | TerminalOutcome::CleanupIndeterminate { message } => (
            TerminalResult::Indeterminate,
            Some(DbError::internal(message.clone())),
        ),
        TerminalOutcome::AlreadyCompleted(inner) => terminal_result(inner),
    }
}

impl TerminalOutcome {
    /// Does this outcome leave the connection in a state that must not be
    /// handed to the next reservation?
    pub(crate) fn quarantines(&self) -> bool {
        match self {
            Self::CommitIndeterminate { .. }
            | Self::RollbackFailed { .. }
            | Self::CleanupIndeterminate { .. } => true,
            Self::AlreadyCompleted(inner) => inner.quarantines(),
            _ => false,
        }
    }

    /// Collapse to the `Result` the transaction orchestrator consumes.
    ///
    /// `Committed` and `RolledBack` are the successes. Everything else is an
    /// error carrying a distinct `code`, so an operator reading a log can tell
    /// "the writes are gone" from "nobody knows".
    pub(crate) fn into_result(self) -> Result<(), DbError> {
        match self {
            Self::Committed | Self::RolledBack => Ok(()),
            Self::AlreadyCompleted(inner) => inner.into_result(),
            Self::Cancelled { cleanup, cause } => Err(DbError::Coded {
                code: CANCELLED_CODE.to_string(),
                message: format!(
                    "db: statement cancelled ({cleanup:?}){}",
                    cause
                        .map(|c| format!("; original error: {c}"))
                        .unwrap_or_default()
                ),
                hint: None,
            }),
            Self::CommitFailed { message } => Err(DbError::Coded {
                code: "commit_failed".to_string(),
                message,
                hint: None,
            }),
            Self::CommitIndeterminate { message } => Err(DbError::Coded {
                code: "commit_indeterminate".to_string(),
                message,
                hint: Some(
                    "the transaction's fate is unknown: SQLite neither confirmed the commit \
                     nor proved a rollback. Re-read the affected rows before retrying."
                        .to_string(),
                ),
            }),
            Self::RollbackFailed { message } => Err(DbError::Coded {
                code: "rollback_failed".to_string(),
                message,
                hint: None,
            }),
            Self::CleanupIndeterminate { message } => Err(DbError::Coded {
                code: "cleanup_indeterminate".to_string(),
                message,
                hint: None,
            }),
        }
    }
}

/// What a cancellation reports when it finds the terminal already claimed.
///
/// The stored outcome is the answer whenever the winner left one. When it did
/// not, **nothing here knows what happened**, and the default must say so: the
/// terminal word alone proves only that some other path claimed the right to
/// decide, never what it decided.
///
/// This defaulted to [`TerminalOutcome::Committed`] until 2026-08-27 - a
/// confirmed commit that nothing proved, which `into_result` then reported as
/// `Ok(())`. That is the collapse this whole module exists to refuse: an
/// uncertainty resolved by assumption, in the direction that publishes success.
pub(crate) fn outcome_for_a_claimed_terminal(reservation: &Reservation) -> TerminalOutcome {
    let stored =
        reservation
            .stored_outcome()
            .unwrap_or_else(|| TerminalOutcome::CommitIndeterminate {
                message: format!(
                    "db: reservation {} had its terminal claimed by another path that recorded no \
                 outcome; nothing proves whether it committed or rolled back",
                    reservation.id()
                ),
            });
    TerminalOutcome::AlreadyCompleted(Box::new(stored))
}

/// Wire code for a statement the platform itself cancelled.
///
/// Shared with the `SQLITE_INTERRUPT` arm of the error mapper
/// (`super::error::from_sqlite`) so a cancellation reports the same code
/// whether the actor classified it or a raw rusqlite error carried it out.
pub(crate) const CANCELLED_CODE: &str = "statement_cancelled";

/// Classify a `COMMIT`, per SC-2's terminal table.
///
/// The result code is evidence, never the verdict. The sample of
/// `is_autocommit` taken after the statement is finalized is the authority.
pub(crate) fn classify_commit(
    conn: &Connection,
    raw: Result<(), rusqlite::Error>,
) -> TerminalOutcome {
    debug_assert!(
        !conn.is_busy(),
        "terminal classification ran with a statement still busy; is_autocommit \
         would be sampled against an unfinalized statement"
    );
    let autocommit = conn.is_autocommit();
    match (raw, autocommit) {
        (Ok(()), true) => TerminalOutcome::Committed,
        (Ok(()), false) => TerminalOutcome::CommitIndeterminate {
            message: "db: COMMIT reported success but the connection is still inside a \
                      transaction; the commit cannot be published as successful"
                .to_string(),
        },
        (Err(e), false) => {
            // The commit did not finish. One ROLLBACK, then let is_autocommit
            // say whether it ended the transaction. "ROLLBACK errored because
            // there was no transaction" is not a failure here - the sample is.
            let rollback = conn.execute_batch("ROLLBACK");
            if conn.is_autocommit() {
                TerminalOutcome::CommitFailed {
                    message: format!(
                        "db: COMMIT failed and was rolled back: {e}{}",
                        rollback
                            .err()
                            .map(|re| format!(" (ROLLBACK reported: {re})"))
                            .unwrap_or_default()
                    ),
                }
            } else {
                TerminalOutcome::CommitIndeterminate {
                    message: format!(
                        "db: COMMIT failed ({e}) and the following ROLLBACK left the \
                         connection inside a transaction; the transaction's fate is unknown"
                    ),
                }
            }
        }
        (Err(e), true) => TerminalOutcome::CommitIndeterminate {
            // The transaction ended, but nothing here proves commit versus
            // auto-rollback. No SQLite result code alone upgrades this.
            message: format!(
                "db: COMMIT returned an error ({e}) but the transaction has ended; \
                 nothing proves whether it committed or auto-rolled-back"
            ),
        },
    }
}

/// Classify a `ROLLBACK`, per SC-2's terminal table.
///
/// `cancellation` selects the two rows that report a cancellation rather than
/// an ordinary rollback; `cause` is the latched original error, carried as
/// diagnostics only.
pub(crate) fn classify_rollback(
    conn: &Connection,
    raw: Result<(), rusqlite::Error>,
    cancellation: bool,
    cause: Option<String>,
) -> TerminalOutcome {
    debug_assert!(
        !conn.is_busy(),
        "terminal classification ran with a statement still busy"
    );
    let autocommit = conn.is_autocommit();
    match (raw, autocommit) {
        (_, false) => {
            let message = "db: ROLLBACK left the connection inside a transaction; cleanup is \
                           unproved"
                .to_string();
            if cancellation {
                TerminalOutcome::CleanupIndeterminate { message }
            } else {
                TerminalOutcome::RollbackFailed { message }
            }
        }
        (Ok(()), true) => {
            if cancellation {
                TerminalOutcome::Cancelled {
                    cleanup: CancelCleanup::RolledBack,
                    cause,
                }
            } else {
                TerminalOutcome::RolledBack
            }
        }
        (Err(_), true) => {
            // SQLite already ended the transaction - the ordinary outcome of
            // interrupting a write. The error is diagnostics, not a verdict.
            if cancellation {
                TerminalOutcome::Cancelled {
                    cleanup: CancelCleanup::AlreadyRolledBack,
                    cause,
                }
            } else {
                TerminalOutcome::RolledBack
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! The terminal table gets an arm here, row by row, with the fault
    //! injected at the terminal statement and `is_autocommit` sampled after.
    //! SC-2 records that the table had no arm at all: the rows that matter are
    //! exactly the ones an implementation reaching for the result code gets
    //! wrong, and nothing above them ruled on any of them.
    //!
    //! What these do NOT establish: that the actor calls the classifier at the
    //! right moment, or that quarantine recycles the connection. Those are
    //! actor-level and are covered in `session.rs` / the integration target.

    use super::*;

    fn synth(extended_code: i32) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(extended_code),
            Some("synthesised".to_string()),
        )
    }

    /// Row 1: `COMMIT` Ok + autocommit true. The only confirmed-commit arm.
    #[test]
    fn commit_ok_with_autocommit_true_is_the_only_committed_arm() {
        let conn_file = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(conn_file.path()).unwrap();
        conn.execute_batch("CREATE TABLE t (x); BEGIN; INSERT INTO t VALUES (1);")
            .unwrap();
        let raw = conn.execute_batch("COMMIT");
        assert_eq!(classify_commit(&conn, raw), TerminalOutcome::Committed);
    }

    /// Row 2: `COMMIT` Ok while still inside a transaction is a protocol
    /// contradiction, and must NOT publish success.
    #[test]
    fn commit_ok_with_autocommit_false_is_indeterminate_not_success() {
        let conn_file = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(conn_file.path()).unwrap();
        conn.execute_batch("CREATE TABLE t (x); BEGIN;").unwrap();
        // Feed a synthetic Ok while the connection is provably in a
        // transaction: this is the contradiction the row describes.
        assert!(!conn.is_autocommit());
        let outcome = classify_commit(&conn, Ok(()));
        assert!(
            matches!(outcome, TerminalOutcome::CommitIndeterminate { .. }),
            "an Ok COMMIT inside an open transaction must not be Committed; got {outcome:?}"
        );
        assert!(outcome.quarantines(), "the contradiction must quarantine");
        conn.execute_batch("ROLLBACK").unwrap();
    }

    /// Row 3: `COMMIT` Err while still in a transaction -> one ROLLBACK ->
    /// autocommit true -> `CommitFailed`.
    #[test]
    fn commit_err_inside_a_transaction_rolls_back_and_reports_commit_failed() {
        let conn_file = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(conn_file.path()).unwrap();
        conn.execute_batch("CREATE TABLE t (x); BEGIN; INSERT INTO t VALUES (1);")
            .unwrap();
        assert!(!conn.is_autocommit());
        let outcome = classify_commit(&conn, Err(synth(5)));
        assert!(
            matches!(outcome, TerminalOutcome::CommitFailed { .. }),
            "expected CommitFailed, got {outcome:?}"
        );
        assert!(!outcome.quarantines());
        assert!(conn.is_autocommit(), "the classifier owed one ROLLBACK");
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "CommitFailed must mean the writes are gone");
    }

    /// Row 4: `COMMIT` Err with the transaction already ended. **Nothing here
    /// proves commit versus auto-rollback**, so the outcome is
    /// `CommitIndeterminate` - never `CommitFailed`.
    #[test]
    fn commit_err_with_the_transaction_already_ended_is_indeterminate() {
        let conn_file = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(conn_file.path()).unwrap();
        conn.execute_batch("CREATE TABLE t (x);").unwrap();
        assert!(conn.is_autocommit());
        let outcome = classify_commit(&conn, Err(synth(9)));
        assert!(
            matches!(outcome, TerminalOutcome::CommitIndeterminate { .. }),
            "a failed COMMIT on an ended transaction must not claim a rollback; got {outcome:?}"
        );
        assert!(outcome.quarantines());
    }

    /// Rows 5 and 6: explicit and cancellation `ROLLBACK`, both Ok.
    #[test]
    fn rollback_ok_reports_rolled_back_or_cancelled_by_intent() {
        for cancellation in [false, true] {
            let conn_file = tempfile::NamedTempFile::new().unwrap();
            let conn = Connection::open(conn_file.path()).unwrap();
            conn.execute_batch("CREATE TABLE t (x); BEGIN; INSERT INTO t VALUES (1);")
                .unwrap();
            let raw = conn.execute_batch("ROLLBACK");
            let outcome = classify_rollback(&conn, raw, cancellation, None);
            if cancellation {
                assert_eq!(
                    outcome,
                    TerminalOutcome::Cancelled {
                        cleanup: CancelCleanup::RolledBack,
                        cause: None
                    }
                );
            } else {
                assert_eq!(outcome, TerminalOutcome::RolledBack);
            }
        }
    }

    /// Row 7: `ROLLBACK` Err with autocommit true - SQLite already ended it.
    /// The error is retained as diagnostics only and must not become a
    /// failure.
    #[test]
    fn rollback_err_on_an_already_ended_transaction_is_not_a_failure() {
        let conn_file = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(conn_file.path()).unwrap();
        conn.execute_batch("CREATE TABLE t (x);").unwrap();
        // A real "cannot rollback - no transaction is active" error.
        let raw = conn.execute_batch("ROLLBACK");
        assert!(raw.is_err(), "precondition: no transaction to roll back");
        assert!(conn.is_autocommit());

        assert_eq!(
            classify_rollback(&conn, raw, false, None),
            TerminalOutcome::RolledBack
        );
        let raw = conn.execute_batch("ROLLBACK");
        assert_eq!(
            classify_rollback(&conn, raw, true, Some("interrupted".to_string())),
            TerminalOutcome::Cancelled {
                cleanup: CancelCleanup::AlreadyRolledBack,
                cause: Some("interrupted".to_string()),
            }
        );
    }

    /// Row 8: `ROLLBACK` leaving the connection inside a transaction.
    #[test]
    fn rollback_that_leaves_a_transaction_open_quarantines() {
        let conn_file = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(conn_file.path()).unwrap();
        conn.execute_batch("CREATE TABLE t (x); BEGIN;").unwrap();
        assert!(!conn.is_autocommit());
        // Synthetic: the statement is claimed to have run but the connection
        // is provably still in a transaction, which is the row's condition.
        let explicit = classify_rollback(&conn, Ok(()), false, None);
        assert!(matches!(explicit, TerminalOutcome::RollbackFailed { .. }));
        assert!(explicit.quarantines());
        let cancelled = classify_rollback(&conn, Ok(()), true, None);
        assert!(matches!(
            cancelled,
            TerminalOutcome::CleanupIndeterminate { .. }
        ));
        assert!(cancelled.quarantines());
        conn.execute_batch("ROLLBACK").unwrap();
    }

    /// Every arm of the table that reports uncertainty must refuse to be read
    /// as success. This is the guard against the collapse SC-2 names.
    #[test]
    fn no_indeterminate_outcome_collapses_into_ok_or_into_commit_failed() {
        let indeterminate = [
            TerminalOutcome::CommitIndeterminate {
                message: "x".to_string(),
            },
            TerminalOutcome::RollbackFailed {
                message: "x".to_string(),
            },
            TerminalOutcome::CleanupIndeterminate {
                message: "x".to_string(),
            },
        ];
        assert!(
            !indeterminate.is_empty(),
            "the uncertainty set must not be empty"
        );
        for outcome in indeterminate {
            assert!(outcome.quarantines(), "{outcome:?} must quarantine");
            let err = outcome
                .clone()
                .into_result()
                .expect_err("an uncertain outcome must never be Ok");
            let code = match &err {
                DbError::Coded { code, .. } => code.clone(),
                other => panic!("expected a Coded error, got {other:?}"),
            };
            assert_ne!(
                code, "commit_failed",
                "{outcome:?} was collapsed into a definite failure"
            );
        }
    }

    // -- the cancellation handshake ---------------------------------------

    #[test]
    fn a_cancel_before_execution_starts_stops_the_actor_from_running() {
        let r = Reservation::new(1, Lane::Tx(TxLaneId(0)), ReservationKind::Transaction, 0);
        assert_eq!(r.request_cancel(), CancelIntent::Set);
        assert!(
            !r.enter_running(7),
            "the actor must refuse to start a cancelled reservation"
        );
    }

    #[test]
    fn a_cancel_while_running_targets_that_exact_command_sequence() {
        let r = Reservation::new(2, Lane::Op, ReservationKind::Autocommit, 0);
        assert!(r.enter_running(42));
        assert_eq!(r.request_cancel(), CancelIntent::Interrupt(42));
    }

    /// SC-2 case 3. The actor claimed the terminal as a completion before
    /// sending COMMIT, so a later drop-cancel gets a question answered, not a
    /// rollback issued.
    #[test]
    fn a_cancel_after_the_completion_claim_neither_interrupts_nor_rolls_back() {
        let r = Reservation::new(3, Lane::Tx(TxLaneId(0)), ReservationKind::Transaction, 0);
        assert!(r.claim_completed());
        r.store_outcome(TerminalOutcome::Committed);

        assert_eq!(r.request_cancel(), CancelIntent::AlreadyCompleted);
        assert!(
            !r.claim_cancelled(),
            "a cancellation must not be able to claim a completed terminal"
        );
        assert_eq!(r.stored_outcome(), Some(TerminalOutcome::Committed));
    }

    #[test]
    fn a_completion_cannot_claim_a_terminal_a_cancellation_already_holds() {
        let r = Reservation::new(4, Lane::Tx(TxLaneId(0)), ReservationKind::Transaction, 0);
        assert_eq!(r.request_cancel(), CancelIntent::Set);
        assert!(r.claim_cancelled());
        assert!(
            !r.claim_completed(),
            "a completion must not overwrite a claimed cancellation"
        );
    }

    /// A cancellation cannot claim a terminal it already holds.
    ///
    /// The claim is a licence to run cleanup, and the actor's cleanup is a
    /// `ROLLBACK` on the reservation's *lane* - which a later reservation may
    /// own by then. Handing that licence out twice is how one duplicate
    /// `Cancel` destroys a stranger's open transaction.
    #[test]
    fn a_second_cancellation_claim_on_the_same_terminal_is_refused() {
        let r = Reservation::new(5, Lane::Tx(TxLaneId(0)), ReservationKind::Transaction, 0);
        assert_eq!(r.request_cancel(), CancelIntent::Set);
        assert!(r.claim_cancelled(), "the first claim must win the terminal");
        assert!(
            !r.claim_cancelled(),
            "a second claim must not re-win a terminal this reservation already holds"
        );
        // And a third, so the arm rules on repetition rather than on parity.
        assert!(!r.claim_cancelled(), "every later claim must lose too");
    }

    /// A claimed terminal with no stored outcome is an **unknown**, not a
    /// commit. Defaulting it to `Committed` published a success nothing
    /// proved and `into_result` turned it into `Ok(())`.
    #[test]
    fn a_claimed_terminal_with_no_stored_outcome_is_indeterminate_not_committed() {
        let r = Reservation::new(6, Lane::Tx(TxLaneId(0)), ReservationKind::Transaction, 0);
        assert!(r.claim_completed());
        assert_eq!(r.stored_outcome(), None, "precondition: nothing recorded");

        let outcome = outcome_for_a_claimed_terminal(&r);
        let TerminalOutcome::AlreadyCompleted(inner) = &outcome else {
            panic!("a claimed terminal must report AlreadyCompleted; got {outcome:?}");
        };
        assert!(
            matches!(**inner, TerminalOutcome::CommitIndeterminate { .. }),
            "an outcome nobody recorded must not be reported as a commit; got {inner:?}"
        );
        assert!(
            outcome.quarantines(),
            "an unproved terminal must quarantine its connection"
        );
        outcome
            .into_result()
            .expect_err("an unproved terminal must never collapse into Ok(())");
    }

    /// The other half of the same rule: when the winner DID record an outcome,
    /// that outcome is the answer and is not overwritten by the default.
    #[test]
    fn a_claimed_terminal_reports_the_outcome_its_winner_recorded() {
        let r = Reservation::new(7, Lane::Tx(TxLaneId(0)), ReservationKind::Transaction, 0);
        assert!(r.claim_completed());
        r.store_outcome(TerminalOutcome::Committed);

        assert_eq!(
            outcome_for_a_claimed_terminal(&r),
            TerminalOutcome::AlreadyCompleted(Box::new(TerminalOutcome::Committed))
        );
        assert!(
            outcome_for_a_claimed_terminal(&r).into_result().is_ok(),
            "a recorded commit is still a success"
        );
    }

    /// `used` is the autocommit lane's ownership word: one reservation, one
    /// command.
    #[test]
    fn a_reservation_records_that_it_has_run_a_command() {
        let r = Reservation::new(8, Lane::Op, ReservationKind::Autocommit, 0);
        assert!(!r.used(), "a freshly minted reservation has run nothing");
        r.mark_used();
        assert!(r.used(), "the mark must survive for the ownership check");
    }
}
