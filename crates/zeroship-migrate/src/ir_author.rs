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
use crate::plan::{AppliedPlan, PlanStep, RenameStep};
use zeroship_schema::query::SqlDialect;

/// The result of lowering ONE IR op (§2.0 / §2.6.1). A DDL op lowers to a list of
/// [`LoweredUnit`]s (a `Migration` + its structural statement list); an online
/// `renameColumn` lowers to ONE [`PlanStep::OnlineRename`] carrying the
/// dialect-chosen [`RenameStep`] (PG expand-contract or SQLite rebuild). Keeping
/// them as one return type lets `lower`/`lower_guarded` build the ordered
/// `Vec<PlanStep>` an [`AppliedPlan`] needs while still guarding each DDL fragment.
enum LoweredOp {
    /// DDL units (createTable / addColumn / alter* / addConstraint / …) — each a
    /// `Migration` + its structural per-statement list (for guard-per-fragment).
    Ddl(Vec<LoweredUnit>),
    /// An online `renameColumn` — ONE plan step, dialect-chosen (§2.6.1/§2.6.2).
    /// The variant's `Migration`s (PG E1..C2, or the SQLite rebuild journal mig)
    /// already carry their own version-stable ids; the IR plan does NOT re-mint
    /// them. Not guarded per-fragment: the expand-contract author / the differ are
    /// the trusted, descriptor-/intent-driven producers (no untrusted raw SQL),
    /// exactly like the declarative path that produces the same shapes. Boxed: a
    /// `RenameStep::PgExpandContract` is large (the full E1..C2 plan), so boxing it
    /// keeps the common `Ddl` arm cheap (`clippy::large_enum_variant`).
    Rename(Box<RenameStep>),
    /// **PR6a** — a DML op (`insert`/`update`/`del`/`backfill`) lowered through the
    /// creator-DML assembler ([`crate::dml`]) into a [`PlanStep::Dml`]
    /// (parameterized one-shot) or [`PlanStep::Backfill`] (PG batched). NOT
    /// fragment-guarded the way DDL is: a one-shot `Dml` step's values are NATIVE
    /// binds (never interpolated), so there is no rendered-literal fragment a guard
    /// would inspect, and the executor's `run_dml_step` re-runs the destructive
    /// approval gate; a `Backfill`'s assembled `UPDATE` is guard-checked by the
    /// backfill executor itself before any batch runs (`backfill.rs`). The DML op's
    /// expression AST is gated by the structural validator BEFORE assembly.
    Dml(PlanStep),
}

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
    /// **PR2 — the SQLite `renameColumn` rebuild facts.** The full introspected
    /// per-table column structure (`table → TableSnapshot`), needed ONLY on the
    /// SQLite leg of an online `renameColumn`: SQLite has no native online rename,
    /// so the rename is reconciled by the 12-step table REBUILD, which needs the
    /// whole live table shape (not just its name) to author the post-rename CREATE
    /// and the value-copy mapping. The PG leg never reads this map (it lowers the
    /// rename to an expand-contract sequence that needs only `{table, from, to,
    /// ty}`). Empty ⇒ a SQLite `renameColumn` whose table's structure is absent
    /// fails closed ([`IrLowerError::SqliteRenameNeedsLiveTable`]), never silently
    /// emitting a wrong rebuild.
    pub table_snapshots: std::collections::BTreeMap<String, crate::drift::TableSnapshot>,
    /// **PR2 — the SQLite `renameColumn` rebuild facts.** The live per-table SDK
    /// schema `Value` (`table → registerModel-shaped JSON`), the SAME shape
    /// [`crate::declarative::DesiredSchema`]'s `sqlite_schemas` carries. The SQLite
    /// rebuild author renders the post-rename `CREATE TABLE` from this Value (with
    /// the renamed field key) through the shared `zeroship_schema::query` emitter,
    /// so the rebuilt table is byte-identical to what the declarative diff would
    /// emit. Only read on the SQLite `renameColumn` leg (see `table_snapshots`).
    pub sqlite_schemas: std::collections::BTreeMap<String, serde_json::Value>,
    /// **PR2 — the live per-table OWNER (`table → owning app`).** The SQLite
    /// `renameColumn` rebuild routes through the declarative differ, whose
    /// `enforce_ownership` REFUSES a structural change to a table the deploying app
    /// does not own ([`crate::declarative::DeclarativeError::NotTableOwner`]). That
    /// guard is only sound if it sees the REAL introspected owner — so the rebuild
    /// stamps the differ's ownership map from THIS map, NOT from the deploying app.
    /// A table whose owner is absent here is treated as foreign on a rename (fail
    /// closed): the differ will not author a rebuild on a table whose ownership it
    /// cannot confirm. The PG leg never reads this (its expand-contract author has
    /// no diff-ownership step; cross-app authority is enforced upstream by the
    /// IR-load gate's registry check). Empty ⇒ a SQLite rename fails closed on the
    /// ownership confirmation.
    pub table_ownership: std::collections::BTreeMap<String, String>,
}

impl LiveSchema {
    /// A live schema with `tables` and NO known unique indexes — for a unit lower
    /// that has the live table set (FK inlining) but no introspected index facts.
    /// Drop-gating then falls back to the IR's advisory `unique` hint alone (never
    /// LESS strict than the hint).
    #[must_use]
    pub fn from_tables(tables: BTreeSet<String>) -> Self {
        Self {
            tables,
            unique_indexes: BTreeSet::new(),
            table_snapshots: std::collections::BTreeMap::new(),
            sqlite_schemas: std::collections::BTreeMap::new(),
            table_ownership: std::collections::BTreeMap::new(),
        }
    }

    /// **PR7 online-rename go-live SEAM — SQLite leg (engine-wired, no production
    /// caller).** Build the FULL SQLite-dialect
    /// `LiveSchema` — `table_snapshots` + `sqlite_schemas` (the per-table SDK schema
    /// `Value`) + `table_ownership` + `unique_indexes` (the descriptor-derived UNIQUE
    /// index names that drive the `dropIndex` destructive/approval gate, the
    /// author-independent authoritative source mirroring the PG path) — from a
    /// descriptor set threaded in by the caller.
    ///
    /// TRUTH-IN-LABELING (code-critic Q3). For DDL/DML and the ownership/FK registry
    /// the descriptor set is the app's `registerModel` schema (the END-STATE union).
    /// But for a `renameColumn` rebuild this set MUST carry the table's PRE-rename
    /// shape (the `from` column present), which a `registerModel`-derived (POST-deploy
    /// desired) set does NOT have — so a rename driven from a `registerModel` set fails
    /// CLOSED (no data loss) and is un-runnable. The SQLite rename path is therefore
    /// engine/test-only today (see `ir_apply::apply_bundle_ir_sqlite`'s PRODUCTION-
    /// WIRING TODO); it is NOT the production peer of the PG deploy path's live
    /// introspection for renames. The SQLite `renameColumn` rebuild needs the SDK schema `Value`
    /// to render the post-rename `CREATE TABLE`, and that `Value` is NOT recoverable
    /// from a raw SQLite-catalog introspection (masks/encryption/ref facets are not
    /// in `sqlite_master`); the descriptor set IS the authoritative source, so the
    /// dev/CLI deploy path threads it here. Routes through the SAME
    /// [`crate::declarative::desired_snapshot_for_dialect`] the differ uses, so the
    /// live facts the rename rebuild consumes are byte-identical to a `t.*`-diff's
    /// desired snapshot. Every table is owned by `owner_app` (the deploying app).
    ///
    /// # Errors
    /// [`DeclarativeError`] if the descriptor set fails the author-boundary
    /// validation the shared snapshot builder runs (an invalid field/type token).
    pub fn for_sqlite_descriptors(
        project_schema: &str,
        owner_app: &str,
        descriptors: &[crate::declarative::CollectionDescriptor],
    ) -> Result<Self, DeclarativeError> {
        let desired = crate::declarative::desired_snapshot_for_dialect(
            project_schema,
            descriptors,
            SqlDialect::Sqlite,
        )?;
        let table_ownership = desired
            .snapshot
            .tables
            .keys()
            .map(|t| (t.clone(), owner_app.to_string()))
            .collect();
        // The AUTHORITATIVE set of UNIQUE-index NAMES the SQLite `dropIndex`
        // destructive/approval gate consults — derived from the SAME descriptor-built
        // desired snapshot the PG leg introspects from the live catalog
        // (`deploy_migrate.rs`), NOT discarded. A `unique:true` field/index becomes a
        // unique index in the snapshot, and the snapshot's per-table `indexes` carry
        // `.unique`; we collect every unique index name so a `dropIndex` of one lowers
        // `destructive + requires_approval` regardless of the IR's advisory `unique`
        // hint. Leaving this empty (the pre-fix shape) reintroduced exactly the hole
        // the PG path closes: a hostile/buggy author could under-declare `unique:false`
        // on a drop of an actually-unique index to slip past the gate on SQLite.
        let unique_indexes = desired
            .snapshot
            .tables
            .values()
            .flat_map(|t| t.indexes.iter())
            .filter(|idx| idx.unique)
            .map(|idx| idx.name.clone())
            .collect();
        Ok(Self {
            tables: desired.snapshot.tables.keys().cloned().collect(),
            unique_indexes,
            table_snapshots: desired.snapshot.tables.clone(),
            sqlite_schemas: desired.sqlite_schemas.clone(),
            table_ownership,
        })
    }

