//! Transaction orchestration shared by Rust callers and the V8 adapter.
//!
//! The reducer owns admission, savepoint frames, settlement and cleanup decisions.
//! The driver executes its actions; host adapters invoke callbacks and report their
//! outcomes without owning SQL sessions.
//!
//! Top-level transactions serialize through an app's transaction lane. Nested
//! callbacks reuse its session and open distinct savepoints. Captured transaction
//! scopes bind operations to their generation and frame; unrelated callbacks keep
//! using autocommit execution.
//!
//! PostgreSQL transactions hold an owned pool lease. SQLite reserves a transaction
//! connection on its actor, separate from autocommit work. An uncertain session is
//! discarded rather than reused. Admission guards release abandoned claims.

#![allow(unsafe_code)]

/// Pure transaction state machine. It owns lifecycle decisions without performing I/O.
pub mod reducer;
pub mod scope;

/// The driver: the only place a reducer action becomes I/O.
pub(crate) mod driver;

#[cfg(test)]
pub mod probe;

use crate::tx_route::TxRoute;
use zeroship_data_orm::error::DbError;

/// Maximum savepoint nesting beneath the top-level transaction.
/// Deeper callback nesting is refused with `savepoint_depth_exceeded`.
pub const MAX_SAVEPOINT_DEPTH: u32 = 8;

/// Future that resolves once this app owns the top-level-transaction
/// claim (see [`crate::context::ThreadDbContext::try_claim_tx`]).
///
/// Held from before the `BEGIN` until after the `COMMIT`/`ROLLBACK` has
/// settled, so it covers the window in which the tx connection slot is
/// still empty — the window two same-turn `transaction()` calls both fell
/// into.
struct AwaitTxClaim {
    app_id: String,
}

impl AwaitTxClaim {
    fn new(app_id: String) -> Self {
        Self { app_id }
    }
}

impl std::future::Future for AwaitTxClaim {
    type Output = Result<(), DbError>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), DbError>> {
        if crate::tx_lanes::with_mut(|l| l.try_claim_tx(&self.app_id)) {
            return std::task::Poll::Ready(Ok(()));
        }
        // The claim is held by the callback this poll is running inside, so
        // nothing can release it before this future resolves. Parking is a
        // deadlock the database cannot see - the transaction holding the lane
        // is idle and healthy - and only a caller-side timeout ends it.
        if crate::tx_lanes::with(|l| l.callback_is_polling(&self.app_id)) {
            return std::task::Poll::Ready(Err(nested_top_level_transaction()));
        }
        // Lost to another task. Park and re-check on the next release;
        // `release_tx_claim` wakes every waiter, so a spurious wake just
        // re-runs this poll.
        crate::tx_lanes::with_mut(|l| l.push_tx_waiter(&self.app_id, cx.waker().clone()));
        std::task::Poll::Pending
    }
}

/// A handle opened a top-level transaction from inside a callback that already
/// holds its lane.
fn nested_top_level_transaction() -> DbError {
    DbError::validation_hinted(
        "nested_top_level_transaction",
        "db: this handle opened a top-level transaction inside a callback holding the same \
         transaction lane",
        "Nest through the handle the callback was given, or open the concurrent transaction on \
         Database::independent().",
    )
}

/// Mark this app's lane while `body` is polled.
///
/// See [`crate::tx_lanes::TxLane`]'s `callback_polls`: the marker is what turns
/// a re-entrant top-level `transaction()` from an invisible self-deadlock into
/// a typed refusal.
pub(crate) fn in_callback<F: std::future::Future>(app_id: &str, body: F) -> InCallback<F> {
    InCallback {
        app_id: app_id.to_owned(),
        body: Box::pin(body),
    }
}

pub(crate) struct InCallback<F> {
    app_id: String,
    body: std::pin::Pin<Box<F>>,
}

/// Lowers the marker even when the callback unwinds.
struct CallbackMark<'a>(&'a str);
impl Drop for CallbackMark<'_> {
    fn drop(&mut self) {
        crate::tx_lanes::with_mut(|l| l.exit_callback(self.0));
    }
}

impl<F: std::future::Future> std::future::Future for InCallback<F> {
    type Output = F::Output;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<F::Output> {
        let this = self.get_mut();
        crate::tx_lanes::with_mut(|l| l.enter_callback(&this.app_id));
        let _mark = CallbackMark(&this.app_id);
        this.body.as_mut().poll(cx)
    }
}

/// Owns a native transaction's admission until settlement takes over.
/// Dropping an admitted guard starts supervised cancellation; its lane remains
/// claimed until cleanup acknowledges rollback or withdraws an uncertain session.
#[derive(Debug)]
pub struct TxAdmission {
    app_id: String,
    owner: crate::OrmContext,
    completion: driver::Completion,
    armed: bool,
}

impl TxAdmission {
    /// Wait for the claim, then arm.
    ///
    /// # Errors
    /// `nested_top_level_transaction` when the claim is held by the callback
    /// this call is running inside, which no amount of waiting can release.
    pub async fn acquire(app_id: String) -> Result<Self, DbError> {
        AwaitTxClaim::new(app_id.clone()).await?;
        Ok(Self::current(app_id))
    }

