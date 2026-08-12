//! The trust boundary (II.1 `boundary`, II.3.2) — the SOLE untrusted crossing.
//!
//! [`admit`]`(charter, draft)` is the *only* place an UNTRUSTED creator [`PolicyDoc`]
//! meets a TRUSTED, FINALIZED charter, so it is the *only* place **escalation** is
//! checked. It lives in its own module — apart from the trusted, total combinators
//! [`overlay`](crate::compose::overlay) / [`restrict`](crate::compose::restrict) and
//! the trusted-side conflict gate
//! [`finalize_charter`](crate::compose::finalize_charter) — precisely so the one
//! security-bearing operator is unmissable when reading or auditing the crate.
//!
//! `admit` produces a LAYERED [`EffectivePolicy`] — `[draft] over [charter]` (H-4) —
//! whose grant query falls through top→down. Grants are **charter-inherited,
//! narrow-only, presence-overridden**: a silent draft inherits the charter's grant; a
//! draft that NARROWS wins by presence (admitted because a narrow is `⊑` the charter);
//! a draft that RAISES above default is admitted only where its value is `⊑` the
//! charter's (II.3.2). Obligations/injects/validates union-up (un-droppable).
//!
//! The escalation check proves each raising draft rule against the layer stack
//! SYMBOLICALLY - it never samples an object. Region arithmetic is ITERATED per rule
//! (never a `⊔`-materialized charter - the C-1 fix), and a difference the scope algebra
//! cannot represent is a refusal, not a pass.
//!
//! `admit`'s `charter` is a finalized [`AdmitCharter`] (a [`crate::RootCharter`], a
//! [`Charter`](crate::compose::Charter) from `finalize_charter`, or an already-composed
//! [`EffectivePolicy`]); an un-finalized [`AssembledCharter`](crate::compose::AssembledCharter)
//! deliberately does NOT implement `AdmitCharter`, so it cannot reach `admit` at the
//! type level (MED).

use crate::compose::{
    check_inject_collisions, check_validate_vs_inject, layered_nondefault_grant_rules,
    pin_layer_key_to_default, render_scope, rules_of, AdmitCharter, ComposeError, EffectivePolicy,
    GrantModel, Layer, LayerTag,
};
use crate::knob::KnobKey;
use crate::registry::PolicyRegistry;
use crate::rule::RuleKind;
use crate::scope::{Difference, Scope};
use crate::value_order::leq_value;
use crate::PolicyDoc;

