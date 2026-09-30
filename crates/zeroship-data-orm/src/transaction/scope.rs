//! Identity carried by a transaction callback and its continuations, and the
//! claim an issued operation holds on its frame.

use crate::binding::DbRoute;
use crate::error::DbError;

/// A captured transaction and savepoint identity. A route's next transaction
/// must never inherit work left running by a previous callback.
#[derive(Clone, Debug)]
pub struct TransactionScope {
    route: DbRoute,
    generation: u64,
    frame: u64,
}

impl TransactionScope {
    /// Decode the scope preserved by a host's async context. These values are
    /// observations, checked against the live ORM protocol before use.
    pub fn observed(route: DbRoute, generation: u64, frame: u64) -> Self {
        Self {
            route,
            generation,
            frame,
        }
    }

    /// Capture the frame after BEGIN or SAVEPOINT has succeeded.
    pub fn current(route: &DbRoute) -> Result<Self, DbError> {
        crate::tx_lanes::with(|lanes| {
            let reducer = lanes.transaction_reducer(route).ok_or_else(expired)?;
            let generation = reducer.generation().ok_or_else(expired)?;
            let frame = reducer.frames().top().ok_or_else(expired)?.id();
            Ok(Self::observed(route.clone(), generation.0, frame.get()))
        })
    }

    /// The tenant AND database this scope belongs to.
    pub fn route(&self) -> &DbRoute {
        &self.route
    }

    pub fn app_id(&self) -> &str {
        self.route.app_id()
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Check immediately before admitting SQL, including after async backend
    /// resolution. An open child also excludes work from its parent frame.
    pub fn check(&self) -> Result<(), DbError> {
        crate::tx_lanes::with(|lanes| self.check_in(lanes))
    }

    fn check_in(&self, lanes: &crate::tx_lanes::TxLanes) -> Result<(), DbError> {
        let reducer = lanes.transaction_reducer(&self.route).ok_or_else(expired)?;
        if reducer.generation().map(|generation| generation.0) != Some(self.generation)
            || !reducer.frames().contains(self.frame)
        {
            return Err(expired());
        }
        // An open child frame belongs to a nested transaction, which is one of
        // this frame's own outstanding calls.
        if reducer.frames().top().map(|frame| frame.id().get()) != Some(self.frame) {
            return Err(connection_busy());
        }
        Ok(())
    }

    /// Claim this frame for one operation, at the moment the operation is
    /// issued.
    ///
    /// A transaction has one connection, and its reducer admits one command on
    /// it at a time. That guard runs when an operation first RUNS, so on its own
    /// it admits a second operation whenever the first has already finished by
    /// then - a schedule a fast backend or a busy executor produces unasked.
    /// Taking this claim synchronously where the operation is issued decides
    /// the overlap from the order the operations were issued in, and nothing
    /// else: while the claim is held, every other operation issued against the
    /// frame is refused with `transaction_connection_busy`. A V8 call is issued
    /// when it is made; a native future when it is first polled.
    ///
    /// Settlement reads the same claims: a frame is not ended while one is
    /// held, and a callback that returned while one was held has its frame
    /// rolled back (`OutstandingWork`).
    ///
    /// The claim is keyed by frame, not by connection. A nested transaction is
    /// an operation of the frame it was issued from and holds that frame until
    /// it settles, while the work inside its callback claims the child frame
    /// the nested transaction opened.
    ///
    /// A frame whose callback has settled takes no new claim: the operation
    /// was made after the callback returned, belongs to a transaction that is
    /// ending, and is refused as expired rather than waited for and committed.
    /// That is checked here and not in [`Self::check`], which also runs for
    /// operations claimed before the callback settled, and those must be
    /// allowed to finish.
    ///
    /// # Errors
    /// `transaction_scope_expired` when the scope's transaction has settled,
    /// its frame has closed, or its callback has settled; and
    /// `transaction_connection_busy` when a nested frame is open above it or
    /// another issued operation holds it.
    pub fn claim_operation(&self) -> Result<OperationClaim, DbError> {
        let owner = crate::orm_context::current();
        owner.lanes_mut(|lanes| {
            self.check_in(lanes)?;
            if lanes.frame_settling(&self.route, self.generation, self.frame) {
                return Err(expired());
            }
            if lanes.claim_frame(&self.route, self.generation, self.frame) {
                Ok(())
            } else {
                Err(connection_busy())
            }
        })?;
        Ok(OperationClaim {
            owner,
            route: self.route.clone(),
            generation: self.generation,
            frame: self.frame,
        })
    }
}

/// One issued operation's hold on its transaction frame.
///
/// Taken by [`TransactionScope::claim_operation`] and released when dropped,
/// which is when the operation that owns it settles or is abandoned. It
/// authorizes nothing: the statements it covers still pass the scope check and
/// the reducer's operation guard. What it adds is the refusal of any other
/// operation issued against the same frame in the meantime.
#[derive(Debug)]
pub struct OperationClaim {
    owner: crate::OrmContext,
    route: DbRoute,
    generation: u64,
    frame: u64,
}

impl Drop for OperationClaim {
    fn drop(&mut self) {
        // Keyed by the session generation, which is never reused, so a claim
        // that outlives its transaction cannot release a later one's frame.
        self.owner
            .lanes_mut(|lanes| lanes.release_frame_claim(&self.route, self.generation, self.frame));
    }
}

/// What a settlement found on the frame it ends, sampled when the callback
/// that owns the frame settled.
///
/// A claim still held at that moment is work the callback started and did not
/// wait for. [`Self::drained`] is what makes settlement wait for it, and
/// [`Self::held`] is what decides that the frame may not commit it.
///
/// Sampling also closes the frame to new claims until this value is dropped,
/// which is when the settle has finished: an operation made after the callback
/// returned is refused rather than waited for and committed.
#[derive(Debug)]
pub(crate) struct OutstandingWork {
    owner: crate::OrmContext,
    route: DbRoute,
    generation: u64,
    frame: u64,
    held: bool,
}

impl OutstandingWork {
    /// Sample `frame`, or the root frame for `None`, synchronously. `None`
    /// when no transaction is admitted, which settlement reports itself.
    pub(crate) fn sample(
        route: &DbRoute,
        frame: Option<crate::transaction::reducer::frames::FrameId>,
    ) -> Option<Self> {
        let owner = crate::orm_context::current();
        owner.clone().lanes_mut(|lanes| {
            let reducer = lanes.transaction_reducer(route)?;
            let generation = reducer.generation()?.0;
            let frame = match frame {
                Some(frame) => frame.get(),
                None => reducer.frames().root()?.id().get(),
            };
            lanes.mark_frame_settling(route, generation, frame);
            Some(Self {
                owner,
                route: route.clone(),
                generation,
                frame,
                held: lanes.frame_claimed(route, generation, frame),
            })
        })
    }

