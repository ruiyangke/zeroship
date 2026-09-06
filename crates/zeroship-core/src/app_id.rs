//! [`AppId`] - the tenant identity of one creator app, as a typed id.
//!
//! # Why this is a type and not a `Uuid` and not a `String`
//!
//! An app id is not only a key. It SEEDS derived identifiers across the data
//! plane: the per-app `PostgreSQL` schema name, the provisioning and runtime role
//! names, the logical-replication publication digest, the replication-slot name,
//! the HKDF salt both per-app encryption keys are derived from, the RLS GUC that
//! carries the tenant in text form, the app-secret AAD, the CHWBL ring position,
//! the KV scope, the object-storage prefix, the dev-tier `SQLite` file name and
//! `ATTACH` alias, the bundle path segment, and the `APP_ID` the isolate reads.
//!
//! Those sites do not agree about which rendering they want, and that is the
//! whole problem. Some want the TENANT STRING (a schema name, a GUC value, a
//! path segment). Two want the EMBEDDED HUNDRED-AND-TWENTY-EIGHT BITS and
//! nothing else: the app-secret AAD in `zeroship_control::env_store` and the
//! consistent-hash ring position in `zeroship_gateway::proxy`. Today every one
//! of them takes a `Uuid` or a `&str`, so both meanings compile at every site
//! and a change of rendering is silently wrong rather than broken.
//!
//! # What this type deliberately does NOT have
//!
//! Following `zeroship_schema::schema_name::SchemaName`, which was defined the
//! same way and for the same reason, this type is characterised by its
//! ABSENCES. Each one is a named failure it exists to prevent.
//!
//! - **No `Display`, no `ToString`.** Every `app_id.to_string()` in the tree
//!   must become a compile error the day this type replaces `Uuid`, because
//!   those calls feed the meter key, the schema name, the deprovision key, the
//!   blob path segment and the isolate's `APP_ID`, and they compile identically
//!   for `Uuid` and for `String`. Forcing a visit to each is the point. Logging
//!   and formatting go through [`AppId::as_str`], which is greppable.
//! - **No inherent `as_bytes`.** `uuid::Uuid::as_bytes` yields the raw hundred
//!   and twenty-eight bits; `str::as_bytes` yields the characters of the printed
//!   id. Both coerce to `&[u8]`, so a receiver that silently changed meaning
//!   would keep compiling - and the two callers that need the raw bits are the
//!   app-secret AAD (every stored app secret) and the CHWBL ring (the whole
//!   worker isolate cache). Those go through [`AppId::uuid`] explicitly.
//! - **No `Deref<Target = str>`, no `AsRef<str>`, no `Borrow<str>`.** These are
//!   the routes by which a tenant id reaches a parameter that wanted a schema
//!   name. `SchemaName` omits the same ones for the same reason. Omitting
//!   `Deref` is also what makes the `as_bytes` absence complete: without it,
//!   `str::as_bytes` is not reachable by autoderef either.
//! - **No `From<&str>`, no `From<String>`, no `new_unchecked`, no public
//!   field.** Construction is [`AppId::mint`] or the fallible [`AppId::parse`].
//!   There is no third way in.
//!
//!   **THERE IS NOW EXACTLY ONE THIRD WAY IN, AND IT IS DATED.**
//!   [`AppId::from_uuid`] wraps the `Uuid` the `zeroship.apps.id` column still
//!   holds, producing a NON-CANONICAL id whose printed form is the hyphenated
//!   uuid. It exists so [`crate::app_derivation`] can become the one producer
//!   of every derived identifier while the column is unchanged, and it is
//!   deleted in the slice that flips the column. The sentence above is kept
//!   rather than rewritten because it states the end state this type is
//!   travelling back to, not a description of the constructor list today.
//!
//!   [`canonical_app_id_for`] is NOT a fourth way in, and the distinction is
//!   the whole reason it is a free function rather than an associated one. It
//!   composes [`crate::typed_id::uuid_to_base62`] with [`AppId::parse`], so
//!   every id it yields is one `parse` already admits; it can construct
//!   nothing this type could not already hold, and it cannot be handed a
//!   string at all. Its docs carry the argument in full.
//! - **No `PartialEq<str>`.** Comparing against a raw string is a decision, not
//!   a convenience.
//!
//! `tests::the_public_surface_omits_every_string_degrading_impl` and
//! `tests::there_is_no_inherent_as_bytes` assert those absences at compile time
//! against a control that reports the same probes as present, so the probes are
//! known to be capable of saying yes.
//!
//! # Ordering
//!
//! `Ord` compares the printed id first, which is byte order over the base62
//! alphabet, which is creation order for a `UUIDv7` body - see
//! [`crate::typed_id::BASE62`]'s note on why that contract belongs to the
//! encoder and the collation together. A database column holding this value
//! inherits `en_US.utf8` unless it is pinned, under which the contract does NOT
//! hold; the day an app id column exists it must be registered in
//! `db/migrations-ts/20260831000001_sortable_entity_id_collations.ts` or its
//! ordering silently breaks. This module adds no column.
//!
//! # Scope
//!
//! `zeroship-cdc-wire` declares its own `AppId` over the same `app_` prefix. The
//! two are deliberately separate types in separate crates: that crate refuses a
//! normal dependency on this one because this crate's closure carries an HTTP
//! client, and `crates/zeroship-cdc-wire/tests/typed_id_oracle.rs` is the
//! differential test that keeps the duplicated parse from drifting from
//! [`crate::typed_id::parse_with_prefix`], which is the parse this type uses.