    fn current(app_id: String) -> Self {
        let completion = crate::tx_lanes::with(|l| l.transaction_completion(&app_id))
            .expect("a claimed lane has an admission identity");
        Self {
            app_id,
            owner: crate::orm_context::current(),
            completion,
            armed: true,
        }
    }

    /// The reducer owns the release from here on.
    pub fn handed_to_reducer(mut self) {
        self.armed = false;
    }
}

impl Drop for TxAdmission {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.owner.with(|| {
            if !crate::tx_lanes::with(|l| self.completion.is_current_in(l, &self.app_id)) {
                return;
            }
            if driver::cancel_admission(&self.app_id, self.completion.clone()) {
                return;
            }
            // No reducer means BEGIN has not been dispatched by this claim.
            crate::tx_lanes::with_mut(|l| l.release_tx_claim(&self.app_id));
        });
    }
}

/// Transaction frame used by a native write that expands one creator call
/// into multiple SQL mutations.
///
/// A normally settled call outside `db.transaction()` owns a top-level
/// `BEGIN`/`COMMIT`; a call inside one owns a savepoint. A row failure is
/// returned only after that frame reports a successful rollback. Settlement
/// goes through [`exec_settle`], and therefore through the reducer - including
/// the PostgreSQL `COMMIT`-answered-with-`ROLLBACK` check the driver's terminal
/// classifier makes. Dropping an unsettled top-level frame transfers cleanup
/// to [`TxAdmission`], which retains ownership until the backend settles.
#[must_use = "an atomic write frame must be settled with finish"]
#[derive(Debug)]
pub struct AtomicWriteFrame {
    route: TxRoute,
    frame: Option<reducer::frames::FrameId>,
    state: AtomicWriteFrameState,
    /// Armed through the callback and savepoint settlement. Root terminal SQL
    /// transfers ownership to the driver's supervised settlement task.
    admission: Option<TxAdmission>,
}

#[derive(Debug, Clone, Copy)]
enum AtomicWriteFrameState {
    Open,
    Settling,
    Settled,
}

impl AtomicWriteFrame {
    /// Open the frame and promote the captured dispatch route onto it.
    pub async fn begin(route: TxRoute) -> Result<Self, DbError> {
        Self::begin_with_isolation(route, None).await
    }

    pub(crate) async fn begin_with_isolation(
        route: TxRoute,
        isolation_level: Option<zeroship_data_orm::error::IsolationLevel>,
    ) -> Result<Self, DbError> {
        route.check_scope()?;
        let nested = route.in_tx();
        let app_id = route.app_id().to_string();
        let schema = route.schema().clone();
        if nested && !crate::tx_lanes::with(|l| l.has_tx_for(&app_id)) {
            return Err(DbError::validation_hinted(
                "transaction_scope_expired",
                "the enclosing transaction has already settled".to_string(),
                "Run the mutation while its enclosing db.transaction callback is still open.",
            ));
        }
        // The depth cap is the frame stack's, not a second copy here.
        let admission = if nested {
            Some(TxAdmission::current(app_id.clone()))
        } else {
            Some(TxAdmission::acquire(app_id.clone()).await?)
        };

        let backend = route.backend().clone();
        match exec_begin_or_savepoint(nested, isolation_level, &app_id, schema, backend).await {
            Ok(frame) => Ok(Self {
                route: route.into_internal_transaction()?,
                frame,
                state: AtomicWriteFrameState::Open,
                admission,
            }),
            Err(error) => {
                // A returned refusal is handled by the caller. Only abandoned
                // nested work forces its enclosing transaction to end.
                if nested {
                    if let Some(admission) = admission {
                        admission.handed_to_reducer();
                    }
                }
                Err(error)
            }
        }
    }

    /// Route all statements and broker effects through this frame.
    pub fn route(&self) -> &TxRoute {
        &self.route
    }

    /// Commit/release a successful body or roll back a failed one.
    ///
    /// The body value is returned only after a confirmed successful settle.
    /// On rollback, the original row error remains the creator-visible error;
    /// a savepoint settle failure wins because the enclosing transaction state
    /// is then no longer trustworthy.
    ///
    /// The body's error type is the caller's, so a domain refusal that must
    /// roll back travels out of the frame as itself rather than through a side
    /// channel. Every failure the frame itself reports is converted into that
    /// type, which is what `E: From<DbError>` buys.
    pub async fn finish<T, E>(mut self, body: Result<T, E>) -> Result<T, E>
    where
        E: From<DbError>,
    {
        if self.admission.as_ref().is_some_and(|admission| {
            !admission.owner.with(|| {
                crate::tx_lanes::with(|l| admission.completion.is_current_in(l, &admission.app_id))
            })
        }) {
            return Err(E::from(scope::expired()));
        }
        let success = body.is_ok();
        // The reducer owns the admission release from here: every path it takes
        // to `Settled` emits `ReleaseAdmission`.
        if self.frame.is_none() {
            if let Some(admission) = self.admission.take() {
                admission.handed_to_reducer();
            }
        }
        self.state = AtomicWriteFrameState::Settling;
        let outcome = exec_settle(self.route.app_id(), success, self.frame).await;
        if let Some(admission) = self.admission.take() {
            admission.handed_to_reducer();
        }
        self.state = AtomicWriteFrameState::Settled;
        match (body, outcome) {
            (Ok(value), SettleOutcome::Ok) => Ok(value),
            (Err(error), SettleOutcome::Ok) => Err(error),
            (_, SettleOutcome::CommitIndeterminate(error)) => {
                Err(E::from(commit_failed_indeterminate(error)))
            }
            (_, SettleOutcome::SettleErr(error)) => Err(E::from(error)),
        }
    }
}

