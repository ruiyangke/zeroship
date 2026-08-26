//! Vendor attributes: the neutral carrier for backend-specific knobs.
//!
//! # What this is for
//!
//! The `Op` enum is closed - a fixed variant set under `deny_unknown_fields` - and that closedness is
//! load-bearing: it makes every `match` in the fold, the lower, validate and `op_support`
//! total, and a `_ =>` arm is where fail-open lives.
//!
//! But a backend has knobs the closed set does not model, and today they arrive by being
//! hand-added to THIS crate. `IndexStorageParams` USED TO hold `pages_per_range`
//! (BRIN) and `fillfactor` - two PostgreSQL storage parameters, in the crate whose whole
//! purpose is to name no vendor. They survived every naming census because they are a
//! FIELD, not a name.
//!
//! So vendor-specific data is already in the IR. This module changes only whether it is
//! centrally defined and closed, or vendor-owned and extensible.
//!
//! # Why the key carries its dialect
//!
//! An [`AttrKey`] is `<dialect>.<name>` - `postgres.fillfactor`, `mysql.row_format` -
//! parsed exactly like `KnobKey` (in `zero-migrate-policy`) parses a charter knob,
//! because it is the same problem and the charter solved it first.
//!
//! The prefix is not decoration. It buys four things a nested
//! `map<DialectId, map<name, value>>` does not:
//!
//! * **One map**, so one sort order and one canonical serialization for the checksum.
//! * **A key is self-describing wherever it lands** - a refusal message, a drift row, a
//!   JSON diff. Nesting loses the dialect the moment the inner key is quoted alone.
//! * **An unprefixed attribute is unrepresentable.** With nesting you can hold an inner
//!   map and lose which outer key it came from; here the dialect is inside the name.
//! * **Collisions are structurally impossible.** `postgres.fillfactor` and
//!   `mysql.fillfactor` are different keys, needing no coordination between vendors.
//!
//! It also makes the two "this key is not mine" cases distinguishable, which is the
//! difference between a skip and a refusal:
//!
//! * `postgres.fillfactor` on a SQLite target - SQLite was never asked. SKIPPED.
//! * `postgres.filfactor` on any target - PostgreSQL IS registered and does not declare
//!   that leaf. REFUSED.
//!
//! A nested carrier can express the first but not the second, because the typo is
//! indistinguishable from a key belonging to nobody.
//!
//! # What lives here and what does not
//!
//! This module is the neutral VOCABULARY only: the key, the value carrier, and the map.
//! It knows no dialect ids and no attribute names. Which keys exist, what shapes their
//! values take and whether they survive a catalog round-trip are declared by each vendor
//! crate through the backend contract - see `zero_migrate_backend::attribute`.
//!
//! Values are [`IrScalar`], not a new type: it already exists, is closed, is
//! checksum-stable and normalizes non-canonical encodings, so an attribute cannot carry
//! something the wire cannot.

use std::borrow::Cow;
use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ir::IrScalar;

/// A namespaced attribute key: `<dialect>.<name>`.
///
/// The namespace is a backend's [`DialectId`](crate::dialect::DialectId) and the leaf is
/// that backend's own name for the knob. Neither is interpreted here - this type only
/// guarantees the SHAPE, so that a key can always be split back into the pair.
///
/// # Two constructors, and why the const one exists
///
/// [`AttrKey::parse`] is the runtime door, taken by anything arriving from outside the
/// process - a deserialized migration, a user's DSL call.
///
/// [`AttrKey::from_static`] is a `const fn` for the DECLARATION side. A vendor crate
/// declares its attribute vocabulary as a `static`, which is const-evaluated, so a
/// malformed key there is a COMPILE error in the vendor's own crate rather than a
/// refusal raised the first time someone happens to author that attribute. That mirrors
/// how `BackendVendor` makes a missing guard an E0063 at the vendor's definition site
/// instead of a silently trusting default.
///
/// `Cow` rather than `String` is what makes that possible: a declared key borrows a
/// `&'static str` and allocates nothing, while a parsed one owns its bytes.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub struct AttrKey(Cow<'static, str>);

/// Why an attribute key was rejected. Structured, because it reaches a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrKeyError {
    /// Not exactly `namespace.name` - no dot, more than one, or an empty part.
    Malformed,
    /// A byte outside `[a-z0-9_]` in one of the two segments.
    IllegalChar,
}

