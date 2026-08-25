//! # `zero-migrate-backend` — the backend CONTRACT
//!
//! The crate `zero_migrate::render::renderer` and `zero_migrate::schema::backends`
//! have both called "the future `zero-migrate-backend`" in their headers since the
//! in-crate backend modules were written. This is it.
//!
//! It holds the per-vendor TRAITS, the vocabulary their signatures name, and
//! the registry shape a vendor crate hands back. It deliberately holds no vendor:
//! nothing here spells a keyword, quotes an identifier or names a dialect. Its
//! wire-level target identity is the open `DialectId` from `zero-migrate-ir`.
//!
//! | trait | question it answers |
//! |---|---|
//! | [`renderer::DmlRenderer`] | how does this vendor spell DML, views and triggers |
//! | [`schema::SchemaRenderer`] | how does this vendor spell column types and collations |
//! | [`ddl::DdlEmitter`] | how does this vendor spell schema-changing statements |
//! | [`fold::CatalogFoldPolicy`] | how does this vendor shape shared catalog replay |
//! | [`existence_probe::ExistenceProbePolicy`] | how do this vendor's catalog identities behave under guarded probes |
//! | [`guard::MigrationGuard`] | what does this vendor REFUSE to run |
//! | [`stored_ddl::StoredDdl`] | how does this vendor parse catalog-stored table DDL |
//! | [`value_format::ValueFormatRenderer`] | how does this vendor render and normalize ID formats |
//! | [`backend::CrossDeployObligations`] | can this vendor open and discharge a cross-deploy obligation |
//! | [`capability::OnlineSchemaChange`] | can this vendor run a zero-downtime online expand |
//!
//! The same dependency rule governs all ten: a trait declared in the engine would
//! force every vendor to depend on the engine, which already depends on every vendor.
//!
//! ```text
//!   zero-migrate-policy ─┐
//!                        ├─> zero-migrate-ir ──> zero-migrate-backend ──┬─> zero-migrate-postgres ─┐
//!                        │                                              ├─> zero-migrate-sqlite   ─┼─> zero-migrate
//!                        └──────────────────────────────────────────────┴─> zero-migrate-mysql    ─┘
//! ```
//!
//! # Why the traits are HERE and not in `zero-migrate-ir`
//!
//! `zero-migrate-ir` is the WIRE CONTRACT: `MigrationIr`, the closed `Op` enum, the
//! closed `Expr` AST, the canonical checksum, the structural validator. Its own
//! manifest calls it "pure data, zero I/O". This crate is 6,000-odd lines of SQL
//! RENDERING — an expression-to-SQL lowerer, a PostgreSQL vendor-DDL renderer, an
//! identifier-quoting seam. Putting that in `-ir` would make every consumer that
//! wants to checksum an envelope compile a SQL renderer, and it would erase the one
//! distinction the two crates exist to keep: what a migration SAYS versus how a
//! vendor WRITES it.
//!
//! The split is also what the `-ir` half already assumes. `DialectId`, `Capability`,
//! `BackendDescriptor` and `BackendRegistry` were promoted into `-ir` because they
//! are IDENTITY and CAPABILITY — facts about a backend that a checksummer or a
//! policy engine legitimately reads. The renderer/parser traits are neither; they
//! are backend-owned spelling and normalization.
//!
//! # What had to come with the traits, and the measurement that bounded it
//!
//! A trait cannot move without the types in its signatures. The transitive closure of
//! the three DML vendor modules over the engine, measured by walking `crate::`
//! references at module granularity with `#[cfg(test)]` stripped, was **54 modules /
//! 113,216 lines** — the whole engine, in effect, because
//! [`error::IrLowerError`] sat in the 16,868-line `render::lower`, which reaches
//! `engine`, `apply::*`, `model::validate` and `render::fold`.
//!
//! Moving three things collapses it to **7 modules / 7,733 lines**:
//! [`error::IrLowerError`], [`error::DeclarativeError`] and [`step::BindValue`].
//! Every other apparent edge dissolved on inspection: `crate::model::ir`,
//! `crate::model::expr` and the former `crate::schema::query` dialect identity were
//! `-ir` re-exports, and `BackfillSpec` / `PlanStep` appeared in doc links only.
//!
//! # What is NOT here
//!
//! The engine's `render::lower`, `render::declarative`, `render::fold`, and the bulk
//! of `schema::query`. The engine still composes comparisons and decisions; the
//! backend contract supplies every vendor-specific spelling and catalog-normalization
//! fact those algorithms consume.
//!
//! # `MigrationBackend` is here, and what it took to get it here
//!
//! [`backend::MigrationBackend`] is the apply/rollback seam: session I/O, the
//! per-migration confined apply, journal row I/O, parse-time non-txn validation,
//! drift introspection, preconditions, and the structured operations. It could not
//! be routed through [`registry::VendorSet`] like the renderer traits — it has an
//! associated type, thirty-odd `async fn`, and borrows its session, so it is not
//! dyn-compatible — which is why it had to physically move rather than be resolved
//! dynamically.
//!
//! Three obstacles held it, and only one of them was about size:
//!
//! * **A direction error.** `TableRebuildSpec::sequence_policy` was typed
//!   `zero_migrate_sqlite::SqliteSequencePolicy` — a VENDOR type, in the shared
//!   plan vocabulary, in a crate the vendors sit above. `PlanStep` reaches it
//!   through `RenameStep::TableRebuild`, so one field stranded
//!   `TableRebuildSpec`, `TableRebuild`, `RenameStep` and `PlanStep` together. The
//!   field carries a neutral [`table_rebuild::SequenceHighWaterPolicy`] now and the
//!   vendor converts at its own boundary.
//! * **A capability nobody had.** `MigrationBackend::shadow()` returned
//!   `Option<&dyn ShadowDryRun>` and all three backends answered `None`, so the
//!   seam asked every vendor to declare a harness none of them had — while
//!   `ShadowDryRun::dry_run_declarative` names the engine's `DeclarativeDeployPlan`
//!   and `DesiredSchema`. The harness is a parameter to the engine's `dry_run` now.
//! * **An error variant nobody built.** `SeedError` dragged the engine's whole
//!   `EngineError` into the dry-run capability's signature for two variants that
//!   had zero constructors workspace-wide. Deleted.
//!
//! What deliberately did NOT come down: `EngineError`, `DeclarativeDeployPlan` and
//! `DesiredSchema`. Those are ORCHESTRATION RESULTS — what the engine DECIDED,
//! built on its `MigrationPlan`, its `ResolvedInject`, and an error enum over
//! `plan::pending` and `ManifestError`. Moving them would move the engine, so
//! `ShadowDryRun` stays above and is handed in rather than declared.

