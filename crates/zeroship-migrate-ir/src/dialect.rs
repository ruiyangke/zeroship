//! Open SQL backend identity.
//!
//! [`DialectId`] is the stable wire, registry, set, and map key. A backend crate
//! declares its own - `DialectId::new("duckdb")` - without editing this crate.
//! Backend spellings, capabilities, validation, guards, and refusal policy live
//! behind the registered backend contracts rather than an identity match here.
//!
//! # This module declares NO ids
//!
//! It used to declare three - `POSTGRES`, `SQLITE`, `MYSQL` - directly above the
//! sentence promising that a backend declares its own without editing this crate,
//! and the engine re-exported all three. So the neutral vocabulary crate at the
//! bottom of the stack named vendors it does not own, and every reference to a
//! vendor's identity anywhere in the tree resolved through a crate that had no
//! business knowing it.
//!
//! They live in the vendors now: `zeroship_migrate_postgres::DIALECT`,
//! `zeroship_migrate_sqlite::DIALECT`, `zeroship_migrate_mysql::DIALECT`, each beside the
//! `BackendVendor` it identifies. Nothing in this crate resolves a dialect by name,
//! and nothing in it can - [`DialectId::new`] is `const` and `pub`, so a crate that
//! must name one and cannot depend on a vendor (this one, and
//! `zero-migrate-backend`, which all three vendors depend on) builds it. Equality is
//! by CONTENT, so an id built that way IS the vendor's.

use core::fmt;
use std::borrow::Cow;

use schemars::JsonSchema;
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};

/// An opaque dialect identity with a stable string name.
///
/// NOT an enum: core code cannot exhaustively match it, which is the property
/// that lets a backend live in a crate the core does not own.
///
/// # Equality is by CONTENT
///
/// `PartialEq`/`Ord`/`Hash` are derived over the string, so they compare the
/// STRING, not the pointer. Two crates that both spell `"postgres"` are the same
/// dialect. That is the desired behaviour and it is also the hazard: nothing
/// structurally prevents two backends from claiming the same name, the way a
/// closed enum did by construction. The rule that covers it is
/// [`crate::backend::BackendRegistry`], which refuses to build over a duplicate
/// id and names both registrants - it is never last-one-wins.
///
/// # Validity
///
/// A well-formed id is lowercase ASCII matching `[a-z][a-z0-9_]*`. There are no
/// aliases and no display names in the id; a human-facing name is a separate
/// field on [`crate::backend::BackendDescriptor`]. [`DialectId::new`] is `const`
/// and does NOT check - a `const` constructor cannot return a `Result` usefully
/// - so the check is enforced at REGISTRATION rather than trusted. See
/// [`DialectId::is_well_formed`].
/// Backend declarations use the borrowed form through [`DialectId::new`]. Wire
/// data uses the owned form: a deserialized migration must be able to name a
/// backend that did not exist when this crate was compiled, without leaking the
/// input string to manufacture a fake `&'static str`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct DialectId(#[schemars(regex(pattern = r"^[a-z][a-z0-9_]*$"))] Cow<'static, str>);

impl DialectId {
    /// Declare an id. `const`, so a backend crate can write
    /// `pub const DUCKDB: DialectId = DialectId::new("duckdb");` at item scope.
    ///
    /// This does NOT validate. Validity is enforced where it can produce a
    /// diagnostic naming the offender: [`crate::backend::BackendRegistry::build`].
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self(Cow::Borrowed(name))
    }

    /// The id's stable string name.
    #[must_use]
    pub const fn as_str(&self) -> &str {
        match &self.0 {
            Cow::Borrowed(name) => name,
            Cow::Owned(name) => name.as_str(),
        }
    }

    /// Whether this id satisfies the id rule: lowercase ASCII `[a-z][a-z0-9_]*`.
    ///
    /// Rejects the empty string, a leading digit or underscore, uppercase, dots,
    /// dashes, and any non-ASCII byte. The registry asserts the same rule at build
    /// time because a backend is not trusted about its own declaration.
    ///
    /// To assert it at COMPILE time - which is what a backend crate wants for its
    /// own declaration - use [`DialectId::is_well_formed_name`]. This method cannot
    /// do that job: reading a `DialectId` const inside a `const` item copies it, and
    /// const evaluation refuses to run the `Cow`'s destructor (E0493).
    #[must_use]
    pub const fn is_well_formed(&self) -> bool {
        Self::is_well_formed_name(self.as_str())
    }

    /// Whether `name` satisfies the id rule: lowercase ASCII `[a-z][a-z0-9_]*`.
    ///
    /// The string-taking form, so a backend can hold itself to the rule where the
    /// diagnostic is a compile error naming its own line:
    ///
    /// ```
    /// use zeroship_migrate_ir::dialect::DialectId;
    /// pub const DIALECT: DialectId = DialectId::new("duckdb");
    /// const _: () = assert!(DialectId::is_well_formed_name("duckdb"));
    /// ```
    ///
    /// [`DialectId::is_well_formed`] delegates here, so the two can never disagree.
    #[must_use]
    pub const fn is_well_formed_name(name: &str) -> bool {
        let bytes = name.as_bytes();
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

impl<'de> Deserialize<'de> for DialectId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let id = Self(Cow::Owned(String::deserialize(deserializer)?));
        if id.is_well_formed() {
            Ok(id)
        } else {
            Err(D::Error::custom(format!(
                "dialect id {:?} must match [a-z][a-z0-9_]*",
                id.as_str()
            )))
        }
    }
}

