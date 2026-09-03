//! The per-thread transaction lane: one open transaction per app, its parked
//! session, and the tombstones that outlive it.
//!
//! # Why this is a file rather than a region of `context.rs`
//!
//! `docs/proposals/2026-08-31-data-crate-shape.md` puts the transaction lane in
//! `zeroship-data-engine`, and `context.rs` in the adapter as the composition
//! root. These four types were the last engine-bound things still embedded in
//! an adapter file, so extracting them turns that crate cut from surgery inside
//! a 2,000-line file into a file move.
//!
//! # Why it may name both vendors
//!
//! [`TxConnection`] holds `OwnedPooledClient` on one arm and
//! `SqliteSessionHandle` on the other, which looks like a vendor-embedding
//! violation and is not: `data-engine` sits ABOVE both vendor crates and is the
//! tier that dispatches between them. The proposal says exactly that of
//! `BackendHandle` - "go to `data-engine`, above them" - and this is the same
//! shape. What the engine may not contain is SQL or dialect knowledge; see #164
//! for why decision 4's "ZERO changes" wording overstates that.
//!
//! # SEC-1
//!
//! A worker OS thread multiplexes up to ~200 isolates, and a creator's
//! `env.db.transaction(async () => await fetch(slow))` parks its session here
//! across the `await`. Every entry point is keyed by `app_id`, which is what
//! makes app A's lane invisible and untakable for co-resident app B.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use compio_postgres::OwnedPooledClient;

use crate::backend::sqlite::session::SqliteSessionHandle;
use zeroship_core::change_event::ChangeEvent;
use zeroship_data_core::error::{CleanupAck, DbError, SettleIntent, TerminalResult};

/// Everything true of one app's open transaction.
///
/// Created by `try_claim_tx` and dropped by `release_tx_claim`, which is the
/// bracket the old `tx_claims` set stood for. Between those two points the
/// reducer is admitted, a session is installed and taken and returned, a
/// canceller is recorded, savepoints mark the emit queue, and the queue is
/// drained or discarded - all of it keyed by one `app_id`, all of it ending
/// together.
///
/// Every field is `Option` or a collection with a meaningful empty state,
/// because a lane exists before it has any of them: `try_claim_tx` wins the
/// race first and `BEGIN` runs after.
#[derive(Default)]
pub(crate) struct TxLane {
    /// The SC-1 state machine. `None` between the claim and `admit_transaction`.
    reducer: Option<crate::transaction::reducer::TxReducer>,

    /// The pinned session, or `None` while a `TxClientSlotGuard` holds it out
    /// of the lane across an await. "In transaction" is the lane existing, not
    /// this being `Some`.
    session: Option<TxConnection>,

    /// How to reach the session out-of-band. Captured at install, because the
    /// one moment forced cleanup needs it is the one moment it cannot borrow
    /// the session.
    canceller: Option<crate::backend::cancel::TxCanceller>,

    /// Parked on this lane ending, so a second top-level `transaction()` for
    /// the same app waits instead of racing.
    claim_waiters: Vec<std::task::Waker>,

    /// Parked on [`Self::session`] refilling. Forced cleanup waits here for a
    /// cancelled statement's holder to give the session back.
    slot_waiters: Vec<std::task::Waker>,

    /// One watermark per open savepoint: the emit-queue length when the frame
    /// opened, so `ROLLBACK TO` truncates to it.
    emit_marks: Vec<usize>,

    /// Change events queued by this transaction, fired on COMMIT and discarded
    /// on ROLLBACK. Only fills inside a transaction - the autocommit path emits
    /// directly.
    pending_emits: Vec<ChangeEvent>,
}

/// **A lane cannot outlive its session, and the disposition is destroy.**
///
/// Removing a lane drops whatever is still parked in it, and for PostgreSQL a
/// plain drop is the WRONG disposition: `OwnedPooledClient::drop` returns the
/// lease to the pool, which would publish a connection still inside its
/// transaction block to the next borrower - on a worker thread that multiplexes
/// co-resident apps, potentially a different tenant's.
///
/// Every settle path disposes of the session before the lane is released, so in
/// practice this fires only on the paths that do not: a teardown, or a lane torn
/// down out from under a holder. Making it structural rather than a rule every
/// caller must remember is the point - see [`destroy_tx_connection`] for what
/// "destroy" costs and why a drop does not achieve it.
impl TxLane {
    /// The queued pre-commit events, for the SEC-1 scoping tests in
    /// `context.rs`. A slice rather than the field, so the tests can read the
    /// queue without production code gaining the ability to mutate a lane's
    /// internals from another module.
    #[cfg(test)]
    pub(crate) fn pending_emits(&self) -> &[ChangeEvent] {
        &self.pending_emits
    }
}

