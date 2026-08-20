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
//!   central table of expected counts would be the census it is fixing.
//! - [`compose`] - the shared docker-compose model. Reusable: four shell gates
//!   and one deploy script currently hand-roll six separate approximations of
//!   it, and this is what they collapse onto.
//! - [`report`] - [`report::Report`] / [`report::Verdict`]. Encodes the
//!   three-outcome rule (green / red / REFUSED) so "checked nothing" cannot be
//!   spelled as a pass.
//! - [`secret_strength`] - the compose secret-strength gate.
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
pub mod compose;
pub mod report;
pub mod secret_strength;