impl Drop for AtomicWriteFrame {
    fn drop(&mut self) {
        if !matches!(self.state, AtomicWriteFrameState::Open) {
            return;
        }

        // The admission guard transfers cancellation to the supervised driver.
        drop(self.admission.take());
    }
}

// ---------------------------------------------------------------------------
// exec_begin_or_savepoint — the async begin/savepoint SQL
// ---------------------------------------------------------------------------

/// Run `BEGIN [ISOLATION LEVEL ...]` (top-level) or `SAVEPOINT <frame>`
/// (nested), through the reducer.
///
/// Returns `Ok(None)` for a top-level begin, `Ok(Some(frame))` for the frame a
/// nested begin opened. The savepoint NAME never leaves the frame stack: the
/// caller settles by frame id, and the name the settle emits is the one that
/// frame minted.
///
/// `backend` is consumed only by the top-level arm - a SAVEPOINT runs on the
/// session the enclosing BEGIN already opened - but it is taken unconditionally
/// so the caller cannot be in the position of deciding whether one is needed.
pub async fn exec_begin_or_savepoint(
    nested: bool,
    isolation_level: Option<zeroship_data_orm::error::IsolationLevel>,
    app_id: &str,
    schema: crate::sql::SchemaName,
    backend: crate::backend::BackendHandle,
) -> Result<Option<reducer::frames::FrameId>, DbError> {
    if nested {
        if isolation_level.is_some() {
            return Err(DbError::validation(
                "nested_isolation_level",
                "db.transaction: savepoints inherit the enclosing transaction's isolation",
            ));
        }
        let driven = driver::open_frame(app_id).await;
        if let Some(refusal) = driven.refusal() {
            return Err(frame_refusal(refusal, driven.error));
        }
        let frame = driven.frame().ok_or_else(|| {
            driven.error.unwrap_or_else(|| {
                DbError::internal("db.transaction: SAVEPOINT did not open a frame")
            })
        })?;
        return Ok(Some(frame));
    }

    let driven = driver::begin_top_level(app_id, schema, isolation_level, backend).await?;
    let outcome = driven.outcome();
    match outcome {
        // Only the genuinely unclassified startup outcome gets the generic
        // code. SetupFailed, ReResolve, and Denied all carry the exact typed
        // DbError captured beside their BeginOutcome.
        Some(
            reducer::TerminalOutcome::Cancelled(reducer::CleanupCause::BeginFailed)
            | reducer::TerminalOutcome::Indeterminate(reducer::CleanupCause::BeginFailed),
        ) => {
            let error = driven.error.unwrap_or_else(|| {
                DbError::internal("db.transaction: BEGIN did not open a transaction")
            });
            if matches!(error, DbError::ValidationFailed { .. }) {
                return Err(error);
            }
            Err(DbError::Coded {
                code: "begin_failed".to_string(),
                message: format!("db.transaction: BEGIN failed: {}", error.message_str()),
                hint: None,
            })
        }
        Some(_) => Err(driven.error.unwrap_or_else(|| {
            DbError::internal("db.transaction: classified BEGIN failure carried no detail")
        })),
        None => match driven.refusal() {
            Some(refusal) => Err(driver::protocol_error(refusal, driven.error)),
            None => Ok(None),
        },
    }
}

/// Lower a frame guard's refusal to the creator-visible error.
///
/// `savepoint_depth_exceeded` keeps the hinted validation shape it has always
/// had; the frame stack is where the cap now lives, so this is the only place
/// that spells it.
fn frame_refusal(refusal: reducer::TxProtocolError, detail: Option<DbError>) -> DbError {
    if refusal
        == reducer::TxProtocolError::Frame(reducer::frames::FrameError::SavepointDepthExceeded)
    {
        return DbError::validation_hinted(
            "savepoint_depth_exceeded",
            format!(
                "db.transaction: nested transaction depth limit ({MAX_SAVEPOINT_DEPTH}) \
                 exceeded — flatten the nesting or split the work into separate transactions"
            ),
            "Each nested env.db.transaction(...) opens a SAVEPOINT; the cap guards against \
             runaway recursion.",
        );
    }
    driver::protocol_error(refusal, detail)
}

/// Run one creator data statement inside `app_id`'s open transaction.
///
/// Goes through the reducer's operation guard: the statement takes the session,
/// and its outcome is reported back. A statement that errors leaves the
/// transaction in `Poisoned`, which is where PostgreSQL has already put it -
/// every further data statement answers `25P02` until the block ends.
///
/// Savepoint statements do not come through here: they are frame events,
/// and the name they carry is the frame's.
///
/// See [`driver::run_operation`] for why the creator CRUD path has not moved
/// onto this yet.
#[allow(
    dead_code,
    reason = "the tests below are its only callers until exec.rs's in-transaction \
              arms move onto the reducer's operation guard"
)]
pub(crate) async fn run_on_tx_conn(app_id: &str, sql: &str) -> Result<(), DbError> {
    driver::run_operation(app_id, sql, &[]).await
}