impl std::fmt::Display for AttrKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str(
                "an attribute key must be `<dialect>.<name>` — exactly one dot, neither \
                 part empty",
            ),
            Self::IllegalChar => f.write_str(
                "an attribute key may use only lowercase letters, digits and underscore \
                 in each part",
            ),
        }
    }
}

impl std::error::Error for AttrKeyError {}

impl AttrKey {
    /// Parse and validate `<dialect>.<name>`.
    ///
    /// Deliberately the same rule as
    /// `KnobKey::parse` in `zero-migrate-policy` applies to charter keys -
    /// one dot, both parts non-empty, `[a-z0-9_]` only. Not a shared implementation
    /// because that type lives in the policy crate, which this leaf does not depend on;
    /// the RULE is shared and this doc is the link between them.
    ///
    /// # Errors
    /// [`AttrKeyError::Malformed`] for the wrong number of parts or an empty one;
    /// [`AttrKeyError::IllegalChar`] for a byte outside the permitted set.
    pub fn parse(s: &str) -> Result<Self, AttrKeyError> {
        let parts: Vec<&str> = s.split('.').collect();
        let [dialect, name] = parts.as_slice() else {
            return Err(AttrKeyError::Malformed);
        };
        if dialect.is_empty() || name.is_empty() {
            return Err(AttrKeyError::Malformed);
        }
        let ok = |seg: &str| {
            seg.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        };
        if !ok(dialect) || !ok(name) {
            return Err(AttrKeyError::IllegalChar);
        }
        Ok(Self(Cow::Owned(s.to_string())))
    }

    /// Build a key at COMPILE time, panicking during const evaluation if it is malformed.
    ///
    /// This is the constructor a vendor crate uses to declare its own vocabulary. Because
    /// a `static` is const-evaluated, a typo in a declared key fails the BUILD, points at
    /// the vendor's own source, and can never reach a user as a runtime refusal.
    ///
    /// The rule enforced here is byte-for-byte the rule [`AttrKey::parse`] enforces - it
    /// is written twice because a `const fn` cannot call the iterator-and-closure form.
    /// That duplication is a real drift risk, so it is not left to inspection: a test
    /// runs BOTH constructors over one shared corpus and asserts they agree on every
    /// case, and it is the corpus, not either implementation, that new cases get added
    /// to.
    ///
    /// # The compile-time property, pinned rather than asserted
    ///
    /// A well-formed declared key compiles:
    ///
    /// ```
    /// use zero_migrate_ir::attribute::AttrKey;
    /// static FILLFACTOR: AttrKey = AttrKey::from_static("postgres.fillfactor");
    /// assert_eq!(FILLFACTOR.dialect(), "postgres");
    /// ```
    ///
    /// A malformed one does NOT - `error[E0080]: evaluation panicked`, pointing at the
    /// declaring `static`:
    ///
    /// ```compile_fail
    /// use zero_migrate_ir::attribute::AttrKey;
    /// static UNPREFIXED: AttrKey = AttrKey::from_static("fillfactor");
    /// ```
    ///
    /// READ THIS BEFORE EDITING. A `compile_fail` block passes when its code fails to
    /// compile for ANY reason, so it silently stops testing anything the moment a name
    /// in it goes stale. This one was verified BY INVERSION against the real tree:
    /// appending the same `static` with a VALID key built clean, and with `"nodot"`
    /// produced exactly `evaluation of `attribute::SCRATCH_BAD` failed inside this call`.
    /// The passing block above is the standing half of that inversion - if `from_static`
    /// or `dialect` is renamed, it goes red instead of leaving the `compile_fail` to
    /// pass for the wrong reason.
    ///
    /// # Panics
    /// At compile time, when used in a const context, if `s` is not `<dialect>.<name>`
    /// with both parts non-empty and drawn from `[a-z0-9_]`. (Called at runtime it
    /// panics for real, which is why runtime callers take `parse` instead.)
    #[must_use]
    pub const fn from_static(s: &'static str) -> Self {
        let bytes = s.as_bytes();
        let mut i = 0;
        let mut dots = 0;
        let mut dot_at = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'.' {
                dots += 1;
                dot_at = i;
            } else if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
                panic!(
                    "an attribute key may use only lowercase letters, digits and \
                     underscore in each part"
                );
            }
            i += 1;
        }
        assert!(
            dots == 1,
            "an attribute key must be `<dialect>.<name>` — exactly one dot"
        );
        assert!(
            dot_at != 0 && dot_at != bytes.len() - 1,
            "an attribute key must be `<dialect>.<name>` — neither part may be empty"
        );
        Self(Cow::Borrowed(s))
    }

    /// The whole key, `<dialect>.<name>`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The namespace - the id of the backend that owns this attribute.
    ///
    /// Returned as `&str` rather than a `DialectId` on purpose: this type promises only
    /// that the namespace is well-SHAPED. Whether any backend is registered under it is
    /// the registry's question, asked at validate, and answering it here would put a
    /// resolution the leaf cannot perform into a parse.
    #[must_use]
    pub fn dialect(&self) -> &str {
        let (dialect, _) = self.0.split_once('.').expect("parsed key holds one dot");
        dialect
    }

    /// The leaf - the owning backend's own name for the knob.
    #[must_use]
    pub fn name(&self) -> &str {
        let (_, name) = self.0.split_once('.').expect("parsed key holds one dot");
        name
    }
}

