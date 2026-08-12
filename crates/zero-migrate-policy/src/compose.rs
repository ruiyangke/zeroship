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
//! never enumerate objects: the `admit` grant check partitions the draft's
//! granted scope by the charter's covering rules (via `⊓`) and compares values on
//! each region, plus the uncovered region via `∖` (where the charter value is
//! `default`). See [`crate::boundary::admit`].
//!
//! # `admit` vs `restrict`
//!
//! - [`crate::boundary::admit`] ingests an UNTRUSTED draft against a trusted charter:
//!   grants must be pointwise `⊑`; require/inject/validate union-up; collisions blamed
//!   on the draft; the creatable-scope lint runs. NO clipping — strict reject.
//! - [`restrict`] meets two TRUSTED charters (host→org→project): grants meet
//!   pointwise; rule-sets union; charter-vs-charter collisions are a loud operator
//!   error. Associative + total. Chains compose with `restrict`; the final untrusted
//!   draft lands via `admit`.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::knob::{KnobDef, KnobKey, KnobKind, KnobValue};
use crate::registry::PolicyRegistry;
use crate::rule::{InjectSpec, Rule, RuleKind, ValidatePredicate};
use crate::scope::pattern::ObjectName;
use crate::scope::{Difference, Scope};
use crate::value_order::{join_value, leq_value, meet_value, ValueOrderError};
use crate::{LoadContext, LoadError, PolicyDoc};

/// The knob key of the scoped creation-gating grant that anchors mandatory injects
/// (II.2.6a). The creatable-scope lint requires this grant's scope `⊑` every
/// mandatory charter inject's scope.
pub(crate) const CREATE_TABLE_KEY: &str = "schema.create_table";

// ══════════════════════════════════════════════════════════════════════════════
// TrustedDoc — provenance newtype (H-1)
// ══════════════════════════════════════════════════════════════════════════════

/// A policy document of HOST provenance (H-1). Constructible ONLY inside this crate,
/// from a host-injected source — the [`RootCharter`] and catalog entries registered
/// at engine construction. There is **no** `PolicyDoc -> TrustedDoc` conversion and
/// **no** public constructor: a creator-supplied `PolicyDoc` (a *draft*) can never
/// become a `TrustedDoc`.
///
/// This is the type-level encoding that keeps [`overlay`] /
/// [`restrict`] from ever seeing an untrusted operand: both accept only `TrustedDoc`.
/// The `extends`-laundering hole (a draft inheriting a trusted base into `overlay`)
/// is closed by construction, not by a runtime provenance check.
///
/// The single public mint site outside `RootCharter::parse_*` is
/// [`TrustedDoc::register_catalog_entry`] — the `ProfileCatalog`-registration wrapper
/// a host uses to turn a document IT authored into a catalog `TrustedDoc`. Passing a
/// creator-editable file here would defeat H-1; that is construction-site discipline
/// (the doc IS host-authored) the type cannot police, but the mint site is named so
/// no one wires an untrusted source into it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TrustedDoc(PolicyDoc);

impl TrustedDoc {
    /// Mint a catalog-entry `TrustedDoc` from a document the HOST authored
    /// (`ProfileCatalog` registration, II.5). Parses + validates as a NON-root layer
    /// (a catalog `env` fragment may not carry `mandatory = true` injects — that is
    /// `RootCharter`-only, M-5). The caller MUST supply a host-authored source; a
    /// creator-editable document wired here would launder untrusted content into the
    /// trusted combinators (H-1 guidance).
    pub fn register_catalog_entry(src: &str, registry: &PolicyRegistry) -> Result<Self, LoadError> {
        Ok(Self(PolicyDoc::parse_toml(
            src,
            registry,
            LoadContext::TrustedCatalogEntry,
        )?))
    }

    /// Mint a catalog-entry `TrustedDoc` from a host-authored JSON document.
    pub fn register_catalog_entry_json(
        src: &str,
        registry: &PolicyRegistry,
    ) -> Result<Self, LoadError> {
        Ok(Self(PolicyDoc::parse_json(
            src,
            registry,
            LoadContext::TrustedCatalogEntry,
        )?))
    }

    /// Mint a catalog-entry `TrustedDoc` that may `extends` a base, resolving it
    /// against the trusted `catalog` (II.7, H-1).
    pub fn register_catalog_entry_with_catalog(
        src: &str,
        registry: &PolicyRegistry,
        catalog: &dyn crate::document::ProfileCatalog,
    ) -> Result<Self, LoadError> {
        Ok(Self(PolicyDoc::parse_toml_with_catalog(
            src,
            registry,
            LoadContext::TrustedCatalogEntry,
            catalog,
        )?))
    }

    /// Crate-internal mint from an already-validated document (the `RootCharter`
    /// constructors, extends-resolution). No provenance is conferred by this call —
    /// the caller is responsible for the document being host-authored.
    pub(crate) fn from_validated(doc: PolicyDoc) -> Self {
        Self(doc)
    }

    /// The underlying validated document (crate-internal; the composer reads it).
    pub(crate) fn doc(&self) -> &PolicyDoc {
        &self.0
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// RootCharter — the ONLY trust anchor
// ══════════════════════════════════════════════════════════════════════════════

/// The host's ROOT CHARTER — the single trust anchor of the whole policy system
/// (II.5), and the outermost layer of every assembled charter (M-5). Wraps a
/// [`TrustedDoc`] loaded with [`LoadContext::RootCharter`] (the only layer allowed
/// `mandatory = true` injects). Every [`EffectivePolicy`] descends from a
/// `RootCharter` through the finalized-[`Charter`] path into [`admit`](crate::boundary::admit);
/// there is no other way in. A `RootCharter` is itself a valid finalized charter (a
/// single trusted layer whose conflicts are caught at load), so it implements
/// [`AdmitCharter`] and may be `admit`'s charter directly.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RootCharter {
    doc: TrustedDoc,
}

impl RootCharter {
    /// Parse + validate a root-charter policy document from TOML. This is a thin
    /// wrapper over [`PolicyDoc::parse_toml`] pinned to [`LoadContext::RootCharter`]
    /// — the only context under which a `mandatory` inject rule loads.
    pub fn parse_toml(src: &str, registry: &PolicyRegistry) -> Result<Self, LoadError> {
        let doc = PolicyDoc::parse_toml(src, registry, LoadContext::RootCharter)?;
        Ok(Self {
            doc: TrustedDoc::from_validated(doc),
        })
    }

    /// Parse + validate a root-charter policy document from JSON.
    pub fn parse_json(src: &str, registry: &PolicyRegistry) -> Result<Self, LoadError> {
        let doc = PolicyDoc::parse_json(src, registry, LoadContext::RootCharter)?;
        Ok(Self {
            doc: TrustedDoc::from_validated(doc),
        })
    }

    /// Parse + validate a root charter that may `extends` a trusted catalog base
    /// (II.7, H-1), resolving the base chain against `catalog` with cycle detection.
    pub fn parse_toml_with_catalog(
        src: &str,
        registry: &PolicyRegistry,
        catalog: &dyn crate::document::ProfileCatalog,
    ) -> Result<Self, LoadError> {
        let doc =
            PolicyDoc::parse_toml_with_catalog(src, registry, LoadContext::RootCharter, catalog)?;
        Ok(Self {
            doc: TrustedDoc::from_validated(doc),
        })
    }

    /// The underlying validated document (for the composer).
    #[must_use]
    pub fn doc(&self) -> &PolicyDoc {
        self.doc.doc()
    }

