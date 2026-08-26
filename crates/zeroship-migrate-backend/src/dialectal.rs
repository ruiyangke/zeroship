//! The DIALECTAL CARRIER: **run-time**, vendor-OWNED values, keyed by the backend
//! that owns them.
//!
//! [`crate::registry::BackendVendor`] already carries everything a vendor knows
//! STATICALLY - its renderers and its policies, all `&'static dyn`. This module is
//! the other half: a place for values a vendor owns that are only known when a
//! process is running, because they were read off a live catalog or handed in by
//! the host. Those cannot be `&'static`, so they cannot live on `BackendVendor`,
//! and that asymmetry is the whole reason this type exists.
//!
//! # The shape, and why it is keyed rather than typed
//!
//! [`zero_migrate_ir::ir::Op::Dialectal`] is the precedent: a `BTreeMap` keyed by
//! the open [`DialectId`], so a fourth backend selects its own key without any
//! neutral crate naming it. A [`Dialectal`] is that same map, carrying an
//! `Arc<dyn ...>` instead of an `Op` sequence, so the VALUE's type stays inside the
//! vendor crate that defines it. Core can hold one, clone one, compare one and
//! hand one back - and cannot read one, because reading takes a `downcast_ref` to
//! a concrete type only the owning vendor names.
//!
//! That is the point. A typed `Option<VendorThing>` field on a neutral struct puts
//! the vendor's name in the neutral vocabulary AND lets neutral code `match` on
//! the vendor's grammar. A keyed `Arc<dyn>` does neither.
//!
//! # What a lookup MISS means
//!
//! A miss is `None`, never a default and never a panic, and each carrier's own
//! consumer decides what `None` means for it. Two answers are in the tree:
//!
//! * **Catalog facts** ([`VendorColumnFacts`]): a miss means *this dialect
//!   recorded nothing about this column*. That is exactly what the `Option`-typed
//!   vendor field it replaced meant, so relocating a fact into a leg cannot change
//!   an answer. Comparisons PAIR legs by [`DialectId`] and require the dialect on
//!   BOTH sides ([`Dialectal::physical_identity`]), which is the same "both, not
//!   either" rule the typed fields were compared under, now enforced by the shape
//!   rather than by an `if let (Some(_), Some(_))` a reader has to notice.
//!
//! * **Host confinement** ([`VendorConfinement`]): a miss means *the host supplied
//!   no settings for this dialect*, and the owning vendor resolves it to its own
//!   default. A miss cannot be a silently wrong answer there because only the
//!   vendor that owns a leg ever reads it, and the value it falls back to is the
//!   same `Default` the neutral constructor used to install eagerly.
//!
//! # What a mis-keyed leg means
//!
//! [`Dialectal::get`] downcasts, so a leg stored under one dialect's id holding
//! another dialect's type reads back as `None` rather than as a wrong value. Only
//! the crate that defines a value type can name it, so pairing a key with the
//! wrong value is confined to a single vendor crate's own code.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use zero_migrate_ir::dialect::DialectId;

/// A value only the dialect that owns it can interpret.
///
/// The supertraits are the price of being carried by neutral code: `Any` so the
/// owning vendor can get its concrete type back, `Debug` so a neutral struct's
/// `Debug` can still print, and `Send + Sync` so a snapshot or a config can cross
/// a task boundary the way every other field already does.
pub trait DialectalValue: Any + fmt::Debug + Send + Sync {
    /// Erase to [`Any`] so [`Dialectal::get`] can downcast.
    ///
    /// Required rather than blanket-provided: a blanket `impl` would need
    /// `Self: Sized`, which a `dyn` value is not.
    fn as_any(&self) -> &dyn Any;

    /// Structural equality against another leg, which is this dialect's own type
    /// when the two carriers agree about a dialect and something else when they do
    /// not.
    ///
    /// Takes `&dyn Any` rather than `&dyn DialectalValue` so a caller holding a
    /// `dyn` SUBtrait object does not need a trait upcast to ask.
    fn dialectal_eq(&self, other: &dyn Any) -> bool;
}

