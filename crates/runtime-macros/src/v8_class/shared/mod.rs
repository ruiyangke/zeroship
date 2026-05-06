//! Shared codegen primitives for `#[v8_class]`. See
//! `docs/proposals/runtime-macros-refactor.md` §4.1 and §3.8. Hosts
//! helpers shared across `emit/` submodules so
//! per-callback codegen sites delegate to one canonical implementation
//! instead of hand-rolling 10 LOC of brand-check + External-recovery
//! preamble seven times.
//!
//! Today this directory hosts:
//! - `recover_box` — the brand-check + External-recovery preamble.
//! - `class_config` — the `ClassConfig` parameter object that replaces
//!   the old 10-argument `gen_install` signature.

pub(crate) mod class_config;
pub(crate) mod recover_box;