use core::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::typed_id::{self, APP_PREFIX, ParseError};

/// The typed id of one creator app: `app_<base62(uuidv7)>`.
///
/// Constructed only through [`AppId::mint`] or [`AppId::parse`]; see the module
/// docs for why there is no infallible route in and no way back out to a bare
/// string except the two named accessors.
///
/// Both fields are kept because the printed id and the embedded bits are needed
/// by different call sites and neither is free to recompute at an accessor. The
/// uuid is a total function of the text - [`crate::typed_id::uuid_to_base62`] is
/// a bijection on the whole hundred-and-twenty-eight-bit space - so deriving
/// equality, hashing and ordering over both is equivalent to deriving them over
/// the text alone.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AppId {
    text: String,
    uuid: uuid::Uuid,
}

impl AppId {
    /// The one prefix this type accepts, re-exported from the typed-id registry
    /// so a caller matching on it cannot pick up a second spelling.
    pub const PREFIX: &'static str = APP_PREFIX;

    /// Mint a fresh app id.
    ///
    /// This is the ONLY minter. It composes [`crate::typed_id::new_v7`] and
    /// [`crate::typed_id::uuid_to_base62`] directly rather than calling
    /// [`crate::typed_id::generate`] so that the freshly generated uuid can be
    /// kept without a decode; `tests::mint_agrees_with_the_shared_generator` is
    /// what binds that composition to `generate`'s so the two cannot drift.
    ///
    /// Note that this makes an app id time-ordered. The `zeroship.apps.id`
    /// column it is destined for defaults to a v4 uuid today, which carries no
    /// ordering and no locality.
    #[must_use]
    pub fn mint() -> Self {
        let uuid = typed_id::new_v7();
        Self {
            text: format!("{APP_PREFIX}_{}", typed_id::uuid_to_base62(&uuid)),
            uuid,
        }
    }

    /// Wrap the `Uuid` an app id is STORED as today, so that a call site
    /// holding one can reach [`crate::app_derivation`] without any other type
    /// in the tree changing.
    ///
    /// TRANSITIONAL. This is the third way in that the module docs above say
    /// does not exist, and that contradiction is deliberate and temporary. It
    /// exists so the derivation seam can be introduced while `zeroship.apps.id`
    /// is still `uuid`; it is scheduled for deletion in the slice that flips
    /// that column, and every caller of it disappears with it.
    ///
    /// **The text it produces is NOT canonical, and that is the point.** A
    /// minted id prints `app_<base62>`; this one prints the hyphenated uuid,
    /// which is what every derived identifier in the tree is composed from
    /// today. Passing the result to a derivation therefore returns today's
    /// bytes, which is what makes the seam behaviour-neutral rather than a
    /// silent re-keying of every schema, role, publication and slot.
    ///
    /// Two consequences follow from that, and neither is a defect:
    ///
    /// - `AppId::parse(AppId::from_uuid(u).as_str())` FAILS. A transitional id
    ///   is a derivation input, not a value to round-trip.
    /// - `from_uuid(u)` and a `mint`ed id carrying the same uuid are NOT equal,
    ///   do not hash alike and do not order alike, because equality is derived
    ///   over the printed text. Do not mix the two in one map.
    #[must_use]
    pub fn from_uuid(uuid: &uuid::Uuid) -> Self {
        Self {
            text: uuid.to_string(),
            uuid: *uuid,
        }
    }

