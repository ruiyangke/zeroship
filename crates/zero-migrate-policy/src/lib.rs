//! `zero-migrate-policy` — the Policy Decision Point (PDP): the *mechanism* half
//! of the zero-migrate policy system.
//!
//! This crate is a true LEAF: pure data + algebra, zero I/O, **no SQL deps**, no
//! dependency on the engine. It ships the policy *machine*; every piece of policy
//! *content* is injected by the consumer.
//!
//! # Phase 1a — the scope lattice
//!
//! The first module is [`scope`]: the security-core primitive. `Scope { Nothing |
//! All | Of{include, exclude} }` over one/two-segment identifier globs, with the
//! full lattice — `normalize`, `⊑` (subset), `⊓` (meet), `⊔` (join), `∖`
//! (difference) — verified EXHAUSTIVELY by a brute-force oracle
//! (`scope::oracle`, `#[cfg(test)]`) against a direct ground-truth matcher. The
//! oracle is the correctness proof: where prose review of the glob algebra could
//! not be trusted, the oracle-green code is authoritative.

pub mod scope;

pub use scope::{
    glob::SegGlob,
    pattern::{ObjectName, Pattern},
    Difference, Scope, ScopeError,
};
