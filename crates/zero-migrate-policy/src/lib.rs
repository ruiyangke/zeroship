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

//! # Phase 1b-i — the knob / rule / document model
//!
//! On top of the scope lattice this cut adds the policy *content model* and its
//! strict loader:
//!
//! - [`knob`] — [`KnobDef`], [`KnobKey`], [`KnobKind`], [`KnobValue`], [`Polarity`],
//!   [`Enforcement`], [`ObjectModel`], and the canonical byte encoding the registry
//!   digest hashes.
//! - [`registry`] — [`PolicyRegistry`]: an OPEN registry (engine builtins +
//!   consumer `with(...)` extensions) with an insertion-order-independent
//!   sha256 [`digest`](PolicyRegistry::digest).
//! - [`rule`] — [`Rule`] / [`RuleKind`] (the four scoped kinds), [`InjectSpec`],
//!   and the FIXED [`ValidatePredicate`] set.
//! - [`document`] — [`PolicyDoc`] + the strict TOML/JSON loader with every
//!   load-time legality gate (`deny_unknown_fields`, registry-validated,
//!   fail-closed, name-normalized).
//!
//! Composition (`compose_strict`/`compose_clamp`), `EffectivePolicy`, and seal
//! verification are Phase 1b-ii — NOT in this cut.

pub mod document;
pub mod knob;
pub mod registry;
pub mod rule;
pub mod scope;
pub mod value_order;

pub use document::{LoadContext, LoadError, LoadWarning, PolicyDoc, SUPPORTED_POLICY_VERSION};
pub use knob::{
    Enforcement, KnobDef, KnobKey, KnobKeyError, KnobKind, KnobValue, KnobValueError, ObjectModel,
    Polarity,
};
pub use registry::{PolicyRegistry, RegistryError};
pub use rule::{
    AuthorPkPolicy, InjectColumn, InjectIndex, InjectSpec, NameGlob, Rule, RuleKind,
    ValidatePredicate,
};
pub use scope::{
    glob::SegGlob,
    normalize_pg_identifier,
    pattern::{ObjectName, Pattern},
    Difference, Scope, ScopeError,
};
pub use value_order::{join_value, leq_value, meet_value, ValueOrderError};
