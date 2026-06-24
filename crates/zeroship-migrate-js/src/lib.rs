//! `zeroship-migrate-js` — the optional JS/TS schema **front-end** for the
//! lean `zeroship-migrate` engine.
//!
//! Atlas's real shape is *many schema front-ends (HCL / SQL / ORM providers)
//! → one internal representation → one diff/migrate engine.* `zeroship-migrate`
//! adopts the same shape with the **descriptor IR**
//! ([`zeroship_migrate::declarative::CollectionDescriptor`]) as the internal
//! representation. This crate is the **JS/TS front-end**: it evaluates a
//! creator `schema.js` (the `@zeroship/db` `t.*` DSL) inside
//! zeroship-runtime's V8 sandbox and lowers it to that IR, then drives the
//! engine's declarative differ to emit a versioned migration file. It is the
//! analog of Atlas's ORM/HCL providers — the zeroship "the tool natively
//! speaks the app's schema" superpower.
//!
//! ## Lean-core purity (design §5.1 guardrail)
//!
//! Evaluating `schema.js` needs a real JS engine (V8), which is heavy. The
//! lean public CLI (`zeroship-migrate migrate ./db/migrations`) must NOT
//! carry it — so the JS engine lives in THIS separate crate. `cargo tree -p
//! zeroship-migrate` carries NO `v8`; `cargo tree -p zeroship-migrate-js`
//! carries `v8` via `zeroship-runtime`. There is no dependency cycle:
//! `zeroship-runtime` depends on neither `zeroship-migrate` nor
//! `zeroship-schema`.
//!
//! ## Public surfaces
//!
//! - [`eval::eval_schema_to_ir`] — eval a `schema.js` → `Vec<CollectionDescriptor>`.
//! - [`generate::generate_migration`] — full `generate --schema` flow:
//!   eval → IR → diff against the live DB → render a dbmate migration file.
//! - [`record::record_migration_to_ir`] — the op.* recorder (design §2.5 / PR1):
//!   eval a migration module's `op.*` `up()` → the typed `.ir.json`
//!   [`MigrationIr`](zeroship_migrate::MigrationIr). This is the JS half of the
//!   anti-drift gate — the single-source-of-truth byte/value-equality the golden
//!   `.ir.json` corpus + the `Checksum::of_ir` round-trip test pin.

pub mod eval;
pub mod generate;
pub mod record;

pub use eval::{eval_schema_to_ir, EvalError};
pub use generate::{generate_migration, GenerateError, GenerateOutcome};
pub use record::{
    lint_migration_determinism, record_migration_to_ir, record_migration_to_json,
    DeterminismFinding, RecordError,
};
