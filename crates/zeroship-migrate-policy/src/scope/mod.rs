//! The scope lattice - the security-core primitive of the policy system.
//!
//! Every policy rule (grant / require / inject / validate) carries a name
//! [`Scope`]: `Nothing` (the bottom of the lattice, matches no object), `All`
//! (the top, matches every object), or a proper `Of { include, exclude }` over
//! schema and schema-qualified-table [`Pattern`]s. The lattice operations -
//! [`Scope::subset`] (containment), [`Scope::meet`] (greatest lower bound),
//! [`Scope::join`] (least upper bound), [`Scope::difference`] (set subtraction) -
//! are the mechanism the composition algebra (II.3.2) uses to prove no-escalation.
//!
//! **The single most dangerous bug in a scope model is conflating the empty set
//! with the universe.** So bottom and top are distinguished *values*, and
//! `Of{include}` is NEVER empty: the
//! [`Scope::of`] constructor errors on empty include and normalizes an
//! exclude-covers-include scope to `Nothing`. A disjoint meet therefore produces
//! `Nothing`, never the universe.
//!
//! Correctness is proven by the brute-force oracle in `oracle`: over a bounded
//! universe of names and globs it asserts every lattice op against a direct
//! ground-truth matcher: the meet is EXACT, [`Scope::subset`] holds exactly when
//! one object set is contained in the other, the join covers at least the union,
//! and the difference covers at least the set subtraction or is rejected outright
//! (never a strict subset - the escalation direction is provably impossible).

pub mod glob;
pub mod pattern;

#[cfg(test)]
mod oracle;

use std::collections::BTreeSet;

pub use glob::SegGlob;
pub use pattern::{normalize_object_name, Pattern};

use pattern::{intersect_pattern, pattern_covers, ObjectName};

/// A name scope: `Nothing` (bottom), `All` (top), or a proper `Of{include, exclude}`.
///
/// `include` is a NON-EMPTY union of patterns (empty include is illegal - use
/// `Nothing`). `exclude` subtracts; **exclude wins on overlap**.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Scope {
    /// Bottom - the empty scope. Matches NO object. The identity of the join and
    /// the annihilator of the meet.
    Nothing,
    /// Top - every object. The identity of the meet and the annihilator of the
    /// join. The canonical loud
    /// token for "everything"; `Of{include:["*"]}` is *semantically* equal but is
    /// NOT this token (the two loud legality gates in II.2.5 use the syntactic
    /// `== All` test, which this lattice does not model).
    All,
    /// A proper scope: `include` non-empty, `exclude` subtracts (exclude-wins).
    /// Patterns are stored in NORMALIZED two-segment form (schema->`P.*`).
    Of {
        include: Vec<Pattern>,
        exclude: Vec<Pattern>,
    },
}

/// A scope construction / normalization error.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ScopeError {
    /// `Of` was constructed with an empty `include`. The author must write
    /// `Nothing` (deny) or `All` (universe) explicitly - there is no empty-vector
    /// spelling of either extreme, so the bottom/top collision is unrepresentable.
    EmptyInclude,
}

/// The result of a difference that the caller MUST treat as fail-closed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Difference {
    /// The exact-or-over-approximated difference, safe to consume. `Objects(this)`
    /// is guaranteed to contain every object in `Objects(A)` but not in
    /// `Objects(B)` (never a strict subset of that set).
    Scope(Scope),
    /// The difference is not cleanly representable within the `Scope` type; the
    /// caller REJECTS (`UncoveredRegionNotRepresentable`). Fail-closed: the engine
    /// never guesses that an un-representable difference is empty.
    NotRepresentable,
}

impl Scope {
    /// Smart constructor for a proper scope. Empty `include` is a hard error;
    /// an exclude that covers every included object normalizes to `Nothing`.
    ///
    /// The result is always a *normalized* scope (see [`Scope::normalize`]).
    pub fn of(include: Vec<Pattern>, exclude: Vec<Pattern>) -> Result<Scope, ScopeError> {
        if include.is_empty() {
            return Err(ScopeError::EmptyInclude);
        }
        Ok(Scope::Of { include, exclude }.normalize())
    }

    /// Convenience: a proper scope with no excludes.
    pub fn include(include: Vec<Pattern>) -> Result<Scope, ScopeError> {
        Self::of(include, Vec::new())
    }

    // -- ground truth (testing only) -------------------------------------------