impl Drop for TxLane {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            destroy_tx_connection(session);
        }
    }
}


/// Pinned transaction client parked in the app-keyed, per-thread tx map.
///
/// Postgres keeps a dedicated libpq connection alive for the lifetime of
/// the transaction; SQLite keeps a handle to the shared session actor and
/// drives `BEGIN` / `SAVEPOINT` / `COMMIT` / `ROLLBACK` over that single
/// worker-owned connection.
// `large_enum_variant` fires here as of 2026-08-27: main's compio-postgres work
// grew `Client` to at least 232 bytes against the SQLite handle's 8, and clippy
// was clean on this crate immediately before that merge.
//
// NOT boxed, deliberately. The lint assumes the enum is stored in bulk, where
// the padding multiplies. This one lives in the per-app transaction map at one
// entry per app with an OPEN transaction, and concurrent transactions are
// bounded by the data pool's 8 connections - so the whole population is under
// 2 KB per thread. Boxing would buy that back at the cost of a heap allocation
// on every transaction begin and a pointer chase on every operation inside it,
// which is the wrong trade on the hot path.
//
// What would change the answer: if `TxConnection` ever becomes something held
// per operation, per row, or in a collection that scales with apps rather than
// with open transactions, box it - the lint's assumption would then be true.
#[allow(clippy::large_enum_variant)]
pub(crate) enum TxConnection {
    Postgres(OwnedPooledClient),
    Sqlite(SqliteSessionHandle),
}

impl TxConnection {
    /// Execute a control statement (`BEGIN`, `SAVEPOINT`, `RELEASE`,
    /// `ROLLBACK TO`) on this pinned transaction session.
    ///
    /// **Terminal statements do NOT come through here.** A terminal statement's
    /// command tag is not cosmetic - PostgreSQL answers `COMMIT` with the tag
    /// `ROLLBACK` when the transaction is in a failed state, and this path
    /// returns a row count, which throws the tag away.
    /// [`crate::transaction::driver`]'s `terminal` reads the tag and classifies
    /// the three-way `TerminalResult` the state machine needs.
    ///
    /// **This takes no backend, and that is the point.** Each variant holds
    /// exactly the `SqlExecutor::Client` of the backend that produced it -
    /// `PostgresBackend::Client = OwnedPooledClient`, `SqliteBackend::Client =
    /// SqliteSessionHandle` - so the variant already names the vendor. Passing a
    /// `BackendHandle` alongside and matching on the pair, as this operation did
    /// until 2026-09-02, re-proved that pairing at runtime and needed an
    /// unreachable "backend/client mismatch" arm to be total. Neither backend's
    /// `client_exec` reads `&self`, so the second half carried no information.
    pub(crate) async fn exec(&self, sql: &str, params: &[&str]) -> Result<u64, DbError> {
        match self {
            Self::Postgres(client) => {
                let rows = client
                    .query_text_params(sql, params)
                    .await
                    .map_err(|e| crate::backend::pg_error::classify(&e))?;
                Ok(rows.len() as u64)
            }
            Self::Sqlite(handle) => handle.exec(sql, params).await,
        }
    }

    /// Send terminal SQL and report what the backend actually did.
    ///
    /// **The vendor decides what happened; SC-1 decides what it means.** Each
    /// arm answers with its own projection - `postgres::terminal_from_tag` /
    /// `terminal_from_status`, `reservation::terminal_result` - so the protocol
    /// never reads a command tag or a rusqlite outcome itself.
    ///
    /// **The vendor is read off the pinned session, never off the thread's
    /// ambient backend.** The pair really can disagree: a `register` with a
    /// changed URL calls [`ThreadDbContext::clear_pool`], which nulls `backend`
    /// and leaves the lanes untouched. When it did, an open transaction on a
    /// live connection settled `Indeterminate` and had its session withdrawn.
    /// The variants ARE the two `SqlExecutor::Client` associated types, so the
    /// session is the authority on how to talk to itself.
    pub(crate) async fn settle(
        &self,
        intent: SettleIntent,
    ) -> (TerminalResult, Option<DbError>) {
        match self {
            Self::Postgres(pg) => match pg.batch_execute_reporting_tag(intent.verb()).await {
                Ok(tag) => (
                    crate::backend::postgres::terminal_from_tag(intent, tag.as_deref()),
                    None,
                ),
                // Sample the status AFTER the statement answered, never before
                // it: inside a poisoned block no retry makes the oracle speak.
                Err(error) => (
                    crate::backend::postgres::terminal_from_status(pg.transaction_status()),
                    Some(crate::backend::pg_error::classify(&error)),
                ),
            },
            Self::Sqlite(handle) => {
                use crate::backend::sqlite::session::TerminalIntent;
                let sqlite_intent = match intent {
                    SettleIntent::Commit => TerminalIntent::Commit,
                    SettleIntent::Rollback => TerminalIntent::Rollback,
                };
                match handle.settle(sqlite_intent).await {
                    Ok(outcome) => {
                        crate::backend::sqlite::reservation::terminal_result(&outcome)
                    }
                    Err(error) => (TerminalResult::Indeterminate, Some(error)),
                }
            }
        }
    }

