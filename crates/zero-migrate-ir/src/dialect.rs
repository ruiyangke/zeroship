//! Dialect IDENTITY: the closed `SqlDialect` enum and the open [`DialectId`].
//!
//! `SqlDialect` names which SQL engine a migration is being rendered/applied
//! for. It is a wire-level target descriptor (three closed variants, no
//! behaviour of its own), so it lives in the leaf `zero-migrate-ir` contract
//! rather than the engine: both the engine's schema-render layer
//! (`zero_migrate::schema::query`, which re-exports it) and the SQL security
//! layer (`zero-migrate-guard`, which is below the engine) name it, and neither
//! may depend on the other.
//!
//! The *dialect-specific spelling* (the `SchemaRenderer` trait, the DDL builders,
//! canonical type maps) lives engine-side in `zero_migrate::schema::query`; this
//! enum carries only the identity of the target.
//!
//! [`DialectId`] is the OPEN identity that replaces the closed enum's role as a
//! set/map key. A backend crate declares its own — `DialectId::new("duckdb")` —
//! without editing anything here, which is the whole point: a closed enum cannot
//! grow a variant from a crate that does not own it.

use core::fmt;
use std::borrow::Cow;

/// The SQL dialect a migration renders/applies against.
///
/// A closed enum: adding a fourth dialect breaks the exhaustive dispatch matches
/// (e.g. `zero_migrate::schema::query::renderer`) at compile time, forcing every
/// dialect-specific spelling to be wired before the crate can build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlDialect {
    /// Postgres dialect: binary DML values wrap their text placeholder with
    /// `decode($N, 'base64')` so a `BYTEA` column receives the original bytes.
    Postgres,
    /// `SQLite` dialect: binary DML values use a numbered `?N` placeholder and
    /// are bound directly as a `BLOB` by the in-process SQLite actor.
    Sqlite,
    /// `MySQL` dialect: binary DML values wrap their text placeholder with
    /// `FROM_BASE64(?)` so a binary column receives the original bytes.
    Mysql,
}

/// The canonical id of the PostgreSQL backend.
pub const POSTGRES: DialectId = DialectId::new("postgres");
/// The canonical id of the `SQLite` backend.
pub const SQLITE: DialectId = DialectId::new("sqlite");
/// The canonical id of the `MySQL` backend.
pub const MYSQL: DialectId = DialectId::new("mysql");

impl SqlDialect {
    /// The open [`DialectId`] this closed variant denotes.
    ///
    /// The bridge between the enum the engine still matches on and the id every
    /// set/map/descriptor is keyed by. It is deliberately one-way: an id does
    /// NOT convert back to a variant, because that direction is exactly what a
    /// fourth backend cannot satisfy.
    #[must_use]
    pub const fn id(self) -> DialectId {
        match self {
            Self::Postgres => POSTGRES,
            Self::Sqlite => SQLITE,
            Self::Mysql => MYSQL,
        }
    }
}

/// An opaque, cheaply copyable dialect identity with a stable string name.
///
/// NOT an enum: core code cannot exhaustively match it, which is the property
/// that lets a backend live in a crate the core does not own.
///
/// # Equality is by CONTENT
///
/// `PartialEq`/`Ord`/`Hash` are derived over `&'static str`, so they compare the
/// STRING, not the pointer. Two crates that both spell `"postgres"` are the same
/// dialect. That is the desired behaviour and it is also the hazard: nothing
/// structurally prevents two backends from claiming the same name, the way a
/// closed enum did by construction. The rule that covers it is
/// [`crate::backend::BackendRegistry`], which refuses to build over a duplicate
/// id and names both registrants — it is never last-one-wins.
///
/// # Validity
///
/// A well-formed id is lowercase ASCII matching `[a-z][a-z0-9_]*`. There are no
/// aliases and no display names in the id; a human-facing name is a separate
/// field on [`crate::backend::BackendDescriptor`]. [`DialectId::new`] is `const`
/// and does NOT check — a `const` constructor cannot return a `Result` usefully
/// — so the check is enforced at REGISTRATION rather than trusted. See
/// [`DialectId::is_well_formed`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DialectId(&'static str);

