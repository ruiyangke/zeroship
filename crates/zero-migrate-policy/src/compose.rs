//! The composition algebra (II.3.2) — the SECURITY CROWN JEWEL. This module turns
//! validated [`PolicyDoc`] layers into an UNFORGEABLE [`EffectivePolicy`] through
//! two operators, and exposes the decision-query API the guard/engine (the PEP)
//! call. All scope resolution and value ordering live here; the PEP holds no
//! `Scope` and runs no lattice op (II.1).
//!
//! # The two lattices
//!
//! Composition rides on two independent lattices, both proven by oracles:
//! - the **scope lattice** (`crate::scope`) — `⊑`/`⊓`/`⊔`/`∖` over object sets;
//! - the **value order** (`crate::value_order`) — `⊑_value`/`⊔_value`/`⊓_value`
//!   per `KnobKind`.
//!
//! # The grant model is POINTWISE and SYMBOLIC (II.3.2)
//!
//! A grant for key `k` is a partial function `Object → Value`, NOT a scope plus a
//! scalar. We represent it symbolically as the set of `(effective_scope, value)`
//! grant rules on `k`; the value at an object `o` is
//!
//! ```text
//! value(P, k, o) = ⨆_value { r.value : r ∈ grants(k), o ∈ Objects(r.scope) }
//!                  (the LOOSEST covering value; the knob default if none covers o)
//! ```
//!
//! and `grantedScope(P,k) = ⊔ { r.scope : r ∈ grants(k), r.value ≠ default }`. We
//! never enumerate objects: the `compose_strict` grant check partitions the draft's
//! granted scope by the ceiling's covering rules (via `⊓`) and compares values on
//! each region, plus the uncovered region via `∖` (where the ceiling value is
//! `default`). See [`compose_strict`].
//!
//! # `compose_strict` vs `compose_clamp`
//!
//! - [`compose_strict`] ingests an UNTRUSTED draft against a trusted ceiling: grants
//!   must be pointwise `⊑`; require/inject/validate union-up; collisions blamed on
//!   the draft; the creatable-scope lint runs. NO clipping — strict reject.
//! - [`compose_clamp`] meets two TRUSTED ceilings (host→org→project): grants meet
//!   pointwise; rule-sets union; ceiling-vs-ceiling collisions are a loud operator
//!   error. Associative + total. Chains compose with clamp; the final untrusted
//!   draft lands via `compose_strict`.

use std::collections::BTreeMap;

use crate::knob::{KnobDef, KnobKey, KnobKind, KnobValue};
use crate::registry::PolicyRegistry;
use crate::rule::{InjectSpec, Rule, RuleKind, ValidatePredicate};
use crate::scope::pattern::ObjectName;
use crate::scope::{Difference, Scope};
use crate::value_order::{join_value, leq_value, meet_value, ValueOrderError};
use crate::{LoadContext, LoadError, PolicyDoc};

/// The knob key of the scoped creation-gating grant that anchors mandatory injects
/// (II.2.6a). The creatable-scope lint requires this grant's scope `⊑` every
/// mandatory ceiling inject's scope.
const CREATE_TABLE_KEY: &str = "core.create_table";

// ══════════════════════════════════════════════════════════════════════════════
// RootCeiling — the ONLY trust anchor
// ══════════════════════════════════════════════════════════════════════════════

/// The host's ROOT CEILING — the single trust anchor of the whole policy system
/// (II.5). Constructed only from a [`PolicyDoc`] loaded with
/// [`LoadContext::RootCeiling`] (the only layer allowed `mandatory = true` injects).
/// Every [`EffectivePolicy`] descends from a `RootCeiling` through
/// [`compose_strict`]/[`compose_clamp`]; there is no other way in.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RootCeiling {
    doc: PolicyDoc,
}

impl RootCeiling {
    /// Parse + validate a root-ceiling policy document from TOML. This is a thin
    /// wrapper over [`PolicyDoc::parse_toml`] pinned to [`LoadContext::RootCeiling`]
    /// — the only context under which a `mandatory` inject rule loads.
    pub fn parse_toml(src: &str, registry: &PolicyRegistry) -> Result<Self, LoadError> {
        Ok(Self { doc: PolicyDoc::parse_toml(src, registry, LoadContext::RootCeiling)? })
    }

    /// Parse + validate a root-ceiling policy document from JSON.
    pub fn parse_json(src: &str, registry: &PolicyRegistry) -> Result<Self, LoadError> {
        Ok(Self { doc: PolicyDoc::parse_json(src, registry, LoadContext::RootCeiling)? })
    }