impl std::fmt::Display for AttrKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for AttrKey {
    type Error = AttrKeyError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<AttrKey> for String {
    fn from(k: AttrKey) -> Self {
        k.0.into_owned()
    }
}

/// The attributes carried by one IR node.
///
/// A `BTreeMap` rather than a `HashMap` because the order is part of the CHECKSUM: the
/// canonical serialization must not depend on insertion order, or one authored migration
/// hashes differently per run.
///
/// Empty is the overwhelmingly common case and serializes away entirely
/// (`skip_serializing_if`), so adding this field to an op does not change the wire form
/// of any migration that carries no attributes - which is every migration authored before
/// this existed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct Attributes(BTreeMap<AttrKey, IrScalar>);

impl Attributes {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// True when nothing is carried. Drives `skip_serializing_if`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many attributes are carried, across all dialects.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Insert one attribute, returning any previous value for the same key.
    pub fn insert(&mut self, key: AttrKey, value: IrScalar) -> Option<IrScalar> {
        self.0.insert(key, value)
    }

    /// The value for one key, if carried.
    #[must_use]
    pub fn get(&self, key: &AttrKey) -> Option<&IrScalar> {
        self.0.get(key)
    }

    /// Every attribute, in canonical key order.
    pub fn iter(&self) -> impl Iterator<Item = (&AttrKey, &IrScalar)> {
        self.0.iter()
    }

    /// The distinct dialect namespaces present, in canonical order.
    ///
    /// This is what the reach rule reads, and the rule is narrower than it first looks: a
    /// node carrying attributes for SEVERAL registered dialects was authored for all of
    /// them and stays portable. Only a node whose attributes name exactly ONE dialect is
    /// pinned to it. A table carrying `postgres.*`, `mysql.*` AND `sqlite.*` deploys
    /// everywhere; the naive rule "has attributes, therefore pin" would refuse it on
    /// every target.
    #[must_use]
    pub fn dialects(&self) -> Vec<&str> {
        let mut seen: Vec<&str> = self.0.keys().map(AttrKey::dialect).collect();
        seen.dedup();
        seen
    }

    /// Every attribute owned by one dialect, in canonical order.
    pub fn for_dialect<'a>(
        &'a self,
        dialect: &'a str,
    ) -> impl Iterator<Item = (&'a AttrKey, &'a IrScalar)> {
        self.0.iter().filter(move |(k, _)| k.dialect() == dialect)
    }
}

impl FromIterator<(AttrKey, IrScalar)> for Attributes {
    fn from_iter<T: IntoIterator<Item = (AttrKey, IrScalar)>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// One op's attribute set, typed so the op it belongs to is a compile-time fact.
///
/// # Why the carrier is per-OP and not per-scope
///
/// An earlier design keyed attributes by a SCOPE enum - `Table`, `Column`,
/// `Index`, `Constraint` - with one wrapper per scope shared across every op at that
/// scope. That axis was an INVENTION: a second taxonomy shadowing the op set, which then
/// had to be kept honest against it by hand. It was not kept honest - the `Index` scope
/// shipped with no carrier at all, declarable and permanently unmatchable, and only a
/// test written specially for the purpose caught it.
///
/// Ops are the taxonomy this crate already has, and [`Op`](crate::ir::Op) is already
/// closed. Keying carriers to ops removes the shadow: there is nothing to keep in sync,
/// because the thing being named already exists.
///
/// # What `OP` is, and why it is a string
///
/// `OP` is the op's canonical wire kind - `"createTable"`, not a Rust path - because that
/// is the spelling the vocabulary, the exported artifact and the TypeScript generator all
/// share. It is also the spelling `dialect-support.toml` uses for its 56 rows, which is
/// what makes a declared op name CHECKABLE: a vendor that writes `"createTabel"` is
/// caught against that canonical list rather than silently declaring a knob no op will
/// ever match. That check is a test, not a type, because the vocabulary is data.
pub trait OpAttributes {
    /// The canonical wire kind of the op this set hangs off.
    const OP: &'static str;

