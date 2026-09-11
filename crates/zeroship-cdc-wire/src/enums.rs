//! The closed enums.
//!
//! "Nested enums are fixed too, and any other discriminant is a fatal v1 decode
//! error rather than an `Unknown` variant." An `Unknown` variant is how a
//! consumer ends up with a `ResetReason` it cannot classify and defaults to the
//! weaker of two watermark rules; there is no such variant here, and there is no
//! `#[non_exhaustive]`, so adding a case is a compile error in every consumer
//! that matches on one.
//!
//! **Discriminants start at `0x01` for every enum in this crate.** The proposal
//! fixes the frame tags that way and does not enumerate the nested ones; this
//! crate fixes them, uniformly, so `0x00` is never a valid discriminant anywhere
//! in the protocol and a zero-filled or truncated-then-padded buffer cannot
//! decode into something meaningful.

use crate::codec::{Reader, Writer};
use crate::error::{DecodeError, EncodeError};
use crate::limits::MAX_CELL_BYTES;

macro_rules! wire_enum {
    (
        $(#[$meta:meta])*
        $name:ident {
            $($(#[$vmeta:meta])* $variant:ident = $value:literal),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            /// Every variant, for table-driven tests and for the round-trip
            /// corpus. A test that enumerates by hand is a test that misses the
            /// variant somebody just added.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The fixed wire discriminant.
            #[must_use]
            pub const fn discriminant(self) -> u8 {
                match self {
                    $(Self::$variant => $value),+
                }
            }

            /// Map a discriminant back.
            ///
            /// # Errors
            /// [`DecodeError::InvalidDiscriminant`] for anything outside the set.
            pub const fn from_discriminant(value: u8) -> Result<Self, DecodeError> {
                match value {
                    $($value => Ok(Self::$variant),)+
                    _ => Err(DecodeError::InvalidDiscriminant {
                        kind: stringify!($name),
                        value,
                    }),
                }
            }

            /// Decode one discriminant byte.
            ///
            /// # Errors
            /// [`DecodeError::Truncated`] or [`DecodeError::InvalidDiscriminant`].
            pub fn decode(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
                Self::from_discriminant(reader.u8()?)
            }

            /// Encode one discriminant byte.
            pub fn encode(self, writer: &mut Writer) {
                writer.u8(self.discriminant());
            }
        }
    };
}

wire_enum! {
    /// The kind of row change a [`crate::Change`] carries.
    ///
    /// One-to-one with `zeroship_data_orm::cdc::ChangeOp`, which is the
    /// in-process broker's vocabulary, and pinned to it by
    /// `tests/typed_id_oracle.rs`. They are separate types because the wire may
    /// not depend on that crate and because a wire discriminant is a contract
    /// with a peer while an in-process enum is not.
    ChangeOp {
        /// A new row.
        Insert = 0x01,
        /// An existing row changed. An identity-changing update carries the
        /// PRE-change identity in `pk` and the new one positionally in `values`.
        Update = 0x02,
        /// A row was removed. `pk` is the deleted identity; there is no old
        /// tuple, here or in `ChangeEvent`.
        Delete = 0x03,
    }
}

wire_enum! {
    /// Why a row was dropped instead of delivered.
    ///
    /// Closed rather than a string, deliberately: "an open reason field is where
    /// a physical column name reappears the moment someone writes a helpful
    /// error message."
    GapReason {
        /// A cell exceeded [`crate::MAX_CELL_BYTES`], or the encoded frame would
        /// have exceeded [`crate::MAX_FRAME_BYTES`].
        OversizeValue = 0x01,
        /// The decoded PostgreSQL type has no wire representation.
        UnrepresentableType = 0x02,
    }
}

wire_enum! {
    /// Why a subscription was reset.
    ///
    /// Eleven reasons, and the watermark rule over them is CLOSED - see
    /// [`ResetReason::watermark`]. "A reason absent from these two sets is a
    /// wire-decode error, not a guessed default"; here it cannot be absent,
    /// because the match is total and the compiler checks it.
    ResetReason {
        /// No prior state existed. The only reason with no prior watermark to
        /// decide about.
        Initial = 0x01,
        /// A different `relay_id` or `leader_term`. Clears: a higher term always
        /// invalidates per-term cursor state.
        RelayFailover = 0x02,
        /// The cursor's Database or Grant generation no longer matches. Clears -
        /// and binding comparison precedes term comparison exactly so that a
        /// simultaneous rebind and failover cannot retain a watermark from the
        /// old Database.
        GrantRebound = 0x03,
        /// An ordinary same-term Datastore reconnect outside an active reset
        /// generation. Clears, because crash recovery comes up on an unchanged
        /// `(systemid, timeline)` while a snapshot restore rewinds the WAL.
        DatastoreReconnect = 0x04,
        /// `systemid` or `timeline` changed, so the same LSN may now name
        /// different WAL. Clears.
        SystemIdentityChanged = 0x05,
        /// The cursor fell below the ring tail. Retains: the ring lost frames,
        /// the LSN space did not move.
        RingOverrun = 0x06,
        /// The Database epoch advanced. Retains.
        DatabaseEpochChanged = 0x07,
        /// The app hit `app_fault_threshold` projection or encoding failures
        /// inside `app_fault_window` and is quarantined. Retains.
        AppDegraded = 0x08,
        /// Quarantine ended and encoding succeeded again. Retains.
        AppReactivated = 0x09,
        /// The exact slot was invalidated (SQLSTATE `55000`, `wal_status =
        /// lost`). Retains.
        SlotInvalidated = 0x0a,
        /// A field changed from wire-plaintext to a protected representation, so
        /// the Datastore took the destructive reset barrier. Retains.
        ClassificationChanged = 0x0b,
    }
}

wire_enum! {
    /// Why the relay refused one app's registration.
    ///
    /// Seven codes, and the relay "makes exactly one resume decision per app, in
    /// this order, and a later rule never hides an earlier failure". The ORDER is
    /// relay logic; what lives here is the closed set and its retry policy, which
    /// is what the worker acts on.
    RegistrationRejectCode {
        /// Absent from topology.
        AppNotInTopology = 0x01,
        /// No active Grant.
        GrantInactive = 0x02,
        /// The app's Datastore stream is unavailable.
        DatastoreUnavailable = 0x03,
        /// Desired, observed and serving Database epochs are not all equal.
        /// **Precedes expected-epoch comparison**, so a worker correctly carrying
        /// the still-serving epoch gets this rather than
        /// [`Self::StaleExpectedBinding`].
        EpochPending = 0x04,
        /// The expected Database or Grant generation, or the expected epoch,
        /// differs from the serving binding. Not a reset.
        StaleExpectedBinding = 0x05,
        /// The cursor's `app_id` or `worker_id` differs from the request.
        CursorBindingMismatch = 0x06,
        /// `next_seq` is above `ring_next_seq`.
        CursorAhead = 0x07,
    }
}

impl RegistrationRejectCode {
    /// What the worker does about it.
    ///
    /// Total by construction. The policies come from the proposal verbatim:
    /// "`EpochPending` and `DatastoreUnavailable` retry with backoff;
    /// `StaleExpectedBinding` triggers one immediate topology refresh first;
    /// `AppNotInTopology` and `GrantInactive` retry only when the lease set
    /// changes; `CursorAhead` and `CursorBindingMismatch` are local invariant
    /// failures and do not spin."
    #[must_use]
    pub const fn retry(self) -> RegistrationRetry {
        match self {
            Self::EpochPending | Self::DatastoreUnavailable => RegistrationRetry::Backoff,
            Self::StaleExpectedBinding => RegistrationRetry::TopologyRefreshFirst,
            Self::AppNotInTopology | Self::GrantInactive => RegistrationRetry::OnLeaseSetChange,
            Self::CursorBindingMismatch | Self::CursorAhead => {
                RegistrationRetry::LocalInvariantFailure
            }
        }
    }
}

/// What a worker does after a per-app rejection. Not a wire type: it has no
/// discriminant and never crosses the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegistrationRetry {
    /// Retry with backoff.
    Backoff,
    /// Refresh topology once, immediately, then retry.
    TopologyRefreshFirst,
    /// Retry only when the lease set changes. Retrying sooner asks the same
    /// question of the same unchanged topology.
    OnLeaseSetChange,
    /// A local invariant failure. Do not spin - the worker sent a cursor it
    /// should not have had, and reconnecting will not fix that.
    LocalInvariantFailure,
}

impl ResetReason {
    /// The closed watermark rule.
    ///
    /// "A higher `leader_term` always invalidates the per-term `AppCursor` and
    /// subscriber state and clears the numeric watermark. A changed `systemid` or
    /// `timeline` invalidates the numeric watermark too, because the same LSN may
    /// now name different WAL." Every reason still refetches live state; this
    /// decides only the numeric `(commit_lsn, change_index)` dedup watermark.
    #[must_use]
    pub const fn watermark(self) -> WatermarkDecision {
        match self {
            Self::Initial => WatermarkDecision::NoPriorWatermark,
            Self::RelayFailover
            | Self::GrantRebound
            | Self::DatastoreReconnect
            | Self::SystemIdentityChanged => WatermarkDecision::Clear,
            Self::RingOverrun
            | Self::DatabaseEpochChanged
            | Self::AppDegraded
            | Self::AppReactivated
            | Self::SlotInvalidated
            | Self::ClassificationChanged => WatermarkDecision::Retain,
        }
    }
}

/// What a reset does to the numeric dedup watermark. Not a wire type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WatermarkDecision {
    /// There was no prior watermark to decide about.
    NoPriorWatermark,
    /// Clear it: the LSN space or the binding moved, so an old
    /// `(commit_lsn, change_index)` no longer names a comparable position.
    Clear,
    /// Retain it on the same `(systemid, timeline, database_id,
    /// grant_generation)`.
    Retain,
}

/// One cell of a row, three ways.
///
/// "**Encoding 'the server did not send it' the same way as 'it is NULL' is the
/// same class of collapse the projection exists to prevent.**" pgoutput sends an
/// unchanged-TOAST marker in place of a large unmodified value, and a consumer
/// that cannot tell the two apart will eventually write the wrong one into a
/// cache and call it a value. `Option<Bytes>` has two inhabitants for three
/// facts, so it is not used.
#[derive(Clone, PartialEq, Eq)]
pub enum CellValue {
    /// A present value, at most [`crate::MAX_CELL_BYTES`] long.
    Value(Vec<u8>),
    /// SQL NULL.
    Null,
    /// The server did not send this column's value - an unchanged TOAST.
    Unavailable,
}

impl core::fmt::Debug for CellValue {
    /// Length only, never bytes. A derived `Debug` here puts creator row data
    /// into any log line that formats a frame, which is exactly what the
    /// publication column list exists to make impossible one process earlier.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Value(bytes) => write!(f, "Value(<{} bytes>)", bytes.len()),
            Self::Null => f.write_str("Null"),
            Self::Unavailable => f.write_str("Unavailable"),
        }
    }
}

