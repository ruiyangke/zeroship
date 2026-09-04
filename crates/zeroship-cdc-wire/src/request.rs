//! The `SubscribeRequest` body and its permit commitment.
//!
//! `POST /internal/v1/cdc/subscribe`, `Content-Type`/`Accept:
//! application/vnd.zeroship.cdc.v1`, "body one binary `SubscribeRequest` capped
//! at 1 MiB and 1,024 apps". The body carries no framing prefix and no tag: it is
//! one message, and its length is the HTTP body's.
//!
//! # The commitment
//!
//! "The permit commitment is SHA-256 over domain `zs-cdc-subscribe-v1\0` plus the
//! canonical request bytes with `term_permit` encoded as zero-length; neither the
//! permit nor the `Authorization` header is in its preimage."
//!
//! Both exclusions matter and they are different. **The permit is out** because
//! control signs the commitment INTO the permit - a permit committing to its own
//! bytes is not computable. **The `Authorization` header is out** because it
//! carries a single-use assertion minted fresh per attempt, so including it would
//! make the commitment change on every retry of an identical request.
//!
//! The zero-length encoding is not "skip the field": the `u32_be(0)` length stays
//! in the preimage. Removing the field entirely would let a request with no
//! permit and a request with an empty permit hash the same.

use sha2::{Digest, Sha256};

use crate::codec::{Reader, Writer};
use crate::error::{DecodeError, EncodeError};
use crate::ids::{AppId, ClusterId, DatabaseId, RelayId, WorkerId};
use crate::limits::{
    MAX_APPS_PER_SHARD, MAX_REQUEST_BYTES, PERMIT_COMMITMENT_DOMAIN, WIRE_VERSION,
};
use crate::scalars::{
    DatabaseEpoch, GrantGeneration, LeaderTerm, RegistrationGeneration, Sequence,
};

/// Floor on one encoded [`AppRegistration`]: two id length prefixes, two `u64`
/// generations and the cursor's `Option` tag.
const MIN_APP_REGISTRATION_BYTES: usize = 4 + 4 + 8 + 8 + 1;

/// The binding the worker believes it is subscribing to.
///
/// Compared against the serving binding before any cursor is looked at: a
/// difference is [`crate::RegistrationRejectCode::StaleExpectedBinding`], "not a
/// reset".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedBinding {
    /// The Database the app expects to be bound to.
    pub database_id: DatabaseId,
    /// The epoch it expects that Database to be serving.
    pub database_epoch: DatabaseEpoch,
    /// The Grant generation it expects.
    pub grant_generation: GrantGeneration,
}

impl ExpectedBinding {
    fn encode_into(&self, writer: &mut Writer) -> Result<(), EncodeError> {
        self.database_id.encode(writer)?;
        self.database_epoch.encode(writer);
        self.grant_generation.encode(writer);
        Ok(())
    }

    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            database_id: DatabaseId::decode(reader)?,
            database_epoch: DatabaseEpoch::decode(reader)?,
            grant_generation: GrantGeneration::decode(reader)?,
        })
    }
}

/// Where one app's stream resumes.
///
/// Carries its own binding as well as its position, and both are compared:
/// "**binding comparison precedes term comparison**, so a simultaneous rebind and
/// failover cannot retain a watermark from the old Database". It carries its own
/// `app_id` and `worker_id` too, so a cursor swapped between two apps or two
/// worker processes is [`crate::RegistrationRejectCode::CursorBindingMismatch`]
/// rather than a plausible-looking resume.
///
/// It does NOT carry the numeric `(commit_lsn, change_index)` dedup watermark.
/// That is worker-local: the relay never consults it, and putting it on the wire
/// would invite a worker to negotiate about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppCursor {
    /// The app this cursor is for.
    pub app_id: AppId,
    /// The worker PROCESS that held it. Neither id survives a restart.
    pub worker_id: WorkerId,
    /// The relay PROCESS that issued it.
    pub relay_id: RelayId,
    /// The term it was issued in. A different pair is
    /// [`crate::ResetReason::RelayFailover`].
    pub leader_term: LeaderTerm,
    /// The Database it was bound to.
    pub database_id: DatabaseId,
    /// The epoch it was reading.
    pub database_epoch: DatabaseEpoch,
    /// The Grant generation it was bound to.
    pub grant_generation: GrantGeneration,
    /// The next sequence number the worker expects. Above `ring_next_seq` is
    /// `CursorAhead`; below `tail_seq` resets as `RingOverrun`.
    pub next_seq: Sequence,
}

