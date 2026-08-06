//! Single-segment identifier globs and their exact intersection algebra.
//!
//! A [`SegGlob`] is a pattern over the bytes of ONE identifier segment (a schema
//! name, or a table name). The model allows **at most one `*`** per segment; this
//! keeps intersection finite and closed. A segment glob is one of:
//!
//! - a **literal** `lit` (no `*`): matches exactly the byte string `lit`;
//! - a **star** `*` (empty prefix, empty suffix): matches every byte string;
//! - a **prefix** glob `p*`: matches `w` iff `w` starts with `p` and `|w| ≥ |p|`;
//! - a **suffix** glob `*s`: matches `w` iff `w` ends with `s` and `|w| ≥ |s|`;
//! - an **infix** glob `p*s`: matches `w` iff `w` starts with `p`, ends with `s`,
//!   and `|w| ≥ |p| + |s|` (the length floor is the H1 fix — see below).
//!
//! The exact per-segment intersection `∩seg : SegGlob × SegGlob → Set<SegGlob>`
//! (a *set*, because shared-`*` corner cases are not expressible as one glob) is
//! the load-bearing primitive of the whole scope lattice: the two-segment
//! `pattern ∩ pattern` is the Cartesian product across segments, and the scope
//! meet `⊓` unions those products. `∩seg` MUST be exact (`Objects(a ∩ b) =
//! Objects(a) ∩ Objects(b)`); the brute-force oracle in `crate::scope::oracle`
//! proves it over a bounded universe.

use std::collections::BTreeSet;

/// A single-segment identifier glob: a literal, or a literal `prefix`/`suffix`
/// pair straddling exactly one `*`.
///
/// Canonicalisation invariant (upheld by every constructor and combinator here):
/// the `(prefix, suffix, has_star)` triple is the unique representation of the
/// object set it denotes. `has_star == false` ⇒ `suffix` is empty and `prefix`
/// is the literal. `*` is `prefix == [] && suffix == [] && has_star`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SegGlob {
    prefix: Vec<u8>,
    suffix: Vec<u8>,
    has_star: bool,
}

impl std::fmt::Debug for SegGlob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SegGlob({})", self.render())
    }
}