impl CellValue {
    /// Wire discriminant of a present value.
    pub const TAG_VALUE: u8 = 0x01;
    /// Wire discriminant of SQL NULL.
    pub const TAG_NULL: u8 = 0x02;
    /// Wire discriminant of an unchanged TOAST.
    pub const TAG_UNAVAILABLE: u8 = 0x03;

    /// Smallest possible encoding: one discriminant byte.
    pub(crate) const MIN_ENCODED_BYTES: usize = 1;

    /// Decode one cell.
    ///
    /// # Errors
    /// [`DecodeError::InvalidDiscriminant`] outside the three tags, or
    /// [`DecodeError::CellTooLarge`] above [`crate::MAX_CELL_BYTES`] - a
    /// conforming producer emits a keyed [`crate::Gap`] instead of an oversize
    /// cell, so receiving one means the peer is not conforming.
    pub fn decode(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        match reader.u8()? {
            Self::TAG_VALUE => {
                let declared = reader.u32()?;
                if declared > MAX_CELL_BYTES {
                    return Err(DecodeError::CellTooLarge { declared });
                }
                let len = usize::try_from(declared).map_err(|_| DecodeError::LengthOverflow)?;
                Ok(Self::Value(reader.take(len)?.to_vec()))
            }
            Self::TAG_NULL => Ok(Self::Null),
            Self::TAG_UNAVAILABLE => Ok(Self::Unavailable),
            value => Err(DecodeError::InvalidDiscriminant {
                kind: "CellValue",
                value,
            }),
        }
    }

