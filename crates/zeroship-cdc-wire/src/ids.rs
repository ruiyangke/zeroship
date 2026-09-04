//! The typed ids this wire carries.
//!
//! "A typed id is its canonical ASCII rendering, bounded to 64 bytes and parsed
//! by the expected concrete id type; a wrong prefix or noncanonical base62 form
//! is fatal."
//!
//! Two properties do the work. **Typed**, so a `DatabaseId` cannot be passed
//! where a `DatastoreId` belongs - the frames here carry up to five ids and the
//! compiler is the only thing that will notice a transposition. **Canonical**, so
//! one entity has exactly one encoding: an id that round-trips to different bytes
//! is an id two caches disagree about.
//!
//! # Canonicality, precisely
//!
//! The platform form is `{prefix}_{base62(uuidv7)}`: 22 base62 characters over
//! `0-9A-Za-z`, fixed width with leading zeros. 62^22 is slightly greater than
//! 2^128, so 22 characters can spell a value no UUID can hold - and that is the
//! whole of the noncanonical case. Rejecting it is one `checked_mul`/`checked_add`
//! accumulation, and without it `id_a != id_b` while both name the same entity.
//!
//! # Why the base62 decode is duplicated here
//!
//! `zeroship_core::typed_id` owns the platform's implementation and this crate
//! deliberately does not depend on it - see `Cargo.toml` for the reason (that
//! crate's normal dependencies include an HTTP client, and this crate's stated
//! property is that it performs no I/O). The duplication is bounded by
//! `tests/typed_id_oracle.rs`, which runs both parsers over one corpus - ids
//! minted by the platform encoder itself, wrong prefixes, wrong lengths,
//! out-of-alphabet bytes, and both sides of the 2^128 boundary (the encoding of
//! an all-ones UUID, which must be accepted, against `ZZZZZZZZZZZZZZZZZZZZZZ`,
//! which is 62^22 - 1 and must not) - and requires them to agree on every input.
//! A drift is a test failure, not a production surprise.

use core::fmt;

use crate::codec::{Reader, Writer};
use crate::error::{DecodeError, EncodeError};
use crate::limits::MAX_TYPED_ID_BYTES;

/// `0-9A-Za-z`, byte-ordered so lexicographic order matches numeric order.
/// Identical to `zeroship_core::typed_id::BASE62`; the oracle test is what keeps
/// it that way.
const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Fixed width of the base62 body of every typed id.
const BASE62_WIDTH: usize = 22;

// `slice::get` is not usable in a const context, so this one function indexes.
// Both indices are provably in range at compile time: `i` is bounded by the
// literal 62, and every byte of `BASE62` is ASCII, hence below the table's 128.
// The alternative is a lazily-built table, which would move a compile-time fact
// into a runtime one.
#[allow(clippy::indexing_slicing)]
const fn build_decode_table() -> [u8; 128] {
    let mut table = [255u8; 128];
    let mut i: u8 = 0;
    while i < 62 {
        table[BASE62[i as usize] as usize] = i;
        i += 1;
    }
    table
}

const DECODE: [u8; 128] = build_decode_table();

/// Accept exactly `{prefix}_{22 canonical base62 chars}`.
fn validate(text: &str, prefix: &'static str) -> Result<(), DecodeError> {
    let malformed = DecodeError::MalformedTypedId {
        expected_prefix: prefix,
    };
    if text.len() > MAX_TYPED_ID_BYTES {
        return Err(malformed);
    }
    let body = text
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('_'))
        .ok_or(malformed)?;
    if body.len() != BASE62_WIDTH {
        return Err(malformed);
    }
    // Accumulate as a 128-bit integer. Overflow is the noncanonical case: the
    // encoder only ever emits values below 2^128, so a string that decodes above
    // it was not produced by one.
    let mut value: u128 = 0;
    for byte in body.bytes() {
        let digit = DECODE
            .get(byte as usize)
            .copied()
            .filter(|d| *d != 255)
            .ok_or(malformed)?;
        value = value
            .checked_mul(62)
            .and_then(|v| v.checked_add(u128::from(digit)))
            .ok_or(malformed)?;
    }
    Ok(())
}

