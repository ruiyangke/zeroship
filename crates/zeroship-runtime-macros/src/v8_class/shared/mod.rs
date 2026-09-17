//! Shared codegen primitives for `#[v8_class]`. Hosts helpers shared
//! across `emit/` submodules so per-callback codegen sites delegate to
//! one canonical implementation instead of each hand-rolling its own
//! brand-check + External-recovery preamble.
//!
//! This directory hosts:
//! - `recover_box` — the brand-check + External-recovery preamble.
//! - `class_config` — the `ClassConfig` parameter object passed to every
//!   codegen helper.

pub(crate) mod class_config;
pub(crate) mod recover_box;
