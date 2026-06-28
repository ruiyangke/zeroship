//! JS/TS schema **front-end** for the `zeroship-migrate` engine.
//!
//! Atlas's real shape is *many schema front-ends (HCL / SQL / ORM providers)
//! → one internal representation → one diff/migrate engine.* `zeroship-migrate`
//! adopts the same shape with the **descriptor IR**
//! ([`crate::render::declarative::CollectionDescriptor`]) as the internal
//! representation. This module is the **JS/TS front-end**: it evaluates a
//! creator `schema.js` (the `@zeroship/db` `t.*` DSL) inside the sandboxed
//! recorder child and lowers it to that IR, then drives the engine's
//! declarative differ to emit a versioned migration file. It is the
//! analog of Atlas's ORM/HCL providers — the zeroship "the tool natively
//! speaks the app's schema" superpower.
//!
//! ## Crate shape
//!
//! The migrate engine is JS-first: V8 is linked unconditionally through
//! `zeroship-runtime`, and the public authoring surface lives here under
//! `crate::frontend`. Native `compio-postgres` remains the deploy/apply
//! fast-path backend; this module is the schema/migration authoring front-end.
//!
//! ## Public surfaces
//!
//! - [`eval::eval_schema_to_ir`] — eval a `schema.js` in the sandboxed recorder child
//!   → `Vec<CollectionDescriptor>`.
//! - [`build::build_migrations`] / [`build::build_one_migration`] — the PR4 build
//!   path: record each `.ts` via the KERNEL-SANDBOXED recorder child
//!   ([`recorder_service::spawn_sandboxed_record`]) → committed `.ir.json`. This is
//!   the ONLY surface that evaluates untrusted migration `.ts`.
//! - [`generate::generate_migration`] — the PLATFORM schema-authority generate path:
//!   eval → IR → diff against the live DB → render a deployable `.sql` migration
//!   (covers the full goodie surface — vector/postgis/FK/CHECK — that the portable
//!   op.* `generate_ops` fail-closes on); distinct from [`scaffold::generate_ops`]
//!   (the creator-facing portable op.* autogenerate → `.ts`/`.ir.json`).
//!
//! The in-process `record::record_migration_to_*` functions (design §2.5 / PR1) run
//! UNSANDBOXED V8 in-process and are `#[doc(hidden)]` test-only oracles — see their
//! safety notes. They are NOT a public recording surface; record via the sandboxed
//! build path above.

pub mod build;
#[doc(hidden)]
pub mod embedding;
pub mod eval;
pub mod gen_types;
pub mod generate;
pub mod record;
pub mod recorder_http;
pub mod recorder_protocol;
pub mod recorder_service;
pub mod sandbox;
pub mod scaffold;

pub use build::{
    assert_packed_hash_matches_committed, build_migrations, build_one_migration,
    discover_migrations, recheck_not_yet_applied, BuildError, BuildOutcome, BuiltMigration,
    DiscoveredMigration, RecordPath, RecordVia, RecorderClient,
};
pub use eval::{eval_schema_to_ir, EvalError};
pub use gen_types::{
    check_artifacts, load_dir_ops, render_artifacts, write_artifacts, GenTypesError,
    GeneratedArtifacts, ENV_DTS_FILE, RUNTIME_DESCRIPTOR_FILE,
};
pub use generate::{generate_migration, GenerateError, GenerateOutcome};
pub use scaffold::{
    generate_ops, scaffold_new_ts, timestamp_14, GeneratedMigration, ScaffoldError,
};
pub use record::{
    lint_migration_determinism, record_migration_to_ir, record_migration_to_ir_with_warnings,
    record_migration_to_json, DeterminismFinding, RecordError, RecordOutcome,
};
pub use recorder_service::{
    recorder_child_path, spawn_sandboxed_record, Authorizer, ConcurrencyLimits, RecordRequest,
    RecordResult, RecorderError, RecorderService, SchemaEvalRequest, SchemaEvalResult,
    spawn_sandboxed_schema_eval,
};
pub use sandbox::{ResourceBudget, SandboxPosture, SandboxReport};