    /// This root as a [`TrustedDoc`] — so it may be an `overlay`/`restrict` operand
    /// (the root is `base`, the outermost layer, M-5).
    #[must_use]
    pub fn as_trusted(&self) -> &TrustedDoc {
        &self.doc
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Composition errors
// ══════════════════════════════════════════════════════════════════════════════

/// A `admit` / `restrict` failure. `admit` errors blame the
/// UNTRUSTED draft; `restrict` errors are loud OPERATOR misconfigurations of
/// two trusted charters.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ComposeError {
    /// A draft grant on `key` is LOOSER than the charter permits somewhere in the
    /// draft's granted scope — the value-blind / scope-blind escalation guard
    /// (II.3.2). `offending_pattern` renders the region the draft over-grants over.
    GrantExceedsCharter {
        /// The knob key whose grant escalated.
        key: KnobKey,
        /// A human-readable render of the offending object region (the uncovered
        /// region, or a covered region where `draft_value ⋢ charter_value`).
        offending_pattern: String,
    },
    /// A region `admit` had to reason about for `key` is not cleanly representable by
    /// `∖` (II.3.1) — the sanctioned fail-closed conservative deny.
    ///
    /// TWO causes reach here, and the variant deliberately does not distinguish them,
    /// because the whole point is that the region could not be named:
    ///
    /// - the draft's grant reaches OUTSIDE everything the charter raises, and the
    ///   difference has no glob form (`All ∖ app_*` is the common one); or
    /// - the charter DOES cover the draft, and the algebra could not prove it -
    ///   subtracting the scopes that would discharge the obligation was not
    ///   representable.
    ///
    /// The second is a FALSE REJECT of a policy a perfect checker would admit. That is
    /// the accepted price of never sampling a witness: an unprovable region denies
    /// rather than passes. A charter that trips this and looks legitimate is a report
    /// worth making, not a policy worth loosening the check for.
    UncoveredRegionNotRepresentable {
        /// The knob key whose region could not be represented.
        key: KnobKey,
    },
    /// A draft `Inject` collides with a charter `Inject` on scope-overlapping
    /// objects (same column name, conflicting PK pin, `author_primary_key`
    /// Allow-vs-Forbid, or same index name) — blamed on the draft (II.4.4).
    DraftInjectCollidesCharter {
        /// What collided (column/index name, or the pinned-PK conflict).
        detail: String,
    },
    /// A draft `Validate` (`ForbiddenColumns` / `TableNameForbidden`) contradicts a
    /// charter `Inject` on overlapping scope (II.4.4).
    DraftValidateContradictsCharterInject {
        /// What contradicts (the forbidden column / table name vs the injected one).
        detail: String,
    },
    /// The draft's `core.create_table` granted scope is NOT `⊑` a mandatory charter
    /// inject's scope — a tenant could create an in-scope table escaping the
    /// mandatory injection (II.2.6a).
    CreatableEscapesMandatoryInject {
        /// A render of the mandatory inject scope the creatable grant escaped.
        inject_scope: String,
    },
    /// Two TRUSTED charters inject conflicting shape on overlapping scope — a loud
    /// operator misconfiguration surfaced at `restrict` (II.4.4).
    CharterInjectCollision {
        /// What collided across the two charters.
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
        ComposeError::RegistryOrValueMismatch {
            detail: format!("{e:?}"),
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// The pointwise grant map
// ══════════════════════════════════════════════════════════════════════════════

/// One resolved grant rule: a scope and the value it grants there. `value ≠ default`
/// is NOT assumed here (the loader keeps default-valued and dead rules); the
/// `granted_scope`/`value_at` accessors filter appropriately.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct GrantRule {
    pub(crate) scope: Scope,
    pub(crate) value: KnobValue,
}

/// The pointwise grant model for ONE key (II.3.2): the symbolic set of covering
/// grant rules + the knob's default (the tightest value, the value everywhere no
/// rule covers). Never enumerates objects.
#[derive(Clone, PartialEq, Eq, Debug)]
#[doc(hidden)]
pub struct GrantKeyMap {
    pub(crate) kind: KnobKind,
    pub(crate) default: KnobValue,
    pub(crate) rules: Vec<GrantRule>,
}

impl GrantKeyMap {
    /// `value(P, k, o)` — the LOOSEST covering rule's value, else the default.
    pub(crate) fn value_at(&self, o: &ObjectName) -> Result<KnobValue, ComposeError> {
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
    pub(crate) fn granted_scope(&self) -> Result<Scope, ComposeError> {
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

    /// `coveredScope(P, k)` — the `⊔` of the scopes of ALL grant rules on this key,
    /// regardless of value (presence, incl. default-valued rules) (II.3.2 (a)). This
    /// is the OVERRIDE/MASKING set: where this layer's value WINS over an inherited
    /// one, even when it narrows the knob DOWN to its default.
    pub(crate) fn covered_scope(&self) -> Scope {
        let mut acc = Scope::Nothing;
        for r in &self.rules {
            acc = acc.join(&r.scope);
        }
        acc
    }

    /// `P.covers(k, o)` — does ANY grant rule on this key cover `o` (presence, any
    /// value)? The masking test: a layer's value WINS at `o` iff it covers `o`.
    pub(crate) fn covers(&self, o: &ObjectName) -> bool {
        self.rules.iter().any(|r| r.scope.objects_membership(o))
    }
}

/// The full pointwise grant model: one [`GrantKeyMap`] per key that has grant rules.
/// A key with no grant rules is absent (its value is the knob default everywhere).
///
/// Exposed (hidden) because the sealed [`Charter`] trait returns it; not part of the
/// stable API — construct/inspect only through the composition entry points.
#[derive(Clone, PartialEq, Eq, Debug)]
#[doc(hidden)]
pub struct GrantModel {
    pub(crate) keys: BTreeMap<KnobKey, GrantKeyMap>,
}

impl GrantModel {
    /// Build the pointwise grant model from a rule list against the registry.
    pub(crate) fn build(rules: &[Rule], registry: &PolicyRegistry) -> Result<Self, ComposeError> {
        let mut keys: BTreeMap<KnobKey, GrantKeyMap> = BTreeMap::new();
        for rule in rules {
            let RuleKind::Grant { key, value } = &rule.kind else {
                continue;
            };
            let def = lookup(registry, key)?;
            let entry = keys.entry(key.clone()).or_insert_with(|| GrantKeyMap {
                kind: def.kind.clone(),
                default: def.default.clone(),
                rules: Vec::new(),
            });
            entry.rules.push(GrantRule {
                scope: rule.scope.clone(),
                value: value.clone(),
            });
        }
        Ok(Self { keys })
    }

    /// The keys present in either model (for iterating both charter and draft).
    pub(crate) fn key_union<'a>(&'a self, other: &'a GrantModel) -> Vec<&'a KnobKey> {
        let mut out: Vec<&KnobKey> = self.keys.keys().chain(other.keys.keys()).collect();
        out.sort();
        out.dedup();
        out
    }

    /// The keys that have grant rules in this model (key-sorted).
    pub(crate) fn keys(&self) -> impl Iterator<Item = &KnobKey> {
        self.keys.keys()
    }

    /// The per-key grant map for `key`, if any.
    pub(crate) fn get(&self, key: &KnobKey) -> Option<&GrantKeyMap> {
        self.keys.get(key)
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Layer — one resolved policy layer (H-4: the layered charter / effective policy)
// ══════════════════════════════════════════════════════════════════════════════

/// Which layer a resolved rule set occupies in a composed stack (H-4). The tag is
/// bound into the seal (II.7) so two stacks with the same *flattened* rule set — but
/// different layer boundaries — encode differently and a re-flatten fails the MAC.
///
/// The cross-layer order, outermost → innermost, is `Base → Env → Draft`. `restrict`
/// flattens a trusted meet into ONE `Base` layer; `overlay` produces `[Env] over
/// [Base]`; `admit` places the untrusted `[Draft]` innermost over the charter layers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LayerTag {
    /// The outermost trusted layer — a `RootCharter`/`restrict`-meet/`overlay` base.
    Base,
    /// A trusted `env` overlay layer (from [`overlay`]`(base, env)`), inside `Base`.
    Env,
    /// The innermost, UNTRUSTED creator draft layer (`admit`).
    Draft,
}

/// One resolved policy layer: its tag + the four resolved rule collections. A
/// [`Charter`]/[`AssembledCharter`]/[`EffectivePolicy`] is a STACK of these, queried
/// top-first (innermost-first) with fall-through (II.3.2 / H-4).
///
/// Exposed (hidden) because the sealed [`AdmitCharter`] trait returns it; not part of
/// the stable API — construct/inspect only through the composition entry points.
#[derive(Clone, PartialEq, Eq, Debug)]
#[doc(hidden)]
pub struct Layer {
    pub(crate) tag: LayerTag,
    pub(crate) grants: GrantModel,
    pub(crate) requires: Vec<Rule>,
    pub(crate) injects: Vec<Rule>,
    pub(crate) validates: Vec<Rule>,
}

impl Layer {
    /// Build a layer from a validated document's rule list.
    pub(crate) fn from_doc(
        tag: LayerTag,
        doc: &PolicyDoc,
        registry: &PolicyRegistry,
    ) -> Result<Self, ComposeError> {
        Ok(Self {
            tag,
            grants: GrantModel::build(&doc.rules, registry)?,
            requires: rules_of(&doc.rules, |k| matches!(k, RuleKind::Require { .. })),
            injects: rules_of(&doc.rules, |k| matches!(k, RuleKind::Inject { .. })),
            validates: rules_of(&doc.rules, |k| matches!(k, RuleKind::Validate { .. })),
        })
    }
}

/// Pin `key` in `layer` so a SILENT region gets the knob DEFAULT, not an inherited
/// charter value — by appending a synthetic default-valued grant rule over the whole
/// universe (`Scope::All`) to the layer's grant map for `key`. Presence-override
/// (II.3.2) then makes this layer WIN the top-down grant fall-through EVERYWHERE for
/// `key`: at an object the draft covered with an EXPLICIT rule, the loosest-covering
/// join within the layer keeps the draft's own (higher) value; at an object the draft
/// left silent, only the synthetic default-valued rule covers, so the value is the
/// knob default — the charter's grant below is never seen. Used by
/// [`admit`](crate::boundary::admit) to realize a `KnobDef.inherit == false` power
/// grant: it is conferred only where the draft ASKED, never by inheritance-by-omission.
/// `pub(crate)`: the boundary module is its sole caller.
pub(crate) fn pin_layer_key_to_default(
    layer: &mut Layer,
    key: &KnobKey,
    kind: &KnobKind,
    default: &KnobValue,
) {
    let entry = layer
        .grants
        .keys
        .entry(key.clone())
        .or_insert_with(|| GrantKeyMap {
            kind: kind.clone(),
            default: default.clone(),
            rules: Vec::new(),
        });
    entry.rules.push(GrantRule {
        scope: Scope::All,
        value: default.clone(),
    });
}

// ══════════════════════════════════════════════════════════════════════════════
// EffectivePolicy — UNFORGEABLE
// ══════════════════════════════════════════════════════════════════════════════

/// The composed, UNFORGEABLE effective policy — the PDP surface the guard/engine
/// query (II.3.2). Its fields are PRIVATE, it has NO `Deserialize` and NO public
/// constructor: the ONLY ways to obtain one are [`crate::admit`] / [`restrict`]
/// from a [`RootCharter`] (or [`EffectivePolicy::deny_all`], the engine-derived
/// floor). Holding one is proof it was composed under the host's root charter — this
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
    /// The layer stack, TOP (innermost) first — `[draft]` over the charter layers
    /// (`[env] over [base]` or a single flattened `restrict`/root `base`) (H-4). The
    /// grant query falls through top→down; obligations/injects/validates union across
    /// ALL layers (union-up). The seal binds the layer boundaries (II.7).
    layers: Vec<Layer>,
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
            layers: Vec::new(),
        }
    }

    /// Refuse a composed policy that can create tables outside a mandatory inject's
    /// scope, so the mandatory column cannot be escaped by creating elsewhere.
    ///
    /// The same question [`finalize_charter`] asks, asked of a policy composed the way
    /// the product composes one. It had only the charter-algebra caller, and nothing
    /// ships that, so a charter granting `schema.create_table` over everything beside a
    /// mandatory inject scoped to one prefix composed clean.
    ///
    /// # Errors
    /// [`ComposeError::CreatableEscapesMandatoryInject`], naming the inject scope the
    /// creatable grant escaped.
    pub fn check_creatable_escape(&self) -> Result<(), ComposeError> {
        creatable_escape_scope(&self.layers).map_or(Ok(()), |inject_scope| {
            Err(ComposeError::CreatableEscapesMandatoryInject { inject_scope })
        })
    }

    /// Refuse a composed policy carrying two conflicting injects in one layer.
    ///
    /// Two same-scope injects contributing the same column with different shapes are
    /// not a lint: the resolver contributes BOTH, so the table is built carrying that
    /// column twice and no database will create it. Refusing here names the two injects
    /// instead of failing later against a live database with a duplicate-column error.
    ///
    /// # Errors
    /// [`ComposeError::CharterInjectCollision`], describing the collision.
    pub fn check_same_layer_inject_conflicts(&self) -> Result<(), ComposeError> {
        same_layer_inject_conflict(&self.layers).map_or(Ok(()), |detail| {
            Err(ComposeError::CharterInjectCollision { detail })
        })
    }

    /// Crate-internal constructor from a LAYER STACK (top-first). Used by the
    /// `boundary::admit` trust-crossing to mint the layered `[draft] over [charter]`
    /// [`EffectivePolicy`] (H-4). Not public: an `EffectivePolicy` is only obtainable
    /// through the composition entry points (unforgeability, E6).
    #[must_use]
    pub(crate) fn from_layers(registry: PolicyRegistry, layers: Vec<Layer>) -> Self {
        Self { registry, layers }
    }

    // ── decision-query API (the PDP surface the guard/engine call) ─────────────

    /// `grants(key, object)` — the effective grant value at `object`, or `None` if the
    /// key is unknown to the registry. LAYERED (H-4): query the TOP layer's covering
    /// rules first (loosest-covering WITHIN that layer); if the top layer has ANY grant
    /// rule on `key` covering `object` (presence — even one at the default), its value
    /// WINS; otherwise fall through to the next layer down. The knob default (tightest)
    /// if no layer covers `object`. `None` means "no such knob"; a default value means
    /// "granted nothing above deny here". All scope resolution happens inside.
    #[must_use]
    pub fn grants(&self, key: &KnobKey, object: &ObjectName) -> Option<KnobValue> {
        // Fall through the layer stack top→down; the first layer that COVERS `object`
        // for `key` (presence) decides the value (its loosest-covering value there).
        for layer in &self.layers {
            if let Some(km) = layer.grants.keys.get(key) {
                if km.covers(object) {
                    return km.value_at(object).ok();
                }
            }
        }
        // No layer covers `object` for `key`: the registry default (deny).
        self.registry.get(key).map(|d| d.default.clone())
    }

    /// `grant_is_top(key)` — is `key` granted (above its default) over the WHOLE
    /// universe (`Scope::All`, ⊤)? True iff the region where `value ≠ default` is
    /// `Scope::All`. This is the distinction the guard's II.2.5 scoped-raw-SQL rules
    /// hinge on: an unqualified name / `SET search_path` / opaque body is admitted
    /// **only** under a ⊤-scoped `core.raw_sql` grant; a merely-scoped grant (any
    /// `Of{…}` region) is "non-⊤" and refuses them. `false` for an unknown key, a key
    /// with no grant above default, or a strictly-narrower-than-⊤ grant.
    #[must_use]
    pub fn grant_is_top(&self, key: &KnobKey) -> bool {
        matches!(self.grant_region(key), GrantRegion::Top)
    }

    /// `grant_region(key)` — the shape of the EFFECTIVE region where `key` is granted
    /// above its default: `Ungranted` (nowhere / unknown key), `Top` (the whole
    /// universe, ⊤), or `Scoped` (a proper, strictly-narrower-than-⊤ region). This is
    /// the three-way distinction the guard's II.2.5 scoped-raw-SQL rules turn on:
    /// unqualified names / `SET search_path` / opaque bodies are admitted under a `Top`
    /// `core.raw_sql` grant, DENIED under a `Scoped` one, and simply not raw-SQL-gated
    /// at all when `Ungranted`.
    ///
    /// The effective granted region is derived from the LAYERED value (H-4): it is
    /// `All` only when the effective value is above default over the whole universe.
    /// Because `grants` falls through, an effective grant can differ from any single
    /// layer's — a narrow-to-default draft over a ⊤ charter grant narrows the region.
    /// We compute it as the join of each layer's effective-above-default contribution,
    /// masked by the layers above (presence-override), which is a `⊒`-conservative
    /// estimate — sound for the ⊤ test (a false `Top` cannot arise: `Top` is only
    /// reported when the effective grant truly covers the universe).
    #[must_use]
    pub fn grant_region(&self, key: &KnobKey) -> GrantRegion {
        match self.effective_granted_scope(key) {
            // `Top` is a claim of universality, so it may only be answered from an
            // EXACT region. An estimate that had to widen is, by construction, a
            // grant with a hole punched in it, which is precisely not universal.
            Ok((Scope::All, Exactness::Exact)) => GrantRegion::Top,
            Ok((Scope::All, Exactness::Widened)) => GrantRegion::Scoped,
            Ok((Scope::Nothing, _)) => GrantRegion::Ungranted,
            Ok((Scope::Of { .. }, _)) => GrantRegion::Scoped,
            Err(_) => GrantRegion::Ungranted,
        }
    }

    /// The EFFECTIVE granted scope for `key` across the layer stack (the object set
    /// where the effective, fall-through value is above default). Computed
    /// layer-by-layer: an upper layer's granted region is its own `grantedScope`; a
    /// lower layer contributes its `grantedScope` MINUS the region already COVERED
    /// (presence) by any layer above it — a lower grant only shows through where no
    /// upper layer masks it. Joins (⊒-conservative) the per-layer contributions.
    fn effective_granted_scope(&self, key: &KnobKey) -> Result<(Scope, Exactness), ComposeError> {
        let mut acc = Scope::Nothing;
        let mut exactness = Exactness::Exact;
        // The union of all higher layers' COVERED scope on this key (presence mask).
        let mut masked_above = Scope::Nothing;
        for layer in &self.layers {
            if let Some(km) = layer.grants.keys.get(key) {
                let granted = km.granted_scope()?;
                // This layer's grant shows through only where no higher layer covers.
                let visible = match granted.difference(&masked_above) {
                    Difference::Scope(s) => s,
                    // The complement of a glob is not a glob, so `All` minus a real
                    // mask has no representation. Fall back to the raw granted region
                    // and record that the estimate WIDENED: callers asking about
                    // universality must not read the widened value as proof of it.
                    Difference::NotRepresentable => {
                        exactness = Exactness::Widened;
                        granted.clone()
                    }
                };
                acc = acc.join(&visible);
                masked_above = masked_above.join(&km.covered_scope());
            }
        }
        Ok((acc, exactness))
    }

    /// Return the literal schema names included by non-default EFFECTIVE grant rules
    /// for `key`, when that granted region is representable as a finite set of
    /// schema-only literal includes with no excludes.
    ///
    /// This is intentionally narrow policy introspection for engine plumbing that
    /// still needs a schema pin/search path during the policy redesign. It gathers the
    /// non-default grant rules across ALL layers (a superset estimate: it does not
    /// subtract higher-layer masking, since the consumer only needs the candidate
    /// schema set). Returns `None` for an unknown key, an ungranted key, a top grant,
    /// globbed schemas, table-granular scopes, or excludes.
    #[must_use]
    pub fn grant_literal_schema_includes(&self, key: &KnobKey) -> Option<Vec<String>> {
        let mut out = BTreeSet::new();
        let mut seen_key = false;
        for layer in &self.layers {
            let Some(km) = layer.grants.keys.get(key) else {
                continue;
            };
            seen_key = true;
            for r in &km.rules {
                if leq_value(&km.kind, &r.value, &km.default).ok()? {
                    continue;
                }
                match &r.scope {
                    Scope::Nothing => {}
                    Scope::All => return None,
                    Scope::Of { include, exclude } => {
                        if !exclude.is_empty() {
                            return None;
                        }
                        for pattern in include {
                            if !pattern.table.is_star() || !pattern.schema.is_literal() {
                                return None;
                            }
                            out.insert(pattern.schema.render());
                        }
                    }
                }
            }
        }
        if !seen_key || out.is_empty() {
            None
        } else {
            Some(out.into_iter().collect())
        }
    }

    /// `obligations(object)` — every covering require rule's `(key, value)` at
    /// `object`, across ALL layers (union-up, II.3.2). The guard unions valued requires
    /// per object. Charter obligations are never masked by the draft — they accumulate.
    #[must_use]
    pub fn obligations(&self, object: &ObjectName) -> Vec<(KnobKey, KnobValue)> {
        self.layers
            .iter()
            .flat_map(|l| l.requires.iter())
            .filter(|r| r.scope.objects_membership(object))
            .filter_map(|r| match &r.kind {
                RuleKind::Require { key, value } => Some((key.clone(), value.clone())),
                _ => None,
            })
            .collect()
    }

    /// `obligation_values_anywhere(key)`: every value a require rule for `key`
    /// carries at a NON-EMPTY scope, anywhere in the layer stack.
    ///
    /// This answers "is this obligation authored at all", the question a PEP must ask
    /// when it holds no object to resolve at: an input it cannot attribute to a
    /// concrete object (raw SQL naming no relation, an unqualified name with no
    /// schema pin) is not provably outside an obligation, so it must fail closed,
    /// but only where an obligation exists to fail closed against. It is NOT a
    /// substitute for [`Self::obligations`]: it erases scope by construction, so a
    /// decision that HAS an object must resolve at that object instead.
    #[must_use]
    pub fn obligation_values_anywhere(&self, key: &KnobKey) -> Vec<KnobValue> {
        self.layers
            .iter()
            .flat_map(|l| l.requires.iter())
            .filter(|r| !matches!(r.scope, Scope::Nothing))
            .filter_map(|r| match &r.kind {
                RuleKind::Require { key: k, value } if k == key => Some(value.clone()),
                _ => None,
            })
            .collect()
    }

    /// `injects_for(object)` — every covering inject rule's [`InjectSpec`] at
    /// `object`, across ALL layers in the SEALED cross-layer inject total order
    /// (outermost/base first, innermost/draft last — union-up, II.4.4).
    #[must_use]
    pub fn injects_for(&self, object: &ObjectName) -> Vec<&InjectSpec> {
        // Outermost-first order: the stack is top(=innermost)-first, so reverse.
        self.layers
            .iter()
            .rev()
            .flat_map(|l| l.injects.iter())
            .filter(|r| r.scope.objects_membership(object))
            .filter_map(|r| match &r.kind {
                RuleKind::Inject { spec } => Some(spec),
                _ => None,
            })
            .collect()
    }

    /// `validates_for(object)` — every covering validate predicate at `object`, across
    /// ALL layers (union-up). A draft can add but never drop a charter predicate.
    #[must_use]
    pub fn validates_for(&self, object: &ObjectName) -> Vec<&ValidatePredicate> {
        self.layers
            .iter()
            .rev()
            .flat_map(|l| l.validates.iter())
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
    // These expose the canonical resolved rule set WITH ITS LAYER BOUNDARIES (H-4)
    // for the seal MAC. Kept here (not in the seal module) because they read private
    // fields; the seal cut is their sole consumer. The layer tag is bound into the
    // MAC so two stacks with the same flattened rule set but different layer
    // boundaries encode differently — a re-flatten fails the MAC (II.7).

    /// The seal payload: for each layer (in stack order, TOP/innermost first), its
    /// [`LayerTag`] paired with its four canonical rule collections. The seal encodes
    /// the layer boundaries so a re-flatten changes the MAC.
    pub(crate) fn seal_layers(&self) -> Vec<SealLayer<'_>> {
        self.layers
            .iter()
            .map(|l| SealLayer {
                tag: l.tag,
                grants: {
                    let mut out = Vec::new();
                    for (key, km) in &l.grants.keys {
                        for r in &km.rules {
                            out.push((key, &r.scope, &r.value));
                        }
                    }
                    out
                },
                requires: &l.requires,
                injects: &l.injects,
                validates: &l.validates,
            })
            .collect()
    }
}

/// One layer's seal payload (crate-internal): its tag + canonical rule views. The
/// seal MACs this in stack order so layer boundaries are bound (H-4 / II.7).
pub(crate) struct SealLayer<'a> {
    pub(crate) tag: LayerTag,
    /// Grant rules flattened `(key, scope, value)`, key-sorted then rule-ordered.
    pub(crate) grants: Vec<(&'a KnobKey, &'a Scope, &'a KnobValue)>,
    pub(crate) requires: &'a [Rule],
    pub(crate) injects: &'a [Rule],
    pub(crate) validates: &'a [Rule],
}

impl LayerTag {
    /// A stable 1-byte discriminant for the seal encoding.
    pub(crate) fn seal_byte(self) -> u8 {
        match self {
            LayerTag::Base => 0x00,
            LayerTag::Env => 0x01,
            LayerTag::Draft => 0x02,
        }
    }
}

// ── layered charter grant queries (crate-internal; consumed by boundary::admit) ──

/// Every grant rule scope of a LAYER STACK for `key` whose value RISES above default,
/// each at its own `effective_scope` (excludes intact) (H-4).
///
/// The grant-bearing subset, deliberately. `admit` subtracts these to find the region
/// no charter rule lifts, and a rule at or below default lifts nothing - counting it
/// would treat its region as charter-granted. It would also make `All ∖ <mask>`
/// unrepresentable and turn a precise escalation report into a fail-closed "not
/// representable", which is safe but says nothing useful about what the draft did
/// wrong.
///
/// Masking is a separate question and is answered separately: a rule at or below
/// default still DECIDES wherever it covers, because the layered query stops at the
/// first covering layer. `admit` reads each layer's own rules for that, rather than
/// asking this function, which is why nothing here needs to report a mask.
pub(crate) fn layered_nondefault_grant_rules<'a>(
    layers: &'a [Layer],
    key: &KnobKey,
) -> Result<Vec<&'a Scope>, ComposeError> {
    let mut out: Vec<&Scope> = Vec::new();
    for layer in layers {
        if let Some(km) = layer.grants.keys.get(key) {
            for r in &km.rules {
                if leq_value(&km.kind, &r.value, &km.default)? {
                    continue;
                }
                out.push(&r.scope);
            }
        }
    }
    Ok(out)
}

/// Whether a computed granted region is the exact set or a widened over-estimate.
///
/// A layer's visible grant is its granted scope minus the region higher layers
/// already cover, and that difference has no glob representation when a real mask
/// is subtracted from the whole universe. The estimate then widens, which is safe
/// for membership questions (it only ever admits more objects than are truly
/// granted, and callers re-check the concrete object) but NOT for universality: a
/// widened `All` is exactly the case where a hole was punched out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Exactness {
    /// Every layer's contribution subtracted cleanly.
    Exact,
    /// At least one contribution had to widen, so the region is an over-estimate.
    Widened,
}

/// The shape of the region where a grant key is above its default (II.2.5). The
/// guard's scoped-raw-SQL rules turn on the three-way `Ungranted`/`Top`/`Scoped`
/// distinction. See [`EffectivePolicy::grant_region`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GrantRegion {
    /// Granted nowhere above default (or unknown key).
    Ungranted,
    /// Granted over the whole universe (`Scope::All`, ⊤).
    Top,
    /// Granted over a proper, strictly-narrower-than-⊤ region.
    Scoped,
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
// AdmitCharter — the FINALIZED charters a draft may be admitted against
// ══════════════════════════════════════════════════════════════════════════════

mod sealed {
    /// Seals [`super::AdmitCharter`]: only the finalized in-crate charter types
    /// implement it, so no external crate can forge a charter — and, crucially, an
    /// [`super::AssembledCharter`] does NOT implement it, so an un-finalized charter
    /// cannot reach `admit` at the type level (MED).
    pub trait Sealed {}
    impl Sealed for super::RootCharter {}
    impl Sealed for super::Charter {}
    impl Sealed for super::EffectivePolicy {}
}

/// A FINALIZED charter a creator draft may be admitted against
/// (`boundary::admit`): a [`RootCharter`] (a single trusted layer, finalized by
/// load), a [`Charter`] (the output of [`finalize_charter`]), or an already-composed
/// [`EffectivePolicy`] used as a charter. Each yields its LAYER STACK (top/innermost
/// first) to `admit`, which prepends the untrusted draft over it (H-4).
///
/// SEALED. An [`AssembledCharter`] — the un-finalized output of [`overlay`] /
/// [`restrict`] — deliberately does **not** implement this trait: it must pass
/// [`finalize_charter`] first, so "un-finalized charter reaches admit" is a TYPE
/// ERROR, not a runtime hazard (MED).
///
/// # The `registry` argument is a precondition, not a choice
///
/// It must be digest-equal to the definition the charter was loaded under, and
/// nothing checks that - see [`admit`](crate::boundary::admit) for what a mismatch
/// buys an attacker. The three implementations do not treat the argument alike, which
/// is worth knowing before assuming a correct layer stack is enough:
///
/// - [`RootCharter`] RE-READS its stored document under whatever is passed, so the
///   layers themselves come out in the caller's vocabulary.
/// - [`Charter`] and [`EffectivePolicy`] IGNORE it and return layers already built at
///   their own load, so those layers stay correct.
///
/// The second case is not safe by omission: `admit` stores the registry it was given
/// on the policy it returns, so a mismatch still decides every later read even when
/// the layers were right.
pub trait AdmitCharter: sealed::Sealed {
    /// The charter's layer stack (top/innermost first), for `admit` to build
    /// `[draft] over [these layers]` (H-4).
    #[doc(hidden)]
    fn charter_layers(&self, registry: &PolicyRegistry) -> Result<Vec<Layer>, ComposeError>;
}

impl AdmitCharter for RootCharter {
    fn charter_layers(&self, registry: &PolicyRegistry) -> Result<Vec<Layer>, ComposeError> {
        // A root charter is a single `Base` layer.
        Ok(vec![Layer::from_doc(LayerTag::Base, self.doc(), registry)?])
    }
}

impl AdmitCharter for Charter {
    fn charter_layers(&self, _registry: &PolicyRegistry) -> Result<Vec<Layer>, ComposeError> {
        Ok(self.layers.clone())
    }
}

impl AdmitCharter for EffectivePolicy {
    fn charter_layers(&self, _registry: &PolicyRegistry) -> Result<Vec<Layer>, ComposeError> {
        Ok(self.layers.clone())
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// AssembledCharter / Charter — un-finalized vs finalized trusted charters (H-3)
// ══════════════════════════════════════════════════════════════════════════════

/// An UN-FINALIZED trusted charter — the output of the total trusted combinators
/// [`overlay`] and [`restrict`] (II.3.2). It is a LAYER STACK (top/innermost first)
/// that has NOT yet passed the conflict lints, so it deliberately does **not**
/// implement [`AdmitCharter`]: it cannot be `admit`'s charter until it clears
/// [`finalize_charter`], which returns a [`Charter`] (MED — the un-finalized/finalized
/// split is type-encoded). `overlay`/`restrict` are TOTAL (never reject); every
/// charter *misconfiguration* (colliding injects, PK conflicts, creatable-escape)
/// surfaces at the explicit `finalize_charter` gate (H-3).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AssembledCharter {
    registry: PolicyRegistry,
    layers: Vec<Layer>,
}

/// A FINALIZED trusted charter — an [`AssembledCharter`] (or a `RootCharter`) that has
/// PASSED [`finalize_charter`]'s conflict lints (H-3). It implements [`AdmitCharter`],
/// so it — and only it, never an [`AssembledCharter`] — may be `admit`'s charter
/// argument. It carries the same layer stack, now proven collision-free.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Charter {
    registry: PolicyRegistry,
    layers: Vec<Layer>,
}

impl Charter {
    /// The registry this charter was composed under.
    #[must_use]
    pub fn registry(&self) -> &PolicyRegistry {
        &self.registry
    }
}

/// The `finalize_charter` conflict-lint failure (H-3) — a loud TRUSTED-SIDE operator
/// misconfiguration of an assembled charter (NOT a trust crossing). Distinct from
/// [`ComposeError`] (the draft-blame / admit errors) so the two failure classes never
/// mix.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FinalizeError {
    /// Two charter inject rules add a column of the same normalized name (with a
    /// divergent shape) on overlapping scope.
    CharterInjectColumnConflict {
        /// What collided (the column name + detail).
        detail: String,
    },
    /// Two charter inject rules pin different primary keys, or conflicting
    /// `author_primary_key` policy, on overlapping scope.
    CharterPrimaryKeyConflict {
        /// The conflicting PK detail.
        detail: String,
    },
    /// Two charter inject rules use the same index name (with divergent columns) on
    /// overlapping scope.
    CharterIndexNameConflict {
        /// The conflicting index detail.
        detail: String,
    },
    /// A charter `Validate` (`ForbiddenColumns`) contradicts a charter `Inject` (an
    /// injected column can never satisfy the predicate) on overlapping scope.
    CharterInjectValidateContradiction {
        /// The contradiction detail.
        detail: String,
    },
    /// The charter's own `core.create_table` granted scope is NOT `⊑` a mandatory
    /// charter inject's scope — the charter would let an in-scope table be created
    /// escaping the mandatory injection (II.2.6a). Surfaced here (charter-side) so the
    /// misconfiguration is caught before any draft (MED).
    CreatableEscapesMandatoryInject {
        /// A render of the mandatory inject scope the creatable grant escaped.
        inject_scope: String,
    },
}