    /// The underlying validated document (for the composer).
    #[must_use]
    pub fn doc(&self) -> &PolicyDoc {
        &self.doc
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Composition errors
// ══════════════════════════════════════════════════════════════════════════════

/// A `compose_strict` / `compose_clamp` failure. `compose_strict` errors blame the
/// UNTRUSTED draft; `compose_clamp` errors are loud OPERATOR misconfigurations of
/// two trusted ceilings.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ComposeError {
    /// A draft grant on `key` is LOOSER than the ceiling permits somewhere in the
    /// draft's granted scope — the value-blind / scope-blind escalation guard
    /// (II.3.2). `offending_pattern` renders the region the draft over-grants over.
    GrantExceedsCeiling {
        /// The knob key whose grant escalated.
        key: KnobKey,
        /// A human-readable render of the offending object region (the uncovered
        /// region, or a covered region where `draft_value ⋢ ceiling_value`).
        offending_pattern: String,
    },
    /// The draft's granted scope for `key` reaches OUTSIDE the ceiling's granted
    /// scope, and that uncovered region is not cleanly representable by `∖`
    /// (II.3.1) — the sanctioned fail-closed conservative deny.
    UncoveredRegionNotRepresentable {
        /// The knob key whose uncovered region could not be represented.
        key: KnobKey,
    },
    /// A draft `Inject` collides with a ceiling `Inject` on scope-overlapping
    /// objects (same column name, conflicting PK pin, `author_primary_key`
    /// Allow-vs-Forbid, or same index name) — blamed on the draft (II.4.4).
    DraftInjectCollidesCeiling {
        /// What collided (column/index name, or the pinned-PK conflict).
        detail: String,
    },
    /// A draft `Validate` (`ForbiddenColumns` / `TableNameForbidden`) contradicts a
    /// ceiling `Inject` on overlapping scope (II.4.4).
    DraftValidateContradictsCeilingInject {
        /// What contradicts (the forbidden column / table name vs the injected one).
        detail: String,
    },
    /// The draft's `core.create_table` granted scope is NOT `⊑` a mandatory ceiling
    /// inject's scope — a tenant could create an in-scope table escaping the
    /// mandatory injection (II.2.6a).
    CreatableEscapesMandatoryInject {
        /// A render of the mandatory inject scope the creatable grant escaped.
        inject_scope: String,
    },
    /// Two TRUSTED ceilings inject conflicting shape on overlapping scope — a loud
    /// operator misconfiguration surfaced at `compose_clamp` (II.4.4).
    CeilingInjectCollision {
        /// What collided across the two ceilings.
        detail: String,
    },
    /// A knob referenced by a rule is absent from the registry the composer was
    /// given, or a value's runtime shape does not match its knob's kind. The loader
    /// makes both unreachable for a document loaded against the same registry; this
    /// exists so composition is total and fail-closed.
    RegistryOrValueMismatch {
        /// Diagnostic detail.
        detail: String,
    },
}

impl From<ValueOrderError> for ComposeError {
    fn from(e: ValueOrderError) -> Self {
        ComposeError::RegistryOrValueMismatch { detail: format!("{e:?}") }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// The pointwise grant map
// ══════════════════════════════════════════════════════════════════════════════

/// One resolved grant rule: a scope and the value it grants there. `value ≠ default`
/// is NOT assumed here (the loader keeps default-valued and dead rules); the
/// `granted_scope`/`value_at` accessors filter appropriately.
#[derive(Clone, PartialEq, Eq, Debug)]
struct GrantRule {
    scope: Scope,
    value: KnobValue,
}

/// The pointwise grant model for ONE key (II.3.2): the symbolic set of covering
/// grant rules + the knob's default (the tightest value, the value everywhere no
/// rule covers). Never enumerates objects.
#[derive(Clone, PartialEq, Eq, Debug)]
#[doc(hidden)]
pub struct GrantKeyMap {
    kind: KnobKind,
    default: KnobValue,
    rules: Vec<GrantRule>,
}

impl GrantKeyMap {
    /// `value(P, k, o)` — the LOOSEST covering rule's value, else the default.
    fn value_at(&self, o: &ObjectName) -> Result<KnobValue, ComposeError> {
        let mut acc = self.default.clone();
        for r in &self.rules {
            if r.scope.objects_membership(o) {
                acc = join_value(&self.kind, &acc, &r.value)?;
            }
        }
        Ok(acc)
    }

    /// `grantedScope(P, k)` — the `⊔` of the scopes of exactly the rules that raise
    /// the value ABOVE default (a default-valued rule contributes nothing). This is
    /// precisely the region where `value ≠ default` (II.3.2).
    fn granted_scope(&self) -> Result<Scope, ComposeError> {
        let mut acc = Scope::Nothing;
        for r in &self.rules {
            // A rule whose value equals default does not raise the grant.
            if leq_value(&self.kind, &r.value, &self.default)? {
                // value ⊑ default ⟺ value == default (default is the tightest), so
                // this rule grants nothing above default — skip it.
                continue;
            }
            acc = acc.join(&r.scope);
        }
        Ok(acc)
    }
}

/// The full pointwise grant model: one [`GrantKeyMap`] per key that has grant rules.
/// A key with no grant rules is absent (its value is the knob default everywhere).
///
/// Exposed (hidden) because the sealed [`Ceiling`] trait returns it; not part of the
/// stable API — construct/inspect only through the composition entry points.
#[derive(Clone, PartialEq, Eq, Debug)]
#[doc(hidden)]
pub struct GrantModel {
    keys: BTreeMap<KnobKey, GrantKeyMap>,
}

impl GrantModel {
    /// Build the pointwise grant model from a rule list against the registry.
    fn build(rules: &[Rule], registry: &PolicyRegistry) -> Result<Self, ComposeError> {
        let mut keys: BTreeMap<KnobKey, GrantKeyMap> = BTreeMap::new();
        for rule in rules {
            let RuleKind::Grant { key, value } = &rule.kind else { continue };
            let def = lookup(registry, key)?;
            let entry = keys.entry(key.clone()).or_insert_with(|| GrantKeyMap {
                kind: def.kind.clone(),
                default: def.default.clone(),
                rules: Vec::new(),
            });
            entry.rules.push(GrantRule { scope: rule.scope.clone(), value: value.clone() });
        }
        Ok(Self { keys })
    }

    /// The keys present in either model (for iterating both ceiling and draft).
    fn key_union<'a>(&'a self, other: &'a GrantModel) -> Vec<&'a KnobKey> {
        let mut out: Vec<&KnobKey> = self.keys.keys().chain(other.keys.keys()).collect();
        out.sort();
        out.dedup();
        out
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// EffectivePolicy — UNFORGEABLE
// ══════════════════════════════════════════════════════════════════════════════

/// The composed, UNFORGEABLE effective policy — the PDP surface the guard/engine
/// query (II.3.2). Its fields are PRIVATE, it has NO `Deserialize` and NO public
/// constructor: the ONLY ways to obtain one are [`compose_strict`] / [`compose_clamp`]
/// from a [`RootCeiling`] (or [`EffectivePolicy::deny_all`], the engine-derived
/// floor). Holding one is proof it was composed under the host's root ceiling — this
/// is the type-level boundary that replaces the old forgeable `OperatorCapability`
/// token (E6).
///
/// It carries:
/// - the pointwise grant model (per key, the covering rules + default);
/// - the require/inject/validate rules, each at its resolved scope, injects in the
///   SEALED cross-layer inject total order (outermost-first, doc-order) (II.4.4);
/// - the registry (for kind/default lookups and the seal's registry digest).
///
/// All scope resolution lives HERE; the guard passes concrete [`ObjectName`]s and
/// receives values/obligations.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EffectivePolicy {
    registry: PolicyRegistry,
    grants: GrantModel,
    /// Require rules (obligations), each at its resolved scope, in composition order.
    requires: Vec<Rule>,
    /// Inject rules in the sealed total order (outermost-first, doc-order).
    injects: Vec<Rule>,
    /// Validate rules, each at its resolved scope, in composition order.
    validates: Vec<Rule>,
}

impl EffectivePolicy {
    /// The engine-derived FLOOR (II.7): the empty rule list — every Grant at its
    /// default-deny, no Require/Inject/Validate rules. A host that wants a fallback
    /// opts into this explicitly; it grants nothing and injects nothing. This is the
    /// one non-compose constructor, and it is still unforgeable content (it produces
    /// an all-default policy, never an escalated one).
    #[must_use]
    pub fn deny_all(registry: &PolicyRegistry) -> Self {
        Self {
            registry: registry.clone(),
            grants: GrantModel { keys: BTreeMap::new() },
            requires: Vec::new(),
            injects: Vec::new(),
            validates: Vec::new(),
        }
    }

    // ── decision-query API (the PDP surface the guard/engine call) ─────────────

    /// `grants(key, object)` — the effective (loosest covering) grant value at
    /// `object`, or `None` if the key is unknown to the registry. For a key with no
    /// covering grant rule this returns the knob's default (the tightest value), NOT
    /// `None`: `None` means "no such knob", a default value means "granted nothing
    /// above deny here". All scope resolution happens inside.
    #[must_use]
    pub fn grants(&self, key: &KnobKey, object: &ObjectName) -> Option<KnobValue> {
        match self.grants.keys.get(key) {
            Some(km) => km.value_at(object).ok(),
            // Key not in the grant model: fall back to the registry default (deny).
            None => self.registry.get(key).map(|d| d.default.clone()),
        }
    }

    /// `obligations(object)` — every covering require rule's `(key, value)` at
    /// `object`, in composition order. The guard unions valued requires per object.
    #[must_use]
    pub fn obligations(&self, object: &ObjectName) -> Vec<(KnobKey, KnobValue)> {
        self.requires
            .iter()
            .filter(|r| r.scope.objects_membership(object))
            .filter_map(|r| match &r.kind {
                RuleKind::Require { key, value } => Some((key.clone(), value.clone())),
                _ => None,
            })
            .collect()
    }

    /// `injects_for(object)` — every covering inject rule's [`InjectSpec`] at
    /// `object`, in the SEALED cross-layer inject total order (II.4.4).
    #[must_use]
    pub fn injects_for(&self, object: &ObjectName) -> Vec<&InjectSpec> {
        self.injects
            .iter()
            .filter(|r| r.scope.objects_membership(object))
            .filter_map(|r| match &r.kind {
                RuleKind::Inject { spec } => Some(spec),
                _ => None,
            })
            .collect()
    }

    /// `validates_for(object)` — every covering validate predicate at `object`, in
    /// composition order.
    #[must_use]
    pub fn validates_for(&self, object: &ObjectName) -> Vec<&ValidatePredicate> {
        self.validates
            .iter()
            .filter(|r| r.scope.objects_membership(object))
            .filter_map(|r| match &r.kind {
                RuleKind::Validate { pred } => Some(pred),
                _ => None,
            })
            .collect()
    }

    /// `is_injected_shape(object, element)` — is `element` (a column name, an index
    /// name, or the primary-key constraint) contributed by SOME covering inject rule
    /// at `object`? Name-match-at-op-time (II.2.6b): normalize + gather covering
    /// injects + test whether a covering `InjectSpec` contributes a name-matching
    /// element. Provenance is not tracked and cannot be spoofed.
    ///
    /// **Scope:** this is the NAME-MATCH immutability decision (whom to forbid
    /// altering). The full-`InjectSpec`-conformance re-evaluation on rename-into
    /// (II.2.6b H3) is the engine/guard's job over the IR — not this pure query.
    #[must_use]
    pub fn is_injected_shape(&self, object: &ObjectName, element: &ShapeElement) -> bool {
        for spec in self.injects_for(object) {
            match element {
                ShapeElement::Column(name) => {
                    if spec.columns.iter().any(|c| names_match(&c.name, name)) {
                        return true;
                    }
                }
                ShapeElement::Index(name) => {
                    if spec.indexes.iter().any(|i| names_match(&i.name, name)) {
                        return true;
                    }
                }
                ShapeElement::PrimaryKey => {
                    if spec.primary_key.is_some() {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// The registry this policy was composed under (for the seal's registry digest).
    #[must_use]
    pub fn registry(&self) -> &PolicyRegistry {
        &self.registry
    }

    // ── seal support (crate-internal; consumed by the seal module) ─────────────
    // These expose the canonical resolved rule set the seal MACs. Kept here (not in
    // the seal module) because they read private fields; the seal cut is their sole
    // consumer.

    /// The require rules in composition order (seal payload).
    pub(crate) fn require_rules(&self) -> &[Rule] {
        &self.requires
    }

    /// The inject rules in the sealed total order (seal payload).
    pub(crate) fn inject_rules(&self) -> &[Rule] {
        &self.injects
    }

    /// The validate rules in composition order (seal payload).
    pub(crate) fn validate_rules(&self) -> &[Rule] {
        &self.validates
    }

    /// The grant rules flattened to `(key, scope, value)` in key-sorted, then
    /// rule-order, form (seal payload). Key order is deterministic (BTreeMap); within
    /// a key, rules keep composition order.
    pub(crate) fn grant_rules_canonical(&self) -> Vec<(&KnobKey, &Scope, &KnobValue)> {
        let mut out = Vec::new();
        for (key, km) in &self.grants.keys {
            for r in &km.rules {
                out.push((key, &r.scope, &r.value));
            }
        }
        out
    }
}

/// A shape element the guard asks about for injected-shape immutability (II.2.6b).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ShapeElement<'a> {
    /// A column by (un-normalized) name.
    Column(&'a str),
    /// An index by (un-normalized) name.
    Index(&'a str),
    /// The table's primary-key constraint.
    PrimaryKey,
}

// ══════════════════════════════════════════════════════════════════════════════
// compose_strict — UNTRUSTED draft against a trusted ceiling
// ══════════════════════════════════════════════════════════════════════════════

mod sealed {
    /// Seals [`super::Ceiling`]: only the two in-crate types implement it, so no
    /// external crate can forge a ceiling by implementing the trait.
    pub trait Sealed {}
    impl Sealed for super::RootCeiling {}
    impl Sealed for super::EffectivePolicy {}
}

/// A trusted ceiling: either a [`RootCeiling`] or an already-composed
/// [`EffectivePolicy`] (the output of a `compose_clamp` chain). Both expose the same
/// resolved rule set + grant model to `compose_strict`. SEALED — the only ceilings
/// are the two trust-anchored types this crate defines; no external impl can forge
/// one. The methods are hidden implementation detail.
pub trait Ceiling: sealed::Sealed {
    #[doc(hidden)]
    fn grant_model(&self, registry: &PolicyRegistry) -> Result<GrantModel, ComposeError>;
    #[doc(hidden)]
    fn ceiling_requires(&self) -> Vec<Rule>;
    #[doc(hidden)]
    fn ceiling_injects(&self) -> Vec<Rule>;
    #[doc(hidden)]
    fn ceiling_validates(&self) -> Vec<Rule>;
}

impl Ceiling for RootCeiling {
    fn grant_model(&self, registry: &PolicyRegistry) -> Result<GrantModel, ComposeError> {
        GrantModel::build(&self.doc.rules, registry)
    }
    fn ceiling_requires(&self) -> Vec<Rule> {
        rules_of(&self.doc.rules, |k| matches!(k, RuleKind::Require { .. }))
    }
    fn ceiling_injects(&self) -> Vec<Rule> {
        rules_of(&self.doc.rules, |k| matches!(k, RuleKind::Inject { .. }))
    }
    fn ceiling_validates(&self) -> Vec<Rule> {
        rules_of(&self.doc.rules, |k| matches!(k, RuleKind::Validate { .. }))
    }
}

impl Ceiling for EffectivePolicy {
    fn grant_model(&self, _registry: &PolicyRegistry) -> Result<GrantModel, ComposeError> {
        Ok(self.grants.clone())
    }
    fn ceiling_requires(&self) -> Vec<Rule> {
        self.requires.clone()
    }
    fn ceiling_injects(&self) -> Vec<Rule> {
        self.injects.clone()
    }
    fn ceiling_validates(&self) -> Vec<Rule> {
        self.validates.clone()
    }
}

/// Ingress of an UNTRUSTED `draft` against a trusted `ceiling` (II.3.2). Returns an
/// [`EffectivePolicy`] iff the draft escalates nowhere; otherwise a [`ComposeError`]
/// blaming the draft. NO clipping — strict reject only.
///
/// The algorithm, pointwise per (key, object) but decided SYMBOLICALLY:
/// - **Grants:** for each key, the draft is admissible iff at every object in the
///   draft's granted scope the draft value is `⊑` the ceiling value. Decided by
///   (1) partitioning the draft's granted scope by each ceiling grant rule's scope
///   (via `⊓`) and comparing loosest values on the overlap; and (2) computing the
///   UNCOVERED region `grantedScope(draft,k) ∖ grantedScope(ceiling,k)` — where the
///   ceiling value is `default` (tightest), so any non-default draft value there
///   rejects; a not-representable `∖` fails closed.
/// - **Require/Inject/Validate:** union-up (ceiling rules ∪ draft rules), each at its
///   own scope; a ceiling rule is never dropped or narrowed. Injects keep the sealed
///   total order: ceiling injects first (outermost), then draft injects.
/// - **Collisions:** a draft inject colliding with a ceiling inject, or a draft
///   validate contradicting a ceiling inject, rejects at compose time.
/// - **Creation-gating lint:** the draft's `core.create_table` granted scope must be
///   `⊑` every MANDATORY ceiling inject's scope.
pub fn compose_strict(
    ceiling: &impl Ceiling,
    draft: &PolicyDoc,
    registry: &PolicyRegistry,
) -> Result<EffectivePolicy, ComposeError> {
    let ceiling_grants = ceiling.grant_model(registry)?;
    let draft_grants = GrantModel::build(&draft.rules, registry)?;

    // ── grant check: pointwise draft ⊑ ceiling, per key ────────────────────────
    for key in draft_grants.key_union(&ceiling_grants) {
        check_grant_key(key, &draft_grants, &ceiling_grants)?;
    }

    // ── require/inject/validate union-up (ceiling ∪ draft) ─────────────────────
    let ceiling_injects = ceiling.ceiling_injects();
    let draft_injects = rules_of(&draft.rules, |k| matches!(k, RuleKind::Inject { .. }));

    // Compose-time collision blame: draft inject vs ceiling inject.
    check_inject_collisions(&ceiling_injects, &draft_injects, false)?;
    // Draft validate contradicting a ceiling inject.
    let draft_validates = rules_of(&draft.rules, |k| matches!(k, RuleKind::Validate { .. }));
    check_validate_vs_inject(&ceiling_injects, &draft_validates)?;

    // Sealed inject total order: ceiling (outermost) first, then draft (inward).
    let mut injects = ceiling_injects.clone();
    injects.extend(draft_injects);

    let mut requires = ceiling.ceiling_requires();
    requires.extend(rules_of(&draft.rules, |k| matches!(k, RuleKind::Require { .. })));

    let mut validates = ceiling.ceiling_validates();
    validates.extend(draft_validates);

    // ── creation-gating lint: draft creatable ⊑ every mandatory ceiling inject ──
    check_creatable_lint(&draft_grants, &ceiling_grants, &ceiling_injects)?;

    Ok(EffectivePolicy { registry: registry.clone(), grants: draft_grants, requires, injects, validates })
}

/// The per-key grant admissibility check (II.3.2): draft value `⊑` ceiling value at
/// every object in the draft's granted scope.
fn check_grant_key(
    key: &KnobKey,
    draft: &GrantModel,
    ceiling: &GrantModel,
) -> Result<(), ComposeError> {
    let Some(dk) = draft.keys.get(key) else {
        return Ok(()); // draft grants nothing on this key → nothing to check.
    };
    let draft_granted = dk.granted_scope()?;
    if matches!(draft_granted, Scope::Nothing) {
        return Ok(()); // draft raises nothing above default → admissible.
    }

    // The ceiling's model for this key (may be absent → everywhere default).
    let ceiling_km = ceiling.keys.get(key);
    let ceiling_granted = match ceiling_km {
        Some(ck) => ck.granted_scope()?,
        None => Scope::Nothing,
    };

    // (1) UNCOVERED region: draft grants where the ceiling grants only default.
    //     Any non-default draft value there escalates → reject (unless it computes
    //     empty). `∖` over-approximates-or-rejects (fail-closed).
    match draft_granted.difference(&ceiling_granted) {
        Difference::NotRepresentable => {
            return Err(ComposeError::UncoveredRegionNotRepresentable { key: key.clone() });
        }
        Difference::Scope(uncovered) => {
            if !matches!(uncovered, Scope::Nothing) {
                // The ceiling value on the uncovered region is `default` (tightest).
                // The draft's granted-scope is exactly where its value ≠ default, so
                // any non-empty uncovered region carries a draft value ⋣ default =
                // escalation. (There is no draft_value ⊑ default case here: a rule in
                // granted_scope() has value strictly above default by construction.)
                return Err(ComposeError::GrantExceedsCeiling {
                    key: key.clone(),
                    offending_pattern: render_scope(&uncovered),
                });
            }
        }
    }

    // (2) COVERED region: partition the draft's granted scope by each ceiling grant
    //     rule's scope (via ⊓) and compare loosest values on the overlap. We compare
    //     value(draft, k, ·) ⊑ value(ceiling, k, ·) at a WITNESS of each region.
    //
    // Because the grant maps are symbolic, we check the value relation over the meet
    // of the draft's granted scope with each ceiling rule's scope, at a canonical
    // witness object of that non-empty meet. This is sound: value() is CONSTANT
    // across a region carved by the ⊓ of all covering rule scopes (any two objects a
    // region-meet contains are covered by exactly the same set of rules → same
    // loosest value), and the oracle exhaustively confirms the region check against
    // pointwise ground truth.
    let Some(ck) = ceiling_km else {
        // Ceiling grants nothing above default anywhere, yet the draft's granted
        // scope is entirely inside ceiling_granted (Nothing) — impossible unless
        // draft_granted is Nothing, handled above. If we reach here the uncovered
        // check already accounted for the whole draft_granted; nothing covered.
        return Ok(());
    };
    // The regions to compare: draft granted ⊓ (each ceiling grant rule's scope).
    for crule in &ck.rules {
        let region = draft_granted.meet(&crule.scope);
        if matches!(region, Scope::Nothing) {
            continue;
        }
        let Some(witness) = witness_of(&region) else {
            // A non-empty region we cannot witness concretely: fail closed.
            return Err(ComposeError::UncoveredRegionNotRepresentable { key: key.clone() });
        };
        let dv = dk.value_at(&witness)?;
        let cv = ck.value_at(&witness)?;
        if !leq_value(&dk.kind, &dv, &cv)? {
            return Err(ComposeError::GrantExceedsCeiling {
                key: key.clone(),
                offending_pattern: render_scope(&region),
            });
        }
    }
    Ok(())
}

/// The creatable-scope lint (II.2.6a): the draft-composed `core.create_table`
/// granted scope must be `⊑` every MANDATORY ceiling inject's scope. Only the ceiling
/// carries mandatory injects (loader-enforced), so we read them from the ceiling.
fn check_creatable_lint(
    draft: &GrantModel,
    _ceiling: &GrantModel,
    ceiling_injects: &[Rule],
) -> Result<(), ComposeError> {
    let create_key = KnobKey::parse(CREATE_TABLE_KEY).ok();
    let Some(create_key) = create_key else { return Ok(()) };
    // The effective creatable scope is the DRAFT's granted scope for create_table
    // (grants compose downward; the draft is the tightest layer for its own grants,
    // and compose_strict already proved draft ⊑ ceiling on this key).
    let creatable = match draft.keys.get(&create_key) {
        Some(km) => km.granted_scope()?,
        None => Scope::Nothing,
    };
    if matches!(creatable, Scope::Nothing) {
        return Ok(()); // creates nothing → cannot escape any inject.
    }
    for rule in ceiling_injects {
        let RuleKind::Inject { spec } = &rule.kind else { continue };
        if !spec.mandatory {
            continue;
        }
        // creatable ⊑ scope(I). `⊑` is sound (a false only conservative-rejects).
        if !creatable.subset(&rule.scope) {
            return Err(ComposeError::CreatableEscapesMandatoryInject {
                inject_scope: render_scope(&rule.scope),
            });
        }
    }
    Ok(())
}

// ══════════════════════════════════════════════════════════════════════════════
// compose_clamp — meet of two TRUSTED ceilings
// ══════════════════════════════════════════════════════════════════════════════

/// A clamped ceiling — the meet of two TRUSTED ceilings (II.3.2). It is an
/// [`EffectivePolicy`] (the same resolved shape), obtained only through
/// [`compose_clamp`]; the alias documents intent at chain sites (host→org→project).
pub type ClampedCeiling = EffectivePolicy;

/// Meet two TRUSTED ceilings pointwise (II.3.2), associative + total. Grants meet
/// pointwise (per key: scope `⊓`, value `⊓_value` at overlap); require/inject/validate
/// rule-sets UNION (each at its own scope); a ceiling-vs-ceiling inject collision is a
/// loud operator error. The inject total order is `outer` first (outermost), then
/// `inner` — concatenation, which is associative so re-chaining is order-stable
/// (II.4.4).
pub fn compose_clamp(
    outer: &impl Ceiling,
    inner: &impl Ceiling,
    registry: &PolicyRegistry,
) -> Result<ClampedCeiling, ComposeError> {
    let og = outer.grant_model(registry)?;
    let ig = inner.grant_model(registry)?;

    // ── grants: pointwise meet, represented as rule-scope ⊓ products ───────────
    let mut clamped_grants = GrantModel { keys: BTreeMap::new() };
    for key in og.key_union(&ig) {
        let km = clamp_grant_key(key, &og, &ig, registry)?;
        if !km.rules.is_empty() {
            clamped_grants.keys.insert(key.clone(), km);
        }
    }

    // ── inject collision (ceiling vs ceiling) → loud operator error ────────────
    let outer_injects = outer.ceiling_injects();
    let inner_injects = inner.ceiling_injects();
    check_inject_collisions(&outer_injects, &inner_injects, true)?;

    // ── require/inject/validate: UNION, each at its own scope ───────────────────
    let mut injects = outer_injects;
    injects.extend(inner_injects);
    let mut requires = outer.ceiling_requires();
    requires.extend(inner.ceiling_requires());
    let mut validates = outer.ceiling_validates();
    validates.extend(inner.ceiling_validates());

    Ok(EffectivePolicy {
        registry: registry.clone(),
        grants: clamped_grants,
        requires,
        injects,
        validates,
    })
}

/// Per-key grant clamp (II.3.2): the pointwise meet, materialized as the finite set of
/// grant rules obtained by intersecting each outer rule's scope with each inner rule's
/// scope (via `⊓`) and meeting their values; empty-scope products drop out.
///
/// A key present in only ONE ceiling meets against the OTHER's default (deny): the
/// meet of any value with the tightest default is the default → the clamped grant is
/// empty on that key. So a one-sided key contributes NO grant rules (correct: the
/// clamp of "granted" with "not granted" is "not granted").
fn clamp_grant_key(
    key: &KnobKey,
    outer: &GrantModel,
    inner: &GrantModel,
    registry: &PolicyRegistry,
) -> Result<GrantKeyMap, ComposeError> {
    let def = lookup(registry, key)?;
    let empty = GrantKeyMap { kind: def.kind.clone(), default: def.default.clone(), rules: Vec::new() };
    let (Some(ok), Some(ik)) = (outer.keys.get(key), inner.keys.get(key)) else {
        // One-sided: meet with the other side's default (deny) ⇒ empty grant.
        return Ok(empty);
    };
    let mut rules = Vec::new();
    for orule in &ok.rules {
        for irule in &ik.rules {
            let scope = orule.scope.meet(&irule.scope);
            if matches!(scope, Scope::Nothing) {
                continue;
            }
            let value = meet_value(&def.kind, &orule.value, &irule.value)?;
            rules.push(GrantRule { scope, value });
        }
    }
    Ok(GrantKeyMap { kind: def.kind.clone(), default: def.default.clone(), rules })
}

// ══════════════════════════════════════════════════════════════════════════════
// collision detection (compose-time blame)
// ══════════════════════════════════════════════════════════════════════════════

/// Compose-time inject-collision check. A left (ceiling/outer) inject colliding with a
/// right (draft/inner) inject on scope-overlapping objects — same column name,
/// conflicting `primary_key` pin, `author_primary_key` Allow-vs-Forbid, or same index
/// name — is a collision. `ceiling_vs_ceiling` selects the error variant (loud
/// operator error vs draft blame).
fn check_inject_collisions(
    left: &[Rule],
    right: &[Rule],
    ceiling_vs_ceiling: bool,
) -> Result<(), ComposeError> {
    for l in left {
        let RuleKind::Inject { spec: ls } = &l.kind else { continue };
        for r in right {
            let RuleKind::Inject { spec: rs } = &r.kind else { continue };
            // Only overlapping scopes can collide.
            if matches!(l.scope.meet(&r.scope), Scope::Nothing) {
                continue;
            }
            if let Some(detail) = inject_specs_collide(ls, rs) {
                return Err(if ceiling_vs_ceiling {
                    ComposeError::CeilingInjectCollision { detail }
                } else {
                    ComposeError::DraftInjectCollidesCeiling { detail }
                });
            }
        }
    }
    Ok(())
}

/// Do two [`InjectSpec`]s collide? Returns the offending detail, or `None`.
///
/// A collision is a CONFLICT, not a mere co-occurrence: two injects that contribute
/// the SAME column identically are not a conflict at the leaf level (the resolver's
/// total order lays them out; identical duplicates are benign). We flag:
/// - a shared column name with DIVERGENT type/nullable/default (same-name, same-shape
///   is benign);
/// - conflicting PK pins (both pin, but to different column lists);
/// - `author_primary_key` Allow-vs-Forbid when BOTH pin a PK (a genuine policy
///   conflict on the same anchored PK);
/// - a shared index name with a DIVERGENT column list.
fn inject_specs_collide(a: &InjectSpec, b: &InjectSpec) -> Option<String> {
    // Column-name collision with divergent shape.
    for ca in &a.columns {
        for cb in &b.columns {
            if names_match(&ca.name, &cb.name)
                && (ca.ty != cb.ty || ca.nullable != cb.nullable || ca.default != cb.default)
            {
                return Some(format!("column `{}` injected with divergent shape", ca.name));
            }
        }
    }
    // Index-name collision with divergent columns.
    for ia in &a.indexes {
        for ib in &b.indexes {
            if names_match(&ia.name, &ib.name) && ia.columns != ib.columns {
                return Some(format!("index `{}` injected with divergent columns", ia.name));
            }
        }
    }
    // Conflicting PK pins.
    if let (Some(pa), Some(pb)) = (&a.primary_key, &b.primary_key) {
        if pa != pb {
            return Some(format!("primary key pinned to divergent columns {pa:?} vs {pb:?}"));
        }
        // Both pin the SAME PK but disagree on author-PK policy → conflict.
        if a.author_primary_key != b.author_primary_key {
            return Some("primary key pinned with divergent author_primary_key policy".to_string());
        }
    }
    None
}

/// A draft `Validate` (`ForbiddenColumns` / `TableNameForbidden`) contradicting a
/// ceiling `Inject` on overlapping scope (II.4.4): a forbidden column naming an
/// injected column, or a forbidden table-name pattern matching would make the
/// ceiling-injected table unsatisfiable. We flag the concrete column contradiction
/// (name-match); table-name contradiction is left to resolve time (injects carry no
/// table name here), matching the loader's single-doc gate.
fn check_validate_vs_inject(
    ceiling_injects: &[Rule],
    draft_validates: &[Rule],
) -> Result<(), ComposeError> {
    for inj in ceiling_injects {
        let RuleKind::Inject { spec } = &inj.kind else { continue };
        for val in draft_validates {
            let RuleKind::Validate { pred } = &val.kind else { continue };
            if matches!(inj.scope.meet(&val.scope), Scope::Nothing) {
                continue;
            }
            if let ValidatePredicate::ForbiddenColumns { names } = pred {
                for col in &spec.columns {
                    for forbidden in names {
                        if names_match(&col.name, forbidden) {
                            return Err(ComposeError::DraftValidateContradictsCeilingInject {
                                detail: format!(
                                    "draft forbids column `{}` a ceiling inject contributes",
                                    col.name
                                ),
                            });
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

// ══════════════════════════════════════════════════════════════════════════════
// helpers
// ══════════════════════════════════════════════════════════════════════════════

/// Look up a knob def, mapping a miss to a fail-closed compose error.
fn lookup<'r>(registry: &'r PolicyRegistry, key: &KnobKey) -> Result<&'r KnobDef, ComposeError> {
    registry
        .get(key)
        .ok_or_else(|| ComposeError::RegistryOrValueMismatch { detail: format!("unknown knob {key}") })
}

/// Clone the rules matching a kind predicate, preserving document order.
fn rules_of(rules: &[Rule], pred: impl Fn(&RuleKind) -> bool) -> Vec<Rule> {
    rules.iter().filter(|r| pred(&r.kind)).cloned().collect()
}

/// Name-match two identifiers under the II.2.7 fold: normalize both to canonical
/// bytes and compare. Used for inject/validate/injected-shape name matching so a
/// case-fold or quoted-dot trick cannot slip a name past the check.
fn names_match(a: &str, b: &str) -> bool {
    fold(a) == fold(b)
}

/// The II.2.7 single-identifier fold (lowercase unquoted, verbatim quoted), reused
/// from the loader's identifier normalizer.
fn fold(s: &str) -> Vec<u8> {
    crate::scope::normalize_pg_identifier(s)
        .map(|o| o.schema)
        .unwrap_or_else(|| s.to_ascii_lowercase().into_bytes())
}

/// Render a scope for diagnostics (a stable, human-readable form of the pattern set).
fn render_scope(s: &Scope) -> String {
    match s {
        Scope::Nothing => "∅".to_string(),
        Scope::All => "*.*".to_string(),
        Scope::Of { include, exclude } => {
            let inc: Vec<String> = include.iter().map(render_pattern).collect();
            if exclude.is_empty() {
                inc.join(",")
            } else {
                let exc: Vec<String> = exclude.iter().map(render_pattern).collect();
                format!("{} \\ {}", inc.join(","), exc.join(","))
            }
        }
    }
}

fn render_pattern(p: &crate::Pattern) -> String {
    format!("{}.{}", p.schema.render(), p.table.render())
}

/// A concrete WITNESS object of a non-empty scope: some `ObjectName` the scope
/// denotes. Used by the covered-region value comparison — value() is constant across
/// a region carved by ⊓, so one witness decides the whole region. Returns `None` for
/// `Nothing` (or an un-witnessable proper scope, treated fail-closed by the caller).
fn witness_of(s: &Scope) -> Option<ObjectName> {
    match s {
        Scope::Nothing => None,
        Scope::All => Some(ObjectName::schema(b"a".to_vec())),
        Scope::Of { include, exclude } => {
            // Try each include pattern's canonical witness; accept the first the whole
            // scope actually admits (not carved by an exclude).
            for p in include {
                let cand = pattern_witness(p);
                if s.objects_membership(&cand) {
                    return Some(cand);
                }
                // Also try a table witness under the schema, in case the schema
                // object itself is excluded but tables under it survive.
                let tcand = ObjectName::table(cand.schema.clone(), b"a".to_vec());
                if s.objects_membership(&tcand) {
                    return Some(tcand);
                }
            }
            let _ = exclude;
            None
        }
    }
}

/// A canonical concrete object a pattern denotes: the schema/table globs filled with
/// a fixed byte (`a`), stars → `a`. This is a WITNESS constructor, not a matcher.
fn pattern_witness(p: &crate::Pattern) -> ObjectName {
    let schema = seg_witness(&p.schema);
    if p.table.is_star() {
        ObjectName::schema(schema)
    } else {
        ObjectName::table(schema, seg_witness(&p.table))
    }
}

/// A concrete segment a glob matches: its literal if a literal; else prefix·suffix
/// (which the glob always matches — `p*s` matches `ps`).
fn seg_witness(g: &crate::SegGlob) -> Vec<u8> {
    // render() gives `p*s` (or the literal). Replace a single `*` with nothing so the
    // witness is `ps`, which every infix/prefix/suffix/star glob matches at its floor.
    let rendered = g.render();
    let w: Vec<u8> = rendered.bytes().filter(|&b| b != b'*').collect();
    // A pure `*` glob renders to `*` → empty witness; use `a` for a non-empty name.
    if w.is_empty() {
        b"a".to_vec()
    } else {
        w
    }
}