    /// Whether an issued operation held the frame when its callback settled.
    pub(crate) const fn held(&self) -> bool {
        self.held
    }

    /// Resolve once no issued operation holds the frame.
    ///
    /// Also resolves when the transaction is forced into cleanup or retired:
    /// neither ends through its frames again, and the settlement that was
    /// waiting goes on to join the cleanup's result.
    pub(crate) const fn drained(&self) -> FrameDrained<'_> {
        FrameDrained { work: self }
    }
}

impl Drop for OutstandingWork {
    fn drop(&mut self) {
        self.owner.lanes_mut(|lanes| {
            lanes.clear_frame_settling(&self.route, self.generation, self.frame);
        });
    }
}

/// The future [`OutstandingWork::drained`] returns.
#[derive(Debug)]
pub(crate) struct FrameDrained<'a> {
    work: &'a OutstandingWork,
}

impl std::future::Future for FrameDrained<'_> {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        let work = self.work;
        let drained = crate::tx_lanes::with_mut(|lanes| {
            let settling = lanes
                .transaction_reducer(&work.route)
                .is_some_and(|reducer| {
                    reducer.cleanup().is_none()
                        && reducer.generation().map(|generation| generation.0)
                            == Some(work.generation)
                });
            if !settling || !lanes.frame_claimed(&work.route, work.generation, work.frame) {
                return true;
            }
            lanes.push_frame_waiter(&work.route, cx.waker());
            false
        });
        if drained {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    }
}

pub(crate) fn expired() -> DbError {
    DbError::validation_hinted(
        "transaction_scope_expired",
        "db: this operation belongs to a transaction scope that has already settled".to_owned(),
        "Await every database call started inside a transaction before its callback returns.",
    )
}

/// Two operations overlapped on one transaction's connection.
///
/// The one creator-facing spelling of the overlap refusal, whichever guard
/// produced it: the claim taken at issue, an open nested frame, the reducer's
/// operation guard, or an empty session slot. One code, one message and one
/// hint, so what a creator reads does not depend on which guard was first.
pub(crate) fn connection_busy() -> DbError {
    DbError::validation_hinted(
        "transaction_connection_busy",
        "db: another operation is already using this transaction's connection".to_owned(),
        "A transaction has one connection, so its operations cannot overlap. Await each db call \
         inside the db.transaction(...) callback, a nested db.transaction(...) included, before \
         starting the next; a Promise.all over several tx calls runs them concurrently on that \
         one connection.",
    )
}

/// A callback returned while a call it started was still outstanding.
///
/// Settlement waits for that call to finish, then rolls the frame back rather
/// than committing work the callback never waited for.
pub(crate) fn work_unfinished() -> DbError {
    DbError::validation_hinted(
        "transaction_work_unfinished",
        "db: the transaction callback returned while a database call it started was still \
         running, so its work was rolled back"
            .to_owned(),
        "Await every db call started inside a db.transaction(...) callback, a nested \
         db.transaction(...) included, before the callback returns.",
    )
}
