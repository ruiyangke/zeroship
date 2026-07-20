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
//! charter's (II.3.2). Obligations/injects/validates union-up (un-droppable). The
//! escalation check computes the uncovered region by ITERATED per-charter-rule `∖`
//! subtraction (never a `⊔`-materialized charter — the C-1 fix), and rejects a
//! non-representable difference (fail-closed).
//!
//! `admit`'s `charter` is a finalized [`AdmitCharter`] (a [`RootCharter`], a
//! [`Charter`](crate::compose::Charter) from `finalize_charter`, or an already-composed
//! [`EffectivePolicy`]); an un-finalized [`AssembledCharter`](crate::compose::AssembledCharter)
//! deliberately does NOT implement `AdmitCharter`, so it cannot reach `admit` at the
//! type level (MED).

use crate::compose::{
    check_inject_collisions, check_validate_vs_inject, layered_nondefault_grant_rules,
    layered_value_at, pin_layer_key_to_default, render_scope, rules_of, witness_of, AdmitCharter,
    ComposeError, EffectivePolicy, GrantModel, Layer, LayerTag,
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
///   default (`grantedScope(draft, k)`) the draft value is `⊑` the charter's LAYERED
///   effective value. Decided by (1) the UNCOVERED region — `grantedScope(draft,k)`
///   minus each charter grant rule's `effective_scope`, iterated ONE RULE AT A TIME
///   (C-1: never a `⊔`-materialized charter), where the charter's value is `default`
///   so any non-default draft value escalates; and (2) the COVERED region — the draft
///   value at a witness of each partition `⊑` the charter's layered effective value
///   there (this arm also catches an upper-charter-layer masking a lower grant to
///   default). A not-representable `∖` fails closed.
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
/// effective value. Uncovered region by iterated per-charter-rule `∖` (C-1);
/// covered-region value comparison at a witness (also catches masking).
fn check_grant_key(
    key: &KnobKey,
    draft: &GrantModel,
    charter_layers: &[Layer],
    registry: &PolicyRegistry,
) -> Result<(), ComposeError> {
    let Some(dk) = draft.get(key) else {
        return Ok(()); // draft grants nothing on this key.
    };
    let draft_granted = dk.granted_scope()?;
    if matches!(draft_granted, Scope::Nothing) {
        return Ok(()); // draft raises nothing above default → admissible.
    }

    let def = registry
        .get(key)
        .ok_or_else(|| ComposeError::RegistryOrValueMismatch {
            detail: format!("unknown knob {key}"),
        })?;

    // (1) UNCOVERED region: R := grantedScope(draft,k); for each charter grant rule r
    //     on k (across layers), R := R ∖ r.effective_scope — ITERATED, per rule, so
    //     `⊔` never materializes the charter side (C-1). Excludes on each charter rule
    //     survive (the ∖ construction keeps a subtrahend's holes). A non-representable
    //     ∖ at any step ⇒ reject (fail-closed).
    let charter_rules = layered_nondefault_grant_rules(charter_layers, key)?;
    let mut r = draft_granted.clone();
    for c_scope in &charter_rules {
        match r.difference(c_scope) {
            Difference::Scope(s) => r = s,
            Difference::NotRepresentable => {
                return Err(ComposeError::UncoveredRegionNotRepresentable { key: key.clone() });
            }
        }
    }
    if !matches!(r, Scope::Nothing) {
        // The charter value on the uncovered region is `default` (tightest); the
        // draft's granted-scope raises above default there ⇒ escalation.
        return Err(ComposeError::GrantExceedsCharter {
            key: key.clone(),
            offending_pattern: render_scope(&r),
        });
    }

    // (2) COVERED region: partition the draft's granted scope by each charter grant
    //     rule's scope (via ⊓) and compare the draft's value to the charter's LAYERED
    //     effective value at a witness of each region. This arm also catches masking:
    //     where an upper charter layer narrows k to default, the layered effective
    //     value is default, so a non-default draft value there rejects here.
    for c_scope in &charter_rules {
        let region = draft_granted.meet(c_scope);
        if matches!(region, Scope::Nothing) {
            continue;
        }
        let Some(witness) = witness_of(&region) else {
            // A non-empty region we cannot witness concretely: fail closed.
            return Err(ComposeError::UncoveredRegionNotRepresentable { key: key.clone() });
        };
        let dv = dk.value_at(&witness)?;
        let cv = layered_value_at(charter_layers, &def.default, key, &witness)?;
        if !leq_value(&def.kind, &dv, &cv)? {
            return Err(ComposeError::GrantExceedsCharter {
                key: key.clone(),
                offending_pattern: render_scope(&region),
            });
        }
    }
    Ok(())
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
