//! Policy, charter and guard: the authority an IR path has, and the SQL surface it is confined to.
//!
//! Declaring each `mod` below is load-bearing: a module missing from this list is a
//! suite that silently stops running.


mod charter_creatable_escape;
mod charter_root_bound;
mod declarative_require_rls_pg;
mod guard_seam;
mod guard_security;
mod guard_vendor_lower;
mod layered_policy;
mod pg_fail_closed_coverage;
mod project_schema_authority;
mod split_part_grammar_boundary;
mod sqlite_confinement;
mod sqlite_dqs_hardening;
mod trigger_binding_a_function_needs_the_function_grant;
mod trigger_ops_require_a_capability_grant;
mod vendor_capabilities_do_not_leak;
mod vendor_capability_policy_authority;