/// The charter-conflict gate (H-3). `overlay`/`restrict` are TOTAL so the algebra
/// stays lawful; a trusted charter they assemble can still contain a
/// *misconfiguration* (colliding injects, PK conflicts, a duplicate index name, an
/// inject-vs-validate self-contradiction, OR a creatable-escape). Those must surface
/// **loudly** before enforcement — so they live here, in one explicit, fallible step,
/// not inside the total combinators.
///
/// Every [`AssembledCharter`] MUST pass `finalize_charter` before it may be `admit`'s
/// `charter` argument — and because `admit`'s charter type is the finalized
/// [`Charter`] (or a `RootCharter`), the type system discharges the "already
/// collision-free" assumption. `finalize_charter` is idempotent and, being a pure
/// lint pass, does not change the rule set on success.
pub fn finalize_charter(assembled: AssembledCharter) -> Result<Charter, FinalizeError> {
    // Collect the charter's injects/validates/grants across all its layers. The lints
    // are over the WHOLE assembled rule set (all layer boundaries flattened for the
    // conflict scan; the layer boundaries are preserved for the query/seal).
    let injects: Vec<&Rule> = assembled
        .layers
        .iter()
        .flat_map(|l| l.injects.iter())
        .collect();
    let validates: Vec<&Rule> = assembled
        .layers
        .iter()
        .flat_map(|l| l.validates.iter())
        .collect();

    // (1) overlapping-scope inject/inject conflicts (column shape, PK pin, index name).
    for (i, a) in injects.iter().enumerate() {
        let RuleKind::Inject { spec: sa } = &a.kind else {
            continue;
        };
        for b in injects.iter().skip(i + 1) {
            let RuleKind::Inject { spec: sb } = &b.kind else {
                continue;
            };
            if matches!(a.scope.meet(&b.scope), Scope::Nothing) {
                continue;
            }
            if let Some(detail) = inject_column_conflict(sa, sb) {
                return Err(FinalizeError::CharterInjectColumnConflict { detail });
            }
            if let Some(detail) = inject_pk_conflict(sa, sb) {
                return Err(FinalizeError::CharterPrimaryKeyConflict { detail });
            }
            if let Some(detail) = inject_index_conflict(sa, sb) {
                return Err(FinalizeError::CharterIndexNameConflict { detail });
            }
        }
    }

    // (2) inject-vs-validate contradiction across the merged layers.
    for inj in &injects {
        let RuleKind::Inject { spec } = &inj.kind else {
            continue;
        };
        for val in &validates {
            let RuleKind::Validate { pred } = &val.kind else {
                continue;
            };
            if matches!(inj.scope.meet(&val.scope), Scope::Nothing) {
                continue;
            }
            if let ValidatePredicate::ForbiddenColumns { names } = pred {
                for col in &spec.columns {
                    for forbidden in names {
                        if names_match(&col.name, forbidden) {
                            return Err(FinalizeError::CharterInjectValidateContradiction {
                                detail: format!(
                                    "charter forbids column `{}` a charter inject contributes",
                                    col.name
                                ),
                            });
                        }
                    }
                }
            }
        }
    }

    // (3) creatable-escape: the charter's own create_table granted scope must be ⊑
    //     every MANDATORY charter inject's scope (MED — moved here from admit). Because
    //     admit later proves draft.create_table ⊑ charter.create_table, this charter-
    //     side bound transitively bounds every admitted draft's creatable region.
    check_charter_creatable_lint(&assembled)?;

    Ok(Charter {
        registry: assembled.registry,
        layers: assembled.layers,
    })
}

