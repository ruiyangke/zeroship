//! Two-segment scope patterns and the concrete object names they match.
//!
//! A [`Pattern`] addresses either a **schema** (one segment: `staging`, `app_*`)
//! or a **schema-qualified table** (two segments: `app_main.events`,
//! `tenant_*.audit`). Before any lattice operation a bare schema pattern `P` is
//! normalized to `P.*` (the schema plus every table in it) so every pattern is a
//! `⟨schemaGlob⟩.⟨tableGlob⟩` pair — the cross-arity rule (II.3.1).
//!
//! An [`ObjectName`] is a concrete normalized name (schema-only or
//! schema.table) — the ground-truth universe element the oracle enumerates and
//! the direct matcher tests membership against.

use std::collections::BTreeSet;

use super::glob::{intersect_seg, SegGlob};

/// A concrete, already-normalized object name: a schema, or a schema-qualified
/// table. This is a UNIVERSE element for the oracle — never a pattern.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ObjectName {
    pub schema: Vec<u8>,
    /// `None` = a schema object; `Some(t)` = the table `schema.t`.
    pub table: Option<Vec<u8>>,
}

impl ObjectName {
    #[must_use]
    pub fn schema(s: impl Into<Vec<u8>>) -> Self {
        Self {
            schema: s.into(),
            table: None,
        }
    }

    #[must_use]
    pub fn table(s: impl Into<Vec<u8>>, t: impl Into<Vec<u8>>) -> Self {
        Self {
            schema: s.into(),
            table: Some(t.into()),
        }
    }
}

/// A two-segment scope pattern in NORMALIZED (always-two-segment) form: a schema
/// glob and a table glob. A schema-only source pattern `P` is stored as `P.*`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Pattern {
    pub schema: SegGlob,
    pub table: SegGlob,
}

impl Pattern {
    /// A schema-only pattern `P`, normalized to `P.*` (cross-arity, II.3.1).
    #[must_use]
    pub fn schema(glob: SegGlob) -> Self {
        Self {
            schema: glob,
            table: SegGlob::star(),
        }
    }

    /// A two-segment `schema.table` pattern.
    #[must_use]
    pub fn table(schema: SegGlob, table: SegGlob) -> Self {
        Self { schema, table }
    }