    /// Roll this session back on SC-1's behalf and report what that PROVED.
    ///
    /// Distinct from [`Self::settle`], and not a special case of it: settle
    /// performs the creator's intent and reports the outcome, while cleanup is
    /// forced and reports only what the backend's health oracle can establish.
    /// Each backend samples that oracle its own way and in an order that is
    /// load-bearing - see the two implementations - so the choice of evidence
    /// stays with the vendor and only the three-way verdict crosses back.
    pub(crate) async fn cleanup(&self) -> CleanupAck {
        match self {
            Self::Postgres(pg) => crate::backend::postgres::cleanup(pg).await,
            Self::Sqlite(handle) => crate::backend::sqlite::reservation::cleanup(handle).await,
        }
    }
}

/// Destroy a transaction session's physical connection instead of returning it.
///
/// **This is what SC-1's `WithdrawSession` means, and a plain `drop` is not it.**
/// `OwnedPooledClient::drop` calls `pool.return_client(entry)`, which
/// republishes the lease as idle - so dropping a withdrawn session hands the
/// next borrower exactly the connection the protocol withdrew. Closing the
/// client's request channel first makes `PoolEntry::is_pool_eligible` false (it
/// tests `!client.is_closed()`), and `return_client` then evicts the entry and
/// releases its capacity slot rather than publishing it. The same
/// close-before-drop idiom is what `backend::lock_guard::LockGuard::drop` uses
/// to terminate a session whose advisory lock it could not release.
///
/// SQLite has no pool to return to - the handle is an `Rc` clone of the single
/// writer actor - so the withdrawal is the best-effort detached `ROLLBACK` that
/// stops the actor holding the transaction open. Dropping the handle alone does
/// not touch the live transaction on the worker thread.
pub(crate) fn destroy_tx_connection(client: TxConnection) {
    match client {
        TxConnection::Postgres(mut client) => {
            client.__private_api_close();
            drop(client);
        }
        TxConnection::Sqlite(handle) => {
            if let Err(error) = handle.try_exec_detached("ROLLBACK", &[]) {
                tracing::warn!(
                    error = %error,
                    "sc1: withdrawing a SQLite transaction session could not enqueue its \
                     fallback ROLLBACK; the actor may hold the transaction until it is reaped"
                );
            }
            drop(handle);
        }
    }
}

/// RAII guard for a transaction client temporarily removed from the
/// per-thread map.
///
/// SQLite needs this guard to stay cancellation-safe: dropping a future
/// mid-await must restore the session-actor handle back into
/// `tx_conn`, otherwise the actor keeps the transaction open while the
/// isolate state claims there is no live tx. Restoring the slot is also
/// harmless on Postgres and keeps the `take` / `put` contract in one
/// typed place.
#[must_use = "TxClientSlotGuard restores the tx slot on Drop unless consumed via into_inner()"]
pub(crate) struct TxClientSlotGuard {
    app_id: String,
    client: Option<TxConnection>,
}

impl TxClientSlotGuard {
    /// Drain `app_id`'s transaction client out of the per-thread map.
    /// SEC-1: the guard restores it to the *same* app's slot on drop, so
    /// a cancellation mid-await can never re-park one app's client under
    /// another's key.
    pub(crate) fn take(app_id: &str) -> Result<Self, DbError> {
        let client = crate::tx_lanes::with_mut(|l| l.take_tx_client_for(app_id))
            .ok_or_else(|| DbError::internal("db: transaction connection lost"))?;
        Ok(Self {
            app_id: app_id.to_string(),
            client: Some(client),
        })
    }

    /// Borrow the pinned client while the guard owns restoration.
    pub(crate) fn client(&self) -> &TxConnection {
        self.client
            .as_ref()
            .expect("TxClientSlotGuard::client called after drop-state transition")
    }
}

impl Drop for TxClientSlotGuard {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            let app_id = std::mem::take(&mut self.app_id);
            crate::tx_lanes::with_mut(|l| l.put_tx_client_for(&app_id, client));
        }
    }
}


