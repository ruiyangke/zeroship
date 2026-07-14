//! The brute-force oracle — the real correctness gate for the scope lattice.
//!
//! This module replaces prose review of the glob algebra. It enumerates a bounded
//! universe of concrete object names and a bounded universe of scopes, computes
//! ground-truth object sets with a DIRECT matcher (never the lattice ops), and
//! asserts every lattice operation against ground truth:
//!
//! - `Objects(a ⊓ b) == Objects(a) ∩ Objects(b)`  — EXACT.
//! - `a ⊑ b  ⟺  Objects(a) ⊆ Objects(b)`.
//! - `Objects(a ⊔ b) ⊇ Objects(a) ∪ Objects(b)`  — sanctioned over-approx.
//! - `Objects(a ∖ b) ⊇ Objects(a) \ Objects(b)` OR rejected — NEVER a strict
//!   subset (the escalation direction is provably impossible).
//! - `normalize` idempotent; any `Of{include:[]}` path yields `Nothing`.
//!
//! Alphabet `{a, b, _}` (the `_` corner where segment/overlap edge cases live),
//! segment length ≤ 3, names of one or two segments. The glob universe is every
//! single-`*` (and literal) segment glob up to that size; scopes are built from a
//! bounded set of one/two-segment patterns plus `Nothing`/`All`.

use std::collections::BTreeSet;

use super::glob::{intersect_seg, SegGlob};
use super::pattern::{ObjectName, Pattern};
use super::{Difference, Scope};

// ── Universe of concrete names ────────────────────────────────────────────────

const ALPHABET: [u8; 3] = [b'a', b'b', b'_'];
const MAX_SEG_LEN: usize = 3;

/// All non-empty byte strings over the alphabet up to `MAX_SEG_LEN`.
fn all_segments() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for len in 1..=MAX_SEG_LEN {
        gen_strings(len, &mut Vec::new(), &mut out);
    }
    out
}

fn gen_strings(len: usize, cur: &mut Vec<u8>, out: &mut Vec<Vec<u8>>) {
    if cur.len() == len {
        out.push(cur.clone());
        return;
    }
    for &c in &ALPHABET {
        cur.push(c);
        gen_strings(len, cur, out);
        cur.pop();
    }
}

/// The bounded universe 𝒰 of concrete object names: every schema object and every
/// schema.table object over short-segment names. (Kept small so the O(N²·|𝒰|)
/// property sweep stays fast.)
fn universe() -> Vec<ObjectName> {
    // Segment names up to length 3 over {a,b,_} so infix globs like `a*a` (which
    // need a ≥2-char witness, and whose overlap corners live at length 3) are
    // actually exercised by the property sweep, not just short-circuited.
    let segs: Vec<Vec<u8>> = universe_name_segments();
    let mut out = Vec::new();
    for s in &segs {
        out.push(ObjectName::schema(s.clone()));
        for t in &segs {
            out.push(ObjectName::table(s.clone(), t.clone()));
        }
    }
    out
}

/// Segment names (≤3 chars over `{a,b,_}`) used to build the concrete name
/// universe for the scope property sweep. Length 3 is where the shared-overlap
/// corners of infix×infix (`a*a ∩ a*a`, `a_*`/`*_a` overlaps) first appear.
fn universe_name_segments() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for len in 1..=3 {
        gen_strings(len, &mut Vec::new(), &mut out);
    }
    out
}

// ── Universe of segment globs ─────────────────────────────────────────────────

