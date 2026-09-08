//! [`declare_entity_id`] - the shared shape of a typed entity id, and the
//! compile-time proof that it kept its absences.
//!
//! # Why a macro, when [`crate::app_id::AppId`] is hand-written
//!
//! `AppId` is not generated from this macro and must not be. It carries a
//! second field and a [`crate::app_id::AppId::uuid`] accessor because two call
//! sites - the app-secret AAD and the CHWBL ring position - derive from the
//! embedded bits rather than the printed id, and its module doc justifies each
//! absence against a specific derivation an app id seeds (a schema name, a role
//! name, a replication slot, an RLS GUC). None of that is true of the ids
//! declared here: they are keys and nothing else, so they hold the text alone
//! and expose no route to the bits.
//!
//! What IS shared is the discipline, and it is the reason this is a macro
//! rather than three copies: the absence tests are long, they need a paired
//! control to be meaningful, and a copy that silently lost one assertion would
//! print exactly what a complete one prints. Generating them means a new entity
//! id cannot be declared without them.
//!
//! # The absences, and the failure each prevents
//!
//! Following `zeroship_data_query_builder::schema_name::SchemaName` and
//! [`crate::app_id::AppId`], a typed entity id is characterised by what it
//! refuses:
//!
//! - **No `Display`, no `ToString`.** An id reaching a format string is a
//!   decision. Rendering goes through `as_str`, which is greppable.
//! - **No `Deref<Target = str>`, `AsRef<str>` or `Borrow<str>`.** These are the
//!   routes by which one entity's id reaches a parameter that wanted another's.
//!   Every id declared here is a bare `text` column in PostgreSQL, so a
//!   `&str`-typed parameter would accept any of them interchangeably.
//! - **No `From<&str>`, `From<String>`, `new_unchecked` or public field.** In is
//!   `mint` or the fallible `parse`, and there is no third way.
//! - **No `PartialEq<str>`.** Comparing against a raw string is a decision.
//!
//! # Ordering
//!
//! `Ord` is byte order over the printed id, which is base62 over a `UUIDv7`
//! body, which is creation order - but ONLY when the storing column is pinned to
//! bytewise collation. `db/migrations-ts/20260831000001_sortable_entity_id_collations.ts`
//! carries that map and its header explains why a locale collation breaks it.
//! **Any column holding one of these ids must be registered there, together with
//! every foreign-key and denormalized copy of it**, or a join against the
//! collated column silently stops using the copy's index. That failure does not
//! error; it degrades.

