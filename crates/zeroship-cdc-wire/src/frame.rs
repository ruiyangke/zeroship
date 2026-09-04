//! The eleven frames.
//!
//! # Framing
//!
//! `u32_be(frame_len) || u8(tag) || payload`, where `frame_len` counts the tag
//! and payload. A zero length, a length over [`crate::MAX_FRAME_BYTES`], invalid
//! UTF-8, trailing payload bytes or an unknown tag is a fatal protocol violation
//! that closes the response.
//!
//! # The envelope
//!
//! Six frames are SEQUENCED and carry `(app_id, grant_generation, seq)`; five are
//! not. The envelope is encoded FIRST, before the fields the proposal's frame
//! table lists, because it is what routes the frame to a ring - a reader that had
//! to decode the body to learn which binding the frame belongs to would have to
//! decode a frame it may be about to discard.
//!
//! `Registered` and `Resync` are connection-local and consume no app sequence:
//! "with two workers on one app, putting one slow worker's reset in the shared
//! sequence would either reset the healthy worker or leave a hole in its cursor."
//! `AppReset` IS shared, "because it records a condition invalidating every
//! worker for the app". `Heartbeat` is Datastore telemetry and belongs to no app.
//!
//! # Why a column list and not a map per row
//!
//! "The framing borrows pgoutput's own solution to the same problem: column names
//! must not repeat per row. A `Change` costs its values plus a small header."
//! [`Change::values`] is positional against the `columns` of the primed or last
//! [`Relation`] for its `relation_generation` - which is also why losing that
//! generation counter is loss of sequence state.

use crate::codec::{frame_length_prefix, Reader, Writer};
use crate::enums::{CellValue, ChangeOp, GapReason, RegistrationOutcome, ResetReason};
use crate::error::{DecodeError, EncodeError};
use crate::ids::{AppId, ClusterId, DatabaseId, DatastoreId, RelayId};
use crate::limits::{FRAME_LENGTH_PREFIX_BYTES, MAX_APPS_PER_SHARD, MAX_FRAME_BYTES, WIRE_VERSION};
use crate::scalars::{
    ChangeIndex, DatabaseEpoch, GrantGeneration, LeaderTerm, Lsn, PrimeBatchId,
    RegistrationGeneration, RelationGeneration, Sequence, SystemId, Timeline,
};

/// Smallest encoding of a length-prefixed string: the four-byte length itself.
const MIN_STRING_BYTES: usize = 4;
/// Smallest encoding of one [`AppOutcome`]: an id length prefix plus an outcome
/// discriminant.
const MIN_APP_OUTCOME_BYTES: usize = MIN_STRING_BYTES + 1;

/// The fixed frame tags.
///
/// "Frame tags are fixed: `Hello=0x01`, `RelationPrime=0x02`, `Registered=0x03`,
/// `Resync=0x04`, `AppReset=0x05`, `Relation=0x06`, `Change=0x07`, `Gap=0x08`,
/// `Truncate=0x09`, `Epoch=0x0a`, `Heartbeat=0x0b`."
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FrameTag {
    /// [`Hello`], always the first frame of a `200`.
    Hello,
    /// [`RelationPrime`].
    RelationPrime,
    /// [`Registered`], the terminal frame of a registration.
    Registered,
    /// [`Resync`].
    Resync,
    /// [`AppReset`].
    AppReset,
    /// [`Relation`].
    Relation,
    /// [`Change`].
    Change,
    /// [`Gap`].
    Gap,
    /// [`Truncate`].
    Truncate,
    /// [`Epoch`].
    Epoch,
    /// [`Heartbeat`].
    Heartbeat,
}

