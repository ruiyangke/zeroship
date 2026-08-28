//! The transaction's single deadline slot.
//!
//! SC-1 rule 4: the deadline is enforced by an independent timer, not by the
//! settle path. A deadline enforced by the settle path is circular - a body
//! that never settles never reaches the settle path.
//!
//! The slot exists because the deadline a transaction is under is **replaced**
//! as the transaction moves, not merely cancelled. One slot holds exactly one
//! of three values, and has exactly four mutators. Every other call is a pure
//! diagnostic: it returns [`DeadlineError::Stale`] and produces no SQL, no
//! reply, no claim change, no interrupt and no state mutation.
//!
//! ## Why `Armed -> Fired` is one atomic flip
//!
//! A duplicate delivery of an already-claimed timer must be a diagnostic. A
//! check-then-mutate lets two deliveries of the same expiry both believe they
//! own it, and SC-1's guard order step 3 would then be ordering a claim that
//! does not exclude. [`DeadlineSlot::claim_fire`] is therefore the only way to
//! observe an expiry, and it consumes the arming.

use std::fmt;
use std::time::Instant;

/// The kinds of deadline this protocol arms. **Closed at three.**
///
/// Every state that can outlive a caller is bounded by exactly one kind, and
/// each kind is reached by replacing the one before it. `Cancelling` and
/// `Settling` are the only states that wait on a backend that owes an answer
/// and has no caller left to give up, and each has its bound; there is no
/// fourth.
///
/// The artifact this contract was reduced from carries a `RetirementFence`
/// kind whose only consumer is a `Quarantining` state SC-1 declines. It is not
/// reintroduced here: see the module docs of [`super`] for why withdrawing the
/// session replaces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeadlineKind {
    /// Armed on entry to `Preparing`. Bounds everything a caller can see:
    /// admission-to-terminal. The only kind reached by `arm_initial`.
    Execution,
    /// Armed on entry to `Cancelling`. Bounds forced cleanup, from the force
    /// winning the gate to the acknowledgement. Replaces `Execution`.
    CancellationSql,
    /// Armed on entry to `Settling`. Bounds terminal SQL, from issue to
    /// answer. Replaces `Execution`.
    TerminalSql,
}

impl DeadlineKind {
    /// The name used in diagnostics and in the creator-visible cause.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Execution => "execution",
            Self::CancellationSql => "cancellation_sql",
            Self::TerminalSql => "terminal_sql",
        }
    }
}

impl fmt::Display for DeadlineKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A generation, minted fresh at every arming and **never reused across
/// kinds**.
///
/// The number alone never authenticates a fire - the `(kind, generation)` pair
/// does. A newtype rather than a bare `u64` so a generation cannot be compared
/// against a frame sequence or a reservation id by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeadlineGeneration(u64);

impl DeadlineGeneration {
    /// The raw value, for diagnostics only.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Mints [`DeadlineGeneration`]s that are unique for one transaction's life.
///
/// Held by the transaction entry, not by the slot: the slot is replaced across
/// kinds and a counter living inside it would restart, which is exactly the
/// reuse the `(kind, generation)` pair exists to prevent.
#[derive(Debug, Default)]
pub struct DeadlineGenerations {
    next: u64,
}

impl DeadlineGenerations {
    /// A fresh generation. Never returns the same value twice.
    pub const fn mint(&mut self) -> DeadlineGeneration {
        self.next += 1;
        DeadlineGeneration(self.next)
    }
}

/// The slot's value. Exactly one of three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadlineState {
    /// No timer is outstanding. The only state `arm_initial` accepts, and the
    /// state `disarm` produces.
    Disarmed,
    /// A timer is scheduled and has not been claimed.
    Armed {
        kind: DeadlineKind,
        generation: DeadlineGeneration,
        at: Instant,
    },
    /// A timer expired and its fire was claimed. The claimant holds the sole
    /// right to act on that expiry.
    Fired {
        kind: DeadlineKind,
        generation: DeadlineGeneration,
    },
}

