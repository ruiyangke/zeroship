//! Which of a plan's live-database preconditions are answerable BEFORE the plan
//! starts, and which are not.
//!
//! # The defect this exists for
//!
//! The engine's atomicity boundary is the STEP, not the plan. One lowered unit
//! runs `BEGIN; SET LOCAL ...; <up>; INSERT journal; COMMIT`, so a plan of several
//! units commits several times. Every [`Precondition`] is evaluated inside the
//! per-migration loop, immediately before that migration's `up` - by which time
//! the earlier units have already committed. A live-database check therefore
//! cannot prevent a half-applied PLAN; it can only prevent a half-applied
//! MIGRATION. `Op::DropColumn`'s
//! [`Precondition::ColumnHasNoBlockingDependents`] is the shipped case: in an
//! ordinary `addColumn` + `dropColumn` envelope the refusal arrives after the
//! added column is committed, leaving a schema that is neither the old shape nor
//! the new one.
//!
//! # The asymmetry that bounds the answer
//!
//! The plan-wide phase is a REFUSAL and nothing else. The per-migration
//! evaluation stays exactly where it is and remains the only thing that decides
//! whether a migration APPLIES - [`crate::apply::executor`]'s second pass calls
//! `evaluate_preconditions` unconditionally for every pending migration, and
//! nothing this module produces reaches it. So the hoist can only ever be wrong
//! IN ONE DIRECTION: by REFUSING a plan that would have succeeded. It cannot be
//! wrong by admitting one.
//!
//! A hoisted refusal is wrong exactly when the assertion is UNMET against the
//! pre-plan database and an EARLIER step of the same plan REPAIRS it - flips it
//! to met. `[dropView, dropColumn]` is the ordinary shape: the view the catalog
//! reports as the blocker is removed by the plan itself, one step earlier.
//!
//! # The classification: does the assertion range over objects the model carries?
//!
//! Classifying by VARIANT ALONE is measurably wrong - every variant can be
//! flipped from unmet to met by SOME earlier step. The variants do split cleanly,
//! but NOT on the axis this module first shipped with.
//!
//! The axis used to be "additive versus removal" - a property of how the assertion
//! responds to the ordinary work a plan does. Since the effect model it is **does
//! this assertion range over objects the model carries** - a property of the
//! MODEL'S CLOSURE. The two agree on today's verdicts and disagree about why, and
//! the second one is the reason the answer cannot be improved by trying harder:
//!
//! - An **obstruction** assertion - [`Precondition::ColumnHasNoBlockingDependents`]
//!   and [`Precondition::ColumnTypeChangeHasNoBlockers`] - ranges over `pg_depend`
//!   EDGES, inheritance links and partition-key memberships. Those are not in the
//!   schema model and CANNOT BE, because the blocker set includes objects this
//!   engine never created: a DBA's view, another application's foreign key, an
//!   inheritance child. The effect model can prove a plan REMOVES a named blocker.
//!   It cannot ENUMERATE the blocker set. **A live query at step 0 is still
//!   required**, and that is what this phase does. It can only be repaired by
//!   REMOVING something, so a prefix that removes nothing leaves the pre-plan
//!   answer valid. (An earlier step can BREAK one - `CREATE VIEW` over the column
//!   does - but that direction is met-to-unmet, which the per-migration seam still
//!   catches, exactly as today.)
//! - An **existence** assertion - [`Precondition::TableExists`],
//!   [`Precondition::TableNotExists`], [`Precondition::ColumnExists`],
//!   [`Precondition::ColumnNotExists`], [`Precondition::RowCount`] - ranges over
//!   objects the model NAMES. `render::fold::effects::state_at` answers them
//!   exactly, given the introspected schema at step 0 and the ops the prefix
//!   replays. They are STILL `Answerability::PlanDependent` here, and still never
//!   hoisted, because hoisting them is a NEW GATE that can refuse a plan which
//!   previously applied - step 6 of the proposal, deliberately behind a flag and
//!   deliberately after `state_at` has been trusted in production. What changed at
//!   step 5 is that they became ANSWERABLE; what has not changed is that this
//!   module does not yet answer them. They stay `Answerability::PlanDependent`.
//! - [`Precondition::SqlBoolean`] is untrusted opaque SQL. The engine cannot
//!   enumerate what it reads, so no earlier step can be proven not to repair it,
//!   and hoisting would run untrusted SQL an extra time. Undecidable, permanently.
//!
//! # The prefix test reads the OP, not the rendered SQL
//!
//! The question is still ONE boolean per step - "can this step clear an
//! obstruction?" - and this module still does not maintain a per-object ledger. The
//! change is WHERE THE BOOLEAN COMES FROM.
//!
//! It used to come from parsing each step's rendered `up` with PostgreSQL's parser
//! and matching the parse tree against a whitelist. That had three costs, and all
//! three are gone:
//!
//! 1. it made core code hold a VENDOR PARSER and apply it dialect-blind, in a module
//!    with no [`SqlDialect`](crate::SqlDialect) reference of its own;
//! 2. it needed a whitelist at all, because a parse tree cannot tell `CREATE VIEW`
//!    from `CREATE OR REPLACE VIEW` - the shape that silently recomputes a view's
//!    dependency edges and so removes a column's blocker with no `DROP` anywhere;
//! 3. its `_ => false` fallback answered for every shape nobody had thought about,
//!    and it answered CONSERVATIVELY - which is safe, but was measurably
//!    over-conservative on five shapes this engine really emits, including
//!    `CREATE MATERIALIZED VIEW`. Each one silently disarmed the hoist and let a
//!    plan half-apply.
//!
//! The verdict now comes from [`Effect`], stamped on each unit at IR-lower time from
//! the op it was lowered from. `Op::CreateView` carries `replace` as a NAMED FIELD,
//! so the case the whitelist existed for is right by construction, and the match in
//! `render::fold::effects::effect_of` is EXHAUSTIVE - a new `Op` variant is a
//! compile error rather than a silent guess.
//!
//! # Direction of every unknown
//!
//! A step with NO op provenance - a `.sql` migration, a declarative plan, the
//! empty-plan journal anchor, a hand-built [`PlanStep`] - carries no [`Effect`], and
//! [`clears_no_obstruction`] reads that absence as "may remove". So does
//! `Op::PgRaw`, whose SQL this engine did not generate. Every unknown therefore
//! falls toward TODAY'S BEHAVIOUR - the precondition lands back on the
//! per-migration seam - rather than toward a new refusal.
//!
//! Note the two are different questions with the same answer, and the proposal's
//! "delete the `_ => false` fallback" means the FIRST one only. An exhaustive op
//! match has no fallback; an unstamped step has no op. Conflating them would claim
//! a guarantee this module does not have.