/// The charter-side creatable-escape lint (II.2.6a, run at finalize). The charter's
/// EFFECTIVE `core.create_table` granted scope (across its layers) must be `⊑` every
/// mandatory charter inject's scope.
fn check_charter_creatable_lint(assembled: &AssembledCharter) -> Result<(), FinalizeError> {
    creatable_escape_scope(&assembled.layers).map_or(Ok(()), |inject_scope| {
        Err(FinalizeError::CreatableEscapesMandatoryInject { inject_scope })
    })
}

/// Conflicting injects WITHIN one layer, which nothing else asks about.
///
/// Scoped to a single layer on purpose. Cross-layer pairs are already `admit`'s
/// business - the layer fold admits each new layer against the accumulated charter, so
/// a conflict between two layers is caught there. What no path examines is two injects
/// inside the SAME layer, and the root layer is the sharpest case because a
/// single-layer charter never passes through `admit` as a draft at all.
///
/// Widening this to all-pairs over the composed stack would re-ask questions `admit`
/// has already answered, and would newly reject a cross-layer pair only if the two
/// predicates disagree - a difference worth establishing before relying on it, not
/// while fixing something else.
fn same_layer_inject_conflict(layers: &[Layer]) -> Option<String> {
    for layer in layers {
        for (index, first) in layer.injects.iter().enumerate() {
            let RuleKind::Inject { spec: first_spec } = &first.kind else {
                continue;
            };
            for second in layer.injects.iter().skip(index + 1) {
                let RuleKind::Inject { spec: second_spec } = &second.kind else {
                    continue;
                };
                // Disjoint scopes cannot collide: two injects may contribute the same
                // column name to different tables.
                if matches!(first.scope.meet(&second.scope), Scope::Nothing) {
                    continue;
                }
                if let Some(detail) = inject_column_conflict(first_spec, second_spec) {
                    return Some(detail);
                }
                if let Some(detail) = inject_pk_conflict(first_spec, second_spec) {
                    return Some(detail);
                }
                if let Some(detail) = inject_index_conflict(first_spec, second_spec) {
                    return Some(detail);
                }
            }
        }
    }
    None
}