/// Every rejected slot call. **Purely diagnostic** - a caller receiving one
/// must produce no SQL, no reply, no claim change, no interrupt and no state
/// mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadlineError {
    /// The call named a `(kind, generation)` pair the slot is not holding, or
    /// was made from a state that mutator does not accept. A duplicate
    /// delivery of an already-claimed timer is exactly this case.
    Stale,
}

impl DeadlineError {
    /// The creator-invisible diagnostic code. This never reaches a creator:
    /// SC-1's guard order makes a stale deadline a no-op, not a rejection the
    /// caller is told about.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Stale => "stale_transaction_deadline",
        }
    }
}

/// What a successful [`DeadlineSlot::arm_initial`] or
/// [`DeadlineSlot::replace_current`] tells the caller to schedule.
///
/// The reducer is pure, so it never sleeps. It returns this and the driver
/// spawns a timer task carrying **only** these three fields plus the
/// transaction key and the event sender - no session, no client, no settle
/// future. That is what makes rule 4's "independent of callback behaviour"
/// true rather than aspirational.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleTimer {
    pub kind: DeadlineKind,
    pub generation: DeadlineGeneration,
    pub at: Instant,
}

/// One transaction's deadline slot.
///
/// Shared by every deadline this protocol arms, holding exactly one
/// [`DeadlineState`].
#[derive(Debug)]
pub struct DeadlineSlot {
    state: DeadlineState,
}