    /// Encode one cell.
    ///
    /// # Errors
    /// [`EncodeError::CellTooLarge`] above [`crate::MAX_CELL_BYTES`]. Refused
    /// here so the oversize decision is made in one place by every encoder.
    pub fn encode(&self, writer: &mut Writer) -> Result<(), EncodeError> {
        match self {
            Self::Value(bytes) => {
                let len = u32::try_from(bytes.len())
                    .map_err(|_| EncodeError::CellTooLarge { len: bytes.len() })?;
                if len > MAX_CELL_BYTES {
                    return Err(EncodeError::CellTooLarge { len: bytes.len() });
                }
                writer.u8(Self::TAG_VALUE);
                writer.u32(len);
                writer.raw(bytes);
                Ok(())
            }
            Self::Null => {
                writer.u8(Self::TAG_NULL);
                Ok(())
            }
            Self::Unavailable => {
                writer.u8(Self::TAG_UNAVAILABLE);
                Ok(())
            }
        }
    }
}

/// What the relay decided about one requested app.
///
/// "`Registered` ... one app-identified outcome per requested app, in request
/// order." The worker's handling differs per variant and the difference is
/// load-bearing: "on `Resumed` it atomically replaces the relation cache and
/// replays; on `Reset` it first clears old relation and live-query state,
/// installs the staged batch atomically, applies the watermark rule, publishes
/// one local broker `Resync`, and only then accepts data."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegistrationOutcome {
    /// The cursor was in `[tail_seq, ring_next_seq)` and streaming continues from
    /// it.
    Resumed,
    /// The app is accepted but its interval is gone; the worker refetches.
    Reset(ResetReason),
    /// The app is not served. "One per-app rejection does not close accepted
    /// apps."
    Rejected(RegistrationRejectCode),
}