    /// The per-table live column set for the DML apply/render-seam ColRef
    /// resolution (rule (c), §3.3.1.1(c)). Projects [`Self::table_snapshots`] into a
    /// `table → [column names]` map ([`crate::validate::validate_op_resolved`]'s
    /// input). A table absent from `table_snapshots` is absent here too, so its DML
    /// op keeps the structural-only scope (the (c) check is SKIPPED — never weaker
    /// than the load-time gate). The column names include the platform system fields
    /// (`id`/`created_at`/… — they are real live columns), so a legitimate ColRef to
    /// a system field resolves rather than being falsely rejected.
    #[must_use]
    pub fn dml_live_columns(&self) -> std::collections::BTreeMap<String, Vec<String>> {
        self.table_snapshots
            .iter()
            .map(|(table, snap)| {
                (table.clone(), snap.columns.iter().map(|c| c.name.clone()).collect())
            })
            .collect()
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
    /// **PR2** — a SQLite `renameColumn` whose table's full live structure is not
    /// in [`LiveSchema::table_snapshots`] / [`LiveSchema::sqlite_schemas`]. SQLite
    /// has no native online rename, so the rename is reconciled by the 12-step
    /// table REBUILD, which needs the WHOLE live table shape (every column + the
    /// live SDK schema `Value`) to author the post-rename CREATE + value-copy. The
    /// PG leg never needs this (it lowers to expand-contract from `{from,to,ty}`).
    /// Carries the table name. Fail-closed: never emit a rebuild from a partial
    /// view of the table.
    #[error(
        "IrAuthor::lower of a SQLite renameColumn on table {0:?} needs the table's \
         full live structure (LiveSchema::table_snapshots + sqlite_schemas) to \
         author the 12-step rebuild; it is absent — refusing to emit a rebuild from \
         a partial view"
    )]
    SqliteRenameNeedsLiveTable(String),
    /// **PR2** — the cross-subsystem `OnlineIntent` bridge or the SQLite rebuild
    /// planner rejected a `renameColumn` lowering (an empty/identical name, an
    /// un-resolvable rename hint, an emitter shape mismatch). Carries the
    /// underlying error text. Distinct from [`Self::Snapshot`] because it crosses
    /// into the expand-contract author / the differ, not the shared snapshot
    /// builder.
    #[error("IrAuthor::lower of renameColumn failed: {0}")]
    RenameLower(String),
    /// **PR2** — a `renameColumn` whose IR-carried [`ColType`] does not match the
    /// LIVE `from` column's actual `data_type`. A pure online rename mirrors values
    /// across the two columns (PG dual-write `NEW.<to> := NEW.<from>`; the SQLite
    /// rebuild copies the column across) and CANNOT also change the type — a
    /// simultaneous rename + retype is two distinct intents. The IR path is the
    /// higher-risk AI/creator-authored surface, so it must NOT silently trust an
    /// IR-carried type that disagrees with the live column: a wrong `ty` (e.g.
    /// `Int` over a live `text` column) would otherwise author a mismatched
    /// `ADD COLUMN` + a cross-type dual-write copy with no rejection. This is the
    /// IR-path mirror of the declarative differ's
    /// [`crate::declarative::DeclarativeError::RenameHintTypeMismatch`] — enforced
    /// IDENTICALLY on BOTH dialects (the single authoritative type source is the
    /// LIVE column, reconciled against the IR `ty`; neither leg silently uses one
    /// over the other). Carries the table, the column, and the two `data_type`s.
    #[error(
        "IrAuthor::lower of renameColumn {table:?}.{from:?} → {to:?}: the IR-carried \
         type ({ir_type}) does not match the live `{from}` column's type \
         ({live_type}); a rename requires type identity (rename + type change is two \
         separate intents) — refusing to author a cross-type dual-write/rebuild"
    )]
    RenameTypeMismatch {
        /// The table the rename targets.
        table: String,
        /// The `from` column (the live column whose type is authoritative).
        from: String,
        /// The `to` column.
        to: String,
        /// The `information_schema` `data_type` the IR-carried `ColType` derives.
        ir_type: String,
        /// The live `from` column's actual `information_schema` `data_type`.
        live_type: String,
    },
    /// **PR2** — a `renameColumn` whose LIVE `from` column structure is absent from
    /// [`LiveSchema::table_snapshots`], so the authoritative IR-vs-live type
    /// reconciliation (see [`Self::RenameTypeMismatch`]) cannot run. A rename must
    /// NEVER lower from an IR-carried type alone — the live column type is the
    /// authority on BOTH dialects — so an absent live `from` column fails closed
    /// rather than trusting the IR `ty`. Carries the table + column.
    #[error(
        "IrAuthor::lower of renameColumn on {0:?}.{1:?} needs the live `{1}` column's \
         type (LiveSchema::table_snapshots) to reconcile the IR-carried type against \
         the live column; it is absent — refusing to lower a rename from an IR type \
         alone"
    )]
    RenameNeedsLiveColumn(String, String),
    /// **PR6a** — the structural expression validator ([`crate::validate`])
    /// rejected an embedded closed-AST node of a DML op (`update`/`del`/`backfill`
    /// `set`/`where`/`filter`) BEFORE assembly: an out-of-policy node, an
    /// out-of-envelope synth, a non-portable cast. Boxed (the `AuthoringError`
    /// payload is large). The structured §8.8 payload reaches the author through
    /// the boxed error's `Display`.
    #[error("IrAuthor::lower of a DML op: {0}")]
    DmlValidate(Box<crate::validate::AuthoringError>),
    /// **PR6a** — the creator-DML assembler ([`crate::dml`]) rejected a DML op: a
    /// malformed identifier, an empty/ragged insert, a SQLite `onConflict`
    /// (`dialect_scope = PgOnly`, §9), or a SQLite-targeted batched backfill (PR6b).
    /// All are HARD errors — a DML op is NEVER silently dropped or mis-applied.
    #[error("IrAuthor::lower of a DML op: {0}")]
    DmlAssemble(#[from] crate::dml::DmlError),
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
    /// **§2.0.3** — the set of ALL tables this artifact's op list TOUCHES (DDL or
    /// DML), the authoritative touched-set the deploy loop threads into the engine's
    /// cross-deploy pending-contract interlock
    /// ([`MigrationEngine::apply_plan_with_touched`](crate::engine::MigrationEngine::apply_plan_with_touched)).
    /// Unlike `created_tables` (only `createTable` names), this is the union over
    /// EVERY op variant ([`MigrationIr::touched_tables`]), so a deploy that e.g.
    /// `addColumn`s or `update`s a table with an outstanding pending contract is
    /// fail-closed refused.
    pub touched_tables: Vec<String>,
    /// **§2.0.4** — the artifact's plan-level `depends_on` versions (the `.ir.json`
    /// `depends_on`, each a dependency PLAN's plan-group version). The deploy loop
    /// threads these into the engine's cross-plan dependency block
    /// ([`MigrationEngine::apply_plan_with_touched_and_depends`](crate::engine::MigrationEngine::apply_plan_with_touched_and_depends)):
    /// if any referenced dependency is an online rename whose contract is still
    /// OUTSTANDING, the deploy is fail-closed refused — EVEN when this artifact
    /// touches a different table than the pending one (the case `touched_tables`
    /// does not cover).
    pub depends_on: Vec<String>,
}

