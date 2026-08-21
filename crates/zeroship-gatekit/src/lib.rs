//! The META-gate, as Rust.
//!
//! A gate here reads TYPED DATA rather than text. `crates/core` renaming a
//! variable in a refusal message on 2026-08-13 de-enumerated a shell gate's
//! entire rule set, and it stayed that way for seven days; when the rules are a
//! Rust item, the compiler is what notices.
//!
//! NOT `zeroship-test-support`. That crate is the DATABASE substrate for cargo
//! tests - scratch databases, fixtures, live-backend plumbing. Gate helpers and
//! test-database helpers are different substrates, and this crate links no
//! driver and no HTTP stack.
//!
//! ## Layout
//!
//! - [`arm_census`] - the META-gate. Rules on the gates themselves: each must
//!   declare, per arm, how many items that arm ruled on and the floor that
//!   number must clear. It checks the PROPERTY and never the values, because a
//!   central table of expected counts would be the census it is fixing. Also
//!   holds [`arm_census::GateRun`], which is how a Rust gate declares its arms.
//! - [`report`] - [`report::Report`] / [`report::Verdict`]. Encodes the
//!   three-outcome rule (green / red / REFUSED) so "checked nothing" cannot be
//!   spelled as a pass.
//!
//! The five docker-compose gates this crate also carried until 2026-08-21 were
//! deleted, along with the shared compose model they read. That was a
//! complexity-budget decision about the crate, not a finding that the checks
//! were wrong: `deploy/compose/docker-compose.yml` now has no automated check
//! of its secret strength, its port publication, its named volumes, its
//! backing-service reachability, or the unsigned workflow-advance flag.
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
//! - Keep a `tests/<name>_gate.sh` SHIM that execs the binary through
//!   `gate_arms_delegate`. The shim carries no rules; it is what keeps a Rust
//!   gate visible to `gate-arm-census`, which enumerates `tests/*_gate.sh`.

pub mod arm_census;
pub mod report;