pub mod advisory;
// What each backend DECLARES about its own vendor attributes: the keys it owns, the IR
// node each attaches to, and what a legal value looks like. The neutral half — the key,
// the map, the scope — is `zero_migrate_ir::attribute`; this is the vendor-facing half,
// so it sits with the other things a `BackendVendor` hands over.
pub mod attribute;
// The caller's approval decision. Named by `OnlineSchemaChange::run_online` and by
// every gated apply/rollback entry point, so it sits with the traits rather than
// above them. Zero dependencies of its own. The engine re-exports it at
// `zero_migrate::approval`.
pub mod approval;
// Pure data for large-table backfill plan steps: the `BackfillSpec` a vendor's
// backfill executor is handed, its cursor contract and its checksum. Depends on
// `zero-migrate-ir` alone. The engine re-exports it at `zero_migrate::model::backfill`.
// THE APPLY/ROLLBACK SEAM: `MigrationBackend` itself, the `CrossDeployObligations`
// capability, and the neutral values their signatures name (`PlaceholderStyle`,
// `JournalFuture`, `ProjectLockHolder`, `ProjectLockAcquisition`,
// `PlanPreconditionVerdict`). The three vendor IMPLEMENTATIONS are still in the
// engine's `apply::backend::{postgres, mysql, sqlite}`; this is the trait they
// implement, sitting below them so they can eventually leave. The engine
// re-exports all of it at `zero_migrate::apply::backend`.
pub mod backend;
pub mod backfill;
// The adoption path's dialect-neutral VOCABULARY: `BaselineOutcome` and the
// `BaselineError` set every `MigrationBackend::baseline_one` impl speaks. No
// implementation comes with it — PostgreSQL journals a recorded-not-run row,
// SQLite does the same through its actor, and MySQL refuses. The engine
// re-exports it at `zero_migrate::apply::baseline`.
pub mod baseline;
// The optional capabilities: `OnlineSchemaChange` in full, with the `OnlineIntent`
// an expand is handed, the `OnlineError` it refuses with and the
// `ExpandContractPlan` a rename lowers to; plus the shadow dry-run's neutral half
// (`ShadowConfig` in, `DryRunReport`/`MigrationResult` out). `ShadowDryRun` itself
// stays in the engine — `dry_run_declarative` names `DeclarativeDeployPlan` and
// `DesiredSchema`, which are orchestration results — and the engine hands the
// harness to its own `dry_run` rather than asking a backend to declare one.
pub mod capability;
// The per-run executor configuration: which project, which schema, which meta
// schema, which timeout budgets, and the composed policy every executor-path
// guard is built from. `ExecutorConfig` is the single most-named type in the
// contract — every `MigrationBackend` I/O method takes a `&ExecutorConfig` — so
// it sits with the traits. The engine re-exports it at `zero_migrate::conn`.
pub mod conn;
// The canonical constraint-`definition` normal form: the conditional-quote codec,
// the FK body, and the FK `ConstraintSnapshot` built on it. It sits here because
// every backend's drift path BUILDS that body — MySQL's catalog stores no rendered
// constraint text at all — so it has to be reachable from a vendor crate. The two
// halves that need a vendor fact take a `&BackendVendor`; the rest names no dialect.
// COMPARISON text, never an emitted identifier route. The engine re-exports the
// neutral half and keeps dialect-taking shims at `zero_migrate::render::declarative`.
pub mod constraint_definition;
pub mod ddl;
pub mod descriptors;
// Run-time, vendor-OWNED values keyed by the backend that owns them — the
// counterpart to `registry::BackendVendor`, which carries everything a vendor
// knows statically. A value read off a live catalog or handed in by the host
// cannot be the `&'static dyn` that struct holds, so it rides here instead of
// becoming a vendor-named field on a neutral struct.
pub mod dialectal;
pub mod dml;
// The drift-report VOCABULARY a backend's drift query hands back: the checksum /
// tamper / orphan shapes, the structural-divergence shapes, their aggregate and
// the shared `DriftError`. The comparison algorithms that produce them stay in
// the engine. The engine re-exports these at `zero_migrate::apply::drift`.
pub mod drift;
// The dialect-neutral network driver seam (`SqlSession`) and its conformance
// suite. A CONTRACT with no vendor in it: `std` is its only dependency, it
// spells no keyword and names no dialect, and the network backends are generic
// over it. The engine re-exports it at `zero_migrate::driver`.
pub mod driver;
pub mod error;
// The crash-simulation seam. It sits here for the same reason the trait does: all
// three `MigrationBackend` implementations trip it at their OWN sub-step boundaries
// (after a DML statement, before a journal row, mid-backfill-batch), so a home above
// the vendors is a home they cannot reach once they are separately linkable. It
// names only `executor::ApplyError` and `std`, and it spells no SQL. The engine
// re-exports it at `zero_migrate::fault`.
#[doc(hidden)]
pub mod fault;
// The apply/rollback VOCABULARY (not an executor): `LockMode`, `ApplyOutcome`,
// `BackendError`, `ApplyError`, `PreconditionVerdict` and the `Rollback*` set. The
// generic orchestration stays in the engine; these are the types every
// `MigrationBackend` signature names. The engine re-exports them at
// `zero_migrate::apply::executor`.
pub mod executor;
pub mod existence_probe;
pub mod fold;
pub mod guard;
// The migration journal's dialect-neutral vocabulary: the wire enums whose exact
// literals are the CONTRACT between the three per-vendor journal writers, the row
// shapes they read back, and the shared `JournalError`. It emits no SQL. The
// engine re-exports it at `zero_migrate::apply::journal`.
pub mod journal;
pub mod mask_codec;
pub mod mask_meta;
pub mod registry;
pub mod renderer;
// What a lowered plan needs the LIVE target to be able to do — the closed
// `DatabaseFeature` set and the deduplicated `DatabaseRequirements` a backend's
// `verify_database_requirements` is handed. The engine re-exports both at
// `zero_migrate::render::plan`.
pub mod requirements;
pub mod schema;
pub mod schema_error;
pub mod snapshot;
pub mod spelling;
// The status VOCABULARY — what a journal read reduces to. The status VERBS stay in
// the engine; this is the shape the engine's neutral verb and a vendor's own
// snapshot read both fill in, so the two can never answer in different words.
pub mod status;
pub mod step;
pub mod stored_ddl;
pub mod table_rebuild;
// The finite-timeout-budget rule every dialect's session render is bound by. Zero
// dependencies of its own; the vendors are what resolve a budget, so the rule sits
// with them. The engine re-exports it at `zero_migrate::apply::timeout`.
pub mod timeout;
pub mod validation;
pub mod value_format;
pub mod vendor;