use std::collections::BTreeSet;

use zero_migrate_ir::effect::Effect;

use crate::model::precondition::{OnUnmet, Precondition};
use crate::render::step::PlanStep;

/// Whether an assertion is one this phase may hoist to the pre-plan database.
///
/// See the module docs. This is the axis the whole mechanism turns on, and it is
/// a property of the QUESTION, not of the plan.
///
/// **NOT retired by the effect model, and that is the honest answer** rather than a
/// missing feature. `state_at` makes the five existence variants ANSWERABLE; it
/// cannot make the two obstruction variants answerable, because their blocker set
/// includes objects the model never carried. So the classification survives, its
/// axis is redrawn, and its effect inverts: the variants that used to be excluded
/// for being repairable are now excluded only until step 6 turns them on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answerability {
    /// "Nothing is in the way." Ranges over catalog EDGES the model does not carry,
    /// so it needs a live query - and a plan prefix that removes nothing leaves that
    /// live answer valid.
    Obstruction,
    /// Answerable from the model (the five existence variants) or unknowable
    /// ([`Precondition::SqlBoolean`]). Never hoisted by THIS phase.
    PlanDependent,
}

/// How each variant responds to an earlier step's effects.
pub(crate) const fn answerability(check: &Precondition) -> Answerability {
    match check {
        // Both read `pg_depend` NORMAL edges pointing AT the column (plus, for the
        // retype, partition-key membership and inheritance). Every one of those is
        // an existing catalog fact that only a removal can clear, and NONE of them
        // is reachable from the schema model - which is exactly why this phase
        // exists and why an effect model cannot replace it.
        Precondition::ColumnHasNoBlockingDependents { .. }
        | Precondition::ColumnTypeChangeHasNoBlockers { .. } => Answerability::Obstruction,
        Precondition::TableExists { .. }
        | Precondition::TableNotExists { .. }
        | Precondition::ColumnExists { .. }
        | Precondition::ColumnNotExists { .. }
        | Precondition::RowCount { .. }
        | Precondition::SqlBoolean { .. } => Answerability::PlanDependent,
    }
}

