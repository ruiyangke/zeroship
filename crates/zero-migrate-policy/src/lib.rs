//! `zero-migrate-policy` — the Policy Decision Point (PDP): the *mechanism* half
//! of the zero-migrate policy system.
//!
//! This crate is a true LEAF: pure data + algebra, zero I/O, **no SQL deps**, no
//! dependency on the engine. It ships the policy *machine*; every piece of policy
//! *content* is injected by the consumer.
//!
//! # The scope lattice
//!
//! The first module is [`scope`]: the security-core primitive. `Scope { Nothing |
//! All | Of{include, exclude} }` over one/two-segment identifier globs, with the
//! full lattice — `normalize`, `⊑` (subset), `⊓` (meet), `⊔` (join), `∖`
//! (difference) — verified EXHAUSTIVELY by a brute-force oracle
//! (`scope::oracle`, `#[cfg(test)]`) against a direct ground-truth matcher. The
//! oracle is the correctness proof: where prose review of the glob algebra could
//! not be trusted, the oracle-green code is authoritative.

//! # The knob / rule / document model
//!
//! On top of the scope lattice the crate layers the policy *content model* and
//! its strict loader:
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
//! # The composition algebra, unforgeable `EffectivePolicy`, and seal
//!
//! On top of the value order sits the SECURITY CROWN JEWEL:
//!
//! - [`value_order`] — the per-knob VALUE lattice (`⊑_value`/`⊔_value`/`⊓_value`),
//!   derived from each [`knob::KnobKind`] so no facet-specific meet can drift.
//! - [`compose`] — [`RootCharter`] (the only trust anchor), [`restrict`] (meet of two
//!   trusted charters — associative, total), and the UNFORGEABLE [`EffectivePolicy`]
//!   with its decision-query API
//!   (`grants`/`obligations`/`injects_for`/`validates_for`/`is_injected_shape`). All
//!   scope resolution lives here; the guard holds no `Scope`.
//! - [`boundary`] — [`admit`] (untrusted-draft ingress: pointwise `draft ⊑ charter`
//!   grants + union-up require/inject/validate + compose-time collision blame + the
//!   creatable-scope lint), the SOLE untrusted trust-boundary crossing, deliberately
//!   apart from the trusted combinators.
//! - [`mod@seal`] - [`SealedPolicy`]: an HMAC over the canonical resolved rule set (in
//!   the sealed inject total order) ‖ registry digest ‖ `(dialect, matcher_version)`
//!   ‖ `charter_version`, plus a nonce. [`SealedPolicy::verify`] HARD-FAILS on any
//!   tamper or binding mismatch.
//!
//! The pointwise-grant admissibility check is proven by a brute-force COMPOSITION
//! ORACLE (`tests/compose_oracle.rs`): `admit` is `Ok` IFF the draft is
//! pointwise `⊑` the charter at every object and key. Where prose review of the
//! escalation check could not be trusted, the oracle-green code is authoritative.

pub mod boundary;
pub mod compose;
pub mod document;
pub mod knob;
pub mod registry;
pub mod rule;
pub mod scope;
pub mod seal;
pub mod value_order;

pub use boundary::admit;
pub use compose::{
    finalize_charter, overlay, restrict, AdmitCharter, AssembledCharter, Charter, ComposeError,
    EffectivePolicy, FinalizeError, GrantRegion, RootCharter, ShapeElement, TrustedDoc,
};
pub use document::{
    LoadContext, LoadError, LoadWarning, PolicyDoc, ProfileCatalog, SUPPORTED_POLICY_VERSION,
};
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
pub use seal::{seal, SealError, SealedPolicy};
pub use value_order::{join_value, leq_value, meet_value, ValueOrderError};