/// Ingress of an UNTRUSTED `draft` against a trusted, FINALIZED `charter` (II.3.2).
/// Returns a LAYERED [`EffectivePolicy`] (`[draft] over [charter]`, H-4) iff the draft
/// escalates nowhere; otherwise a [`ComposeError`] blaming the draft. NO clipping —
/// strict reject only.
///
/// The algorithm, pointwise per (key, object) but decided SYMBOLICALLY:
/// - **Grants — charter-inherited, narrow-only, presence-overridden.** For each grant
///   key the draft is admissible iff at every object where the draft RAISES `k` above
///   default the draft value is `⊑` the charter's LAYERED effective value. Proved one
///   RAISING DRAFT RULE at a time against the layer stack, because a join is a least
///   upper bound and so the join of the covering draft rules is within the charter
///   exactly when each of them is. Each rule walks the layers top-first: a layer must
///   lift the rule's value wherever it covers, and everything that layer covers -
///   including rules at or below default, which grant nothing but still decide - is
///   retired before falling through. Whatever no layer raises sits at the knob default
///   and escalates. A not-representable `∖` fails closed.
/// - **Require/Inject/Validate:** union-up across all layers; a charter rule is never
///   dropped or narrowed.
/// - **Collisions:** a draft inject colliding with a charter inject, or a draft
///   validate contradicting a charter inject, rejects at compose time.
///
/// The creatable-scope lint is NOT here — it is a charter-side misconfiguration lint
/// run at [`finalize_charter`](crate::compose::finalize_charter) (MED), and admit's
/// per-key `draft ⊑ charter` grant check transitively bounds the draft's creatable
/// region below the charter's.
pub fn admit(
    charter: &impl AdmitCharter,
    draft: &PolicyDoc,
    registry: &PolicyRegistry,
) -> Result<EffectivePolicy, ComposeError> {
    let charter_layers = charter.charter_layers(registry)?;
    let draft_grants = GrantModel::build(&draft.rules, registry)?;

    // ── grant check: pointwise draft ⊑ charter (charter-inherited), per key ─────
    // The keys to check = every key the DRAFT grants (a key the draft is silent on
    // simply inherits the charter — no escalation possible).
    for key in draft_grants.keys() {
        check_grant_key(key, &draft_grants, &charter_layers, registry)?;
    }

    // ── require/inject/validate collision blame (draft vs charter) ──────────────
    let charter_injects =
        flatten_charter(&charter_layers, |k| matches!(k, RuleKind::Inject { .. }));
    let draft_injects = rules_of(&draft.rules, |k| matches!(k, RuleKind::Inject { .. }));
    check_inject_collisions(&charter_injects, &draft_injects)?;
    let draft_validates = rules_of(&draft.rules, |k| matches!(k, RuleKind::Validate { .. }));
    check_validate_vs_inject(&charter_injects, &draft_validates)?;

    // ── build the layered result: [draft] over [charter layers] (H-4) ───────────
    let mut draft_layer = Layer::from_doc(LayerTag::Draft, draft, registry)?;

    // ── non-inheritable POWER GRANTS (KnobDef.inherit == false) ─────────────────
    // A knob marked `inherit = false` must NOT flow to a SILENT draft from the
    // charter: "override the platform's injected columns" is a grant a creator earns
    // only by asking for it EXPLICITLY, never by inheritance-by-omission. Since the
    // layered grant query falls through the draft layer to the charter, we realize the
    // pin by giving the draft layer a synthetic DEFAULT-valued grant rule over the
    // whole universe (`Scope::All`) for every `inherit = false` key. Presence-override
    // (II.3.2) then makes the draft layer WIN the fall-through EVERYWHERE for that key:
    // where the draft EXPLICITLY granted it (already bounded ⊑ the charter by the check
    // above), the loosest-covering join within the draft layer keeps the draft's own
    // value; where the draft is SILENT, only the synthetic default rule covers, so the
    // value is the knob default — never the inherited charter value.
    for def in registry.iter() {
        if def.inherit {
            continue;
        }
        pin_layer_key_to_default(&mut draft_layer, &def.key, &def.kind, &def.default);
    }

    let mut layers = Vec::with_capacity(charter_layers.len() + 1);
    layers.push(draft_layer); // TOP (innermost) first
    layers.extend(charter_layers);

    Ok(EffectivePolicy::from_layers(registry.clone(), layers))
}

/// The per-key CHARTER-INHERITED grant admissibility check (II.3.2): at every object
/// where the draft raises `k` above default, the draft value `⊑` the charter's layered
/// effective value.
///
/// Proved PER DRAFT RULE, symbolically, never by sampling a point.
///
/// The identity that makes one rule at a time sufficient: the draft's value at an object
/// is the JOIN of every draft rule covering it, and a join is a least upper bound, so
///
/// ```text
/// join(v_i) ⊑ c(o)   ⟺   every covering v_i ⊑ c(o)
/// ```
///
/// Forward because each `v_i ⊑ join(v_i)`; backward because a `c(o)` above every `v_i`
/// is above their least upper bound. So overlapping draft rules need no reconciling:
/// prove each rule's own value against the charter over its own scope and the join
/// follows.
///
/// This replaced a partition-and-sample check that compared values at ONE witness per
/// region. Neither side is constant across such a region. A charter whose upper layer
/// denied `app*` and re-granted `app` was sampled at `app` - the witness of a glob is
/// built from its literal prefix - so the mask at `appx` was never compared, and a
/// draft re-granting `app*` was admitted and got back exactly the authority the layer
/// removed. The draft side varied too: two draft rules inside one charter region meant
/// admission depended on which was written first.
fn check_grant_key(
    key: &KnobKey,
    draft: &GrantModel,
    charter_layers: &[Layer],
    registry: &PolicyRegistry,
) -> Result<(), ComposeError> {
    let Some(dk) = draft.get(key) else {
        return Ok(()); // draft grants nothing on this key.
    };
    let def = registry
        .get(key)
        .ok_or_else(|| ComposeError::RegistryOrValueMismatch {
            detail: format!("unknown knob {key}"),
        })?;

    for rule in &dk.rules {
        // A rule at or below the knob default raises nothing, so it cannot escalate.
        if leq_value(&def.kind, &rule.value, &def.default)? {
            continue;
        }
        prove_rule_within_charter(key, def, &rule.scope, &rule.value, charter_layers)?;
    }
    Ok(())
}