    /// The attributes themselves.
    fn attributes(&self) -> &Attributes;

    /// Mutable access, for building a set up.
    fn attributes_mut(&mut self) -> &mut Attributes;
}

/// Mint one op-carrying wrapper.
///
/// The wrappers are identical but for their op and their prose, which is exactly why they
/// are generated: a hand-copied one is free to differ in a way nothing checks. `OP` is the
/// single line that varies, so it is the single thing the macro takes.
///
/// Each is a TRANSPARENT wrapper - it serializes exactly as the bare map does, so it costs
/// nothing on the wire, and an empty one is skipped entirely by its field's
/// `skip_serializing_if`.
macro_rules! op_attributes {
    ($(#[$meta:meta])* $name:ident => $op:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
        #[serde(transparent)]
        #[schemars(transparent)]
        pub struct $name(Attributes);

        impl $name {
            /// An empty set.
            #[must_use]
            pub fn new() -> Self {
                Self(Attributes::new())
            }

            /// True when nothing is carried. Drives `skip_serializing_if`.
            #[must_use]
            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }

            /// Mutable access, for building a set up.
            pub fn entries_mut(&mut self) -> &mut Attributes {
                &mut self.0
            }
        }

        impl From<Attributes> for $name {
            fn from(attrs: Attributes) -> Self {
                Self(attrs)
            }
        }

        impl OpAttributes for $name {
            const OP: &'static str = $op;

            fn attributes(&self) -> &Attributes {
                &self.0
            }

            fn attributes_mut(&mut self) -> &mut Attributes {
                &mut self.0
            }
        }
    };
}

op_attributes! {
    /// Attributes on `Op::CreateTable` - the table's own storage options.
    CreateTableAttributes => "createTable"
}

op_attributes! {
    /// Attributes on `Op::CreatePartition`.
    ///
    /// A partition IS a table and takes its own storage options; PostgreSQL lets one
    /// override what its parent declared. Its own carrier rather than a shared
    /// table-scoped one, because "same knob" is a property of the KEY, not of a shared
    /// Rust type - and a vendor may legitimately permit a knob on `createTable` and not
    /// on `createPartition`.
    CreatePartitionAttributes => "createPartition"
}

op_attributes! {
    /// Attributes on `Op::SetTableOptions` - how a table option is CHANGED after
    /// creation.
    ///
    /// Without this carrier a knob could be set at create and never altered, which is a
    /// limitation no server imposes.
    SetTableOptionsAttributes => "setTableOptions"
}

op_attributes! {
    /// Attributes on `Op::CreateIndex`.
    ///
    /// Attributes on `Op::CreateIndex` -- an index's storage parameters.
    ///
    /// This carrier RETIRED `IndexStorageParams`, which held PostgreSQL's `fillfactor`
    /// and `pages_per_range` as named fields in this neutral crate and in the neutral
    /// contract crate's `IndexSnapshot`. Both are now ordinary declarations in the
    /// PostgreSQL crate, and core's drift pass no longer spells either name.
    CreateIndexAttributes => "createIndex"
}

op_attributes! {
    /// Attributes on `Op::AddColumn` - per-column vendor options such as PostgreSQL's
    /// `STORAGE` and `COMPRESSION`.
    ///
    /// `Op::AddColumn` is FLAT - it does not embed an `IrColumn`, and its own doc explains
    /// that an added column deliberately has no `id_prefix` slot ("an added column is
    /// NEVER the system PK"), also omitting `unique`, `references` and `collation`. The
    /// carrier sits on the op because that is where the column's definition sits.
    AddColumnAttributes => "addColumn"
}

op_attributes! {
    /// Attributes on `Op::AddConstraint`.
    ///
    /// `IrConstraint` is `{name, kind}` and models no deferrability, so
    /// `DEFERRABLE`/`INITIALLY DEFERRED` - which PostgreSQL and SQLite have and MySQL does
    /// not - is a genuine vendor surface here rather than a duplicate of something already
    /// carried.
    AddConstraintAttributes => "addConstraint"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> AttrKey {
        AttrKey::parse(s).unwrap_or_else(|e| panic!("`{s}` must parse: {e}"))
    }

    #[test]
    fn a_key_splits_into_the_dialect_that_owns_it_and_the_name_it_gave_it() {
        let k = key("postgres.fillfactor");
        assert_eq!(k.dialect(), "postgres");
        assert_eq!(k.name(), "fillfactor");
        assert_eq!(k.as_str(), "postgres.fillfactor");
    }

    /// The whole point of the prefix: two vendors may use one word without colliding.
    #[test]
    fn the_same_leaf_under_two_dialects_is_two_keys() {
        let pg = key("postgres.fillfactor");
        let my = key("mysql.fillfactor");
        assert_ne!(pg, my, "the dialect prefix must make these distinct keys");
        assert_eq!(
            pg.name(),
            my.name(),
            "the control: the LEAF really is shared"
        );

        let mut attrs = Attributes::new();
        attrs.insert(pg, IrScalar::Int(90));
        attrs.insert(my, IrScalar::Int(70));
        assert_eq!(attrs.len(), 2, "one must not have overwritten the other");
    }

    /// THE ONE PLACE a new key-rule case gets added.
    ///
    /// `AttrKey` has two constructors enforcing one rule - `parse` for runtime input and
    /// the `const fn` `from_static` for vendor declarations - and a `const fn` cannot
    /// call the iterator-and-closure form the first is written in. So the rule IS
    /// implemented twice, and the guard against them drifting is that every case below
    /// is run through BOTH.
    ///
    /// Add cases here, never to an individual test.
    fn key_corpus() -> Vec<(&'static str, Result<(), AttrKeyError>)> {
        vec![
            // Accepted.
            ("postgres.fillfactor", Ok(())),
            ("mysql.row_format", Ok(())),
            ("sqlite.strict", Ok(())),
            // The control on the permitted set: digits and underscore really are
            // admitted, so the IllegalChar rows below reject the CHARACTER, not the shape.
            ("postgres.autovacuum_vacuum_scale_factor", Ok(())),
            ("mysql8.key_block_size", Ok(())),
            // Wrong number of parts, or an empty one.
            ("fillfactor", Err(AttrKeyError::Malformed)),
            ("postgres.storage.extra", Err(AttrKeyError::Malformed)),
            ("", Err(AttrKeyError::Malformed)),
            (".", Err(AttrKeyError::Malformed)),
            ("postgres.", Err(AttrKeyError::Malformed)),
            (".storage", Err(AttrKeyError::Malformed)),
            // A byte outside [a-z0-9_].
            ("Postgres.storage", Err(AttrKeyError::IllegalChar)),
            ("postgres.Storage", Err(AttrKeyError::IllegalChar)),
            ("postgres.stor-age", Err(AttrKeyError::IllegalChar)),
            ("pg sql.x", Err(AttrKeyError::IllegalChar)),
        ]
    }

    #[test]
    fn parse_answers_the_key_rule_over_the_whole_corpus() {
        for (input, expected) in key_corpus() {
            let got = AttrKey::parse(input).map(|_| ());
            assert_eq!(got, expected, "`{input}`");
        }
    }

    /// The anti-drift check: the `const fn` declaration constructor must accept and
    /// reject exactly what `parse` does.
    ///
    /// `from_static` panics instead of returning, because a malformed key in a vendor's
    /// `static` has to fail the BUILD rather than become a runtime refusal. Called at
    /// runtime that panic is catchable, which is what lets one corpus judge both.
    #[test]
    fn the_const_constructor_agrees_with_parse_on_every_corpus_case() {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // the panics below are expected; stay quiet
        let outcomes: Vec<_> = key_corpus()
            .into_iter()
            .map(|(input, expected)| {
                let accepted = std::panic::catch_unwind(|| AttrKey::from_static(input)).is_ok();
                (input, expected, accepted)
            })
            .collect();
        std::panic::set_hook(hook);

        for (input, expected, accepted) in outcomes {
            assert_eq!(
                accepted,
                expected.is_ok(),
                "`{input}`: from_static and parse disagree — the two implementations of \
                 the key rule have drifted"
            );
        }
    }

    /// Canonical order is part of the checksum, so it must not follow insertion.
    #[test]
    fn iteration_is_canonically_ordered_not_insertion_ordered() {
        let mut a = Attributes::new();
        a.insert(key("sqlite.strict"), IrScalar::Bool(true));
        a.insert(key("mysql.engine"), IrScalar::Str("innodb".into()));
        a.insert(key("postgres.fillfactor"), IrScalar::Int(85));

        let mut b = Attributes::new();
        b.insert(key("postgres.fillfactor"), IrScalar::Int(85));
        b.insert(key("sqlite.strict"), IrScalar::Bool(true));
        b.insert(key("mysql.engine"), IrScalar::Str("innodb".into()));

        let order = |x: &Attributes| x.iter().map(|(k, _)| k.to_string()).collect::<Vec<_>>();
        assert_eq!(
            order(&a),
            order(&b),
            "two insertion orders, one canonical order"
        );
        assert_eq!(
            order(&a),
            vec!["mysql.engine", "postgres.fillfactor", "sqlite.strict"],
            "and that order is sorted by key"
        );
    }

    /// The reach rule reads this, and it is narrower than "carries attributes".
    #[test]
    fn a_node_authored_for_three_dialects_names_three_not_one() {
        let mut all = Attributes::new();
        all.insert(key("postgres.fillfactor"), IrScalar::Int(85));
        all.insert(key("mysql.engine"), IrScalar::Str("innodb".into()));
        all.insert(key("sqlite.strict"), IrScalar::Bool(true));
        assert_eq!(
            all.dialects(),
            vec!["mysql", "postgres", "sqlite"],
            "carrying attributes for every backend must NOT read as pinned to one — the \
             naive rule would refuse this node on every target"
        );

        let mut one = Attributes::new();
        one.insert(key("postgres.fillfactor"), IrScalar::Int(85));
        one.insert(key("postgres.parallel_workers"), IrScalar::Int(4));
        assert_eq!(
            one.dialects(),
            vec!["postgres"],
            "two attributes from ONE backend is the pinned case"
        );
    }

    #[test]
    fn one_dialects_attributes_can_be_read_without_seeing_anothers() {
        let mut a = Attributes::new();
        a.insert(key("postgres.fillfactor"), IrScalar::Int(85));
        a.insert(key("postgres.parallel_workers"), IrScalar::Int(4));
        a.insert(key("mysql.engine"), IrScalar::Str("innodb".into()));

        let pg: Vec<&str> = a.for_dialect("postgres").map(|(k, _)| k.name()).collect();
        assert_eq!(pg, vec!["fillfactor", "parallel_workers"]);

        let none: Vec<&str> = a.for_dialect("duckdb").map(|(k, _)| k.name()).collect();
        assert!(
            none.is_empty(),
            "an unregistered namespace simply yields nothing here"
        );
    }

    /// Adding this field must not change the wire form of a migration that carries none.
    #[test]
    fn an_empty_set_round_trips_as_an_empty_map() {
        let empty = Attributes::new();
        assert!(empty.is_empty());
        let json = serde_json::to_string(&empty).expect("serialize");
        assert_eq!(json, "{}");
        let back: Attributes = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, empty);
    }

