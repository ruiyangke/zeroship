//! Policy, charter and guard: the authority an IR path has, and the SQL surface it is confined to.
//!
//! A THEMED test binary. Every `mod` below was its own `tests/*.rs` integration
//! target until the whole directory was regrouped; each one statically linked its
//! own copy of the crate. Declaring them here makes them modules of ONE binary.
//! Nothing about the tests themselves changed - a `mod` missing from this list is a
//! test that silently stops running, so the list is the load-bearing part of the file.

#[macro_use]
#[path = "../support/mod.rs"]
mod support;

mod charter_creatable_escape;
mod charter_root_bound;
mod declarative_require_rls_pg;
mod guard_seam;
mod guard_security;
mod layered_policy;
mod pg_fail_closed_coverage;
mod split_part_grammar_boundary;
mod sqlite_confinement;
mod sqlite_dqs_hardening;
mod vendor_capabilities_do_not_leak;
mod vendor_capability_policy_authority;
