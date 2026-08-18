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
//! # The classification: OBSTRUCTION assertions, and everything else
//!
//! Classifying by VARIANT ALONE is measurably wrong - every variant can be
//! flipped from unmet to met by SOME earlier step. But the variants do split
//! cleanly on how they respond to the ordinary ADDITIVE work a plan does, and
//! that split is the whole mechanism:
//!
//! - An **obstruction** assertion - [`Precondition::ColumnHasNoBlockingDependents`]
//!   and [`Precondition::ColumnTypeChangeHasNoBlockers`] - asserts that NOTHING IS
//!   IN THE WAY of an operation on an object that must already exist. It can only
//!   be repaired by REMOVING something: a dependent object, a dependency edge, an
//!   inheritance link, a partition-key membership. A step that only ADDS catalog
//!   facts cannot repair it. (It can BREAK one - `CREATE VIEW` over the column
//!   does - but that direction is met-to-unmet, which the per-migration seam
//!   still catches, exactly as today.)
//! - An **existence** assertion - [`Precondition::TableExists`],
//!   [`Precondition::TableNotExists`], [`Precondition::ColumnExists`],
//!   [`Precondition::ColumnNotExists`], [`Precondition::RowCount`] - is repaired
//!   by precisely the additive work a plan is made of. `CREATE TABLE` is what
//!   makes `TableExists` true. For these, ADDITIVITY IS NOT INNOCENCE, it is the
//!   repair, so no prefix test can license hoisting one. They are answerable
//!   pre-plan only when there is no earlier step at all, which is a behavioural
//!   no-op.
//! - [`Precondition::SqlBoolean`] is untrusted opaque SQL. The engine cannot
//!   enumerate what it reads, so no earlier step can be proven not to repair it,
//!   and hoisting would run untrusted SQL an extra time.
//!
//! So the prefix test the engine actually needs is ONE boolean per step - "can
//! this step clear an obstruction?" - rather than a per-object effect ledger.
//! A ledger would have to be right about creation, deletion, column addition,
//! column removal and row counts independently, each silently wrong until some
//! plan exercised it, and each wrong in the over-refusal direction. One boolean
//! on the axis that matters is auditable by reading the whitelist once.
//!
//! # Direction of every unknown
//!
//! [`clears_no_obstruction`] is a WHITELIST. An `up` that does not parse, a
//! statement shape outside the list, an `ALTER TABLE` subcommand outside the
//! list, a `DO $$ ... $$` block whose DDL is inside a string, a structured step
//! whose SQL is generated at apply time - all answer `false`, which means the
//! precondition behind them is NOT hoisted and lands back on the per-migration
//! seam. Every unknown therefore falls toward TODAY'S BEHAVIOUR rather than
//! toward a new refusal.

use std::collections::BTreeSet;

use pg_query::protobuf::node::Node as NodeEnum;
use pg_query::protobuf::AlterTableType;

use crate::model::precondition::{OnUnmet, Precondition};
use crate::render::step::PlanStep;

/// Whether an assertion is one the plan's ADDITIVE work cannot satisfy.
///
/// See the module docs. This is the axis the whole mechanism turns on, and it is
/// a property of the QUESTION, not of the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answerability {
    /// "Nothing is in the way." Repaired only by REMOVAL, so a plan prefix that
    /// removes nothing leaves the pre-plan answer valid.
    Obstruction,
    /// Repaired by ordinary additive work, or unknowable. Never hoisted behind
    /// another step.
    PlanDependent,
}

