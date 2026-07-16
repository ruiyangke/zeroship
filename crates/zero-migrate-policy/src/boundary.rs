//! The trust boundary (II.1 `boundary`, II.3.2) — the SOLE untrusted crossing.
//!
//! [`admit`]`(ceiling, draft)` is the *only* place an UNTRUSTED creator [`PolicyDoc`]
//! meets a TRUSTED, FINALIZED ceiling, so it is the *only* place **escalation** is
//! checked. It lives in its own module — apart from the trusted, total combinators
//! [`overlay`](crate::compose::overlay) / [`restrict`](crate::compose::restrict) and
//! the trusted-side conflict gate
//! [`finalize_ceiling`](crate::compose::finalize_ceiling) — precisely so the one
//! security-bearing operator is unmissable when reading or auditing the crate.
//!
//! `admit` produces a LAYERED [`EffectivePolicy`] — `[draft] over [ceiling]` (H-4) —
//! whose grant query falls through top→down. Grants are **ceiling-inherited,
//! narrow-only, presence-overridden**: a silent draft inherits the ceiling's grant; a
//! draft that NARROWS wins by presence (admitted because a narrow is `⊑` the ceiling);
//! a draft that RAISES above default is admitted only where its value is `⊑` the
//! ceiling's (II.3.2). Obligations/injects/validates union-up (un-droppable). The
//! escalation check computes the uncovered region by ITERATED per-ceiling-rule `∖`
//! subtraction (never a `⊔`-materialized ceiling — the C-1 fix), and rejects a
//! non-representable difference (fail-closed).
//!
//! `admit`'s `ceiling` is a finalized [`AdmitCeiling`] (a [`RootCeiling`], a
//! [`Ceiling`](crate::compose::Ceiling) from `finalize_ceiling`, or an already-composed
//! [`EffectivePolicy`]); an un-finalized [`AssembledCeiling`](crate::compose::AssembledCeiling)
//! deliberately does NOT implement `AdmitCeiling`, so it cannot reach `admit` at the
//! type level (MED).

use crate::compose::{
    check_inject_collisions, check_validate_vs_inject, layered_nondefault_grant_rules,
    layered_value_at, pin_layer_key_to_default, render_scope, rules_of, witness_of, AdmitCeiling,
    ComposeError, EffectivePolicy, GrantModel, Layer, LayerTag,
};
use crate::knob::KnobKey;
use crate::registry::PolicyRegistry;
use crate::rule::RuleKind;
use crate::scope::{Difference, Scope};
use crate::value_order::leq_value;
use crate::PolicyDoc;

/// Ingress of an UNTRUSTED `draft` against a trusted, FINALIZED `ceiling` (II.3.2).
/// Returns a LAYERED [`EffectivePolicy`] (`[draft] over [ceiling]`, H-4) iff the draft
/// escalates nowhere; otherwise a [`ComposeError`] blaming the draft. NO clipping —
/// strict reject only.
///
/// The algorithm, pointwise per (key, object) but decided SYMBOLICALLY:
/// - **Grants — ceiling-inherited, narrow-only, presence-overridden.** For each grant
///   key the draft is admissible iff at every object where the draft RAISES `k` above
///   default (`grantedScope(draft, k)`) the draft value is `⊑` the ceiling's LAYERED
///   effective value. Decided by (1) the UNCOVERED region — `grantedScope(draft,k)`
///   minus each ceiling grant rule's `effective_scope`, iterated ONE RULE AT A TIME
///   (C-1: never a `⊔`-materialized ceiling), where the ceiling's value is `default`
///   so any non-default draft value escalates; and (2) the COVERED region — the draft
///   value at a witness of each partition `⊑` the ceiling's layered effective value
///   there (this arm also catches an upper-ceiling-layer masking a lower grant to
///   default). A not-representable `∖` fails closed.
/// - **Require/Inject/Validate:** union-up across all layers; a ceiling rule is never
///   dropped or narrowed.
/// - **Collisions:** a draft inject colliding with a ceiling inject, or a draft
///   validate contradicting a ceiling inject, rejects at compose time.
///
/// The creatable-scope lint is NOT here — it is a ceiling-side misconfiguration lint
/// run at [`finalize_ceiling`](crate::compose::finalize_ceiling) (MED), and admit's
/// per-key `draft ⊑ ceiling` grant check transitively bounds the draft's creatable
/// region below the ceiling's.
pub fn admit(
    ceiling: &impl AdmitCeiling,
    draft: &PolicyDoc,
    registry: &PolicyRegistry,
) -> Result<EffectivePolicy, ComposeError> {
    let ceiling_layers = ceiling.ceiling_layers(registry)?;
    let draft_grants = GrantModel::build(&draft.rules, registry)?;

    // ── grant check: pointwise draft ⊑ ceiling (ceiling-inherited), per key ─────
    // The keys to check = every key the DRAFT grants (a key the draft is silent on
    // simply inherits the ceiling — no escalation possible).
    for key in draft_grants.keys() {
        check_grant_key(key, &draft_grants, &ceiling_layers, registry)?;
    }

    // ── require/inject/validate collision blame (draft vs ceiling) ──────────────
    let ceiling_injects =
        flatten_ceiling(&ceiling_layers, |k| matches!(k, RuleKind::Inject { .. }));
    let draft_injects = rules_of(&draft.rules, |k| matches!(k, RuleKind::Inject { .. }));
    check_inject_collisions(&ceiling_injects, &draft_injects)?;
    let draft_validates = rules_of(&draft.rules, |k| matches!(k, RuleKind::Validate { .. }));
    check_validate_vs_inject(&ceiling_injects, &draft_validates)?;

    // ── build the layered result: [draft] over [ceiling layers] (H-4) ───────────
    let mut draft_layer = Layer::from_doc(LayerTag::Draft, draft, registry)?;

    // ── non-inheritable POWER GRANTS (KnobDef.inherit == false) ─────────────────
    // A knob marked `inherit = false` must NOT flow to a SILENT draft from the
    // ceiling: "override the platform's injected columns" is a grant a creator earns
    // only by asking for it EXPLICITLY, never by inheritance-by-omission. Since the
    // layered grant query falls through the draft layer to the ceiling, we realize the
    // pin by giving the draft layer a synthetic DEFAULT-valued grant rule over the
    // whole universe (`Scope::All`) for every `inherit = false` key. Presence-override
    // (II.3.2) then makes the draft layer WIN the fall-through EVERYWHERE for that key:
    // where the draft EXPLICITLY granted it (already bounded ⊑ the ceiling by the check
    // above), the loosest-covering join within the draft layer keeps the draft's own
    // value; where the draft is SILENT, only the synthetic default rule covers, so the
    // value is the knob default — never the inherited ceiling value.
    for def in registry.iter() {
        if def.inherit {
            continue;
        }
        pin_layer_key_to_default(&mut draft_layer, &def.key, &def.kind, &def.default);
    }

    let mut layers = Vec::with_capacity(ceiling_layers.len() + 1);
    layers.push(draft_layer); // TOP (innermost) first
    layers.extend(ceiling_layers);

    Ok(EffectivePolicy::from_layers(registry.clone(), layers))
}