/// Every app's open transaction on this worker thread, plus the withdrawal
/// tombstones that must outlive a lane.
///
/// # Why this is one owner, and its own type
///
/// `docs/proposals/2026-09-02-thread-context-ownership.md` calls `context.rs`
/// four owners wearing one struct, and this is the largest. Measured
/// 2026-09-02: 25 methods touch `lanes` and `withdrawn_tx_sessions` and NOTHING
/// else on `ThreadDbContext` - no pool, no descriptor cache, no generation
/// counter. They were separable as a unit, and the separation is what lets the
/// engine tier move to its own crate without dragging the adapter's
/// per-isolate state along.
///
/// The bodies below moved BYTE-FOR-BYTE. That was possible because the field
/// names are unchanged, so every `self.lanes` / `self.withdrawn_tx_sessions`
/// still resolves - now to this struct. Nothing was rewritten, which is the
/// property that made it safe to do to the transaction hot path.
///
/// # SEC-1 is the reason for the key, and it is unchanged
///
/// A worker OS thread multiplexes up to ~200 isolates, and a creator's
/// `env.db.transaction(async () => await fetch(slow))` parks its session here
/// across the `await`. Keying by `app_id` makes app A's lane invisible and
/// untouchable to co-resident app B, which would otherwise run B's SQL inside
/// A's transaction, snapshot and per-app role.
///
/// # The tombstone is beside the lanes, not inside them
///
/// `withdrawn_tx_sessions` marks a session the protocol condemned. SC-1
/// withdraws a session and retires its lane in the same settle, but the session
/// can be out of the slot in another future's hands, and that future's `Drop`
/// returns it afterwards - to a lane that no longer exists. A flag inside
/// [`TxLane`] would die with the lane and let a condemned session reach the
/// pool, and from there the next borrower. A tombstone cannot live inside the
/// thing it is a tombstone for.
#[derive(Default)]
pub(crate) struct TxLanes {
    /// Every app's open transaction, one entry each.
    ///
    /// **The entry IS the claim.** This map replaced nine parallel
    /// `HashMap<String, _>` on 2026-09-02 - the session, the reducer, the
    /// canceller, the withdrawal tombstone, two waiter lists, the savepoint
    /// emit marks, the pending-emit queue, and a `tx_claims: HashSet<String>`
    /// that existed only to say "one of these is in flight". They were created
    /// together, mutated together and destroyed together, so they were one
    /// entity written nine ways.
    lanes: HashMap<String, TxLane>,

    /// Apps whose transaction session was withdrawn: anything returning to the
    /// slot is destroyed rather than parked. Cleared by
    /// [`Self::admit_transaction`], never by retirement - the tombstone belongs
    /// to the withdrawn session, not to the app.
    withdrawn_tx_sessions: HashSet<String>,
}

impl TxLanes {
    /// An empty lane set. A fresh worker thread has no open transaction.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The lane map, for the SEC-1 scoping tests in `context.rs`.
    ///
    /// Named `by_app` rather than exposing the field so the call reads
    /// `ctx.lanes.by_app()` instead of `ctx.lanes.lanes` - and so the field
    /// itself stays private, keeping the 25 forwarding methods the whole lane
    /// surface.
    #[cfg(test)]
    pub(crate) fn by_app(&self) -> &HashMap<String, TxLane> {
        &self.lanes
    }

    // ----- TX_CONN / SAVEPOINT_DEPTH ------

    /// `true` if a transaction connection is currently parked **for
    /// `app_id`** (`tx_conns[app_id] = Some`). Returns `true` even
    /// between an in-flight take/return on the same tx client
    /// ([`Self::take_tx_client_for`] → [`Self::put_tx_client_for`]),
    /// because callers wrap the await in those two calls and the slot is
    /// conceptually still "active".
    ///
    /// SEC-1: a parked tx owned by another app reads as `false` here, so
    /// a co-resident app falls through to its own autocommit path under
    /// its own role rather than executing inside the owner's tx.
    pub(crate) fn has_tx_for(&self, app_id: &str) -> bool {
        self.lanes.get(app_id).is_some_and(|l| l.session.is_some())
    }

