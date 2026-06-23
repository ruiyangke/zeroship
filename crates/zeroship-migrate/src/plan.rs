//! The applied-execution plan model (`op.*` DSL design §2.0) and the single
//! shared plan orchestrator's data types.
//!
//! One authored migration artifact — a `.sql` file *or* (after PR1) an
//! `.ir.json` — lowers to an [`AppliedPlan`]: an ordered sequence of
//! [`PlanStep`]s the engine's single shared `apply_plan`
//! ([`MigrationEngine::apply_plan`](crate::engine::MigrationEngine::apply_plan))
//! runs in order. The step *types* reuse the engine's existing phase artifacts
//! ([`Migration`], [`BackfillSpec`](crate::backfill::BackfillSpec),
//! [`ExpandContractPlan`](crate::expand_contract::ExpandContractPlan), and the
//! existing [`declarative::SqliteRebuild`](crate::declarative::SqliteRebuild)) —
//! PR0 introduces **no** new rebuild struct and **no** change to [`Migration`].
//!
//! # Naming — a deliberate collision avoidance (§2.0)
//!
//! This is **`AppliedPlan`**, NOT `MigrationPlan`. `MigrationPlan`
//! ([`engine::MigrationPlan`](crate::engine::MigrationPlan)) is the read-only
//! lint/dry-run **preview** result and keeps its name and meaning, untouched.
//! `AppliedPlan` is the net-new *ordered execution artifact*. The two coexist on
//! the public surface as distinct symbols.
//!
//! # The single-`Migration` case is the degenerate one-step plan (§2.0)
//!
//! A pure-DDL `.sql` (or `.ir.json` with no DML/backfill/online op) lowers to a
//! plan whose `steps == [Ddl(one Migration)]` — the overwhelming common case,
//! and the only shape the platform Flyway-mode loader ever produces. The
//! [`AppliedPlan::single_step`] facade builds exactly that, and
//! [`AppliedPlan::single_step_migration`] reads it back out (fail-closed on a
//! multi-step plan, §5.2).

use crate::backfill::BackfillSpec;
use crate::declarative::SqliteRebuild;
use crate::expand_contract::ExpandContractPlan;
use crate::migration::{Checksum, Migration, MigrationFlags, MigrationId};
use crate::precondition::PreconditionCheck;

/// The dialect reach of an applied plan, derived from its ops (§2.0). A separate,
/// journaled facet — **not** folded into the identity checksum. PR0 only ever
/// produces [`DialectScope::Both`] (every `.sql`/declarative artifact is
/// dialect-neutral or per-dialect-rendered from the same logical shape); the
/// `PgOnly` arm is reserved for the IR `op.raw({pg})` path (PR1+).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialectScope {
    /// Applies faithfully to both Postgres and SQLite.
    Both,
    /// Postgres-only (a `PgOnly` `op.raw` artifact); refused against a SQLite
    /// deploy target at load (PR1+). Never produced by the PR0 `.sql` path.
    PgOnly,
}

/// A rename lowered to ONE of two **dialect-distinct executable shapes**, chosen
/// by the deploy-target dialect at lowering (§2.6.2). The dual-execution dispatch
/// in `apply_plan` is structural — a match on this variant.
#[derive(Debug, Clone)]
pub enum RenameStep {
    /// **Postgres**: an online expand-contract. Executed via
    /// [`OnlineSchemaChange::run_online`](crate::expand_contract::OnlineSchemaChange::run_online)
    /// (the only impl, [`PgOnline`](crate::expand_contract::PgOnline)). The
    /// [`ExpandContractPlan`] bundles E1..C2 (`Vec<Migration>`) + the
    /// [`BackfillSpec`]; the contract (C1/C2) is partitioned across deploys as
    /// `pending_contract` (§2.0.2).
    PgExpandContract(ExpandContractPlan),
    /// **SQLite**: an OFFLINE 12-step table rebuild. Executed via
    /// [`MigrationBackend::rebuild_one`](crate::backend::MigrationBackend::rebuild_one),
    /// NOT via `run_online`. REUSES the existing
    /// [`declarative::SqliteRebuild`](crate::declarative::SqliteRebuild)
    /// (`{ migration, spec }`) verbatim — no parallel struct — so the
    /// shape-adapter maps a `declarative::SqliteRebuild` straight in with no
    /// conversion. A SQLite rebuild is atomic and single-deploy: it has **no**
    /// `pending_contract` partition (§2.0.2).
    SqliteRebuild(SqliteRebuild),
}

