//! Repository gates, as Rust.
//!
//! The first crate of the programme that replaces the ~41k-line shell harness
//! under `tests/`. It exists for one reason: a shell gate derives what it
//! checks from TEXT - grep over source, sed over YAML indentation - and text
//! coupling breaks silently when the text improves. `crates/core` renaming a
//! variable in a refusal message on 2026-08-13 de-enumerated the compose
//! secret gate's entire rule set, and it stayed that way for seven days.
//!
//! A gate here reads TYPED DATA instead, from the crate that owns it. When the
//! rules change, the compiler is what notices.
//!
//! NOT `zeroship-test-support`. That crate is the DATABASE substrate for cargo
//! tests - scratch databases, fixtures, live-backend plumbing. Surveyed
//! 2026-08-20: only 3 of the 22 shell gates source anything from `tests/lib`,
//! and none of the three want a database. Gate helpers and test-database
//! helpers are genuinely different substrates, and a gate binary should not
//! link a Postgres driver to read a YAML file.
//!
//! ## Layout
//!
//! - [`arm_census`] - the META-gate. Rules on the gates themselves: each must
//!   declare, per arm, how many items that arm ruled on and the floor that
//!   number must clear. It checks the PROPERTY and never the values, because a
//!   central table of expected counts would be the census it is fixing. Also
//!   holds [`arm_census::GateRun`], which is how a Rust gate declares its arms.
//! - [`compose`] - the shared docker-compose model. Every compose gate here
//!   reads it; the five separate `grep`/`awk` approximations of YAML they used
//!   to carry are gone.
//! - [`report`] - [`report::Report`] / [`report::Verdict`]. Encodes the
//!   three-outcome rule (green / red / REFUSED) so "checked nothing" cannot be
//!   spelled as a pass.
//! - [`secret_strength`] - compose secrets meet the product's strength rules.
//! - [`port_exposure`] - only the edge publishes on all interfaces.
//! - [`stateful_volume`] - stateful services keep data on a named volume.
//! - [`backing_service_reach`] - every backing service compose RUNS is one
//!   somebody is CONFIGURED TO REACH. Red on the tracked tree, on purpose.
//! - [`workflow_advance_flag`] - `deploy/` does not arm the unsigned
//!   workflow-advance path while the gateway edge is unauthenticated.
//!
//! ## The shell entry points
//!
//! Each gate keeps a `tests/<name>_gate.sh` SHIM that execs its binary through
//! `gate_arms_delegate`. The shim carries no rules - it is a path CI and humans
//! already know - and it is what keeps a Rust gate visible to
//! `gate-arm-census`, which enumerates `tests/*_gate.sh`. Deleting the shims
//! would take four gates out of the only check that they declare their arms at
//! all, so the shell FILE survives and the shell LOGIC does not.
//!
//! ## Conventions for a gate added here
//!
//! - Take inputs as ARGUMENTS. No environment variables, for configuration or
//!   for paths: a gate that reads its own process environment is a gate whose
//!   result depends on how it was launched.
//! - Take the rule set as a parameter, so the empty case is reachable from a
//!   test rather than only by editing the source.
//! - Return a [`report::Report`]; let the binary do the printing and the
//!   exiting.

pub mod arm_census;
pub mod backing_service_reach;
pub mod compose;
pub mod port_exposure;
pub mod report;
pub mod secret_strength;
pub mod stateful_volume;
pub mod workflow_advance_flag;
