//! The strict document loader (II.3). Parses a policy document from TOML or JSON
//! into the resolved in-memory model (`crate::rule`), validating against the
//! [`PolicyRegistry`] and enforcing every load-time legality gate (II.2.4–II.2.7,
//! II.4.2). This is a SECURITY CORE: strict parsing (`deny_unknown_fields`),
//! fail-closed, registry-validated, name-normalized at the scope boundary.
//!
//! What this module does NOT do (Phase 1b-ii): composition (`admit`/
//! `restrict`), `EffectivePolicy`, seal verification. A loaded [`PolicyDoc`]
//! is a single validated layer, not a composed effective policy.

use serde::Deserialize;

use crate::knob::{KnobDef, KnobKey, KnobKind, KnobValue, ObjectModel, Polarity};
use crate::registry::PolicyRegistry;
use crate::rule::{
    AuthorPkPolicy, InjectColumn, InjectIndex, InjectSpec, NameGlob, Rule, RuleKind,
    ValidatePredicate,
};
use crate::scope::normalize_pg_identifier;
use crate::{Pattern, Scope, ScopeError};

/// The highest `policy_version` MAJOR this loader understands. An unknown major is
/// a hard error (E7 — versioned for code-evolution discipline).
pub const SUPPORTED_POLICY_VERSION: u32 = 1;

/// The layer a document is being loaded AS. Two axes it governs:
/// - **`mandatory` injects** are `RootCharter`-only; any non-root layer that carries
///   one is rejected (`MandatoryInjectOnNonRootLayer`, II.4.2).
/// - **`extends`** is TRUSTED-only (H-1): the `RootCharter` and trusted catalog
///   entries may inherit a trusted base; an untrusted creator DRAFT that carries
///   `extends` is a hard load error (`ExtendsForbiddenInDraft`, II.7).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoadContext {
    /// The host's root charter — the only layer allowed a mandatory inject; trusted,
    /// so `extends` is permitted.
    RootCharter,
    /// A TRUSTED, non-root catalog entry (a `ProfileCatalog` `env` fragment): no
    /// mandatory inject, but `extends` IS permitted (resolved against the trusted
    /// catalog).
    TrustedCatalogEntry,
    /// An UNTRUSTED creator draft (submitted to `admit`): no mandatory inject, and
    /// `extends` is a hard load error (H-1).
    NonRootLayer,
}

impl LoadContext {
    /// Whether this context is trusted (may use `extends`).
    #[must_use]
    fn is_trusted(self) -> bool {
        matches!(
            self,
            LoadContext::RootCharter | LoadContext::TrustedCatalogEntry
        )
    }
}

/// A resolved, validated policy document — one layer (II.3). Produced by the
/// loader after all legality gates pass. Dead rules (effective scope `Nothing`)
/// are surfaced as [`warnings`](PolicyDoc::warnings), not dropped silently.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PolicyDoc {
    /// The document's declared version.
    pub policy_version: u32,
    /// The policy-wide default scope (`None` = `All`, II.2.4). Kept for the
    /// composer; every non-Global rule's scope has already been met with it.
    pub default_scope: Option<Scope>,
    /// The resolved rules, in document order (grants, requires, injects,
    /// validates — preserving each section's authored order).
    pub rules: Vec<Rule>,
    /// Non-fatal load diagnostics (dead rules, II.3.1).
    pub warnings: Vec<LoadWarning>,
}

/// A non-fatal load diagnostic.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LoadWarning {
    /// A rule's effective scope is `Nothing` — it can never match any object, so it
    /// is inert. We WARN rather than error: a dead rule is not a security hazard
    /// (it grants/requires/injects nothing), and erroring would make an otherwise
    /// legal `default_scope ⊓ rule.scope = ∅` composition un-loadable. The rule is
    /// retained in `rules` so the composer sees the same list the author wrote.
    DeadRule { index: usize },
}

