//! Phase-shared codegen primitives for `#[v8_class]` (Wave 3 of the
//! runtime-macros refactor — design `docs/proposals/runtime-macros-refactor.md`
//! §4.1, §3.8). Hosts helpers shared across `emit/` submodules so
//! per-callback codegen sites delegate to one canonical implementation
//! instead of hand-rolling 10 LOC of brand-check + External-recovery
//! preamble seven times.
//!
//! Wave 3 ships:
//! - `recover_box` — the brand-check + External-recovery preamble.
//! - `class_config` — the `ClassConfig` parameter object that closes
//!   `gen_install`'s 10-arg signature (design §3.1, F4).
//!
//! Future waves migrate `must_str`, `op_error::gen_throw_op_error_arms`,
//! and the `Cell<Option<usize>>` reentry guard here too. Per design §4.1
//! the directory may relocate to `crate-root/shared/` in Wave 5.

pub(crate) mod class_config;
pub(crate) mod recover_box;