    /// Parse `"app_*"` (schema→`app_*.*`) or `"app_*.events"` (two segments).
    /// Returns `None` if the text has more than two dot-segments or a bad glob.
    ///
    /// NOTE: this is the blunt parser over the alphabet the scope oracle uses;
    /// it splits on EVERY `.` and applies no identifier folding. The oracle proves
    /// the lattice over globs built by this parser, so it stays byte-exact — do not
    /// route it through [`normalize_pg_identifier`]. The document loader uses
    /// [`Pattern::parse_normalized`] for the II.2.7 quote/fold semantics.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split('.').collect();
        match parts.as_slice() {
            [schema] => Some(Self::schema(SegGlob::parse(schema)?)),
            [schema, table] => Some(Self::table(SegGlob::parse(schema)?, SegGlob::parse(table)?)),
            _ => None,
        }
    }

    /// Parse + NORMALIZE a scope pattern per PostgreSQL identifier semantics
    /// (II.2.7) — the constructor the document loader (the PDP) MUST use so that a
    /// pattern literal and a concrete op name fold to the same canonical bytes.
    ///
    /// Segmenting: the text is split into segments on **unquoted dots only**. A
    /// double-quoted run `"…"` is one *quoted* segment whose inner dots (and glob
    /// `*`) are literal — `"a.b"` is a single segment (a table literally named
    /// `a.b`), NOT `a`-schema/`b`-table. `""` inside a quoted run is an escaped
    /// quote (PG doubling). At most two segments (schema[.table]).
    ///
    /// Per-segment folding: an **unquoted** segment folds to ASCII-lowercase (PG
    /// downcases unquoted identifiers) and its single `*` is a glob metacharacter;
    /// a **quoted** segment is preserved byte-verbatim and is a pure literal (a `*`
    /// inside quotes is the literal byte `*`, never a glob).
    ///
    /// Returns `None` on: more than one `*` in an unquoted segment (illegal glob),
    /// more than two segments, an unterminated quote, or an empty segment.
    #[must_use]
    pub fn parse_normalized(s: &str) -> Option<Self> {
        let segs = split_normalize_segments(s)?;
        match segs.as_slice() {
            [schema] => Some(Self::schema(seg_from_normalized(schema)?)),
            [schema, table] => Some(Self::table(
                seg_from_normalized(schema)?,
                seg_from_normalized(table)?,
            )),
            _ => None,
        }
    }

    /// The `*.*` pattern — matches every object (schema and table alike). This is
    /// the pattern spelling of `Objects(All)`; used by `Scope::All` legality/
    /// membership reasoning and as the include of a universe `Of{["*"]}`.
    #[must_use]
    pub fn universe() -> Self {
        Self {
            schema: SegGlob::star(),
            table: SegGlob::star(),
        }
    }

    /// Ground-truth matcher: does this pattern match the concrete name `n`?
    ///
    /// A schema-only object `schema` matches a pattern iff the pattern's table
    /// glob accepts the empty table AND the schema glob matches — i.e. the
    /// pattern's table segment covers "no table". We model a schema object as the
    /// table segment being ABSENT; a pattern matches it iff `table == *` (the
    /// normalized `P.*` form) — `P.*` covers "the schema itself" as well as its
    /// tables. A pattern with a NON-`*` table glob matches only tables.
    #[must_use]
    pub fn matches(&self, n: &ObjectName) -> bool {
        if !self.schema.matches(&n.schema) {
            return false;
        }
        match &n.table {
            // A concrete table: the table glob must match its name.
            Some(t) => self.table.matches(t),
            // A schema object: matched only by the schema-normalized `P.*` form
            // (table glob is exactly `*`). A pattern like `s.events` addresses a
            // table, never the schema object itself.
            None => self.table.is_star(),
        }
    }
}

/// One segment of a normalized scope pattern: its post-fold bytes and whether it
/// was authored QUOTED (a pure literal — `*` is not a glob) or UNQUOTED (glob).
struct NormalizedSegment {
    bytes: Vec<u8>,
    quoted: bool,
}

/// Split `s` into 1–2 segments on **unquoted dots only** and fold each per II.2.7.
///
/// A double-quoted run `"…"` is a single quoted segment; its inner bytes are taken
/// verbatim with `""` collapsing to one `"` (PG quote-doubling). Dots and `*`
/// inside quotes are literal. Returns `None` on an unterminated quote, an empty
/// segment, quoted-and-unquoted mixing within a single segment, or more than two
/// segments.
fn split_normalize_segments(s: &str) -> Option<Vec<NormalizedSegment>> {
    let bytes = s.as_bytes();
    let mut out: Vec<NormalizedSegment> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    // Track whether the current segment has seen a quoted run and/or unquoted
    // bytes; a segment must be wholly one or the other (no `a"b"` frankenidents).
    let mut cur_quoted = false;
    let mut cur_unquoted = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                // Begin a quoted run; consume to the closing quote, honoring `""`.
                cur_quoted = true;
                i += 1;
                loop {
                    if i >= bytes.len() {
                        return None; // unterminated quote
                    }
                    if bytes[i] == b'"' {
                        if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                            cur.push(b'"'); // escaped quote
                            i += 2;
                            continue;
                        }
                        i += 1; // closing quote
                        break;
                    }
                    cur.push(bytes[i]);
                    i += 1;
                }
            }
            b'.' => {
                // Unquoted dot: segment boundary.
                out.push(finish_segment(&cur, cur_quoted, cur_unquoted)?);
                cur.clear();
                cur_quoted = false;
                cur_unquoted = false;
                i += 1;
            }
            c => {
                cur_unquoted = true;
                // Unquoted byte: ASCII-lowercase fold (PG downcases unquoted idents).
                cur.push(c.to_ascii_lowercase());
                i += 1;
            }
        }
        if out.len() > 2 {
            return None;
        }
    }
    out.push(finish_segment(&cur, cur_quoted, cur_unquoted)?);
    if out.is_empty() || out.len() > 2 {
        return None;
    }
    Some(out)
}