/// A typed scalar bound into a parameterized [`PlanStep::Dml`] statement.
///
/// PR0 introduces the variant + a minimal value model so a trusted constructor
/// (and PR0's test (9)) can hand-build a `Dml` step; the creator-DML *assembler*
/// that produces these from `op.insert`/`op.update` is net-new in PR6a. Bound
/// values are passed to the driver as native parameters (`$n` on PG, `?n` on
/// SQLite) — never string-interpolated — so a bind value can never alter
/// statement structure (§2.3.2).
#[derive(Debug, Clone, PartialEq)]
pub enum BindValue {
    /// SQL `NULL`.
    Null,
    /// A boolean.
    Bool(bool),
    /// An exact 64-bit integer (the only integer domain the IR admits, §2.3.2).
    Int(i64),
    /// A decimal/float carried as its canonical string form (§2.3.2 numeric
    /// domain: no `f64` in the IR identity).
    Decimal(String),
    /// A UTF-8 text value.
    Text(String),
}

/// One ordered step of an [`AppliedPlan`] (§2.0). The step types reuse the
/// engine's existing phase artifacts; only `Dml` carries a net-new payload.
#[derive(Debug, Clone)]
pub enum PlanStep {
    /// A transactional or non-txn DDL statement bundle — an existing
    /// [`Migration`] (single `up: String`, no parameter slot).
    Ddl(Migration),
    /// A parameterized DML statement (insert/update/delete) — the net-new
    /// variant (§2.3.2). `template` is the placeholder SQL the journal hashes;
    /// `binds` are the typed values a `Migration{up:String}` has no slot for.
    Dml {
        /// The journal version this DML step records under (its sub-version,
        /// §2.0.1). A `Migration`-less step still needs an identity to journal.
        version: MigrationId,
        /// Human-readable label for status/diagnostics.
        name: String,
        /// The placeholder SQL (the journal hashes this; the binds fold into the
        /// plan checksum, §2.3.2).
        template: String,
        /// The ordered typed values bound natively to the template.
        binds: Vec<BindValue>,
        /// `true` ⇒ the step's DDL/journal runs inside a transaction (the
        /// default for DML).
        transactional: bool,
        /// `true` ⇒ data loss (a `delete`); the gate decides (§2.1.1).
        destructive: bool,
        /// The declaring app's `owner_app` — the journal-identity attribution
        /// (§2.0.1). Folded into the journal `ChecksumInput` so two DML steps
        /// with an identical `(template, binds)` authored by DIFFERENT apps hash
        /// to DIFFERENT journal checksums (and the journal row attributes the
        /// right owner). PR0 hand-builds Dml steps in tests; PR6a's creator-DML
        /// assembler MUST supply the real declaring `owner_app` here.
        owner_app: String,
    },
    /// A crash-safe batched data backfill — an existing
    /// [`BackfillSpec`](crate::backfill::BackfillSpec), run by `run_backfill`
    /// (PG) / the SQLite backfill executor (PR6b).
    Backfill(BackfillSpec),
    /// A rename, lowered to ONE of two dialect-distinct executable shapes
    /// (§2.6.2), dual-dispatched by `apply_plan` on the [`RenameStep`] variant.
    OnlineRename(RenameStep),
}

impl PlanStep {
    /// Whether this step carries data loss for the destructive/approval gate
    /// (§2.1.1). A `Ddl`/`OnlineRename(SqliteRebuild)` reads its migration's
    /// `flags.destructive`; a `Dml` reads its own flag; a `Backfill` is not
    /// destructive (it writes, never drops); a PG `OnlineRename` is gated by its
    /// contract migrations at the pending-contract apply, not here.
    #[must_use]
    pub fn is_destructive(&self) -> bool {
        match self {
            PlanStep::Ddl(m) => m.flags.destructive,
            PlanStep::Dml { destructive, .. } => *destructive,
            PlanStep::Backfill(_) => false,
            PlanStep::OnlineRename(RenameStep::SqliteRebuild(rb)) => rb.migration.flags.destructive,
            // A PG expand's E1..E3 are additive; the destructive C2 is gated at
            // the pending-contract apply (§2.0.2).
            PlanStep::OnlineRename(RenameStep::PgExpandContract(_)) => false,
        }
    }

    /// Whether this step has a defined `down` for plan-level rollback (§2.1.2).
    /// A `Backfill`, a `Dml`, and an in-flight `OnlineRename` are `down: None`;
    /// a `Ddl` is rollbackable iff its migration carries a `down`.
    #[must_use]
    pub fn has_down(&self) -> bool {
        match self {
            PlanStep::Ddl(m) => m.down.is_some(),
            // No general inverse of a DML statement / a batched backfill; an
            // online rename's down is roll-forward while in flight (§2.1.1).
            PlanStep::Dml { .. } | PlanStep::Backfill(_) | PlanStep::OnlineRename(_) => false,
        }
    }
}