/// Prove one draft grant rule `(scope, value)` is within the charter everywhere it
/// applies, by walking the layer stack top-first and retiring the region each layer
/// decides.
///
/// A layer DECIDES wherever any of its rules covers, because the query falls through
/// top-down and stops at the first layer that covers (`layered_value_at`). So per layer:
/// prove the covered part of the residual satisfies the draft's value, then subtract
/// everything that layer covers - INCLUDING rules at or below default, which grant
/// nothing but still mask - and carry the rest to the next layer down. A residual that
/// survives every layer sees the knob default, which is below any raising value, so a
/// non-empty residual at the end is an escalation.
///
/// Within a layer the value is a join too, so "the layer satisfies `value` here" means
/// some covering rule is at or above `value` there. That is the same lattice identity
/// read the other way, and it is why this needs no arrangement of overlapping scopes.
fn prove_rule_within_charter(
    key: &KnobKey,
    def: &crate::knob::KnobDef,
    scope: &Scope,
    value: &crate::knob::KnobValue,
    charter_layers: &[Layer],
) -> Result<(), ComposeError> {
    // Scopes an upper layer has already decided. A point one of these covers never
    // reaches a lower layer, so a lower layer sitting below the draft there is not an
    // escalation. Carried as a LIST rather than folded into the region, because the
    // fold is what forces a subtraction the glob algebra often cannot express: `All`
    // minus `app_*` has no representation, so a residual narrowed layer by layer
    // refused charters that plainly compose.
    let mut decided: Vec<&Scope> = Vec::new();

    for layer in charter_layers {
        let Some(km) = layer.grants.keys.get(key) else {
            continue; // this layer is silent on the key, so it decides nothing.
        };

        let mut satisfying: Vec<&Scope> = Vec::new();
        let mut falling_short: Vec<&Scope> = Vec::new();
        for r in &km.rules {
            if leq_value(&def.kind, value, &r.value)? {
                satisfying.push(&r.scope);
            } else {
                falling_short.push(&r.scope);
            }
        }

        // Only a rule BELOW the draft's value can produce an escalation, so the
        // subtraction runs only where one exists. A layer whose every covering rule
        // satisfies costs nothing and cannot fail to be represented.
        for short in &falling_short {
            let suspect = scope.meet(short);
            if matches!(suspect, Scope::Nothing) {
                continue;
            }
            // Escaping = covered by a rule below the draft, not lifted by a sibling
            // rule in the same layer, and not already decided further up.
            let mut exempt: Vec<&Scope> = decided.clone();
            exempt.extend_from_slice(&satisfying);
            let Some(escaping) = subtract_each(&suspect, &exempt) else {
                return Err(ComposeError::UncoveredRegionNotRepresentable { key: key.clone() });
            };
            if !matches!(escaping, Scope::Nothing) {
                return Err(ComposeError::GrantExceedsCharter {
                    key: key.clone(),
                    offending_pattern: render_scope(&escaping),
                });
            }
        }

        for r in &km.rules {
            decided.push(&r.scope);
        }
    }

    // Whatever no charter rule RAISES is at the knob default, which is below any value
    // that reaches here. A rule at or below default covers without lifting, so it does
    // not rescue this region - the subtrahends are the raising rules only.
    let raising = layered_nondefault_grant_rules(charter_layers, key)?;
    let Some(unraised) = subtract_each(scope, &raising) else {
        return Err(ComposeError::UncoveredRegionNotRepresentable { key: key.clone() });
    };
    if !matches!(unraised, Scope::Nothing) {
        return Err(ComposeError::GrantExceedsCharter {
            key: key.clone(),
            offending_pattern: render_scope(&unraised),
        });
    }
    Ok(())
}