impl SegGlob {
    /// An exact literal segment (no `*`).
    #[must_use]
    pub fn literal(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            prefix: bytes.into(),
            suffix: Vec::new(),
            has_star: false,
        }
    }

    /// The `*` glob — matches every byte string in this segment.
    #[must_use]
    pub fn star() -> Self {
        Self {
            prefix: Vec::new(),
            suffix: Vec::new(),
            has_star: true,
        }
    }

    /// A prefix/star/suffix/infix glob from its `p` and `s` pieces (one `*`
    /// between them). Both pieces may be empty (yielding `*`).
    #[must_use]
    pub fn infix(prefix: impl Into<Vec<u8>>, suffix: impl Into<Vec<u8>>) -> Self {
        Self {
            prefix: prefix.into(),
            suffix: suffix.into(),
            has_star: true,
        }
    }

    /// Parse a single-segment glob from its textual form. Exactly zero or one
    /// `*` is permitted (the caller guarantees this at document load).
    ///
    /// Returns `None` if more than one `*` is present.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let bytes = s.as_bytes();
        let mut stars = bytes.iter().enumerate().filter(|(_, &b)| b == b'*');
        match (stars.next(), stars.next()) {
            (None, _) => Some(Self::literal(bytes.to_vec())),
            (Some((star, _)), None) => Some(Self::infix(
                bytes[..star].to_vec(),
                bytes[star + 1..].to_vec(),
            )),
            (Some(_), Some(_)) => None, // more than one `*` is illegal
        }
    }

    #[must_use]
    pub fn is_star(&self) -> bool {
        self.has_star && self.prefix.is_empty() && self.suffix.is_empty()
    }

    /// The literal bytes before the `*` (the whole value when there is no star).
    ///
    /// For the seal's canonical encoding, which must distinguish globs that
    /// [`Self::render`] spells identically.
    #[must_use]
    pub(crate) fn prefix_bytes(&self) -> &[u8] {
        &self.prefix
    }

    /// The literal bytes after the `*` (empty when there is no star).
    #[must_use]
    pub(crate) fn suffix_bytes(&self) -> &[u8] {
        &self.suffix
    }

    #[must_use]
    pub fn is_literal(&self) -> bool {
        !self.has_star
    }

    /// Render back to textual form (for diagnostics / doc examples).
    #[must_use]
    pub fn render(&self) -> String {
        let p = String::from_utf8_lossy(&self.prefix);
        if self.has_star {
            let s = String::from_utf8_lossy(&self.suffix);
            format!("{p}*{s}")
        } else {
            p.into_owned()
        }
    }

    /// Ground-truth matcher: does this glob match the concrete segment `w`?
    ///
    /// This is the *direct* matcher the oracle uses as ground truth — it is NOT
    /// derived from the lattice ops. Every algebraic operation is verified against
    /// this.
    #[must_use]
    pub fn matches(&self, w: &[u8]) -> bool {
        if self.has_star {
            w.len() >= self.prefix.len() + self.suffix.len()
                && w.starts_with(&self.prefix)
                && w.ends_with(&self.suffix)
        } else {
            w == self.prefix.as_slice()
        }
    }

    /// Does `self`'s object set cover `other`'s? (`Objects(other) ⊆ Objects(self)`)
    ///
    /// Sound and EXACT for single-`*` segment globs. Used by the scope-`⊑` cover
    /// relation.
    #[must_use]
    pub fn covers(&self, other: &SegGlob) -> bool {
        match (self.has_star, other.has_star) {
            // literal covers literal: only if identical.
            (false, false) => self.prefix == other.prefix,
            // literal never covers an infinite star-glob.
            (false, true) => false,
            // star-glob covers a literal iff the literal matches it.
            (true, false) => self.matches(&other.prefix),
            // star-glob covers star-glob: `self = ps*ss` covers `other = po*so`
            // iff every string matching `other` also matches `self`. Since
            // `other` can be arbitrarily long in the middle, `self`'s prefix must
            // be a prefix of `other`'s prefix AND `self`'s suffix a suffix of
            // `other`'s suffix — but ONLY when those pieces don't force an overlap
            // in short witnesses of `other`. Because both are single-`*` globs and
            // `other` admits arbitrarily long middles, the shortest such witnesses
            // still start with `other.prefix` and end with `other.suffix`; the
            // cover holds iff `self.prefix ⊑ other.prefix` and `self.suffix` is a
            // suffix of `other.suffix`. (The length floor of `self` is then
            // automatically met by any witness long enough to satisfy `other`,
            // because a witness of `other` can be extended arbitrarily.)
            (true, true) => {
                other.prefix.starts_with(&self.prefix) && other.suffix.ends_with(&self.suffix)
            }
        }
    }
}

/// Exact per-segment intersection `∩seg`.
///
/// Returns the *set* of globs whose union of object sets equals
/// `Objects(a) ∩ Objects(b)`. EXACT: no over- or under-approximation.
///
/// The corner cases (prefix×suffix, infix×infix) enumerate every consistent
/// overlap length. **[H1 FIX]** every emitted infix/literal witness is filtered by
/// the joint length floor `|w| ≥ max(|p1|+|s1|, |p2|+|s2|)`, so e.g.
/// `a*a ∩seg a*a` does NOT contain `"a"` — the doc's reduction dropped the
/// per-input floors and over-approximated.
#[must_use]
pub fn intersect_seg(a: &SegGlob, b: &SegGlob) -> BTreeSet<SegGlob> {
    let mut out = BTreeSet::new();

    match (a.has_star, b.has_star) {
        // ── literal × literal ──────────────────────────────────────────────
        (false, false) => {
            if a.prefix == b.prefix {
                out.insert(a.clone());
            }
        }
        // ── literal × glob (and symmetric) ─────────────────────────────────
        (false, true) => {
            if b.matches(&a.prefix) {
                out.insert(a.clone());
            }
        }
        (true, false) => {
            if a.matches(&b.prefix) {
                out.insert(b.clone());
            }
        }
        // ── glob × glob ────────────────────────────────────────────────────
        // Both `a = pa*sa`, `b = pb*sb`. A string matches both iff it starts
        // with BOTH pa and pb (⇒ one prefix a prefix of the other; take the
        // longer as p, else ∅), ends with BOTH sa and sb (⇒ one suffix a suffix
        // of the other; take the longer as s, else ∅), AND is long enough to
        // satisfy both length floors: |w| ≥ max(|pa|+|sa|, |pb|+|sb|).
        (true, true) => {
            let (Some(p), Some(s)) = (
                longer_prefix(&a.prefix, &b.prefix),
                longer_suffix(&a.suffix, &b.suffix),
            ) else {
                return out; // ∅: prefixes or suffixes contradict
            };
            let floor = (a.prefix.len() + a.suffix.len()).max(b.prefix.len() + b.suffix.len());
            enumerate_prefix_suffix(&p, &s, floor, &mut out);
        }
    }

    subsume(out)
}