/// Every load-time legality failure. Each maps to a named gate (II.2.4–II.2.7,
/// II.4.2); the loader is fail-closed — any one of these rejects the whole document.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LoadError {
    /// The serde/format parse failed (bad TOML/JSON, unknown field via
    /// `deny_unknown_fields`, wrong shape).
    Parse { detail: String },
    /// `policy_version` names a major this loader does not understand (E7).
    UnknownPolicyVersion { found: u32 },
    /// A rule's knob key string is not a well-formed `namespace.name`.
    MalformedKnobKey { key: String },
    /// A rule references a knob key not present in the registry (deny_unknown
    /// against the runtime-extensible known set, II.2.1).
    UnknownKnobKey { key: String },
    /// A `[[grant]]` names a non-Grant-polarity knob, or a `[[require]]` a
    /// non-Require-polarity knob (section↔polarity lint, II.3).
    SectionPolarityMismatch {
        key: String,
        expected: Polarity,
        found: Polarity,
    },
    /// A knob value is invalid for its knob's kind (or below a `UintCharter`
    /// hard floor) (II.2.1).
    InvalidKnobValue { key: String, detail: String },
    /// A scope pattern literal is malformed (bad glob, >2 segments, bad quoting).
    MalformedScope { pattern: String },
    /// A scope was authored with an empty include (the ⊥/⊤ collision guard).
    EmptyInclude,
    /// A Global-`object_model` knob carries a non-⊤ scope (II.2.5). Global knobs
    /// must be authored `scope = All` and are exempt from the default-scope meet.
    ScopeIllegalForGlobalKnob { key: String },
    /// A `PerSchema` knob carries a table-granular (two-segment) scope (II.2.5).
    ScopeTooGranularForKnob { key: String },
    /// A Grant-kind rule has neither its own scope nor a `default_scope` — it would
    /// acquire ⊤ by omission (A3 foot-gun, II.3).
    GrantScopeUnbounded { key: String },
    /// A `mandatory = true` inject rule on a non-root layer (II.4.2).
    MandatoryInjectOnNonRootLayer,
    /// A single document both injects a column X and forbids X (or forbids the
    /// inject's own required table name) on overlapping scope (II.4.4).
    SelfContradictoryInjectValidate { detail: String },
    /// An UNTRUSTED creator draft carries an `extends` field (H-1, II.7). A draft may
    /// never inherit a catalog base — the charter it is admitted against already
    /// carries the operator floor. Forbidding it removes the untrusted-`extends`
    /// laundering hazard outright.
    ExtendsForbiddenInDraft,
    /// A trusted document's `extends` names a base not present in the injected trusted
    /// catalog (II.7). Resolution is fail-closed: an unknown base is an error, never a
    /// silent skip.
    ExtendsUnknownBase { base: String },
    /// A trusted `extends` chain revisits a base name — a cycle (II.7). Fail-closed.
    ExtendsCycle { base: String },
}

impl From<ScopeError> for LoadError {
    fn from(e: ScopeError) -> Self {
        match e {
            ScopeError::EmptyInclude => LoadError::EmptyInclude,
        }
    }
}

// ── wire (serde) types ──────────────────────────────────────────────────────────
//
// The on-wire document is deliberately separate from the resolved model: it holds
// the *authored* (pre-normalization, pre-registry-validation) shape. Every struct
// is `deny_unknown_fields` so a typo'd key is a hard parse error, not a silent drop.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireDoc {
    policy_version: u32,
    /// A trusted base to inherit (II.7, H-1). TRUSTED-only: a creator draft carrying
    /// `extends` is a hard load error; a trusted doc resolves it against the injected
    /// trusted catalog with cycle detection.
    #[serde(default)]
    extends: Option<String>,
    #[serde(default)]
    default_scope: Option<WireScope>,
    #[serde(default)]
    grant: Vec<WireGrant>,
    #[serde(default)]
    require: Vec<WireGrant>,
    #[serde(default)]
    inject: Vec<WireInject>,
    #[serde(default)]
    validate: Vec<WireValidate>,
}