/// The creatable-escape question, asked of a composed layer stack: is the effective
/// `schema.create_table` scope contained by every MANDATORY inject's scope? Answers
/// with the inject scope that was escaped, or `None` when nothing escapes.
///
/// ONE definition, deliberately, because two composition paths ask it. The charter
/// algebra asks through [`finalize_charter`], and the layer fold the product runs asks
/// through [`EffectivePolicy::check_creatable_escape`]. When the check lived in only one
/// of them the other silently stopped enforcing it, which is the state this repairs.
fn creatable_escape_scope(layers: &[Layer]) -> Option<String> {
    let Ok(create_key) = KnobKey::parse(CREATE_TABLE_KEY) else {
        return None;
    };
    // The effective creatable scope: the ⊒-conservative join of each layer's granted
    // create_table scope (over-approx is the fail-closed direction for a ⊑ containment
    // check — it can only turn a pass into a reject).
    let mut creatable = Scope::Nothing;
    for layer in layers {
        if let Some(km) = layer.grants.keys.get(&create_key) {
            let Ok(granted) = km.granted_scope() else {
                return Some("<unrepresentable creatable scope>".to_string());
            };
            creatable = creatable.join(&granted);
        }
    }
    if matches!(creatable, Scope::Nothing) {
        return None;
    }
    for layer in layers {
        for rule in &layer.injects {
            let RuleKind::Inject { spec } = &rule.kind else {
                continue;
            };
            if !spec.mandatory {
                continue;
            }
            if !creatable.subset(&rule.scope) {
                return Some(render_scope(&rule.scope));
            }
        }
    }
    None
}