/// The longer of two byte strings if one is a prefix of the other, else `None`.
fn longer_prefix(x: &[u8], y: &[u8]) -> Option<Vec<u8>> {
    if x.starts_with(y) {
        Some(x.to_vec())
    } else if y.starts_with(x) {
        Some(y.to_vec())
    } else {
        None
    }
}

/// The longer of two byte strings if one is a suffix of the other, else `None`.
fn longer_suffix(x: &[u8], y: &[u8]) -> Option<Vec<u8>> {
    if x.ends_with(y) {
        Some(x.to_vec())
    } else if y.ends_with(x) {
        Some(y.to_vec())
    } else {
        None
    }
}

/// Enumerate every glob in `Objects(p*) ∩ Objects(*s)` that also satisfies the
/// joint length `floor` — i.e. the set whose union is exactly
/// `{ w : w starts with p, w ends with s, |w| ≥ floor }`.
///
/// A witness `w` starts with `p` and ends with `s`. Two sub-populations:
///
/// - **`|w| ≥ |p|+|s|`** (prefix and suffix do not overlap): these are exactly
///   `Objects(p*s)`, the infix glob. Because `enumerate_prefix_suffix` is only
///   ever called from the glob×glob arm with `p = longer(pa,pb)` and
///   `s = longer(sa,sb)`, we always have `base = |p|+|s| ≥ floor` (each input's
///   `|pi|+|si| ≤ base`), so the whole infix clears the floor and is emitted
///   verbatim. The `floor ≤ base` guard makes that reliance explicit and total.
/// - **`|w| < |p|+|s|`** (prefix and suffix must overlap by `o = |p|+|s|−|w|` ≥ 1
///   characters): finite, and each is a LITERAL of length `base − o`. Such a
///   literal exists iff `p`'s last `o` bytes equal `s`'s first `o` bytes. This is
///   where the [H1 FIX] bites: an overlap literal shorter than `floor` denotes a
///   string too short for one of the two INPUT globs (e.g. `"a"` for
///   `a*a ∩seg a*a`, whose inputs each require `|w| ≥ 2`) and MUST be dropped.
///
/// The star-carrying member is the single infix `p*s`; every overlap witness is a
/// literal. `subsume` (called by `intersect_seg`) then drops any literal the infix
/// already covers — but a floor-clipped literal never reappears, because the infix
/// only covers witnesses of length ≥ base > the clipped literal's length.
fn enumerate_prefix_suffix(p: &[u8], s: &[u8], floor: usize, out: &mut BTreeSet<SegGlob>) {
    let base = p.len() + s.len();

    // Non-overlapping witnesses (|w| ≥ base): exactly Objects(p*s). Always clears
    // the floor for the pairs this function receives (base ≥ floor); the guard
    // documents and enforces that invariant rather than silently over-emitting.
    debug_assert!(
        floor <= base,
        "enumerate_prefix_suffix invariant: base ({base}) ≥ floor ({floor})"
    );
    if floor <= base {
        out.insert(SegGlob::infix(p.to_vec(), s.to_vec()));
    }

    // Overlapping witnesses (|w| = base − o, o ≥ 1): finite literals, each valid
    // only when p's last o bytes equal s's first o bytes, and each subject to the
    // joint length floor (the H1 fix — a too-short literal is dropped).
    let min = p.len().min(s.len());
    for o in 1..=min {
        if p[p.len() - o..] == s[..o] {
            let mut lit = p.to_vec();
            lit.extend_from_slice(&s[o..]);
            debug_assert_eq!(lit.len(), base - o);
            if lit.len() >= floor {
                out.insert(SegGlob::literal(lit));
            }
        }
    }
}