impl AppCursor {
    fn encode_into(&self, writer: &mut Writer) -> Result<(), EncodeError> {
        self.app_id.encode(writer)?;
        self.worker_id.encode(writer)?;
        self.relay_id.encode(writer)?;
        self.leader_term.encode(writer);
        self.database_id.encode(writer)?;
        self.database_epoch.encode(writer);
        self.grant_generation.encode(writer);
        self.next_seq.encode(writer);
        Ok(())
    }

    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            app_id: AppId::decode(reader)?,
            worker_id: WorkerId::decode(reader)?,
            relay_id: RelayId::decode(reader)?,
            leader_term: LeaderTerm::decode(reader)?,
            database_id: DatabaseId::decode(reader)?,
            database_epoch: DatabaseEpoch::decode(reader)?,
            grant_generation: GrantGeneration::decode(reader)?,
            next_seq: Sequence::decode(reader)?,
        })
    }
}

/// One app in a subscribe request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppRegistration {
    /// The app. "`cdc_lifecycle` builds `apps` solely from its server-injected
    /// active lease map. No V8 value, request field or creator header can supply
    /// or replace an `app_id`."
    pub app_id: AppId,
    /// What the worker believes the binding is.
    pub expected_binding: ExpectedBinding,
    /// Where to resume, if the worker has a cursor. "No cursor resets as
    /// `Initial`."
    pub cursor: Option<AppCursor>,
}

impl AppRegistration {
    fn encode_into(&self, writer: &mut Writer) -> Result<(), EncodeError> {
        self.app_id.encode(writer)?;
        self.expected_binding.encode_into(writer)?;
        writer.option(self.cursor.as_ref(), |writer, cursor| {
            cursor.encode_into(writer)
        })
    }

    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            app_id: AppId::decode(reader)?,
            expected_binding: ExpectedBinding::decode_from(reader)?,
            cursor: reader.option(AppCursor::decode_from)?,
        })
    }
}

/// One shard of one worker's registration snapshot.
///
/// "One immutable request per shard; apps from two clusters never share a
/// response. All shards of one snapshot share a `registration_generation` and
/// carry `shard_index` and `shard_count`."
///
/// Admission is keyed by `(worker_id, registration_generation, shard_index)`, and
/// "identical request" means the binary body bytes only - which is why this type
/// has one canonical encoding and no optional formatting.
#[derive(Clone, PartialEq, Eq)]
pub struct SubscribeRequest {
    /// Always [`crate::WIRE_VERSION`].
    pub wire_version: u16,
    /// The cluster. "The relay rejects a foreign `cluster_id`, then authenticates
    /// the assertion and redeems the permit, **before examining any app id**."
    pub cluster_id: ClusterId,
    /// The worker PROCESS.
    pub worker_id: WorkerId,
    /// The control-signed term permit, opaque here. It binds cluster,
    /// worker-process id, credential, admission epoch, relay pair, shard key and
    /// the commitment; this crate carries and excludes it, and control mints and
    /// verifies it.
    pub term_permit: Vec<u8>,
    /// Shared by every shard of this snapshot.
    pub registration_generation: RegistrationGeneration,
    /// This shard's index.
    pub shard_index: u32,
    /// How many shards the snapshot has. One fixed value per generation.
    pub shard_count: u32,
    /// The apps, at most [`crate::MAX_APPS_PER_SHARD`].
    pub apps: Vec<AppRegistration>,
}

