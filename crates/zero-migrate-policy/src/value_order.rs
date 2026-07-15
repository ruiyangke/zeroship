//! The per-knob VALUE order — the second lattice the composition algebra rides on
//! (the first being the scope lattice). Every grant/require value composes by its
//! knob's declared [`KnobKind`], not by hand-written per-facet meets (E2/E3): the
//! order is *derived* from the kind, uniformly, so no facet can drift.
//!
//! For a grant key the security-relevant relation is **`⊑_value`** ("the draft's
//! value is no looser than the ceiling's"): the composer accepts a draft grant at an
//! object only when `draft_value ⊑_value ceiling_value` there (II.3.2). The dual
//! operations are:
//!
//! - **`join_value` (`⊔_value`, the LOOSEST):** aggregates several covering grant
//!   rules within ONE layer — more rules on a key can only loosen (II.3.2). Bool OR,
//!   StrSet ∪, Uint max, OrderedEnum rank-max.
//! - **`meet_value` (`⊓_value`, the TIGHTEST):** clamps two trusted ceilings
//!   pointwise (`restrict`, II.3.2). Bool AND, StrSet ∩, Uint min,
//!   OrderedEnum rank-min.
//! - **`leq_value` (`⊑_value`):** the polarity order — Bool implication
//!   (`false ⊑ true`), StrSet ⊆, Uint ≤, OrderedEnum rank ≤. `a ⊑ b ⟺ join(a,b)=b`
//!   (equivalently `meet(a,b)=a`), the standard lattice identity.
//!
//! `Digest`/`Pinned` values are OPAQUE: no order, only equality — `⊑` is `==`, and
//! `join`/`meet` are only defined (and only ever asked) when the two are equal. The
//! `Pinned` polarity's delegable/non-delegable handling lives in the composer, not
//! here; this module gives the pure value algebra.
//!
//! Every operation here is defined RELATIVE to a [`KnobKind`] (the value's declared
//! shape). A value whose runtime shape does not match its kind is a caller bug — the
//! loader validated every authored value against its kind (`KnobValue::validate_for`)
//! before it ever reached composition — so a mismatch is reported as a structured
//! [`ValueOrderError`] rather than silently mis-ordering (fail-closed).

use std::collections::BTreeSet;

use crate::knob::{KnobKind, KnobValue};

/// A value-order evaluation error: a [`KnobValue`] whose runtime shape does not fit
/// the [`KnobKind`] it is being ordered under. The loader makes this unreachable for
/// authored values; it exists so the algebra is total and fail-closed rather than
/// panicking or silently guessing an order.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ValueOrderError {
    /// The value's shape does not match the kind (e.g. a `Bool` under a `UintCeiling`).
    ShapeMismatch,
    /// An `OrderedEnum` value naming a variant the kind does not declare — it has no
    /// rank, so it cannot be ordered.
    UnknownVariant,
}

/// `a ⊑_value b` under `kind` — is `a` no LOOSER than `b`? (The security direction:
/// a draft value `a` is admissible against a ceiling value `b` iff `a ⊑ b`.)
///
/// - `Bool`: `false ⊑ true` and reflexive; `true ⊑ false` is false (implication).
/// - `StrSet`: subset.
/// - `UintCeiling`: `≤`.
/// - `OrderedEnum`: rank `≤` (tightest→loosest variant order).
/// - `Digest`: equality (opaque, no order).
pub fn leq_value(kind: &KnobKind, a: &KnobValue, b: &KnobValue) -> Result<bool, ValueOrderError> {
    match kind {
        KnobKind::Bool => {
            let (x, y) = (as_bool(a)?, as_bool(b)?);
            // false ⊑ false, false ⊑ true, true ⊑ true, NOT (true ⊑ false).
            Ok(!x || y)
        }
        KnobKind::StrSet => {
            let (x, y) = (as_strset(a)?, as_strset(b)?);
            Ok(x.is_subset(&y))
        }
        KnobKind::UintCeiling { .. } => {
            let (x, y) = (as_uint(a)?, as_uint(b)?);
            Ok(x <= y)
        }
        KnobKind::OrderedEnum { variants } => {
            let (x, y) = (rank(variants, a)?, rank(variants, b)?);
            Ok(x <= y)
        }
        KnobKind::Digest => Ok(as_str(a)? == as_str(b)?),
    }
}