impl DialectId {
    /// Declare an id. `const`, so a backend crate can write
    /// `pub const DUCKDB: DialectId = DialectId::new("duckdb");` at item scope.
    ///
    /// This does NOT validate. Validity is enforced where it can produce a
    /// diagnostic naming the offender: [`crate::backend::BackendRegistry::build`].
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    /// The id's stable string name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }

    /// Whether this id satisfies the id rule: lowercase ASCII `[a-z][a-z0-9_]*`.
    ///
    /// Rejects the empty string, a leading digit or underscore, uppercase, dots,
    /// dashes, and any non-ASCII byte. `const` so a backend can assert its own id
    /// at compile time; the registry asserts it again at build time because a
    /// backend is not trusted about its own declaration.
    #[must_use]
    pub const fn is_well_formed(self) -> bool {
        let bytes = self.0.as_bytes();
        if bytes.is_empty() {
            return false;
        }
        if !bytes[0].is_ascii_lowercase() {
            return false;
        }
        let mut i = 1;
        while i < bytes.len() {
            let b = bytes[i];
            if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
                return false;
            }
            i += 1;
        }
        true
    }
}

impl fmt::Debug for DialectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DialectId({:?})", self.0)
    }
}

impl fmt::Display for DialectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// A set of dialect identities, with NO cap on how many it can hold.
///
/// This replaces `DialectSet(u8)`, whose three used bits and five spare ones put
/// a hard ceiling of EIGHT backends on the engine. The ceiling was never the
/// bits alone: the set was keyed by a closed three-variant enum, so an id with
/// no variant had no bit to occupy and vanished on insertion. Both are gone —
/// membership is now the id itself.
///
/// Members are kept sorted and deduplicated, so `PartialEq` is SET equality
/// (insertion order does not matter) and lookup is a binary search.
///
/// Lifted here from `zero_migrate::model::support` (which re-exports it
/// unchanged) so the [`crate::backend::BackendRegistry`] can key on the same set
/// type the support matrix uses, rather than growing a second one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialectSet(Cow<'static, [DialectId]>);

impl DialectSet {
    /// The empty set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(Cow::Borrowed(&[]))
    }

    /// Every dialect the engine currently ships.
    #[must_use]
    pub fn all() -> Self {
        Self::from_ids([POSTGRES, SQLITE, MYSQL])
    }

    /// Build from the three per-dialect support booleans of the closed-enum era.
    #[must_use]
    pub fn from_bools(postgres: bool, sqlite: bool, mysql: bool) -> Self {
        let mut ids = Vec::with_capacity(3);
        if postgres {
            ids.push(POSTGRES);
        }
        if sqlite {
            ids.push(SQLITE);
        }
        if mysql {
            ids.push(MYSQL);
        }
        Self::from_ids(ids)
    }

    /// Build from an arbitrary run of dialect identities. Duplicates collapse.
    #[must_use]
    pub fn from_ids(ids: impl IntoIterator<Item = DialectId>) -> Self {
        let mut members: Vec<DialectId> = ids.into_iter().collect();
        members.sort_unstable();
        members.dedup();
        Self(Cow::Owned(members))
    }

    /// Whether the closed [`crate::validate::Dialect`] variant is a member.
    #[must_use]
    pub fn contains(&self, dialect: crate::validate::Dialect) -> bool {
        self.contains_id(dialect.id())
    }

    /// Whether an id is a member.
    #[must_use]
    pub fn contains_id(&self, id: DialectId) -> bool {
        self.0.binary_search(&id).is_ok()
    }

    /// How many dialects the set holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The members, in ascending id order.
    pub fn iter(&self) -> impl Iterator<Item = DialectId> + '_ {
        self.0.iter().copied()
    }
}