/// Declare a typed entity id: a newtype over the printed `<prefix>_<base62>`
/// form, constructible only by minting or parsing, with its absences asserted.
///
/// Generates the type, `PREFIX`, `mint`, `parse`, `as_str`, `Serialize`,
/// `Deserialize`, and a test module proving the absences against a control.
macro_rules! declare_entity_id {
    (
        $(#[$type_meta:meta])*
        $name:ident, $prefix:path, $tests:ident $(,)?
    ) => {
        $(#[$type_meta])*
        ///
        /// Constructed only through `mint` or `parse`. See
        /// [`crate::entity_id`] for the absences this type is required to keep
        /// and why an id column holding it must be registered in the collation
        /// migration.
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name {
            text: String,
        }

        impl $name {
            /// The one prefix this type accepts, re-exported from the typed-id
            /// registry so a caller matching on it cannot pick up a second
            /// spelling.
            pub const PREFIX: &'static str = $prefix;

            /// Mint a fresh id. This is the ONLY minter.
            ///
            /// Delegates to [`crate::typed_id::generate`] rather than composing
            /// the encoder itself: unlike [`crate::app_id::AppId::mint`] this
            /// type keeps no uuid field, so there is nothing to save by
            /// inlining and a second composition would be a second thing to
            /// drift.
            #[must_use]
            pub fn mint() -> Self {
                Self {
                    text: $crate::typed_id::generate($prefix),
                }
            }

            /// Parse a canonical id.
            ///
            /// Delegates to [`crate::typed_id::parse_with_prefix`], so it
            /// refuses exactly what that refuses: a wrong prefix is a
            /// [`crate::typed_id::ParseError::WrongPrefix`], and a wrong
            /// length, a character outside base62, a missing separator or a
            /// body above the hundred-and-twenty-eight-bit range is a
            /// [`crate::typed_id::ParseError::Malformed`].
            ///
            /// The decoded uuid is discarded deliberately: it is used to
            /// VALIDATE the body and this type exposes no route to the bits.
            ///
            /// # Errors
            ///
            /// The shared parser's, introducing no new vocabulary.
            pub fn parse(raw: &str) -> Result<Self, $crate::typed_id::ParseError> {
                let _validated = $crate::typed_id::parse_with_prefix(raw, $prefix)?;
                Ok(Self {
                    text: raw.to_owned(),
                })
            }

            /// The printed id.
            ///
            /// Named rather than an `AsRef`/`Deref` impl so that every place
            /// this id degrades back into an untyped string is greppable.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.text
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(
                &self,
                serializer: S,
            ) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.text)
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            /// Decoding goes through `parse`, so a uuid-shaped or
            /// wrong-prefixed value on the wire is a DECODE FAILURE rather than
            /// a silently accepted key.
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> Result<Self, D::Error> {
                struct IdVisitor;

                impl ::serde::de::Visitor<'_> for IdVisitor {
                    type Value = $name;

                    fn expecting(
                        &self,
                        f: &mut ::core::fmt::Formatter<'_>,
                    ) -> ::core::fmt::Result {
                        write!(f, "a typed id of the form {}_<base62>", $prefix)
                    }

                    fn visit_str<E: ::serde::de::Error>(
                        self,
                        raw: &str,
                    ) -> Result<$name, E> {
                        $name::parse(raw).map_err(::serde::de::Error::custom)
                    }
                }

                deserializer.deserialize_str(IdVisitor)
            }
        }

        #[cfg(test)]
        mod $tests {
            use super::$name;

            /// Compile-time probe for whether a type implements a trait, without
            /// specialization: an inherent associated const shadows the trait's
            /// default when - and only when - its bound is satisfied.
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

            /// The absences [`crate::entity_id`] names, asserted mechanically.
            ///
            /// PAIRED WITH A CONTROL. `String` implements every one of these
            /// probes, so the second half proves the probe machinery can report
            /// `true`; without it a probe broken into always answering `false`
            /// would print exactly what a correct one prints.
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
                    !probe_display::Probe::<$name>::IMPLEMENTED,
                    "must not implement Display: rendering goes through as_str"
                );
                assert!(
                    !probe_to_string::Probe::<$name>::IMPLEMENTED,
                    "must not implement ToString either - it can be written \
                     directly on a type that has no Display"
                );
                assert!(
                    !probe_deref::Probe::<$name>::IMPLEMENTED,
                    "must not Deref: it is what would make str's inherent \
                     methods reachable by autoderef"
                );
                assert!(
                    !probe_as_ref_str::Probe::<$name>::IMPLEMENTED,
                    "must not implement AsRef<str>: one entity's id would reach \
                     a parameter that wanted another's"
                );
                assert!(
                    !probe_borrow_str::Probe::<$name>::IMPLEMENTED,
                    "must not implement Borrow<str>"
                );
                assert!(
                    !probe_from_str_ref::Probe::<$name>::IMPLEMENTED,
                    "construction is mint or parse; there is no third way in"
                );
                assert!(
                    !probe_from_string::Probe::<$name>::IMPLEMENTED,
                    "construction is mint or parse; there is no third way in"
                );
                assert!(
                    !probe_partial_eq_str::Probe::<$name>::IMPLEMENTED,
                    "comparing against a raw string is a decision, not a \
                     convenience"
                );

                // The control. If these fail, the probes above measure nothing.
                assert!(probe_display::Probe::<String>::IMPLEMENTED);
                assert!(probe_to_string::Probe::<String>::IMPLEMENTED);
                assert!(probe_deref::Probe::<String>::IMPLEMENTED);
                assert!(probe_as_ref_str::Probe::<String>::IMPLEMENTED);
                assert!(probe_borrow_str::Probe::<String>::IMPLEMENTED);
                assert!(probe_from_str_ref::Probe::<String>::IMPLEMENTED);
                assert!(probe_from_string::Probe::<String>::IMPLEMENTED);
                // This control was MISSING until 2026-09-08: eight absences were
                // asserted and seven controlled, so `partial_eq_str` was the one
                // probe that could have broken into answering `false`
                // unconditionally and still printed exactly what a correct run
                // prints. The imbalance is invisible by reading - both lists are
                // long and neither is ordered - which is why the fix is a control
                // rather than a comment.
                assert!(probe_partial_eq_str::Probe::<String>::IMPLEMENTED);
            }

            /// Sentinel returned by the blanket fallback below. A receiver that
            /// has its OWN inherent `as_bytes` resolves to that instead, and the
            /// binding stops compiling.
            struct NoInherentAsBytes;

            trait AsBytesFallback {
                fn as_bytes(&self) -> NoInherentAsBytes {
                    NoInherentAsBytes
                }
            }

            impl<T> AsBytesFallback for T {}

            /// No route to the bits, asserted mechanically rather than promised.
            ///
            /// The macro's `parse` discards the decoded uuid deliberately, and
            /// this is what holds that open: an inherent `as_bytes` would be a
            /// route to the 128 bits, and a value derived from those bits does
            /// not move when the printed form does - which is what makes such a
            /// derivation fail quietly rather than loudly.
            ///
            /// PAIRED WITH A CONTROL, and the control is `uuid::Uuid` on
            /// purpose: its inherent `as_bytes` wins over the blanket fallback
            /// at the same autoref step, so the second half proves the shadow is
            /// defeatable by a real inherent method rather than unconditionally
            /// true.
            ///
            /// Lived only in `crate::app_id`'s own tests until 2026-09-08, so
            /// the three macro-declared ids asserted nothing of the kind. It
            /// moved here rather than being copied, because `AppId` adopting the
            /// macro would otherwise delete it with no compile error, no gate
            /// failure and no diff line saying a test went.
            #[test]
            fn there_is_no_inherent_as_bytes() {
                let id = $name::mint();
                let _: NoInherentAsBytes = id.as_bytes();

                let control = ::uuid::Uuid::nil();
                let raw: &[u8; 16] = control.as_bytes();
                assert_eq!(raw, &[0u8; 16]);
            }

            /// The id survives a serde MAP KEY position, and a uuid-shaped key
            /// is a decode failure rather than a dropped entry.
            ///
            /// A map key is the position where a wrong shape is quietest: serde
            /// reports a bad key as an error only if the key type refuses it, and
            /// a `String` key would accept anything. This is the only map-key
            /// position test in the tree.
            #[test]
            fn works_as_a_serde_map_key_and_refuses_a_uuid_shaped_one() {
                let mut map = ::std::collections::BTreeMap::new();
                let id = $name::mint();
                map.insert(id.clone(), 7u8);

                let json = ::serde_json::to_string(&map).expect("serializes as a map key");
                assert_eq!(json, format!("{{\"{}\":7}}", id.as_str()));

                let back: ::std::collections::BTreeMap<$name, u8> =
                    ::serde_json::from_str(&json).expect("round trips");
                assert_eq!(back, map);

                let uuid_keyed = "{\"0191e7a2-b3c4-4d5e-8f90-123456789abc\":7}";
                assert!(
                    ::serde_json::from_str::<::std::collections::BTreeMap<$name, u8>>(uuid_keyed)
                        .is_err(),
                    "a uuid-shaped key must be a decode failure, not a dropped entry"
                );
            }

            #[test]
            fn mint_round_trips_through_parse() {
                let minted = $name::mint();
                let reparsed = $name::parse(minted.as_str()).expect("minted id must parse");
                assert_eq!(minted, reparsed);
            }

            #[test]
            fn mint_is_prefixed_and_unique() {
                let a = $name::mint();
                let b = $name::mint();
                assert_ne!(a, b, "each mint must yield a distinct id");
                for id in [&a, &b] {
                    assert!(
                        id.as_str().starts_with(&format!("{}_", $name::PREFIX)),
                        "minted id must carry its prefix: {}",
                        id.as_str()
                    );
                }
            }

            /// The boundary rejection that matters most: another entity's id is
            /// the wrong TYPE, and the parser must say so rather than accept a
            /// well-formed base62 body under a foreign tag.
            #[test]
            fn parse_refuses_a_foreign_prefix_and_a_malformed_body() {
                let foreign = $name::mint();
                let foreign = foreign.as_str().replacen($name::PREFIX, "zzz", 1);
                assert!(
                    $name::parse(&foreign).is_err(),
                    "a foreign prefix must be refused: {foreign}"
                );

                assert!($name::parse("").is_err(), "empty must be refused");
                assert!(
                    $name::parse($name::PREFIX).is_err(),
                    "a bare prefix with no separator or body must be refused"
                );
                assert!(
                    $name::parse(&format!("{}_", $name::PREFIX)).is_err(),
                    "an empty body must be refused"
                );
                assert!(
                    $name::parse(&format!("{}_!!!", $name::PREFIX)).is_err(),
                    "a body outside base62 must be refused"
                );
            }

            /// Ordering is byte order over the printed id, which is creation
            /// order for a UUIDv7 body. This is the in-process half of the
            /// contract; the storing column must be pinned to bytewise
            /// collation for the database half to agree.
            #[test]
            fn ordering_is_creation_order() {
                let mut minted: Vec<$name> = (0..16).map(|_| $name::mint()).collect();
                let as_minted = minted.clone();
                minted.sort();
                assert_eq!(
                    minted, as_minted,
                    "sorting minted ids must preserve mint order; if this fails \
                     the encoder is no longer order-preserving and every column \
                     holding this id has lost its ordering contract"
                );
            }

            #[test]
            fn serde_round_trips_and_refuses_a_foreign_id_on_the_wire() {
                let id = $name::mint();
                let json = serde_json::to_string(&id).expect("serialize");
                assert_eq!(json, format!("\"{}\"", id.as_str()));
                let back: $name = serde_json::from_str(&json).expect("deserialize");
                assert_eq!(id, back);

                assert!(
                    serde_json::from_str::<$name>("\"zzz_0000000000000000000000\"").is_err(),
                    "a foreign prefix on the wire must be a decode failure, not \
                     a silently accepted key"
                );
                assert!(
                    serde_json::from_str::<$name>(
                        "\"550e8400-e29b-41d4-a716-446655440000\""
                    )
                    .is_err(),
                    "a bare uuid on the wire must be a decode failure"
                );
            }
        }
    };
}

pub(crate) use declare_entity_id;