// ══════════════════════════════════════════════════════════════════════════════
// overlay — trusted config cascade (last-wins), TOTAL (H-1: TrustedDoc operands)
// ══════════════════════════════════════════════════════════════════════════════

/// Assemble ONE operator's own charter from co-authored profile fragments
/// (`base + env`, II.5) into an [`AssembledCharter`]. Both operands are [`TrustedDoc`]
/// (H-1: a creator draft cannot be an `overlay` operand — the type forbids it), so
/// `overlay` is **total** and **never rejects**.
///
/// Semantics: **presence-based last-wins** for scalar grant/obligation values (where
/// `over` has ANY covering rule on a key at an object, `over` wins — it may loosen OR
/// tighten, and may narrow to default), and **rule-list accumulation** (base-then-over)
/// for inject/validate. This is realized by producing a 2-layer stack `[Env=over] over
/// [Base=base]` (H-4): the query falls through top→down, so `over`'s presence masks
/// `base` exactly where `over` covers, and inherits `base` elsewhere; inject/validate
/// lists union with `Base` (outermost) first, then `Env` — the M-3 base-then-over order.
///
/// Charter misconfigurations (colliding injects across `base`/`env`, PK conflicts) are
/// NOT rejected here (that would break totality); they surface at [`finalize_charter`]
/// (H-3), which every assembled charter passes before it may be `admit`'s charter.
pub fn overlay(
    base: &TrustedDoc,
    over: &TrustedDoc,
    registry: &PolicyRegistry,
) -> Result<AssembledCharter, ComposeError> {
    let base_layer = Layer::from_doc(LayerTag::Base, base.doc(), registry)?;
    let over_layer = Layer::from_doc(LayerTag::Env, over.doc(), registry)?;
    // Stack: TOP (innermost query-precedence) first → `[Env=over] over [Base=base]`.
    Ok(AssembledCharter {
        registry: registry.clone(),
        layers: vec![over_layer, base_layer],
    })
}

