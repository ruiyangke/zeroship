//! The numeric vocabulary. "The primitive layout is closed."
//!
//! Two families, and the difference between them is enforced by the type rather
//! than by a comment at the call site.
//!
//! **Bounded generations** - [`LeaderTerm`], [`DatabaseEpoch`],
//! [`GrantGeneration`]. "They encode as `u64` but their constructors admit only
//! `1..=i64::MAX`, matching the `bigint` columns that persist them; a decoder
//! refuses an out-of-domain value before comparing or storing it." The order in
//! that sentence is the requirement: refusing AFTER a comparison means a peer can
//! win a fence check with a value the database could never have produced.
//!
//! **Plain counters** - LSNs, sequences, batch ids, relation generations. Every
//! `u64` value is legal; zero included. `Lsn(0)` is `0/0`, which is what a
//! `START_REPLICATION` asking for the slot's own `confirmed_flush_lsn` sends.
//!
//! Nothing here is a bare `u64` in a frame. A `Change` carries a commit LSN, a
//! change index and a relation generation adjacently; transposing two of them is
//! a compile error only if they are different types.

use crate::codec::{Reader, Writer};
use crate::error::DecodeError;
use crate::limits::{GENERATION_MAX, GENERATION_MIN};

/// A `u64` newtype with the full domain.
macro_rules! u64_scalar {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            /// Wrap a value.
            #[must_use]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            /// The wrapped value.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }

            /// Decode a big-endian `u64`.
            ///
            /// # Errors
            /// [`DecodeError::Truncated`] when fewer than eight bytes remain.
            pub fn decode(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
                Ok(Self(reader.u64()?))
            }

            /// Encode as a big-endian `u64`.
            pub fn encode(self, writer: &mut Writer) {
                writer.u64(self.0);
            }
        }
    };
}

/// A `u32` newtype with the full domain.
macro_rules! u32_scalar {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u32);

        impl $name {
            /// Wrap a value.
            #[must_use]
            pub const fn new(value: u32) -> Self {
                Self(value)
            }

            /// The wrapped value.
            #[must_use]
            pub const fn get(self) -> u32 {
                self.0
            }

            /// Decode a big-endian `u32`.
            ///
            /// # Errors
            /// [`DecodeError::Truncated`] when fewer than four bytes remain.
            pub fn decode(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
                Ok(Self(reader.u32()?))
            }

            /// Encode as a big-endian `u32`.
            pub fn encode(self, writer: &mut Writer) {
                writer.u32(self.0);
            }
        }
    };
}

/// A `u64` newtype whose constructor admits only `1..=i64::MAX`.
macro_rules! bounded_generation {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            /// Wrap a value in `1..=i64::MAX`.
            ///
            /// # Errors
            /// [`DecodeError::OutOfDomain`] outside that range. Fallible on
            /// purpose: an infallible constructor is one a decoder can bypass.
            pub const fn new(value: u64) -> Result<Self, DecodeError> {
                if value < GENERATION_MIN || value > GENERATION_MAX {
                    return Err(DecodeError::OutOfDomain {
                        kind: stringify!($name),
                    });
                }
                Ok(Self(value))
            }

            /// The wrapped value.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }

            /// Decode a big-endian `u64` and enforce the domain BEFORE the value
            /// is returned, so it can never be compared or stored first.
            ///
            /// # Errors
            /// [`DecodeError::Truncated`] or [`DecodeError::OutOfDomain`].
            pub fn decode(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
                Self::new(reader.u64()?)
            }

            /// Encode as a big-endian `u64`.
            pub fn encode(self, writer: &mut Writer) {
                writer.u64(self.0);
            }
        }
    };
}

bounded_generation! {
    /// The leader term.
    ///
    /// Minted by an atomic increment in the coordination database under
    /// `predecessor_fenced_term = leader_term`. Monotonic, and "term exhaustion
    /// is a hard operator refusal, never a wrapping increment" - which is why the
    /// domain stops at `i64::MAX` rather than `u64::MAX`.
    LeaderTerm
}

bounded_generation! {
    /// A Database's schema epoch.
    ///
    /// Advances in the widen transaction, alongside the
    /// `pg_logical_emit_message` marker, so "the publication reached its new
    /// shape" and "the database epoch advanced" are one commit.
    DatabaseEpoch
}

bounded_generation! {
    /// The generation of the Grant binding an app to a Database.
    ///
    /// Part of every sequenced frame's envelope: a frame lives "in the one ring
    /// for that exact binding", so a rebind cannot deliver into the old ring.
    GrantGeneration
}

u64_scalar! {
    /// A PostgreSQL LSN as its raw `XLogPtr` integer.
    ///
    /// "An LSN is its raw `XLogPtr` integer, not `0/16B6C50` text." The text form
    /// is a display convention with two variable-width halves; comparing two of
    /// them lexicographically is wrong in a way that looks right in a log.
    Lsn
}

u64_scalar! {
    /// Per-app ring sequence. The cursor rule is `[tail_seq, ring_next_seq)`:
    /// above is `CursorAhead`, below is `RingOverrun`, in range resumes.
    Sequence
}

u64_scalar! {
    /// Groups the `RelationPrime` frames of one priming batch.
    ///
    /// Sent before one `Registered` or `Resync`. "Connection-monotonic
    /// never-reused", so a worker can require exactly `prime_relation_count`
    /// distinct generations under one batch and know a partial batch when it
    /// sees one.
    PrimeBatchId
}

u64_scalar! {
    /// Identifies one shape of one relation.
    ///
    /// "Monotonic `u64` never reused within
    /// `(relay_id, leader_term, app_id, grant_generation)`, including across a
    /// same-term Datastore reconnect or slot recreation. Losing that counter is
    /// loss of sequence state and forces self-demotion."
    RelationGeneration
}

u64_scalar! {
    /// Shared by every shard of one worker registration snapshot. "A generation
    /// below the highest admitted is always `NonMonotonicRegistrationGeneration`
    /// and disturbs no response."
    RegistrationGeneration
}

u64_scalar! {
    /// `IDENTIFY_SYSTEM`'s `systemid`.
    ///
    /// A change means the LSN space changed, so the numeric dedup watermark is
    /// invalidated even when the term did not move - and, before that, it is
    /// treated as a misrouted tenant DSN rather than a restore.
    SystemId
}

u32_scalar! {
    /// The 0-based pgoutput DML ordinal within one transaction.
    ///
    /// "Incremented
    /// exactly once per decoded Insert/Update/Delete BEFORE projection, Grant
    /// lookup or fan-out, and reset at every `Begin`". With the commit LSN it is
    /// the whole dedup key; a per-change LSN is not, because those go backwards
    /// across overlapping transactions and collapse under `heap_multi_insert`.
    ChangeIndex
}

u32_scalar! {
    /// `IDENTIFY_SYSTEM`'s timeline. Promotion increases it; crash recovery does
    /// not, which is why an unchanged pair is not evidence of an unrewound WAL
    /// and every same-term reconnect resets anyway.
    Timeline
}