impl RegistrationOutcome {
    /// Wire discriminant of [`Self::Resumed`].
    pub const TAG_RESUMED: u8 = 0x01;
    /// Wire discriminant of [`Self::Reset`].
    pub const TAG_RESET: u8 = 0x02;
    /// Wire discriminant of [`Self::Rejected`].
    pub const TAG_REJECTED: u8 = 0x03;

    /// Decode one outcome.
    ///
    /// # Errors
    /// [`DecodeError::InvalidDiscriminant`] for an unknown outer or nested tag.
    pub fn decode(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        match reader.u8()? {
            Self::TAG_RESUMED => Ok(Self::Resumed),
            Self::TAG_RESET => Ok(Self::Reset(ResetReason::decode(reader)?)),
            Self::TAG_REJECTED => Ok(Self::Rejected(RegistrationRejectCode::decode(reader)?)),
            value => Err(DecodeError::InvalidDiscriminant {
                kind: "RegistrationOutcome",
                value,
            }),
        }
    }

    /// Encode one outcome.
    pub fn encode(self, writer: &mut Writer) {
        match self {
            Self::Resumed => writer.u8(Self::TAG_RESUMED),
            Self::Reset(reason) => {
                writer.u8(Self::TAG_RESET);
                reason.encode(writer);
            }
            Self::Rejected(code) => {
                writer.u8(Self::TAG_REJECTED);
                code.encode(writer);
            }
        }
    }
}