impl FrameTag {
    /// Every tag, in wire order.
    pub const ALL: &'static [Self] = &[
        Self::Hello,
        Self::RelationPrime,
        Self::Registered,
        Self::Resync,
        Self::AppReset,
        Self::Relation,
        Self::Change,
        Self::Gap,
        Self::Truncate,
        Self::Epoch,
        Self::Heartbeat,
    ];

    /// The fixed tag byte.
    #[must_use]
    pub const fn byte(self) -> u8 {
        match self {
            Self::Hello => 0x01,
            Self::RelationPrime => 0x02,
            Self::Registered => 0x03,
            Self::Resync => 0x04,
            Self::AppReset => 0x05,
            Self::Relation => 0x06,
            Self::Change => 0x07,
            Self::Gap => 0x08,
            Self::Truncate => 0x09,
            Self::Epoch => 0x0a,
            Self::Heartbeat => 0x0b,
        }
    }

    /// Map a tag byte back.
    ///
    /// # Errors
    /// [`DecodeError::UnknownFrameTag`] outside the eleven. Its own variant
    /// rather than `InvalidDiscriminant`, because the proposal names "an unknown
    /// tag" as its own fatal class and an operator counting protocol violations
    /// wants the two apart.
    pub const fn from_byte(tag: u8) -> Result<Self, DecodeError> {
        match tag {
            0x01 => Ok(Self::Hello),
            0x02 => Ok(Self::RelationPrime),
            0x03 => Ok(Self::Registered),
            0x04 => Ok(Self::Resync),
            0x05 => Ok(Self::AppReset),
            0x06 => Ok(Self::Relation),
            0x07 => Ok(Self::Change),
            0x08 => Ok(Self::Gap),
            0x09 => Ok(Self::Truncate),
            0x0a => Ok(Self::Epoch),
            0x0b => Ok(Self::Heartbeat),
            _ => Err(DecodeError::UnknownFrameTag { tag }),
        }
    }

    /// Whether frames with this tag carry an [`Envelope`] and consume an app
    /// sequence number.
    #[must_use]
    pub const fn is_sequenced(self) -> bool {
        match self {
            Self::AppReset
            | Self::Relation
            | Self::Change
            | Self::Gap
            | Self::Truncate
            | Self::Epoch => true,
            Self::Hello
            | Self::RelationPrime
            | Self::Registered
            | Self::Resync
            | Self::Heartbeat => false,
        }
    }
}

/// The routing header on every sequenced frame.
///
/// "Every sequenced frame carries the envelope `(app_id, grant_generation, seq)`
/// and lives in the one ring for that exact binding."
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// The app whose ring this frame belongs to.
    pub app_id: AppId,
    /// The Grant generation the ring is bound to. A frame carrying a stale
    /// generation belongs to a ring that no longer exists.
    pub grant_generation: GrantGeneration,
    /// Position in that ring's sequence.
    pub seq: Sequence,
}

impl Envelope {
    fn encode_payload(&self, writer: &mut Writer) -> Result<(), EncodeError> {
        self.app_id.encode(writer)?;
        self.grant_generation.encode(writer);
        self.seq.encode(writer);
        Ok(())
    }

    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            app_id: AppId::decode(reader)?,
            grant_generation: GrantGeneration::decode(reader)?,
            seq: Sequence::decode(reader)?,
        })
    }
}

/// The first frame of every `200`, and the relay's identity for the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// Always [`crate::WIRE_VERSION`]. Any other value is refused on decode
    /// rather than negotiated.
    pub wire_version: u16,
    /// The relay PROCESS. With `leader_term`, the fencing pair.
    pub relay_id: RelayId,
    /// The term this relay won.
    pub leader_term: LeaderTerm,
    /// The cluster this relay leads. A worker checks it before anything else.
    pub cluster_id: ClusterId,
    /// `IDENTIFY_SYSTEM`'s `systemid`.
    pub systemid: SystemId,
    /// `IDENTIFY_SYSTEM`'s timeline.
    pub timeline: Timeline,
}

/// One relation's shape, sent before the `Registered` that references it.
///
/// Unsequenced. Sent "for every relation generation referenced by the replay
/// interval **and every currently active generation**, under a
/// connection-monotonic never-reused `prime_batch_id`", so a reset cannot discard
/// the primes sent immediately before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationPrime {
    /// Groups this prime with the others of its batch.
    pub prime_batch_id: PrimeBatchId,
    /// The app the relation belongs to.
    pub app_id: AppId,
    /// The Database the relation lives in.
    pub database_id: DatabaseId,
    /// The Database epoch this shape belongs to.
    pub database_epoch: DatabaseEpoch,
    /// The Grant generation.
    pub grant_generation: GrantGeneration,
    /// The generation this shape is; [`Change::values`] is positional against it.
    pub relation_generation: RelationGeneration,
    /// Creator-visible collection name.
    pub collection: String,
    /// The published column list, in wire order. Never contains a
    /// `__zs_raw__<field>` name: the projection is built from `valueColumn` and
    /// nothing else, one process earlier, by PostgreSQL.
    pub columns: Vec<String>,
}

/// The terminal frame of one registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    /// The generation every shard of this snapshot shares.
    pub registration_generation: RegistrationGeneration,
    /// One outcome per requested app, in request order.
    pub outcomes: Vec<AppOutcome>,
}