// ══════════════════════════════════════════════════════════════════════════════
// restrict — meet of two TRUSTED docs (H-1: TrustedDoc operands, TOTAL)
// ══════════════════════════════════════════════════════════════════════════════

/// Meet two TRUSTED documents pointwise into an [`AssembledCharter`] (II.3.2) — the
/// trusted tightening gradient (host ⊒ org ⊒ project). Both operands are
/// [`TrustedDoc`] (H-1: an untrusted draft cannot be a `restrict` operand — the type
/// forbids it), so `restrict` is **total** and **never rejects**. It is a genuine
/// lattice MEET (⊓): grants meet pointwise (per key: scope `⊓`, value `⊓_value` at
/// overlap); require/inject/validate rule-sets UNION (each at its own scope).
///
/// The result is a SINGLE flattened `Base` layer — `restrict` flattens a trusted meet
/// into one meet-layer (H-4). Charter-vs-charter inject/PK/index collisions are NOT
/// rejected here (that would break the meet's associativity); they surface at
/// [`finalize_charter`] (H-3), which every assembled charter passes before it can be
/// `admit`'s charter.
pub fn restrict(
    outer: &TrustedDoc,
    inner: &TrustedDoc,
    registry: &PolicyRegistry,
) -> Result<AssembledCharter, ComposeError> {
    let og = GrantModel::build(&outer.doc().rules, registry)?;
    let ig = GrantModel::build(&inner.doc().rules, registry)?;

    // ── grants: pointwise meet, represented as rule-scope ⊓ products ───────────
    let mut clamped_grants = GrantModel {
        keys: BTreeMap::new(),
    };
    for key in og.key_union(&ig) {
        let km = clamp_grant_key(key, &og, &ig, registry)?;
        if !km.rules.is_empty() {
            clamped_grants.keys.insert(key.clone(), km);
        }
    }

    // ── require/inject/validate: UNION, each at its own scope (outer first) ──────
    let mut injects = rules_of(&outer.doc().rules, |k| matches!(k, RuleKind::Inject { .. }));
    injects.extend(rules_of(&inner.doc().rules, |k| {
        matches!(k, RuleKind::Inject { .. })
    }));
    let mut requires = rules_of(&outer.doc().rules, |k| {
        matches!(k, RuleKind::Require { .. })
    });
    requires.extend(rules_of(&inner.doc().rules, |k| {
        matches!(k, RuleKind::Require { .. })
    }));
    let mut validates = rules_of(&outer.doc().rules, |k| {
        matches!(k, RuleKind::Validate { .. })
    });
    validates.extend(rules_of(&inner.doc().rules, |k| {
        matches!(k, RuleKind::Validate { .. })
    }));

    // A single flattened `Base` layer (the meet is one meet-layer, H-4).
    let layer = Layer {
        tag: LayerTag::Base,
        grants: clamped_grants,
        requires,
        injects,
        validates,
    };
    Ok(AssembledCharter {
        registry: registry.clone(),
        layers: vec![layer],
    })
}

