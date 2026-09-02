//! The strict-LIFO frame stack, its monotonic savepoint naming, and the
//! per-frame effect buffers.
//!
//! SC-1 "Frames and effects": only the innermost open frame may issue data
//! SQL, open a child, or close. The root frame is created only once `BEGIN` is
//! confirmed; a child is inserted as `Opening` before `SAVEPOINT` is sent and
//! becomes `Open` only when that command succeeds.
//!
//! ## Savepoint names are monotonic, never depth-derived
//!
//! `ROLLBACK TO SAVEPOINT` deliberately **leaves the savepoint defined**, and
//! PostgreSQL resolves a savepoint name to the **most recently established**
//! one (`libs/compio-postgres/src/transaction.rs:63-82`). A depth-derived name
//! is therefore reused after the depth decrements, so a leftover savepoint
//! shadows an enclosing frame of the same name and sends the enclosing rollback
//! **to the wrong scope**.
//!
//! The sequence governs naming only; the simultaneous-open depth stays capped
//! at [`super::super::MAX_SAVEPOINT_DEPTH`], which is the existing public
//! limit.

use std::collections::BTreeSet;

/// A frame's identity. Minted by the registry from its own sequence and
/// **never reused**, so a caller cannot collide two frames or resurrect a
/// closed name.
///
/// `OpenFrame` takes no caller-supplied child id for exactly this reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
// `pub`, and the fourth item in this crate whose zero-ish reader count would
// have narrowed it wrongly. Phase 0.5's audit tried `pub(crate)` on
// 2026-09-02: `tests/native_transaction.rs` binds one from
// `probe::open_frame(APP)` and compares two of them, never writing the type's
// name, so six errors. Its siblings in this file narrowed cleanly. See the
// return-position rule in tests/lib/pub_fence_census.sh.
pub struct FrameId(u64);

impl FrameId {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Whether a frame's `SAVEPOINT` has landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameStatus {
    /// Inserted before `SAVEPOINT` was sent. Cannot yet act.
    Opening,
    /// `SAVEPOINT` succeeded (or this is the root, created on a confirmed
    /// `BEGIN`).
    Open,
    /// `ROLLBACK TO` succeeded and the matching `RELEASE` has not yet landed.
    ///
    /// **A rolled-back frame is not closed until its `RELEASE` lands**, so
    /// until then the transaction is not `Idle` and the frame is not available
    /// for new work. Collapsing this into `Open` is what reports `Idle` the
    /// moment `ROLLBACK TO` returns.
    RolledBack,
}

/// One frame on the stack.
#[derive(Debug, Clone)]
pub(crate) struct Frame {
    id: FrameId,
    /// `None` for the root.
    savepoint: Option<Box<str>>,
    status: FrameStatus,
    /// This frame's queued effects. Per frame, not per app: a flat app-keyed
    /// buffer lets a top-level `COMMIT` drain a rolled-back child's queue and
    /// tell a subscriber about a row that does not exist.
    effects: Vec<Effect>,
}

impl Frame {
    #[must_use]
    pub const fn id(&self) -> FrameId {
        self.id
    }

    /// The savepoint name, or `None` for the root.
    #[must_use]
    pub fn savepoint(&self) -> Option<&str> {
        self.savepoint.as_deref()
    }

    #[must_use]
    pub const fn status(&self) -> FrameStatus {
        self.status
    }

    #[must_use]
    pub const fn is_root(&self) -> bool {
        self.savepoint.is_none()
    }

    /// This frame's queued effects, for diagnosis.
    #[must_use]
    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }
}

/// A change event queued by a write, published only from a confirmed root
/// commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Effect {
    pub collection: Box<str>,
    pub payload: Box<str>,
}

impl Effect {
    pub fn new(collection: impl Into<Box<str>>, payload: impl Into<Box<str>>) -> Self {
        Self {
            collection: collection.into(),
            payload: payload.into(),
        }
    }
}

/// Why a frame operation was refused. Each is a value the caller receives,
/// never an out-of-band log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameError {
    /// The named frame is not the top of the stack. None of the frame guards
    /// mutates the stack, so this changes nothing.
    SavepointNotCurrent,
    /// A ninth simultaneous child.
    SavepointDepthExceeded,
    /// The root frame cannot be closed by a frame close; only root settlement
    /// ends it.
    SavepointRootCannotClose,
    /// No frame is open - `BEGIN` has not been confirmed.
    NoOpenFrame,
}