impl Default for DeadlineSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl DeadlineSlot {
    /// A slot holding no timer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: DeadlineState::Disarmed,
        }
    }

    /// The current value. Read-only; every mutation goes through the four
    /// mutators below.
    #[must_use]
    pub const fn state(&self) -> DeadlineState {
        self.state
    }

    /// Mutator 1. **Legal from `Disarmed` only.**
    ///
    /// Arms the first deadline of a transaction's life, which is always
    /// [`DeadlineKind::Execution`] - it is the only kind `arm_initial` is ever
    /// called with, because the other two are reached by replacement.
    ///
    /// Arming from `Armed` or `Fired` would abandon a timer the transaction is
    /// still under, so both are stale.
    ///
    /// # Errors
    ///
    /// Returns the refusal this operation's guard produced. Every one is a
    /// value the caller receives, never an out-of-band log line.
    pub fn arm_initial(
        &mut self,
        kind: DeadlineKind,
        generation: DeadlineGeneration,
        at: Instant,
    ) -> Result<ScheduleTimer, DeadlineError> {
        if self.state != DeadlineState::Disarmed {
            return Err(DeadlineError::Stale);
        }
        self.state = DeadlineState::Armed {
            kind,
            generation,
            at,
        };
        Ok(ScheduleTimer {
            kind,
            generation,
            at,
        })
    }

    /// Mutator 2. **Legal from `Armed` on that exact pair.**
    ///
    /// Flips to `Fired` atomically, granting the caller the sole right to act
    /// on that expiry. A second delivery of the same expiry finds `Fired` and
    /// is stale, which is what makes the claim exclude rather than merely
    /// check.
    ///
    /// # Errors
    ///
    /// Returns the refusal this operation's guard produced. Every one is a
    /// value the caller receives, never an out-of-band log line.
    pub fn claim_fire(
        &mut self,
        kind: DeadlineKind,
        generation: DeadlineGeneration,
    ) -> Result<(), DeadlineError> {
        match self.state {
            DeadlineState::Armed {
                kind: armed_kind,
                generation: armed_generation,
                ..
            } if armed_kind == kind && armed_generation == generation => {
                self.state = DeadlineState::Fired { kind, generation };
                Ok(())
            }
            _ => Err(DeadlineError::Stale),
        }
    }

    /// Mutator 3. **Legal from `Armed` or `Fired`, of `expected_kind`.**
    ///
    /// Becomes `Armed` on the successor kind with a fresh generation.
    ///
    /// Accepting `Fired` is deliberate: a deadline that has already fired and
    /// driven the transaction into forced cleanup must still be replaceable by
    /// the deadline that bounds *that* cleanup. Otherwise a hung rollback is
    /// unbounded and strands the admission claim, which is rule 5's DBR-11
    /// failure reached by a second route.
    ///
    /// `expected_kind` is not decoration. A second force arriving in
    /// `Cancelling` finds [`DeadlineKind::CancellationSql`] current, fails the
    /// expectation, and is a pure diagnostic rather than a second cleanup with
    /// a fresh generation.
    ///
    /// # Errors
    ///
    /// Returns the refusal this operation's guard produced. Every one is a
    /// value the caller receives, never an out-of-band log line.
    pub fn replace_current(
        &mut self,
        expected_kind: DeadlineKind,
        next_kind: DeadlineKind,
        next_generation: DeadlineGeneration,
        next_at: Instant,
    ) -> Result<ScheduleTimer, DeadlineError> {
        let current_kind = match self.state {
            DeadlineState::Armed { kind, .. } | DeadlineState::Fired { kind, .. } => kind,
            DeadlineState::Disarmed => return Err(DeadlineError::Stale),
        };
        if current_kind != expected_kind {
            return Err(DeadlineError::Stale);
        }
        self.state = DeadlineState::Armed {
            kind: next_kind,
            generation: next_generation,
            at: next_at,
        };
        Ok(ScheduleTimer {
            kind: next_kind,
            generation: next_generation,
            at: next_at,
        })
    }

    /// Mutator 4. **Legal from any state**, at terminal cleanup.
    ///
    /// Unlike the other three this never fails: a transaction reaching
    /// `Settled` must leave no armed timer behind whatever it was under, and a
    /// terminal cleanup that could be refused would leak one.
    pub const fn disarm(&mut self) {
        self.state = DeadlineState::Disarmed;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    /// Where this fails today: `DeadlineSlot` did not exist, so the module
    /// did not compile. It also fails on any implementation that lets
    /// `arm_initial` overwrite a live arming.
    #[test]
    fn arm_initial_is_legal_only_from_disarmed() {
        let mut generations = DeadlineGenerations::default();
        let mut slot = DeadlineSlot::new();
        let first = generations.mint();
        assert!(slot
            .arm_initial(DeadlineKind::Execution, first, later())
            .is_ok());

        // Armed -> a second arm_initial is a pure diagnostic. Arming over a
        // live timer would abandon the deadline the transaction is under.
        let second = generations.mint();
        assert_eq!(
            slot.arm_initial(DeadlineKind::Execution, second, later()),
            Err(DeadlineError::Stale)
        );
        assert!(
            matches!(slot.state(), DeadlineState::Armed { generation, .. } if generation == first),
            "a rejected arm_initial must not mutate the slot"
        );

        // Fired -> also refused; only `replace_current` moves a fired slot on.
        slot.claim_fire(DeadlineKind::Execution, first).unwrap();
        assert_eq!(
            slot.arm_initial(DeadlineKind::Execution, second, later()),
            Err(DeadlineError::Stale)
        );
    }

    /// The duplicate-delivery case SC-1 guard order step 3 depends on.
    ///
    /// Where this fails today: no slot existed. It fails on a check-then-mutate
    /// implementation, where both deliveries of one expiry observe `Armed` and
    /// both believe they own the fire.
    #[test]
    fn a_second_delivery_of_one_expiry_does_not_also_claim_it() {
        let mut generations = DeadlineGenerations::default();
        let mut slot = DeadlineSlot::new();
        let generation = generations.mint();
        slot.arm_initial(DeadlineKind::Execution, generation, later())
            .unwrap();

        assert!(slot.claim_fire(DeadlineKind::Execution, generation).is_ok());
        assert_eq!(
            slot.claim_fire(DeadlineKind::Execution, generation),
            Err(DeadlineError::Stale),
            "the second delivery of one expiry must be a pure diagnostic"
        );
    }

    /// A generation is never reused across kinds, so the number alone never
    /// authenticates a fire.
    ///
    /// Where this fails today: no slot existed. It fails on an implementation
    /// that compares only the generation, which is the shape that lets a
    /// stale `Execution` timer claim a live `CancellationSql` arming.
    #[test]
    fn the_pair_authenticates_a_fire_not_the_generation_alone() {
        let mut generations = DeadlineGenerations::default();
        let mut slot = DeadlineSlot::new();
        let execution = generations.mint();
        slot.arm_initial(DeadlineKind::Execution, execution, later())
            .unwrap();
        slot.claim_fire(DeadlineKind::Execution, execution).unwrap();

        let cancellation = generations.mint();
        assert_ne!(execution, cancellation, "generations are never reused");
        slot.replace_current(
            DeadlineKind::Execution,
            DeadlineKind::CancellationSql,
            cancellation,
            later(),
        )
        .unwrap();

        // The right number under the wrong kind is stale.
        assert_eq!(
            slot.claim_fire(DeadlineKind::Execution, cancellation),
            Err(DeadlineError::Stale)
        );
        // The right kind under the wrong number is stale.
        assert_eq!(
            slot.claim_fire(DeadlineKind::CancellationSql, execution),
            Err(DeadlineError::Stale)
        );
        // Only the pair works.
        assert!(slot
            .claim_fire(DeadlineKind::CancellationSql, cancellation)
            .is_ok());
    }

    /// `replace_current` accepts `Fired`, which is what bounds forced cleanup
    /// after an execution deadline already fired.
    ///
    /// Where this fails today: no slot existed. It fails on an implementation
    /// that accepts only `Armed`, which leaves a hung rollback unbounded and
    /// strands the admission claim (DBR-11 by a second route).
    #[test]
    fn a_fired_deadline_is_replaceable_by_the_one_that_bounds_its_cleanup() {
        let mut generations = DeadlineGenerations::default();
        let mut slot = DeadlineSlot::new();
        let execution = generations.mint();
        slot.arm_initial(DeadlineKind::Execution, execution, later())
            .unwrap();
        slot.claim_fire(DeadlineKind::Execution, execution).unwrap();
        assert!(matches!(slot.state(), DeadlineState::Fired { .. }));

        let cancellation = generations.mint();
        let scheduled = slot
            .replace_current(
                DeadlineKind::Execution,
                DeadlineKind::CancellationSql,
                cancellation,
                later(),
            )
            .expect("a fired Execution deadline is replaceable by CancellationSql");
        assert_eq!(scheduled.kind, DeadlineKind::CancellationSql);
    }

    /// `expected_kind` is what makes a second force in `Cancelling` a
    /// diagnostic rather than a second cleanup.
    ///
    /// Where this fails today: no slot existed. It fails on an implementation
    /// that ignores `expected_kind`, which is the shape that lets two forces
    /// each arm their own cleanup deadline over one transaction.
    #[test]
    fn a_second_force_in_cancelling_fails_the_kind_expectation() {
        let mut generations = DeadlineGenerations::default();
        let mut slot = DeadlineSlot::new();
        let execution = generations.mint();
        slot.arm_initial(DeadlineKind::Execution, execution, later())
            .unwrap();
        let cancellation = generations.mint();
        slot.replace_current(
            DeadlineKind::Execution,
            DeadlineKind::CancellationSql,
            cancellation,
            later(),
        )
        .unwrap();

        // A second force still names `Execution` as its expected kind - every
        // call site does - and must be refused.
        let second = generations.mint();
        assert_eq!(
            slot.replace_current(
                DeadlineKind::Execution,
                DeadlineKind::CancellationSql,
                second,
                later()
            ),
            Err(DeadlineError::Stale)
        );
        assert!(
            matches!(
                slot.state(),
                DeadlineState::Armed { generation, .. } if generation == cancellation
            ),
            "a rejected replace_current must not mutate the slot"
        );
    }

    /// `disarm` is total: terminal cleanup must leave no armed timer behind
    /// whatever state the slot was in.
    ///
    /// Where this fails today: no slot existed. It fails on an implementation
    /// where `disarm` is fallible, which leaks a timer out of any state its
    /// precondition did not anticipate.
    #[test]
    fn disarm_is_total_over_every_slot_state() {
        let mut generations = DeadlineGenerations::default();
        for arm in 0..3 {
            let mut slot = DeadlineSlot::new();
            let generation = generations.mint();
            if arm >= 1 {
                slot.arm_initial(DeadlineKind::Execution, generation, later())
                    .unwrap();
            }
            if arm == 2 {
                slot.claim_fire(DeadlineKind::Execution, generation).unwrap();
            }
            slot.disarm();
            assert_eq!(slot.state(), DeadlineState::Disarmed);
        }
    }
}