impl core::fmt::Debug for SubscribeRequest {
    /// Redacts `term_permit`. It is a credential; a derived `Debug` puts it in
    /// any log line that formats a request.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SubscribeRequest")
            .field("wire_version", &self.wire_version)
            .field("cluster_id", &self.cluster_id)
            .field("worker_id", &self.worker_id)
            .field(
                "term_permit",
                &format_args!("<{} bytes>", self.term_permit.len()),
            )
            .field("registration_generation", &self.registration_generation)
            .field("shard_index", &self.shard_index)
            .field("shard_count", &self.shard_count)
            .field("apps", &self.apps.len())
            .finish()
    }
}

impl SubscribeRequest {
    /// Encode the body.
    ///
    /// # Errors
    /// [`EncodeError::TooManyApps`] above [`crate::MAX_APPS_PER_SHARD`], or
    /// [`EncodeError::RequestTooLarge`] above [`crate::MAX_REQUEST_BYTES`].
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let bytes = self.encode_with_permit(&self.term_permit)?;
        if bytes.len() > MAX_REQUEST_BYTES as usize {
            return Err(EncodeError::RequestTooLarge { len: bytes.len() });
        }
        Ok(bytes)
    }

    /// The permit commitment: SHA-256 over the domain separator and the canonical
    /// request bytes with `term_permit` encoded as zero-length.
    ///
    /// # Errors
    /// Whatever [`Self::encode`] would refuse about the app count.
    pub fn commitment(&self) -> Result<[u8; 32], EncodeError> {
        let canonical = self.encode_with_permit(&[])?;
        let mut hasher = Sha256::new();
        hasher.update(PERMIT_COMMITMENT_DOMAIN);
        hasher.update(&canonical);
        Ok(hasher.finalize().into())
    }

    fn encode_with_permit(&self, permit: &[u8]) -> Result<Vec<u8>, EncodeError> {
        if self.apps.len() > MAX_APPS_PER_SHARD as usize {
            return Err(EncodeError::TooManyApps {
                count: self.apps.len(),
            });
        }
        let mut writer = Writer::new();
        writer.u16(self.wire_version);
        self.cluster_id.encode(&mut writer)?;
        self.worker_id.encode(&mut writer)?;
        writer.bytes(permit)?;
        self.registration_generation.encode(&mut writer);
        writer.u32(self.shard_index);
        writer.u32(self.shard_count);
        writer.count(self.apps.len())?;
        for app in &self.apps {
            app.encode_into(&mut writer)?;
        }
        Ok(writer.into_vec())
    }

    /// Decode a body.
    ///
    /// # Errors
    /// [`DecodeError::RequestTooLarge`] before anything is parsed,
    /// [`DecodeError::UnsupportedWireVersion`] for any version but
    /// [`crate::WIRE_VERSION`], [`DecodeError::TooManyApps`] above the shard cap,
    /// and [`DecodeError::TrailingBytes`] for a body longer than its fields.
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        if buf.len() > MAX_REQUEST_BYTES as usize {
            return Err(DecodeError::RequestTooLarge { len: buf.len() });
        }
        let mut reader = Reader::new(buf);
        let wire_version = reader.u16()?;
        if wire_version != WIRE_VERSION {
            return Err(DecodeError::UnsupportedWireVersion {
                declared: wire_version,
            });
        }
        let cluster_id = ClusterId::decode(&mut reader)?;
        let worker_id = WorkerId::decode(&mut reader)?;
        let term_permit = reader.bytes()?.to_vec();
        let registration_generation = RegistrationGeneration::decode(&mut reader)?;
        let shard_index = reader.u32()?;
        let shard_count = reader.u32()?;
        let count = reader.count("AppRegistration", MIN_APP_REGISTRATION_BYTES)?;
        if count > MAX_APPS_PER_SHARD {
            return Err(DecodeError::TooManyApps { count });
        }
        let mut apps = Vec::with_capacity(count as usize);
        for _ in 0..count {
            apps.push(AppRegistration::decode_from(&mut reader)?);
        }
        reader.finish()?;
        Ok(Self {
            wire_version,
            cluster_id,
            worker_id,
            term_permit,
            registration_generation,
            shard_index,
            shard_count,
            apps,
        })
    }
}
