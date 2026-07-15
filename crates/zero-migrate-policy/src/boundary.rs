//! The trust boundary (II.1 `boundary`, II.3.2) — the SOLE untrusted crossing.
//!
//! `admit(ceiling, draft)` is the *only* place an UNTRUSTED creator [`PolicyDoc`]
//! meets a TRUSTED ceiling, so it is the *only* place **escalation** is checked. It
//! lives in its own module — apart from the trusted, total combinators
//! [`overlay`](crate::compose::overlay) / [`restrict`](crate::compose::restrict) and
//! the trusted-side conflict gate
//! [`finalize_ceiling`](crate::compose::finalize_ceiling) — precisely so the one
//! security-bearing operator is unmissable when reading or auditing the crate.
//!
//! `admit` returns `Ok(EffectivePolicy)` iff the draft escalates nothing; otherwise
//! a [`ComposeError`] blaming the draft. It takes a **finalized** [`Ceiling`] (a
//! [`RootCeiling`] or an already-composed [`EffectivePolicy`]) so an un-finalized
//! assembled ceiling cannot reach it at the type level.

use crate::compose::{
    check_inject_collisions, check_validate_vs_inject, render_scope, rules_of, witness_of, Ceiling,
    ComposeError, EffectivePolicy, GrantModel, CREATE_TABLE_KEY,
};
use crate::knob::KnobKey;
use crate::registry::PolicyRegistry;
use crate::rule::{Rule, RuleKind};
use crate::scope::{Difference, Scope};
use crate::value_order::leq_value;
use crate::PolicyDoc;

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
pub fn admit(
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

    Ok(EffectivePolicy::from_parts(registry.clone(), draft_grants, requires, injects, validates))
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
    // and `admit` already proved draft ⊑ ceiling on this key).
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