impl fmt::Debug for DialectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DialectId({:?})", self.0)
    }
}

impl fmt::Display for DialectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A set of dialect identities, with NO cap on how many it can hold.
///
/// This replaces `DialectSet(u8)`, whose three used bits and five spare ones put
/// a hard ceiling of EIGHT backends on the engine. The ceiling was never the
/// bits alone: the set was keyed by a closed three-variant enum, so an id with
/// no variant had no bit to occupy and vanished on insertion. Both are gone -
/// membership is now the id itself.
///
/// Members are kept sorted and deduplicated, so `PartialEq` is SET equality
/// (insertion order does not matter) and lookup is a binary search.
///
/// Lifted here from `zeroship_migrate::model::support` (which re-exports it
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

    /// Build from an arbitrary run of dialect identities. Duplicates collapse.
    #[must_use]
    pub fn from_ids(ids: impl IntoIterator<Item = DialectId>) -> Self {
        let mut members: Vec<DialectId> = ids.into_iter().collect();
        members.sort_unstable();
        members.dedup();
        Self(Cow::Owned(members))
    }

    /// Whether an id is a member.
    #[must_use]
    pub fn contains_id(&self, id: &DialectId) -> bool {
        self.0.binary_search(id).is_ok()
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
    pub fn iter(&self) -> impl Iterator<Item = &DialectId> + '_ {
        self.0.iter()
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

    // A well-formedness test over the shipping ids USED TO LIVE HERE. It looped over the three
    // constants this module declared and asserted the id rule on each. Both halves of
    // it moved and the proposition is now checked more strongly than a test can:
    //
    // * the three ids live in the vendor crates, so this crate cannot see them; and
    // * each vendor asserts its own with `const _: () = assert!(
    //   DialectId::is_well_formed_name(NAME));`, which is a COMPILE error naming the
    //   offending line rather than a test failure naming a value.
    //
    // A fourth backend gets the same check by writing the same line, which the old
    // test could never have given it - it enumerated three names it had to be
    // edited to extend.

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
            "",                   // empty
            "Postgres",           // uppercase
            "1postgres",          // leading digit
            "_postgres",          // leading underscore
            "post-gres",          // dash
            "post.gres",          // dot
            "post gres",          // space
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
    fn a_dialect_set_is_unbounded_and_order_free() {
        let many: Vec<DialectId> = (0..64)
            .map(|i| {
                let name: &'static str = Box::leak(format!("backend_{i}").into_boxed_str());
                DialectId::new(name)
            })
            .collect();
        let set = DialectSet::from_ids(many.iter().cloned());
        assert_eq!(set.len(), 64);
        for id in &many {
            assert!(set.contains_id(id), "{id} must be a member");
        }
        assert!(!set.contains_id(&DialectId::new("absent")));

        // Set equality, not sequence equality.
        let mut reversed = many.clone();
        reversed.reverse();
        assert_eq!(DialectSet::from_ids(reversed), set);
        // Duplicates collapse.
        let doubled = many.iter().cloned().chain(many.iter().cloned());
        assert_eq!(DialectSet::from_ids(doubled).len(), 64);
    }
}