    /// Take `app_id`'s top-level-transaction claim if it is free.
    /// `true` means this caller now owns it and MUST release it via
    /// [`Self::release_tx_claim`] when its transaction settles.
    ///
    /// **Claiming IS opening the lane.** The check and the set are one
    /// `HashMap::entry`, so the window that used to exist between them cannot:
    /// a second `transaction()` for the same app finds the entry occupied and
    /// parks, rather than racing to fill a slot both read as free.
    pub(crate) fn try_claim_tx(&mut self, app_id: &str) -> bool {
        match self.lanes.entry(app_id.to_string()) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(TxLane::default());
                true
            }
        }
    }

    /// `true` if some top-level transaction for `app_id` is in flight —
    /// including one whose `BEGIN` has not landed yet, which is the
    /// window [`Self::has_tx_for`] cannot see.
    pub(crate) fn tx_claimed_by(&self, app_id: &str) -> bool {
        self.lanes.contains_key(app_id)
    }

    /// Close `app_id`'s lane and wake everything parked on it.
    ///
    /// The lane is REMOVED, which is the release: every field goes with it, so
    /// no residue can outlive the transaction that owned it. The waiters are
    /// taken out first and woken after, because they are about to re-claim.
    ///
    /// A session still in the lane at this point is DESTROYED by
    /// [`TxLane::drop`], never returned to the pool - see that impl for why the
    /// distinction is a security one rather than a tidiness one.
    pub(crate) fn release_tx_claim(&mut self, app_id: &str) {
        let Some(mut lane) = self.lanes.remove(app_id) else {
            return;
        };
        for waker in std::mem::take(&mut lane.claim_waiters) {
            waker.wake();
        }
    }

    /// Park a waker on `app_id`'s lane closing.
    ///
    /// A no-op when there is no lane: the claim is already free, so the caller
    /// will win it on its next poll rather than sleeping for a wake that has
    /// no one to send it.
    pub(crate) fn push_tx_waiter(&mut self, app_id: &str, waker: std::task::Waker) {
        if let Some(lane) = self.lanes.get_mut(app_id) {
            lane.claim_waiters.push(waker);
        } else {
            waker.wake();
        }
    }

    /// Park a connection in `app_id`'s transaction slot. Returns the
    /// previous occupant for that app, if any (callers should ensure
    /// this is `None` — every top-level begin path holds `app_id`'s
    /// claim from [`Self::try_claim_tx`] first, which is what keeps two
    /// in-flight `BEGIN`s from reaching here). A different app's parked
    /// tx is never disturbed (SEC-1).
    pub(crate) fn install_tx_client(
        &mut self,
        app_id: &str,
        client: TxConnection,
    ) -> Option<TxConnection> {
        self.lanes
            .entry(app_id.to_string())
            .or_default()
            .session
            .replace(client)
    }

    /// Take `app_id`'s transaction client out of the slot. The caller
    /// must either return it via [`Self::put_tx_client_for`] (when the
    /// await is short and the slot should remain "in transaction") or
    /// drop the client (when settling the tx). Returns `None` when no tx
    /// is parked for `app_id` — including when another app owns the only
    /// parked tx (SEC-1: app B cannot drain app A's client).
    pub(crate) fn take_tx_client_for(&mut self, app_id: &str) -> Option<TxConnection> {
        self.lanes.get_mut(app_id)?.session.take()
    }

    /// Return a client previously taken via [`Self::take_tx_client_for`]
    /// to `app_id`'s slot.
    ///
    /// **A withdrawn session is destroyed here rather than parked.** SC-1's
    /// [`Action::WithdrawSession`](crate::transaction::reducer::Action::WithdrawSession)
    /// can land while another future holds the session out of the slot; that
    /// future's [`TxClientSlotGuard`] restores it on drop, and without this
    /// check the restoration would hand a withdrawn session straight back to
    /// the pool.
    pub(crate) fn put_tx_client_for(&mut self, app_id: &str, client: TxConnection) {
        // **Checked before the lane, because the tombstone outlives it.** A
        // withdrawal retires its lane moments later, and the holder's `Drop`
        // can land after that; consulting the lane first would find nothing and
        // fall through to parking a session the protocol already condemned.
        if self.withdrawn_tx_sessions.contains(app_id) {
            destroy_tx_connection(client);
            return;
        }
        // No lane and no tombstone means the transaction that owned this
        // session settled normally and released it. There is nowhere to park it
        // and nobody to serve it, so it is destroyed rather than resurrecting a
        // closed lane.
        let Some(lane) = self.lanes.get_mut(app_id) else {
            destroy_tx_connection(client);
            return;
        };
        // **An occupied slot means this client is not the current session.**
        // The normal take/put cycle leaves the slot empty between the two calls,
        // so an occupant here can only be a LATER transaction's session - which
        // happens when a withdrawal races the guard that was holding the old
        // one: the next transaction is admitted (clearing the tombstone) before
        // the guard's `Drop` runs. Without this arm the dead session would
        // overwrite the live one, and the tombstone alone cannot cover it
        // because it is cleared by exactly the admission that creates the race.
        if lane.session.is_some() {
            tracing::warn!(
                app_id,
                "a transaction session returned to an occupied slot; destroying it \
                 rather than clobbering the session that is there"
            );
            destroy_tx_connection(client);
            return;
        }
        lane.session = Some(client);
        // The session is back. Wake anything waiting to reclaim it - forced
        // cleanup that cancelled the statement this guard was running is
        // parked on exactly this moment.
        //
        // Deliberately NOT woken on either arm above: a destroyed session is
        // not a reclaimable one, and waking there would hand a waiter an empty
        // slot it has to re-check anyway. Those waiters are released by
        // `retire_transaction` instead, which is what a destroyed session's
        // transaction always reaches.
        self.wake_tx_slot_waiters(app_id);
    }

    /// Wake everything parked on `app_id`'s transaction slot.
    fn wake_tx_slot_waiters(&mut self, app_id: &str) {
        if let Some(waiters) = self
            .lanes
            .get_mut(app_id)
            .map(|lane| std::mem::take(&mut lane.slot_waiters))
        {
            for waker in waiters {
                waker.wake();
            }
        }
    }

    /// Park a waker on `app_id`'s transaction slot refilling.
    ///
    /// Deduplicated by [`std::task::Waker::will_wake`] because the waiter
    /// re-registers on every poll and a `timeout` wrapper polls it more than
    /// once per wake; without this the list would grow for the life of the
    /// wait.
    pub(crate) fn push_tx_slot_waiter(&mut self, app_id: &str, waker: &std::task::Waker) {
        // No lane means no session is ever coming back to this slot, so the
        // caller is woken to re-check rather than parked on a wake that has no
        // sender. `retire_transaction` served that role before the lane owned
        // its own waiters.
        let Some(lane) = self.lanes.get_mut(app_id) else {
            waker.wake_by_ref();
            return;
        };
        if lane.slot_waiters.iter().any(|p| p.will_wake(waker)) {
            return;
        }
        lane.slot_waiters.push(waker.clone());
    }

    /// Record the canceller for the session just installed in `app_id`'s slot.
    pub(crate) fn install_tx_canceller(
        &mut self,
        app_id: &str,
        canceller: crate::backend::cancel::TxCanceller,
    ) {
        self.lanes.entry(app_id.to_string()).or_default().canceller = Some(canceller);
    }

    /// Clone out `app_id`'s canceller.
    ///
    /// Cloned rather than borrowed on purpose: cancelling is `async`, and a
    /// `RefCell` borrow of this context must never be held across an await.
    pub(crate) fn tx_canceller_for(
        &self,
        app_id: &str,
    ) -> Option<crate::backend::cancel::TxCanceller> {
        self.lanes.get(app_id).and_then(|lane| lane.canceller.clone())
    }

    /// Drop `app_id`'s canceller.
    ///
    /// **This must happen BEFORE the pooled lease is returned, and that is not
    /// tidiness - it is the difference between keeping the connection and losing
    /// it.** `Pool::return_client` calls `pool_cancel_lease_prevents_reuse`,
    /// which is `Arc::strong_count(lease) > 1 || lease.is_uncertain()`, and
    /// retires the physical session when it is true. A retained `CancelToken`
    /// holds one of those strong references, so a canceller still parked here
    /// when the session goes back destroys exactly the connection cancellation
    /// exists to preserve. That is the pool's documented contract - "if it is
    /// retained, the pool retires the physical session instead of letting the
    /// token target its next borrower" - and it is a real defence, not an
    /// inconvenience: it is why a canceller cannot outlive its lease and reach
    /// the next borrower's query.
    ///
    /// [`Self::retire_transaction`] also drops it, as a backstop for the paths
    /// that never install a session.
    pub(crate) fn remove_tx_canceller(&mut self, app_id: &str) {
        if let Some(lane) = self.lanes.get_mut(app_id) { lane.canceller = None; }
    }

    // ----- SC-1 TRANSACTION REDUCER -----------------------------------

    /// Admit a transaction for `app_id` and return the actions admission
    /// emits.
    ///
    /// The execution deadline is armed on this transition - the same one that
    /// grants admission - so queue time does not consume the transaction's
    /// execution budget.
    pub(crate) fn admit_transaction(
        &mut self,
        app_id: &str,
        expected: crate::transaction::reducer::identity::ExpectedAuthority,
        budgets: crate::transaction::reducer::TxBudgets,
        now: std::time::Instant,
        max_depth: u32,
    ) -> Vec<crate::transaction::reducer::Action> {
        let (reducer, actions) =
            crate::transaction::reducer::TxReducer::admit(expected, budgets, now, max_depth);
        let lane = self.lanes.entry(app_id.to_string()).or_default();
        let previous = lane.reducer.replace(reducer);
        debug_assert!(
            previous.is_none(),
            "admit_transaction: a transaction is already admitted for this app",
        );
        // A fresh transaction starts from a clean withdrawal state; the
        // tombstone belongs to the session that was withdrawn, not to the app.
        self.withdrawn_tx_sessions.remove(app_id);
        actions
    }

    /// Apply one event to `app_id`'s reducer. `None` means no transaction is
    /// admitted for that app.
    pub(crate) fn apply_transaction_event(
        &mut self,
        app_id: &str,
        event: crate::transaction::reducer::TxEvent,
        now: std::time::Instant,
    ) -> Option<Vec<crate::transaction::reducer::Action>> {
        self.lanes
            .get_mut(app_id).and_then(|lane| lane.reducer.as_mut())
            .map(|reducer| reducer.apply(event, now))
    }

    /// Borrow `app_id`'s reducer, for the frame stack and the latched cleanup
    /// cause the driver reads back.
    pub(crate) fn transaction_reducer(
        &self,
        app_id: &str,
    ) -> Option<&crate::transaction::reducer::TxReducer> {
        self.lanes.get(app_id).and_then(|lane| lane.reducer.as_ref())
    }

    /// The authority `app_id`'s transaction was admitted under, for the events
    /// that must carry it (guard order step 1).
    pub(crate) fn transaction_expected_authority(
        &self,
        app_id: &str,
    ) -> Option<&crate::transaction::reducer::identity::ExpectedAuthority> {
        self.lanes.get(app_id).and_then(|lane| lane.reducer.as_ref()).map(|r| r.expected())
    }

    /// Drop `app_id`'s settled reducer and the frame watermarks that died with
    /// its frames.
    ///
    /// Leaving the watermarks would let the next transaction's first savepoint
    /// pop a stale mark and truncate that transaction's buffer to an unrelated
    /// length.
    ///
    /// It also drops the canceller and RELEASES anything parked on the slot.
    /// The waiter is forced cleanup waiting to reclaim a cancelled statement's
    /// session; if this transaction has been retired out from under it - which
    /// is what the `CancellationSql` deadline does - the session it is waiting
    /// for is never coming, and it must be woken to discover that rather than
    /// sitting out its whole grace.
    pub(crate) fn retire_transaction(&mut self, app_id: &str) {
        if let Some(lane) = self.lanes.get_mut(app_id) {
            lane.reducer = None;
            lane.emit_marks.clear();
            lane.canceller = None;
        }
        self.wake_tx_slot_waiters(app_id);
    }


    /// SC-1 `Action::WithdrawSession`: mark `app_id`'s session withdrawn and
    /// hand the caller whatever is in the slot to destroy.
    ///
    /// The tombstone outlives this call deliberately, and **outlives the
    /// lane** (see [`Self::put_tx_client_for`]). It is recorded beside the
    /// lanes rather than inside one because its whole purpose is to describe a
    /// session whose lane is gone: set here, cleared by the next
    /// [`Self::admit_transaction`], never by retirement.
    pub(crate) fn withdraw_tx_session(&mut self, app_id: &str) -> Option<TxConnection> {
        self.withdrawn_tx_sessions.insert(app_id.to_string());
        self.lanes.get_mut(app_id)?.session.take()
    }

    /// Has `app_id`'s transaction session been withdrawn?
    pub(crate) fn tx_session_withdrawn(&self, app_id: &str) -> bool {
        self.withdrawn_tx_sessions.contains(app_id)
    }

    // ----- FRAME EFFECT WATERMARKS ------------------------------------

    /// Record the queued-event watermark for a frame that just opened.
    pub(crate) fn push_frame_emit_mark(&mut self, app_id: &str) {
        let lane = self.lanes.entry(app_id.to_string()).or_default();
        let mark = lane.pending_emits.len();
        lane.emit_marks.push(mark);
    }

    /// Pop a released frame's watermark without truncating: those events belong
    /// to the enclosing frame now, exactly as its rows do.
    pub(crate) fn pop_frame_emit_mark(&mut self, app_id: &str) -> Option<usize> {
        self.lanes
            .get_mut(app_id)
            .and_then(|lane| lane.emit_marks.pop())
    }

    /// Discard the events the current frame queued, truncating back to its
    /// watermark.
    ///
    /// Called **only after `ROLLBACK TO SAVEPOINT` has succeeded**, because only
    /// then are the matching database changes known to be undone. Discarding
    /// before the statement ran is the "mutate on the assumption it will
    /// succeed" shape: it leaves the failure row's documented fate - retain the
    /// effects for diagnosis, poison the transaction - unachievable, since the
    /// evidence is already gone.
    ///
    /// The watermark is NOT popped here. A rolled-back frame is not closed
    /// until its `RELEASE` lands, and that is the call that pops it.
    pub(crate) fn discard_frame_effects(&mut self, app_id: &str) {
        // A missing mark means the stacks desynced. Truncating to 0 would
        // discard the ENCLOSING frame's events too, so leave the buffer alone:
        // over-publishing is a bug, but silently dropping a committed row's
        // event is a worse one.
        let Some(lane) = self.lanes.get_mut(app_id) else {
            return;
        };
        if let Some(mark) = lane.emit_marks.last().copied() {
            if mark <= lane.pending_emits.len() {
                lane.pending_emits.truncate(mark);
            }
        }
    }

    // ----- PENDING_EMITS ---------------------------------------------

    /// Push a `ChangeEvent` onto the owning app's pending-emits queue
    /// (keyed by the event's own `app_id`; the queue is allocated
    /// lazily on first push within that app's tx).
    pub(crate) fn push_pending_emit(&mut self, ev: ChangeEvent) {
        self.lanes
            .entry(ev.app_id.clone())
            .or_default()
            .pending_emits
            .push(ev);
    }

    /// Drain `app_id`'s pending-emits queue (returns `Vec::new()` if the
    /// app has none queued). Called by the transaction settle path on
    /// COMMIT. SEC-1: only the committing app's events are returned, so
    /// one app's COMMIT cannot fire another's pre-commit events.
    pub(crate) fn drain_pending_emits_for(&mut self, app_id: &str) -> Vec<ChangeEvent> {
        self.lanes
            .get_mut(app_id)
            .map(|lane| std::mem::take(&mut lane.pending_emits))
            .unwrap_or_default()
    }

    /// Clear `app_id`'s pending-emits queue without firing any events.
    /// Called by the transaction settle path on ROLLBACK and by
    /// `exec_begin` to drop any stale residue from an interrupted prior
    /// run. A different app's queue is untouched (SEC-1).
    pub(crate) fn clear_pending_emits_for(&mut self, app_id: &str) {
        if let Some(lane) = self.lanes.get_mut(app_id) {
            lane.pending_emits.clear();
        }
    }
}