    /// `objects_membership` - does this scope match the concrete object `n`?
    ///
    /// This is the direct membership function used as ground truth by the oracle
    /// (is `n` a member of `Objects(scope)`) and by enforcement-time matching. It is NOT derived
    /// from the lattice ops. Exclude wins: an object excluded by any exclude
    /// pattern is out, regardless of includes.
    #[must_use]
    pub fn objects_membership(&self, n: &ObjectName) -> bool {
        match self {
            Scope::Nothing => false,
            Scope::All => true,
            Scope::Of { include, exclude } => {
                include.iter().any(|p| p.matches(n)) && !exclude.iter().any(|p| p.matches(n))
            }
        }
    }

    /// Does this scope denote the whole universe (is `Objects(self)` every object)?
    ///
    /// True for `All` and for an `Of` whose includes cover `*.*` with no clipping
    /// exclude. SOUND: a `true` guarantees universality (used only where a false
    /// negative conservative-rejects - the "`All` is a subset of `C`"
    /// biconditional). We recognise
    /// the canonical universe-include: some include pattern is `*.*` (covers every
    /// object) and no exclude overlaps it.
    #[must_use]
    fn denotes_universe(&self) -> bool {
        match self {
            Scope::All => true,
            Scope::Nothing => false,
            Scope::Of { include, exclude } => {
                let universe_pat = Pattern::universe();
                include
                    .iter()
                    .any(|p| pattern_covers(p, &universe_pat) && pattern_covers(&universe_pat, p))
                    && exclude.is_empty()
            }
        }
    }

    // -- normalize -------------------------------------------------------------

    /// Fold to canonical form: an `Of` with empty include -> `Nothing`; an `Of`
    /// whose excludes cover every included object -> `Nothing`. Idempotent.
    ///
    /// Note: we deliberately do NOT canonicalize `Of{include:["*"], exclude:[]}`
    /// to `All` - the two are semantically equal but the security legality gates
    /// (II.2.5) distinguish them syntactically. Keeping `Of{["*"]}` as-is is
    /// correct for the lattice (which is defined over `Objects`).
    #[must_use]
    pub fn normalize(self) -> Scope {
        match self {
            Scope::Nothing => Scope::Nothing,
            Scope::All => Scope::All,
            Scope::Of { include, exclude } => {
                if include.is_empty() {
                    return Scope::Nothing;
                }
                // Drop any include pattern wholly covered by some exclude pattern
                // (that include contributes nothing). If ALL includes vanish,
                // the scope is empty.
                let live: Vec<Pattern> = include
                    .iter()
                    .filter(|inc| !exclude.iter().any(|exc| pattern_covers(exc, inc)))
                    .cloned()
                    .collect();
                if live.is_empty() {
                    return Scope::Nothing;
                }
                // Dedup includes; drop excludes irrelevant to the surviving
                // includes (an exclude disjoint from every include does nothing).
                let include = dedup(live);
                let exclude: Vec<Pattern> = dedup(exclude)
                    .into_iter()
                    .filter(|exc| {
                        include
                            .iter()
                            .any(|inc| !intersect_pattern(inc, exc).is_empty())
                    })
                    .collect();
                Scope::Of { include, exclude }
            }
        }
    }

    // -- subset (SOUND: may conservative-reject) -------------------------------

    /// Is every object `self` denotes also denoted by `other`?
    ///
    /// SOUND: a `true` result guarantees containment; a `false` may be a
    /// conservative reject (the sanctioned direction for `admit`). The
    /// exclude-aware decision procedure (II.3.1) is:
    ///   1. every `self` include-region (minus self's excludes) is covered by some
    ///      single `other` include pattern, AND
    ///   2. no `other` exclude clips a region `self` still includes.
    #[must_use]
    pub fn subset(&self, other: &Scope) -> bool {
        match (self, other) {
            (Scope::Nothing, _) => true,
            (_, Scope::All) => true,
            // `All` is a subset of C iff C denotes the universe.
            // `Of{include:["*"]}` is semantically `All` (II.2.3), so the lattice
            // containment - defined over `Objects` - must accept it, even though
            // the security legality gates
            // (which this lattice does not model) use the syntactic `== All` test.
            (Scope::All, _) => other.denotes_universe(),
            // `self` is a subset of `Nothing` iff `Objects(self)` is empty. Every
            // normalized scope with an empty object set IS `Nothing` (the
            // constructor/normalize fold empty-object `Of` to `Nothing`), so the
            // only pair that holds is `Nothing` against `Nothing`.
            (_, Scope::Nothing) => matches!(self, Scope::Nothing),
            (
                Scope::Of {
                    include: di,
                    exclude: de,
                },
                Scope::Of {
                    include: ci,
                    exclude: ce,
                },
            ) => {
                for d in di {
                    // The region of `d` that self actually keeps = d minus de.
                    // For each such region we need coverage by some c AND
                    // disjointness from every ce (unless de already carves it).
                    if !region_covered_and_clear(d, de, ci, ce) {
                        return false;
                    }
                }
                true
            }
        }
    }