    #[test]
    fn a_populated_set_round_trips_through_the_wire() {
        let mut a = Attributes::new();
        a.insert(key("postgres.fillfactor"), IrScalar::Int(85));
        a.insert(key("sqlite.strict"), IrScalar::Bool(true));
        a.insert(key("mysql.engine"), IrScalar::Str("innodb".into()));

        let json = serde_json::to_string(&a).expect("serialize");
        let back: Attributes = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, a, "the carrier must survive the wire unchanged");
        assert_eq!(
            json, r#"{"mysql.engine":"innodb","postgres.fillfactor":85,"sqlite.strict":true}"#,
            "and it must serialize in canonical key order, because the checksum reads it"
        );
    }

    /// A malformed key must die at DESERIALIZE, not survive as data.
    #[test]
    fn the_wire_refuses_a_malformed_key_rather_than_carrying_it() {
        let bad = r#"{"fillfactor":85}"#;
        assert!(
            serde_json::from_str::<Attributes>(bad).is_err(),
            "an unprefixed key must not deserialize — the prefix is the invariant"
        );
        let good = r#"{"postgres.fillfactor":85}"#;
        assert!(
            serde_json::from_str::<Attributes>(good).is_ok(),
            "the control: the same shape WITH a prefix does deserialize"
        );
    }
}