thread_local! {
    /// This worker thread's transaction lanes.
    ///
    /// # Why this is a SECOND thread-local, when `context.rs` exists to have one
    ///
    /// `context.rs`'s module doc says its purpose was folding several scattered
    /// `thread_local!`s into a single [`crate::context::ThreadDbContext`] with
    /// typed accessors. This is not a return to that scattered state and the
    /// reason is different in kind: the lanes and the backend slot are separate
    /// OWNERS bound for separate CRATES. `data-engine` cannot reach
    /// `plugin-db`'s thread-local without a Cargo cycle, because plugin-db
    /// already depends on data-engine.
    ///
    /// What made the old arrangement bad was untyped slots with per-site
    /// borrow rituals. Each owner here keeps its typed accessors; there are
    /// simply two owners.
    ///
    /// # Two RefCells are safer than one, not riskier
    ///
    /// A nested borrow of the SAME `RefCell` panics, and `context.rs:296`
    /// records one place that hazard already shapes the code: `set_pool`
    /// constructs a backend while holding `&mut self`, so backends take their
    /// key source as an argument rather than reaching for the thread-local.
    /// Splitting owners removes the possibility of a lane operation and a
    /// backend operation nesting into one another at all.
    ///
    /// Measured before splitting (#165): of 55 `crate::context::with*` closures
    /// in the engine, ZERO touch both an engine owner and an adapter owner. No
    /// closure's atomicity changes here, because none spanned the boundary.
    static TX_LANES: RefCell<TxLanes> = RefCell::new(TxLanes::new());
}

/// Read this thread's lanes.
pub(crate) fn with<R>(f: impl FnOnce(&TxLanes) -> R) -> R {
    TX_LANES.with(|l| f(&l.borrow()))
}

/// Mutate this thread's lanes.
pub(crate) fn with_mut<R>(f: impl FnOnce(&mut TxLanes) -> R) -> R {
    TX_LANES.with(|l| f(&mut l.borrow_mut()))
}

/// Drop every lane on this thread.
///
/// Paired with `crate::reset_context_for_tests`, which resets the ADAPTER's
/// thread-local. Both must run: a test that reset only the context would leave
/// the previous test's lanes - and therefore its transaction claims - visible
/// to the next one on the same thread.
#[cfg(any(test, feature = "test-helpers"))]
pub(crate) fn reset_for_tests() {
    with_mut(|l| *l = TxLanes::new());
}
