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
        Self { schema: s.into(), table: None }
    }

    #[must_use]
    pub fn table(s: impl Into<Vec<u8>>, t: impl Into<Vec<u8>>) -> Self {
        Self { schema: s.into(), table: Some(t.into()) }
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
        Self { schema: glob, table: SegGlob::star() }
    }

    /// A two-segment `schema.table` pattern.
    #[must_use]
    pub fn table(schema: SegGlob, table: SegGlob) -> Self {
        Self { schema, table }
    }

    /// Parse `"app_*"` (schema→`app_*.*`) or `"app_*.events"` (two segments).
    /// Returns `None` if the text has more than two dot-segments or a bad glob.
    ///
    /// NOTE: this is the blunt phase-1a parser over the alphabet the oracle uses;
    /// full PG identifier normalization/quoting (II.2.7) is a later phase.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split('.').collect();
        match parts.as_slice() {
            [schema] => Some(Self::schema(SegGlob::parse(schema)?)),
            [schema, table] => Some(Self::table(SegGlob::parse(schema)?, SegGlob::parse(table)?)),
            _ => None,
        }
    }

    /// The `*.*` pattern — matches every object (schema and table alike). This is
    /// the pattern spelling of `Objects(All)`; used by `Scope::All` legality/
    /// membership reasoning and as the include of a universe `Of{["*"]}`.
    #[must_use]
    pub fn universe() -> Self {
        Self { schema: SegGlob::star(), table: SegGlob::star() }
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
            out.insert(Pattern { schema: s.clone(), table: t.clone() });
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