/// Finalize one accumulated segment: reject the empty segment and the mixed
/// quoted+unquoted case (`a"b"` — never a real identifier and never safe to fold).
fn finish_segment(bytes: &[u8], quoted: bool, unquoted: bool) -> Option<NormalizedSegment> {
    if bytes.is_empty() {
        return None; // empty segment (e.g. `a.`, `.b`, ``)
    }
    if quoted && unquoted {
        return None; // `a"b"` mixing — reject fail-closed
    }
    Some(NormalizedSegment {
        bytes: bytes.to_vec(),
        quoted,
    })
}

/// Build a [`SegGlob`] from a normalized segment: a quoted segment is a pure
/// literal (its `*` is a literal byte); an unquoted segment's single `*` is a glob.
fn seg_from_normalized(seg: &NormalizedSegment) -> Option<SegGlob> {
    if seg.quoted {
        // Verbatim literal — even a `*` is the byte `*`, never a glob.
        Some(SegGlob::literal(seg.bytes.clone()))
    } else {
        // Reuse the single-`*` glob parser over the already-folded bytes.
        let text = String::from_utf8_lossy(&seg.bytes);
        SegGlob::parse(&text)
    }
}

/// Normalize a CONCRETE object name (schema or `schema.table`) per II.2.7 for
/// enforcement-time / predicate matching — the same fold [`Pattern::parse_normalized`]
/// applies to pattern literals. A concrete name carries no globs, so an unquoted
/// `*` would be a nonsensical identifier byte; we fold it verbatim (lowercased)
/// rather than treating it as a glob (a name never globs). Returns `None` on the
/// same structural rejects as the pattern parser.
#[must_use]
pub fn normalize_pg_identifier(s: &str) -> Option<ObjectName> {
    let segs = split_normalize_segments(s)?;
    match segs.as_slice() {
        [schema] => Some(ObjectName::schema(schema.bytes.clone())),
        [schema, table] => Some(ObjectName::table(schema.bytes.clone(), table.bytes.clone())),
        _ => None,
    }
}

/// Exact two-segment pattern intersection: the Cartesian product of the
/// per-segment `∩seg` sets, flattened (II.3.1). `∅` if either segment set is `∅`.
#[must_use]
pub fn intersect_pattern(a: &Pattern, b: &Pattern) -> BTreeSet<Pattern> {
    let schemas = intersect_seg(&a.schema, &b.schema);
    if schemas.is_empty() {
        return BTreeSet::new();
    }
    let tables = intersect_seg(&a.table, &b.table);
    if tables.is_empty() {
        return BTreeSet::new();
    }
    let mut out = BTreeSet::new();
    for s in &schemas {
        for t in &tables {
            out.insert(Pattern {
                schema: s.clone(),
                table: t.clone(),
            });
        }
    }
    out
}