macro_rules! typed_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// The one prefix this id type accepts. Adopting these types in the
            /// entity work means re-exporting them; changing a spelling is this
            /// one line and a wire break.
            pub const PREFIX: &'static str = $prefix;

            /// Parse a canonical rendering.
            ///
            /// # Errors
            /// [`DecodeError::MalformedTypedId`] for a wrong prefix, a wrong
            /// length, a character outside base62, or a noncanonical value.
            pub fn parse(text: &str) -> Result<Self, DecodeError> {
                validate(text, Self::PREFIX)?;
                Ok(Self(text.to_owned()))
            }

            /// The canonical rendering.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Decode one length-prefixed id.
            ///
            /// # Errors
            /// [`DecodeError::TypedIdTooLong`] when the declared length exceeds
            /// the 64-byte bound - checked BEFORE the bytes are taken, so an
            /// oversize claim never reaches the parser - then whatever
            /// [`Self::parse`] returns.
            pub fn decode(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
                let declared = reader.u32()?;
                let len = usize::try_from(declared).map_err(|_| DecodeError::LengthOverflow)?;
                if len > MAX_TYPED_ID_BYTES {
                    return Err(DecodeError::TypedIdTooLong { declared });
                }
                let raw = reader.take(len)?;
                let text = core::str::from_utf8(raw).map_err(|_| DecodeError::InvalidUtf8)?;
                Self::parse(text)
            }

            /// Encode as a length-prefixed string.
            ///
            /// # Errors
            /// [`EncodeError::ValueTooLarge`] is unreachable for a parsed id and
            /// is returned rather than asserted.
            pub fn encode(&self, writer: &mut Writer) -> Result<(), EncodeError> {
                writer.string(&self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl core::str::FromStr for $name {
            type Err = DecodeError;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Self::parse(text)
            }
        }
    };
}

typed_id! {
    /// One tenant. `app_` is `zeroship_core::typed_id::APP_PREFIX`; the wire and
    /// the platform must not disagree about what an app id looks like.
    AppId, "app"
}

typed_id! {
    /// One physical PostgreSQL database. Names the shared publication and the
    /// exact slot: `datastore_publication_name(id)` and
    /// `datastore_slot_name(id)` are keyed by this, never by `app_id`.
    ///
    /// **The prefix is the one open spelling on this wire.**
    /// `docs/proposals/2026-08-28-app-database-decoupling.md:25` writes
    /// `ds_<base62 uuidv7>`, and its own line 45 says "the prefix is `dbs` for
    /// uniformity with `crates/zeroship-core/src/typed_id.rs`, whose every prefix
    /// is three lowercase letters". `ds` is two. The literal table wins here
    /// because it is the entity authority's written spelling and because the same
    /// paragraph records that neither `parse` nor `parse_with_prefix` enforces
    /// length - but the contradiction is real and belongs to that proposal, not
    /// to this crate. If it resolves to a three-letter prefix, this literal is
    /// where it changes.
    DatastoreId, "ds"
}

typed_id! {
    /// One creator-owned schema inside a Datastore: "the unit that is owned,
    /// migrated, granted, published and dropped".
    DatabaseId, "dbs"
}

typed_id! {
    /// One physical PostgreSQL cluster, and the leader-election scope.
    ///
    /// **Neither proposal pins this form.** `cdc_cluster.cluster_id` is a primary
    /// key of unstated type and the topology long-poll puts it in a URL path.
    /// This crate makes it a typed id rather than a free-form name for two
    /// reasons: the wire section names exactly one identifier form ("a typed id
    /// is its canonical ASCII rendering..."), so a second form would be an
    /// invention either way; and a canonical base62 id cannot contain `/`, `.`
    /// or `%`, which is the path-traversal rule
    /// `zeroship_core::typed_id::parse_with_prefix` exists to enforce. `clu` is
    /// chosen here, and an operator-assigned name would need this decision
    /// reopened rather than a cast added.
    ClusterId, "clu"
}

typed_id! {
    /// One worker PROCESS.
    ///
    /// "`worker_id` and `relay_id` are typed `wrk_`/`rly_`
    /// UUIDv7 process identities minted at start; neither survives a restart."
    /// That is what makes a cursor from a dead process detectable rather than
    /// merely stale.
    WorkerId, "wrk"
}

typed_id! {
    /// One relay PROCESS. Paired with [`crate::LeaderTerm`] it is the fencing
    /// identity: "a different `relay_id` or `leader_term` resets as
    /// `RelayFailover`", and both halves are compared, never one.
    RelayId, "rly"
}