/// How each variant responds to an earlier step's effects.
pub(crate) const fn answerability(check: &Precondition) -> Answerability {
    match check {
        // Both read `pg_depend` NORMAL edges pointing AT the column (plus, for the
        // retype, partition-key membership and inheritance). Every one of those is
        // an existing catalog fact that only a removal can clear.
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
/// A WHITELIST, and deliberately so. The question "does this remove a
/// dependency?" invites classifying a shape by what it is called - and
/// `CREATE OR REPLACE VIEW`, which this engine really does emit
/// (`render/renderer.rs`, PostgreSQL's `CreateOrReplaceView` capability), reads
/// as a creation while it silently recomputes the view's dependency edges and so
/// removes a column's blocker with no `DROP` anywhere. Asking instead "is this
/// one of the shapes proven to add only?" gets that case right by construction,
/// and gets every shape nobody has thought about yet right too.
pub(crate) fn clears_no_obstruction(step: &PlanStep) -> bool {
    match step {
        PlanStep::Ddl(m) => sql_clears_no_obstruction(&m.up),
        // A parameterized INSERT/UPDATE/DELETE over a structurally known table.
        // Rows are not catalog facts: no row change can remove a `pg_depend` edge,
        // an inheritance link, or a partition-key membership. The template is
        // built by `render::dml`, which emits those three statements and nothing
        // else, so a `Dml` step cannot carry DDL.
        PlanStep::Dml { .. } | PlanStep::Backfill { .. } => true,
        // These render their DDL at APPLY time against the live catalog they are
        // validated by, so the step carries the intent and not the statements. A
        // primary-key replacement in particular drops a constraint and its index.
        PlanStep::AlterPrimaryKey(_)
        | PlanStep::SynchronizeIdentity(_)
        | PlanStep::OnlineRename(_) => false,
    }
}

/// Read a rendered statement bundle. A `Migration.up` is `statements.join(";\n")`
/// and really can carry several, so every statement is judged.
fn sql_clears_no_obstruction(sql: &str) -> bool {
    let Ok(parsed) = pg_query::parse(sql) else {
        return false;
    };
    parsed.protobuf.stmts.iter().all(|raw| {
        raw.stmt
            .as_ref()
            .and_then(|node| node.node.as_ref())
            .is_some_and(statement_clears_no_obstruction)
    })
}

/// One statement's verdict.
fn statement_clears_no_obstruction(node: &NodeEnum) -> bool {
    match node {
        NodeEnum::AlterTableStmt(alter) => alter.cmds.iter().all(|cmd| {
            matches!(cmd.node.as_ref(), Some(NodeEnum::AlterTableCmd(cmd))
                if subtype_clears_no_obstruction(cmd.subtype))
        }),
        // A REPLACE recomputes the object's dependency edges, so a body that stops
        // reading a column removes that column's blocker without any DROP. Only
        // the non-replacing forms are additive.
        NodeEnum::ViewStmt(view) => !view.replace,
        NodeEnum::CreateFunctionStmt(function) => !function.replace,
        // Purely additive, and none of them can touch an existing column's
        // dependency edges, inheritance, or partition-key membership.
        //
        // `CreateStmt` covers `CREATE TABLE ... PARTITION OF`, which RAISES the new
        // child's `attinhcount` rather than lowering anybody's.
        NodeEnum::CreateStmt(_)
        | NodeEnum::IndexStmt(_)
        | NodeEnum::CreateSeqStmt(_)
        | NodeEnum::CreateEnumStmt(_)
        | NodeEnum::CreateDomainStmt(_)
        | NodeEnum::CreateSchemaStmt(_)
        | NodeEnum::CreateExtensionStmt(_)
        | NodeEnum::CreateTrigStmt(_)
        | NodeEnum::CreatePolicyStmt(_)
        | NodeEnum::CommentStmt(_)
        // GRANT and REVOKE both parse here. A privilege is not a `pg_depend` edge
        // on a column.
        | NodeEnum::GrantStmt(_)
        // The empty-plan journal anchor renders `SELECT 1`.
        | NodeEnum::SelectStmt(_) => true,
        // Everything else. Notably every `DROP`, every `RENAME` (which removes a
        // name as well as creating one), `DROP OWNED BY`, and every
        // `DO $$ ... $$` block, whose DDL sits inside a string no parse can see
        // through.
        _ => false,
    }
}

/// One `ALTER TABLE` subcommand's verdict.
///
/// Excluded on purpose, each because it really can clear an obstruction:
/// `DROP COLUMN` and `DROP CONSTRAINT` remove the dependent outright;
/// `ALTER COLUMN ... TYPE` rebuilds whatever depended on the old type;
/// `SET`/`DROP DEFAULT` replaces the `pg_attrdef` entry and its edges;
/// `DROP EXPRESSION` and `DROP IDENTITY` remove the generated/identity
/// machinery; `NO INHERIT` clears `attinhcount`; `DETACH PARTITION` clears both
/// inheritance and partition-key membership on the detached child. Every
/// subcommand not named below, including any this engine does not emit today,
/// is excluded as well.
fn subtype_clears_no_obstruction(subtype: i32) -> bool {
    matches!(
        AlterTableType::try_from(subtype),
        Ok(AlterTableType::AtAddColumn
            | AlterTableType::AtAddColumnToView
            | AlterTableType::AtAddConstraint
            | AlterTableType::AtAddIndex
            | AlterTableType::AtValidateConstraint
            | AlterTableType::AtSetNotNull
            | AlterTableType::AtDropNotNull
            | AlterTableType::AtAttachPartition
            | AlterTableType::AtEnableRowSecurity
            | AlterTableType::AtDisableRowSecurity
            | AlterTableType::AtForceRowSecurity
            | AlterTableType::AtNoForceRowSecurity)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::migration::{
        Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId,
    };
    use crate::model::precondition::PreconditionCheck;

    fn ddl(version: &str, up: &str, checks: Vec<PreconditionCheck>) -> PlanStep {
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
        };
        m.checksum = Checksum::of(&ChecksumInput::from_migration(&m));
        PlanStep::Ddl(m)
    }

    fn drop_blocked(table: &str, column: &str) -> Precondition {
        Precondition::ColumnHasNoBlockingDependents {
            table: table.to_string(),
            column: column.to_string(),
        }
    }

    /// The shapes the engine's PostgreSQL renderer emits that ADD only. A plan
    /// prefix of these leaves an obstruction assertion's pre-plan answer valid.
    #[test]
    fn the_additive_shapes_the_renderer_emits_clear_nothing() {
        for sql in [
            r#"ALTER TABLE "s"."t" ADD COLUMN "survivor" text"#,
            r#"ALTER TABLE "s"."t" ADD COLUMN "a" text, ADD COLUMN "b" text"#,
            r#"ALTER TABLE "s"."t" ADD CONSTRAINT "u" UNIQUE ("a")"#,
            r#"ALTER TABLE "s"."t" ADD CONSTRAINT "f" FOREIGN KEY ("a") REFERENCES "s"."o" ("id")"#,
            r#"ALTER TABLE "s"."t" VALIDATE CONSTRAINT "f""#,
            r#"ALTER TABLE "s"."t" ALTER COLUMN "a" SET NOT NULL"#,
            r#"ALTER TABLE "s"."t" ENABLE ROW LEVEL SECURITY"#,
            r#"CREATE TABLE "s"."t" ("id" int NOT NULL, "v" text)"#,
            r#"CREATE UNIQUE INDEX IF NOT EXISTS "ix" ON "s"."t" ("a")"#,
            r#"CREATE VIEW "s"."v" AS SELECT 1 AS a"#,
            r#"CREATE TYPE "s"."e" AS ENUM ('a')"#,
            r#"CREATE SEQUENCE "s"."q""#,
            r#"CREATE POLICY "p" ON "s"."t" FOR SELECT USING (true)"#,
            r#"COMMENT ON COLUMN "s"."t"."a" IS 'x'"#,
            r#"GRANT SELECT ON "s"."t" TO "r""#,
            r#"REVOKE SELECT ON "s"."t" FROM "r""#,
            "SELECT 1",
            // A masked addColumn is TWO statements in one `up`.
            "ALTER TABLE \"s\".\"t\" ADD COLUMN \"secret\" bytea;\nCOMMENT ON COLUMN \"s\".\"t\".\"secret\" IS 'zero-migrate:enc'",
        ] {
            assert!(
                sql_clears_no_obstruction(sql),
                "{sql} adds only and must leave a pre-plan obstruction answer valid"
            );
        }
    }

    /// Every shape that can remove a dependency edge, an inheritance link or a
    /// partition-key membership. `CREATE OR REPLACE VIEW` is the one that reads
    /// like a creation and is not.
    #[test]
    fn the_removing_shapes_do_not_clear_nothing() {
        for sql in [
            r#"CREATE OR REPLACE VIEW "s"."v" AS SELECT 1 AS a"#,
            r#"CREATE OR REPLACE FUNCTION "s"."f"() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$"#,
            r#"DROP VIEW "s"."reader""#,
            r#"DROP INDEX "s"."ix""#,
            r#"DROP TABLE "s"."t""#,
            r#"DROP TRIGGER "tg" ON "s"."t""#,
            r#"DROP POLICY "p" ON "s"."t""#,
            r#"DROP TYPE "s"."e""#,
            r#"ALTER TABLE "s"."t" DROP COLUMN "other""#,
            r#"ALTER TABLE "s"."t" DROP CONSTRAINT "ck""#,
            r#"ALTER TABLE "s"."t" ALTER COLUMN "a" TYPE bigint"#,
            r#"ALTER TABLE "s"."t" ALTER COLUMN "a" DROP DEFAULT"#,
            r#"ALTER TABLE "s"."t" ALTER COLUMN "a" SET DEFAULT 1"#,
            r#"ALTER TABLE "s"."t" DETACH PARTITION "s"."p""#,
            r#"ALTER TABLE "s"."t" NO INHERIT "s"."parent""#,
            r#"ALTER TABLE "s"."t" RENAME COLUMN "a" TO "b""#,
            r#"ALTER TABLE "s"."t" RENAME TO "u""#,
            "DO $$ BEGIN EXECUTE 'DROP VIEW v'; END $$",
            // An additive statement beside a removing one is still a removal.
            "ALTER TABLE \"s\".\"t\" ADD COLUMN \"a\" text;\nDROP VIEW \"s\".\"v\"",
            // Unparseable text answers the same way as a removal: fail toward the
            // per-migration seam.
            "this is not sql at all",
        ] {
            assert!(
                !sql_clears_no_obstruction(sql),
                "{sql} can clear an obstruction and must send the check back to the \
                 per-migration seam"
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
                "{check:?} is repaired by the ordinary additive work a plan does, so \
                 no prefix test can license hoisting it"
            );
        }
    }

    /// The RED shape: an ordinary op that commits, then a blocked drop.
    #[test]
    fn a_drop_behind_an_add_is_plan_stable() {
        let steps = vec![
            ddl(
                "v0",
                r#"ALTER TABLE "s"."t" ADD COLUMN "survivor" text"#,
                vec![],
            ),
            ddl(
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
    #[test]
    fn a_drop_behind_a_removal_is_left_to_the_per_migration_seam() {
        for earlier in [
            r#"DROP VIEW "s"."reader""#,
            r#"CREATE OR REPLACE VIEW "s"."reader" AS SELECT 1 AS a"#,
        ] {
            let steps = vec![
                ddl("v0", earlier, vec![]),
                ddl(
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
            ddl("v0", r#"DROP VIEW "s"."reader""#, vec![]),
            ddl("v1", r#"ALTER TABLE "s"."t" ADD COLUMN "a" text"#, vec![]),
            ddl(
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
            ddl("v0", r#"ALTER TABLE "s"."t" ADD COLUMN "a" text"#, vec![]),
            ddl(
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
        let steps = vec![ddl(
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
            ddl(
                "v0",
                r#"ALTER TABLE "s"."t" ADD COLUMN "survivor" text"#,
                vec![],
            ),
            ddl(
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
