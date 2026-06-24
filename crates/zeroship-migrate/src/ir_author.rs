//! `IrAuthor` — the DDL **Lower** phase (§6, §6.4, §6.5).
//!
//! `IrAuthor::lower` compiles a validated, ownership-checked [`MigrationIr`]
//! (DDL ops) into the same [`Migration`] shape the declarative differ produces.
//! It is the IR-path peer of `DeclarativeAuthor::diff`.
//!
//! # Single source of truth (§6.5 MANDATE)
//!
//! `IrAuthor` does **NOT** hand-construct snapshots, and does NOT re-spell the
//! default / system-field / encryption-/comment-sentinel logic. It routes every
//! op's fields through the SHARED, dialect-parameterized snapshot-builder
//! [`crate::declarative::build_table_snapshot`] — the SAME builder the differ's
//! `desired_snapshot_for_dialect` calls — and then renders the resulting
//! [`TableSnapshot`] / [`ColumnSnapshot`] / [`IndexSnapshot`] through the SAME
//! render methods the differ uses (`DeclarativeAuthor::lower_*`, which delegate to
//! `render_create_table` / the `DdlEmitter`). So the emitted SQL is byte-identical
//! to the declarative path BY CONSTRUCTION. The §6.4 cross-path byte-identity
//! golden (in `tests/ir_author_render_parity.rs`) guards against accidental
//! regression — not against two independent implementations.
//!
//! # The only IR-path-specific code: the type/shape MAPPING
//!
//! The one thing `IrAuthor` owns is mapping the IR op vocabulary
//! ([`Op`]/[`IrColumn`]/[`ColType`]/[`IrDefault`]) onto the descriptor shape the
//! shared builder consumes ([`FieldDescriptor`]). This is a pure structural
//! translation — it carries NO default/sentinel rendering (that stays in the
//! shared builder), so the §6.5 single-source guarantee holds.

use std::collections::BTreeSet;

use crate::declarative::{
    build_table_snapshot, CollectionDescriptor, DeclarativeAuthor, DeclarativeError,
    FieldDescriptor, LoweredUnit,
};
use crate::drift::{ColumnSnapshot, IndexSnapshot};
use crate::guard::{guard_for, GuardConfig, GuardError};
use crate::ir::{
    ColType, IrColumn, IrConstraint, IrConstraintKind, IrDefault, IndexMethod, MigrationIr, Op,
};
use crate::migration::Migration;
use crate::plan::{AppliedPlan, PlanStep};
use zeroship_schema::query::SqlDialect;

/// The LIVE-schema facts the IR-path Lower phase consults — the IR-path peer of
/// the full [`crate::drift::SchemaSnapshot`] the differ diffs against.
///
/// The differ reads BOTH "which tables already exist" (drives FK inline-vs-defer)
/// and "is THIS index UNIQUE in the live catalog" (drives the `render_drop_index`
/// destructive/approval gate) from the authoritative introspected snapshot. The
/// IR path must consult the SAME authoritative source — never trust an
/// author-supplied hint for a security-relevant gate — so this bundle carries both
/// live facts the lower needs:
///
/// - `tables` — the set of tables already present (FK to a live target inlines; to
///   a not-yet-live target defers on PG / errors on SQLite — mirroring `diff`).
/// - `unique_indexes` — the set of index NAMES the live catalog reports as UNIQUE.
///   A `dropIndex` of a name in this set lowers `destructive + requires_approval`
///   REGARDLESS of the IR's `unique` hint: the hint is advisory and is OR-ed with
///   this live fact, so a hostile/buggy author who sets `unique:false` (or omits
///   it) on a drop of an actually-unique index can NOT defeat the approval gate
///   (the gate the spec intends — silently dropping a unique index removes a
///   data-integrity guarantee). When introspection is unavailable (a unit lower
///   with no live schema), the set is empty and gating falls back to the hint
///   alone — never LESS strict than the hint.
#[derive(Debug, Clone, Default)]
pub struct LiveSchema {
    /// Tables already present in the project schema (FK inline-vs-defer).
    pub tables: BTreeSet<String>,
    /// Index NAMES the live catalog reports as UNIQUE (drop-gating, OR-ed with the
    /// IR's advisory `unique` hint — the live fact is authoritative).
    pub unique_indexes: BTreeSet<String>,
}

impl LiveSchema {
    /// A live schema with `tables` and NO known unique indexes — for a unit lower
    /// that has the live table set (FK inlining) but no introspected index facts.
    /// Drop-gating then falls back to the IR's advisory `unique` hint alone (never
    /// LESS strict than the hint).
    #[must_use]
    pub fn from_tables(tables: BTreeSet<String>) -> Self {
        Self { tables, unique_indexes: BTreeSet::new() }
    }
}

impl From<&BTreeSet<String>> for LiveSchema {
    /// Bridge the bare live-table set used throughout the unit lower tests into the
    /// bundled facts (no known unique indexes — the hint-only fallback).
    fn from(tables: &BTreeSet<String>) -> Self {
        Self::from_tables(tables.clone())
    }
}

/// The IR-path DDL author (§6). Wraps a [`DeclarativeAuthor`] so it reuses the
/// declarative render seam verbatim; the IR-specific work is the op→descriptor
/// mapping that feeds the shared snapshot-builder.
#[derive(Debug)]
pub struct IrAuthor {
    project_schema: String,
    decl: DeclarativeAuthor,
    dialect: SqlDialect,
}

/// A failure lowering an IR op to SQL.
#[derive(Debug, thiserror::Error)]
pub enum IrLowerError {
    /// The op's fields could not be modelled as a snapshot (e.g. an unknown type
    /// token, an unsafe ref target). Carries the shared builder's error.
    #[error(transparent)]
    Snapshot(#[from] DeclarativeError),
    /// An op `IrAuthor::lower` does not yet compile (DML / online-intent ops are a
    /// later wave; this Lower phase covers the DDL ops). Carries the op tag.
    #[error("IrAuthor::lower does not yet compile op {0:?} (DDL ops only in this wave)")]
    UnsupportedOp(&'static str),
    /// A DDL op whose body carries a closed-AST [`crate::expr::Expr`] (a `CHECK`
    /// constraint predicate, or an `alterColumnType` `using` cast) that the
    /// Wave-C expression→SQL renderer must materialize. The op family lowers; the
    /// Expr-bearing variant waits on that renderer (the same wave the DML
    /// executors wait on — there is no `Expr`→SQL path yet). Carries the slot tag.
    #[error(
        "IrAuthor::lower cannot yet render the closed-AST expression in {0} \
         (the Expr→SQL renderer is the later expression wave)"
    )]
    ExprRenderDeferred(&'static str),
    /// A stand-alone `alterColumn*` / `addConstraint` / `dropConstraint` op on the
    /// SQLite dialect. SQLite has no native `ALTER COLUMN` / `ALTER TABLE
    /// ADD|DROP CONSTRAINT`; the differ reconciles these via the 12-step table
    /// REBUILD, which needs the full LIVE table structure (unavailable in this
    /// pure-render lower). So stand-alone IR lowering of these ops is PG-only; the
    /// SQLite leg routes through the declarative diff rebuild seam. Carries the op
    /// tag.
    #[error(
        "IrAuthor::lower of stand-alone op {0:?} is Postgres-only — SQLite has no \
         native ALTER COLUMN / ADD|DROP CONSTRAINT; route it through the \
         declarative diff rebuild seam (the 12-step table rebuild)"
    )]
    SqliteRebuildOnly(&'static str),
}

/// One rendered SQL FRAGMENT of a lowered op, carrying its attribution (§6.1.1):
/// the originating op INDEX (its position in `MigrationIr::ops`) and the op's kind.
/// A single op can render multiple fragments (`createTable` emits the table + an
/// inline `COMMENT ON COLUMN` side output + per-table indexes), and each is
/// guarded INDIVIDUALLY so a denial is attributable to the exact op — not buried
/// in a concatenated `up` blob.
#[derive(Debug, Clone)]
pub struct GuardedFragment {
    /// The originating op's 0-based index in `MigrationIr::ops`.
    pub op_index: usize,
    /// The op's kind tag (e.g. `"createTable"`) — the human-facing attribution.
    /// (The `.ts` source-map location threads through the §5.1 provenance blob in
    /// a later wave; the op-index + kind is the attribution available at lower.)
    pub op_kind: &'static str,
    /// The single rendered SQL statement (NO trailing `;`), guarded as-is.
    pub sql: String,
}

/// A guard DENIAL attributed to the exact op that produced the denied fragment
/// (§6.1.1). The human message leads with the op-index + kind so an author/AI
/// sees *which* op the guard refused, not a bare "statement denied".
#[derive(Debug, thiserror::Error)]
#[error(
    "op #{op_index} ({op_kind}): rendered statement denied by guard: {source}"
)]
pub struct FragmentGuardDenied {
    /// The op whose rendered fragment the guard denied.
    pub op_index: usize,
    /// The op's kind tag.
    pub op_kind: &'static str,
    /// The underlying guard error.
    #[source]
    pub source: GuardError,
}

/// A failure of the guard-per-fragment lower ([`IrAuthor::lower_guarded`]):
/// lowering failed, OR a rendered fragment was denied by the guard (attributed to
/// its op, §6.1.1), OR the fragment-reassembly byte-identity invariant broke.
#[derive(Debug, thiserror::Error)]
pub enum IrGuardedLowerError {
    /// Lowering a validated op to SQL failed.
    #[error(transparent)]
    Lower(#[from] IrLowerError),
    /// A rendered fragment was denied by the guard, attributed to its op.
    #[error(transparent)]
    Denied(#[from] FragmentGuardDenied),
    /// The reassembly invariant `applied_up == join(guarded_fragments, ";\n")`
    /// broke for a lowered migration — an engine bug (fragment splitting that does
    /// not round-trip). Fail closed rather than apply a `up` that diverges from
    /// what was guarded.
    #[error(
        "fragment-reassembly invariant broke for migration {name:?}: the join of the \
         individually-guarded fragments is not byte-identical to the lowered `up` \
         (guard/render seam bug)"
    )]
    ReassemblyMismatch {
        /// The migration whose reassembly diverged.
        name: String,
    },
}