/// Run-time values owned by the backends that produced them, keyed by
/// [`DialectId`].
///
/// `T` is the `dyn` trait naming what KIND of value this carrier holds, so a
/// column-facts carrier and a confinement carrier are different types and cannot
/// be assigned to one another. See the module doc for miss semantics.
pub struct Dialectal<T: DialectalValue + ?Sized> {
    legs: BTreeMap<DialectId, Arc<T>>,
}

impl<T: DialectalValue + ?Sized> Dialectal<T> {
    /// An empty carrier: no dialect has recorded anything.
    #[must_use]
    pub fn new() -> Self {
        Self {
            legs: BTreeMap::new(),
        }
    }

    /// Record `value` as the leg `dialect` owns, replacing any leg already there.
    pub fn insert(&mut self, dialect: DialectId, value: Arc<T>) {
        self.legs.insert(dialect, value);
    }

    /// The leg `dialect` owns, downcast to the concrete type `V` that dialect's
    /// crate defines.
    ///
    /// `None` for BOTH "no leg for this dialect" and "a leg that is not a `V`".
    /// The two are not distinguished because no caller can act on the difference:
    /// only the vendor that defines `V` asks, and a leg under its own key that is
    /// not its own type cannot be produced from outside its crate.
    #[must_use]
    pub fn get<V: DialectalValue>(&self, dialect: &DialectId) -> Option<&V> {
        self.legs
            .get(dialect)
            .and_then(|leg| leg.as_any().downcast_ref::<V>())
    }

    /// Whether `dialect` has recorded a leg here at all, without naming its type.
    ///
    /// This is the neutral half of provenance: a caller can ask WHETHER a dialect
    /// spoke without being able to hear what it said.
    #[must_use]
    pub fn carries(&self, dialect: &DialectId) -> bool {
        self.legs.contains_key(dialect)
    }

    /// No dialect has recorded anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.legs.is_empty()
    }

    /// Every dialect with a leg here, in [`DialectId`] order.
    pub fn dialects(&self) -> impl Iterator<Item = &DialectId> {
        self.legs.keys()
    }

    /// Every dialect that has a leg on BOTH sides, paired.
    ///
    /// This is the "both, not either" rule the typed vendor fields were compared
    /// under, made structural: a dialect that recorded on one side only is not
    /// yielded, so a comparator built on this cannot accuse a snapshot of
    /// differing from one that never described the same thing.
    pub fn paired<'a>(&'a self, other: &'a Self) -> impl Iterator<Item = (&'a T, &'a T)> {
        self.legs.iter().filter_map(move |(dialect, mine)| {
            other
                .legs
                .get(dialect)
                .map(|theirs| (mine.as_ref(), theirs.as_ref()))
        })
    }
}

impl<T: DialectalValue + ?Sized> Default for Dialectal<T> {
    fn default() -> Self {
        Self::new()
    }
}

// Hand-written: a derive would demand `T: Clone`, and `T` is a `dyn` trait. The
// legs are `Arc`, so this clones the map and bumps refcounts rather than the
// values.
impl<T: DialectalValue + ?Sized> Clone for Dialectal<T> {
    fn clone(&self) -> Self {
        Self {
            legs: self.legs.clone(),
        }
    }
}

// Hand-written for the same reason. Prints as a map from dialect id to the leg's
// own `Debug`, so a neutral struct that carries one still shows what a vendor put
// in it.
impl<T: DialectalValue + ?Sized> fmt::Debug for Dialectal<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.legs.iter().map(|(id, leg)| (id, leg.as_ref())))
            .finish()
    }
}