// ---------------------------------------------------------------------------
// run_begin_continuation — mint view, call callback, attach handlers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// V8 handler callbacks — reclaim the finalizer, spawn the settle op
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// exec_settle — COMMIT / ROLLBACK / RELEASE / ROLLBACK TO
// ---------------------------------------------------------------------------

/// Result of the settle SQL, carrying enough to build the outer promise's
/// `ResolveValue`.
#[derive(Debug)]
pub enum SettleOutcome {
    /// Settle SQL succeeded.
    Ok,
    /// COMMIT failed after the body resolved — the tx state is
    /// indeterminate; reject with `commit_failed_indeterminate`.
    CommitIndeterminate(DbError),
    /// A rollback / release / rollback-to failed. The frame state is no
    /// longer trustworthy, so this error governs how the outer settles.
    SettleErr(DbError),
}

/// Run the settle for this transaction level, through the reducer.
///
/// Top-level (`frame == None`):
///   - success → `COMMIT`, dispose of the session, publish the queued effects.
///   - failure → `ROLLBACK`, dispose of the session, discard them.
///
/// Nested (`frame == Some(id)`):
///   - success → `RELEASE` (keeps the session open).
///   - failure → `ROLLBACK TO` **then** `RELEASE` (keeps it open; the enclosing
///     transaction continues). The second statement is not optional: `ROLLBACK
///     TO SAVEPOINT` leaves the savepoint defined, and leaving it there is what
///     a later frame of the same name would shadow.
///
/// The only state that ends a settle early is `Settled`; a settle that
/// arrives while an operation owns the session waits in `Quiescing` for it
/// to come back, and a session that is genuinely unreachable when terminal
/// SQL is due is `Indeterminate`, which withdraws and tells the creator. An
/// absent client is never proof that terminal SQL has run.
pub async fn exec_settle(
    app_id: &str,
    success: bool,
    frame: Option<reducer::frames::FrameId>,
) -> SettleOutcome {
    match frame {
        Some(frame) => {
            let close = if success {
                reducer::frames::FrameClose::Released
            } else {
                reducer::frames::FrameClose::RolledBackTo
            };
            let driven = driver::close_frame(app_id, frame, close).await;
            if driven.frame().is_some() {
                return SettleOutcome::Ok;
            }
            // A FAILED frame settle keeps the frame's events. The transaction
            // is poisoned and will not commit (a COMMIT on a failed tx is
            // answered `ROLLBACK`, which the driver's terminal classifier
            // detects), so nothing publishes either way - but the events are
            // the diagnostic evidence for why the close failed, and discarding
            // them before the statement ran destroyed exactly that.
            let refusal = driven.refusal();
            let error = driven
                .error
                .or_else(|| refusal.map(|r| driver::protocol_error(r, None)))
                .unwrap_or_else(|| DbError::internal("db.transaction: savepoint settle failed"));
            SettleOutcome::SettleErr(if success {
                savepoint_release_failed_indeterminate(error)
            } else {
                rollback_failed_indeterminate(error)
            })
        }
        None => {
            let intent = if success {
                reducer::SettleIntent::Commit
            } else {
                reducer::SettleIntent::Rollback
            };
            let driven = driver::settle_root(app_id, intent).await;
            let Some(outcome) = driven.outcome() else {
                let error = driven.refusal().map_or_else(
                    || DbError::internal("db.transaction: the settle produced no outcome"),
                    |refusal| driver::protocol_error(refusal, None),
                );
                return SettleOutcome::SettleErr(error);
            };
            // The driver's mapping already carries the right code for each
            // outcome, so nothing here re-wraps it: a known outcome must
            // never be reported as `commit_failed_indeterminate`.
            match driver::outcome_error(outcome, intent, driven.error) {
                None => SettleOutcome::Ok,
                Some(error) if matches!(outcome, reducer::TerminalOutcome::Indeterminate(_)) => {
                    SettleOutcome::CommitIndeterminate(error)
                }
                Some(error) => SettleOutcome::SettleErr(error),
            }
        }
    }
}

pub fn commit_failed_indeterminate(error: DbError) -> DbError {
    DbError::Coded {
        code: "commit_failed_indeterminate".to_string(),
        message: format!(
            "commit failed - transaction state indeterminate: {}",
            error.message_str()
        ),
        hint: None,
    }
}

fn rollback_failed_indeterminate(error: DbError) -> DbError {
    DbError::Coded {
        code: "rollback_failed_indeterminate".to_string(),
        message: format!(
            "rollback failed - transaction state indeterminate: {}",
            error.message_str()
        ),
        hint: None,
    }
}

fn savepoint_release_failed_indeterminate(error: DbError) -> DbError {
    DbError::Coded {
        code: "savepoint_release_failed_indeterminate".to_string(),
        message: format!(
            "savepoint release failed - transaction state indeterminate: {}",
            error.message_str()
        ),
        hint: None,
    }
}

/// Whether this app still has an active transaction frame in the host context.
pub fn is_active(app_id: &str) -> bool {
    crate::tx_lanes::with(|lanes| lanes.has_tx_for(app_id))
}