/// Per-key grant clamp (II.3.2): the pointwise meet, materialized as the finite set of
/// grant rules obtained by intersecting each outer rule's scope with each inner rule's
/// scope (via `⊓`) and meeting their values; empty-scope products drop out.
///
/// A key present in only ONE charter meets against the OTHER's default (deny): the
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
    let empty = GrantKeyMap {
        kind: def.kind.clone(),
        default: def.default.clone(),
        rules: Vec::new(),
    };
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
    Ok(GrantKeyMap {
        kind: def.kind.clone(),
        default: def.default.clone(),
        rules,
    })
}

// ══════════════════════════════════════════════════════════════════════════════
// collision detection (compose-time blame)
// ══════════════════════════════════════════════════════════════════════════════

/// Compose-time DRAFT-vs-CHARTER inject-collision check (`admit`). A charter inject
/// colliding with a draft inject on scope-overlapping objects — same column name with
/// divergent shape, conflicting `primary_key` pin, `author_primary_key`
/// Allow-vs-Forbid, or same index name with divergent columns — is blamed on the
/// draft. (Charter-vs-charter collisions are NOT checked here — they surface at
/// [`finalize_charter`], H-3.)
pub(crate) fn check_inject_collisions(
    charter_injects: &[Rule],
    draft_injects: &[Rule],
) -> Result<(), ComposeError> {
    for l in charter_injects {
        let RuleKind::Inject { spec: ls } = &l.kind else {
            continue;
        };
        for r in draft_injects {
            let RuleKind::Inject { spec: rs } = &r.kind else {
                continue;
            };
            // Only overlapping scopes can collide.
            if matches!(l.scope.meet(&r.scope), Scope::Nothing) {
                continue;
            }
            if let Some(detail) = inject_specs_collide(ls, rs) {
                return Err(ComposeError::DraftInjectCollidesCharter { detail });
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
                return Some(format!(
                    "column `{}` injected with divergent shape",
                    ca.name
                ));
            }
        }
    }
    // Index-name collision with divergent columns.
    for ia in &a.indexes {
        for ib in &b.indexes {
            if names_match(&ia.name, &ib.name) && ia.columns != ib.columns {
                return Some(format!(
                    "index `{}` injected with divergent columns",
                    ia.name
                ));
            }
        }
    }
    // Conflicting PK pins.
    if let (Some(pa), Some(pb)) = (&a.primary_key, &b.primary_key) {
        if pa != pb {
            return Some(format!(
                "primary key pinned to divergent columns {pa:?} vs {pb:?}"
            ));
        }
        // Both pin the SAME PK but disagree on author-PK policy → conflict.
        if a.author_primary_key != b.author_primary_key {
            return Some("primary key pinned with divergent author_primary_key policy".to_string());
        }
    }
    None
}

// The three split conflict predicates the FINALIZE gate uses (they classify a
// charter-vs-charter inject collision into its precise `FinalizeError` variant).

/// Two injects share a column name with a DIVERGENT shape (type/nullable/default)?
fn inject_column_conflict(a: &InjectSpec, b: &InjectSpec) -> Option<String> {
    for ca in &a.columns {
        for cb in &b.columns {
            if names_match(&ca.name, &cb.name)
                && (ca.ty != cb.ty || ca.nullable != cb.nullable || ca.default != cb.default)
            {
                return Some(format!(
                    "column `{}` injected with divergent shape",
                    ca.name
                ));
            }
        }
    }
    None
}

/// Two injects pin divergent primary keys, or the same PK with divergent
/// `author_primary_key` policy?
fn inject_pk_conflict(a: &InjectSpec, b: &InjectSpec) -> Option<String> {
    if let (Some(pa), Some(pb)) = (&a.primary_key, &b.primary_key) {
        if pa != pb {
            return Some(format!(
                "primary key pinned to divergent columns {pa:?} vs {pb:?}"
            ));
        }
        if a.author_primary_key != b.author_primary_key {
            return Some("primary key pinned with divergent author_primary_key policy".to_string());
        }
    }
    None
}

/// Two injects share an index name with a DIVERGENT column list?
fn inject_index_conflict(a: &InjectSpec, b: &InjectSpec) -> Option<String> {
    for ia in &a.indexes {
        for ib in &b.indexes {
            if names_match(&ia.name, &ib.name) && ia.columns != ib.columns {
                return Some(format!(
                    "index `{}` injected with divergent columns",
                    ia.name
                ));
            }
        }
    }
    None
}

/// A draft `Validate` (`ForbiddenColumns` / `TableNameForbidden`) contradicting a
/// charter `Inject` on overlapping scope (II.4.4): a forbidden column naming an
/// injected column, or a forbidden table-name pattern matching would make the
/// charter-injected table unsatisfiable. We flag the concrete column contradiction
/// (name-match); table-name contradiction is left to resolve time (injects carry no
/// table name here), matching the loader's single-doc gate.
pub(crate) fn check_validate_vs_inject(
    charter_injects: &[Rule],
    draft_validates: &[Rule],
) -> Result<(), ComposeError> {
    for inj in charter_injects {
        let RuleKind::Inject { spec } = &inj.kind else {
            continue;
        };
        for val in draft_validates {
            let RuleKind::Validate { pred } = &val.kind else {
                continue;
            };
            if matches!(inj.scope.meet(&val.scope), Scope::Nothing) {
                continue;
            }
            if let ValidatePredicate::ForbiddenColumns { names } = pred {
                for col in &spec.columns {
                    for forbidden in names {
                        if names_match(&col.name, forbidden) {
                            return Err(ComposeError::DraftValidateContradictsCharterInject {
                                detail: format!(
                                    "draft forbids column `{}` a charter inject contributes",
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
pub(crate) fn lookup<'r>(
    registry: &'r PolicyRegistry,
    key: &KnobKey,
) -> Result<&'r KnobDef, ComposeError> {
    registry
        .get(key)
        .ok_or_else(|| ComposeError::RegistryOrValueMismatch {
            detail: format!("unknown knob {key}"),
        })
}

/// Clone the rules matching a kind predicate, preserving document order.
pub(crate) fn rules_of(rules: &[Rule], pred: impl Fn(&RuleKind) -> bool) -> Vec<Rule> {
    rules.iter().filter(|r| pred(&r.kind)).cloned().collect()
}

/// Name-match two identifiers under the II.2.7 fold: normalize both to canonical
/// bytes and compare. Used for inject/validate/injected-shape name matching so a
/// case-fold or quoted-dot trick cannot slip a name past the check.
pub(crate) fn names_match(a: &str, b: &str) -> bool {
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
pub(crate) fn render_scope(s: &Scope) -> String {
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
pub(crate) fn witness_of(s: &Scope) -> Option<ObjectName> {
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