/// One precondition judged answerable against the pre-plan database.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PlanStableCheck<'a> {
    /// The version the refusal is reported against - the same identity the
    /// per-migration seam uses, so the operator reads the same name.
    pub(crate) version: &'a str,
    /// The assertion to evaluate before any step runs.
    pub(crate) check: &'a Precondition,
}

/// The subset of `steps`' preconditions that may be evaluated against the
/// PRE-PLAN database, in plan order.
///
/// A precondition declared on the step at index `i` is included iff ALL of:
///
/// 1. its `on_unmet` is [`OnUnmet::Halt`]. [`OnUnmet::Skip`] means "leave THIS
///    migration pending and continue the batch" - a per-migration verdict with
///    no whole-plan reading, so there is no plan-level refusal for it to make.
///    Its own documentation records that a wrong `Skip` is quieter and worse than
///    a wrong `Halt`, which settles which way to fail;
/// 2. its [`answerability`] is [`Answerability::Obstruction`];
/// 3. `i > 0`. With no earlier step the hoist is a behavioural no-op that only
///    doubles the catalog reads. It is also why AUTHORED preconditions are
///    untouched by this phase on every path the engine builds: `render::lower`'s
///    `assemble_plan` attaches `ir.preconditions` to `steps[0]` and nowhere else,
///    and one lowered IR is one plan;
/// 4. the step is a [`PlanStep::Ddl`]. That is exactly the population the
///    executor's per-migration seam evaluates, so the hoist can only move a
///    refusal that already exists. `AlterPrimaryKey` and `SynchronizeIdentity`
///    carry a `preconditions` field that NOTHING evaluates today; judging one
///    here would invent a gate rather than move one;
/// 5. its version is not already satisfied in the journal (`satisfied`). The
///    executor's pending set is `set − completed − superseded`, and a migration
///    outside it never has its preconditions evaluated at all. Judging one here
///    would refuse a RETRY that succeeds today - see the retry deadlock the
///    single-variant retype preflight shipped with;
/// 6. every step at index `< i` [`clears_no_obstruction`].
///
/// **Slice order is execution order for engine-lowered plans only.** `assemble_plan`
/// stamps step versions monotone in ordinal, and `order_pending` is
/// version-tiebroken, so index order and execution order agree. A direct
/// [`crate::MigrationEngine::apply_plan`] caller that hand-builds a step vector
/// with non-monotone versions can have a later slice index execute first; the
/// consequence there is a hoist that should not have happened, i.e. the
/// over-refusal direction, never a missed refusal.
pub(crate) fn plan_stable_checks<'a>(
    steps: &'a [PlanStep],
    satisfied: &BTreeSet<String>,
) -> Vec<PlanStableCheck<'a>> {
    let mut hoisted = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        // The prefix is judged as we walk, so a step is never judged against
        // itself, and the walk stops at the first step that could clear an
        // obstruction: from there on nothing is answerable pre-plan, so there is
        // nothing left to parse for.
        if index > 0 {
            if let PlanStep::Ddl(m) = step {
                if !satisfied.contains(m.version.as_str()) {
                    hoisted.extend(
                        m.preconditions
                            .iter()
                            .filter(|pc| pc.on_unmet == OnUnmet::Halt)
                            .map(|pc| &pc.check)
                            .filter(|check| answerability(check) == Answerability::Obstruction)
                            .map(|check| PlanStableCheck {
                                version: m.version.as_str(),
                                check,
                            }),
                    );
                }
            }
        }
        if !clears_no_obstruction(step) {
            break;
        }
    }
    hoisted
}

/// Whether any step could contribute a plan-stable check, so the whole phase - a
/// journal read plus one parse per step - is skipped for the overwhelming
/// majority of plans, which declare no preconditions at all.
pub(crate) fn plan_declares_hoistable_shape(steps: &[PlanStep]) -> bool {
    steps.iter().skip(1).any(|step| {
        matches!(step, PlanStep::Ddl(m) if m.preconditions.iter().any(|pc| {
            pc.on_unmet == OnUnmet::Halt
                && answerability(&pc.check) == Answerability::Obstruction
        }))
    })
}