/// What one authored artifact (`.sql` today; `.ir.json` after PR1) becomes after
/// lowering — an ordered execution plan (§2.0). NOT a single [`Migration`]; NOT
/// the dry-run [`MigrationPlan`](crate::engine::MigrationPlan).
#[derive(Debug, Clone)]
pub struct AppliedPlan {
    /// The outer plan version (the filename's `<NNNN>` → deterministic UUIDv7).
    pub version: MigrationId,
    /// Human-readable name.
    pub name: String,
    /// Ordered steps; `apply_plan` runs them in sequence.
    pub steps: Vec<PlanStep>,
    /// ONE checksum over the canonical artifact (for a `.sql` plan this is the
    /// single step's `Migration.checksum`; for an `.ir.json` it is
    /// `Checksum::of_ir` over the op list, PR1).
    pub checksum: Checksum,
    /// Flags derived ∪ overridden from the artifact (§2.1.1).
    pub flags: MigrationFlags,
    /// The plan's dialect reach (§2.0); a separate journaled facet, not folded
    /// into the checksum.
    pub dialect_scope: DialectScope,
    /// `false` if ANY step is `down: None` (Backfill/Dml/in-flight OnlineRename);
    /// surfaced by status/rollback BEFORE attempt (§2.1.2).
    pub rollbackable: bool,
    /// The declaring app (server-stamped on the IR path, §8.6).
    pub owner_app: String,
    /// Cross-plan ordering deps (attach to the first step, §2.0.1).
    pub depends_on: Vec<MigrationId>,
    /// Squash supersession identity.
    pub supersedes: Vec<MigrationId>,
    /// Preconditions evaluated before the plan's first step.
    pub preconditions: Vec<PreconditionCheck>,
}

/// The fail-closed error of [`AppliedPlan::single_step_migration`] (§5.2): the
/// plan is not a single `Ddl` step, so a `Migration`-only consumer (the platform
/// Flyway-mode runner) cannot operate on it. This arm is provably unreachable on
/// the platform path (a Flyway `.sql` always lowers to one `Ddl` step) — it
/// exists for defense in depth.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "migration plan {version} is not a single-step DDL plan (has {step_count} step(s)); \
     a Migration-only consumer cannot operate on a multi-step plan"
)]
pub struct NotSingleStep {
    /// The plan version that failed the single-step precondition.
    pub version: String,
    /// How many steps the plan actually has.
    pub step_count: usize,
}

impl AppliedPlan {
    /// Build the **degenerate one-step plan** for a single pure-DDL [`Migration`]
    /// — the loader facade for a `.sql` file (§2.0, §5.2). The plan's identity
    /// fields mirror the migration; `dialect_scope` is `Both` (a `.sql` plan is
    /// not `op.raw`-pinned), and `rollbackable` follows the migration's `down`.
    #[must_use]
    pub fn single_step(migration: Migration) -> Self {
        let version = migration.version.clone();
        let name = migration.name.clone();
        let checksum = migration.checksum.clone();
        let flags = migration.flags; // MigrationFlags is Copy
        let owner_app = migration.owner_app.clone();
        let depends_on = migration.depends_on.clone();
        let supersedes = migration.supersedes.clone();
        let preconditions = migration.preconditions.clone();
        let rollbackable = migration.down.is_some();
        AppliedPlan {
            version,
            name,
            steps: vec![PlanStep::Ddl(migration)],
            checksum,
            flags,
            dialect_scope: DialectScope::Both,
            rollbackable,
            owner_app,
            depends_on,
            supersedes,
            preconditions,
        }
    }

    /// The thin `Migration`-facade the platform Flyway-mode runner consumes
    /// (§5.2): a plan whose `steps == [Ddl(_)]` yields that one `&Migration`;
    /// any other shape fails closed with [`NotSingleStep`]. This keeps the
    /// platform runner operating over [`Migration`] and decoupled from
    /// `PlanStep`/`RenameStep` evolution.
    ///
    /// # Errors
    /// [`NotSingleStep`] if the plan is not exactly one `Ddl` step.
    pub fn single_step_migration(&self) -> Result<&Migration, NotSingleStep> {
        match self.steps.as_slice() {
            [PlanStep::Ddl(m)] => Ok(m),
            other => Err(NotSingleStep {
                version: self.version.as_str().to_string(),
                step_count: other.len(),
            }),
        }
    }

    /// True iff the plan is a single `Ddl` step (the platform-path precondition,
    /// §5.2). Convenience over [`single_step_migration`](Self::single_step_migration).
    #[must_use]
    pub fn is_single_step(&self) -> bool {
        matches!(self.steps.as_slice(), [PlanStep::Ddl(_)])
    }

    /// Recompute `rollbackable` from the current steps (§2.1.2): `true` iff every
    /// step has a defined `down`. Used by adapters that assemble `steps`
    /// directly.
    #[must_use]
    pub fn compute_rollbackable(steps: &[PlanStep]) -> bool {
        steps.iter().all(PlanStep::has_down)
    }
}