/// One app's registration result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppOutcome {
    /// The app it is about. Identified rather than positional so a worker cannot
    /// silently misalign a truncated list with its request.
    pub app_id: AppId,
    /// What the relay decided.
    pub outcome: RegistrationOutcome,
}

/// A connection-local reset. Consumes no app sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resync {
    /// The prime batch staged for this reset.
    pub prime_batch_id: PrimeBatchId,
    /// How many distinct relation generations that batch contains. "Count zero is
    /// a valid complete empty batch" - the worker requires exactly this many
    /// rather than treating zero as absence.
    pub prime_relation_count: u32,
    /// The app.
    pub app_id: AppId,
    /// The Database.
    pub database_id: DatabaseId,
    /// The Database epoch.
    pub database_epoch: DatabaseEpoch,
    /// The Grant generation.
    pub grant_generation: GrantGeneration,
    /// Why. Decides the watermark rule through [`ResetReason::watermark`].
    pub reason: ResetReason,
    /// Where this connection resumes: "every reset skips [the retained interval]
    /// and accepts exactly the current `ring_next_seq`".
    pub accepted_cursor: Sequence,
}

/// An app-wide reset. Sequenced, because it invalidates every worker on the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppReset {
    /// Routing header.
    pub envelope: Envelope,
    /// Why.
    pub reason: ResetReason,
}

/// A relation shape delivered in-stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    /// Routing header.
    pub envelope: Envelope,
    /// The generation this shape is.
    pub relation_generation: RelationGeneration,
    /// Creator-visible collection name.
    pub collection: String,
    /// The published column list, in wire order.
    pub columns: Vec<String>,
}

/// One committed row change.
#[derive(Clone, PartialEq, Eq)]
pub struct Change {
    /// Routing header.
    pub envelope: Envelope,
    /// Which [`Relation`] `values` is positional against.
    pub relation_generation: RelationGeneration,
    /// Insert, update or delete.
    pub op: ChangeOp,
    /// `Begin.final_lsn` - "LSN of the commit record", available on the FIRST
    /// frame of the transaction, so the relay stamps it as it decodes without
    /// buffering.
    pub commit_lsn: Lsn,
    /// The 0-based DML ordinal within that transaction. With `commit_lsn`, the
    /// whole dedup key.
    pub change_index: ChangeIndex,
    /// The replica-identity vector. For an identity-changing UPDATE this is the
    /// PRE-change identity and the new one is in `values`; otherwise it is the
    /// current identity for insert and update, and the deleted identity for
    /// delete. There is no old tuple, here or in `ChangeEvent`.
    pub pk: Vec<CellValue>,
    /// The row, positional against the relation's `columns`.
    pub values: Vec<CellValue>,
}

impl core::fmt::Debug for Change {
    /// Cell COUNTS, never cells. [`CellValue`]'s own `Debug` already redacts, and
    /// this keeps a `{:?}` on a whole frame from printing a row's worth of
    /// redaction markers that a reader might mistake for the row.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Change")
            .field("envelope", &self.envelope)
            .field("relation_generation", &self.relation_generation)
            .field("op", &self.op)
            .field("commit_lsn", &self.commit_lsn)
            .field("change_index", &self.change_index)
            .field("pk_cells", &self.pk.len())
            .field("value_cells", &self.values.len())
            .finish()
    }
}

/// A row that could not be delivered, keyed so a subscriber knows WHICH row.
#[derive(Clone, PartialEq, Eq)]
pub struct Gap {
    /// Routing header.
    pub envelope: Envelope,
    /// Creator-visible collection name.
    pub collection: String,
    /// The replica-identity vector of the dropped row.
    pub pk: Vec<CellValue>,
    /// Commit LSN of the transaction it was in.
    pub commit_lsn: Lsn,
    /// DML ordinal within that transaction.
    pub change_index: ChangeIndex,
    /// Why, from a closed set.
    pub reason: GapReason,
}

impl core::fmt::Debug for Gap {
    /// Key cell COUNT, never key cells.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Gap")
            .field("envelope", &self.envelope)
            .field("collection", &self.collection)
            .field("pk_cells", &self.pk.len())
            .field("commit_lsn", &self.commit_lsn)
            .field("change_index", &self.change_index)
            .field("reason", &self.reason)
            .finish()
    }
}

/// `TRUNCATE` reached one or more collections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truncate {
    /// Routing header.
    pub envelope: Envelope,
    /// The collections truncated, in one statement.
    pub collections: Vec<String>,
}