    /// Parse a canonical app id.
    ///
    /// Delegates to [`crate::typed_id::parse_with_prefix`], so it refuses
    /// exactly what that refuses and says the same thing: a wrong prefix is a
    /// [`ParseError::WrongPrefix`] boundary rejection, and a wrong length, a
    /// character outside base62, a missing separator or a body above the
    /// hundred-and-twenty-eight-bit range is a [`ParseError::Malformed`].
    ///
    /// # Errors
    ///
    /// See above; the error values are the shared parser's, unwrapped and
    /// unwrapped again into no new vocabulary.
    pub fn parse(raw: &str) -> Result<Self, ParseError> {
        let uuid = typed_id::parse_with_prefix(raw, APP_PREFIX)?;
        Ok(Self {
            text: raw.to_owned(),
            uuid,
        })
    }

    /// The printed id - the TENANT string.
    ///
    /// Named rather than an `AsRef`/`Deref` impl so that every place an app id
    /// degrades back into an untyped string is greppable.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The embedded hundred and twenty eight bits.
    ///
    /// The two callers that must keep deriving from these rather than from the
    /// printed id are the app-secret AAD and the CHWBL ring position; both would
    /// otherwise change value the day the printed form changes, and both fail
    /// quietly when they do.
    #[must_use]
    pub const fn uuid(&self) -> uuid::Uuid {
        self.uuid
    }
}

/// The CANONICAL [`AppId`] of an app the database still stores as a `Uuid`.
///
/// TRANSITIONAL, and the exact pair of [`AppId::from_uuid`]. Both take the
/// `Uuid` in `zeroship.apps.id` and both are deleted when that column flips;
/// they differ in which of the two renderings they produce, and choosing
/// between them is choosing which side of the migration the caller is on:
///
/// - [`AppId::from_uuid`] produces TODAY's bytes, the hyphenated uuid. Feed it
///   to [`crate::app_derivation`] and every derived identifier - schema, role,
///   publication, salt, meter key - comes back exactly as it is spelled in the
///   live database. That is what makes the derivation seam behaviour-neutral.
/// - This produces TOMORROW's bytes, `app_<base62>`, the id the column will
///   hold. It is for the places that carry an app id as an IDENTITY rather than
///   as a derivation input: the gateway-to-worker request path, where the two
///   processes only have to agree with each other.
///
/// Mixing them is a bug the type system cannot catch, because both are an
/// `AppId`. It is a LOUD bug and that is deliberate: equality is over the
/// printed text, so a map keyed by one and probed with the other MISSES rather
/// than silently returning a neighbour's route.
///
/// # This is not a fourth way into the type
///
/// The module docs say construction is a mint or a fallible parse and that
/// there is no other way in. This does not add one. It takes a `Uuid`, not a
/// string, and it funnels through [`AppId::parse`], so the set of values it can
/// produce is a subset of what `parse` already admits - `uuid_to_base62` is a
/// bijection onto exactly twenty-two base62 characters, so the composed id is
/// always well-formed and always round-trips: `canonical_app_id_for(u).uuid()
/// == u`. A free function rather than an associated one so it is not part of
/// the type's constructor surface and so it disappears without touching it.
///
/// # Panics
///
/// Never, for any `Uuid`. The `expect` is unreachable by construction and is
/// bound by `tests::the_canonical_conversion_round_trips_the_extreme_uuids`,
/// which drives the two ends of the hundred-and-twenty-eight-bit range and a
/// sweep of random ones.
#[must_use]
pub fn canonical_app_id_for(stored: &uuid::Uuid) -> AppId {
    let printed = format!("{APP_PREFIX}_{}", typed_id::uuid_to_base62(stored));
    AppId::parse(&printed).expect("uuid_to_base62 yields twenty-two base62 characters")
}