/// `a ⊔_value b` under `kind` — the LOOSEST (least upper bound): the within-layer
/// grant aggregation (II.3.2). Bool OR, StrSet ∪, Uint max, OrderedEnum rank-max.
/// For `Digest`, only defined when equal (opaque) — a mismatch is an error the
/// composer never triggers on the grant path (digests are `Pinned`, not aggregated).
pub fn join_value(
    kind: &KnobKind,
    a: &KnobValue,
    b: &KnobValue,
) -> Result<KnobValue, ValueOrderError> {
    match kind {
        KnobKind::Bool => Ok(KnobValue::Bool(as_bool(a)? || as_bool(b)?)),
        KnobKind::StrSet => {
            let mut u = as_strset(a)?;
            u.extend(as_strset(b)?);
            Ok(KnobValue::StrSet(sorted(u)))
        }
        KnobKind::UintCeiling { .. } => Ok(KnobValue::Uint(as_uint(a)?.max(as_uint(b)?))),
        KnobKind::OrderedEnum { variants } => {
            let (ra, rb) = (rank(variants, a)?, rank(variants, b)?);
            Ok(a_or_b_by_rank(a, b, ra, rb, ra >= rb))
        }
        KnobKind::Digest => {
            if as_str(a)? == as_str(b)? {
                Ok(a.clone())
            } else {
                Err(ValueOrderError::ShapeMismatch)
            }
        }
    }
}

/// `a ⊓_value b` under `kind` — the TIGHTEST (greatest lower bound): the ceiling
/// clamp (`restrict`, II.3.2). Bool AND, StrSet ∩, Uint min, OrderedEnum
/// rank-min. `Digest` only when equal (opaque).
pub fn meet_value(
    kind: &KnobKind,
    a: &KnobValue,
    b: &KnobValue,
) -> Result<KnobValue, ValueOrderError> {
    match kind {
        KnobKind::Bool => Ok(KnobValue::Bool(as_bool(a)? && as_bool(b)?)),
        KnobKind::StrSet => {
            let (x, y) = (as_strset(a)?, as_strset(b)?);
            Ok(KnobValue::StrSet(sorted(x.intersection(&y).cloned().collect())))
        }
        KnobKind::UintCeiling { .. } => Ok(KnobValue::Uint(as_uint(a)?.min(as_uint(b)?))),
        KnobKind::OrderedEnum { variants } => {
            let (ra, rb) = (rank(variants, a)?, rank(variants, b)?);
            Ok(a_or_b_by_rank(a, b, ra, rb, ra <= rb))
        }
        KnobKind::Digest => {
            if as_str(a)? == as_str(b)? {
                Ok(a.clone())
            } else {
                Err(ValueOrderError::ShapeMismatch)
            }
        }
    }
}

// ── shape accessors (fail-closed) ─────────────────────────────────────────────────

fn as_bool(v: &KnobValue) -> Result<bool, ValueOrderError> {
    match v {
        KnobValue::Bool(b) => Ok(*b),
        _ => Err(ValueOrderError::ShapeMismatch),
    }
}

fn as_uint(v: &KnobValue) -> Result<u64, ValueOrderError> {
    match v {
        KnobValue::Uint(u) => Ok(*u),
        _ => Err(ValueOrderError::ShapeMismatch),
    }
}

fn as_str(v: &KnobValue) -> Result<&str, ValueOrderError> {
    match v {
        KnobValue::Str(s) => Ok(s),
        _ => Err(ValueOrderError::ShapeMismatch),
    }
}

fn as_strset(v: &KnobValue) -> Result<BTreeSet<String>, ValueOrderError> {
    match v {
        KnobValue::StrSet(xs) => Ok(xs.iter().cloned().collect()),
        _ => Err(ValueOrderError::ShapeMismatch),
    }
}

/// The 0-based rank of an `OrderedEnum` value in its tightest→loosest `variants`
/// list. `Err(UnknownVariant)` if the value names no declared variant.
fn rank(variants: &[String], v: &KnobValue) -> Result<usize, ValueOrderError> {
    let s = as_str(v)?;
    variants
        .iter()
        .position(|cand| cand == s)
        .ok_or(ValueOrderError::UnknownVariant)
}

/// Pick `a` or `b` by rank comparison, returning the winner as a `KnobValue` clone.
/// `take_a` decides ties toward `a` (both are equal-ranked so either is fine).
fn a_or_b_by_rank(a: &KnobValue, b: &KnobValue, _ra: usize, _rb: usize, take_a: bool) -> KnobValue {
    if take_a {
        a.clone()
    } else {
        b.clone()
    }
}