/// A failure in the loader's IR branch ([`IrAuthor::load_and_lower`]): either the
/// fail-closed LOAD GATE refused the artifact, or LOWERING a validated op failed.
#[derive(Debug, thiserror::Error)]
pub enum LoadAndLowerError {
    /// The `.ir.json` LOAD GATE refused the artifact (deserialize / ir_version /
    /// structural validate / ownership / checksum-hint).
    #[error(transparent)]
    Load(#[from] crate::ir_load::IrLoadError),
    /// Lowering a validated, owned op to SQL failed.
    #[error(transparent)]
    Lower(#[from] IrLowerError),
}

/// A failure in the GUARD-per-fragment loader branch
/// ([`IrAuthor::load_and_lower_guarded`]): the fail-closed LOAD GATE refused the
/// artifact, OR the guard-per-fragment lower failed/denied a rendered fragment
/// (carrying the op-index attribution, §6.1.1). This is the error the PRODUCTION
/// `.ir.json` deploy path surfaces, so a guard denial reaches the creator with the
/// exact offending op index + kind — not buried in a whole-`up` denial.
#[derive(Debug, thiserror::Error)]
pub enum LoadAndLowerGuardedError {
    /// The `.ir.json` LOAD GATE refused the artifact.
    #[error(transparent)]
    Load(#[from] crate::ir_load::IrLoadError),
    /// The guard-per-fragment lower failed, denied a fragment (op-index
    /// attribution), or broke the reassembly invariant.
    #[error(transparent)]
    Lower(#[from] IrGuardedLowerError),
}

/// The result of [`IrAuthor::load_and_lower_guarded`]: the lowered, guard-checked
/// migrations + the per-op guarded fragments (DX attribution) + the set of tables
/// this artifact CREATES (its `createTable` ops). The deploy loop folds
/// `created_tables` into the ownership registry + FK-inline live-set BEFORE the
/// next `.ir.json` file, so a same-deploy migration that touches an earlier file's
/// table resolves ownership / inlines FKs correctly (cross-file correctness).
#[derive(Debug)]
pub struct LoweredArtifact {
    /// The lowered artifact as a single ordered [`AppliedPlan`] (§2.0 / §5.2):
    /// one `.ir.json` → ONE plan, whose `Ddl` steps are the lowered, guard-checked
    /// migrations (their `up` is provably the reassembly of the guarded fragments,
    /// §6.1.1) and whose `checksum` is the dialect-neutral [`Checksum::of_ir`] over
    /// the op list (§2.4). The deploy path routes this plan's steps through
    /// `MigrationEngine::apply_plan` (§5.2). For PR1's pure-DDL ops every step is a
    /// `PlanStep::Ddl`; richer step kinds (Backfill/Dml/OnlineRename) arrive in
    /// PR2/PR6a.
    pub plan: AppliedPlan,
    /// The per-op guarded fragments (op-index + kind attribution).
    pub fragments: Vec<GuardedFragment>,
    /// The tables this artifact creates (its `createTable` op names), for the
    /// deploy loop to fold into the cross-file registry + live-set.
    pub created_tables: Vec<String>,
}

impl LoweredArtifact {
    /// The lowered `Ddl` migrations, in plan-step order — the flat view the
    /// deploy-side traceability manifest + diagnostics consume. (PR1 steps are all
    /// `Ddl`; a non-`Ddl` step would simply not appear here.)
    #[must_use]
    pub fn migrations(&self) -> Vec<Migration> {
        self.plan
            .steps
            .iter()
            .filter_map(|s| match s {
                PlanStep::Ddl(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }
}

impl IrAuthor {
    /// Construct an IR author bound to a project schema + deploying app, for a
    /// target dialect. The deploying app is the `owner_app` stamped on every
    /// emitted migration (ownership is enforced UPSTREAM by the IR-load gate).
    #[must_use]
    pub fn new(
        project_schema: impl Into<String>,
        owner_app: impl Into<String>,
        dialect: SqlDialect,
    ) -> Self {
        let project_schema = project_schema.into();
        Self {
            decl: DeclarativeAuthor::new_for_dialect(
                project_schema.clone(),
                owner_app,
                dialect,
            ),
            project_schema,
            dialect,
        }
    }

    /// The loader's IR branch (§7.2): run the fail-closed `.ir.json` LOAD GATE
    /// (deserialize → `ir_version` → `validate_ir` → server-stamped ownership →
    /// advisory checksum-hint compare) and then LOWER the validated, owned IR to
    /// migrations. This is the single creator-facing entry the `.ir.json` deploy
    /// path calls — the peer of the platform `.sql` Flyway loader
    /// ([`crate::loader::load_dir`]), which never routes IR.
    ///
    /// `registry` is the project's table→owner map (drives the §8.6 ownership
    /// check); `live` the introspected [`LiveSchema`] facts — the tables already
    /// present (FK inline-vs-defer) AND the live UNIQUE-index names (the
    /// authoritative `dropIndex` destructive/approval gate, OR-ed with the IR hint).
    ///
    /// # Errors
    /// - [`LoadAndLowerError::Load`] — the load gate refused the artifact
    ///   (malformed, future ir_version, structural reject incl. the fail-closed
    ///   bare-name DropIndex, ownership violation, or checksum-hint mismatch).
    /// - [`LoadAndLowerError::Lower`] — lowering a validated op failed.
    pub fn load_and_lower(
        &self,
        bytes: &str,
        deploying_app: &str,
        registry: &std::collections::BTreeMap<String, String>,
        live: &LiveSchema,
    ) -> Result<Vec<Migration>, LoadAndLowerError> {
        let target = match self.dialect {
            SqlDialect::Postgres => crate::validate::Dialect::Postgres,
            SqlDialect::Sqlite => crate::validate::Dialect::Sqlite,
        };
        let ir = crate::ir_load::load_ir_document(bytes, deploying_app, target, registry)
            .map_err(LoadAndLowerError::Load)?;
        self.lower(&ir, live).map_err(LoadAndLowerError::Lower)
    }

    /// The PRODUCTION `.ir.json` deploy entry (§6.1.1 + §7.2): run the fail-closed
    /// LOAD GATE, then lower with **guard-per-fragment attribution**
    /// ([`lower_guarded`]) so a guard denial carries the exact op-index + kind to
    /// the creator (the 422), not a bare whole-`up` denial. Returns the lowered
    /// migrations + the per-op fragments + the tables this artifact CREATES (for
    /// the deploy loop's cross-file registry/live-set advance).
    ///
    /// This is the guard-attributed peer of [`load_and_lower`]: the deploy path
    /// calls THIS so the §6.1.1 attribution reaches a real deploy (the engine's
    /// subsequent whole-`up` guard is a belt-and-suspenders re-check, but the
    /// op-attributed denial happens HERE first).
    ///
    /// # Errors
    /// - [`LoadAndLowerGuardedError::Load`] — the load gate refused the artifact.
    /// - [`LoadAndLowerGuardedError::Lower`] — a lower failure, a guard-denied
    ///   fragment (op-index attributed), or a reassembly-invariant break.
    pub fn load_and_lower_guarded(
        &self,
        bytes: &str,
        deploying_app: &str,
        registry: &std::collections::BTreeMap<String, String>,
        live: &LiveSchema,
        guard_cfg: &GuardConfig,
    ) -> Result<LoweredArtifact, LoadAndLowerGuardedError> {
        let target = match self.dialect {
            SqlDialect::Postgres => crate::validate::Dialect::Postgres,
            SqlDialect::Sqlite => crate::validate::Dialect::Sqlite,
        };
        let ir = crate::ir_load::load_ir_document(bytes, deploying_app, target, registry)
            .map_err(LoadAndLowerGuardedError::Load)?;
        // The tables this artifact creates — folded by the caller into the
        // cross-file registry + live-set before the next `.ir.json`.
        let created_tables: Vec<String> = ir
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::CreateTable { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        let (migrations, fragments) = self
            .lower_guarded(&ir, guard_cfg, live)
            .map_err(LoadAndLowerGuardedError::Lower)?;
        // Wrap the lowered DDL steps as ONE AppliedPlan whose checksum is the
        // dialect-neutral `Checksum::of_ir` over the op list (§2.0 / §5.2), and
        // STAMP that same anchor onto every step's journaled `Migration.checksum`
        // (§5.3 / §2.6.1): the drift anchor that enters the journal is the
        // canonical op list, NOT the per-dialect rendered SQL. So a re-deploy of
        // the SAME `.ir.json` on EITHER backend re-derives the SAME anchor (no
        // false drift), while editing the authoring `.ts` (⇒ a different op list)
        // shifts the anchor and the executor's net-applied drift gate aborts.
        let plan = self.assemble_plan(&ir, migrations);
        Ok(LoweredArtifact { plan, fragments, created_tables })
    }

    /// Assemble the lowered DDL `Migration`s into ONE [`AppliedPlan`] (§2.0 / §5.2),
    /// stamping the dialect-neutral [`Checksum::of_ir`] anchor (§5.3) onto BOTH the
    /// plan and every step's journaled `Migration.checksum`.
    ///
    /// **Why stamp the op-list `of_ir` onto each step's checksum.** The journal
    /// records `Migration.checksum` and the executor's net-applied drift gate
    /// (`drift.rs`) compares the journaled value to the lowered `Migration.checksum`
    /// on re-deploy. Stamping the canonical-op-list `of_ir` there makes the
    /// journaled drift anchor the DIALECT-NEUTRAL op list (§2.6.1's "one plan
    /// checksum over the canonical op list, not the rendered SQL"), so the anchor is
    /// the SAME on a PG re-deploy and a SQLite re-deploy of the same artifact — and a
    /// `.ts` edit (a changed op list) is detected as drift regardless of dialect.
    /// The per-dialect rendered `up`/`down` still applies; only the IDENTITY anchor
    /// is the neutral op list.
    fn assemble_plan(&self, ir: &MigrationIr, mut migrations: Vec<Migration>) -> AppliedPlan {
        let anchor = crate::ir_load::authoritative_ir_checksum(ir);
        for m in &mut migrations {
            m.checksum = anchor.clone();
        }
        // The plan-group identity (§2.0.1): for PR1 the steps keep their own
        // per-op journal versions, so the plan `version` is a marker — the first
        // step's version (deterministic within a deploy), or a fresh id for the
        // degenerate empty plan (a no-op IR).
        let version = migrations
            .first()
            .map(|m| m.version.clone())
            .unwrap_or_else(crate::migration::MigrationId::generate);
        let steps: Vec<PlanStep> = migrations.into_iter().map(PlanStep::Ddl).collect();
        let rollbackable = AppliedPlan::compute_rollbackable(&steps);
        AppliedPlan {
            version,
            name: ir.name.clone(),
            steps,
            checksum: anchor,
            // PR1 lowers DDL with default-derived flags; the dialect-neutral
            // identity flags are the default set (the per-dialect transactional/
            // concurrently divergence is a render concern, NOT the identity — §2.4).
            flags: crate::migration::MigrationFlags::default(),
            dialect_scope: crate::plan::DialectScope::Both,
            rollbackable,
            owner_app: ir.owner_app.clone(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: ir.preconditions.clone(),
        }
    }

    /// Lower a validated [`MigrationIr`]'s DDL ops to ONE [`AppliedPlan`] (§2.0 /
    /// §5.2) — the named-contract peer of [`lower`](Self::lower) (which returns the
    /// flat `Vec<Migration>` the §6.4 byte-identity goldens compare). The plan's
    /// `checksum` is the dialect-neutral [`Checksum::of_ir`] anchor and each `Ddl`
    /// step's journaled checksum is stamped with it (§5.3 — see
    /// [`assemble_plan`](Self::assemble_plan)).
    ///
    /// # Errors
    /// Same as [`lower`](Self::lower).
    pub fn lower_plan(
        &self,
        ir: &MigrationIr,
        live: &LiveSchema,
    ) -> Result<AppliedPlan, IrLowerError> {
        let migrations = self.lower(ir, live)?;
        Ok(self.assemble_plan(ir, migrations))
    }

    /// Lower a validated [`MigrationIr`]'s DDL ops to [`Migration`]s.
    ///
    /// `live` carries the introspected [`LiveSchema`] facts: `live.tables` is the
    /// set of tables already present in the project (so an FK to a live target
    /// inlines, and a non-live target defers on PG / errors on SQLite — mirroring
    /// `diff`); `live.unique_indexes` is the authoritative set of live UNIQUE-index
    /// names that drives the `dropIndex` destructive/approval gate (OR-ed with the
    /// IR's advisory `unique` hint). Tables created EARLIER in the same IR are added
    /// to the working live-table set as lowering proceeds, so an intra-migration FK
    /// inlines correctly.
    ///
    /// # Errors
    /// - [`IrLowerError::Snapshot`] — the shared builder rejected an op's fields.
    /// - [`IrLowerError::UnsupportedOp`] — a non-DDL op (DML / online intent).
    pub fn lower(
        &self,
        ir: &MigrationIr,
        live: &LiveSchema,
    ) -> Result<Vec<Migration>, IrLowerError> {
        let mut out: Vec<Migration> = Vec::new();
        let mut live_tables: BTreeSet<String> = live.tables.clone();
        for op in &ir.ops {
            // The whole-up `lower` discards the structural statement list (it is the
            // §6.4 parity leg, which only compares the joined `up`); the guarded
            // path ([`lower_guarded`]) consumes the list to guard true statements.
            out.extend(
                self.lower_one_op(op, &mut live_tables, &live.unique_indexes)?
                    .into_iter()
                    .map(|(mig, _statements)| mig),
            );
        }
        Ok(out)
    }

    /// Lower a SINGLE op to its migration(s), advancing the working `live` set
    /// when the op creates a table (so a later intra-IR FK inlines). Factored out
    /// of [`lower`] so the guard-per-fragment path ([`lower_guarded`]) can attribute
    /// each op's rendered fragments to its op index (§6.1.1).
    ///
    /// # Errors
    /// - [`IrLowerError::Snapshot`] — the shared builder rejected the op's fields.
    /// - [`IrLowerError::UnsupportedOp`] — a non-DDL op (DML / online intent).
    fn lower_one_op(
        &self,
        op: &Op,
        live: &mut BTreeSet<String>,
        live_unique_indexes: &BTreeSet<String>,
    ) -> Result<Vec<LoweredUnit>, IrLowerError> {
        let migs = match op {
            Op::CreateTable { name, columns, .. } => {
                // A column carrying a SYNTH default (`now()`/`genRandomUuid()`) must
                // FAIL CLOSED, not silently lower with NO default. `ir_default_to_value`
                // maps `IrDefault::Fn → None` — correct for the system-field path the
                // differ owns (it never emits a volatile synth on a user column), but a
                // user-authored synth default would be SILENTLY DROPPED. Rendering a
                // synth default is the deferred Expr→SQL synth wave; until it lands, an
                // author-supplied synth default is refused here rather than lost.
                reject_synth_default(columns.iter().map(|c| (c.name.as_str(), c.default.as_ref())))?;
                let desc = self.create_table_descriptor(name, columns);
                let snap = build_table_snapshot(&self.project_schema, &desc, self.dialect)?;
                // The SQLite CREATE routes through the shared `zeroship_schema`
                // emitter, which consumes the SDK schema `Value` — built here from
                // the SAME descriptor bridge (`descriptor_to_sdk_schema`) the
                // differ's `desired_snapshot_for_dialect` uses, so the §6.4
                // byte-identity holds on the SQLite leg (the PG leg ignores it).
                let sqlite_schema = crate::declarative::descriptor_to_sdk_schema(&desc);
                let migs = self.decl.lower_create_table(name, &snap, &sqlite_schema, live)?;
                // The just-created table is now live for any later intra-IR FK.
                live.insert(name.clone());
                migs
            }
            Op::AddColumn { table, column, ty, nullable, default } => {
                // Fail-closed on a synth default (see the createTable arm): a
                // user-authored `now()`/`genRandomUuid()` must NOT be silently dropped.
                reject_synth_default(std::iter::once((column.as_str(), default.as_ref())))?;
                let col =
                    self.add_column_snapshot(table, column, ty, *nullable, default.as_ref())?;
                vec![self.decl.lower_add_column(table, &col)]
            }
            Op::CreateIndex { table, columns, name, unique, using, .. } => {
                let idx = create_index_snapshot(table, columns, name.as_deref(), *unique, *using);
                vec![self.decl.lower_create_index(table, &idx)]
            }
            Op::DropTable { table, .. } => vec![self.decl.lower_drop_table(table)],
            Op::DropColumn { table, column, .. } => {
                vec![self.decl.lower_drop_column(table, column)]
            }
            Op::DropIndex { name, unique, .. } => {
                // A bare-name DropIndex is rejected fail-closed UPSTREAM by the
                // validator (§8.6); a table-hinted one reaches here.
                //
                // The destructive/approval GATE is driven by the index's TRUE
                // uniqueness, resolved from the AUTHORITATIVE live catalog
                // (`live_unique_indexes`, introspected the SAME way the differ's
                // `render_drop_index` reads `IndexSnapshot::unique`) — NOT from the
                // author-supplied `unique` hint alone. The hint is advisory and is
                // OR-ed with the live fact: a hostile/buggy author who sets
                // `unique:false` (or omits it) on a drop of an ACTUALLY-unique index
                // can NOT defeat the gate. Dropping a UNIQUE index silently removes a
                // data-integrity guarantee (duplicate rows become possible; a later
                // re-add fails on the dirtied data), so it lowers
                // `destructive + requires_approval` and is REFUSED under
                // `Approval::None` rather than applied silently. A plain
                // (live-non-unique AND no hint) drop stays ungated/reversible. The
                // render is the same `DROP INDEX` either way; only the gating differs.
                //
                // Hint-only fallback: when the live facts are unavailable (a unit
                // lower with no introspected schema), `live_unique_indexes` is empty
                // and gating falls back to the hint — never LESS strict than before.
                let is_unique = unique.unwrap_or(false) || live_unique_indexes.contains(name);
                let idx = IndexSnapshot::btree(name.clone(), is_unique, Vec::new());
                vec![self.decl.lower_drop_index(&idx)]
            }
            Op::AlterColumnType { table, column, ty, using } => {
                // SQLite has NO `ALTER COLUMN` — a type change is reconciled by the
                // differ's 12-step table REBUILD, which needs the full live table
                // structure (not available in this pure-render lower). So stand-alone
                // alterColumnType lowers on PG only; on SQLite it routes through the
                // declarative diff rebuild seam (fail-closed here).
                self.require_pg_for("alterColumnType")?;
                // A `using` cast is a closed-AST `Expr`; rendering an `Expr` to SQL is
                // the Wave-C expression renderer (the same wave the DML executors wait
                // on). Until it lands, a cast-bearing type change cannot lower.
                if using.is_some() {
                    return Err(IrLowerError::ExprRenderDeferred("alterColumnType.using"));
                }
                // Build the desired `ColumnSnapshot` via the SHARED builder (a
                // one-field descriptor) so the emitted `data_type` is byte-identical
                // to the differ's type mapping — never re-spelled (§6.5).
                let col = self.add_column_snapshot(table, column, ty, None, None)?;
                vec![self.decl.lower_alter_column_type(table, &col)]
            }
            Op::AlterColumnNullability { table, column, nullable } => {
                // Same SQLite rebuild constraint as alterColumnType.
                self.require_pg_for("alterColumnNullability")?;
                vec![self.decl.lower_alter_column_nullability(table, column, *nullable)]
            }
            Op::RenameColumn { .. } => return Err(IrLowerError::UnsupportedOp("renameColumn")),
            Op::AddConstraint { table, constraint } => self.lower_add_constraint(table, constraint)?,
            Op::DropConstraint { table, name } => {
                // SQLite has no `ALTER TABLE … DROP CONSTRAINT` (rebuild-only); PG only.
                self.require_pg_for("dropConstraint")?;
                vec![self.decl.lower_drop_constraint(table, name)]
            }
            Op::Insert { .. } => return Err(IrLowerError::UnsupportedOp("insert")),
            Op::Update { .. } => return Err(IrLowerError::UnsupportedOp("update")),
            Op::Delete { .. } => return Err(IrLowerError::UnsupportedOp("delete")),
            Op::Backfill { .. } => return Err(IrLowerError::UnsupportedOp("backfill")),
        };
        Ok(migs)
    }

    /// **Guard-per-fragment + reassembly (§6.1.1).** Lower the IR's DDL ops and,
    /// for EACH op, guard every rendered SQL FRAGMENT individually — carrying the
    /// op index + kind — BEFORE the step's `up` is assembled. Only after all of an
    /// op's fragments pass the guard is the `up` reassembled by joining exactly
    /// those guarded fragments with the canonical `;\n` separator, and the
    /// byte-identity invariant `applied_up == join(guarded_fragments)` is asserted.
    ///
    /// A DENIED fragment aborts the WHOLE lower with the op-index attribution
    /// ([`FragmentGuardDenied`]) and applies NOTHING — there is no partial plan.
    ///
    /// Returns the per-op guarded fragments (for status/DX attribution) alongside
    /// the lowered migrations whose `up` is provably the reassembly of those exact
    /// fragments. The SQLite leg's guard ([`crate::guard::SqliteDescriptorGuard`])
    /// trusts descriptor-/IR-generated DDL (no string deny-list), so it never
    /// denies — but the fragment split + reassembly invariant still runs, so the
    /// `up`↔fragment correspondence holds on both dialects.
    ///
    /// # Errors
    /// - [`IrGuardedLowerError::Lower`] — an op failed to lower.
    /// - [`IrGuardedLowerError::Denied`] — a rendered fragment was guard-denied.
    /// - [`IrGuardedLowerError::ReassemblyMismatch`] — the fragment split did not
    ///   round-trip (engine bug; fail closed).
    pub fn lower_guarded(
        &self,
        ir: &MigrationIr,
        guard_cfg: &GuardConfig,
        live: &LiveSchema,
    ) -> Result<(Vec<Migration>, Vec<GuardedFragment>), IrGuardedLowerError> {
        let guard = guard_for(guard_cfg);
        let mut migrations: Vec<Migration> = Vec::new();
        let mut fragments: Vec<GuardedFragment> = Vec::new();
        let mut live_tables: BTreeSet<String> = live.tables.clone();

        for (op_index, op) in ir.ops.iter().enumerate() {
            let op_kind = op_kind_tag(op);
            // Lower this op (advancing `live_tables` for intra-IR FK inlining). A
            // lower failure aborts before any guarding — nothing applied. Each unit
            // carries its STRUCTURAL per-statement list (the exact statements the
            // renderer built, NOT a textual re-split of `up`).
            let op_units = self.lower_one_op(op, &mut live_tables, &live.unique_indexes)?;

            for (mig, statements) in op_units {
                // Guard EACH true statement individually so a denial is attributed
                // to THIS op (§6.1.1) — not buried in a concatenated blob. The
                // statements come STRUCTURALLY from the renderer (the CREATE/ALTER,
                // its `COMMENT ON COLUMN` side output, follow-on system indexes),
                // never from a textual `;\n` split — so a string-literal column
                // DEFAULT whose value itself contains `;\n` (e.g. `DEFAULT 'a;\nb'`)
                // is one whole statement, never broken mid-literal.
                for stmt in &statements {
                    guard.check(stmt).map_err(|source| FragmentGuardDenied {
                        op_index,
                        op_kind,
                        source,
                    })?;
                    fragments.push(GuardedFragment {
                        op_index,
                        op_kind,
                        sql: stmt.clone(),
                    });
                }
                // Byte-identity invariant: the step's `up` MUST be exactly the join
                // of the structural statements we just guarded — nothing inserted,
                // rewritten, or re-quoted between guarding and concatenation
                // (§6.1.1). With structural fragments this is the renderer's own
                // `join(";\n")` round-tripping, so it holds by construction; the
                // assertion remains a fail-closed engine-bug tripwire.
                let reassembled = statements.join(";\n");
                if reassembled != mig.up {
                    return Err(IrGuardedLowerError::ReassemblyMismatch {
                        name: mig.name.clone(),
                    });
                }
                migrations.push(mig);
            }
        }
        Ok((migrations, fragments))
    }

    /// Map an IR `createTable` op to the [`CollectionDescriptor`] the shared
    /// snapshot-builder consumes. Pure structural translation — no default /
    /// sentinel rendering (that lives in the shared builder, §6.5).
    fn create_table_descriptor(&self, name: &str, columns: &[IrColumn]) -> CollectionDescriptor {
        CollectionDescriptor {
            name: name.to_string(),
            owner_app: self.decl.owner_app().to_string(),
            fields: columns.iter().map(ir_column_to_field).collect(),
            indexes: Vec::new(),
        }
    }

    /// Build the [`ColumnSnapshot`] for an `addColumn` op by routing its single
    /// field through the SHARED builder (a one-field descriptor) and pulling the
    /// matching column out — so the default / encryption / comment sentinel is
    /// built by the shared kernel, never re-spelled here (§6.5).
    fn add_column_snapshot(
        &self,
        table: &str,
        column: &str,
        ty: &ColType,
        nullable: Option<bool>,
        default: Option<&IrDefault>,
    ) -> Result<ColumnSnapshot, IrLowerError> {
        let field = ir_column_to_field(&IrColumn {
            name: column.to_string(),
            ty: ty.clone(),
            nullable,
            default: default.cloned(),
            unique: None,
        });
        let desc = CollectionDescriptor {
            name: table.to_string(),
            owner_app: self.decl.owner_app().to_string(),
            fields: vec![field],
            indexes: Vec::new(),
        };
        let snap = build_table_snapshot(&self.project_schema, &desc, self.dialect)?;
        snap.columns
            .into_iter()
            .find(|c| c.name == column)
            .ok_or(IrLowerError::UnsupportedOp("addColumn (column folded away)"))
    }

    /// Fail closed unless the target dialect is Postgres — the stand-alone
    /// `alterColumn*` / `addConstraint` / `dropConstraint` render coverage (§6) is
    /// PG-native; SQLite reconciles these via the 12-step rebuild in the
    /// declarative diff path (which needs full live structure, not this
    /// pure-render lower). See [`IrLowerError::SqliteRebuildOnly`].
    fn require_pg_for(&self, op: &'static str) -> Result<(), IrLowerError> {
        match self.dialect {
            SqlDialect::Postgres => Ok(()),
            SqlDialect::Sqlite => Err(IrLowerError::SqliteRebuildOnly(op)),
        }
    }

    /// Lower a stand-alone `addConstraint` op (§6). FK / UNIQUE / PRIMARY KEY are
    /// column-list constraints that lower to `ALTER TABLE … ADD CONSTRAINT …` on
    /// Postgres, reusing the differ's render seam (so an FK is byte-identical to a
    /// deferred FK). A `CHECK` constraint carries a closed-AST `Expr` predicate
    /// whose SQL rendering is the later expression wave, so it lowers to
    /// [`IrLowerError::ExprRenderDeferred`] until that renderer lands. SQLite is
    /// rebuild-only ([`IrLowerError::SqliteRebuildOnly`]).
    fn lower_add_constraint(
        &self,
        table: &str,
        constraint: &IrConstraint,
    ) -> Result<Vec<LoweredUnit>, IrLowerError> {
        self.require_pg_for("addConstraint")?;
        let name = constraint.name.as_deref();
        let mig = match &constraint.kind {
            IrConstraintKind::Fk { columns, references_table, .. } => {
                // PR1 single-column FK (the `ref` shape references the target's
                // `id`); a multi-column FK is a later wave.
                let local = columns.first().ok_or(IrLowerError::UnsupportedOp(
                    "addConstraint(fk) with no local column",
                ))?;
                if columns.len() != 1 {
                    return Err(IrLowerError::UnsupportedOp(
                        "addConstraint(fk) multi-column (later wave)",
                    ));
                }
                let fk = crate::declarative::ir_fk_constraint_snapshot(
                    &self.project_schema,
                    name,
                    local,
                    references_table,
                );
                self.decl.lower_add_fk(table, &fk)
            }
            IrConstraintKind::Unique { columns } => {
                let body = format!("UNIQUE ({})", quote_cols(columns));
                let cname =
                    name.map_or_else(|| derived_constraint_name(table, columns, "key"), str::to_string);
                // A UNIQUE add on an existing table scans + locks and can fail on
                // existing duplicates — gated (requires_approval), like SET NOT NULL.
                self.decl.lower_add_constraint(table, &cname, &body, true)
            }
            IrConstraintKind::Pk { columns } => {
                let body = format!("PRIMARY KEY ({})", quote_cols(columns));
                let cname =
                    name.map_or_else(|| derived_constraint_name(table, columns, "pkey"), str::to_string);
                // A PK add scans + locks the whole table under ACCESS EXCLUSIVE and
                // fails on a NULL/duplicate key — gated (requires_approval).
                self.decl.lower_add_constraint(table, &cname, &body, true)
            }
            IrConstraintKind::Check { .. } => {
                // A CHECK predicate is a closed-AST `Expr`; rendering it to SQL is
                // the Wave-C expression renderer (no `Expr`→SQL path yet).
                return Err(IrLowerError::ExprRenderDeferred("addConstraint(check)"));
            }
        };
        Ok(vec![mig])
    }

}

/// **Test-only** textual `;\n` split, retained for the reassembly assertions in
/// migrations whose `up` carries NO interior `;\n` (a plain column, an encrypted
/// column → `CREATE;\nCOMMENT`). The PRODUCTION guarded path
/// ([`IrAuthor::lower_guarded`]) NO LONGER splits textually — it carries the
/// renderer's STRUCTURAL per-statement list ([`crate::declarative::LoweredUnit`])
/// instead, so a string-literal column DEFAULT whose value itself contains `;\n`
/// (e.g. `DEFAULT 'a;\nb'`) is never broken mid-statement. This helper would
/// over-split such an `up`; it is kept only for tests that do not exercise that
/// case.
#[cfg(test)]
fn split_up_fragments(up: &str) -> Vec<&str> {
    up.split(";\n").collect()
}

/// The op kind tag for §6.1.1 attribution — the human-facing name the guard
/// denial / status surface leads with.
const fn op_kind_tag(op: &Op) -> &'static str {
    match op {
        Op::CreateTable { .. } => "createTable",
        Op::AddColumn { .. } => "addColumn",
        Op::CreateIndex { .. } => "createIndex",
        Op::DropTable { .. } => "dropTable",
        Op::DropColumn { .. } => "dropColumn",
        Op::DropIndex { .. } => "dropIndex",
        Op::AlterColumnType { .. } => "alterColumnType",
        Op::AlterColumnNullability { .. } => "alterColumnNullability",
        Op::RenameColumn { .. } => "renameColumn",
        Op::AddConstraint { .. } => "addConstraint",
        Op::DropConstraint { .. } => "dropConstraint",
        Op::Insert { .. } => "insert",
        Op::Update { .. } => "update",
        Op::Delete { .. } => "delete",
        Op::Backfill { .. } => "backfill",
    }
}

/// Build the [`IndexSnapshot`] for a `createIndex` op. A plain B-tree index is
/// the common case; a non-`btree` `using` carries the access method. Pure
/// translation (no state), so a free function.
fn create_index_snapshot(
    table: &str,
    columns: &[String],
    name: Option<&str>,
    unique: Option<bool>,
    using: Option<IndexMethod>,
) -> IndexSnapshot {
    let idx_name = name.map_or_else(
        || crate::author::cap_ident_name(&format!("{table}_{}_idx", columns.join("_"))),
        ToString::to_string,
    );
    let unique = unique.unwrap_or(false);
    let mut idx = IndexSnapshot::btree(idx_name, unique, columns.to_vec());
    if let Some(m) = using {
        idx.access_method = index_method_access(m).to_string();
    }
    idx
}

/// Map an [`IrColumn`] to the [`FieldDescriptor`] the shared snapshot-builder
/// consumes. Pure structural translation of the type + nullability + default +
/// unique; the snapshot's default/sentinel rendering is the shared builder's job.
fn ir_column_to_field(c: &IrColumn) -> FieldDescriptor {
    // `nullable` defaults to TRUE (the `t.*` lexicon — §3.2); `required` is the
    // inverse the descriptor models. An explicit `nullable: false` ⇒ required.
    let required = !c.nullable.unwrap_or(true);
    let (ty, references) = col_type_to_token(&c.ty);
    // An ENCRYPTED column carries the inner token as `ty` PLUS the `encrypted`
    // facet — the shared builder reads the facet to pick BYTEA + the `zsenc`
    // sentinel (built by the shared kernel, never re-spelled here, §6.5). The
    // empty `{}` selects the kernel's defaults (`randomised:default:<inner>`),
    // matching the differ's `t.encrypted(...)` shape.
    let encrypted = matches!(c.ty, ColType::Encrypted { .. })
        .then(|| serde_json::json!({}));
    FieldDescriptor {
        name: c.name.clone(),
        ty,
        required,
        unique: c.unique.unwrap_or(false),
        references,
        default: c.default.as_ref().and_then(ir_default_to_value),
        encrypted,
        ..Default::default()
    }
}

/// Map a closed [`ColType`] to the descriptor's `(type_token, references?)`. The
/// tokens are exactly the SDK `FieldDef` type spellings the shared kernel maps
/// (`def_to_column_type_for_dialect`).
fn col_type_to_token(ty: &ColType) -> (String, Option<String>) {
    match ty {
        ColType::String => ("string".into(), None),
        ColType::Text => ("string".into(), None),
        ColType::Int | ColType::BigInt => ("int".into(), None),
        ColType::Float => ("number".into(), None),
        ColType::Bool => ("boolean".into(), None),
        ColType::Json => ("json".into(), None),
        ColType::Timestamp => ("date".into(), None),
        ColType::Uuid => ("string".into(), None),
        ColType::Bytea => ("bytes".into(), None),
        ColType::Ref { references } => ("ref".into(), Some(references.clone())),
        ColType::Vector { .. } => ("vector".into(), None),
        ColType::GeoPoint => ("geoPoint".into(), None),
        ColType::Decimal { .. } => ("number".into(), None),
        // An encrypted column wraps an inner type; the descriptor carries it as the
        // inner token with the `encrypted` facet set (the shared builder reads the
        // facet to pick BYTEA + the sentinel). The inner token drives the masked
        // sibling's plaintext shape.
        ColType::Encrypted { of } => {
            let (inner, _) = col_type_to_token(of);
            (inner, None)
        }
    }
}

/// Fail-closed guard: refuse any column carrying a SYNTH default
/// (`IrDefault::Fn`, i.e. `now()`/`genRandomUuid()`) at lower, rather than letting
/// [`ir_default_to_value`] silently map it to `None` (no default). This closes the
/// self-footgun the §LOW finding flagged: a user-authored synth default would
/// otherwise be DROPPED with no error. Rendering a synth default to per-dialect SQL
/// is the deferred Expr→SQL synth wave (the same wave DML waits on); until then an
/// author-supplied synth default is REFUSED ([`IrLowerError::ExprRenderDeferred`]),
/// never lost.
///
/// The differ's own system-field path (`id`/timestamps) injects its volatile
/// defaults through the shared builder, NOT through an `IrDefault::Fn` on a user
/// column, so this guard never trips on a legitimate createTable — only on an
/// explicit author-supplied synth default the renderer cannot yet materialize.
fn reject_synth_default<'a>(
    cols: impl Iterator<Item = (&'a str, Option<&'a IrDefault>)>,
) -> Result<(), IrLowerError> {
    for (_name, default) in cols {
        if matches!(default, Some(IrDefault::Fn { .. })) {
            return Err(IrLowerError::ExprRenderDeferred("column default (synth now/genRandomUuid)"));
        }
    }
    Ok(())
}

/// Map an [`IrDefault`] to the descriptor's `default` JSON value. A literal maps
/// to its scalar. A synth `now`/`genRandomUuid` maps to `None` HERE — but it is
/// never reached for a user-authored synth default, because [`reject_synth_default`]
/// fails that case CLOSED at lower (the self-footgun fix): the silent `None` is
/// reserved for the differ's system-field path, which never routes a synth through
/// this function. So `None` here matches the differ (which never sees a synth
/// default on an autogenerated column) WITHOUT silently dropping an author's
/// request.
fn ir_default_to_value(d: &IrDefault) -> Option<serde_json::Value> {
    use crate::ir::IrScalar;
    use serde_json::Value;
    match d {
        IrDefault::Literal { value } => Some(match value {
            IrScalar::Null => Value::Null,
            IrScalar::Bool(b) => Value::Bool(*b),
            IrScalar::Int(i) => Value::from(*i),
            // A decimal is carried as its canonical string; the descriptor's
            // `default` is rendered as a literal by the shared builder.
            IrScalar::Decimal(s) => Value::String(s.clone()),
            IrScalar::Str(s) => Value::String(s.clone()),
            // A bytes default is not an autogenerated-column default the differ
            // emits; carry it as its canonical base64 string for completeness.
            IrScalar::Bytes(b) => {
                use base64::Engine as _;
                Value::String(base64::engine::general_purpose::STANDARD.encode(b))
            }
        }),
        IrDefault::Fn { .. } => None,
    }
}

/// Quote + comma-join a constraint's column list (`"a", "b"`). Each identifier is
/// double-quoted (embedded `"` doubled) so the column list can never alter the
/// statement structure — the SAME quoting the declarative emitter uses.
fn quote_cols(cols: &[String]) -> String {
    cols.iter()
        .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A deterministic constraint name for an unnamed UNIQUE/PK add:
/// `<table>_<cols>_<suffix>` (`key` for UNIQUE, `pkey` for PRIMARY KEY), capped to
/// the server-side identifier limit via [`crate::author::cap_ident_name`] so the
/// authored name matches what PG stores (an un-capped name would be truncated on
/// CREATE and never round-trip).
fn derived_constraint_name(table: &str, cols: &[String], suffix: &str) -> String {
    crate::author::cap_ident_name(&format!("{table}_{}_{suffix}", cols.join("_")))
}

/// The access-method string for a closed [`IndexMethod`] — matches the spellings
/// the snapshot's `access_method` carries (and `render_create_index` emits).
fn index_method_access(m: IndexMethod) -> &'static str {
    match m {
        IndexMethod::Btree => "btree",
        IndexMethod::Gin => "gin",
        IndexMethod::Gist => "gist",
        IndexMethod::Ivfflat => "ivfflat",
        IndexMethod::Hnsw => "hnsw",
        IndexMethod::Fts5 => "fts5",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn registry(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(t, o)| (t.to_string(), o.to_string())).collect()
    }

    use crate::ir::{IrColumn as TIrColumn, IrFlagsOverride};

    /// Build a one-op `createTable` IR for the guard-per-fragment tests.
    fn create_table_ir(table: &str, cols: Vec<TIrColumn>) -> MigrationIr {
        MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::CreateTable {
                name: table.into(),
                columns: cols,
                constraints: vec![],
                indexes: vec![],
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        }
    }

    // §6.1.1 — the byte-identity invariant: for a MULTI-statement op (a
    // createTable with an encrypted column → `CREATE TABLE …;\nCOMMENT ON COLUMN
    // …`), the lowered `up` is byte-identical to the join of the individually
    // guarded fragments, and >1 fragment is actually guarded.
    #[test]
    fn guard_per_fragment_reassembly_is_byte_identical_pg() {
        let ir = create_table_ir(
            "vault",
            vec![TIrColumn {
                name: "secret".into(),
                ty: ColType::Encrypted { of: Box::new(ColType::String) },
                nullable: None,
                default: None,
                unique: None,
            }],
        );
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let guard_cfg = GuardConfig::confined("app".to_string());
        let (migs, frags) = author
            .lower_guarded(&ir, &guard_cfg, &LiveSchema::default())
            .expect("guarded lower of a clean createTable passes");

        // The createTable emits a multi-statement `up` (CREATE + COMMENT sentinel),
        // so MORE THAN ONE fragment is guarded for op #0.
        let op0_frags: Vec<_> = frags.iter().filter(|f| f.op_index == 0).collect();
        assert!(
            op0_frags.len() >= 2,
            "an encrypted-column createTable renders >1 fragment (CREATE + COMMENT); got {}",
            op0_frags.len()
        );
        assert!(op0_frags.iter().all(|f| f.op_kind == "createTable"));
        assert!(
            op0_frags.iter().any(|f| f.sql.contains("COMMENT ON COLUMN")),
            "the COMMENT sentinel is a SEPARATELY-guarded fragment"
        );

        // Reassembly: each migration's `up` == join of THAT migration's guarded
        // fragments with `;\n` (the invariant `lower_guarded` enforces). Verify it
        // independently here over the createTable migration.
        let create_mig = migs
            .iter()
            .find(|m| m.up.contains("CREATE TABLE"))
            .expect("a CREATE migration");
        let reassembled = split_up_fragments(&create_mig.up).join(";\n");
        assert_eq!(reassembled, create_mig.up, "reassembly must be byte-identical");
    }

    // §6.1.1 — a DENIED fragment aborts the WHOLE lower with the op-index
    // attribution, and NOTHING is applied. We force a denial by guarding the
    // rendered `"app".…` DDL under a guard CONFINED to a DIFFERENT schema, so the
    // qualified reference is a cross-schema construct the guard refuses — the same
    // refusal a hostile cross-tenant fragment would trigger.
    #[test]
    fn guard_per_fragment_denied_aborts_with_op_index_pg() {
        let ir = create_table_ir(
            "widgets",
            vec![TIrColumn {
                name: "title".into(),
                ty: ColType::String,
                nullable: None,
                default: None,
                unique: None,
            }],
        );
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        // Guard confined to "other" — the rendered `CREATE TABLE "app".…` is then a
        // cross-schema reference the Confined guard denies.
        let guard_cfg = GuardConfig::confined("other".to_string());
        let err = author
            .lower_guarded(&ir, &guard_cfg, &LiveSchema::default())
            .expect_err("a fragment outside the confined schema must be denied");
        match err {
            IrGuardedLowerError::Denied(d) => {
                assert_eq!(d.op_index, 0, "the denial attributes to op #0");
                assert_eq!(d.op_kind, "createTable");
            }
            other => panic!("expected a per-fragment Denied, got: {other}"),
        }
    }

    // §6.1.1 — the SQLite leg: the descriptor guard trusts IR-generated DDL (no
    // string deny-list), so it never denies, but the fragment split + reassembly
    // invariant still runs and holds on SQLite.
    #[test]
    fn guard_per_fragment_reassembly_holds_sqlite() {
        let ir = create_table_ir(
            "widgets",
            vec![TIrColumn {
                name: "title".into(),
                ty: ColType::String,
                nullable: Some(false),
                default: None,
                unique: None,
            }],
        );
        let author = IrAuthor::new("app", "app_a", SqlDialect::Sqlite);
        let guard_cfg = GuardConfig::confined_sqlite("app".to_string());
        let (migs, frags) = author
            .lower_guarded(&ir, &guard_cfg, &LiveSchema::default())
            .expect("SQLite guarded lower passes (descriptor guard trusts IR DDL)");
        assert!(!frags.is_empty(), "fragments are still attributed on SQLite");
        for m in &migs {
            let reassembled = split_up_fragments(&m.up).join(";\n");
            assert_eq!(reassembled, m.up, "SQLite reassembly must be byte-identical");
        }
    }

    // MED (code-critic): a LEGITIMATE portable string-literal column DEFAULT whose
    // value CONTAINS the substring `;\n` must lower CLEANLY through the production
    // `lower_guarded` path — the fragment split MUST NOT break the single
    // CREATE/ADD statement on the interior `;\n` of the quoted literal. `sql_str`
    // escapes ONLY `'` (never a newline/semicolon), so `DEFAULT 'a;\nb'` renders an
    // `up` with an interior `;\n`. Pre-fix the TEXTUAL `split_up_fragments(";\n")`
    // over-split this single statement into two malformed fragments, tripping
    // `ReassemblyMismatch` (or a guard denial on a syntactically-broken half) — so a
    // valid default was non-deployable via the IR deploy path. Post-fix the
    // fragments are carried STRUCTURALLY (one fragment per TRUE statement), the
    // interior `;\n` stays inside its statement, and `join(";\n") == up` holds.
    #[test]
    fn string_default_with_embedded_semicolon_newline_lowers_clean_pg() {
        // The portable default value literally contains `;\n` (and a bare `;`).
        let nasty = "a;\nb;c";
        let ir = create_table_ir(
            "docs",
            vec![TIrColumn {
                name: "note".into(),
                ty: ColType::String,
                nullable: Some(false),
                default: Some(IrDefault::Literal {
                    value: crate::ir::IrScalar::Str(nasty.into()),
                }),
                unique: None,
            }],
        );
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let guard_cfg = GuardConfig::confined("app".to_string());

        // The whole-up `lower` is the canonical reference (the §6.4 parity leg).
        let whole = author
            .lower(&ir, &LiveSchema::default())
            .expect("whole-up lower of a string default succeeds");
        let whole_create = whole
            .iter()
            .find(|m| m.up.contains("CREATE TABLE"))
            .expect("a CREATE migration");
        // Sanity: the rendered `up` REALLY carries the interior `;\n` (the trap).
        assert!(
            whole_create.up.contains("DEFAULT 'a;\nb;c'"),
            "the string default must render with its embedded ;\\n verbatim; up = {:?}",
            whole_create.up
        );

        // The PRODUCTION guarded path must NOT trip ReassemblyMismatch / deny the
        // valid default.
        let (migs, frags) = author
            .lower_guarded(&ir, &guard_cfg, &LiveSchema::default())
            .expect("guarded lower of a portable ;\\n string default must succeed");

        // The guarded createTable `up` is byte-identical to the whole-up reference.
        let guarded_create = migs
            .iter()
            .find(|m| m.up.contains("CREATE TABLE"))
            .expect("a guarded CREATE migration");
        assert_eq!(
            guarded_create.up, whole_create.up,
            "the guarded create `up` must be byte-identical to the whole-up lower"
        );

        // The CREATE TABLE is exactly ONE structural fragment for op #0 (the
        // interior `;\n` of the literal did NOT split it). The DEFAULT lives whole
        // inside that single fragment.
        let create_frag = frags
            .iter()
            .find(|f| f.op_index == 0 && f.sql.contains("CREATE TABLE"))
            .expect("a CREATE TABLE fragment attributed to op #0");
        assert_eq!(create_frag.op_kind, "createTable");
        assert!(
            create_frag.sql.contains("DEFAULT 'a;\nb;c'"),
            "the whole string default (incl. its ;\\n) stays inside ONE fragment; got {:?}",
            create_frag.sql
        );
    }

    // MED (code-critic): an IR dropIndex of a UNIQUE index must lower
    // `destructive + requires_approval` — exactly like the differ's
    // `render_drop_index` gates a unique-index drop — so it is REFUSED under
    // `Approval::None` and never applies silently. A plain (non-unique) index drop
    // stays ungated. Pre-fix, IrAuthor hardcoded `unique:false`, so a unique drop
    // lowered ungated (the regression this pins).
    #[test]
    fn drop_unique_index_lowers_destructive_and_approval_gated() {
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);

        // A UNIQUE-index drop: gated.
        let ir_unique = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::DropIndex {
                name: "users_email_uniq".into(),
                table: Some("users".into()),
                unique: Some(true),
                if_exists: None,
                concurrently: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let migs = author.lower(&ir_unique, &LiveSchema::default()).expect("lower");
        let m = migs.iter().find(|m| m.up.contains("DROP INDEX")).expect("a DROP INDEX");
        assert!(
            m.flags.destructive,
            "a unique-index drop must lower destructive (removes a data-integrity guarantee)"
        );
        assert!(
            m.flags.requires_approval,
            "a unique-index drop must lower requires_approval (refused under Approval::None)"
        );

        // A PLAIN (non-unique) index drop: ungated, reversible.
        let ir_plain = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::DropIndex {
                name: "users_created_at_idx".into(),
                table: Some("users".into()),
                unique: None,
                if_exists: None,
                concurrently: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let migs = author.lower(&ir_plain, &LiveSchema::default()).expect("lower");
        let m = migs.iter().find(|m| m.up.contains("DROP INDEX")).expect("a DROP INDEX");
        assert!(!m.flags.destructive, "a plain index drop stays non-destructive");
        assert!(!m.flags.requires_approval, "a plain index drop stays ungated");
    }

    /// Byte-compare a [`ColumnSnapshot`] including the EMISSION-ONLY facets that its
    /// `PartialEq` excludes (`default` + the two sentinels). The §6.5 fixtures pin
    /// EXACTLY those excluded fields (the encryption / comment sentinels), so a
    /// plain `==` would not detect a sentinel divergence — we assert them field by
    /// field.
    fn assert_col_byte_eq(a: &ColumnSnapshot, b: &ColumnSnapshot, ctx: &str) {
        assert_eq!(a.name, b.name, "{ctx}: name");
        assert_eq!(a.data_type, b.data_type, "{ctx}: data_type");
        assert_eq!(a.nullable, b.nullable, "{ctx}: nullable");
        assert_eq!(a.default, b.default, "{ctx}: default (emission-only)");
        assert_eq!(
            a.encryption_sentinel, b.encryption_sentinel,
            "{ctx}: encryption_sentinel (emission-only, the §6.5 fixture-1 property)"
        );
        assert_eq!(
            a.comment_sentinel, b.comment_sentinel,
            "{ctx}: comment_sentinel (emission-only, the §6.5 fixture-1 property)"
        );
    }

    // §6.5 FIXTURE 1 (code-critic LOW, snapshot-level): `IrAuthor`'s `addColumn` of an
    // ENCRYPTED column yields a `ColumnSnapshot` whose `encryption_sentinel` +
    // `comment_sentinel` are BYTE-EQUAL to the differ's — pinned at the SNAPSHOT
    // layer, independent of the §6.4 render golden. Because both paths route the
    // field through the SAME shared `build_table_snapshot`, the property holds by
    // construction; this fixture is the dedicated regression-pin the spec enumerates
    // so a future divergence in IrAuthor's op→descriptor mapping (e.g. dropping the
    // `encrypted` facet) is caught at the snapshot layer, not only via render.
    #[test]
    fn ir_author_encrypted_addcolumn_snapshot_is_byte_equal_to_differ_pg() {
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            let author = IrAuthor::new("app", "app_a", dialect);

            // IrAuthor's snapshot for the encrypted column (its real lowering seam).
            let ir_col = author
                .add_column_snapshot(
                    "vault",
                    "secret",
                    &ColType::Encrypted { of: Box::new(ColType::String) },
                    None,
                    None,
                )
                .expect("ir add_column_snapshot");

            // The differ's snapshot for the SAME field, via the SAME shared builder
            // fed from a `t.encrypted(...)`-shaped descriptor (`encrypted: {}` selects
            // the kernel defaults — the shape `ir_column_to_field` emits).
            let desc = CollectionDescriptor {
                name: "vault".into(),
                owner_app: "app_a".into(),
                fields: vec![FieldDescriptor {
                    name: "secret".into(),
                    ty: "string".into(),
                    encrypted: Some(serde_json::json!({})),
                    ..Default::default()
                }],
                indexes: vec![],
            };
            let differ_snap = build_table_snapshot("app", &desc, dialect).expect("differ snapshot");
            let differ_col = differ_snap
                .columns
                .iter()
                .find(|c| c.name == "secret")
                .expect("differ secret column");

            assert_col_byte_eq(&ir_col, differ_col, &format!("{dialect:?} encrypted addColumn"));
            // The encrypted column actually CARRIES a sentinel (so the equality above
            // is a meaningful pin, not a None==None tautology).
            assert!(
                ir_col.encryption_sentinel.is_some() || ir_col.comment_sentinel.is_some(),
                "{dialect:?}: an encrypted column must carry an encryption/comment sentinel"
            );
        }
    }

    // §6.5 FIXTURE 2 (code-critic LOW, snapshot-level): `IrAuthor`'s `createTable`
    // injects the SEVEN system fields + THREE system indexes BYTE-EQUAL to the
    // differ's `desired_snapshot` TableSnapshot. Pinned at the snapshot layer,
    // independent of the §6.4 render golden — so a future fork of IrAuthor's
    // descriptor mapping that drops/renames a system field or index is caught here.
    #[test]
    fn ir_author_createtable_snapshot_injects_system_fields_byte_equal_to_differ() {
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            let author = IrAuthor::new("app", "app_a", dialect);
            let user_cols = vec![
                TIrColumn {
                    name: "title".into(),
                    ty: ColType::Text,
                    nullable: Some(false),
                    default: None,
                    unique: None,
                },
            ];

            // IrAuthor's createTable snapshot (its real lowering seam: the private
            // descriptor mapping → shared builder).
            let ir_desc = author.create_table_descriptor("notes", &user_cols);
            let ir_snap = build_table_snapshot("app", &ir_desc, dialect).expect("ir snapshot");

            // The differ's snapshot for the SAME user-facing table.
            let differ_desc = CollectionDescriptor {
                name: "notes".into(),
                owner_app: "app_a".into(),
                fields: vec![FieldDescriptor {
                    name: "title".into(),
                    ty: "string".into(),
                    required: true,
                    ..Default::default()
                }],
                indexes: vec![],
            };
            let differ_snap =
                build_table_snapshot("app", &differ_desc, dialect).expect("differ snapshot");

            // The full TableSnapshot (columns + indexes + constraints) is byte-equal
            // — system fields injected identically. `TableSnapshot`'s `==` covers
            // columns/indexes/constraints; the per-column sentinels of the (non-
            // encrypted) system fields are all `None`, so `==` is exact here.
            assert_eq!(
                ir_snap.columns, differ_snap.columns,
                "{dialect:?}: createTable columns (incl. injected system fields) must be byte-equal"
            );
            assert_eq!(
                ir_snap.indexes, differ_snap.indexes,
                "{dialect:?}: createTable indexes (incl. system indexes) must be byte-equal"
            );
            assert_eq!(
                ir_snap.constraints, differ_snap.constraints,
                "{dialect:?}: createTable constraints must be byte-equal"
            );
            // The injected system fields are actually PRESENT (so the equality is a
            // meaningful pin). The seven system fields include `id`, `created_at`,
            // `updated_at` — assert a representative subset by name.
            for sys in ["id", "created_at", "updated_at"] {
                assert!(
                    ir_snap.columns.iter().any(|c| c.name == sys),
                    "{dialect:?}: system field {sys:?} must be injected by createTable"
                );
            }
        }
    }

    // LOW (code-critic, this fix): an author-supplied SYNTH default
    // (`now()`/`genRandomUuid()`) on a user column must FAIL CLOSED at lower
    // (`ExprRenderDeferred`), NOT be silently dropped to no-default. Pre-fix
    // `ir_default_to_value` mapped `IrDefault::Fn → None`, so the lower SUCCEEDED and
    // the column emitted with NO default — the author's request silently lost (the
    // self-footgun this pins). RED before the fix: `lower` returns Ok with a
    // default-less CREATE; GREEN after: `lower` returns `ExprRenderDeferred`.
    #[test]
    fn synth_default_on_user_column_fails_closed_not_silently_dropped() {
        use crate::ir::SynthDefaultFn;
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);

        // createTable with a column whose default is a synth `now()`.
        let ir_create = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::CreateTable {
                name: "events".into(),
                columns: vec![TIrColumn {
                    name: "at".into(),
                    ty: ColType::Timestamp,
                    nullable: None,
                    default: Some(IrDefault::Fn { r#fn: SynthDefaultFn::Now }),
                    unique: None,
                }],
                constraints: vec![],
                indexes: vec![],
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let err = author
            .lower(&ir_create, &LiveSchema::default())
            .expect_err("a synth default on a user column must fail closed, not silently drop");
        assert!(
            matches!(err, IrLowerError::ExprRenderDeferred(slot) if slot.contains("synth")),
            "expected ExprRenderDeferred for the synth default, got {err:?}"
        );

        // addColumn with a synth `genRandomUuid()` default — same fail-closed.
        let ir_add = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::AddColumn {
                table: "events".into(),
                column: "token".into(),
                ty: ColType::Uuid,
                nullable: Some(false),
                default: Some(IrDefault::Fn { r#fn: SynthDefaultFn::GenRandomUuid }),
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let err = author
            .lower(&ir_add, &LiveSchema::default())
            .expect_err("a synth default on an addColumn must fail closed");
        assert!(
            matches!(err, IrLowerError::ExprRenderDeferred(slot) if slot.contains("synth")),
            "expected ExprRenderDeferred for the addColumn synth default, got {err:?}"
        );

        // A LITERAL default still lowers fine (the guard is synth-specific, not a
        // blanket default ban).
        let ir_lit = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::AddColumn {
                table: "events".into(),
                column: "kind".into(),
                ty: ColType::Text,
                nullable: Some(false),
                default: Some(IrDefault::Literal { value: crate::ir::IrScalar::Str("x".into()) }),
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        author
            .lower(&ir_lit, &LiveSchema::default())
            .expect("a literal default must still lower");
    }

    // MED-1 (code-critic, this fix): the destructive/approval gate for a UNIQUE-index
    // drop must NOT trust the author-supplied `unique` hint alone — it must resolve
    // the index's TRUE uniqueness from the AUTHORITATIVE live catalog
    // (`LiveSchema::unique_indexes`, the same source the differ's `render_drop_index`
    // reads), OR-ed with the hint. A hostile/buggy author who sets `unique:false`
    // (or omits it) on a drop of an actually-unique index must STILL lower
    // `destructive + requires_approval`, so the drop is refused under
    // `Approval::None` rather than silently removing a data-integrity guarantee.
    //
    // RED before the fix: pre-fix the gate read `unique.unwrap_or(false)` ONLY, so a
    // `unique:false`/absent drop of a live-unique index lowered UNGATED (the
    // approval-gate bypass this pins).
    #[test]
    fn drop_index_uniqueness_resolved_from_live_overrides_understated_hint() {
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);

        // The index IS unique in the live catalog…
        let mut live = LiveSchema::default();
        live.unique_indexes.insert("users_email_uniq".to_string());

        // …but the author UNDER-DECLARES it (`unique:false`) on the drop — a
        // hostile/buggy hint that must NOT defeat the gate.
        let ir_understated = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::DropIndex {
                name: "users_email_uniq".into(),
                table: Some("users".into()),
                unique: Some(false),
                if_exists: None,
                concurrently: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let migs = author.lower(&ir_understated, &live).expect("lower");
        let m = migs.iter().find(|m| m.up.contains("DROP INDEX")).expect("a DROP INDEX");
        assert!(
            m.flags.destructive,
            "a drop of a LIVE-unique index must lower destructive even when the IR hint says unique:false"
        );
        assert!(
            m.flags.requires_approval,
            "a drop of a LIVE-unique index must lower requires_approval even when the IR hint under-declares it"
        );

        // The SAME drop with an EMPTY live set (no introspection) falls back to the
        // hint alone — `unique:false` ⇒ ungated. The live fact, when present, is what
        // adds the gate; the hint-only fallback is never LESS strict than the hint.
        let migs_no_live = author.lower(&ir_understated, &LiveSchema::default()).expect("lower");
        let m = migs_no_live.iter().find(|m| m.up.contains("DROP INDEX")).expect("a DROP INDEX");
        assert!(
            !m.flags.destructive && !m.flags.requires_approval,
            "with no live facts, the gate falls back to the (false) hint — ungated"
        );

        // And a live-unique index dropped with an ABSENT hint (the common
        // omit-the-flag case) is ALSO gated by the live fact.
        let ir_absent_hint = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::DropIndex {
                name: "users_email_uniq".into(),
                table: Some("users".into()),
                unique: None,
                if_exists: None,
                concurrently: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let migs = author.lower(&ir_absent_hint, &live).expect("lower");
        let m = migs.iter().find(|m| m.up.contains("DROP INDEX")).expect("a DROP INDEX");
        assert!(
            m.flags.destructive && m.flags.requires_approval,
            "a drop of a LIVE-unique index with no hint must STILL be gated by the live fact"
        );
    }

    // The loader's IR branch end-to-end (§7.2): a well-formed `.ir.json`
    // createTable by its declarer loads (fail-closed gate passes) AND lowers to a
    // CREATE TABLE migration.
    #[test]
    fn load_and_lower_create_table_end_to_end() {
        let bytes = r#"{"ir_version":1,"name":"m","ops":[
            {"op":"createTable","name":"fresh","columns":[{"name":"title","type":"text"}]}
        ]}"#;
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let migs = author
            .load_and_lower(bytes, "app_a", &registry(&[]), &LiveSchema::default())
            .expect("a fresh createTable by its declarer loads + lowers");
        assert!(
            migs.iter().any(|m| m.up.contains("CREATE TABLE \"app\".\"fresh\"")),
            "lowering must emit the CREATE TABLE"
        );
    }

    // The fail-closed bare-name DropIndex (§8.6) is refused by the LOAD GATE the
    // loader's IR branch runs — proving the fix is wired into the real entry, not
    // only the validator unit test.
    #[test]
    fn load_and_lower_refuses_bare_name_drop_index() {
        let bytes = r#"{"ir_version":1,"name":"m","ops":[
            {"op":"dropIndex","name":"victim_idx"}
        ]}"#;
        let author = IrAuthor::new("app", "app_intruder", SqlDialect::Postgres);
        let err = author
            .load_and_lower(
                bytes,
                "app_intruder",
                &registry(&[("victim", "app_victim")]),
                &LiveSchema::default(),
            )
            .unwrap_err();
        match err {
            LoadAndLowerError::Load(crate::ir_load::IrLoadError::Validate(ae)) => {
                assert_eq!(ae.code, crate::validate::CODE_UNSUPPORTED);
                assert_eq!(ae.kind, Some(crate::validate::UnsupportedKind::Op));
            }
            other => panic!("expected a fail-closed Load(Validate) reject, got: {other}"),
        }
    }

    // MED (code-critic): the PRODUCTION `.ir.json` deploy entry
    // (`load_and_lower_guarded`, wired into `apply_bundle_ir_migrations`) carries
    // the §6.1.1 op-index attribution on a guard denial — proving the attribution
    // reaches the REAL deploy path, not only the `lower_guarded` unit tests. We
    // force a denial with a guard CONFINED to a DIFFERENT schema, so the rendered
    // `CREATE TABLE "app".…` is a cross-schema construct the guard refuses.
    #[test]
    fn load_and_lower_guarded_denial_carries_op_index_attribution() {
        let bytes = r#"{"ir_version":1,"name":"m","ops":[
            {"op":"createTable","name":"widgets","columns":[{"name":"title","type":"text"}]}
        ]}"#;
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        // Guard confined to "other" — the rendered `"app".…` DDL is a cross-schema
        // reference the Confined guard denies, attributed to op #0.
        let guard_cfg = GuardConfig::confined("other".to_string());
        let err = author
            .load_and_lower_guarded(bytes, "app_a", &registry(&[]), &LiveSchema::default(), &guard_cfg)
            .expect_err("a fragment outside the confined schema must be denied via the wired entry");
        match err {
            LoadAndLowerGuardedError::Lower(IrGuardedLowerError::Denied(d)) => {
                assert_eq!(d.op_index, 0, "the denial attributes to op #0 through the deploy entry");
                assert_eq!(d.op_kind, "createTable");
            }
            other => panic!("expected a per-fragment Denied via the guarded entry, got: {other}"),
        }
    }

    // The guarded deploy entry also reports the artifact's created tables (for the
    // cross-file registry/live-set advance) and lowers a clean createTable.
    #[test]
    fn load_and_lower_guarded_reports_created_tables() {
        let bytes = r#"{"ir_version":1,"name":"m","ops":[
            {"op":"createTable","name":"fresh","columns":[{"name":"title","type":"text"}]}
        ]}"#;
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let guard_cfg = GuardConfig::confined("app".to_string());
        let out = author
            .load_and_lower_guarded(bytes, "app_a", &registry(&[]), &LiveSchema::default(), &guard_cfg)
            .expect("a clean createTable loads + guarded-lowers");
        assert_eq!(out.created_tables, vec!["fresh".to_string()], "the createTable is reported");
        assert!(out.migrations().iter().any(|m| m.up.contains("CREATE TABLE \"app\".\"fresh\"")));
        assert!(!out.fragments.is_empty(), "fragments are attributed");
    }

    // F-MED (code-critic, #92/#93): the drift anchor on the IR path is the
    // DIALECT-NEUTRAL `Checksum::of_ir` over the canonical op list (§5.3 / §2.6.1),
    // NOT the per-statement rendered-SQL `Checksum::of`. `lower_plan` stamps that
    // anchor onto BOTH the AppliedPlan and every `Ddl` step's journaled
    // `Migration.checksum` — so the journal records the op-list anchor and a
    // re-deploy compares against it. This test would FAIL pre-fix (the lowered
    // Migrations carried `Checksum::of(up,down)` — a PG-specific rendered-SQL hash).
    #[test]
    fn ir_plan_anchor_is_of_ir_not_rendered_sql() {
        let ir = create_table_ir("widgets", vec![TIrColumn {
            name: "title".into(),
            ty: ColType::Text,
            nullable: Some(false),
            default: None,
            unique: None,
        }]);
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let plan = author.lower_plan(&ir, &LiveSchema::default()).expect("lower_plan");

        // The authoritative op-list anchor (server-stamped owner already on `ir`).
        let expected = crate::migration::Checksum::of_ir(
            &crate::ir::CanonicalOpList(&ir.ops),
            &crate::migration::MigrationFlags::default(),
            &ir.owner_app,
            &[],
            &[],
            &ir.preconditions,
        );

        // (a) the PLAN checksum is the op-list anchor.
        assert_eq!(
            plan.checksum.as_str(),
            expected.as_str(),
            "the AppliedPlan checksum must be Checksum::of_ir over the op list"
        );

        // (b) EVERY journaled `Ddl` step checksum is the op-list anchor — the value
        //     the journal records + the executor's drift gate compares.
        let mut steps = 0;
        for s in &plan.steps {
            if let PlanStep::Ddl(m) = s {
                steps += 1;
                assert_eq!(
                    m.checksum.as_str(),
                    expected.as_str(),
                    "each Ddl step's journaled checksum must be the op-list anchor, not rendered SQL"
                );
                // It must NOT equal the rendered-SQL `Checksum::of` (the pre-fix value).
                let rendered = crate::migration::Checksum::of(
                    &crate::migration::ChecksumInput::from_migration(m),
                );
                assert_ne!(
                    m.checksum.as_str(),
                    rendered.as_str(),
                    "the journaled anchor must be the dialect-neutral op-list checksum, \
                     NOT the rendered-SQL Checksum::of"
                );
            }
        }
        assert!(steps >= 1, "the createTable lowers to at least one Ddl step");
    }

    // F-MED (#92): the op-list drift anchor is DIALECT-NEUTRAL — the SAME `.ir.json`
    // lowered for PG and for SQLite journals the SAME checksum (so a re-deploy on
    // either backend compares against one anchor; §2.6.1's single-checksum
    // invariant). Pre-fix the anchor was the per-dialect rendered SQL, which
    // DIVERGES (PG `CREATE TABLE app.widgets` vs SQLite `CREATE TABLE "widgets"`).
    #[test]
    fn ir_plan_anchor_is_dialect_neutral_pg_eq_sqlite() {
        let ir = create_table_ir("widgets", vec![TIrColumn {
            name: "title".into(),
            ty: ColType::Text,
            nullable: Some(false),
            default: None,
            unique: None,
        }]);
        let pg = IrAuthor::new("app", "app_a", SqlDialect::Postgres)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("pg lower_plan");
        let sqlite = IrAuthor::new("app", "app_a", SqlDialect::Sqlite)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("sqlite lower_plan");
        assert_eq!(
            pg.checksum.as_str(),
            sqlite.checksum.as_str(),
            "the op-list anchor must be identical across PG and SQLite renders"
        );
        // And the rendered `up` MUST differ (proving the anchor is NOT the SQL).
        let pg_up = match &pg.steps[0] {
            PlanStep::Ddl(m) => m.up.clone(),
            _ => unreachable!(),
        };
        let sqlite_up = match &sqlite.steps[0] {
            PlanStep::Ddl(m) => m.up.clone(),
            _ => unreachable!(),
        };
        assert_ne!(pg_up, sqlite_up, "the rendered SQL DOES diverge per dialect — only the anchor is shared");
    }

    // F-MED (#92): editing the authoring op list (a `.ts` edit) changes the op list
    // ⇒ changes the journaled anchor ⇒ the executor's net-applied drift gate would
    // abort on re-deploy. Two IRs differing only in a column type produce different
    // plan anchors.
    #[test]
    fn ir_plan_anchor_changes_when_op_list_changes() {
        let a = create_table_ir("t", vec![TIrColumn {
            name: "c".into(), ty: ColType::Text, nullable: None, default: None, unique: None,
        }]);
        let b = create_table_ir("t", vec![TIrColumn {
            name: "c".into(), ty: ColType::Int, nullable: None, default: None, unique: None,
        }]);
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let pa = author.lower_plan(&a, &LiveSchema::default()).expect("lower a");
        let pb = author.lower_plan(&b, &LiveSchema::default()).expect("lower b");
        assert_ne!(
            pa.checksum.as_str(),
            pb.checksum.as_str(),
            "a changed op list must move the drift anchor (text vs int column)"
        );
    }

    // An op on ANOTHER app's table is refused by the load gate (ownership) before
    // any lowering happens.
    #[test]
    fn load_and_lower_refuses_cross_tenant_op() {
        let bytes = r#"{"ir_version":1,"name":"m","ops":[
            {"op":"dropColumn","table":"users","column":"x"}
        ]}"#;
        let author = IrAuthor::new("app", "app_intruder", SqlDialect::Postgres);
        let err = author
            .load_and_lower(
                bytes,
                "app_intruder",
                &registry(&[("users", "app_owner")]),
                &LiveSchema::default(),
            )
            .unwrap_err();
        assert!(
            matches!(
                err,
                LoadAndLowerError::Load(crate::ir_load::IrLoadError::NotTableOwner { .. })
            ),
            "got: {err}"
        );
    }
}