/// Every single-`*` (and literal) segment glob whose literal pieces are short.
fn all_seg_globs() -> Vec<SegGlob> {
    let mut out = Vec::new();
    out.push(SegGlob::star());
    // literals
    for s in short_literals() {
        out.push(SegGlob::literal(s));
    }
    // prefix p*, suffix *s, infix p*s over short p and s (each possibly empty,
    // but not both — that's `*` already covered).
    let lits = short_literals();
    let with_empty: Vec<Vec<u8>> = std::iter::once(Vec::new()).chain(lits).collect();
    for p in &with_empty {
        for s in &with_empty {
            if p.is_empty() && s.is_empty() {
                continue; // that's `*`
            }
            out.push(SegGlob::infix(p.clone(), s.clone()));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Short literal pieces for glob prefixes/suffixes (≤2 chars).
fn short_literals() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for len in 1..=2 {
        gen_strings(len, &mut Vec::new(), &mut out);
    }
    out
}

// ── Universe of scopes ────────────────────────────────────────────────────────

/// A bounded set of two-segment patterns for building scopes: a handful of schema
/// and table globs combined, plus schema-only patterns.
fn pattern_pool() -> Vec<Pattern> {
    // Keep the pattern glob pool small so scope-pair sweeps stay tractable.
    let glob_pool: Vec<SegGlob> = vec![
        SegGlob::star(),
        SegGlob::literal(b"a".to_vec()),
        SegGlob::literal(b"b".to_vec()),
        SegGlob::literal(b"a_".to_vec()),
        SegGlob::infix(b"a".to_vec(), Vec::new()),  // a*
        SegGlob::infix(Vec::new(), b"a".to_vec()),  // *a
        SegGlob::infix(b"a".to_vec(), b"a".to_vec()), // a*a
    ];
    let mut out = Vec::new();
    // schema-only patterns (normalize to P.*)
    for g in &glob_pool {
        out.push(Pattern::schema(g.clone()));
    }
    // two-segment patterns (a subset — schema glob × table glob)
    for sg in &glob_pool {
        for tg in &glob_pool {
            out.push(Pattern::table(sg.clone(), tg.clone()));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A bounded universe of scopes for the pair sweep: `Nothing`, `All`, and a
/// selection of proper `Of` scopes built from small include/exclude pattern sets.
fn scope_universe() -> Vec<Scope> {
    let pats = pattern_pool();
    let mut out = vec![Scope::Nothing, Scope::All];

    // Single-include scopes (with 0 or 1 excludes) — the workhorses.
    for inc in &pats {
        // no exclude
        if let Ok(s) = Scope::include(vec![inc.clone()]) {
            out.push(s);
        }
        // one exclude (only a small excl pool to bound the blowup)
        for exc in excl_pool() {
            if let Ok(s) = Scope::of(vec![inc.clone()], vec![exc.clone()]) {
                out.push(s);
            }
        }
    }

    // A few two-include scopes to exercise unions.
    let two_inc: Vec<Pattern> = vec![
        Pattern::schema(SegGlob::literal(b"a".to_vec())),
        Pattern::schema(SegGlob::literal(b"b".to_vec())),
    ];
    if let Ok(s) = Scope::include(two_inc.clone()) {
        out.push(s);
    }
    if let Ok(s) = Scope::of(two_inc, vec![Pattern::table(
        SegGlob::literal(b"a".to_vec()),
        SegGlob::literal(b"a".to_vec()),
    )]) {
        out.push(s);
    }

    dedup_scopes(out)
}

/// A small exclude pattern pool.
fn excl_pool() -> Vec<Pattern> {
    vec![
        Pattern::schema(SegGlob::literal(b"a".to_vec())),          // a.*
        Pattern::table(SegGlob::literal(b"a".to_vec()), SegGlob::literal(b"a".to_vec())), // a.a
        Pattern::table(SegGlob::star(), SegGlob::infix(b"a".to_vec(), Vec::new())),       // *.a*
        Pattern::schema(SegGlob::infix(b"a".to_vec(), Vec::new())),                        // a*.*
    ]
}

fn dedup_scopes(v: Vec<Scope>) -> Vec<Scope> {
    let mut seen: Vec<Scope> = Vec::new();
    for s in v {
        if !seen.contains(&s) {
            seen.push(s);
        }
    }
    seen
}

// ── Ground truth ──────────────────────────────────────────────────────────────

/// `Objects(scope)` over the bounded universe — the DIRECT matcher, ground truth.
fn objects(scope: &Scope, univ: &[ObjectName]) -> BTreeSet<usize> {
    univ.iter()
        .enumerate()
        .filter(|(_, n)| scope.objects_membership(n))
        .map(|(i, _)| i)
        .collect()
}

// ══════════════════════════════════════════════════════════════════════════════
// The property sweep
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn oracle_lattice_properties_hold_over_bounded_universe() {
    let univ = universe();
    let scopes = scope_universe();

    // Guard against the universe silently degenerating to something trivial (which
    // would make every assertion vacuous). These are floors, not exact counts.
    assert!(univ.len() >= 500, "name universe too small: {}", univ.len());
    assert!(scopes.len() >= 100, "scope universe too small: {}", scopes.len());

    // Precompute ground-truth object sets once.
    let obj: Vec<BTreeSet<usize>> = scopes.iter().map(|s| objects(s, &univ)).collect();

    for (i, a) in scopes.iter().enumerate() {
        // normalize idempotent
        let n1 = a.clone().normalize();
        let n2 = n1.clone().normalize();
        assert_eq!(n1, n2, "normalize not idempotent for {a:?}");
        // normalize preserves Objects
        assert_eq!(
            objects(&n1, &univ),
            obj[i],
            "normalize changed Objects for {a:?}"
        );

        for (j, b) in scopes.iter().enumerate() {
            let oa = &obj[i];
            let ob = &obj[j];

            // ── ⊓ EXACT ──────────────────────────────────────────────────────
            let meet = a.meet(b);
            let om: BTreeSet<usize> = oa.intersection(ob).copied().collect();
            assert_eq!(
                objects(&meet, &univ),
                om,
                "MEET not exact:\n a={a:?}\n b={b:?}\n meet={meet:?}"
            );

            // ── ⊑  ⟺  ⊆ ──────────────────────────────────────────────────────
            let sub = a.subset(b);
            let is_subset = oa.is_subset(ob);
            assert_eq!(
                sub, is_subset,
                "SUBSET disagrees with ⊆:\n a={a:?}\n b={b:?}\n subset()={sub} ⊆={is_subset}"
            );

            // ── ⊔ ⊇ ∪ (over-approx sanctioned) ───────────────────────────────
            let join = a.join(b);
            let oj: BTreeSet<usize> = oa.union(ob).copied().collect();
            assert!(
                oj.is_subset(&objects(&join, &univ)),
                "JOIN under-approximates ∪:\n a={a:?}\n b={b:?}\n join={join:?}"
            );

            // ── ∖ ⊇ \  OR reject — NEVER a strict subset ─────────────────────
            let diff = a.difference(b);
            let od: BTreeSet<usize> = oa.difference(ob).copied().collect();
            match diff {
                Difference::NotRepresentable => { /* fail-closed reject — allowed */ }
                Difference::Scope(d) => {
                    let odiff = objects(&d, &univ);
                    assert!(
                        od.is_subset(&odiff),
                        "DIFFERENCE under-approximates (ESCALATION!):\n a={a:?}\n b={b:?}\n \
                         diff={d:?}\n missing={:?}",
                        od.difference(&odiff).collect::<Vec<_>>()
                    );
                }
            }
        }
    }
}

/// The `∩seg` glob-intersection primitive, proven EXACT over the full glob
/// universe and the concrete-segment universe.
#[test]
fn oracle_intersect_seg_is_exact() {
    let globs = all_seg_globs();
    let segs = all_segments();

    for a in &globs {
        for b in &globs {
            let result = intersect_seg(a, b);
            // Ground truth: the set of concrete segments matching BOTH.
            for w in &segs {
                let truth = a.matches(w) && b.matches(w);
                let got = result.iter().any(|g| g.matches(w));
                assert_eq!(
                    truth, got,
                    "∩seg not exact on segment {:?}:\n a={a:?}\n b={b:?}\n result={result:?}",
                    String::from_utf8_lossy(w)
                );
            }
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Pinned enumerated cases (expected sets computed FROM the oracle, then pinned)
// ══════════════════════════════════════════════════════════════════════════════

/// Helper: the EXACT `∩seg` result as a set of rendered strings.
fn seg_intersect_rendered(a: &str, b: &str) -> BTreeSet<String> {
    let ga = SegGlob::parse(a).unwrap();
    let gb = SegGlob::parse(b).unwrap();
    intersect_seg(&ga, &gb).iter().map(SegGlob::render).collect()
}

fn rendered(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn pinned_a_star_x() {
    // a_* ∩seg *_x  →  { a_x, a_*_x }
    assert_eq!(seg_intersect_rendered("a_*", "*_x"), rendered(&["a_x", "a_*_x"]));
}

#[test]
fn pinned_a_a_shared_overlap() {
    // [H2 FIX] a_a_* ∩seg *_a_a. Prefix p = "a_a_", suffix s = "_a_a",
    // base = |p|+|s| = 8, floor = max(4,4) = 4. Overlaps o (p's last o bytes ==
    // s's first o bytes), witness length = base − o:
    //   o=1: p[-1..]="_" == s[..1]="_" ✓ → "a_a_" · s[1..]="a_a"  = "a_a_a_a" (len 7)
    //   o=2: p[-2..]="a_" == s[..2]="_a" ✗
    //   o=3: p[-3..]="_a_" == s[..3]="_a_" ✓ → "a_a_" · s[3..]="a" = "a_a_a"   (len 5)
    //   o=4: p[-4..]="a_a_" == s[..4]="_a_a" ✗
    // Plus the infix p*s = "a_a_*_a_a" for the non-overlapping witnesses.
    // ⇒ true set { a_a_a, a_a_a_a, a_a_*_a_a }. The instruction's extra "a_a"
    // literal (len 3) is BELOW the floor 4 and is correctly ABSENT (H1).
    let set = seg_intersect_rendered("a_a_*", "*_a_a");

    // Verify exactness against direct enumeration over a name universe.
    let ga = SegGlob::parse("a_a_*").unwrap();
    let gb = SegGlob::parse("*_a_a").unwrap();
    let globs = intersect_seg(&ga, &gb);
    for w in all_segments_len_upto(9) {
        let truth = ga.matches(&w) && gb.matches(&w);
        let got = globs.iter().any(|g| g.matches(&w));
        assert_eq!(
            truth,
            got,
            "a_a_* ∩ *_a_a not exact at {:?}",
            String::from_utf8_lossy(&w)
        );
    }

    // Pin the LITERAL corrected H2 vector (drift guard — not a recompute).
    assert_eq!(set, rendered(&["a_a_a", "a_a_a_a", "a_a_*_a_a"]));
    // And it must NOT contain the too-short "a_a" (H1 length floor).
    assert!(!set.contains("a_a"), "H2 set must exclude the too-short literal a_a");
}

fn all_segments_len_upto(max: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for len in 0..=max {
        gen_strings(len, &mut Vec::new(), &mut out);
    }
    out
}

#[test]
fn pinned_a_star_a_excludes_a() {
    // [H1] a*a ∩seg a*a  →  { a*a }, and must NOT contain "a".
    let set = seg_intersect_rendered("a*a", "a*a");
    assert_eq!(set, rendered(&["a*a"]));
    let g = SegGlob::parse("a*a").unwrap();
    let result = intersect_seg(&g, &g);
    assert!(
        !result.iter().any(|x| x.matches(b"a")),
        "a*a ∩ a*a must NOT admit 'a'"
    );
    // And idempotence: g ∩ g denotes exactly Objects(g).
    for w in all_segments_len_upto(5) {
        assert_eq!(
            g.matches(&w),
            result.iter().any(|x| x.matches(&w)),
            "a*a ∩ a*a ≠ Objects(a*a) at {:?}",
            String::from_utf8_lossy(&w)
        );
    }
}

#[test]
fn pinned_cross_arity_app_events() {
    // app_* ∩ *.events  →  app_*.events  (cross-arity: app_* → app_*.*)
    let a = Pattern::parse("app_*").unwrap();
    let b = Pattern::parse("*.events").unwrap();
    let univ = universe_for_names(&[
        ("app_1", Some("events")),
        ("app_1", Some("other")),
        ("x", Some("events")),
    ]);
    let sa = Scope::include(vec![a]).unwrap();
    let sb = Scope::include(vec![b]).unwrap();
    let meet = sa.meet(&sb);
    let oa = objects(&sa, &univ);
    let ob = objects(&sb, &univ);
    let om: BTreeSet<usize> = oa.intersection(&ob).copied().collect();
    assert_eq!(objects(&meet, &univ), om);
    // Concretely: only app_1.events survives.
    assert!(meet.objects_membership(&ObjectName::table(b"app_1".to_vec(), b"events".to_vec())));
    assert!(!meet.objects_membership(&ObjectName::table(b"app_1".to_vec(), b"other".to_vec())));
    assert!(!meet.objects_membership(&ObjectName::table(b"x".to_vec(), b"events".to_vec())));
}

fn universe_for_names(specs: &[(&str, Option<&str>)]) -> Vec<ObjectName> {
    specs
        .iter()
        .map(|(s, t)| match t {
            Some(tt) => ObjectName::table(s.as_bytes().to_vec(), tt.as_bytes().to_vec()),
            None => ObjectName::schema(s.as_bytes().to_vec()),
        })
        .collect()
}

#[test]
fn pinned_disjoint_meet_is_nothing() {
    // staging ⊓ analytics == Nothing (disjoint proper scopes meet to ⊥, never 𝒰).
    let staging = Scope::include(vec![Pattern::parse("staging").unwrap()]).unwrap();
    let analytics = Scope::include(vec![Pattern::parse("analytics").unwrap()]).unwrap();
    assert_eq!(staging.meet(&analytics), Scope::Nothing);
}

#[test]
fn pinned_all_is_meet_identity_and_join_absorbing() {
    let s = Scope::include(vec![Pattern::parse("app_*").unwrap()]).unwrap();
    // All ⊓ S = S ; S ⊓ All = S
    assert_eq!(Scope::All.meet(&s), s);
    assert_eq!(s.meet(&Scope::All), s);
    // All ⊔ S = All ; S ⊔ All = All
    assert_eq!(Scope::All.join(&s), Scope::All);
    assert_eq!(s.join(&Scope::All), Scope::All);
}

#[test]
fn pinned_default_meet_disjoint_rule_is_nothing() {
    // default_scope ⊓ disjoint-rule == Nothing (the default-scope narrowing case).
    let default_scope = Scope::include(vec![Pattern::parse("app_*").unwrap()]).unwrap();
    let disjoint_rule = Scope::include(vec![Pattern::parse("staging").unwrap()]).unwrap();
    assert_eq!(default_scope.meet(&disjoint_rule), Scope::Nothing);
}

#[test]
fn pinned_difference_exclude_counterexample_stays_reject() {
    // The settled counterexample: A = {app_*}, B = {app_*, exclude app_tmp_*}.
    // A ∖ B must be NON-EMPTY (it contains app_tmp_* objects), so the strict
    // uncovered-region check REJECTS the escalation. Under-approx (empty) would
    // wrongly accept.
    let a = Scope::include(vec![Pattern::parse("app_*").unwrap()]).unwrap();
    let b = Scope::of(
        vec![Pattern::parse("app_*").unwrap()],
        vec![Pattern::parse("app_tmp_*").unwrap()],
    )
    .unwrap();

    // Also the subset direction: A ⋢ B (A would grant app_tmp_* that B forbids).
    assert!(!a.subset(&b), "A ⊑ B must be FALSE (escalation) — B excludes app_tmp_*");

    let diff = a.difference(&b);
    match diff {
        Difference::NotRepresentable => { /* fail-closed reject — acceptable */ }
        Difference::Scope(d) => {
            // Must be non-empty and retain app_tmp_* objects (the region B carved
            // out that A\B keeps). An empty result would wrongly accept the
            // escalation.
            assert_ne!(d, Scope::Nothing, "A ∖ B must be non-empty, else escalation accepted");
            assert!(
                d.objects_membership(&ObjectName::table(b"app_tmp_x".to_vec(), b"t".to_vec())),
                "A ∖ B must retain app_tmp_* — got {d:?}"
            );
        }
    }
}

#[test]
fn pinned_empty_include_yields_nothing() {
    // Any Of{include:[]} path yields Nothing / errors — never a live Of{[]}.
    assert_eq!(Scope::include(vec![]), Err(super::ScopeError::EmptyInclude));
    // An exclude covering the include normalizes to Nothing.
    let s = Scope::of(
        vec![Pattern::parse("a").unwrap()],
        vec![Pattern::parse("a").unwrap()],
    )
    .unwrap();
    assert_eq!(s, Scope::Nothing);
}