impl FromIterator<DialectId> for DialectSet {
    fn from_iter<T: IntoIterator<Item = DialectId>>(iter: T) -> Self {
        Self::from_ids(iter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipping_ids_are_well_formed() {
        for id in [POSTGRES, SQLITE, MYSQL] {
            assert!(id.is_well_formed(), "{id} must satisfy the id rule");
        }
    }

    #[test]
    fn equality_is_by_content_not_pointer() {
        // Two separately-allocated `&'static str` with the same bytes. Rust may
        // or may not intern these; content equality must hold either way.
        let a = DialectId::new("postgres");
        let b = DialectId::new(concat!("postg", "res"));
        assert_eq!(a, b);
        assert_eq!(a.cmp(&b), core::cmp::Ordering::Equal);
    }

    #[test]
    fn the_id_rule_refuses_the_shapes_it_names() {
        for bad in [
            "",           // empty
            "Postgres",   // uppercase
            "1postgres",  // leading digit
            "_postgres",  // leading underscore
            "post-gres",  // dash
            "post.gres",  // dot
            "post gres",  // space
            "postgresql\u{00e9}", // non-ASCII
        ] {
            assert!(
                !DialectId::new(bad).is_well_formed(),
                "{bad:?} must be refused by the id rule"
            );
        }
        for good in ["pg", "postgres", "mysql8", "cockroach_db", "a"] {
            assert!(
                DialectId::new(good).is_well_formed(),
                "{good:?} must satisfy the id rule"
            );
        }
    }

    #[test]
    fn dialect_wire_spelling_is_the_id() {
        // `Dialect::as_str` is the `dialect` field of the structured authoring
        // rejection — a WIRE spelling. It is derived from the id, so this pins
        // that the two can never drift into two names for one dialect.
        for dialect in [
            crate::validate::Dialect::Postgres,
            crate::validate::Dialect::Sqlite,
            crate::validate::Dialect::Mysql,
        ] {
            assert_eq!(dialect.as_str(), dialect.id().as_str());
        }
        assert_eq!(crate::validate::Dialect::Postgres.as_str(), "postgres");
        assert_eq!(crate::validate::Dialect::Sqlite.as_str(), "sqlite");
        assert_eq!(crate::validate::Dialect::Mysql.as_str(), "mysql");
    }

    #[test]
    fn a_dialect_set_is_unbounded_and_order_free() {
        let many: Vec<DialectId> = (0..64)
            .map(|i| {
                let name: &'static str = Box::leak(format!("backend_{i}").into_boxed_str());
                DialectId::new(name)
            })
            .collect();
        let set = DialectSet::from_ids(many.iter().copied());
        assert_eq!(set.len(), 64);
        for id in &many {
            assert!(set.contains_id(*id), "{id} must be a member");
        }
        assert!(!set.contains_id(DialectId::new("absent")));

        // Set equality, not sequence equality.
        let mut reversed = many.clone();
        reversed.reverse();
        assert_eq!(DialectSet::from_ids(reversed), set);
        // Duplicates collapse.
        let doubled = many.iter().copied().chain(many.iter().copied());
        assert_eq!(DialectSet::from_ids(doubled).len(), 64);
    }

    #[test]
    fn from_bools_agrees_with_the_ids_it_names() {
        assert_eq!(DialectSet::from_bools(false, false, false), DialectSet::empty());
        assert_eq!(DialectSet::from_bools(true, true, true), DialectSet::all());
        let pg_mysql = DialectSet::from_bools(true, false, true);
        assert!(pg_mysql.contains_id(POSTGRES));
        assert!(pg_mysql.contains_id(MYSQL));
        assert!(!pg_mysql.contains_id(SQLITE));
        assert_eq!(pg_mysql.len(), 2);
    }

    #[test]
    fn sql_dialect_maps_onto_its_id() {
        assert_eq!(SqlDialect::Postgres.id().as_str(), "postgres");
        assert_eq!(SqlDialect::Sqlite.id().as_str(), "sqlite");
        assert_eq!(SqlDialect::Mysql.id().as_str(), "mysql");
    }
}