/// The per-key CEILING-INHERITED grant admissibility check (II.3.2): at every object
/// where the draft raises `k` above default, the draft value `⊑` the ceiling's layered
/// effective value. Uncovered region by iterated per-ceiling-rule `∖` (C-1);
/// covered-region value comparison at a witness (also catches masking).
fn check_grant_key(
    key: &KnobKey,
    draft: &GrantModel,
    ceiling_layers: &[Layer],
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

    // (1) UNCOVERED region: R := grantedScope(draft,k); for each ceiling grant rule r
    //     on k (across layers), R := R ∖ r.effective_scope — ITERATED, per rule, so
    //     `⊔` never materializes the ceiling side (C-1). Excludes on each ceiling rule
    //     survive (the ∖ construction keeps a subtrahend's holes). A non-representable
    //     ∖ at any step ⇒ reject (fail-closed).
    let ceiling_rules = layered_nondefault_grant_rules(ceiling_layers, key)?;
    let mut r = draft_granted.clone();
    for c_scope in &ceiling_rules {
        match r.difference(c_scope) {
            Difference::Scope(s) => r = s,
            Difference::NotRepresentable => {
                return Err(ComposeError::UncoveredRegionNotRepresentable { key: key.clone() });
            }
        }
    }
    if !matches!(r, Scope::Nothing) {
        // The ceiling value on the uncovered region is `default` (tightest); the
        // draft's granted-scope raises above default there ⇒ escalation.
        return Err(ComposeError::GrantExceedsCeiling {
            key: key.clone(),
            offending_pattern: render_scope(&r),
        });
    }

    // (2) COVERED region: partition the draft's granted scope by each ceiling grant
    //     rule's scope (via ⊓) and compare the draft's value to the ceiling's LAYERED
    //     effective value at a witness of each region. This arm also catches masking:
    //     where an upper ceiling layer narrows k to default, the layered effective
    //     value is default, so a non-default draft value there rejects here.
    for c_scope in &ceiling_rules {
        let region = draft_granted.meet(c_scope);
        if matches!(region, Scope::Nothing) {
            continue;
        }
        let Some(witness) = witness_of(&region) else {
            // A non-empty region we cannot witness concretely: fail closed.
            return Err(ComposeError::UncoveredRegionNotRepresentable { key: key.clone() });
        };
        let dv = dk.value_at(&witness)?;
        let cv = layered_value_at(ceiling_layers, &def.default, key, &witness)?;
        if !leq_value(&def.kind, &dv, &cv)? {
            return Err(ComposeError::GrantExceedsCeiling {
                key: key.clone(),
                offending_pattern: render_scope(&region),
            });
        }
    }
    Ok(())
}

/// Flatten a ceiling's layer stack to the rules of one kind (for the draft-vs-ceiling
/// collision checks, which are over the whole ceiling rule set).
fn flatten_ceiling(
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