#[cfg(test)]
thread_local! {
    /// The backend `install_sqlite_backend_for_test` opened, for [`test_backend`].
    ///
    /// The fixture opens the backend, so the fixture holds it; the engine
    /// cannot name the adapter's per-isolate context.
    static TEST_BACKEND: std::cell::RefCell<Option<crate::backend::BackendHandle>> =
        const { std::cell::RefCell::new(None) };
}

/// The backend the tests below already installed, for the `begin` argument.
///
/// Sync, and it can be: only the COLD path needs to await, and every test here
/// opens a SQLite backend before it begins a transaction. Production resolves
/// through `tx_scope::ensure_backend`, which owns the cold arm.
/// The fixture schema every in-file transaction test opens against.
///
/// Spelled once so a test cannot accidentally open on a schema other than the
/// tenant it names - which is the shape this typing change exists to make
/// visible.
#[cfg(test)]
fn test_schema() -> crate::sql::SchemaName {
    crate::sql::SchemaName::new("app_sqlite").expect("fixture schema name")
}

#[cfg(test)]
fn test_backend() -> crate::backend::BackendHandle {
    TEST_BACKEND.with(|slot| {
        slot.borrow()
            .clone()
            .expect("test must install a backend before beginning a transaction")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // V8 settlement lowering is tested in the adapter crate.
    use std::path::PathBuf;
    use std::rc::Rc;

    use crate::backend::sqlite::SqliteBackend;
    use crate::tests::fixtures::DatabaseFixture;

    fn run<F: std::future::Future>(f: F) -> F::Output {
        compio::runtime::Runtime::new()
            .expect("compio runtime build")
            .block_on(f)
    }

    struct ContextReset;

    impl Drop for ContextReset {
        fn drop(&mut self) {
            crate::tx_lanes::with_mut(|l| {
                let _ = l.take_tx_client_for("app_sqlite");
                l.retire_transaction("app_sqlite");
                l.release_tx_claim("app_sqlite");
                l.clear_pending_emits_for("app_sqlite");
            });
            // The backend slot this clears is the fixture's own thread-local,
            // not the adapter's per-isolate pool: this crate cannot name that
            // one, and clearing it here was only ever a way to unpark a handle
            // the fixture itself had parked.
            super::TEST_BACKEND.with(|slot| slot.borrow_mut().take());
        }
    }

    fn install_sqlite_backend_for_test() -> (Rc<SqliteBackend>, tempfile::TempDir, ContextReset) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let backend = Rc::new(
            crate::backend_selection::new_sqlite_backend(
                PathBuf::from(dir.path()),
                crate::encryption::ProjectKeySource::unavailable(),
            )
            .expect("open sqlite backend"),
        );
        let reset = ContextReset;
        crate::tx_lanes::with_mut(|l| {
            let _ = l.take_tx_client_for("app_sqlite");
            l.retire_transaction("app_sqlite");
            l.release_tx_claim("app_sqlite");
            l.clear_pending_emits_for("app_sqlite");
        });
        super::TEST_BACKEND.with(|slot| {
            *slot.borrow_mut() = Some(crate::backend::BackendHandle::new(Rc::clone(&backend)));
        });
        (backend, dir, reset)
    }

    /// **Dispatch emits the reducer's monotonic names, not depth-derived ones.**
    ///
    /// Savepoint names must be unique per frame: `ROLLBACK TO SAVEPOINT`
    /// leaves the savepoint defined, and PostgreSQL resolves a name to the
    /// most recently established one, so a reused name would send an
    /// enclosing frame's rollback to the wrong scope.
    ///
    /// The arm drives the real dispatch entry point, not the frame stack: it
    /// opens a frame, settles it, opens another at the same depth, and
    /// requires the two minted names to differ.
    #[test]
    fn dispatch_emits_monotonic_savepoint_names_at_the_same_depth() {
        run(async {
            let (backend, _dir, _reset) = install_sqlite_backend_for_test();
            let probe = backend.autocommit_client();
            backend
                .execute_fixture_on(&probe, "CREATE TABLE notes (id INTEGER PRIMARY KEY)", &[])
                .await
                .expect("create table");

            exec_begin_or_savepoint(false, None, "app_sqlite", test_schema(), test_backend())
                .await
                .expect("begin");

            let first =
                exec_begin_or_savepoint(true, None, "app_sqlite", test_schema(), test_backend())
                    .await
                    .expect("first nested frame")
                    .expect("a nested begin opens a frame");
            // Roll it back, which on PostgreSQL leaves the savepoint defined -
            // the precondition that makes a reused name dangerous.
            match exec_settle("app_sqlite", false, Some(first)).await {
                SettleOutcome::Ok => {}
                other => panic!("expected Ok for the first frame settle, got {other:?}"),
            }

            let second =
                exec_begin_or_savepoint(true, None, "app_sqlite", test_schema(), test_backend())
                    .await
                    .expect("second nested frame")
                    .expect("a nested begin opens a frame");
            assert_ne!(
                first, second,
                "a frame id is minted from a monotonic sequence and never reused"
            );

            let names = crate::tx_lanes::with(|l| {
                l.transaction_reducer("app_sqlite")
                    .expect("the transaction is still open")
                    .frames()
                    .minted_names()
                    .clone()
            });
            assert_eq!(
                names.len(),
                2,
                "two frames at the same depth must mint two DISTINCT names; a \
                 depth-derived scheme mints one name twice and the set collapses \
                 to a single entry. got {names:?}"
            );
            assert!(
                names.iter().all(|name| name.starts_with("zs_sp_")),
                "the zs_ prefix keeps the name out of any plausible user-chosen \
                 savepoint namespace; got {names:?}"
            );

            match exec_settle("app_sqlite", false, Some(second)).await {
                SettleOutcome::Ok => {}
                other => panic!("expected Ok for the second frame settle, got {other:?}"),
            }
            let _ = exec_settle("app_sqlite", false, None).await;
        });
    }

    #[test]
    fn max_savepoint_depth_is_eight() {
        // Pin the proposal's depth cap so a future change is a deliberate
        // edit, not an accidental drift.
        assert_eq!(MAX_SAVEPOINT_DEPTH, 8);
    }

    /// `driver::outcome_error` names each outcome distinctly.
    ///
    /// A commit the server rolled back is NOT indeterminate: its outcome is
    /// known and its writes are gone. Collapsing the two hides a definite
    /// failure behind a retryable-looking one.
    #[test]
    fn outcome_errors_do_not_collapse_rolled_back_into_indeterminate() {
        use reducer::{CleanupCause, SettleIntent, TerminalOutcome};

        fn code_of(error: &DbError) -> &str {
            match error {
                DbError::Coded { code, .. } => code,
                other => panic!("expected a coded error, got {other:?}"),
            }
        }

        assert!(
            driver::outcome_error(TerminalOutcome::Committed, SettleIntent::Commit, None).is_none(),
            "a confirmed commit is not an error"
        );
        let rolled_back =
            driver::outcome_error(TerminalOutcome::RolledBack, SettleIntent::Commit, None)
                .expect("a COMMIT answered ROLLBACK is a failure");
        let indeterminate = driver::outcome_error(
            TerminalOutcome::Indeterminate(CleanupCause::BackendHealthUnknown),
            SettleIntent::Commit,
            None,
        )
        .expect("an unproved terminal is a failure");
        assert_eq!(code_of(&rolled_back), "commit_rolled_back");
        assert_eq!(code_of(&indeterminate), "commit_failed_indeterminate");
        assert_ne!(
            code_of(&rolled_back),
            code_of(&indeterminate),
            "a definite failure must not be reported under the code for an \
             unknown one - the observable difference would migrate into retry \
             timing, which every caller can measure and none can act on"
        );

        // Every cleanup cause reaches the creator under its OWN code, so a
        // caller can tell a deadline from a denial from a cancel.
        for cause in [
            CleanupCause::Cancelled,
            CleanupCause::Detached,
            CleanupCause::EpochChanged,
            CleanupCause::BeginFailed,
        ] {
            let error = driver::outcome_error(
                TerminalOutcome::Cancelled(cause),
                SettleIntent::Commit,
                None,
            )
            .expect("a cancelled transaction is an error to the caller");
            assert_eq!(code_of(&error), cause.code());
        }
    }

    /// The probe reads and seeds through `op_conn`; writes that belong to the
    /// transaction go through [`run_on_tx_conn`], which is the only path onto
    /// `tx_conn`.
    ///
    /// Before SC-2 these tests used `fixture_session()` for the probe
    /// AND drove the transaction's writes through it, which worked only
    /// because both were the same single connection. That coupling is the
    /// divergence SC-2 retires, so the probe now names the lane it wants.
    #[test]
    fn sqlite_top_level_begin_accepts_serializable_and_commits() {
        run(async {
            let (backend, _dir, _reset) = install_sqlite_backend_for_test();
            let probe = backend.autocommit_client();
            backend
                .execute_fixture_on(
                    &probe,
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                    &[],
                )
                .await
                .expect("create table");

            exec_begin_or_savepoint(
                false,
                Some(zeroship_data_orm::error::IsolationLevel::Serializable),
                "app_sqlite",
                test_schema(),
                test_backend(),
            )
            .await
            .expect("begin sqlite tx");
            run_on_tx_conn("app_sqlite", "INSERT INTO notes (title) VALUES ('kept')")
                .await
                .expect("insert inside sqlite tx");

            match exec_settle("app_sqlite", true, None).await {
                SettleOutcome::Ok => {}
                other => panic!("expected Ok settle outcome, got {other:?}"),
            }

            let rows = probe
                .query("SELECT COUNT(*) FROM notes", &[])
                .await
                .expect("count notes after commit");
            assert_eq!(rows[0][0].as_deref(), Some("1"));
            assert!(!crate::tx_lanes::with(|l| l.has_tx_for("app_sqlite")));
        });
    }

    /// **An open transaction survives the thread's backend being cleared.**
    ///
    /// `ThreadDbContext::clear_pool` nulls `backend` and leaves `tx_conns`
    /// untouched, so a `register` with a changed URL puts the thread in a state
    /// where a live pinned session exists and the ambient backend does not.
    /// The pinned [`crate::driver::Session`] remains the authority for commands
    /// and settlement; neither path consults the ambient backend.
    #[test]
    fn an_open_transaction_outlives_the_threads_backend_being_cleared() {
        run(async {
            let (backend, _dir, _reset) = install_sqlite_backend_for_test();
            // Taken from the backend we own, not from the context, so it stays
            // usable as an oracle after the context's handle is dropped.
            let probe = backend.autocommit_client();
            backend
                .execute_fixture_on(
                    &probe,
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                    &[],
                )
                .await
                .expect("create table");

            exec_begin_or_savepoint(false, None, "app_sqlite", test_schema(), test_backend())
                .await
                .expect("begin sqlite tx");
            run_on_tx_conn("app_sqlite", "INSERT INTO notes (title) VALUES ('before')")
                .await
                .expect("insert before the backend is cleared");

            // The window this test exists for: ambient backend gone, pinned
            // session still open and still perfectly usable. The slot cleared
            // is the fixture's, which is where this crate's tests park a
            // backend now - the adapter's per-isolate pool is out of reach and
            // was never what `run_on_tx_conn` reads.
            super::TEST_BACKEND.with(|slot| slot.borrow_mut().take());
            assert!(crate::tx_lanes::with(|l| l.has_tx_for("app_sqlite")));

            run_on_tx_conn("app_sqlite", "INSERT INTO notes (title) VALUES ('after')")
                .await
                .expect("insert after the backend is cleared");

            match exec_settle("app_sqlite", true, None).await {
                SettleOutcome::Ok => {}
                other => panic!("expected Ok settle outcome, got {other:?}"),
            }

            let rows = probe
                .query("SELECT COUNT(*) FROM notes", &[])
                .await
                .expect("count notes after commit");
            assert_eq!(rows[0][0].as_deref(), Some("2"));
            assert!(!crate::tx_lanes::with(|l| l.has_tx_for("app_sqlite")));
        });
    }

    #[test]
    fn sqlite_top_level_reject_path_actively_rolls_back() {
        run(async {
            let (backend, _dir, _reset) = install_sqlite_backend_for_test();
            let probe = backend.autocommit_client();
            backend
                .execute_fixture_on(
                    &probe,
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                    &[],
                )
                .await
                .expect("create table");

            exec_begin_or_savepoint(false, None, "app_sqlite", test_schema(), test_backend())
                .await
                .expect("begin sqlite tx");
            run_on_tx_conn(
                "app_sqlite",
                "INSERT INTO notes (title) VALUES ('rolled-back')",
            )
            .await
            .expect("insert inside sqlite tx");

            match exec_settle("app_sqlite", false, None).await {
                SettleOutcome::Ok => {}
                other => panic!("expected Ok settle outcome, got {other:?}"),
            }

            let rows = probe
                .query("SELECT COUNT(*) FROM notes", &[])
                .await
                .expect("count notes after rollback");
            assert_eq!(rows[0][0].as_deref(), Some("0"));
            assert!(!crate::tx_lanes::with(|l| l.has_tx_for("app_sqlite")));
        });
    }

    /// **Forced cleanup reaches an unreachable SQLite session through SC-2's
    /// canceller, and rolls it back rather than giving up on it.**
    ///
    /// The PostgreSQL arm of this property is
    /// `a_forced_cleanup_cancels_the_running_statement_and_keeps_the_connection`
    /// in `crates/zeroship-data-v8/src/tests/postgres/transactions.rs`; this is its dev-tier peer, and it is
    /// here rather than there because it needs no server.
    ///
    /// The two backends differ in how much a cancellation accomplishes, and this
    /// arm exists to hold that difference to the assertion rather than to a
    /// comment: SQLite's actor answers a `Cancel` only after it has rolled back
    /// and retired the reservation, so there is nothing to reclaim and no
    /// `ROLLBACK` for the driver to issue afterwards. `SqliteCancelHandle::cancel`
    /// carried a `// no production canceller yet` allow until this change; this
    /// is the caller that made it false.
    ///
    /// The session is taken out of the slot with a bare
    /// [`crate::tx_lanes::TxClientSlotGuard`], which is what ordinary CRUD does
    /// (the module header's "known gap"), so the reducer still reports
    /// `SessionOwnership::Registry`. That is the production shape today, not a
    /// contrived one.
    ///
    /// **The row count is the load-bearing assertion.** An outcome of
    /// `Cancelled` only means the driver believed a rollback happened; the
    /// probe reading `0` on a SEPARATE connection is the actor having actually
    /// done it.
    ///
    /// **Mutation that reddens this arm:** in `driver::cleanup`, answer the
    /// empty-slot `Registry`/`Command` case with `CleanupAck::Indeterminate`
    /// instead of `cancel_and_reclaim(..)`. The outcome becomes
    /// `Indeterminate(Cancelled)` and the session is withdrawn.
    #[test]
    fn a_forced_cleanup_cancels_an_unreachable_sqlite_session() {
        run(async {
            let (backend, _dir, _reset) = install_sqlite_backend_for_test();
            let probe = backend.autocommit_client();
            backend
                .execute_fixture_on(
                    &probe,
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                    &[],
                )
                .await
                .expect("create table");

            exec_begin_or_savepoint(false, None, "app_sqlite", test_schema(), test_backend())
                .await
                .expect("begin sqlite tx");
            run_on_tx_conn("app_sqlite", "INSERT INTO notes (title) VALUES ('doomed')")
                .await
                .expect("insert inside sqlite tx");

            // Another future owns the session. Forced cleanup cannot take it out
            // of the slot, so the only route left is the canceller captured when
            // the session was installed.
            let held =
                crate::tx_lanes::TxClientSlotGuard::take("app_sqlite").expect("hold the session");

            let driven = driver::cancel("app_sqlite").await;
            assert_eq!(
                driven.outcome(),
                Some(reducer::TerminalOutcome::Cancelled(
                    reducer::CleanupCause::Cancelled
                )),
                "the actor rolls back and retires the reservation before it \
                 acknowledges, so the cleanup goal is PROVED - answering \
                 Indeterminate here is what used to abandon a live session"
            );
            assert!(
                !crate::tx_lanes::with(|l| l.tx_session_withdrawn("app_sqlite")),
                "a proved cleanup withdraws nothing"
            );

            drop(held);

            let rows = probe
                .query("SELECT COUNT(*) FROM notes", &[])
                .await
                .expect("count notes after the forced cleanup");
            assert_eq!(
                rows[0][0].as_deref(),
                Some("0"),
                "the cancellation must have really rolled the transaction back, \
                 not merely reported that it did"
            );
        });
    }

    /// **The divergence SC-2 Decision 1 retires**, stated as an assertion.
    ///
    /// `tx_route.rs` used to carry: "SQLite runs the whole app on one
    /// connection ... so a correctly pool-routed write still executes inside
    /// whatever transaction that connection is holding." That is what this
    /// test measures: an autocommit write issued while the app's OWN explicit
    /// transaction is open, followed by that transaction's `ROLLBACK`.
    ///
    /// On one connection the autocommit row is destroyed - the pre-SC-2 tree
    /// leaves `0` rows. With `op_conn` and `tx_conn` split it survives and the
    /// transaction's own row does not.
    ///
    /// It says autocommit **write** and issues it before the transaction takes
    /// the write lock, deliberately. WAL gives concurrent readers, not
    /// concurrent writers: an autocommit write racing a `tx_conn` that already
    /// holds the write lock still waits out `busy_timeout`, and no number of
    /// connections changes that.
    #[test]
    fn an_autocommit_write_survives_the_apps_own_transaction_rollback() {
        run(async {
            let (backend, _dir, _reset) = install_sqlite_backend_for_test();
            let probe = backend.autocommit_client();
            backend
                .execute_fixture_on(
                    &probe,
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                    &[],
                )
                .await
                .expect("create table");

            exec_begin_or_savepoint(false, None, "app_sqlite", test_schema(), test_backend())
                .await
                .expect("begin sqlite tx");

            // No transaction anywhere in this call's async scope.
            backend
                .execute_fixture_on(
                    &probe,
                    "INSERT INTO notes (title) VALUES ('autocommit')",
                    &[],
                )
                .await
                .expect("autocommit insert while a transaction is open");

            run_on_tx_conn("app_sqlite", "INSERT INTO notes (title) VALUES ('doomed')")
                .await
                .expect("insert inside sqlite tx");

            match exec_settle("app_sqlite", false, None).await {
                SettleOutcome::Ok => {}
                other => panic!("expected Ok settle outcome, got {other:?}"),
            }

            let rows = probe
                .query("SELECT title FROM notes ORDER BY id", &[])
                .await
                .expect("read notes after rollback");
            assert_eq!(
                rows.len(),
                1,
                "the autocommit write must survive the transaction's ROLLBACK \
                 and the transaction's own write must not; got {rows:?}"
            );
            assert_eq!(rows[0][0].as_deref(), Some("autocommit"));
        });
    }

    #[test]
    fn dropped_atomic_write_frame_rolls_back_top_level_sqlite() {
        run(async {
            let (backend, _dir, _reset) = install_sqlite_backend_for_test();
            let probe = backend.autocommit_client();
            backend
                .execute_fixture_on(
                    &probe,
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                    &[],
                )
                .await
                .expect("create table");

            let frame = AtomicWriteFrame::begin(crate::exec::ambient_route_for_tests(
                "app_sqlite",
                test_backend(),
            ))
            .await
            .expect("begin atomic write frame");
            run_on_tx_conn(
                "app_sqlite",
                "INSERT INTO notes (title) VALUES ('must-rollback')",
            )
            .await
            .expect("insert inside atomic write frame");
            drop(frame);

            let rows = probe
                .query("SELECT COUNT(*) FROM notes", &[])
                .await
                .expect("count notes after frame cancellation");
            assert_eq!(
                rows[0][0].as_deref(),
                Some("0"),
                "dropping an unsettled frame must roll back its writes"
            );
            let next = compio::time::timeout(
                std::time::Duration::from_secs(1),
                AtomicWriteFrame::begin(
                    crate::tx_route::CapturedRoute::pool_for_tests(
                        "app_sqlite",
                        test_backend().sql_registration().clone(),
                    )
                    .bind(test_backend())
                    .unwrap(),
                ),
            )
            .await
            .expect("supervised cleanup must release the transaction lane")
            .expect("begin after cleanup");
            next.finish(Ok::<_, DbError>(()))
                .await
                .expect("settle replacement");
            assert!(!crate::tx_lanes::with(|l| {
                l.has_tx_for("app_sqlite") || l.tx_claimed_by("app_sqlite")
            }));
        });
    }
}