impl FrameError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::SavepointNotCurrent => "savepoint_not_current",
            Self::SavepointDepthExceeded => "savepoint_depth_exceeded",
            Self::SavepointRootCannotClose => "savepoint_root_cannot_close",
            Self::NoOpenFrame => "no_open_frame",
        }
    }
}

/// How a frame closed, which decides the fate of its effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameClose {
    /// `RELEASE` succeeded: effects are appended to the parent, in order.
    /// Nothing is published.
    Released,
    /// `ROLLBACK TO` succeeded: effects are **discarded** - the database
    /// changes they describe are known to be undone.
    RolledBackTo,
}

/// The strict-LIFO frame stack.
#[derive(Debug, Default)]
pub(crate) struct FrameStack {
    frames: Vec<Frame>,
    /// Monotonic, never reset, never depth-derived. Governs both the frame id
    /// and the savepoint name.
    next_sequence: u64,
    /// Every savepoint name this transaction has ever minted, so invariant 9's
    /// "a frame sequence or name never repeats" is checkable rather than
    /// merely intended.
    minted_names: BTreeSet<Box<str>>,
    /// The simultaneous-open depth cap.
    max_depth: u32,
}

impl FrameStack {
    /// A stack with no frames. `BEGIN` has not been confirmed.
    #[must_use]
    pub const fn new(max_depth: u32) -> Self {
        Self {
            frames: Vec::new(),
            next_sequence: 0,
            minted_names: BTreeSet::new(),
            max_depth,
        }
    }

    /// Has `BEGIN` been confirmed?
    #[must_use]
    pub const fn has_root(&self) -> bool {
        !self.frames.is_empty()
    }

    /// The number of frames currently open, root included.
    #[must_use]
    pub const fn depth(&self) -> usize {
        self.frames.len()
    }

    /// The innermost frame - the only one that may act.
    #[must_use]
    pub fn top(&self) -> Option<&Frame> {
        self.frames.last()
    }

    /// Every savepoint name minted so far, for the arm that rules on reuse.
    #[must_use]
    pub const fn minted_names(&self) -> &BTreeSet<Box<str>> {
        &self.minted_names
    }

    /// Create the root frame. Called only on a confirmed `BEGIN`.
    pub fn open_root(&mut self) -> FrameId {
        debug_assert!(self.frames.is_empty(), "root is opened exactly once");
        let id = self.mint_id();
        self.frames.push(Frame {
            id,
            savepoint: None,
            status: FrameStatus::Open,
            effects: Vec::new(),
        });
        id
    }

    /// Insert a child as `Opening` and mint its savepoint name.
    ///
    /// The name comes from the monotonic sequence, so it is never one a
    /// leftover savepoint on the server could shadow.
    ///
    /// # Errors
    ///
    /// Returns the refusal this operation's guard produced. Every one is a
    /// value the caller receives, never an out-of-band log line.
    pub fn open_child(&mut self) -> Result<(FrameId, Box<str>), FrameError> {
        let Some(top) = self.frames.last() else {
            return Err(FrameError::NoOpenFrame);
        };
        if top.status != FrameStatus::Open {
            // A frame mid-open or mid-rollback is not available for new work.
            return Err(FrameError::SavepointNotCurrent);
        }
        // The depth that WOULD be opened is the current depth. `frames`
        // includes the root, and the cap counts savepoint levels, so a stack of
        // `max_depth + 1` frames is `max_depth` savepoints.
        if self.frames.len() as u64 > u64::from(self.max_depth) {
            return Err(FrameError::SavepointDepthExceeded);
        }
        let id = self.mint_id();
        let name: Box<str> = format!("zs_sp_{}", id.get()).into_boxed_str();
        debug_assert!(
            !self.minted_names.contains(&name),
            "monotonic naming must never repeat"
        );
        self.minted_names.insert(name.clone());
        self.frames.push(Frame {
            id,
            savepoint: Some(name.clone()),
            status: FrameStatus::Opening,
            effects: Vec::new(),
        });
        Ok((id, name))
    }

    /// Promote the top frame from `Opening` to `Open` on a successful
    /// `SAVEPOINT`.
    ///
    /// # Errors
    ///
    /// Returns the refusal this operation's guard produced. Every one is a
    /// value the caller receives, never an out-of-band log line.
    pub fn confirm_child_open(&mut self, id: FrameId) -> Result<(), FrameError> {
        let Some(top) = self.frames.last_mut() else {
            return Err(FrameError::NoOpenFrame);
        };
        if top.id != id {
            return Err(FrameError::SavepointNotCurrent);
        }
        top.status = FrameStatus::Open;
        Ok(())
    }