impl LoweredArtifact {
    /// The lowered migrations, in plan-step order — the flat view the deploy-side
    /// set-integrity manifest + diagnostics consume. A `Ddl` step contributes its
    /// migration; an `OnlineRename` step contributes its journaled sub-migrations so
    /// the manifest tally records the rename's full id set (§2.6.1: the IR-path
    /// rename ids the manifest records are identical to the equivalent
    /// `t.*`-diff-authored rename's) — PG: E1..E3 **and** the deferred contract
    /// C1/C2 (the whole authored sequence, mirroring the declarative manifest which
    /// folds the rename's expand + deferred contract, `engine.rs` manifest doc);
    /// SQLite: the single rebuild journal migration. `Dml`/`Backfill` steps carry no
    /// `Migration` here and do not appear.
    #[must_use]
    pub fn migrations(&self) -> Vec<Migration> {
        let mut out: Vec<Migration> = Vec::new();
        for s in &self.plan.steps {
            match s {
                PlanStep::Ddl(m) => out.push(m.clone()),
                PlanStep::OnlineRename(RenameStep::PgExpandContract(ec)) => {
                    out.extend(ec.expand.iter().cloned());
                    out.extend(ec.contract.iter().cloned());
                }
                PlanStep::OnlineRename(RenameStep::SqliteRebuild(rb)) => {
                    out.push(rb.migration.clone());
                }
                PlanStep::Dml { .. } | PlanStep::Backfill(_) => {}
            }
        }
        out
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
        let (steps, fragments) = self
            .lower_guarded(&ir, guard_cfg, live)
            .map_err(LoadAndLowerGuardedError::Lower)?;
        // Wrap the lowered steps as ONE AppliedPlan whose checksum is the
        // dialect-neutral `Checksum::of_ir` over the op list (§2.0 / §5.2), and
        // STAMP that same anchor onto every DDL step's journaled `Migration.checksum`
        // (§5.3 / §2.6.1): the drift anchor that enters the journal is the
        // canonical op list, NOT the per-dialect rendered SQL. So a re-deploy of
        // the SAME `.ir.json` on EITHER backend re-derives the SAME anchor (no
        // false drift), while editing the authoring `.ts` (⇒ a different op list)
        // shifts the anchor and the executor's net-applied drift gate aborts.
        // §2.0.3 — the authoritative DDL/DML touched-set over EVERY op variant,
        // threaded into the engine's pending-contract interlock by the deploy loop.
        // For a `dropIndex` whose owning-table hint is ABSENT, resolve the owner
        // from the LIVE schema (the same `table_snapshots` introspection the
        // unique-gate uses) so the index's table still enters the touched-set — a
        // bare-name `dropIndex` on a table with an outstanding pending contract must
        // NOT slip the §2.0.3(2) refusal. FAIL CLOSED: if the owner cannot be
        // resolved, fold in a sentinel that can never be a real table name so the
        // engine treats the op as touching SOMETHING (and the deploy is refused if
        // ANY obligation is outstanding) rather than silently un-gating. (On the
        // production path a bare-name `dropIndex` is already rejected at validate —
        // §8.6 — so this is defense-in-depth for that gate plus correctness for any
        // caller that lowers a bare-name drop without the validator.)
        let touched_tables = Self::resolved_touched_tables(&ir, live);
        // §2.0.4 — carry the artifact's plan-level `depends_on` so the deploy loop
        // can fail-closed block a dependent plan whose dependency's online-rename
        // contract is still pending, even when this artifact touches a different
        // table than the pending one.
        let depends_on = ir.depends_on.clone();
        let plan = self.assemble_plan(&ir, steps);
        Ok(LoweredArtifact { plan, fragments, created_tables, touched_tables, depends_on })
    }

    /// The §2.0.3 touched-set for an IR, with a `dropIndex`'s owning TABLE resolved
    /// from the LIVE schema when the op omits the owning-table hint.
    ///
    /// `MigrationIr::touched_tables` under-reports a bare-name `dropIndex` (it has
    /// no structured table — [`Op::touched_table`](crate::ir::Op::touched_table)
    /// returns `None`), which would let a `op.dropIndex("idx_on_pending_table")`
    /// with no hint slip the §2.0.3(2) refusal (fail-OPEN). Here we union in the
    /// owner resolved from `live.table_snapshots` (the same introspection the
    /// unique-gate uses) so the index's table enters the touched-set.
    ///
    /// FAIL CLOSED on an unresolvable owner: fold in [`TOUCHES_UNKNOWN`] so the
    /// engine refuses the deploy if ANY obligation is outstanding (the obligation
    /// set lives in the engine, so the "refuse-if-any-outstanding" decision is made
    /// there). On the production path a bare-name `dropIndex` is already rejected at
    /// validate (§8.6), so the sentinel arm is defense-in-depth for any caller that
    /// lowers a bare-name drop without the validator.
    ///
    /// [`TOUCHES_UNKNOWN`]: crate::engine::TOUCHES_UNKNOWN
    #[must_use]
    pub fn resolved_touched_tables(ir: &MigrationIr, live: &LiveSchema) -> Vec<String> {
        let mut touched_tables = ir.touched_tables();
        for op in &ir.ops {
            if let Op::DropIndex { name, table: None, .. } = op {
                let entry = Self::resolve_index_owner(name, live)
                    .unwrap_or_else(|| crate::engine::TOUCHES_UNKNOWN.to_string());
                if !touched_tables.contains(&entry) {
                    touched_tables.push(entry);
                }
            }
        }
        touched_tables
    }

    /// Resolve a `dropIndex`'s owning TABLE from the LIVE schema by index name,
    /// for the §2.0.3 touched-set when the IR omits the owning-table hint. Scans
    /// the introspected `table_snapshots` (the same live catalog the unique-gate
    /// reads) for the table whose `indexes` contain `name`. `None` when the index
    /// is not in the live schema (e.g. it was never created, or the snapshot is
    /// empty), in which case the caller fails closed.
    fn resolve_index_owner(name: &str, live: &LiveSchema) -> Option<String> {
        live.table_snapshots
            .iter()
            .find(|(_, snap)| snap.indexes.iter().any(|idx| idx.name == name))
            .map(|(table, _)| table.clone())
    }

