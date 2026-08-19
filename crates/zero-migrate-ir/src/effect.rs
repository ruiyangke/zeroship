//! What ONE lowered step does to the catalog facts an assertion can read.
//!
//! Step 5 of `docs/proposals/single-fold-and-effects.md` section G. The governing
//! identity is `state_at(N) = live_at_0 (+) fold(effects[0..N])`, and the proposal's
//! point is that THE TWO TERMS ARE NOT EQUALLY KNOWABLE.
//!
//! # What this type is, and what it deliberately is not
//!
//! This is the OBSTRUCTION PROJECTION of a step's meaning, not the step's full state
//! delta. The full delta is what `render::fold::fold_ops_onto` already computes, and
//! `state_at(N)` is spelled in terms of that rather than in terms of
//! this enum. Both are derived from the SAME op vocabulary, which is the whole of
//! decision 1: if the effect model is wrong about what an op removes, the fold is
//! wrong about it too, and the snapshot corpus catches it.
//!
//! Being exact about the identity, because it is easy to oversell: `Effect` is a
//! two-valued classification, so `fold` of a run of `Effect`s does NOT reconstruct a
//! schema. The op stream is the effect stream; this enum is the answer to one
//! question asked of each element of it.
//!
//! # The question it answers
//!
//! "Can this step REMOVE a catalog fact that an obstruction assertion reads?" - a
//! `pg_depend` edge pointing at a column, an inheritance link, or a partition-key
//! membership.
//!
//! It is derived from the OP, above the dialect boundary, which is what lets the
//! engine answer it without a SQL parser. The shape that forces the question is
//! `CREATE OR REPLACE VIEW`: it reads as a creation and silently recomputes the
//! view's dependency edges, so a body that stops reading a column removes that
//! column's blocker with no `DROP` anywhere. At the SQL level that ambiguity needs a
//! whitelist to resolve. At the op level it does not exist -
//! [`Op::CreateView`](crate::ir::Op::CreateView) carries `replace` as a NAMED FIELD.
//!
//! # Direction of the unknown
//!
//! A step with no op provenance - a `.sql` migration, a declarative plan, a
//! hand-built [`crate::migration::Migration`] - carries `None` rather than an
//! `Effect`, and every consumer reads `None` as [`Effect::MayRemove`]. That is the
//! same direction the SQL whitelist failed in: toward the per-migration seam, i.e.
//! toward today's behaviour, never toward a new refusal.

/// What a step does to the catalog facts an obstruction assertion reads.
///
/// See the module docs. Two-valued on purpose: the question is one boolean per step,
/// and the correction this makes to the shipped design is WHERE THE BOOLEAN IS
/// DERIVED FROM, not how many bits it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Effect {
    /// The step provably only ADDS catalog facts. It cannot remove a `pg_depend`
    /// edge pointing at an existing column, cannot clear an inheritance link, and
    /// cannot clear a partition-key membership - so an obstruction assertion's
    /// pre-plan answer is still the answer a later step will meet.
    AddsOnly,
    /// The step can remove one of those facts, so a pre-plan answer may be stale by
    /// the time the later step runs. Also the reading for an op whose SQL the engine
    /// does not generate itself
    /// ([`Op::PgRaw`](crate::ir::Op::PgRaw)) and for a step with no op provenance.
    MayRemove,
}

impl Effect {
    /// Whether this step provably clears no obstruction.
    ///
    /// The spelling the prefix test reads, so the `None`-means-[`Effect::MayRemove`]
    /// rule lives in exactly one place: `Option::is_some_and(Effect::adds_only)`.
    #[must_use]
    pub const fn adds_only(self) -> bool {
        matches!(self, Self::AddsOnly)
    }
}