/// A `StrSet` is stored order-insensitively but rendered deterministically (sorted)
/// so the canonical seal encoding is stable regardless of authored order.
fn sorted(set: BTreeSet<String>) -> Vec<String> {
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bset(items: &[&str]) -> KnobValue {
        KnobValue::StrSet(items.iter().map(|s| (*s).to_string()).collect())
    }

    #[test]
    fn bool_order() {
        let k = KnobKind::Bool;
        assert!(leq_value(&k, &KnobValue::Bool(false), &KnobValue::Bool(true)).unwrap());
        assert!(!leq_value(&k, &KnobValue::Bool(true), &KnobValue::Bool(false)).unwrap());
        assert_eq!(
            join_value(&k, &KnobValue::Bool(false), &KnobValue::Bool(true)).unwrap(),
            KnobValue::Bool(true)
        );
        assert_eq!(
            meet_value(&k, &KnobValue::Bool(true), &KnobValue::Bool(false)).unwrap(),
            KnobValue::Bool(false)
        );
    }

    #[test]
    fn uint_order() {
        let k = KnobKind::UintCeiling { hard_floor: 1 };
        assert!(leq_value(&k, &KnobValue::Uint(60), &KnobValue::Uint(600)).unwrap());
        assert!(!leq_value(&k, &KnobValue::Uint(600), &KnobValue::Uint(60)).unwrap());
        assert_eq!(
            join_value(&k, &KnobValue::Uint(60), &KnobValue::Uint(600)).unwrap(),
            KnobValue::Uint(600)
        );
        assert_eq!(
            meet_value(&k, &KnobValue::Uint(60), &KnobValue::Uint(600)).unwrap(),
            KnobValue::Uint(60)
        );
    }

    #[test]
    fn strset_order() {
        let k = KnobKind::StrSet;
        assert!(leq_value(&k, &bset(&["a"]), &bset(&["a", "b"])).unwrap());
        assert!(!leq_value(&k, &bset(&["a", "c"]), &bset(&["a", "b"])).unwrap());
        assert_eq!(join_value(&k, &bset(&["a"]), &bset(&["b"])).unwrap(), bset(&["a", "b"]));
        assert_eq!(meet_value(&k, &bset(&["a", "b"]), &bset(&["b", "c"])).unwrap(), bset(&["b"]));
    }

    #[test]
    fn ordered_enum_order() {
        // forbid(0) < warn(1) < allow(2).
        let k = KnobKind::OrderedEnum {
            variants: vec!["forbid".into(), "warn".into(), "allow".into()],
        };
        let forbid = KnobValue::Str("forbid".into());
        let warn = KnobValue::Str("warn".into());
        let allow = KnobValue::Str("allow".into());
        assert!(leq_value(&k, &forbid, &allow).unwrap());
        assert!(!leq_value(&k, &allow, &warn).unwrap());
        assert_eq!(join_value(&k, &forbid, &warn).unwrap(), warn); // rank-max
        assert_eq!(meet_value(&k, &allow, &warn).unwrap(), warn); // rank-min
        assert_eq!(
            rank(&["forbid".into(), "warn".into()], &KnobValue::Str("nope".into())),
            Err(ValueOrderError::UnknownVariant)
        );
    }

    #[test]
    fn digest_is_equality_only() {
        let k = KnobKind::Digest;
        let a = KnobValue::Str("deadbeef".into());
        let b = KnobValue::Str("cafef00d".into());
        assert!(leq_value(&k, &a, &a).unwrap());
        assert!(!leq_value(&k, &a, &b).unwrap());
        assert_eq!(join_value(&k, &a, &a).unwrap(), a);
        assert_eq!(meet_value(&k, &a, &a).unwrap(), a);
        assert_eq!(join_value(&k, &a, &b), Err(ValueOrderError::ShapeMismatch));
    }

    #[test]
    fn leq_agrees_with_join_meet_identities() {
        // a ⊑ b ⟺ join(a,b) == b ⟺ meet(a,b) == a, over a small Uint domain.
        let k = KnobKind::UintCeiling { hard_floor: 1 };
        for a in 1u64..=6 {
            for b in 1u64..=6 {
                let (va, vb) = (KnobValue::Uint(a), KnobValue::Uint(b));
                let leq = leq_value(&k, &va, &vb).unwrap();
                assert_eq!(leq, join_value(&k, &va, &vb).unwrap() == vb);
                assert_eq!(leq, meet_value(&k, &va, &vb).unwrap() == va);
            }
        }
    }

    #[test]
    fn shape_mismatch_is_error_not_panic() {
        let k = KnobKind::Bool;
        assert_eq!(leq_value(&k, &KnobValue::Uint(1), &KnobValue::Bool(true)), Err(ValueOrderError::ShapeMismatch));
    }
}