/// Whether this step provably cannot clear an obstruction: cannot remove a
/// `pg_depend` edge pointing at a column, cannot clear an inheritance link, and
/// cannot clear a partition-key membership.
///
/// Read off the [`Effect`] the lower stamped from the step's OP, never off the
/// rendered SQL. See the module docs for why the altitude matters: the shape that
/// forces the question is `CREATE OR REPLACE VIEW`, which reads as a creation while
/// it silently recomputes the view's dependency edges, and the op settles it with a
/// named `replace` field where a parse tree needed a whitelist.
///
/// An UNSTAMPED `Ddl` step answers `false`. That is the same fail-closed direction
/// the whitelist's `_ => false` had, for a different reason: no op provenance rather
/// than no matching shape.
pub(crate) fn clears_no_obstruction(step: &PlanStep) -> bool {
    match step {
        PlanStep::Ddl(m) => m.effect.is_some_and(Effect::adds_only),
        // A parameterized INSERT/UPDATE/DELETE over a structurally known table.
        // Rows are not catalog facts: no row change can remove a `pg_depend` edge,
        // an inheritance link, or a partition-key membership. The template is
        // built by `render::dml`, which emits those three statements and nothing
        // else, so a `Dml` step cannot carry DDL. Structurally true - these steps
        // carry no `Migration` and so no stamp.
        PlanStep::Dml { .. } | PlanStep::Backfill { .. } => true,
        // These render their DDL at APPLY time against the live catalog they are
        // validated by, so the step carries the intent and not the statements. A
        // primary-key replacement in particular drops a constraint and its index,
        // and an online rename ends by dropping the old column.
        //
        // Left as a step-kind verdict rather than routed through the stamp, and the
        // two agree: `effect_of` classifies `Op::AlterPrimaryKey`,
        // `Op::SynchronizeIdentity`, `Op::RenameColumn` and `Op::RenameTable` as
        // removals too. Pinned by `the_step_kind_verdicts_agree_with_the_op_model`
        // so the duplication cannot drift silently.
        PlanStep::AlterPrimaryKey(_)
        | PlanStep::AlterColumnType(_)
        | PlanStep::SynchronizeIdentity(_)
        | PlanStep::OnlineRename(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::migration::{
        Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId,
    };
    use crate::model::precondition::PreconditionCheck;

    /// A `Ddl` step carrying the effect the lower would have stamped on it.
    ///
    /// The `up` is now DECORATION for these tests - it is journalled and reported,
    /// but no longer read to decide anything. That is the change under test, and it
    /// is why several arms below pair an `up` with an effect the old SQL whitelist
    /// would have disagreed about.
    fn ddl(
        version: &str,
        up: &str,
        effect: Option<Effect>,
        checks: Vec<PreconditionCheck>,
    ) -> PlanStep {
        let mut m = Migration {
            version: MigrationId::derive(version, up.as_bytes()),
            name: version.to_string(),
            up: up.to_string(),
            down: None,
            checksum: Checksum::of(&ChecksumInput {
                up,
                down: None,
                flags: &MigrationFlags::default(),
                owner_app: "test",
                depends_on: &[],
                supersedes: &[],
                preconditions: &[],
            }),
            flags: MigrationFlags::default(),
            owner_app: "test".to_string(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: checks,
            existence_guard: None,
            effect,
        };
        m.checksum = Checksum::of(&ChecksumInput::from_migration(&m));
        PlanStep::Ddl(m)
    }

    /// A step the lower proved additive.
    fn adds(version: &str, up: &str, checks: Vec<PreconditionCheck>) -> PlanStep {
        ddl(version, up, Some(Effect::AddsOnly), checks)
    }

    /// A step the lower could not prove additive.
    fn removes(version: &str, up: &str, checks: Vec<PreconditionCheck>) -> PlanStep {
        ddl(version, up, Some(Effect::MayRemove), checks)
    }

    fn drop_blocked(table: &str, column: &str) -> Precondition {
        Precondition::ColumnHasNoBlockingDependents {
            table: table.to_string(),
            column: column.to_string(),
        }
    }

    /// The verdict is read off the STAMP, not off the `up`.
    ///
    /// Both arms pair an `up` with the OPPOSITE of what the deleted SQL whitelist
    /// would have said about it, so a reintroduced parser fails this test rather
    /// than passing it quietly. The first `up` is `CREATE MATERIALIZED VIEW`, which
    /// the whitelist could not prove additive and which a live PostgreSQL says
    /// clears nothing; the second is an `ADD COLUMN`, which it could.
    #[test]
    fn the_stamp_decides_and_the_rendered_sql_does_not() {
        assert!(
            clears_no_obstruction(&adds(
                "v0",
                r#"CREATE MATERIALIZED VIEW "s"."mv" AS SELECT "id" FROM "s"."t""#,
                vec![],
            )),
            "a step the lower proved additive must clear nothing whatever its SQL parses as"
        );
        assert!(
            !clears_no_obstruction(&removes(
                "v0",
                r#"ALTER TABLE "s"."t" ADD COLUMN "a" text"#,
                vec![],
            )),
            "a step the lower could not prove additive must send the check back to the \
             per-migration seam whatever its SQL parses as"
        );
    }

    /// A step with NO op provenance - a `.sql` migration, a declarative plan, the
    /// empty-plan anchor, a hand-built one - falls toward today's behaviour.
    #[test]
    fn an_unstamped_step_clears_nothing_provably() {
        let steps = vec![
            ddl("v0", r#"SELECT 1"#, None, vec![]),
            removes(
                "v1",
                r#"ALTER TABLE "s"."t" DROP COLUMN "doomed""#,
                vec![PreconditionCheck::halt(drop_blocked("t", "doomed"))],
            ),
        ];
        assert!(
            plan_stable_checks(&steps, &BTreeSet::new()).is_empty(),
            "an unstamped step carries no proof, so the hoist must be disarmed"
        );
    }

    /// The three step kinds decided by KIND rather than by stamp must agree with
    /// what the op model says about the ops that produce them. The duplication is
    /// deliberate - those steps carry intent rather than statements - but it must
    /// not be free to drift.
    #[test]
    fn the_step_kind_verdicts_agree_with_the_op_model() {
        use crate::model::ir::Op;
        use crate::render::fold::effects::effect_of;

        for source in [
            r#"{"op":"alterPrimaryKey","table":"t",
                "action":{"kind":"drop","expectedColumns":["id"]}}"#,
            r#"{"op":"renameColumn","table":"t","from":"a","to":"b","type":"int"}"#,
            r#"{"op":"synchronizeIdentity","table":"t","column":"id",
                "writesQuiesced":"import_window"}"#,
        ] {
            let op: Op = serde_json::from_str(source)
                .unwrap_or_else(|error| panic!("{source} must parse as an Op: {error}"));
            assert_eq!(
                effect_of(&op),
                Effect::MayRemove,
                "{source} produces a step kind `clears_no_obstruction` answers false for, \
                 so the op model must answer the same way"
            );
        }
    }

    #[test]
    fn only_the_obstruction_variants_are_ever_plan_stable() {
        assert_eq!(
            answerability(&drop_blocked("t", "c")),
            Answerability::Obstruction
        );
        assert_eq!(
            answerability(&Precondition::ColumnTypeChangeHasNoBlockers {
                table: "t".into(),
                column: "c".into()
            }),
            Answerability::Obstruction
        );
        for check in [
            Precondition::TableExists { table: "t".into() },
            Precondition::TableNotExists { table: "t".into() },
            Precondition::ColumnExists {
                table: "t".into(),
                column: "c".into(),
            },
            Precondition::ColumnNotExists {
                table: "t".into(),
                column: "c".into(),
            },
            Precondition::RowCount {
                table: "t".into(),
                op: crate::model::precondition::CmpOp::Eq,
                value: 0,
            },
            Precondition::SqlBoolean {
                sql: "SELECT true".into(),
            },
        ] {
            assert_eq!(
                answerability(&check),
                Answerability::PlanDependent,
                "{check:?} does not range over catalog edges the model cannot carry, so \
                 THIS phase is not what answers it"
            );
        }
    }

    /// The RED shape: an ordinary op that commits, then a blocked drop.
    #[test]
    fn a_drop_behind_an_add_is_plan_stable() {
        let steps = vec![
            adds(
                "v0",
                r#"ALTER TABLE "s"."t" ADD COLUMN "survivor" text"#,
                vec![],
            ),
            removes(
                "v1",
                r#"ALTER TABLE "s"."t" DROP COLUMN "doomed""#,
                vec![PreconditionCheck::halt(drop_blocked("t", "doomed"))],
            ),
        ];
        let hoisted = plan_stable_checks(&steps, &BTreeSet::new());
        assert_eq!(hoisted.len(), 1);
        assert_eq!(hoisted[0].check, &drop_blocked("t", "doomed"));
        assert!(plan_declares_hoistable_shape(&steps));
    }

    /// The over-refusal control: the plan removes its own blocker one step earlier.
    ///
    /// `CREATE OR REPLACE VIEW` is the second arm and is the whole of decision 7 -
    /// it reads as a creation and is not one. The op model gets it from
    /// `Op::CreateView`'s `replace` field, which is why the whitelist could go.
    #[test]
    fn a_drop_behind_a_removal_is_left_to_the_per_migration_seam() {
        for earlier in [
            r#"DROP VIEW "s"."reader""#,
            r#"CREATE OR REPLACE VIEW "s"."reader" AS SELECT 1 AS a"#,
        ] {
            let steps = vec![
                removes("v0", earlier, vec![]),
                removes(
                    "v1",
                    r#"ALTER TABLE "s"."t" DROP COLUMN "doomed""#,
                    vec![PreconditionCheck::halt(drop_blocked("t", "doomed"))],
                ),
            ];
            assert!(
                plan_stable_checks(&steps, &BTreeSet::new()).is_empty(),
                "{earlier} can remove the blocker, so the pre-plan answer is not the \
                 answer the step will meet"
            );
        }
    }

    /// A removal ANYWHERE in the prefix disarms the hoist, not just the step
    /// immediately before.
    #[test]
    fn a_removal_early_in_the_prefix_disarms_a_later_hoist() {
        let steps = vec![
            removes("v0", r#"DROP VIEW "s"."reader""#, vec![]),
            adds("v1", r#"ALTER TABLE "s"."t" ADD COLUMN "a" text"#, vec![]),
            removes(
                "v2",
                r#"ALTER TABLE "s"."t" DROP COLUMN "doomed""#,
                vec![PreconditionCheck::halt(drop_blocked("t", "doomed"))],
            ),
        ];
        assert!(plan_stable_checks(&steps, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn a_skip_check_is_never_plan_stable() {
        let steps = vec![
            adds("v0", r#"ALTER TABLE "s"."t" ADD COLUMN "a" text"#, vec![]),
            removes(
                "v1",
                r#"ALTER TABLE "s"."t" DROP COLUMN "doomed""#,
                vec![PreconditionCheck::skip(drop_blocked("t", "doomed"))],
            ),
        ];
        assert!(plan_stable_checks(&steps, &BTreeSet::new()).is_empty());
        assert!(!plan_declares_hoistable_shape(&steps));
    }

    #[test]
    fn a_first_step_check_is_left_where_it_is() {
        let steps = vec![removes(
            "v0",
            r#"ALTER TABLE "s"."t" DROP COLUMN "doomed""#,
            vec![PreconditionCheck::halt(drop_blocked("t", "doomed"))],
        )];
        assert!(
            plan_stable_checks(&steps, &BTreeSet::new()).is_empty(),
            "with no earlier step the hoist is a no-op that only doubles the reads"
        );
    }

    /// The executor's pending set is `set - completed - superseded`, so a
    /// satisfied step's preconditions are never evaluated. Judging one here would
    /// refuse a retry that succeeds today.
    #[test]
    fn a_journal_satisfied_step_is_not_judged_again() {
        let steps = vec![
            adds(
                "v0",
                r#"ALTER TABLE "s"."t" ADD COLUMN "survivor" text"#,
                vec![],
            ),
            removes(
                "v1",
                r#"ALTER TABLE "s"."t" DROP COLUMN "doomed""#,
                vec![PreconditionCheck::halt(drop_blocked("t", "doomed"))],
            ),
        ];
        let PlanStep::Ddl(second) = &steps[1] else {
            unreachable!()
        };
        let satisfied = BTreeSet::from([second.version.as_str().to_string()]);
        assert!(plan_stable_checks(&steps, &satisfied).is_empty());
    }

    /// A backfill or a DML step changes rows, and rows are not catalog facts.
    #[test]
    fn a_row_mutation_clears_no_obstruction() {
        let step = PlanStep::Dml {
            version: MigrationId::derive("dml", b"x"),
            checksum: Checksum::of(&ChecksumInput {
                up: "x",
                down: None,
                flags: &MigrationFlags::default(),
                owner_app: "test",
                depends_on: &[],
                supersedes: &[],
                preconditions: &[],
            }),
            name: "dml".into(),
            template: "UPDATE t SET a = $1".into(),
            binds: Vec::new(),
            target_schema: "s".into(),
            target_table: "t".into(),
            conflict_target: None,
            mutates_data: true,
            transactional: true,
            destructive: false,
            requires_approval: false,
            owner_app: "test".into(),
        };
        assert!(clears_no_obstruction(&step));
    }
}