/// Drop any glob strictly covered by a *different* glob in the set. A `BTreeSet`
/// already dedups equal globs, so `h != g` makes this strict subsumption (never
/// drops a glob because it covers itself).
fn subsume(globs: BTreeSet<SegGlob>) -> BTreeSet<SegGlob> {
    let v: Vec<SegGlob> = globs.iter().cloned().collect();
    v.iter()
        .filter(|g| !v.iter().any(|h| h != *g && h.covers(g)))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<SegGlob> {
        items.iter().map(|s| SegGlob::parse(s).unwrap()).collect()
    }

    #[test]
    fn star_matches_everything() {
        let s = SegGlob::star();
        assert!(s.matches(b""));
        assert!(s.matches(b"abc"));
    }

    #[test]
    fn literal_matches_only_itself() {
        let l = SegGlob::literal(b"ab".to_vec());
        assert!(l.matches(b"ab"));
        assert!(!l.matches(b"abc"));
        assert!(!l.matches(b"a"));
    }

    #[test]
    fn infix_length_floor() {
        // a*a matches "aa", "aba", but NOT "a" (needs len >= 2).
        let g = SegGlob::infix(b"a".to_vec(), b"a".to_vec());
        assert!(!g.matches(b"a"));
        assert!(g.matches(b"aa"));
        assert!(g.matches(b"aba"));
    }

    #[test]
    fn h1_a_star_a_idempotent_excludes_a() {
        // a*a ∩seg a*a must be {a*a}, and must NOT admit "a".
        let g = SegGlob::infix(b"a".to_vec(), b"a".to_vec());
        let r = intersect_seg(&g, &g);
        assert_eq!(r, set(&["a*a"]));
        for glob in &r {
            assert!(!glob.matches(b"a"), "a*a ∩ a*a must not match 'a'");
        }
    }

    #[test]
    fn prefix_suffix_disjoint_chars() {
        // a_* ∩seg *_x  →  { a_x, a_*_x }  (prefix a_, suffix _x, no shared char)
        let a = SegGlob::infix(b"a_".to_vec(), Vec::new());
        let b = SegGlob::infix(Vec::new(), b"_x".to_vec());
        let r = intersect_seg(&a, &b);
        assert_eq!(r, set(&["a_x", "a_*_x"]));
    }

    #[test]
    fn prefix_suffix_shared_char_enumerates_all_overlaps() {
        // a_a_* ∩seg *_a_a : the corrected H2 set (see the oracle for the full
        // derivation + exactness proof). o=3 gives a_a_a, o=1 gives a_a_a_a, plus
        // the infix a_a_*_a_a. The too-short "a_a" (< floor 4) is absent.
        let a = SegGlob::infix(b"a_a_".to_vec(), Vec::new());
        let b = SegGlob::infix(Vec::new(), b"_a_a".to_vec());
        let r = intersect_seg(&a, &b);
        assert_eq!(r, set(&["a_a_a", "a_a_a_a", "a_a_*_a_a"]));
    }

    #[test]
    fn covers_star_over_literal() {
        assert!(SegGlob::star().covers(&SegGlob::literal(b"abc".to_vec())));
        assert!(
            SegGlob::infix(b"a".to_vec(), Vec::new()).covers(&SegGlob::literal(b"abc".to_vec()))
        );
        assert!(
            !SegGlob::infix(b"b".to_vec(), Vec::new()).covers(&SegGlob::literal(b"abc".to_vec()))
        );
    }
}