// Two carriers are equal when they cover the same dialects and every leg says it
// equals its counterpart. The per-leg answer comes from the VENDOR
// (`dialectal_eq`), because what makes two of its values the same is its question,
// not core's.
impl<T: DialectalValue + ?Sized> PartialEq for Dialectal<T> {
    fn eq(&self, other: &Self) -> bool {
        self.legs.len() == other.legs.len()
            && self.legs.iter().all(|(dialect, mine)| {
                other
                    .legs
                    .get(dialect)
                    .is_some_and(|theirs| mine.dialectal_eq(theirs.as_any()))
            })
    }
}

impl<T: DialectalValue + ?Sized> Eq for Dialectal<T> {}

/// The catalog facts one backend recovered about one column, which no other
/// backend and no neutral crate can interpret.
///
/// A backend registers a leg here only for a fact its own catalog carries and the
/// portable [`ColumnSnapshot`](crate::snapshot::ColumnSnapshot) surface cannot:
/// the portable `data_type` is normalized, so a vendor whose normalization loses a
/// distinction it must still be able to compare records the exact identity here
/// and answers the comparison itself.
pub trait VendorColumnFacts: DialectalValue {
    /// Do these facts and `other`'s - the same dialect's, on the other side of a
    /// comparison - describe the same PHYSICAL column?
    ///
    /// A vendor that cannot ESTABLISH a difference must answer `true`: a differ's
    /// safe direction is to decline to report a difference it cannot prove, and
    /// the portable comparison the caller falls back to is still applied.
    fn physical_identity(&self, other: &dyn VendorColumnFacts) -> bool;

    /// How to spell the two sides of a `data_type` drift line, when this vendor's
    /// own facts are what decided [`Self::physical_identity`].
    ///
    /// `None` keeps the caller's portable spelling. `Some` is used verbatim, so a
    /// vendor that returns two equal strings has told the reader nothing - the
    /// caller keeps the portable pair in that case rather than printing a line
    /// that names no difference.
    fn type_drift_report(&self, other: &dyn VendorColumnFacts) -> Option<(String, String)>;
}

impl Dialectal<dyn VendorColumnFacts> {
    /// The VENDOR verdict on whether two columns are the same physical column.
    ///
    /// `None` means no dialect described this column on both sides, so no vendor
    /// is in a position to answer and the caller keeps its portable comparison.
    /// That is the same fall-through the typed `Option` field produced when either
    /// side was absent, and it is deliberate: a fact compared against an absent one
    /// describes nothing about the database.
    #[must_use]
    pub fn physical_identity(&self, other: &Self) -> Option<bool> {
        self.paired(other)
            .map(|(mine, theirs)| mine.physical_identity(theirs))
            .reduce(|left, right| left && right)
    }

    /// The two sides of a `data_type` drift line in the owning vendor's spelling,
    /// or `None` to keep the caller's portable pair.
    #[must_use]
    pub fn type_drift_report(&self, other: &Self) -> Option<(String, String)> {
        self.paired(other)
            .find_map(|(mine, theirs)| mine.type_drift_report(theirs))
    }
}

/// The run-time confinement settings one backend reads, supplied per project by
/// the host.
///
/// Unlike [`VendorColumnFacts`] this is INPUT, not something read off a catalog,
/// which is why it needed a carrier at all: a host-supplied value cannot be the
/// `&'static dyn` that [`crate::registry::BackendVendor`] holds.
pub trait VendorConfinement: DialectalValue {}

#[cfg(test)]
mod tests {
    use super::*;

    const A: DialectId = DialectId::new("a");
    const B: DialectId = DialectId::new("b");