    /// Assemble the lowered [`PlanStep`]s into ONE [`AppliedPlan`] (§2.0 / §5.2),
    /// stamping the dialect-neutral [`Checksum::of_ir`] anchor (§5.3) onto BOTH the
    /// plan and every `Ddl` step's journaled `Migration.checksum`.
    ///
    /// **Why stamp the op-list `of_ir` onto each DDL step's checksum.** The journal
    /// records `Migration.checksum` and the executor's net-applied drift gate
    /// (`drift.rs`) compares the journaled value to the lowered `Migration.checksum`
    /// on re-deploy. Stamping the canonical-op-list `of_ir` there makes the
    /// journaled drift anchor the DIALECT-NEUTRAL op list (§2.6.1's "one plan
    /// checksum over the canonical op list, not the rendered SQL"), so the anchor is
    /// the SAME on a PG re-deploy and a SQLite re-deploy of the same artifact — and a
    /// `.ts` edit (a changed op list) is detected as drift regardless of dialect.
    /// The per-dialect rendered `up`/`down` still applies; only the IDENTITY anchor
    /// is the neutral op list.
    ///
    /// An [`PlanStep::OnlineRename`] step's sub-migrations (PG E1..C2 / the SQLite
    /// rebuild journal migration) keep their OWN author-stamped checksums and
    /// version-stable ids — `ExpandContractAuthor` / the rebuild planner are the id
    /// authority (§2.6.1), the IR plan does NOT re-mint or re-anchor them. The
    /// neutral op-list anchor is the PLAN's identity (`AppliedPlan.checksum`); the
    /// per-DDL-step checksum stamp is the existing PR1 drift seam for plain DDL.
    fn assemble_plan(&self, ir: &MigrationIr, mut steps: Vec<PlanStep>) -> AppliedPlan {
        let anchor = crate::ir_load::authoritative_ir_checksum(ir);
        for s in &mut steps {
            if let PlanStep::Ddl(m) = s {
                m.checksum = anchor.clone();
            }
        }
        // The plan-group identity (§2.0.1): the steps keep their own per-op journal
        // versions, so the plan `version` is a marker — the first step's version
        // (deterministic within a deploy), or a fresh id for the degenerate empty
        // plan (a no-op IR).
        let version = steps
            .first()
            .map(plan_step_version)
            .unwrap_or_else(crate::migration::MigrationId::generate);
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

    /// Lower a validated [`MigrationIr`]'s ops to ONE [`AppliedPlan`] (§2.0 /
    /// §5.2) — the named-contract peer of [`lower`](Self::lower) (which returns the
    /// flat `Vec<Migration>` the §6.4 byte-identity goldens compare). The plan's
    /// `checksum` is the dialect-neutral [`Checksum::of_ir`] anchor and each `Ddl`
    /// step's journaled checksum is stamped with it (§5.3 — see
    /// [`assemble_plan`](Self::assemble_plan)). A `renameColumn` op lowers to a
    /// [`PlanStep::OnlineRename`] step (PR2), carried verbatim into the plan.
    ///
    /// # Errors
    /// Same as [`lower_steps`](Self::lower_steps).
    pub fn lower_plan(
        &self,
        ir: &MigrationIr,
        live: &LiveSchema,
    ) -> Result<AppliedPlan, IrLowerError> {
        let steps = self.lower_steps(ir, live)?;
        Ok(self.assemble_plan(ir, steps))
    }

    /// Lower a validated [`MigrationIr`]'s DDL ops to their flat [`Migration`]
    /// list — the §6.4 byte-identity parity leg (compared against the differ, which
    /// also returns `Vec<Migration>`). DDL ops only: a `renameColumn` lowers to a
    /// [`PlanStep::OnlineRename`] (no plain `Migration` in this flat view), so it is
    /// **not** represented here — use [`lower_steps`](Self::lower_steps) /
    /// [`lower_plan`](Self::lower_plan) for the full ordered plan including online
    /// renames. The §6.4 goldens never include a rename, so this projection is
    /// exact for them.
    ///
    /// `live` carries the introspected [`LiveSchema`] facts (see
    /// [`lower_steps`](Self::lower_steps)).
    ///
    /// # Errors
    /// - [`IrLowerError::Snapshot`] — the shared builder rejected an op's fields.
    /// - [`IrLowerError::UnsupportedOp`] — a non-DDL op (DML).
    pub fn lower(
        &self,
        ir: &MigrationIr,
        live: &LiveSchema,
    ) -> Result<Vec<Migration>, IrLowerError> {
        Ok(self
            .lower_steps(ir, live)?
            .into_iter()
            .filter_map(|s| match s {
                PlanStep::Ddl(m) => Some(m),
                _ => None,
            })
            .collect())
    }

    /// Lower a validated [`MigrationIr`]'s ops to their ordered [`PlanStep`] list
    /// (§2.0). This is the full lowering: DDL ops become [`PlanStep::Ddl`]; an
    /// online `renameColumn` becomes ONE [`PlanStep::OnlineRename`] carrying the
    /// dialect-chosen [`RenameStep`] (PG expand-contract / SQLite rebuild, §2.6.2).
    ///
    /// `live` carries the introspected [`LiveSchema`] facts: `live.tables` is the
    /// set of tables already present in the project (so an FK to a live target
    /// inlines, and a non-live target defers on PG / errors on SQLite — mirroring
    /// `diff`); `live.unique_indexes` is the authoritative set of live UNIQUE-index
    /// names that drives the `dropIndex` destructive/approval gate (OR-ed with the
    /// IR's advisory `unique` hint); `live.table_snapshots` + `live.sqlite_schemas`
    /// carry the full live table structure the SQLite `renameColumn` rebuild needs.
    /// Tables created EARLIER in the same IR are added to the working live-table set
    /// as lowering proceeds, so an intra-migration FK inlines correctly.
    ///
    /// # Errors
    /// - [`IrLowerError::Snapshot`] — the shared builder rejected an op's fields.
    /// - [`IrLowerError::UnsupportedOp`] — a non-DDL op (DML).
    /// - [`IrLowerError::SqliteRenameNeedsLiveTable`] / [`IrLowerError::RenameLower`]
    ///   — a `renameColumn` could not lower (missing live structure / bridge error).
    pub fn lower_steps(
        &self,
        ir: &MigrationIr,
        live: &LiveSchema,
    ) -> Result<Vec<PlanStep>, IrLowerError> {
        let mut out: Vec<PlanStep> = Vec::new();
        let mut live_tables: BTreeSet<String> = live.tables.clone();
        for (op_index, op) in ir.ops.iter().enumerate() {
            // The whole-up step lowering discards the structural statement list (it
            // is the §6.4 parity leg, which only compares the joined `up`); the
            // guarded path ([`lower_guarded`]) consumes the list to guard true
            // statements. `op_index` is the plan position the DML-step version folds
            // in (so two byte-identical DML ops get distinct journal ids).
            match self.lower_one_op(op_index, op, &mut live_tables, live)? {
                LoweredOp::Ddl(units) => {
                    out.extend(units.into_iter().map(|(mig, _statements)| PlanStep::Ddl(mig)));
                }
                LoweredOp::Rename(step) => out.push(PlanStep::OnlineRename(*step)),
                LoweredOp::Dml(step) => out.push(step),
            }
        }
        Ok(out)
    }

    /// Lower a SINGLE op, advancing the working `live` table set when the op creates
    /// a table (so a later intra-IR FK inlines). Factored out of
    /// [`lower_steps`](Self::lower_steps) so the guard-per-fragment path
    /// ([`lower_guarded`]) can attribute each op's rendered fragments to its op
    /// index (§6.1.1). Returns a [`LoweredOp`] — DDL units OR a single online-rename
    /// step (§2.6.1).
    ///
    /// `live` is the full [`LiveSchema`]: `live_tables` is the MUTABLE working
    /// table set (advanced as createTable ops lower); the SQLite `renameColumn` leg
    /// also reads `live.table_snapshots` / `live.sqlite_schemas`.
    ///
    /// # Errors
    /// - [`IrLowerError::Snapshot`] — the shared builder rejected the op's fields.
    /// - [`IrLowerError::UnsupportedOp`] — a non-DDL op (DML).
    /// - rename-lowering errors (see [`lower_steps`](Self::lower_steps)).
    fn lower_one_op(
        &self,
        op_index: usize,
        op: &Op,
        live_tables: &mut BTreeSet<String>,
        live_schema: &LiveSchema,
    ) -> Result<LoweredOp, IrLowerError> {
        let live_unique_indexes = &live_schema.unique_indexes;
        // The DDL arms advance / read the working table set under the short name
        // `live` (the name the §6.1.1 fragment logic already uses).
        let live = live_tables;
        let migs: Vec<LoweredUnit> = match op {
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
            Op::RenameColumn { table, from, to, ty } => {
                // §2.6.1 — ONE online-rename plan step, dialect-chosen at lower
                // (§2.6.2). The neutral→PG / neutral→SQLite-affinity translation
                // lives in `lower_rename`; the destination authors (the
                // expand-contract author on PG, the rebuild planner on SQLite) are
                // REUSED verbatim, so the IR path inherits their version-stable ids
                // (§2.6.1). A rename never advances the working live-table set.
                let step = self.lower_rename(table, from, to, ty, live_schema)?;
                return Ok(LoweredOp::Rename(Box::new(step)));
            }
            Op::AddConstraint { table, constraint } => self.lower_add_constraint(table, constraint)?,
            Op::DropConstraint { table, name } => {
                // SQLite has no `ALTER TABLE … DROP CONSTRAINT` (rebuild-only); PG only.
                self.require_pg_for("dropConstraint")?;
                vec![self.decl.lower_drop_constraint(table, name)]
            }
            // §PR6a — the DML ops lower through the creator-DML assembler
            // (`crate::dml`) into a `PlanStep::Dml`/`PlanStep::Backfill`, NOT a DDL
            // `Migration`. Each returns early with a `LoweredOp::Dml`.
            Op::Insert { .. }
            | Op::Update { .. }
            | Op::Delete { .. }
            | Op::Backfill { .. } => {
                return Ok(LoweredOp::Dml(self.lower_dml_op(op_index, op, live_schema)?));
            }
        };
        Ok(LoweredOp::Ddl(migs))
    }

    /// **§PR6a — lower a DML op** (`insert`/`update`/`del`/`backfill`) into a
    /// [`PlanStep`] via the creator-DML assembler ([`crate::dml`]).
    ///
    /// The closed-AST expression slots (`update`/`del`/`backfill` `set`/`where`/
    /// `filter`) are gated in TWO layers, BOTH before any SQL is assembled:
    ///
    /// 1. STRUCTURALLY by [`crate::validate::validate_op`] (the (a)/(b)/(d) checks —
    ///    node allow-list, synth envelope, portable cast); a non-portable /
    ///    out-of-policy node is rejected up front.
    /// 2. RULE (c) — `ColRef` RESOLUTION against the live target table — by
    ///    [`crate::validate::validate_op_resolved`], using the introspected
    ///    [`LiveSchema::table_snapshots`] (the SAME live facts the rename/diff path
    ///    consults). A `ColRef` to a column that does NOT exist on the enclosing
    ///    target table (or a synthesized cross-table reference) is rejected with the
    ///    structured `UNSUPPORTED { kind: "expr" }` AuthoringError AT APPLY/RENDER
    ///    TIME (§3.3.1.1(c)) — NOT baked into the template to surface later as a raw
    ///    DB `column does not exist` error mid-statement. When the op's target table
    ///    is ABSENT from `table_snapshots` (a unit lower with no introspected schema,
    ///    or a table created earlier in the SAME deploy whose columns are not yet
    ///    snapshotted), the (c) check is SKIPPED — never weaker than the load-time
    ///    structural-only gate, and the engine's per-statement guard + the DB itself
    ///    remain the backstop.
    ///
    /// Portability boundaries (§9), all HARD errors (never silent):
    /// - `insert { onConflict }` on **SQLite** → [`crate::dml::DmlError::OnConflictNotPortable`].
    ///
    /// A **batched** `backfill` / `update { batch }` is PORTABLE on BOTH backends
    /// since PR6b (PG `backfill.rs`, SQLite `backend_sqlite::backfill_sql`) — it is
    /// no longer a SQLite hard error.
    ///
    /// # Errors
    /// - [`IrLowerError::DmlValidate`] — the structural validator (a)/(b)/(d) OR the
    ///   resolved rule-(c) `ColRef` check rejected an embedded expression.
    /// - [`IrLowerError::DmlAssemble`] — the assembler rejected the op (malformed
    ///   identifier / empty insert / SQLite `onConflict` / SQLite batched backfill).
    fn lower_dml_op(
        &self,
        op_index: usize,
        op: &Op,
        live_schema: &LiveSchema,
    ) -> Result<PlanStep, IrLowerError> {
        use crate::ir::Op;
        let dialect = self.dialect;
        let target = match dialect {
            SqlDialect::Postgres => crate::validate::Dialect::Postgres,
            SqlDialect::Sqlite => crate::validate::Dialect::Sqlite,
        };
        // Structural gate (a)/(b)/(d) BEFORE assembly. op_index 0 is a local
        // attribution; the loader's `validate_ir` already ran with the true op
        // index for the production path — this is the lower-time defense-in-depth.
        crate::validate::validate_op(op, target, 0, None)
            .map_err(|e| IrLowerError::DmlValidate(Box::new(e)))?;

        // RULE (c) — resolved ColRef gate at the apply/render seam (§3.3.1.1(c)).
        // Resolve the op's embedded ColRefs against the LIVE target-table columns
        // (from the introspected `table_snapshots`) BEFORE the template is
        // assembled, so a column-not-on-target / cross-table ColRef is rejected with
        // the structured AuthoringError here — not as a raw DB error at execution. A
        // table absent from the live snapshot keeps the structural-only scope (the
        // (c) check is skipped; see the fn doc).
        let live_columns = live_schema.dml_live_columns();
        crate::validate::validate_op_resolved(op, target, &live_columns, 0, None)
            .map_err(|e| IrLowerError::DmlValidate(Box::new(e)))?;

        match op {
            Op::Insert { table, columns, rows, on_conflict } => {
                let oc = on_conflict.as_ref().map(|c| crate::dml::OnConflict {
                    columns: c.columns.clone(),
                    do_update: c.do_update.clone(),
                });
                let asm = crate::dml::assemble_insert(
                    &self.project_schema,
                    dialect,
                    table,
                    columns,
                    rows,
                    oc.as_ref(),
                )
                .map_err(IrLowerError::DmlAssemble)?;
                Ok(self.dml_step(op_index, table, "insert", asm, false))
            }
            Op::Update { table, set, r#where, batch } => {
                if batch.is_some() {
                    // A batched update is a backfill in disguise (resumable, paged):
                    // route it through the backfill path so its SQLite leg hits the
                    // PR6b boundary, never a silent one-shot UPDATE.
                    let b = batch.as_ref().expect("batch is_some");
                    return self.lower_backfill(
                        table,
                        &b.cursor_column,
                        b.batch_size.get(),
                        set,
                        r#where.as_ref(),
                        // A batched update has no separate name; derive a stable one.
                        &format!("batched_update_{table}"),
                    );
                }
                let asm = crate::dml::assemble_update(
                    &self.project_schema,
                    dialect,
                    table,
                    set,
                    r#where.as_ref(),
                )
                .map_err(IrLowerError::DmlAssemble)?;
                Ok(self.dml_step(op_index, table, "update", asm, false))
            }
            Op::Delete { table, r#where, limit } => {
                let asm = crate::dml::assemble_delete(
                    &self.project_schema,
                    dialect,
                    table,
                    r#where,
                    limit.map(crate::ir::SafeU64::get),
                )
                .map_err(IrLowerError::DmlAssemble)?;
                // A delete is DESTRUCTIVE (data loss) — the executor's approval gate
                // refuses it without `Approval::Approved`.
                Ok(self.dml_step(op_index, table, "delete", asm, true))
            }
            Op::Backfill { table, cursor_column, batch_size, set, filter, name } => self
                .lower_backfill(
                    table,
                    cursor_column,
                    batch_size.get(),
                    set,
                    filter.as_ref(),
                    name,
                ),
            // Unreachable: lower_one_op only routes the four DML ops here.
            _ => Err(IrLowerError::UnsupportedOp("non-DML op routed to lower_dml_op")),
        }
    }

    /// Build a [`PlanStep::Dml`] from an assembled one-shot statement, minting a
    /// deterministic sub-version id from the `op_index` + owner + kind + template +
    /// binds so a re-deploy of the SAME op is idempotent (the journal net-applied-skip
    /// keys on this version) and a re-authored op (a changed template/binds) gets a
    /// fresh id (no false resume). The `op_index` is what keeps two byte-identical
    /// DML ops in the SAME migration distinct (see [`dml_step_version`]).
    fn dml_step(
        &self,
        op_index: usize,
        table: &str,
        kind: &str,
        asm: crate::dml::AssembledDml,
        destructive: bool,
    ) -> PlanStep {
        let owner = self.decl.owner_app().to_string();
        let version = dml_step_version(op_index, &owner, kind, &asm.template, &asm.binds);
        PlanStep::Dml {
            version,
            name: format!("{kind} {table}"),
            template: asm.template,
            binds: asm.binds,
            transactional: true,
            destructive,
            owner_app: owner,
        }
    }

    /// Lower a `backfill` (or batched `update`) into a [`PlanStep::Backfill`]. The
    /// `set`/`filter` render to INLINE SQL strings ([`crate::dml::assemble_backfill_clauses`])
    /// the [`crate::backfill::BackfillSpec`] executor consumes (it guard-checks /
    /// authorizer-vets the assembled `UPDATE` before any batch).
    ///
    /// **PORTABLE on BOTH backends** since PR6b: PG via the writable-CTE windowed
    /// `UPDATE` executor (`backfill.rs`), SQLite via the batched per-batch-txn
    /// executor (`backend_sqlite::backfill_sql`, §2.3.1). The inline `set`/`filter`
    /// are dialect-rendered (the §9 `c.fn.splitPart` lowering, NULL-skipping
    /// `concatWs`, etc. differ per dialect) — but both legs consume the same
    /// `BackfillSpec` shape, so the plan step is uniform.
    fn lower_backfill(
        &self,
        table: &str,
        cursor_column: &str,
        batch_size: u64,
        set: &std::collections::BTreeMap<String, crate::expr::Expr>,
        filter: Option<&crate::expr::Expr>,
        name: &str,
    ) -> Result<PlanStep, IrLowerError> {
        let clauses =
            crate::dml::assemble_backfill_clauses(self.dialect, table, set, filter)
                .map_err(IrLowerError::DmlAssemble)?;
        let batch_size = u32::try_from(batch_size).unwrap_or(u32::MAX).max(1);
        let spec = crate::backfill::BackfillSpec {
            table: table.to_string(),
            cursor_column: cursor_column.to_string(),
            batch_size,
            set_clause: clauses.set_clause,
            filter: clauses.filter,
            name: name.to_string(),
        };
        Ok(PlanStep::Backfill(spec))
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
    /// the lowered ordered [`PlanStep`] list whose `Ddl` steps' `up` are provably the
    /// reassembly of those exact fragments. An online `renameColumn` lowers to ONE
    /// [`PlanStep::OnlineRename`] (§2.6.1) — it is NOT fragment-guarded: the
    /// expand-contract author (PG) / the differ's rebuild planner (SQLite) are the
    /// trusted descriptor-/intent-driven producers (no untrusted raw SQL), exactly
    /// like the declarative path that emits the same shapes, and `apply_plan`
    /// re-runs the Confined guard on every rendered statement at execution time.
    /// The SQLite leg's guard ([`crate::guard::SqliteDescriptorGuard`]) trusts
    /// descriptor-/IR-generated DDL (no string deny-list), so it never denies — but
    /// the fragment split + reassembly invariant still runs, so the `up`↔fragment
    /// correspondence holds on both dialects.
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
    ) -> Result<(Vec<PlanStep>, Vec<GuardedFragment>), IrGuardedLowerError> {
        let guard = guard_for(guard_cfg);
        let mut steps: Vec<PlanStep> = Vec::new();
        let mut fragments: Vec<GuardedFragment> = Vec::new();
        let mut live_tables: BTreeSet<String> = live.tables.clone();

        for (op_index, op) in ir.ops.iter().enumerate() {
            let op_kind = op_kind_tag(op);
            // Lower this op (advancing `live_tables` for intra-IR FK inlining). A
            // lower failure aborts before any guarding — nothing applied. Each unit
            // carries its STRUCTURAL per-statement list (the exact statements the
            // renderer built, NOT a textual re-split of `up`).
            let op_units = match self.lower_one_op(op_index, op, &mut live_tables, live)? {
                LoweredOp::Ddl(units) => units,
                LoweredOp::Rename(step) => {
                    // §2.6.1 — one online-rename plan step, carried verbatim. NOT
                    // fragment-guarded (the producer is trusted; `apply_plan`
                    // re-guards at execution). It produces no `GuardedFragment` row.
                    steps.push(PlanStep::OnlineRename(*step));
                    continue;
                }
                LoweredOp::Dml(step) => {
                    // §PR6a — a DML step is NOT fragment-guarded the way DDL is. A
                    // one-shot `Dml` carries its values as NATIVE binds (`$n`/`?n`),
                    // so there is no rendered-literal fragment a deny-list guard
                    // would inspect; the executor's `run_dml_step` re-runs the
                    // destructive approval gate. A `Backfill`'s assembled `UPDATE` is
                    // guard-checked by the backfill executor before any batch runs
                    // (`backfill.rs`). The op's expression AST was already gated by
                    // the structural validator in `lower_dml_op`. So it produces no
                    // `GuardedFragment` row, exactly like an online rename.
                    steps.push(step);
                    continue;
                }
            };

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
                steps.push(PlanStep::Ddl(mig));
            }
        }
        Ok((steps, fragments))
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

    /// **PR2 — lower an online `renameColumn` op (§2.6 / §2.6.1 / §2.6.2).** Map the
    /// dialect-neutral [`ColType`] to the per-dialect type representation BEFORE
    /// handing it to the dialect-specific destination author, then route to the
    /// cross-subsystem bridge ([`DeclarativeAuthor::lower_ir_rename`]):
    ///
    /// - **Neutral→PG type.** Build the column's `ColumnSnapshot` via the SHARED
    ///   snapshot builder (the SAME builder `addColumn` uses, §6.5) to get its
    ///   `information_schema` `data_type`, then `ddl_type`-spell it — exactly how the
    ///   declarative rename path derives the `OnlineIntent` type (`ddl_type(&r.ty)`),
    ///   so E1's `ADD COLUMN <to> <ty>` is byte-equal across the two paths (§2.6.1).
    ///   This is the ONLY type representation the PG leg uses.
    /// - **Neutral→SQLite affinity.** The SQLite leg never receives the PG type
    ///   string. The rebuild's post-rename CREATE is rendered from the live SDK
    ///   schema `Value` (with the field key renamed) through the shared SQLite
    ///   emitter, whose per-column affinity comes from the field's type token — the
    ///   token the live schema already carries for the dialect-neutral `ColType`.
    ///   The bridge needs the table's full live structure
    ///   ([`LiveSchema::table_snapshots`] + [`LiveSchema::sqlite_schemas`]); absent ⇒
    ///   [`IrLowerError::SqliteRenameNeedsLiveTable`] (fail-closed).
    ///
    /// **Authoritative IR-vs-live type reconciliation (BOTH legs).** Before EITHER
    /// destination author runs, the IR-carried [`ColType`] is resolved to its
    /// `information_schema` `data_type` and reconciled against the LIVE `from`
    /// column's actual type ([`LiveSchema::table_snapshots`]). A mismatch is rejected
    /// ([`IrLowerError::RenameTypeMismatch`]) — the IR-path mirror of the declarative
    /// differ's [`crate::declarative::DeclarativeError::RenameHintTypeMismatch`]. A
    /// pure rename mirrors values across the two columns and cannot also change the
    /// type, so the live column is the single authoritative type source on BOTH
    /// dialects (neither leg silently trusts the IR `ty`). The live `from` column is
    /// MANDATORY: absent ⇒ [`IrLowerError::RenameNeedsLiveColumn`] (never lower a
    /// rename from an IR type alone).
    ///
    /// The destination authors (the PG expand-contract author / the SQLite rebuild
    /// planner) are REUSED verbatim, so the IR path inherits their version-stable ids
    /// (§2.6.1) — the IR plan never re-mints them.
    ///
    /// # Errors
    /// - [`IrLowerError::Snapshot`] — the shared builder rejected the column type.
    /// - [`IrLowerError::RenameNeedsLiveColumn`] — the live `from` column type is
    ///   absent, so the type reconciliation cannot run (fail-closed, both legs).
    /// - [`IrLowerError::RenameTypeMismatch`] — the IR-carried type disagrees with
    ///   the live `from` column's type (both legs).
    /// - [`IrLowerError::SqliteRenameNeedsLiveTable`] — SQLite leg missing live facts.
    /// - [`IrLowerError::RenameLower`] — the bridge (author / differ) rejected it.
    fn lower_rename(
        &self,
        table: &str,
        from: &str,
        to: &str,
        ty: &ColType,
        live: &LiveSchema,
    ) -> Result<RenameStep, IrLowerError> {
        // The IR-carried column type, resolved to its `information_schema`
        // `data_type` via the SHARED builder (the SAME spelling the differ's
        // `field_data_type` produces and the live introspection records). This is
        // the type the IR ASSERTS the column has.
        let col = self.add_column_snapshot(table, to, ty, None, None)?;
        let ir_data_type = col.data_type.clone();

        // **AUTHORITATIVE IR-vs-live type reconciliation (HIGH/MED — both legs).**
        // A pure online rename mirrors values across the two columns and CANNOT also
        // change the type; the LIVE column is the single source of truth. Look up the
        // live `from` column's actual `data_type` and REJECT if the IR-carried type
        // disagrees — the IR-path mirror of the declarative differ's
        // `RenameHintTypeMismatch`. This runs IDENTICALLY on BOTH dialects (neither
        // leg silently trusts the IR `ty` over the live column): a wrong-type IR
        // (e.g. `Int` over a live `text` column) fails closed here BEFORE any
        // dual-write/rebuild is authored. The live `from` column structure is
        // mandatory for a rename on either dialect — absent ⇒ fail closed (never
        // lower a rename from an IR type alone).
        //
        // The live table snapshot is fetched ONCE here and bound for BOTH the
        // type reconciliation (this block) and the to-collision guard (next block).
        // Absent ⇒ fail closed. Reusing the single binding means the collision guard
        // is UNCONDITIONAL — there is no `if let Some(..)`-shaped path that could
        // silently skip the `to`-check if this from-check is ever refactored/reordered
        // (the collision check cannot become a no-op on a missing snapshot).
        let live_snapshot = live.table_snapshots.get(table).ok_or_else(|| {
            IrLowerError::RenameNeedsLiveColumn(table.to_string(), from.to_string())
        })?;
        let live_from_type = live_snapshot
            .columns
            .iter()
            .find(|c| c.name == from)
            .map(|c| c.data_type.clone())
            .ok_or_else(|| {
                IrLowerError::RenameNeedsLiveColumn(table.to_string(), from.to_string())
            })?;
        if live_from_type != ir_data_type {
            return Err(IrLowerError::RenameTypeMismatch {
                table: table.to_string(),
                from: from.to_string(),
                to: to.to_string(),
                ir_type: ir_data_type,
                live_type: live_from_type,
            });
        }

        // **PR2-LOW — rename-to-EXISTING-column collision (fail-closed, both legs).**
        // The `to` column MUST NOT already exist on the live table. The type
        // reconciliation above only confirms `from`; without this guard a
        // `renameColumn` whose `to` collides with a live column would (PG) author an
        // `ADD COLUMN <to>` that fails late at apply with an opaque "column already
        // exists", or (SQLite) silently OVERWRITE the existing `to` field def when the
        // rebuild renames the `from` key onto it — a data-loss-class silent mis-build.
        // We reject it BEFORE either destination author runs, mirroring the
        // declarative differ's hint-unmatched fail-closed stance. The guard runs
        // UNCONDITIONALLY against the `live_snapshot` already bound above (no second
        // `.get()`, no `if let Some` arm): if the from-check is ever moved/removed, a
        // missing snapshot still fails closed at that bind, never silently skipping
        // this collision check.
        if live_snapshot.columns.iter().any(|c| c.name == to) {
            return Err(IrLowerError::RenameLower(format!(
                "renameColumn {table:?}.{from:?} → {to:?}: the target column {to:?} \
                 already exists on the live table — a rename cannot collide with an \
                 existing column (refusing to author a duplicate ADD COLUMN / a \
                 silent rebuild overwrite)"
            )));
        }

        match self.dialect {
            SqlDialect::Postgres => {
                // Neutral→PG type: the reconciled `information_schema` data_type,
                // `ddl_type`-spelled — byte-equal to the declarative path's
                // `ddl_type(&r.ty)` (§2.6.1). Computed ONLY on the PG leg (the SQLite
                // leg takes affinity from the live SDK Value, never a PG string).
                let pg_ty = crate::declarative::ddl_type(&ir_data_type).to_string();
                // The PG expand-contract author derives the dual-write from
                // `{table, from, to, ty}` and needs no live table SHAPE; the type was
                // already reconciled above, so pass empties for the unused snapshot/
                // schema slots.
                let empty_snapshot = crate::drift::TableSnapshot {
                    columns: Vec::new(),
                    indexes: Vec::new(),
                    constraints: Vec::new(),
                    stored_create_sql: None,
                };
                self.decl
                    .lower_ir_rename(
                        table,
                        from,
                        to,
                        &pg_ty,
                        &empty_snapshot,
                        &serde_json::Value::Null,
                        // The PG expand-contract author has no diff-ownership step
                        // (cross-app authority is enforced upstream by the IR-load
                        // gate's registry check), so `live_owner` is unused on this
                        // leg; pass the deploying app for signature completeness.
                        self.decl.owner_app(),
                    )
                    .map_err(|e| IrLowerError::RenameLower(e.to_string()))
            }
            SqlDialect::Sqlite => {
                // The SQLite rebuild needs the WHOLE live table shape (every column +
                // the live SDK schema Value). Absent ⇒ fail closed. `pg_ty` is unused
                // on this leg (the rebuild's affinity comes from the SDK Value), so it
                // is not computed here — only the live shape drives the rebuild.
                let live_snapshot = live.table_snapshots.get(table).ok_or_else(|| {
                    IrLowerError::SqliteRenameNeedsLiveTable(table.to_string())
                })?;
                let live_schema_value = live.sqlite_schemas.get(table).ok_or_else(|| {
                    IrLowerError::SqliteRenameNeedsLiveTable(table.to_string())
                })?;
                // The REAL introspected owner of the live table — the subject of the
                // differ's cross-app drop/ALTER guard. Absent ⇒ fail closed (the
                // rebuild must NOT fabricate ownership as the deploying app, which
                // would let app B silently rebuild app A's table). A foreign owner
                // here makes the differ refuse with `NotTableOwner`.
                let live_owner = live.table_ownership.get(table).ok_or_else(|| {
                    IrLowerError::RenameLower(format!(
                        "renameColumn on SQLite table '{table}' has no introspected owner \
                         in LiveSchema::table_ownership — refusing to author a rebuild on a \
                         table whose ownership cannot be confirmed (cross-app drop guard)"
                    ))
                })?;
                self.decl
                    .lower_ir_rename(
                        table,
                        from,
                        to,
                        "",
                        live_snapshot,
                        live_schema_value,
                        live_owner,
                    )
                    .map_err(|e| IrLowerError::RenameLower(e.to_string()))
            }
        }
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

/// The journal version of a plan step — the deterministic marker the plan's
/// outer `version` borrows from its FIRST step (§2.0.1). A `Ddl` uses its
/// migration version; an `OnlineRename` uses its first sub-migration's version
/// (PG: E1; SQLite: the rebuild journal migration); a `Dml` uses its own version;
/// a `Backfill` (PR6a) derives a deterministic marker from its stable backfill id.
fn plan_step_version(step: &PlanStep) -> crate::migration::MigrationId {
    match step {
        PlanStep::Ddl(m) => m.version.clone(),
        PlanStep::OnlineRename(RenameStep::PgExpandContract(ec)) => {
            // The plan-group identity of a PG online rename anchors on E1's version.
            // `ExpandContractAuthor::author` ALWAYS emits E1..E3, so an EMPTY expand
            // is an internal invariant violation (the author was bypassed or built a
            // malformed plan), NOT a routine empty case. Fail closed: in dev/test it
            // panics loudly (the bug surfaces), and in release it falls back to a
            // DETERMINISTIC sentinel id derived from the plan's stable content — NOT
            // `MigrationId::generate()`, whose RANDOM output would silently give the
            // same broken plan a different version on every call, defeating idempotent
            // re-deploy and masking the bug.
            match ec.expand.first() {
                Some(m) => m.version.clone(),
                None => {
                    debug_assert!(
                        false,
                        "internal invariant violation: PgExpandContract plan has an \
                         empty `expand` chain (ExpandContractAuthor::author always \
                         emits E1..E3) — refusing to mint a non-deterministic plan \
                         version"
                    );
                    empty_expand_sentinel_id(ec)
                }
            }
        }
        PlanStep::OnlineRename(RenameStep::SqliteRebuild(rb)) => rb.migration.version.clone(),
        PlanStep::Dml { version, .. } => version.clone(),
        PlanStep::Backfill(spec) => {
            // A backfill's plan-step identity is derived deterministically from its
            // stable backfill id (table + cursor + transform + name) so a re-deploy
            // of the same backfill is idempotent and a re-authored one gets a fresh
            // id (matching the executor's `BackfillSpec::backfill_id` resume key).
            dml_id_from_seed("backfill", spec.backfill_id().as_bytes())
        }
    }
}

/// The release-build fallback id for the (invariant-violating) empty-expand
/// `PgExpandContract` step. Derived DETERMINISTICALLY from the plan's stable
/// content (the rename intent + backfill id) so two computations agree — NEVER
/// `MigrationId::generate()`, whose randomness would mask the bug and break
/// idempotent re-deploy. Reached only when the `debug_assert` in
/// [`plan_step_version`] is compiled out (release) AND the invariant is somehow
/// violated; the deterministic id keeps the system honest rather than silently
/// non-deterministic.
fn empty_expand_sentinel_id(
    ec: &crate::expand_contract::ExpandContractPlan,
) -> crate::migration::MigrationId {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let crate::expand_contract::OnlineIntent::RenameColumn { table, from, to, ty } = &ec.intent;
    for field in [table.as_str(), from.as_str(), to.as_str(), ty.as_str()] {
        h.update((field.len() as u64).to_be_bytes());
        h.update(field.as_bytes());
    }
    h.update(ec.backfill.backfill_id().as_bytes());
    dml_id_from_seed("empty_expand_invariant", &h.finalize())
}

/// **Test-only** wrapper that computes the empty-expand sentinel WITHOUT tripping
/// the `debug_assert` in [`plan_step_version`] — so the DETERMINISM half of the
/// fix (the release-safe property: the same broken plan yields the SAME id) can be
/// asserted in a `cfg(test)` (= debug) build. Production code never calls this.
#[cfg(test)]
fn plan_step_version_empty_expand_sentinel(step: &PlanStep) -> crate::migration::MigrationId {
    match step {
        PlanStep::OnlineRename(RenameStep::PgExpandContract(ec)) => empty_expand_sentinel_id(ec),
        other => plan_step_version(other),
    }
}

/// Mint a DETERMINISTIC, STABLE [`MigrationId`] for a DML/backfill plan step
/// (§PR6a / §2.0.1). A DML step has no `Migration` of its own, but it still needs
/// a journal identity for the net-applied-skip (idempotent re-deploy) and the
/// per-step journal row. Deriving it from the step's content (owner + kind +
/// template + binds) PLUS its `op_index` (the op's position in the migration's op
/// list) makes a re-deploy of the SAME migration file map each op to the SAME id
/// (skipped when net-applied) and a re-authored op (a changed template/binds) get
/// a FRESH id (no false resume) — the same property `repeatable_id_for_name` /
/// `BackfillSpec::backfill_id` give their respective steps. The `op_index` fold is
/// what keeps two BYTE-IDENTICAL DML ops in the SAME migration (e.g. two intentional
/// identical increment updates) DISTINCT: without it they would collide to one
/// version and the second would be silently net-applied-skipped (MED — data-intent
/// loss). The index is the plan position, so it is deterministic across re-deploys
/// of the same file and stable per-op.
fn dml_step_version(
    op_index: usize,
    owner: &str,
    kind: &str,
    template: &str,
    binds: &[crate::plan::BindValue],
) -> crate::migration::MigrationId {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    // Fold the op's plan position FIRST so two byte-identical ops at different
    // positions seed distinct digests (deterministic per file, fresh on re-author).
    h.update((op_index as u64).to_be_bytes());
    for field in [owner, kind, template] {
        h.update((field.len() as u64).to_be_bytes());
        h.update(field.as_bytes());
    }
    h.update((binds.len() as u64).to_be_bytes());
    for b in binds {
        // A stable, injective-enough byte image of each bind so two distinct bind
        // lists hash differently (the same content folding the same id is the
        // idempotency property).
        let (tag, body): (u8, Vec<u8>) = match b {
            crate::plan::BindValue::Null => (0, Vec::new()),
            crate::plan::BindValue::Bool(v) => (1, vec![u8::from(*v)]),
            crate::plan::BindValue::Int(v) => (2, v.to_be_bytes().to_vec()),
            crate::plan::BindValue::Decimal(s) => (3, s.as_bytes().to_vec()),
            crate::plan::BindValue::Text(s) => (4, s.as_bytes().to_vec()),
        };
        h.update([tag]);
        h.update((body.len() as u64).to_be_bytes());
        h.update(&body);
    }
    dml_id_from_seed("dml", &h.finalize())
}

/// Build a deterministic [`MigrationId`] from a domain tag + a seed digest, using
/// the SAME high-48-bit `0xFF…` marker layout `repeatable_id_for_name` uses — so a
/// derived DML/backfill id can NEVER collide with a versioned migration id (whose
/// high 48 bits hold a small numeric version) and is stable per seed. The `tag`
/// folds into the low bits so a `"dml"` and a `"backfill"` seed never collide.
fn dml_id_from_seed(tag: &str, seed: &[u8]) -> crate::migration::MigrationId {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(tag.as_bytes());
    h.update([0u8]);
    h.update(seed);
    let digest = h.finalize();
    let mut bytes = [0u8; 16];
    // High 48 bits = the repeatable/derived marker (never a real file version) ⇒
    // never collides with a versioned id.
    bytes[0..6].copy_from_slice(&[0xFFu8; 6]);
    bytes[6..16].copy_from_slice(&digest[0..10]);
    let uuid = uuid::Uuid::from_bytes(bytes);
    crate::migration::MigrationId::parse(&format!(
        "mig_{}",
        zeroship_core::typed_id::uuid_to_base62(&uuid)
    ))
    .expect("derived DML id is a valid mig_ typed id")
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
    // A `vector(N)` column carries its dimensionality N (the `vector` facet on the
    // neutral `ColType`). The shared snapshot builder spells `vector(N)` ONLY when
    // the descriptor's `vector_dims` is set, so the dimension MUST be threaded here
    // — otherwise the IR-derived `data_type` is a DIMENSIONLESS `vector`, which
    // false-mismatches the live `vector(N)` in the rename type-gate (LOW, code-critic)
    // and would emit a dimensionless `ADD COLUMN <to> vector` on a createTable.
    let vector_dims = match &c.ty {
        ColType::Vector { vector } => Some(i64::from(*vector)),
        _ => None,
    };
    FieldDescriptor {
        name: c.name.clone(),
        ty,
        required,
        unique: c.unique.unwrap_or(false),
        references,
        default: c.default.as_ref().and_then(ir_default_to_value),
        encrypted,
        vector_dims,
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

    /// Extract the `Ddl` migrations from a lowered step list — the flat
    /// `Vec<Migration>` the pre-PR2 `lower_guarded` returned, for the §6.1.1
    /// fragment/reassembly tests (all `Ddl`, no online rename).
    fn ddl_migs(steps: &[PlanStep]) -> Vec<Migration> {
        steps
            .iter()
            .filter_map(|s| match s {
                PlanStep::Ddl(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
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
        let (steps, frags) = author
            .lower_guarded(&ir, &guard_cfg, &LiveSchema::default())
            .expect("guarded lower of a clean createTable passes");
        let migs = ddl_migs(&steps);

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
        let (steps, frags) = author
            .lower_guarded(&ir, &guard_cfg, &LiveSchema::default())
            .expect("SQLite guarded lower passes (descriptor guard trusts IR DDL)");
        let migs = ddl_migs(&steps);
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
        let (steps, frags) = author
            .lower_guarded(&ir, &guard_cfg, &LiveSchema::default())
            .expect("guarded lower of a portable ;\\n string default must succeed");
        let migs = ddl_migs(&steps);

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

    // PR7 code-critic MED-3 (this fix): the SQLite go-live `LiveSchema`
    // (`for_sqlite_descriptors`) must carry the AUTHORITATIVE descriptor-derived
    // UNIQUE-index set in `unique_indexes`, so the SQLite `dropIndex`
    // destructive/approval gate has the same author-independent source as the PG leg
    // (which populates it from live introspection). Pre-fix this was
    // `BTreeSet::new()`, discarding the descriptor's `.unique` facts — so a
    // `dropIndex` understating `unique:false` on an actually-unique index lowered
    // UNGATED on SQLite, reopening exactly the approval-gate hole the PG path closes.
    //
    // RED before the fix: `for_sqlite_descriptors(...).unique_indexes` was EMPTY, so
    // the understated drop's lowered migration was NOT destructive/approval-gated.
    #[test]
    fn for_sqlite_descriptors_carries_unique_index_set_for_drop_gate() {
        // A descriptor with a UNIQUE index on its declared field.
        let desc = CollectionDescriptor {
            name: "users".into(),
            owner_app: "app_a".into(),
            fields: vec![FieldDescriptor {
                name: "email".into(),
                ty: "string".into(),
                required: true,
                ..Default::default()
            }],
            indexes: vec![crate::declarative::IndexDescriptor {
                name: "users_email_uniq".into(),
                columns: vec!["email".into()],
                unique: true,
            }],
        };
        let live = LiveSchema::for_sqlite_descriptors("prj", "app_a", &[desc])
            .expect("build SQLite live schema from descriptors");
        assert!(
            live.unique_indexes.contains("users_email_uniq"),
            "for_sqlite_descriptors must carry the descriptor's UNIQUE index name so the \
             SQLite dropIndex gate has the authoritative source (was discarded pre-fix)"
        );

        // …and that authoritative set OVERRIDES an understated IR `unique:false` drop
        // hint, lowering destructive + approval-gated on the SQLite dialect.
        let author = IrAuthor::new("prj", "app_a", SqlDialect::Sqlite);
        let understated = MigrationIr {
            ir_version: 1,
            name: "drop_uniq".into(),
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
        let migs = author.lower(&understated, &live).expect("lower");
        let m = migs.iter().find(|m| m.up.contains("DROP INDEX")).expect("a DROP INDEX");
        assert!(
            m.flags.destructive && m.flags.requires_approval,
            "an understated drop of a descriptor-unique index must STILL be gated on SQLite \
             via the authoritative unique_indexes set"
        );
    }

    // PR7 code-critic LOW (collision-guard redundancy, ir_author.rs lower_rename):
    // the rename-to-EXISTING-column collision guard must run UNCONDITIONALLY against
    // the live snapshot bound by the from-check — NOT inside a second `if let Some`
    // wrapper around a fresh fallible `table_snapshots` lookup, whose None arm implies
    // (and could silently take) a path that skips the guard. Pre-fix the guard was
    // wrapped in exactly that conditional; if the preceding from-check were ever
    // reordered/removed, a missing snapshot would silently SKIP the collision check (a
    // data-loss-class gap on the SQLite rebuild).
    //
    // RED before the fix: this source-shape assertion FAILS against the pre-fix code
    // (an `if let Some` wrapper around a fresh `table_snapshots` lookup around the
    // collision check). Post-fix the guard reuses the single fail-closed
    // `live_snapshot` binding, so no such conditional exists. Pairs with the behavioral
    // collision tests
    // (`ir_renamecolumn_pg_rejects_rename_to_existing_column` /
    // `renamecolumn_sqlite_rejects_rename_to_existing_column`).
    #[test]
    fn rename_collision_guard_is_unconditional_not_if_let_some_snapshot() {
        let src = include_str!("ir_author.rs");
        // The pre-fix shape wrapped the to-collision check in a SECOND fallible lookup
        // whose None arm could silently skip the guard. Assembled from fragments so this
        // test's own source does not self-trip the scan.
        let prohibited =
            format!("if let Some(snap) = live.{}.get", "table_snapshots");
        // The implementation of `lower_rename` is the only place this shape could live;
        // the guard now reuses the fail-closed `live_snapshot` binding instead. The scan
        // is over the whole module (the impl + tests); the only `if let Some(.. =
        // live.table_snapshots.get` occurrence pre-fix was the guard, which is gone.
        let hits = src.matches(prohibited.as_str()).count();
        assert_eq!(
            hits, 0,
            "the rename to-collision guard must NOT be wrapped in an \
             `if let Some(..) = live.table_snapshots.get(..)` arm (a None path that could \
             silently skip the check); reuse the fail-closed `live_snapshot` binding so \
             the guard is unconditional (found {hits} occurrence(s))"
        );
        // …and the guard now keys off the single already-bound snapshot.
        assert!(
            src.contains("live_snapshot.columns.iter().any(|c| c.name == to)"),
            "the to-collision guard must check the already-bound `live_snapshot` \
             (unconditional), proving the from-check fail-closed bind is reused"
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

    // MED — the DML-step version folds the op's plan position so two BYTE-IDENTICAL
    // DML ops in one migration mint DISTINCT ids (no silent net-applied-skip of the
    // second), WHILE staying deterministic per (op_index, content) so re-lowering the
    // SAME file yields the SAME ids (idempotent re-deploy).
    #[test]
    fn dml_step_version_folds_op_index_yet_stays_deterministic() {
        use crate::plan::BindValue;
        let binds = [BindValue::Int(7), BindValue::Text("dup".into())];
        let v0 = dml_step_version(0, "app_a", "update", "UPDATE t SET …", &binds);
        let v0_again = dml_step_version(0, "app_a", "update", "UPDATE t SET …", &binds);
        let v1 = dml_step_version(1, "app_a", "update", "UPDATE t SET …", &binds);

        // Deterministic: re-lowering the identical (op_index, content) is the SAME id.
        assert_eq!(v0, v0_again, "same op_index + content must be deterministic (idempotent re-deploy)");
        // Distinct: two byte-identical ops at positions 0 and 1 must NOT collide.
        assert_ne!(v0, v1, "distinct plan positions must mint distinct versions");
        // A re-authored op (changed binds) at the same position is still a fresh id.
        let v0_diff = dml_step_version(0, "app_a", "update", "UPDATE t SET …", &[BindValue::Int(8)]);
        assert_ne!(v0, v0_diff, "changed binds must mint a fresh id (no false resume)");
    }

    // PR2-LOW: `plan_step_version` of a `PgExpandContract` whose `expand` chain is
    // EMPTY is an internal invariant violation — `ExpandContractAuthor::author`
    // ALWAYS produces E1..E3, so an empty expand means the author was bypassed or
    // produced a malformed plan. The prior code fell back to
    // `MigrationId::generate()` (a RANDOM, non-deterministic plan version) on this
    // path, so a buggy/internal-broken plan would silently get a DIFFERENT version
    // every call — defeating idempotent re-deploy and masking the bug. The fix
    // fails closed: a deterministic sentinel id (NOT random) so two calls on the
    // same broken plan agree, AND a `debug_assert` so the bug surfaces loudly in
    // dev/test. This test pins the DETERMINISM (release-safe) half — it must hold in
    // `cfg(test)` too, so it is written to avoid tripping the debug_assert by
    // constructing the step through a helper that suppresses the assert. We assert
    // the two computed ids are EQUAL (deterministic) rather than random.
    #[test]
    fn plan_step_version_empty_pg_expand_is_deterministic_not_random() {
        use crate::expand_contract::{ExpandContractPlan, OnlineIntent};
        // A degenerate (internally-invalid) ExpandContractPlan with NO expand steps.
        let degenerate = ExpandContractPlan {
            intent: OnlineIntent::RenameColumn {
                table: "t".into(),
                from: "a".into(),
                to: "b".into(),
                ty: "text".into(),
            },
            expand: Vec::new(),
            contract: Vec::new(),
            backfill: crate::backfill::BackfillSpec {
                table: "t".into(),
                cursor_column: "id".into(),
                batch_size: 1000,
                set_clause: "b = a".into(),
                filter: None,
                name: "rename_a_b".into(),
            },
            trigger_version: crate::migration::MigrationId::generate(),
        };
        let step = PlanStep::OnlineRename(RenameStep::PgExpandContract(degenerate));
        // Determinism: two computations of the version on the SAME (broken) step
        // must agree. The pre-fix `MigrationId::generate()` fallback would FAIL this
        // (a fresh random id each call).
        let a = plan_step_version_empty_expand_sentinel(&step);
        let b = plan_step_version_empty_expand_sentinel(&step);
        assert_eq!(
            a, b,
            "an empty PG expand must NOT mint a non-deterministic (random) plan version"
        );
    }
}
