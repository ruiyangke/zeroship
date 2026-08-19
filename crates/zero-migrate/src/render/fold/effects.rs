//! **The effect model over the finished fold.** Step 5 of
//! `docs/proposals/single-fold-and-effects.md` section G.
//!
//! Two things live here, and the difference between them is the proposal's whole
//! answer:
//!
//! * `state_at` - `state_at(N) = live_at_0 (+) fold(effects[0..N])`, the state the
//!   plan's `N`th step will meet;
//! * `effect_of` - what ONE op does to the catalog facts an OBSTRUCTION assertion
//!   reads.
//!
//! (Backticks rather than intra-doc links throughout this `//!` block, for the reason
//! `single_fold`'s module doc records: a module-level comment resolves paths in the
//! PARENT module's scope, so a bare `[state_at]` here is looked up in `render::fold`
//! and dangles. The rustdoc gate counts those, and it caught three of them on the
//! first run of this very module.)
//!
//! # The two terms are not equally knowable
//!
//! `live_at_0` is the introspected [`SchemaSnapshot`], which the engine already
//! builds. The prefix delta is the same replay `fold_ops_onto` performs. So for an
//! assertion that ranges over objects the model NAMES - a table, a column, a row
//! count - `state_at(N)` answers it exactly.
//!
//! An OBSTRUCTION assertion does not range over those. It ranges over `pg_depend`
//! EDGES, inheritance links and partition-key memberships, and the blocker set
//! includes objects this engine never created: a DBA's view, another application's
//! foreign key, an inheritance child. Those are not in the model and CANNOT BE. The
//! effect model can prove a plan REMOVES a named blocker; it cannot ENUMERATE the
//! blocker set. **A live query at step 0 is still required**, and
//! `apply::plan_precondition::answerability` is what keeps that true.
//!
//! So this module does not retire the classification. It changes what the
//! classification's axis MEANS, and it changes where the prefix test gets its answer
//! from - the op, not the rendered SQL.
//!
//! # Why the op and not the SQL
//!
//! `CREATE OR REPLACE VIEW` recomputes the view's dependency edges, so a body that
//! stops reading a column removes that column's blocker with no `DROP` anywhere. A
//! parse tree reads it as a creation. Resolving that at the SQL level took a
//! WHITELIST plus a `_ => false` fallback for every shape nobody had thought about.
//!
//! At the op level the ambiguity does not exist: `Op::CreateView` carries
//! `replace` as a NAMED FIELD, and so does `Op::CreateFunction`. `effect_of` gets
//! the case right by construction, and its match is EXHAUSTIVE - a new `Op` variant
//! is a COMPILE ERROR here rather than a silent fall-through to a guess.
//!
//! That exhaustiveness is the deleted fallback. It is not the same thing as the
//! `Option` on a step: a step with no op provenance carries no effect at all, which
//! is a different question with the same fail-closed answer.

use zero_migrate_ir::effect::Effect;

use zero_migrate_policy::EffectivePolicy;

use crate::model::ir::{MigrationIr, Op};
use crate::model::snapshot::SchemaSnapshot;
use crate::render::fold::{fold_ops_onto, FoldError};
use crate::SqlDialect;

/// The state the plan's `n`th step will meet: the live schema at step 0, advanced by
/// the ops the first `n` steps replay.
///
/// This is the governing identity of section E spelled as code. `base` is
/// `live_at_0` - pass the [`SchemaSnapshot`] the engine introspected under the held
/// lock, or [`SchemaSnapshot::default`] for the pure-offline reading. `n` counts OPS,
/// which is the unit an effect is attached to; `n >= ops.len()` yields the final
/// state.
///
/// It is a REFOLD rather than an incremental delta. The proposal flags the cost as
/// unmeasured (`decision 8`), and this is the shape that is obviously correct; if it
/// ever proves too slow the incremental form is an optimisation of a function whose
/// answer is already pinned by tests.
///
/// # Errors
/// Any [`FoldError`] the structural catalog replay reports, unchanged - a stream the
/// catalog refuses yields no state rather than a partial one.
pub fn state_at(
    base: &SchemaSnapshot,
    ops: &[Op],
    n: usize,
    dialect: SqlDialect,
    project_schema: &str,
    effective: &EffectivePolicy,
) -> Result<SchemaSnapshot, FoldError> {
    let prefix = &ops[..n.min(ops.len())];
    fold_ops_onto(base, prefix, dialect, project_schema, effective)
}