    /// Drop a child whose `SAVEPOINT` failed. Its effects go with it; nothing
    /// was ever queued against a frame that never opened.
    ///
    /// # Errors
    ///
    /// Returns the refusal this operation's guard produced. Every one is a
    /// value the caller receives, never an out-of-band log line.
    pub fn abandon_opening_child(&mut self, id: FrameId) -> Result<(), FrameError> {
        let Some(top) = self.frames.last() else {
            return Err(FrameError::NoOpenFrame);
        };
        if top.id != id || top.is_root() {
            return Err(FrameError::SavepointNotCurrent);
        }
        self.frames.pop();
        Ok(())
    }

    /// Mark the top frame rolled back. **Does not pop it**: the frame is not
    /// closed until its `RELEASE` lands.
    ///
    /// # Errors
    ///
    /// Returns the refusal this operation's guard produced. Every one is a
    /// value the caller receives, never an out-of-band log line.
    pub fn mark_rolled_back(&mut self, id: FrameId) -> Result<(), FrameError> {
        let Some(top) = self.frames.last_mut() else {
            return Err(FrameError::NoOpenFrame);
        };
        if top.id != id {
            return Err(FrameError::SavepointNotCurrent);
        }
        if top.is_root() {
            return Err(FrameError::SavepointRootCannotClose);
        }
        // The fate is applied here, AFTER the statement succeeded, never
        // before. Discarding on the assumption that `ROLLBACK TO` will succeed
        // makes the failure row unachievable - the diagnostic evidence it calls
        // for is already gone by the time the failure is known.
        top.effects.clear();
        top.status = FrameStatus::RolledBack;
        Ok(())
    }

    /// Queue an effect against the current frame.
    ///
    /// Invariant 11: success appends only to the current frame.
    ///
    /// # Errors
    ///
    /// Returns the refusal this operation's guard produced. Every one is a
    /// value the caller receives, never an out-of-band log line.
    pub fn queue_effect(&mut self, effect: Effect) -> Result<(), FrameError> {
        let Some(top) = self.frames.last_mut() else {
            return Err(FrameError::NoOpenFrame);
        };
        if top.status != FrameStatus::Open {
            return Err(FrameError::SavepointNotCurrent);
        }
        top.effects.push(effect);
        Ok(())
    }

    /// Close the top frame, applying `close`'s fate to its effects.
    ///
    /// Returns the closed frame's id.
    ///
    /// # Errors
    ///
    /// Returns the refusal this operation's guard produced.
    ///
    /// # Panics
    ///
    /// On a stack invariant this module maintains itself; a caller cannot
    /// reach it with any sequence of public calls.
    pub fn close_top(&mut self, id: FrameId, close: FrameClose) -> Result<FrameId, FrameError> {
        let Some(top) = self.frames.last() else {
            return Err(FrameError::NoOpenFrame);
        };
        if top.id != id {
            return Err(FrameError::SavepointNotCurrent);
        }
        if top.is_root() {
            return Err(FrameError::SavepointRootCannotClose);
        }
        let frame = self.frames.pop().expect("checked non-empty above");
        match close {
            FrameClose::Released => {
                // Invariant 11: release moves the exact child sequence to the
                // parent, in order.
                let parent = self
                    .frames
                    .last_mut()
                    .expect("a child always has a parent - the root cannot close here");
                parent.effects.extend(frame.effects);
            }
            FrameClose::RolledBackTo => {
                // Already discarded by `mark_rolled_back`, and discarded again
                // here for the path that rolls back and releases in one step.
                // Never touches the parent's buffer: truncating would discard
                // parent effects, and silently dropping a committed row's event
                // is worse than over-publishing.
                drop(frame.effects);
            }
        }
        Ok(frame.id)
    }

    /// Detach every retained effect for publication from a confirmed root
    /// commit, in order.
    ///
    /// Only the root can be open at this point in a healthy transaction; any
    /// deeper frame's buffer is included because a commit confirms the whole
    /// tree, and a child that closed already moved or discarded its own.
    pub fn take_effects_for_confirmed_commit(&mut self) -> Vec<Effect> {
        let mut out = Vec::new();
        for frame in &mut self.frames {
            out.append(&mut frame.effects);
        }
        out
    }