impl Serialize for AppId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text)
    }
}

impl<'de> Deserialize<'de> for AppId {
    /// Decoding goes through [`AppId::parse`], so a uuid-shaped value on the
    /// wire is a DECODE FAILURE rather than a silently accepted key.
    ///
    /// That matters most for the gateway's route and version maps, where an app
    /// id is a map KEY: a producer and consumer disagreeing about the rendering
    /// would otherwise drop entries. It puts the mismatch in the log; it does
    /// not by itself make the gateway fail closed, because a failed poll cycle
    /// retains the previous snapshot.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct AppIdVisitor;

        impl Visitor<'_> for AppIdVisitor {
            type Value = AppId;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "an app typed id of the form {APP_PREFIX}_<base62>")
            }

            fn visit_str<E: de::Error>(self, raw: &str) -> Result<AppId, E> {
                AppId::parse(raw).map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_str(AppIdVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Compile-time probe for whether a type implements a trait, without
    /// specialization: an inherent associated const shadows the trait's default
    /// when - and only when - its bound is satisfied.
    macro_rules! impl_probe {
        ($modname:ident, $bound:path) => {
            mod $modname {
                use core::marker::PhantomData;

                pub struct Probe<T: ?Sized>(PhantomData<T>);

                pub trait Fallback {
                    const IMPLEMENTED: bool = false;
                }

                impl<T: ?Sized> Fallback for Probe<T> {}

                impl<T: ?Sized + $bound> Probe<T> {
                    pub const IMPLEMENTED: bool = true;
                }
            }
        };
    }

    impl_probe!(probe_display, ::core::fmt::Display);
    impl_probe!(probe_to_string, ::std::string::ToString);
    impl_probe!(probe_deref, ::core::ops::Deref);
    impl_probe!(probe_as_ref_str, ::core::convert::AsRef<str>);
    impl_probe!(probe_borrow_str, ::core::borrow::Borrow<str>);
    impl_probe!(probe_from_str_ref, ::core::convert::From<&'static str>);
    impl_probe!(probe_from_string, ::core::convert::From<::std::string::String>);
    impl_probe!(probe_partial_eq_str, ::core::cmp::PartialEq<str>);

    /// The absences the module doc names, asserted mechanically.
    ///
    /// PAIRED WITH A CONTROL. `String` implements every one of these probes, so
    /// the second half proves the probe machinery can report `true`; without it
    /// a probe that had been broken into always answering `false` would print
    /// exactly what a correct one prints.
    ///
    /// MUTATION-CHECKED, both directions. Adding `impl fmt::Display for AppId`
    /// fails the first two assertions and leaves every other test in this module
    /// green. Flipping the inherent `IMPLEMENTED` to `false` - which blinds the
    /// probe into answering `false` for everything, the exact way this test
    /// could pass while measuring nothing - fails the control instead.
    ///
    /// WHAT THIS DOES NOT CATCH: it says nothing about inherent methods. An
    /// inherent `fn to_string(&self) -> String` on `AppId` would satisfy every
    /// assertion here. `there_is_no_inherent_as_bytes` covers the one inherent
    /// method whose absence is load-bearing; the rest is review.
    #[test]
    fn the_public_surface_omits_every_string_degrading_impl() {
        use probe_as_ref_str::Fallback as _;
        use probe_borrow_str::Fallback as _;
        use probe_deref::Fallback as _;
        use probe_display::Fallback as _;
        use probe_from_str_ref::Fallback as _;
        use probe_from_string::Fallback as _;
        use probe_partial_eq_str::Fallback as _;
        use probe_to_string::Fallback as _;

        assert!(
            !probe_display::Probe::<AppId>::IMPLEMENTED,
            "AppId must not implement Display: every app_id.to_string() must be \
             a compile error, not a silently correct-looking rendering"
        );
        assert!(
            !probe_to_string::Probe::<AppId>::IMPLEMENTED,
            "AppId must not implement ToString either - it can be written \
             directly on a type that has no Display"
        );
        assert!(
            !probe_deref::Probe::<AppId>::IMPLEMENTED,
            "AppId must not Deref: it is what would make str::as_bytes reachable \
             by autoderef"
        );
        assert!(
            !probe_as_ref_str::Probe::<AppId>::IMPLEMENTED,
            "AppId must not implement AsRef<str>: a tenant id would reach a \
             parameter that wanted a schema name"
        );
        assert!(
            !probe_borrow_str::Probe::<AppId>::IMPLEMENTED,
            "AppId must not implement Borrow<str>"
        );
        assert!(
            !probe_from_str_ref::Probe::<AppId>::IMPLEMENTED,
            "AppId must not implement From<&str>: construction is a mint or a \
             fallible parse, and there is no third way in"
        );
        assert!(
            !probe_from_string::Probe::<AppId>::IMPLEMENTED,
            "AppId must not implement From<String>"
        );
        assert!(
            !probe_partial_eq_str::Probe::<AppId>::IMPLEMENTED,
            "AppId must not implement PartialEq<str>: comparing against a raw \
             string is a decision, not a convenience"
        );

        // The control. Every probe above must be capable of answering `true`.
        assert!(probe_display::Probe::<String>::IMPLEMENTED);
        assert!(probe_to_string::Probe::<String>::IMPLEMENTED);
        assert!(probe_deref::Probe::<String>::IMPLEMENTED);
        assert!(probe_as_ref_str::Probe::<String>::IMPLEMENTED);
        assert!(probe_borrow_str::Probe::<String>::IMPLEMENTED);
        assert!(probe_from_str_ref::Probe::<String>::IMPLEMENTED);
        assert!(probe_from_string::Probe::<String>::IMPLEMENTED);
        assert!(probe_partial_eq_str::Probe::<String>::IMPLEMENTED);
    }

    /// Sentinel returned by the blanket fallback below. A receiver that has its
    /// OWN inherent `as_bytes` resolves to that instead, and the binding stops
    /// compiling.
    struct NoInherentAsBytes;

    trait AsBytesFallback {
        fn as_bytes(&self) -> NoInherentAsBytes {
            NoInherentAsBytes
        }
    }

    impl<T> AsBytesFallback for T {}

    /// The absence that protects every stored app secret and the whole worker
    /// isolate cache.
    ///
    /// PAIRED WITH A CONTROL, and the control is `uuid::Uuid` on purpose: its
    /// inherent `as_bytes` is exactly the one the app-secret AAD binds, and it
    /// wins over the blanket fallback at the same autoref step, so the second
    /// half proves the shadow is defeatable by a real inherent method rather
    /// than being unconditionally true.
    ///
    /// MUTATION-CHECKED: adding `fn as_bytes(&self) -> &[u8]` to the `impl
    /// AppId` block makes the first binding a type error, `expected
    /// NoInherentAsBytes, found &[u8]`.
    #[test]
    fn there_is_no_inherent_as_bytes() {
        let id = AppId::mint();
        let _: NoInherentAsBytes = id.as_bytes();

        // The control: an inherent as_bytes shadows the fallback.
        let control = uuid::Uuid::nil();
        let raw: &[u8; 16] = control.as_bytes();
        assert_eq!(raw, &[0u8; 16]);
    }

    #[test]
    fn mint_round_trips_through_the_shared_parser() {
        let minted = AppId::mint();
        assert!(minted.as_str().starts_with("app_"), "{}", minted.as_str());

        let reparsed = AppId::parse(minted.as_str()).expect("a minted id must parse");
        assert_eq!(reparsed, minted);
        assert_eq!(reparsed.uuid(), minted.uuid());

        // And the uuid this type carries is the one the shared parser extracts,
        // not a second decode that could disagree.
        let shared = typed_id::parse_with_prefix(minted.as_str(), APP_PREFIX)
            .expect("the shared parser must accept a minted id");
        assert_eq!(shared, minted.uuid());
    }

    /// `mint` inlines the format that [`typed_id::generate`] spells, so that it
    /// can keep the uuid without decoding it back. This is what stops the two
    /// spellings from drifting.
    #[test]
    fn mint_agrees_with_the_shared_generator() {
        let minted = AppId::mint();
        let generated = typed_id::generate(APP_PREFIX);

        assert_eq!(minted.as_str().len(), generated.len());

        let (minted_prefix, _) = typed_id::parse(minted.as_str()).expect("minted id parses");
        let (generated_prefix, _) = typed_id::parse(&generated).expect("generated id parses");
        assert_eq!(minted_prefix, generated_prefix);
        assert_eq!(minted_prefix, AppId::PREFIX);
    }

    /// The control for the refusal arms below: a real app id is accepted and
    /// preserved byte for byte, so those arms are measuring the predicate rather
    /// than a constructor that refuses everything.
    ///
    /// MUTATION-CHECKED for the pair as a whole: replacing the `parse_with_prefix`
    /// call in [`AppId::parse`] with a bare `typed_id::parse` - dropping the
    /// prefix boundary check - fails `parse_refuses_the_derived_oauth_client_id`
    /// and the serde refusal, and leaves this control green. It does NOT fail
    /// `parse_refuses_a_hyphenated_uuid`, because a hyphenated uuid carries no
    /// separator and is refused a step earlier; that arm binds a different thing.
    #[test]
    fn parse_accepts_a_real_app_id_and_preserves_it_byte_for_byte() {
        let raw = typed_id::generate(APP_PREFIX);
        let parsed = AppId::parse(&raw).expect("a real app id must parse");
        assert_eq!(parsed.as_str(), raw);
    }

    #[test]
    fn parse_refuses_a_hyphenated_uuid() {
        // The shape `zeroship.apps.id` holds today. It must not become an AppId
        // by accident during the column flip.
        let err = AppId::parse("0191e7a2-b3c4-4d5e-8f90-123456789abc")
            .expect_err("a hyphenated uuid is not an app id");
        assert!(matches!(err, ParseError::Malformed(_)), "{err:?}");
    }

    #[test]
    fn parse_refuses_the_derived_oauth_client_id() {
        // `oac_<base62-app-id>` carries the SAME hundred and twenty eight bits
        // in the same encoding. Only the prefix separates them, which is exactly
        // why the boundary check is a prefix check.
        let app = uuid::Uuid::now_v7();
        let client_id = typed_id::app_oauth_client_id(&app);
        let err = AppId::parse(&client_id).expect_err("an oauth client id is not an app id");
        match err {
            ParseError::WrongPrefix { expected, got } => {
                assert_eq!(expected, APP_PREFIX);
                assert_eq!(got, typed_id::APP_OAUTH_CLIENT_PREFIX);
            }
            ParseError::Malformed(msg) => panic!("should be a boundary rejection, got {msg}"),
        }
    }

    #[test]
    fn parse_refuses_a_body_that_is_not_twenty_two_base62_characters() {
        let raw = typed_id::generate(APP_PREFIX);
        let body = raw.split_once('_').expect("has a body").1;

        for bad in [
            format!("app_{}", &body[..body.len() - 1]),
            format!("app_{body}0"),
            "app_".to_owned(),
            // Twenty two legal characters, above the representable range.
            "app_ZZZZZZZZZZZZZZZZZZZZZZ".to_owned(),
            // Right length, one character outside the alphabet.
            format!("app_-{}", &body[1..]),
        ] {
            let err = AppId::parse(&bad).expect_err(&format!("{bad} must be refused"));
            assert!(
                matches!(err, ParseError::Malformed(_)),
                "{bad} is a malformed body, not a boundary crossing: {err:?}"
            );
        }
    }

    #[test]
    fn parse_refuses_a_missing_separator() {
        assert!(matches!(
            AppId::parse("app").expect_err("no separator"),
            ParseError::Malformed(_)
        ));
        assert!(matches!(
            AppId::parse("").expect_err("empty"),
            ParseError::Malformed(_)
        ));
    }

    #[test]
    fn serialize_is_the_bare_canonical_string() {
        let id = AppId::mint();
        let json = serde_json::to_string(&id).expect("serializes");
        assert_eq!(json, format!("\"{}\"", id.as_str()));
    }

    #[test]
    fn deserialize_goes_through_parse_and_refuses_every_other_shape() {
        let id = AppId::mint();
        let json = serde_json::to_string(&id).expect("serializes");
        let back: AppId = serde_json::from_str(&json).expect("round trips");
        assert_eq!(back, id);

        for bad in [
            "\"0191e7a2-b3c4-4d5e-8f90-123456789abc\"",
            "\"oac_0000000000000000000000\"",
            "\"app_000000000000000000000\"",
            "\"\"",
            "42",
            "null",
        ] {
            assert!(
                serde_json::from_str::<AppId>(bad).is_err(),
                "{bad} must not decode as an app id"
            );
        }
    }

    /// The gateway's route and version maps key on the app id, so the decode
    /// has to work in serde's map-key position, where a deserializer is handed
    /// only a string. This is also the arm that would have caught a producer and
    /// consumer disagreeing about the rendering.
    #[test]
    fn works_as_a_serde_map_key_and_refuses_a_uuid_shaped_one() {
        let mut map = BTreeMap::new();
        let id = AppId::mint();
        map.insert(id.clone(), 7u8);

        let json = serde_json::to_string(&map).expect("serializes as a map key");
        assert_eq!(json, format!("{{\"{}\":7}}", id.as_str()));

        let back: BTreeMap<AppId, u8> = serde_json::from_str(&json).expect("round trips");
        assert_eq!(back, map);

        let uuid_keyed = "{\"0191e7a2-b3c4-4d5e-8f90-123456789abc\":7}";
        assert!(
            serde_json::from_str::<BTreeMap<AppId, u8>>(uuid_keyed).is_err(),
            "a uuid-shaped key must be a decode failure, not a dropped entry"
        );
    }

    /// The canonical conversion is total and lossless over the whole
    /// hundred-and-twenty-eight-bit space, which is what lets its `expect` be
    /// unreachable and what lets the worker hand the uuid back to the control
    /// plane after the gateway sent it the printed form.
    ///
    /// The two ends of the range are here on purpose: `nil` is the value that
    /// would expose a missing zero-pad (base62 of zero is one character, not
    /// twenty-two) and `max` the one that would expose an overflow in the
    /// divide loop. Either would be a `Malformed` panic, not a wrong answer.
    #[test]
    fn the_canonical_conversion_round_trips_the_extreme_uuids() {
        let mut cases = vec![uuid::Uuid::nil(), uuid::Uuid::max()];
        cases.extend((0..64).map(|_| uuid::Uuid::new_v4()));
        cases.push(typed_id::new_v7());

        for raw in cases {
            let id = canonical_app_id_for(&raw);
            assert_eq!(id.uuid(), raw, "the embedded bits must survive");
            assert!(id.as_str().starts_with("app_"), "{}", id.as_str());
            assert_eq!(id.as_str().len(), "app_".len() + 22);
            assert_eq!(
                AppId::parse(id.as_str()).expect("a canonical id parses"),
                id,
                "the canonical rendering must be one parse already admits"
            );
        }
    }

    /// The two transitional conversions produce DIFFERENT ids from the same
    /// uuid, and nothing may quietly paper over that.
    ///
    /// This is the property the gateway's route map depends on: it is keyed on
    /// the canonical rendering, so probing it with the uuid rendering has to
    /// miss. If these two ever compared equal, a producer and a consumer that
    /// disagreed about the rendering would silently agree instead, which is the
    /// failure mode the whole typed id exists to remove.
    #[test]
    fn the_two_transitional_renderings_of_one_uuid_are_not_the_same_id() {
        let raw = uuid::Uuid::new_v4();
        let derivation_input = AppId::from_uuid(&raw);
        let wire_identity = canonical_app_id_for(&raw);

        assert_eq!(derivation_input.uuid(), wire_identity.uuid());
        assert_ne!(derivation_input, wire_identity);
        assert_ne!(derivation_input.as_str(), wire_identity.as_str());

        let mut map = BTreeMap::new();
        map.insert(wire_identity.clone(), 7u8);
        assert_eq!(map.get(&wire_identity), Some(&7));
        assert_eq!(
            map.get(&derivation_input),
            None,
            "a rendering disagreement must be a miss, not a hit on a neighbour"
        );
    }

    /// `Ord` compares the printed id first, so it is the base62 byte order, so
    /// it is creation order. The database column that will hold this value does
    /// NOT inherit that ordering unless its collation is pinned - see the module
    /// docs.
    #[test]
    fn ordering_is_creation_order_under_byte_comparison() {
        let first = AppId::mint();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = AppId::mint();
        assert!(
            second > first,
            "{} should sort after {}",
            second.as_str(),
            first.as_str()
        );
        assert!(second.as_str() > first.as_str(), "and by the printed id too");
    }
}