/// The Database epoch advanced.
///
/// Appended "atomically ... to every active grantee ring for that Database" only
/// after the desired/observed/serving rendezvous, so a worker cannot see an E+1
/// frame before all three agree on E+1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Epoch {
    /// Routing header.
    pub envelope: Envelope,
    /// The new epoch.
    pub database_epoch: DatabaseEpoch,
}

/// Datastore health telemetry.
///
/// "**Never a worker resume coordinate.**" `last_confirmed_lsn` is the slot's
/// position, and the slot advances at full speed while delivery to every worker
/// is failing - which is exactly why a worker resumes from an `AppCursor`
/// instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heartbeat {
    /// The Datastore.
    pub datastore_id: DatastoreId,
    /// The slot's last confirmed position.
    pub last_confirmed_lsn: Lsn,
}

/// One frame of the response stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// See [`Hello`].
    Hello(Hello),
    /// See [`RelationPrime`].
    RelationPrime(RelationPrime),
    /// See [`Registered`].
    Registered(Registered),
    /// See [`Resync`].
    Resync(Resync),
    /// See [`AppReset`].
    AppReset(AppReset),
    /// See [`Relation`].
    Relation(Relation),
    /// See [`Change`].
    Change(Change),
    /// See [`Gap`].
    Gap(Gap),
    /// See [`Truncate`].
    Truncate(Truncate),
    /// See [`Epoch`].
    Epoch(Epoch),
    /// See [`Heartbeat`].
    Heartbeat(Heartbeat),
}

impl Frame {
    /// This frame's tag.
    #[must_use]
    pub const fn tag(&self) -> FrameTag {
        match self {
            Self::Hello(_) => FrameTag::Hello,
            Self::RelationPrime(_) => FrameTag::RelationPrime,
            Self::Registered(_) => FrameTag::Registered,
            Self::Resync(_) => FrameTag::Resync,
            Self::AppReset(_) => FrameTag::AppReset,
            Self::Relation(_) => FrameTag::Relation,
            Self::Change(_) => FrameTag::Change,
            Self::Gap(_) => FrameTag::Gap,
            Self::Truncate(_) => FrameTag::Truncate,
            Self::Epoch(_) => FrameTag::Epoch,
            Self::Heartbeat(_) => FrameTag::Heartbeat,
        }
    }

    /// The envelope, for the six sequenced frames.
    #[must_use]
    pub const fn envelope(&self) -> Option<&Envelope> {
        match self {
            Self::AppReset(f) => Some(&f.envelope),
            Self::Relation(f) => Some(&f.envelope),
            Self::Change(f) => Some(&f.envelope),
            Self::Gap(f) => Some(&f.envelope),
            Self::Truncate(f) => Some(&f.envelope),
            Self::Epoch(f) => Some(&f.envelope),
            Self::Hello(_)
            | Self::RelationPrime(_)
            | Self::Registered(_)
            | Self::Resync(_)
            | Self::Heartbeat(_) => None,
        }
    }