    // -- meet (EXACT) ----------------------------------------------------------

    /// The greatest lower bound of `self` and `other` (EXACT): `Objects(result)`
    /// is exactly the intersection of `Objects(self)` and `Objects(other)`.
    #[must_use]
    pub fn meet(&self, other: &Scope) -> Scope {
        match (self, other) {
            (Scope::Nothing, _) | (_, Scope::Nothing) => Scope::Nothing,
            (Scope::All, s) | (s, Scope::All) => s.clone(),
            (
                Scope::Of {
                    include: ai,
                    exclude: ae,
                },
                Scope::Of {
                    include: bi,
                    exclude: be,
                },
            ) => {
                // include = the union over every a in ai and b in bi of their
                // pairwise pattern intersection, flattened.
                let mut include: Vec<Pattern> = Vec::new();
                for a in ai {
                    for b in bi {
                        include.extend(intersect_pattern(a, b));
                    }
                }
                // exclude = ae union be (exclude-wins: unioning excludes only shrinks)
                let mut exclude = ae.clone();
                exclude.extend(be.iter().cloned());
                Scope::Of {
                    include: dedup(include),
                    exclude: dedup(exclude),
                }
                .normalize()
            }
        }
    }

    // -- join (conservative upwards: may over-approximate) ---------------------

    /// The least upper bound of `self` and `other`, allowed to OVER-approximate:
    /// `Objects(result)` contains at least the union of `Objects(self)` and
    /// `Objects(other)`.
    ///
    /// Every consumer aggregates scopes UPWARD, where "cover at least the union"
    /// is the safe direction: the `grantedScope` and `coveredScope` accumulations
    /// and the effective creatable region in `crate::compose` (II.3.2), plus the
    /// two-term construction in [`Scope::difference`]. Naive `exclude = ea intersect eb`
    /// over-includes; we then subtract from the joined excludes any exclude region
    /// now re-admitted by the other side's include, keeping the result a sound
    /// upper bound.
    #[must_use]
    pub fn join(&self, other: &Scope) -> Scope {
        match (self, other) {
            (Scope::All, _) | (_, Scope::All) => Scope::All,
            (Scope::Nothing, s) | (s, Scope::Nothing) => s.clone(),
            (
                Scope::Of {
                    include: ai,
                    exclude: ae,
                },
                Scope::Of {
                    include: bi,
                    exclude: be,
                },
            ) => {
                let include = dedup([ai.clone(), bi.clone()].concat());
                // Start from ea intersect eb (only objects BOTH sides exclude can stay
                // excluded), then drop any exclude that overlaps the OTHER side's
                // include (that side re-admits it). Keeping an exclude that still
                // overlaps a live include would UNDER-include -> unsafe; dropping it
                // OVER-includes -> the sanctioned upward direction.
                let mut exclude: Vec<Pattern> = Vec::new();
                for ea_p in ae {
                    for eb_p in be {
                        exclude.extend(intersect_pattern(ea_p, eb_p));
                    }
                }
                // Conservative repair: if any surviving exclude still overlaps a
                // live include from either side, drop it (over-include, which is
                // the safe upward direction).
                let exclude: Vec<Pattern> = dedup(exclude)
                    .into_iter()
                    .filter(|exc| {
                        !include
                            .iter()
                            .any(|inc| !intersect_pattern(inc, exc).is_empty())
                    })
                    .collect();
                Scope::Of { include, exclude }.normalize()
            }
        }
    }

    // -- difference (OVER-approx or reject - NEVER under-approx) [C1 FIX] ------