/// Does pattern `a` cover pattern `b`? (`Objects(b) ⊆ Objects(a)`) — per-segment
/// cover, sound and exact for single-`*` globs.
#[must_use]
pub fn pattern_covers(a: &Pattern, b: &Pattern) -> bool {
    a.schema.covers(&b.schema) && a.table.covers(&b.table)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_normalizes_to_star_table() {
        let p = Pattern::parse("app_x").unwrap();
        assert_eq!(p, Pattern::schema(SegGlob::parse("app_x").unwrap()));
        assert!(p.table.is_star());
    }

    #[test]
    fn schema_pattern_matches_schema_object_and_its_tables() {
        let p = Pattern::parse("app_x").unwrap();
        assert!(p.matches(&ObjectName::schema(b"app_x".to_vec())));
        assert!(p.matches(&ObjectName::table(b"app_x".to_vec(), b"events".to_vec())));
        assert!(!p.matches(&ObjectName::schema(b"other".to_vec())));
    }

    #[test]
    fn table_pattern_does_not_match_schema_object() {
        let p = Pattern::parse("app_x.events").unwrap();
        assert!(!p.matches(&ObjectName::schema(b"app_x".to_vec())));
        assert!(p.matches(&ObjectName::table(b"app_x".to_vec(), b"events".to_vec())));
        assert!(!p.matches(&ObjectName::table(b"app_x".to_vec(), b"other".to_vec())));
    }

    #[test]
    fn normalize_unquoted_folds_lowercase() {
        // `App_*` → glob `app_*` (schema), matches the folded catalog name `app_x`.
        let p = Pattern::parse_normalized("App_*").unwrap();
        assert_eq!(p, Pattern::parse("app_*").unwrap());
        assert!(p.matches(&ObjectName::schema(b"app_x".to_vec())));
    }

    #[test]
    fn normalize_quoted_verbatim_not_matched_by_unquoted_pattern() {
        // A quoted pattern `"App_x"` is a byte-exact literal, distinct from `app_x`.
        let quoted = Pattern::parse_normalized("\"App_x\"").unwrap();
        assert!(quoted.matches(&ObjectName::schema(b"App_x".to_vec())));
        assert!(!quoted.matches(&ObjectName::schema(b"app_x".to_vec())));
        // And an unquoted `app_*` pattern does not match the quoted concrete name.
        let unq = Pattern::parse_normalized("app_*").unwrap();
        let quoted_name = normalize_pg_identifier("\"App_x\"").unwrap();
        assert!(!unq.matches(&quoted_name));
        // But it does match the unquoted `App_x`, which folds to `app_x`.
        let folded_name = normalize_pg_identifier("App_x").unwrap();
        assert!(unq.matches(&folded_name));
    }

    #[test]
    fn normalize_quoted_dot_is_one_segment() {
        // `"a.b"` is ONE segment (a table `a.b` in the default schema), not a.b.
        let p = Pattern::parse_normalized("\"a.b\"").unwrap();
        // As a schema-only pattern its schema glob is the literal `a.b`.
        assert_eq!(p, Pattern::schema(SegGlob::literal(b"a.b".to_vec())));
        // The schema pattern `a` must NOT match the object named `a.b`.
        let schema_a = Pattern::parse_normalized("a").unwrap();
        let obj = normalize_pg_identifier("\"a.b\"").unwrap();
        assert!(!schema_a.matches(&obj));
    }

    #[test]
    fn normalize_quoted_star_is_literal_not_glob() {
        // A `*` inside quotes is the literal byte, not a wildcard.
        let p = Pattern::parse_normalized("\"a*b\"").unwrap();
        assert_eq!(p, Pattern::schema(SegGlob::literal(b"a*b".to_vec())));
        assert!(p.matches(&ObjectName::schema(b"a*b".to_vec())));
        assert!(!p.matches(&ObjectName::schema(b"axb".to_vec())));
    }

    #[test]
    fn normalize_rejects_bad_shapes() {
        assert!(Pattern::parse_normalized("a.b.c").is_none()); // 3 segments
        assert!(Pattern::parse_normalized("a.").is_none()); // empty trailing seg
        assert!(Pattern::parse_normalized(".b").is_none()); // empty leading seg
        assert!(Pattern::parse_normalized("\"unterminated").is_none());
        assert!(Pattern::parse_normalized("a*b*c").is_none()); // two stars unquoted
    }

    #[test]
    fn cross_arity_intersection() {
        // app_* (→ app_*.*) ∩ *.events  →  app_*.events
        let a = Pattern::parse("app_*").unwrap();
        let b = Pattern::parse("*.events").unwrap();
        let r = intersect_pattern(&a, &b);
        assert_eq!(r.len(), 1);
        let only = r.iter().next().unwrap();
        assert!(only.matches(&ObjectName::table(b"app_1".to_vec(), b"events".to_vec())));
        assert!(!only.matches(&ObjectName::table(b"app_1".to_vec(), b"other".to_vec())));
    }
}