    #[derive(Debug, PartialEq, Eq)]
    struct Facts(&'static str);

    impl DialectalValue for Facts {
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn dialectal_eq(&self, other: &dyn Any) -> bool {
            other.downcast_ref::<Self>().is_some_and(|o| self == o)
        }
    }

    impl VendorColumnFacts for Facts {
        fn physical_identity(&self, other: &dyn VendorColumnFacts) -> bool {
            other
                .as_any()
                .downcast_ref::<Self>()
                .is_some_and(|o| self == o)
        }
        fn type_drift_report(&self, other: &dyn VendorColumnFacts) -> Option<(String, String)> {
            let other = other.as_any().downcast_ref::<Self>()?;
            (self != other).then(|| (self.0.to_string(), other.0.to_string()))
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Other(u8);

    impl DialectalValue for Other {
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn dialectal_eq(&self, other: &dyn Any) -> bool {
            other.downcast_ref::<Self>().is_some_and(|o| self == o)
        }
    }

    fn carrier(entries: &[(DialectId, &'static str)]) -> Dialectal<dyn VendorColumnFacts> {
        let mut carrier: Dialectal<dyn VendorColumnFacts> = Dialectal::new();
        for (dialect, value) in entries {
            carrier.insert(
                dialect.clone(),
                Arc::new(Facts(value)) as Arc<dyn VendorColumnFacts>,
            );
        }
        carrier
    }

    #[test]
    fn an_absent_leg_reads_as_none_rather_than_a_default() {
        let carrier = carrier(&[(A, "x")]);
        assert!(carrier.get::<Facts>(&B).is_none());
        assert!(!carrier.carries(&B));
        assert_eq!(carrier.get::<Facts>(&A), Some(&Facts("x")));
    }

    #[test]
    fn a_leg_of_another_type_reads_as_none_rather_than_a_wrong_value() {
        let mut carrier: Dialectal<dyn DialectalValue> = Dialectal::new();
        carrier.insert(A, Arc::new(Other(7)));
        assert!(carrier.get::<Facts>(&A).is_none());
        // ... and `carries` still reports the leg, which is the distinction the
        // provenance seam needs and the typed read deliberately does not make.
        assert!(carrier.carries(&A));
    }

    #[test]
    fn a_one_sided_leg_is_never_paired_so_no_vendor_answers() {
        let left = carrier(&[(A, "x")]);
        let right = carrier(&[(B, "x")]);
        assert_eq!(left.paired(&right).count(), 0);
        assert_eq!(left.physical_identity(&right), None);
        assert_eq!(left.type_drift_report(&right), None);
    }

    #[test]
    fn an_empty_carrier_on_either_side_declines() {
        let full = carrier(&[(A, "x")]);
        let empty = Dialectal::<dyn VendorColumnFacts>::new();
        assert_eq!(full.physical_identity(&empty), None);
        assert_eq!(empty.physical_identity(&full), None);
    }

    #[test]
    fn a_paired_leg_answers_with_the_vendors_own_verdict() {
        assert_eq!(
            carrier(&[(A, "x")]).physical_identity(&carrier(&[(A, "x")])),
            Some(true)
        );
        assert_eq!(
            carrier(&[(A, "x")]).physical_identity(&carrier(&[(A, "y")])),
            Some(false)
        );
        assert_eq!(
            carrier(&[(A, "x")]).type_drift_report(&carrier(&[(A, "y")])),
            Some(("x".to_string(), "y".to_string()))
        );
    }

    #[test]
    fn equality_asks_every_leg_and_covers_the_same_dialects() {
        assert_eq!(
            carrier(&[(A, "x"), (B, "y")]),
            carrier(&[(A, "x"), (B, "y")])
        );
        assert_ne!(carrier(&[(A, "x")]), carrier(&[(A, "x"), (B, "y")]));
        assert_ne!(carrier(&[(A, "x")]), carrier(&[(B, "x")]));
        assert_ne!(carrier(&[(A, "x")]), carrier(&[(A, "y")]));
    }

    #[test]
    fn debug_prints_each_leg_under_its_dialect() {
        assert_eq!(
            format!("{:?}", carrier(&[(A, "x")])),
            r#"{DialectId("a"): Facts("x")}"#
        );
    }
}