    /// Encode the whole frame, length prefix included.
    ///
    /// # Errors
    /// [`EncodeError::FrameTooLarge`] when the result would exceed
    /// [`crate::MAX_FRAME_BYTES`], plus whatever the payload's own limits refuse.
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let mut payload = Writer::new();
        self.encode_payload(&mut payload)?;
        let payload = payload.into_vec();
        let frame_len = payload
            .len()
            .checked_add(1)
            .ok_or(EncodeError::FrameTooLarge { len: payload.len() })?;
        let declared =
            u32::try_from(frame_len).map_err(|_| EncodeError::FrameTooLarge { len: frame_len })?;
        if declared > MAX_FRAME_BYTES {
            return Err(EncodeError::FrameTooLarge { len: frame_len });
        }
        let mut out = Writer::new();
        out.u32(declared);
        out.u8(self.tag().byte());
        out.raw(&payload);
        Ok(out.into_vec())
    }

    /// Decode exactly one whole frame, length prefix included.
    ///
    /// # Errors
    /// [`DecodeError::Truncated`] when the buffer is shorter than the declared
    /// frame, [`DecodeError::TrailingBytes`] when it is longer, and the framing
    /// violations [`frame_length_prefix`] rules on.
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        let total = frame_length_prefix(buf)?.ok_or(DecodeError::Truncated {
            needed: FRAME_LENGTH_PREFIX_BYTES,
            available: buf.len(),
        })?;
        if buf.len() < total {
            return Err(DecodeError::Truncated {
                needed: total,
                available: buf.len(),
            });
        }
        if buf.len() > total {
            return Err(DecodeError::TrailingBytes {
                remaining: buf.len() - total,
            });
        }
        let mut reader = Reader::new(buf);
        // Already validated by `frame_length_prefix`; consumed to advance.
        let _prefix = reader.u32()?;
        let tag = FrameTag::from_byte(reader.u8()?)?;
        let frame = Self::decode_body(tag, &mut reader)?;
        reader.finish()?;
        Ok(frame)
    }

    /// Decode a payload whose tag has already been read.
    ///
    /// For a caller that framed the stream itself. Trailing bytes are still
    /// fatal.
    ///
    /// # Errors
    /// Whatever the payload decoder returns, plus
    /// [`DecodeError::TrailingBytes`].
    pub fn decode_payload(tag: FrameTag, payload: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(payload);
        let frame = Self::decode_body(tag, &mut reader)?;
        reader.finish()?;
        Ok(frame)
    }

    fn encode_payload(&self, writer: &mut Writer) -> Result<(), EncodeError> {
        match self {
            Self::Hello(f) => {
                writer.u16(f.wire_version);
                f.relay_id.encode(writer)?;
                f.leader_term.encode(writer);
                f.cluster_id.encode(writer)?;
                f.systemid.encode(writer);
                f.timeline.encode(writer);
            }
            Self::RelationPrime(f) => {
                f.prime_batch_id.encode(writer);
                f.app_id.encode(writer)?;
                f.database_id.encode(writer)?;
                f.database_epoch.encode(writer);
                f.grant_generation.encode(writer);
                f.relation_generation.encode(writer);
                writer.string(&f.collection)?;
                encode_strings(writer, &f.columns)?;
            }
            Self::Registered(f) => {
                if f.outcomes.len() > MAX_APPS_PER_SHARD as usize {
                    return Err(EncodeError::TooManyApps {
                        count: f.outcomes.len(),
                    });
                }
                f.registration_generation.encode(writer);
                writer.count(f.outcomes.len())?;
                for outcome in &f.outcomes {
                    outcome.app_id.encode(writer)?;
                    outcome.outcome.encode(writer);
                }
            }
            Self::Resync(f) => {
                f.prime_batch_id.encode(writer);
                writer.u32(f.prime_relation_count);
                f.app_id.encode(writer)?;
                f.database_id.encode(writer)?;
                f.database_epoch.encode(writer);
                f.grant_generation.encode(writer);
                f.reason.encode(writer);
                f.accepted_cursor.encode(writer);
            }
            Self::AppReset(f) => {
                f.envelope.encode_payload(writer)?;
                f.reason.encode(writer);
            }
            Self::Relation(f) => {
                f.envelope.encode_payload(writer)?;
                f.relation_generation.encode(writer);
                writer.string(&f.collection)?;
                encode_strings(writer, &f.columns)?;
            }
            Self::Change(f) => {
                f.envelope.encode_payload(writer)?;
                f.relation_generation.encode(writer);
                f.op.encode(writer);
                f.commit_lsn.encode(writer);
                f.change_index.encode(writer);
                encode_cells(writer, &f.pk)?;
                encode_cells(writer, &f.values)?;
            }
            Self::Gap(f) => {
                f.envelope.encode_payload(writer)?;
                writer.string(&f.collection)?;
                encode_cells(writer, &f.pk)?;
                f.commit_lsn.encode(writer);
                f.change_index.encode(writer);
                f.reason.encode(writer);
            }
            Self::Truncate(f) => {
                f.envelope.encode_payload(writer)?;
                encode_strings(writer, &f.collections)?;
            }
            Self::Epoch(f) => {
                f.envelope.encode_payload(writer)?;
                f.database_epoch.encode(writer);
            }
            Self::Heartbeat(f) => {
                f.datastore_id.encode(writer)?;
                f.last_confirmed_lsn.encode(writer);
            }
        }
        Ok(())
    }

    fn decode_body(tag: FrameTag, reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(match tag {
            FrameTag::Hello => {
                let wire_version = reader.u16()?;
                if wire_version != WIRE_VERSION {
                    return Err(DecodeError::UnsupportedWireVersion {
                        declared: wire_version,
                    });
                }
                Self::Hello(Hello {
                    wire_version,
                    relay_id: RelayId::decode(reader)?,
                    leader_term: LeaderTerm::decode(reader)?,
                    cluster_id: ClusterId::decode(reader)?,
                    systemid: SystemId::decode(reader)?,
                    timeline: Timeline::decode(reader)?,
                })
            }
            FrameTag::RelationPrime => Self::RelationPrime(RelationPrime {
                prime_batch_id: PrimeBatchId::decode(reader)?,
                app_id: AppId::decode(reader)?,
                database_id: DatabaseId::decode(reader)?,
                database_epoch: DatabaseEpoch::decode(reader)?,
                grant_generation: GrantGeneration::decode(reader)?,
                relation_generation: RelationGeneration::decode(reader)?,
                collection: reader.string()?,
                columns: decode_strings(reader, "columns")?,
            }),
            FrameTag::Registered => {
                let registration_generation = RegistrationGeneration::decode(reader)?;
                let count = reader.count("AppOutcome", MIN_APP_OUTCOME_BYTES)?;
                if count > MAX_APPS_PER_SHARD {
                    return Err(DecodeError::TooManyApps { count });
                }
                let mut outcomes = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    outcomes.push(AppOutcome {
                        app_id: AppId::decode(reader)?,
                        outcome: RegistrationOutcome::decode(reader)?,
                    });
                }
                Self::Registered(Registered {
                    registration_generation,
                    outcomes,
                })
            }
            FrameTag::Resync => Self::Resync(Resync {
                prime_batch_id: PrimeBatchId::decode(reader)?,
                prime_relation_count: reader.u32()?,
                app_id: AppId::decode(reader)?,
                database_id: DatabaseId::decode(reader)?,
                database_epoch: DatabaseEpoch::decode(reader)?,
                grant_generation: GrantGeneration::decode(reader)?,
                reason: ResetReason::decode(reader)?,
                accepted_cursor: Sequence::decode(reader)?,
            }),
            FrameTag::AppReset => Self::AppReset(AppReset {
                envelope: Envelope::decode_payload(reader)?,
                reason: ResetReason::decode(reader)?,
            }),
            FrameTag::Relation => Self::Relation(Relation {
                envelope: Envelope::decode_payload(reader)?,
                relation_generation: RelationGeneration::decode(reader)?,
                collection: reader.string()?,
                columns: decode_strings(reader, "columns")?,
            }),
            FrameTag::Change => Self::Change(Change {
                envelope: Envelope::decode_payload(reader)?,
                relation_generation: RelationGeneration::decode(reader)?,
                op: ChangeOp::decode(reader)?,
                commit_lsn: Lsn::decode(reader)?,
                change_index: ChangeIndex::decode(reader)?,
                pk: decode_cells(reader, "pk")?,
                values: decode_cells(reader, "values")?,
            }),
            FrameTag::Gap => Self::Gap(Gap {
                envelope: Envelope::decode_payload(reader)?,
                collection: reader.string()?,
                pk: decode_cells(reader, "pk")?,
                commit_lsn: Lsn::decode(reader)?,
                change_index: ChangeIndex::decode(reader)?,
                reason: GapReason::decode(reader)?,
            }),
            FrameTag::Truncate => Self::Truncate(Truncate {
                envelope: Envelope::decode_payload(reader)?,
                collections: decode_strings(reader, "collections")?,
            }),
            FrameTag::Epoch => Self::Epoch(Epoch {
                envelope: Envelope::decode_payload(reader)?,
                database_epoch: DatabaseEpoch::decode(reader)?,
            }),
            FrameTag::Heartbeat => Self::Heartbeat(Heartbeat {
                datastore_id: DatastoreId::decode(reader)?,
                last_confirmed_lsn: Lsn::decode(reader)?,
            }),
        })
    }
}

fn encode_strings(writer: &mut Writer, values: &[String]) -> Result<(), EncodeError> {
    writer.count(values.len())?;
    for value in values {
        writer.string(value)?;
    }
    Ok(())
}

fn decode_strings(reader: &mut Reader<'_>, kind: &'static str) -> Result<Vec<String>, DecodeError> {
    reader.vec(kind, MIN_STRING_BYTES, Reader::string)
}

fn encode_cells(writer: &mut Writer, cells: &[CellValue]) -> Result<(), EncodeError> {
    writer.count(cells.len())?;
    for cell in cells {
        cell.encode(writer)?;
    }
    Ok(())
}

fn decode_cells(
    reader: &mut Reader<'_>,
    kind: &'static str,
) -> Result<Vec<CellValue>, DecodeError> {
    reader.vec(kind, CellValue::MIN_ENCODED_BYTES, CellValue::decode)
}