    /// Discard every frame buffer, for every terminal outcome other than a
    /// confirmed root commit.
    pub fn discard_all_effects(&mut self) {
        for frame in &mut self.frames {
            frame.effects.clear();
        }
    }

    /// Every retained effect, for diagnosis. Does not detach them.
    #[must_use]
    pub fn retained_effects(&self) -> Vec<&Effect> {
        self.frames.iter().flat_map(Frame::effects).collect()
    }

    const fn mint_id(&mut self) -> FrameId {
        self.next_sequence += 1;
        FrameId(self.next_sequence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_DEPTH: u32 = crate::transaction::MAX_SAVEPOINT_DEPTH;

    fn opened() -> FrameStack {
        let mut stack = FrameStack::new(MAX_DEPTH);
        stack.open_root();
        stack
    }

    /// The savepoint-shadowing arm, driven at the naming layer.
    ///
    /// It fails on a depth-derived name (`zs_sp_<depth>`), which is what the
    /// orchestrator emitted before dispatch moved onto this stack: open a child
    /// at depth 1, roll it back, open another at depth 1, and both are named
    /// `zs_sp_1` - so the second rollback resolves to whichever leftover
    /// PostgreSQL established most recently.
    ///
    /// Its peers one layer out are
    /// `transaction::tests::dispatch_emits_monotonic_savepoint_names_at_the_same_depth`
    /// (the dispatch entry points, on SQLite) and
    /// `sc1_driver::dispatch_emits_the_reducers_monotonic_savepoint_names`
    /// (live PostgreSQL, asserting the released name really is gone from the
    /// server).
    #[test]
    fn a_savepoint_name_is_never_reused_at_the_same_depth() {
        let mut stack = opened();
        let (first_id, first_name) = stack.open_child().unwrap();
        stack.confirm_child_open(first_id).unwrap();
        stack.mark_rolled_back(first_id).unwrap();
        stack.close_top(first_id, FrameClose::RolledBackTo).unwrap();

        // Same depth as the first child, so a depth-derived name repeats here.
        let (second_id, second_name) = stack.open_child().unwrap();
        assert_ne!(
            first_name, second_name,
            "a leftover savepoint of the reused name shadows the enclosing frame \
             and sends its rollback to the wrong scope"
        );
        assert_ne!(first_id, second_id);
        assert_eq!(
            stack.minted_names().len(),
            2,
            "invariant 9: a frame sequence or name never repeats"
        );
    }

    /// Invariant 11 over the two fates.
    ///
    /// Where this fails today: `FrameStack` did not exist. The release half
    /// fails on an implementation that drops a released child's effects; the
    /// rollback half fails on one that appends them to the parent anyway,
    /// which is the flat-buffer shape that tells a subscriber about a row that
    /// does not exist.
    #[test]
    fn release_moves_a_childs_effects_to_the_parent_and_rollback_discards_them() {
        let mut stack = opened();
        stack.queue_effect(Effect::new("notes", "root-1")).unwrap();

        let (released, _) = stack.open_child().unwrap();
        stack.confirm_child_open(released).unwrap();
        stack
            .queue_effect(Effect::new("notes", "released-1"))
            .unwrap();
        stack.close_top(released, FrameClose::Released).unwrap();

        let (rolled, _) = stack.open_child().unwrap();
        stack.confirm_child_open(rolled).unwrap();
        stack
            .queue_effect(Effect::new("notes", "rolled-1"))
            .unwrap();
        stack.mark_rolled_back(rolled).unwrap();
        stack.close_top(rolled, FrameClose::RolledBackTo).unwrap();

        let published: Vec<Box<str>> = stack
            .take_effects_for_confirmed_commit()
            .into_iter()
            .map(|effect| effect.payload)
            .collect();
        assert_eq!(
            published,
            vec!["root-1".into(), "released-1".into()],
            "a released child's effects reach the parent in order; a rolled-back \
             child's are discarded and no parent effect is touched"
        );
    }

    /// The FAILED-`ROLLBACK TO` row: effects are retained for diagnosis.
    ///
    /// Where this fails today: `FrameStack` did not exist. It fails on an
    /// implementation that discards on the ASSUMPTION the rollback will
    /// succeed - which the success-path arm above cannot catch, because a
    /// premature discard leaves it green while destroying the evidence this
    /// row requires.
    #[test]
    fn a_failed_rollback_to_retains_the_frames_effects_for_diagnosis() {
        let mut stack = opened();
        let (child, _) = stack.open_child().unwrap();
        stack.confirm_child_open(child).unwrap();
        stack.queue_effect(Effect::new("notes", "at-risk")).unwrap();

        // The `ROLLBACK TO` FAILED, so `mark_rolled_back` is never called and
        // no `RELEASE` follows. The frame and its effects both remain.
        assert_eq!(stack.depth(), 2, "the frame is not closed");
        let retained: Vec<&str> = stack
            .retained_effects()
            .iter()
            .map(|effect| &*effect.payload)
            .collect();
        assert_eq!(
            retained,
            vec!["at-risk"],
            "the failure row keeps the frame's effects for diagnosis"
        );

        // Root cleanup finally discards them; nothing publishes.
        stack.discard_all_effects();
        assert!(stack.retained_effects().is_empty());
    }

    /// A rolled-back frame is not closed until its `RELEASE` lands.
    ///
    /// Where this fails today: `FrameStack` did not exist. It fails on an
    /// implementation that pops on `ROLLBACK TO`, which reports the
    /// transaction available for new work one statement early.
    #[test]
    fn a_rolled_back_frame_stays_on_the_stack_until_its_release_lands() {
        let mut stack = opened();
        let (child, _) = stack.open_child().unwrap();
        stack.confirm_child_open(child).unwrap();
        stack.mark_rolled_back(child).unwrap();

        assert_eq!(stack.depth(), 2);
        assert_eq!(stack.top().unwrap().status(), FrameStatus::RolledBack);
        assert_eq!(
            stack.queue_effect(Effect::new("notes", "too-early")),
            Err(FrameError::SavepointNotCurrent),
            "a rolled-back frame is not available for new work"
        );

        stack.close_top(child, FrameClose::RolledBackTo).unwrap();
        assert_eq!(stack.depth(), 1);
    }

    /// Only the top acts, the root cannot close, and the cap is the existing
    /// public one.
    ///
    /// Where this fails today: `FrameStack` did not exist. Each assertion
    /// fails on the guard being absent; none of them mutates the stack, which
    /// the depth assertions after each rejection check.
    #[test]
    fn the_frame_guards_refuse_without_mutating_the_stack() {
        let mut stack = opened();
        let root = stack.top().unwrap().id();
        assert_eq!(
            stack.close_top(root, FrameClose::Released),
            Err(FrameError::SavepointRootCannotClose)
        );
        assert_eq!(stack.depth(), 1);

        let (child, _) = stack.open_child().unwrap();
        stack.confirm_child_open(child).unwrap();
        assert_eq!(
            stack.close_top(root, FrameClose::Released),
            Err(FrameError::SavepointNotCurrent),
            "a non-top parent is SavepointNotCurrent"
        );
        assert_eq!(stack.depth(), 2);
    }

    /// The ninth simultaneous child is refused.
    ///
    /// Where this fails today: `FrameStack` did not exist. The fixture opens
    /// children WITHOUT closing any, so the cap it reaches is the
    /// simultaneous-open depth - the thing the cap is about - rather than the
    /// monotonic sequence, which is uncapped by design.
    #[test]
    fn a_ninth_simultaneous_child_is_refused_while_the_sequence_runs_on() {
        let mut stack = opened();
        for _ in 0..MAX_DEPTH {
            let (id, _) = stack.open_child().expect("within the cap");
            stack.confirm_child_open(id).unwrap();
        }
        assert_eq!(stack.depth(), MAX_DEPTH as usize + 1, "root plus MAX children");
        assert_eq!(
            stack.open_child().map(|(id, _)| id),
            Err(FrameError::SavepointDepthExceeded)
        );
        assert_eq!(stack.depth(), MAX_DEPTH as usize + 1, "the guard did not mutate");

        // Close one and the sequence keeps climbing: the cap governs
        // simultaneous depth, never naming.
        let top = stack.top().unwrap().id();
        let before = stack.minted_names().len();
        stack.close_top(top, FrameClose::Released).unwrap();
        let (_, name) = stack.open_child().expect("a slot freed up");
        assert_eq!(stack.minted_names().len(), before + 1);
        assert!(
            !stack
                .minted_names()
                .iter()
                .filter(|minted| **minted != name)
                .any(|minted| **minted == *name),
            "the new name is fresh"
        );
    }
}