/// [`state_at`] for a whole lowered artifact's op list.
///
/// The spelling a caller holding a [`MigrationIr`] wants, so the `ops` field access
/// and the dialect pairing are not repeated at every call site.
///
/// # Errors
/// See [`state_at`].
pub fn ir_state_at(
    base: &SchemaSnapshot,
    ir: &MigrationIr,
    n: usize,
    dialect: SqlDialect,
    project_schema: &str,
    effective: &EffectivePolicy,
) -> Result<SchemaSnapshot, FoldError> {
    state_at(base, &ir.ops, n, dialect, project_schema, effective)
}

/// What ONE op does to the catalog facts an obstruction assertion reads.
///
/// EXHAUSTIVE, with no `_` arm: this is section H's op-exhaustiveness requirement
/// applied to the effect model, and it is what replaces the SQL whitelist's
/// `_ => false`. A variant added to [`Op`] fails the build here, where the question
/// "can this remove a dependency edge?" is answerable, rather than silently
/// defaulting.
///
/// # The refusal direction
///
/// [`Effect::MayRemove`] is the conservative answer: it DISARMS the hoist and leaves
/// the assertion on the per-migration seam, which is today's behaviour. So a variant
/// classified wrongly as `MayRemove` costs a capability; one classified wrongly as
/// [`Effect::AddsOnly`] costs a WRONG REFUSAL. Every `AddsOnly` below is therefore a
/// claim about PostgreSQL's catalog, and the ones the SQL whitelist disagreed with
/// are adjudicated against a live server in
/// `tests/pg_engine/pg_plan_precondition_preflight.rs`.
#[must_use]
#[allow(clippy::match_same_arms)]
pub fn effect_of(op: &Op) -> Effect {
    match op {
        // ---------------------------------------------------------------
        // Creation. A brand-new object has no pre-existing edge to remove.
        // ---------------------------------------------------------------
        Op::CreateTable { .. }
        // `CREATE TABLE ... PARTITION OF` RAISES the new child's `attinhcount`
        // rather than lowering anybody's.
        | Op::CreatePartition { .. }
        // ATTACH likewise only raises it.
        | Op::AttachPartition { .. }
        | Op::AddColumn { .. }
        | Op::CreateIndex { .. }
        | Op::AddConstraint { .. }
        | Op::ValidateConstraint { .. }
        | Op::CreateEnum { .. }
        | Op::CreateDomain { .. }
        | Op::CreateSequence { .. }
        | Op::CreateSchema { .. }
        | Op::CreateExtension { .. }
        | Op::CreatePolicy { .. }
        | Op::CreateTrigger { .. } => Effect::AddsOnly,

        // ---------------------------------------------------------------
        // Column facets that add or relax without touching another object's
        // dependency on the column.
        // ---------------------------------------------------------------
        Op::SetColumnNotNull { .. }
        | Op::DropColumnNotNull { .. }
        // Reloptions (`fillfactor`, autovacuum, …) and the storage/RLS toggles.
        // None is a `pg_depend` edge, an inheritance link, or a partition key.
        | Op::SetTableOptions { .. }
        | Op::SetRls { .. }
        // A comment is a `pg_description` row, not a dependency.
        | Op::Comment { .. } => Effect::AddsOnly,

        // ---------------------------------------------------------------
        // Privileges and roles. A privilege is an ACL entry on the object, not a
        // `pg_depend` edge on a column - which is why REVOKE is here beside GRANT.
        // `DROP ROLE`, by contrast, can cascade, and `DROP OWNED BY` is a removal
        // by name.
        // ---------------------------------------------------------------
        Op::Grant { .. } | Op::Revoke { .. } | Op::CreateRole { .. } | Op::AlterRole { .. } => {
            Effect::AddsOnly
        }

        // ---------------------------------------------------------------
        // Rows. Not catalog facts at all: no row change can remove a `pg_depend`
        // edge, an inheritance link, or a partition-key membership.
        // ---------------------------------------------------------------
        Op::Insert { .. } | Op::Update { .. } | Op::Delete { .. } | Op::Backfill { .. } => {
            Effect::AddsOnly
        }

        // ---------------------------------------------------------------
        // DECISION 7. The two shapes that read as creations and are not.
        // ---------------------------------------------------------------
        //
        // A REPLACE recomputes the object's dependency edges, so a body that stops
        // reading a column removes that column's blocker with no `DROP` anywhere.
        // This is the case the SQL whitelist existed for, and the op answers it
        // from a named field instead of from a parse tree.
        //
        // A materialized view has no `CREATE OR REPLACE` form at all - the renderer
        // refuses `materialized + replace` fail-closed - so it can only ever be the
        // additive leg.
        Op::CreateView { replace, .. } | Op::CreateFunction { replace, .. } => {
            if replace.unwrap_or(false) {
                Effect::MayRemove
            } else {
                Effect::AddsOnly
            }
        }

        // ---------------------------------------------------------------
        // Removal by name. Each really can clear an obstruction.
        // ---------------------------------------------------------------
        Op::DropTable { .. }
        | Op::DropColumn { .. }
        | Op::DropIndex { .. }
        | Op::DropConstraint { .. }
        | Op::DropView { .. }
        | Op::DropEnum { .. }
        | Op::DropDomain { .. }
        | Op::DropSequence { .. }
        | Op::DropSchema { .. }
        | Op::DropExtension { .. }
        | Op::DropRole { .. }
        | Op::DropOwnedBy { .. }
        | Op::DropPolicy { .. }
        | Op::DropTrigger { .. }
        | Op::DropFunction { .. } => Effect::MayRemove,

        // A rename removes a NAME as well as creating one, and this engine's
        // renames are carrier-following: the old spelling disappears from every
        // expression that held it.
        Op::RenameTable { .. } | Op::RenameColumn { .. } => Effect::MayRemove,

        // `ALTER COLUMN ... TYPE` rebuilds whatever depended on the old type;
        // `SET`/`DROP DEFAULT` replaces the `pg_attrdef` entry and its edges.
        Op::SetColumnType { .. }
        | Op::SetColumnDefault { .. }
        | Op::DropColumnDefault { .. } => Effect::MayRemove,

        // DETACH clears both the inheritance link and the partition-key membership
        // on the detached child; DROP takes the child with them.
        Op::DetachPartition { .. } | Op::DropPartition { .. } => Effect::MayRemove,

        // A primary-key replacement drops a constraint AND its index. Identity
        // synchronization is conservative rather than measured: it renders against
        // the live catalog at apply time, so the step carries the intent and not
        // the statements, and `plan_precondition` reads both step kinds as removals
        // for that reason.
        Op::AlterPrimaryKey { .. } | Op::SynchronizeIdentity { .. } => Effect::MayRemove,

        // `ALTER SEQUENCE ... OWNED BY NONE` REMOVES the `pg_depend` edge between a
        // sequence and the column that owns it. That is exactly the fact an
        // obstruction assertion reads, so the whole variant is a removal even
        // though its commoner forms (`RESTART`, `INCREMENT`) are not.
        Op::AlterSequence { .. } => Effect::MayRemove,

        // The one genuine escape: SQL this engine did not generate and cannot
        // enumerate. Undecidable, permanently, exactly like `Precondition::SqlBoolean`.
        Op::PgRaw { .. } => Effect::MayRemove,

        // Never reached: `flatten_dialectal_ops` selects the leg before any
        // consumer sees an op, and `lower_one_op` refuses a wrapper outright. Fail
        // closed rather than assert, so a future caller that forgets to flatten
        // loses a hoist instead of gaining a wrong one.
        Op::Dialectal { .. } => Effect::MayRemove,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(source: &str) -> Op {
        serde_json::from_str(source)
            .unwrap_or_else(|error| panic!("{source} must parse as an Op: {error}"))
    }

    /// Ops that provably only ADD catalog facts.
    ///
    /// The last four are the ones the deleted SQL whitelist could NOT prove
    /// additive - it answered `false` for each, disarming the hoist and letting a
    /// plan half-apply. Measured on `839b9aca` before the change, not assumed.
    /// `CREATE MATERIALIZED VIEW` is adjudicated against a live server in
    /// `tests/pg_engine/pg_plan_precondition_preflight.rs`.
    #[test]
    fn the_additive_ops_clear_nothing() {
        for source in [
            r#"{"op":"addColumn","table":"t","column":"c","type":"text"}"#,
            r#"{"op":"validateConstraint","table":"t","name":"f"}"#,
            r#"{"op":"setColumnNotNull","table":"t","column":"c"}"#,
            r#"{"op":"dropColumnNotNull","table":"t","column":"c"}"#,
            r#"{"op":"createSchema","name":"other"}"#,
            r#"{"op":"createExtension","name":"citext"}"#,
            // A view that is NOT a replace. Decision 7's additive leg.
            r#"{"op":"createView","name":"v","query":{"kind":"structured",
                "select":{"from":{"name":"t"},"projection":[]}}}"#,
            // THE ONES THE WHITELIST GOT WRONG, measured on the unchanged tree.
            r#"{"op":"createView","name":"mv","materialized":true,
                "query":{"kind":"structured",
                "select":{"from":{"name":"t"},"projection":[]}}}"#,
            r#"{"op":"createRole","name":"r"}"#,
        ] {
            assert_eq!(
                effect_of(&op(source)),
                Effect::AddsOnly,
                "{source} adds only, so an obstruction assertion's pre-plan answer \
                 survives it"
            );
        }
    }

    /// Ops that can remove a dependency edge, an inheritance link or a
    /// partition-key membership.
    #[test]
    fn the_removing_ops_do_not_clear_nothing() {
        for source in [
            r#"{"op":"dropColumn","table":"t","column":"c"}"#,
            r#"{"op":"dropTable","table":"t"}"#,
            r#"{"op":"dropConstraint","table":"t","name":"ck"}"#,
            r#"{"op":"dropView","name":"v"}"#,
            r#"{"op":"setColumnType","table":"t","column":"c","toType":"bigInt"}"#,
            r#"{"op":"dropColumnDefault","table":"t","column":"c"}"#,
            r#"{"op":"renameTable","table":"t","to":"u"}"#,
            r#"{"op":"renameColumn","table":"t","from":"a","to":"b","type":"int"}"#,
            r#"{"op":"dropOwnedBy","roles":["r"]}"#,
        ] {
            assert_eq!(
                effect_of(&op(source)),
                Effect::MayRemove,
                "{source} can clear an obstruction, so the check must land back on the \
                 per-migration seam"
            );
        }
    }

    /// DECISION 7, isolated. The two shapes that read as creations and are not.
    ///
    /// The ONLY thing separating the two verdicts is a named field on the op. A
    /// parse tree needs `ViewStmt.replace`; the op has it without parsing anything.
    #[test]
    fn a_replace_is_a_removal_and_a_create_is_not() {
        let head = r#"{"op":"createView","name":"v","query":{"kind":"structured",
            "select":{"from":{"name":"t"},"projection":[]}}"#;
        assert_eq!(effect_of(&op(&format!("{head}}}"))), Effect::AddsOnly);
        assert_eq!(
            effect_of(&op(&format!(r#"{head},"replace":true}}"#))),
            Effect::MayRemove,
            "CREATE OR REPLACE VIEW recomputes the view's dependency edges, so it \
             removes a column's blocker with no DROP anywhere"
        );
    }

    /// A dialectal wrapper must never be classified as additive. It is refused
    /// before lower and flattened before every other consumer, so this is the
    /// fail-closed answer for a caller that forgets to flatten.
    #[test]
    fn an_unflattened_dialectal_wrapper_is_a_removal() {
        assert_eq!(
            effect_of(&op(
                r#"{"op":"dialectal","pg":[{"op":"addColumn","table":"t",
                    "column":"c","type":"text"}]}"#
            )),
            Effect::MayRemove
        );
    }

    fn table(name: &str) -> Op {
        op(&format!(
            r#"{{"op":"createTable","name":{name:?},"columns":[
                {{"name":"id","type":"int","nullable":false}}],"primaryKey":["id"]}}"#
        ))
    }

    fn fold_prefix(base: &SchemaSnapshot, ops: &[Op], n: usize) -> SchemaSnapshot {
        state_at(
            base,
            ops,
            n,
            SqlDialect::Postgres,
            "public",
            &crate::test_fixtures::no_inject("public"),
        )
        .unwrap_or_else(|error| panic!("a prefix of {n} must fold: {error:?}"))
    }

    /// `state_at(0)` is `live_at_0` untouched - the first term of the identity
    /// standing alone, with no prefix applied.
    #[test]
    fn state_at_zero_is_the_live_schema_it_was_seeded_with() {
        let base = SchemaSnapshot::default();
        assert_eq!(fold_prefix(&base, &[table("t")], 0), base);
    }

    /// The prefix delta is EXACT for objects the model names - which is what makes
    /// the five existence assertions answerable at all, and is the half of the
    /// identity that IS knowable offline.
    #[test]
    fn state_at_advances_one_op_at_a_time_and_saturates() {
        let ops = vec![
            table("t"),
            op(r#"{"op":"addColumn","table":"t","column":"later","type":"text"}"#),
        ];
        let base = SchemaSnapshot::default();

        assert!(!fold_prefix(&base, &ops, 0).tables.contains_key("t"));

        let one = fold_prefix(&base, &ops, 1);
        assert!(one.tables.contains_key("t"), "the first op has run");
        assert!(
            !one.tables["t"].columns.iter().any(|c| c.name == "later"),
            "the SECOND op has not run at state_at(1) - a prefix, not the whole plan"
        );

        let two = fold_prefix(&base, &ops, 2);
        assert!(two.tables["t"].columns.iter().any(|c| c.name == "later"));
        assert_eq!(
            fold_prefix(&base, &ops, 99),
            two,
            "a prefix past the end saturates at the final state rather than erroring"
        );
    }

    /// The OTHER half of the identity: `live_at_0` carries objects the plan never
    /// created, and `state_at` must not lose them.
    ///
    /// This is the property that makes the obstruction limit concrete. The engine
    /// can carry an object it did not create through the fold; what it CANNOT do is
    /// enumerate the `pg_depend` edges pointing at a column, because the blocker set
    /// includes objects that never appear in any op stream at all.
    #[test]
    fn state_at_carries_the_base_it_did_not_create() {
        let seeded = fold_prefix(&SchemaSnapshot::default(), &[table("pre")], 1);
        let later = fold_prefix(&seeded, &[table("fresh")], 1);

        assert!(
            later.tables.contains_key("pre"),
            "an object the prefix never created must survive the fold"
        );
        assert!(later.tables.contains_key("fresh"));
    }
}