/// `region ∖ s` for every `s`, one at a time, never against a `⊔`-materialized union
/// (C-1). `None` when a step cannot be represented and the residual is still non-empty.
///
/// A step the scope algebra cannot express is DEFERRED rather than fatal, because
/// subtraction order matters: `All ∖ app_*` has no glob form, but a later subtrahend of
/// `All` empties the region anyway. Failing on the first unrepresentable step made the
/// answer depend on the order rules happen to sit in, and refused charters that are
/// plainly admissible. Retry until a pass makes no progress, then fail closed.
fn subtract_each(region: &Scope, subtrahends: &[&Scope]) -> Option<Scope> {
    let mut r = region.clone();
    let mut deferred: Vec<&Scope> = subtrahends.to_vec();
    while !deferred.is_empty() && !matches!(r, Scope::Nothing) {
        let before = deferred.len();
        let mut still_deferred = Vec::with_capacity(before);
        for s in deferred {
            match r.difference(s) {
                Difference::Scope(next) => r = next,
                Difference::NotRepresentable => still_deferred.push(s),
            }
        }
        deferred = still_deferred;
        if deferred.len() == before {
            break; // no progress this pass, so another cannot help.
        }
    }
    if !deferred.is_empty() && !matches!(r, Scope::Nothing) {
        return None;
    }
    Some(r)
}

/// Flatten a charter's layer stack to the rules of one kind (for the draft-vs-charter
/// collision checks, which are over the whole charter rule set).
fn flatten_charter(
    layers: &[Layer],
    pred: impl Fn(&RuleKind) -> bool + Copy,
) -> Vec<crate::rule::Rule> {
    layers
        .iter()
        .rev() // outermost first
        .flat_map(|l| {
            let injects = l.injects.iter();
            let validates = l.validates.iter();
            injects.chain(validates)
        })
        .filter(|r| pred(&r.kind))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Enforcement, KnobDef, KnobKind, KnobValue, LoadContext, ObjectModel, ObjectName, Polarity,
        RootCharter,
    };

    const TIMEOUT_KEY: &str = "runtime.timeout_ms";

    fn registry() -> PolicyRegistry {
        PolicyRegistry::empty()
            .with([KnobDef {
                key: KnobKey::parse(TIMEOUT_KEY).expect("test key is valid"),
                kind: KnobKind::UintCharter { hard_floor: 1 },
                polarity: Polarity::Grant,
                default: KnobValue::Uint(1),
                enforcement: Enforcement::Enforced,
                object_model: ObjectModel::Global,
                requires_db_privilege: false,
                inherit: true,
                docs: String::new(),
            }])
            .expect("test registry is valid")
    }

    fn root(registry: &PolicyRegistry, value: u64) -> RootCharter {
        RootCharter::parse_toml(
            &format!(
                r#"policy_version = 1
[[grant]]
key = "{TIMEOUT_KEY}"
value = {value}
scope = "all"
"#
            ),
            registry,
        )
        .expect("root charter loads")
    }

    fn layer(registry: &PolicyRegistry, value: u64) -> PolicyDoc {
        PolicyDoc::parse_toml(
            &format!(
                r#"policy_version = 1
[[grant]]
key = "{TIMEOUT_KEY}"
value = {value}
scope = "all"
"#
            ),
            registry,
            LoadContext::NonRootLayer,
        )
        .expect("non-root layer loads")
    }

    #[test]
    fn effective_policy_is_a_charter_for_another_narrowing_admit() {
        let registry = registry();
        let platform = root(&registry, 30_000);
        let org = layer(&registry, 10_000);
        let app = layer(&registry, 5_000);

        let platform_org = admit(&platform, &org, &registry).expect("org narrows platform");
        let effective = admit(&platform_org, &app, &registry).expect("app narrows org");

        assert_eq!(
            effective.grants(
                &KnobKey::parse(TIMEOUT_KEY).expect("test key is valid"),
                &ObjectName::schema(b"app".to_vec()),
            ),
            Some(KnobValue::Uint(5_000))
        );
    }

    #[test]
    fn later_layer_cannot_re_escalate_past_an_intermediate_narrowing() {
        let registry = registry();
        let platform = root(&registry, 30_000);
        let org = layer(&registry, 10_000);
        let app = layer(&registry, 20_000);

        let platform_org = admit(&platform, &org, &registry).expect("org narrows platform");
        let result = admit(&platform_org, &app, &registry);

        assert!(
            matches!(result, Err(ComposeError::GrantExceedsCharter { .. })),
            "a later layer must be checked against the accumulated narrowing: {result:?}"
        );
    }
}