/// A scope on the wire: either the loud `all = true` / `nothing = true` tokens, or
/// a proper `{ include, exclude }`. Exactly one form; mixing is a parse error.
#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum WireScope {
    /// `"all"` / `"nothing"` shorthand string form.
    Token(String),
    /// `{ all = true }` or `{ nothing = true }`.
    Extreme {
        #[serde(default)]
        all: Option<bool>,
        #[serde(default)]
        nothing: Option<bool>,
    },
    /// `{ include = [...], exclude = [...] }`.
    Of {
        include: Vec<String>,
        #[serde(default)]
        exclude: Vec<String>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireGrant {
    key: String,
    value: WireValue,
    #[serde(default)]
    scope: Option<WireScope>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireInject {
    #[serde(default)]
    scope: Option<WireScope>,
    #[serde(default)]
    columns: Vec<WireColumn>,
    #[serde(default)]
    indexes: Vec<WireIndex>,
    #[serde(default)]
    primary_key: Option<Vec<String>>,
    #[serde(default)]
    author_primary_key: WireAuthorPk,
    #[serde(default)]
    mandatory: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireColumn {
    name: String,
    #[serde(rename = "type")]
    ty: String,
    #[serde(default = "wire_true")]
    nullable: bool,
    #[serde(default)]
    default: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireIndex {
    name: String,
    columns: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum WireAuthorPk {
    #[default]
    Allow,
    Forbid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireValidate {
    #[serde(default)]
    scope: Option<WireScope>,
    predicate: WirePredicate,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WirePredicate {
    HasPrimaryKey,
    ColumnNamePattern {
        #[serde(default)]
        require: Vec<String>,
        #[serde(default)]
        forbid: Vec<String>,
    },
    ForbiddenColumns {
        names: Vec<String>,
    },
    TypeNullability {
        column: String,
        #[serde(default)]
        #[serde(rename = "type")]
        ty: Option<String>,
        #[serde(default)]
        nullable: Option<bool>,
    },
    RequireIndex {
        columns: Vec<String>,
    },
    TableNameForbidden {
        patterns: Vec<String>,
    },
}

/// A knob value on the wire, matching the four `KnobValue` shapes.
#[derive(Deserialize)]
#[serde(untagged)]
enum WireValue {
    Bool(bool),
    Uint(u64),
    StrSet(Vec<String>),
    Str(String),
}

impl WireValue {
    fn into_knob_value(self) -> KnobValue {
        match self {
            WireValue::Bool(b) => KnobValue::Bool(b),
            WireValue::Uint(u) => KnobValue::Uint(u),
            WireValue::StrSet(v) => KnobValue::StrSet(v),
            WireValue::Str(s) => KnobValue::Str(s),
        }
    }
}

fn wire_true() -> bool {
    true
}

impl PolicyDoc {
    /// Parse + validate a policy document from TOML.
    pub fn parse_toml(
        src: &str,
        registry: &PolicyRegistry,
        ctx: LoadContext,
    ) -> Result<PolicyDoc, LoadError> {
        let wire: WireDoc = toml::from_str(src).map_err(|e| LoadError::Parse {
            detail: e.to_string(),
        })?;
        Self::resolve(wire, registry, ctx)
    }

    /// Parse + validate a policy document from JSON.
    pub fn parse_json(
        src: &str,
        registry: &PolicyRegistry,
        ctx: LoadContext,
    ) -> Result<PolicyDoc, LoadError> {
        let wire: WireDoc = serde_json::from_str(src).map_err(|e| LoadError::Parse {
            detail: e.to_string(),
        })?;
        Self::resolve(wire, registry, ctx)
    }

    /// Parse + validate a TRUSTED document that may `extends` a base, resolving the
    /// base chain against the injected trusted `catalog` (II.7, H-1) with cycle
    /// detection. `ctx` MUST be a trusted context (`RootCharter`/`TrustedCatalogEntry`)
    /// — an untrusted-draft context with `extends` is `ExtendsForbiddenInDraft`. The
    /// base document's rules are inherited UNDER this document's (doc-level overlay:
    /// this doc's rules are the inner/override layer, base is outer), and its
    /// `default_scope` is inherited when this doc omits its own.
    pub fn parse_toml_with_catalog(
        src: &str,
        registry: &PolicyRegistry,
        ctx: LoadContext,
        catalog: &dyn ProfileCatalog,
    ) -> Result<PolicyDoc, LoadError> {
        let wire: WireDoc = toml::from_str(src).map_err(|e| LoadError::Parse {
            detail: e.to_string(),
        })?;
        Self::resolve_with_catalog(wire, registry, ctx, Some(catalog), &mut Vec::new())
    }

    /// The resolution + legality core shared by both format front-ends.
    fn resolve(
        wire: WireDoc,
        registry: &PolicyRegistry,
        ctx: LoadContext,
    ) -> Result<PolicyDoc, LoadError> {
        Self::resolve_with_catalog(wire, registry, ctx, None, &mut Vec::new())
    }

    /// The resolution core, catalog-aware for `extends`. `seen` tracks the base chain
    /// for cycle detection.
    fn resolve_with_catalog(
        wire: WireDoc,
        registry: &PolicyRegistry,
        ctx: LoadContext,
        catalog: Option<&dyn ProfileCatalog>,
        seen: &mut Vec<String>,
    ) -> Result<PolicyDoc, LoadError> {
        // Gate: policy_version major.
        if wire.policy_version != SUPPORTED_POLICY_VERSION {
            return Err(LoadError::UnknownPolicyVersion {
                found: wire.policy_version,
            });
        }

        // Gate: `extends` provenance (H-1, II.7).
        let extends = wire.extends.clone();
        if extends.is_some() && !ctx.is_trusted() {
            // An untrusted creator draft may never inherit a base.
            return Err(LoadError::ExtendsForbiddenInDraft);
        }

        let default_scope = match wire.default_scope {
            Some(ws) => Some(resolve_scope(ws)?),
            None => None,
        };

        let mut rules: Vec<Rule> = Vec::new();

        // ── grants ──────────────────────────────────────────────────────────────
        for g in wire.grant {
            let (key, def) = lookup_knob(registry, &g.key)?;
            // Section↔polarity lint: [[grant]] must name a Grant-polarity knob.
            if !matches!(def.polarity, Polarity::Grant) {
                return Err(LoadError::SectionPolarityMismatch {
                    key: g.key,
                    expected: Polarity::Grant,
                    found: def.polarity,
                });
            }
            let value = g.value.into_knob_value();
            validate_value(&g.key, &value, &def.kind)?;
            let scope = resolve_rule_scope(g.scope, def, &default_scope, RuleClass::Grant, &g.key)?;
            rules.push(Rule {
                scope,
                kind: RuleKind::Grant { key, value },
            });
        }

        // ── requires ─────────────────────────────────────────────────────────────
        for r in wire.require {
            let (key, def) = lookup_knob(registry, &r.key)?;
            if !matches!(def.polarity, Polarity::Require) {
                return Err(LoadError::SectionPolarityMismatch {
                    key: r.key,
                    expected: Polarity::Require,
                    found: def.polarity,
                });
            }
            let value = r.value.into_knob_value();
            validate_value(&r.key, &value, &def.kind)?;
            // Require rules are object-filterable (never Global-key by
            // section↔polarity + object_model contract), and are NOT subject to the
            // A3 grant-unbounded gate (that is Grant-kind only).
            let scope =
                resolve_rule_scope(r.scope, def, &default_scope, RuleClass::Require, &r.key)?;
            rules.push(Rule {
                scope,
                kind: RuleKind::Require { key, value },
            });
        }

        // ── injects ──────────────────────────────────────────────────────────────
        for mut inj in wire.inject {
            // mandatory-on-non-root gate.
            if inj.mandatory && ctx != LoadContext::RootCharter {
                return Err(LoadError::MandatoryInjectOnNonRootLayer);
            }
            let scope = resolve_content_scope(inj.scope.take(), &default_scope)?;
            let spec = resolve_inject(inj)?;
            rules.push(Rule {
                scope,
                kind: RuleKind::Inject { spec },
            });
        }

        // ── validates ─────────────────────────────────────────────────────────────
        for val in wire.validate {
            let scope = resolve_content_scope(val.scope, &default_scope)?;
            let pred = resolve_predicate(val.predicate)?;
            rules.push(Rule {
                scope,
                kind: RuleKind::Validate { pred },
            });
        }

        // Cross-rule single-doc legality: self-contradictory inject vs validate.
        check_self_contradiction(&rules)?;

        let mut this = PolicyDoc {
            policy_version: wire.policy_version,
            default_scope,
            rules,
            warnings: Vec::new(),
        };

        // ── extends resolution (trusted-only, catalog-resolved, cycle-detected) ────
        if let Some(base_name) = extends {
            // Reachable only in a trusted context (the untrusted-draft case errored
            // above). A trusted `extends` REQUIRES a catalog; without one it is an
            // unknown base (fail-closed — never a silent skip).
            let Some(catalog) = catalog else {
                return Err(LoadError::ExtendsUnknownBase { base: base_name });
            };
            // Cycle detection over the base chain.
            if seen.iter().any(|n| n == &base_name) {
                return Err(LoadError::ExtendsCycle { base: base_name });
            }
            seen.push(base_name.clone());
            let base_src =
                catalog
                    .get_source(&base_name)
                    .ok_or_else(|| LoadError::ExtendsUnknownBase {
                        base: base_name.clone(),
                    })?;
            // Resolve the base as a TRUSTED catalog entry (it may itself `extends`).
            let base_wire: WireDoc = toml::from_str(&base_src).map_err(|e| LoadError::Parse {
                detail: e.to_string(),
            })?;
            let base = Self::resolve_with_catalog(
                base_wire,
                registry,
                LoadContext::TrustedCatalogEntry,
                Some(catalog),
                seen,
            )?;
            // Doc-level overlay: the base's rules are inherited UNDER this doc's (base
            // first, then this doc's — this doc overrides on scalar knobs at query
            // time, and rule lists accumulate). `default_scope` is inherited when this
            // doc omits its own.
            this = overlay_docs(base, this);
        }

        // Dead-rule detection (warn, not error) — computed AFTER extends merge.
        this.warnings = this
            .rules
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r.scope, Scope::Nothing))
            .map(|(index, _)| LoadWarning::DeadRule { index })
            .collect();

        Ok(this)
    }
}

/// A trusted PROFILE CATALOG (II.5): resolves a base NAME to its host-authored source
/// document. Every entry is host-injected — a `TrustedDoc` provenance (H-1). Used by
/// the loader to resolve a trusted `extends`. Returning the SOURCE (not a parsed doc)
/// lets the loader re-resolve a base's own `extends` chain with shared cycle tracking.
pub trait ProfileCatalog {
    /// The host-authored source of the named base, or `None` if not in the catalog.
    fn get_source(&self, name: &str) -> Option<String>;
}

/// Doc-level overlay for `extends` (II.7): the `base` is the OUTER layer, `over` the
/// INNER (override) layer. Rule lists ACCUMULATE (base first, then `over`) so a
/// query's loosest-covering/fall-through sees `over` first; `over`'s `default_scope`
/// wins when present, else `base`'s is inherited. This mirrors the `overlay`
/// combinator's list-accumulation at the document level (the composer's scalar
/// last-wins is realized at query time by rule order).
fn overlay_docs(base: PolicyDoc, over: PolicyDoc) -> PolicyDoc {
    let mut rules = base.rules;
    rules.extend(over.rules);
    PolicyDoc {
        policy_version: over.policy_version,
        default_scope: over.default_scope.or(base.default_scope),
        rules,
        warnings: Vec::new(),
    }
}

/// Which section a rule came from — drives the A3 grant-unbounded gate.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RuleClass {
    Grant,
    Require,
}

/// Look up a knob key in the registry (parse the key, then require it present).
fn lookup_knob<'r>(
    registry: &'r PolicyRegistry,
    key: &str,
) -> Result<(KnobKey, &'r KnobDef), LoadError> {
    let parsed = KnobKey::parse(key).map_err(|_| LoadError::MalformedKnobKey {
        key: key.to_string(),
    })?;
    let def = registry
        .get(&parsed)
        .ok_or_else(|| LoadError::UnknownKnobKey {
            key: key.to_string(),
        })?;
    Ok((parsed, def))
}

/// Validate a knob value against its kind, mapping the error to a `LoadError`.
fn validate_value(key: &str, value: &KnobValue, kind: &KnobKind) -> Result<(), LoadError> {
    value
        .validate_for(kind)
        .map_err(|e| LoadError::InvalidKnobValue {
            key: key.to_string(),
            detail: format!("{e:?}"),
        })
}

/// Resolve a `Grant`/`Require` rule's effective scope, applying the II.2.5 object
/// attribution + II.2.4 default-scope meet (with the Global-knob carve-outs).
fn resolve_rule_scope(
    authored: Option<WireScope>,
    def: &KnobDef,
    default_scope: &Option<Scope>,
    class: RuleClass,
    key: &str,
) -> Result<Scope, LoadError> {
    match def.object_model {
        ObjectModel::Global => {
            // A Global knob's scope is a legality marker: only the syntactic `All`
            // token is admissible; anything else (incl. omission → inherit narrow
            // default) is a hard error. Global rules are EXEMPT from the meet.
            match authored {
                Some(ws) => {
                    let scope = resolve_scope(ws)?;
                    if scope == Scope::All {
                        Ok(Scope::All)
                    } else {
                        Err(LoadError::ScopeIllegalForGlobalKnob {
                            key: key.to_string(),
                        })
                    }
                }
                // Omission would inherit the (possibly narrow) default — illegal for
                // a Global knob, which must be spelled `All` loudly.
                None => Err(LoadError::ScopeIllegalForGlobalKnob {
                    key: key.to_string(),
                }),
            }
        }
        ObjectModel::PerSchema | ObjectModel::PerTable => {
            let own = match authored {
                Some(ws) => Some(resolve_scope(ws)?),
                None => None,
            };

            // Granularity gate: a table-granular pattern on a PerSchema key is illegal.
            if matches!(def.object_model, ObjectModel::PerSchema) {
                if let Some(s) = &own {
                    if scope_has_table_granular_pattern(s) {
                        return Err(LoadError::ScopeTooGranularForKnob {
                            key: key.to_string(),
                        });
                    }
                }
            }

            // A3: a Grant-kind rule with neither its own scope nor a default_scope
            // would acquire ⊤ by omission → hard error.
            if class == RuleClass::Grant && own.is_none() && default_scope.is_none() {
                return Err(LoadError::GrantScopeUnbounded {
                    key: key.to_string(),
                });
            }

            Ok(effective_meet(own, default_scope))
        }
    }
}

/// Resolve an `Inject`/`Validate` rule's effective scope. These carry no knob key,
/// are always object-filterable, and always take the default-scope meet (II.2.4).
fn resolve_content_scope(
    authored: Option<WireScope>,
    default_scope: &Option<Scope>,
) -> Result<Scope, LoadError> {
    let own = match authored {
        Some(ws) => Some(resolve_scope(ws)?),
        None => None,
    };
    Ok(effective_meet(own, default_scope))
}

/// `effective_scope = default_scope ⊓ rule.scope` with the omission rules:
/// - no own scope, no default → `All` (⊤; the only path to ⊤, and only reached by
///   non-Grant rules — the Grant A3 gate rejects this case before we get here).
/// - own scope, no default → own scope.
/// - no own scope, default → default.
/// - both → the meet.
fn effective_meet(own: Option<Scope>, default_scope: &Option<Scope>) -> Scope {
    match (own, default_scope) {
        (None, None) => Scope::All,
        (Some(s), None) => s,
        (None, Some(d)) => d.clone(),
        (Some(s), Some(d)) => d.meet(&s),
    }
}

/// Does any include/exclude pattern of a proper scope address a specific TABLE
/// (a two-segment pattern with a non-`*` table glob)? Used by the PerSchema gate.
fn scope_has_table_granular_pattern(scope: &Scope) -> bool {
    match scope {
        Scope::Nothing | Scope::All => false,
        Scope::Of { include, exclude } => include
            .iter()
            .chain(exclude.iter())
            .any(pattern_is_table_granular),
    }
}

/// A pattern is table-granular iff its table segment is not the wildcard `*` — i.e.
/// it names a specific table (or table glob), not a whole schema. A schema-only
/// authored pattern normalizes to `P.*`, whose table glob IS `*`, so it is
/// schema-granular (legal on PerSchema).
fn pattern_is_table_granular(p: &Pattern) -> bool {
    !p.table.is_star()
}

/// Resolve a wire scope into a normalized [`Scope`] (II.2.7 fold at the pattern
/// boundary). `all`/`nothing` tokens map to the extremes; a proper `Of` normalizes
/// every include/exclude pattern through [`Pattern::parse_normalized`].
fn resolve_scope(ws: WireScope) -> Result<Scope, LoadError> {
    match ws {
        WireScope::Token(t) => match t.as_str() {
            "all" => Ok(Scope::All),
            "nothing" => Ok(Scope::Nothing),
            _ => Err(LoadError::MalformedScope { pattern: t }),
        },
        WireScope::Extreme { all, nothing } => match (all, nothing) {
            (Some(true), None) => Ok(Scope::All),
            (None, Some(true)) => Ok(Scope::Nothing),
            // `all = false` / `nothing = false` / both-set are not a valid extreme
            // spelling — reject fail-closed rather than guess an intended scope.
            _ => Err(LoadError::MalformedScope {
                pattern: "ambiguous all/nothing".into(),
            }),
        },
        WireScope::Of { include, exclude } => {
            let inc = normalize_patterns(&include)?;
            let exc = normalize_patterns(&exclude)?;
            Ok(Scope::of(inc, exc)?)
        }
    }
}

/// Normalize a list of pattern literals through the II.2.7 fold, rejecting any
/// malformed one.
fn normalize_patterns(lits: &[String]) -> Result<Vec<Pattern>, LoadError> {
    lits.iter()
        .map(|lit| {
            Pattern::parse_normalized(lit).ok_or_else(|| LoadError::MalformedScope {
                pattern: lit.clone(),
            })
        })
        .collect()
}

/// Resolve a wire inject into an [`InjectSpec`] (names are used verbatim by the
/// future resolver; the leaf crate does not fold column names — that is the
/// resolver's II.2.7 responsibility over the IR, matching author declarations).
fn resolve_inject(inj: WireInject) -> Result<InjectSpec, LoadError> {
    let columns = inj
        .columns
        .into_iter()
        .map(|c| InjectColumn {
            name: c.name,
            ty: c.ty,
            nullable: c.nullable,
            default: c.default,
        })
        .collect();
    let indexes = inj
        .indexes
        .into_iter()
        .map(|i| InjectIndex {
            name: i.name,
            columns: i.columns,
        })
        .collect();
    let author_primary_key = match inj.author_primary_key {
        WireAuthorPk::Allow => AuthorPkPolicy::Allow,
        WireAuthorPk::Forbid => AuthorPkPolicy::Forbid,
    };
    Ok(InjectSpec {
        columns,
        indexes,
        primary_key: inj.primary_key,
        author_primary_key,
        mandatory: inj.mandatory,
    })
}

/// Resolve a wire predicate, folding its name-glob literals per II.2.7.
fn resolve_predicate(wp: WirePredicate) -> Result<ValidatePredicate, LoadError> {
    Ok(match wp {
        WirePredicate::HasPrimaryKey => ValidatePredicate::HasPrimaryKey,
        WirePredicate::ColumnNamePattern { require, forbid } => {
            ValidatePredicate::ColumnNamePattern {
                require: name_globs(&require)?,
                forbid: name_globs(&forbid)?,
            }
        }
        WirePredicate::ForbiddenColumns { names } => ValidatePredicate::ForbiddenColumns { names },
        WirePredicate::TypeNullability {
            column,
            ty,
            nullable,
        } => ValidatePredicate::TypeNullability {
            column,
            ty,
            nullable,
        },
        WirePredicate::RequireIndex { columns } => ValidatePredicate::RequireIndex { columns },
        WirePredicate::TableNameForbidden { patterns } => {
            // Table-name patterns are full schema.table scope patterns.
            ValidatePredicate::TableNameForbidden {
                patterns: normalize_patterns(&patterns)?,
            }
        }
    })
}

/// Fold a list of single-segment name globs (II.2.7). A quoted literal is verbatim;
/// an unquoted one folds to lowercase with a single `*` as a wildcard. Rejects >1
/// segment (a name glob is one identifier) and malformed globs.
fn name_globs(lits: &[String]) -> Result<Vec<NameGlob>, LoadError> {
    lits.iter()
        .map(|lit| {
            // Reuse the pattern normalizer, then require a schema-only (single-seg)
            // result whose schema glob is the folded name. A dotted name-glob is
            // rejected — a column/table-name predicate matches ONE identifier.
            let pat = Pattern::parse_normalized(lit).ok_or_else(|| LoadError::MalformedScope {
                pattern: lit.clone(),
            })?;
            // A single-segment pattern is `schema.*`; its schema glob is the name.
            // A two-segment (table-granular) pattern is not a valid name glob.
            if !pat.table.is_star() {
                return Err(LoadError::MalformedScope {
                    pattern: lit.clone(),
                });
            }
            Ok(NameGlob { glob: pat.schema })
        })
        .collect()
}

/// Single-document self-contradiction gate (II.4.4): an inject of column X on some
/// scope S, plus a validate `ForbiddenColumns[X]` (or a `TableNameForbidden` that
/// matches the inject's own required table name — not modeled here, injects carry
/// no table name) on a scope overlapping S, is internally inconsistent → error.
///
/// Overlap is decided by the scope lattice meet (`S ⊓ T != Nothing`). Column-name
/// comparison is by folded byte equality (II.2.7) — the validate literals are
/// already folded; the inject column names are folded through the same identifier
/// normalizer for the comparison.
fn check_self_contradiction(rules: &[Rule]) -> Result<(), LoadError> {
    for inj in rules {
        let RuleKind::Inject { spec } = &inj.kind else {
            continue;
        };
        for val in rules {
            let RuleKind::Validate { pred } = &val.kind else {
                continue;
            };
            // Only meaningful when the two scopes overlap.
            if matches!(inj.scope.meet(&val.scope), Scope::Nothing) {
                continue;
            }
            if let ValidatePredicate::ForbiddenColumns { names } = pred {
                for col in &spec.columns {
                    let inj_folded = fold_name(&col.name);
                    for forbidden in names {
                        if fold_name(forbidden) == inj_folded {
                            return Err(LoadError::SelfContradictoryInjectValidate {
                                detail: format!(
                                    "inject column `{}` forbidden by an overlapping validate",
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

/// Fold a bare identifier for byte-comparison (the II.2.7 unquoted-lowercase fold
/// over a single segment). Used for the self-contradiction column-name compare.
fn fold_name(s: &str) -> Vec<u8> {
    // A validate `ForbiddenColumns` name / inject column name is a single unquoted
    // identifier; fold via the same normalizer and take the schema-glob bytes.
    normalize_pg_identifier(s)
        .map(|o| o.schema)
        .unwrap_or_else(|| s.to_ascii_lowercase().into_bytes())
}