    /// The objects `self` grants that `other` does NOT cover.
    ///
    /// **[C1 FIX]** The result MUST over-approximate or reject - NEVER
    /// under-approximate. `Objects(result)` contains at least every object in
    /// `Objects(self)` but not in `Objects(other)` (an
    /// over-approx can only turn an accept into a reject; an under-approx would let
    /// a genuinely-uncovered region compute empty -> wrongly ACCEPT an escalation).
    ///
    /// The doc's original "A minus B is `Of{A.include, A.exclude union B.include}`"
    /// silently drops `B.exclude`, under-approximating (it carves out
    /// `B.exclude`'s holes that A-minus-B must KEEP). We restore them: the
    /// difference is the join of two terms:
    ///
    /// ```text
    /// A minus B  =  join( A with B.include unioned into excludes,  [the doc's term]
    ///                     meet(A, Of{ include: B.exclude }) )      [the dropped holes]
    /// ```
    ///
    /// The first term is `Objects(A)` minus `Objects(B.include)`. The second is
    /// the intersection of `Objects(A)` and `Objects(B.exclude)`, the part of A
    /// that B excluded and so still belongs to A-minus-B. Their union contains at
    /// least `Objects(A)` minus `Objects(B)`, and equals it when B has no
    /// excludes. The join may over-approximate (safe). If the construction is
    /// not cleanly representable, we return [`Difference::NotRepresentable`].
    #[must_use]
    pub fn difference(&self, other: &Scope) -> Difference {
        match (self, other) {
            // Subtracting the universe leaves nothing.
            (_, Scope::All) => Difference::Scope(Scope::Nothing),
            // Subtracting the empty scope leaves A unchanged.
            (_, Scope::Nothing) => Difference::Scope(self.clone()),
            // The empty scope minus anything is empty. The universe minus B is not
            // representable in general (the complement of a glob is not a glob)
            // unless B is empty or the universe, both handled above - so reject.
            (Scope::Nothing, _) => Difference::Scope(Scope::Nothing),
            (Scope::All, _) => Difference::NotRepresentable,
            (
                Scope::Of {
                    include: ai,
                    exclude: ae,
                },
                Scope::Of {
                    include: bi,
                    exclude: be,
                },
            ) => {
                // Term 1: A with B.include folded into excludes.
                let term1 = Scope::Of {
                    include: ai.clone(),
                    exclude: dedup([ae.clone(), bi.clone()].concat()),
                }
                .normalize();

                // Term 2: meet(A, Of{include: B.exclude}) - the holes B carved out.
                let term2 = if be.is_empty() {
                    Scope::Nothing
                } else {
                    // Build Of{include: be} without excludes; meet with A.
                    match Scope::include(be.clone()) {
                        Ok(b_holes) => self.meet(&b_holes),
                        Err(_) => Scope::Nothing, // be empty => no holes
                    }
                };

                Difference::Scope(term1.join(&term2))
            }
        }
    }
}

/// Dedup a pattern vec preserving first-seen order.
fn dedup(patterns: Vec<Pattern>) -> Vec<Pattern> {
    let mut seen: BTreeSet<Pattern> = BTreeSet::new();
    patterns
        .into_iter()
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

/// The exclude-aware containment check per include-region (II.3.1 steps 2 & 3), SOUND.
///
/// For a self include-pattern `d` (whose kept region is `d` minus self's excludes
/// `de`): return true iff every part of that kept region is covered by some single
/// `other` include drawn from `ci` AND avoids every `other` exclude drawn from
/// `ce` (or the overlap is already carved out by `de`).
fn region_covered_and_clear(d: &Pattern, de: &[Pattern], ci: &[Pattern], ce: &[Pattern]) -> bool {
    // Step 2: coverage. The whole of `d` must be covered by SOME single c
    // (conservative: no union-cover proof). If `d` is fully carved by de, it is
    // vacuously fine - but we approximate "carved" conservatively: only when some
    // single de pattern covers d. Otherwise require a covering c.
    let d_fully_carved = de.iter().any(|e| pattern_covers(e, d));
    if d_fully_carved {
        return true;
    }
    let covered = ci.iter().any(|c| pattern_covers(c, d));
    if !covered {
        return false;
    }
    // Step 3: disjointness from every other-exclude. If some ce clips d's kept
    // region and self does not carve that clip out, reject.
    for e in ce {
        let overlap = intersect_pattern(d, e);
        if overlap.is_empty() {
            continue; // ce disjoint from d - fine.
        }
        // The overlap objects are clipped by `other`. self keeps them unless de
        // covers the overlap. Conservative-sound: require that EVERY overlap
        // pattern is covered by some single de pattern; else reject.
        let carved = overlap
            .iter()
            .all(|o| de.iter().any(|dexc| pattern_covers(dexc, o)));
        if !carved {
            return false;
        }
    }
    true
}
