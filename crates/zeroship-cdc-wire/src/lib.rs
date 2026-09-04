//! The CDC relay/worker wire contract.
//!
//! This crate is the types both `zeroship-cdc` (the relay) and
//! `zeroship-plugin-db` (the worker) name, and nothing else. It encodes and
//! decodes; it performs no I/O, opens no connection, and holds no state.
//! `docs/proposals/2026-08-28-cdc-service.md`, section "The worker/relay wire
//! contract", is the authority for every constant, tag and layout rule below.
//!
//! # What is here, and what deliberately is not
//!
//! Here: the framing, the eleven frame types, the `SubscribeRequest` and its
//! permit commitment, the closed enums (`ResetReason`, `RegistrationRejectCode`,
//! `GapReason`, `CellValue`, `ChangeOp`), the identity and generation vocabulary
//! those frames carry, and the closed HTTP problem codes the worker classifies.
//!
//! Not here, and not because it was forgotten:
//!
//! - **The relay.** Ring buffers, leader election, slot lifecycle, decode,
//!   confirmation, quarantine. The proposal blocks the relay twice - on the
//!   Datastore/Database/Grant entities of
//!   `docs/proposals/2026-08-28-app-database-decoupling.md`, and on a platform
//!   move to PostgreSQL 18.4 the deployment has not made. Neither blocks the
//!   encoding.
//! - **The topology snapshot and `TopologyAck`.** Those are the relay/control
//!   contract, and their shape is the entity work's to fix.
//!   `Datastore { engine, cluster_id, dsn_secret_ref, resource_key }` does not
//!   exist yet.
//! - **`ChangeEvent`.** That is the in-process broker's shape
//!   (`zeroship_core::change_event`), and the proposal changes it separately.
//!   Fusing the two would make the wire format move whenever the local event
//!   does.
//!
//! # The identity vocabulary is defined here, and that was a decision
//!
//! `DatastoreId`, `ClusterId`, `DatabaseEpoch` and `GrantGeneration` had zero
//! occurrences anywhere in `crates/` or `db/migrations-ts/` when this crate was
//! written. They are defined HERE rather than deferred to the decoupling
//! proposal, for four reasons:
//!
//! 1. **The alternative is empty.** Every sequenced frame carries the envelope
//!    `(app_id, grant_generation, seq)`, and eight of the eleven frames carry a
//!    Datastore, Database, epoch or generation besides. A wire crate that ships
//!    only the identity-free frames ships a byte reader, not a wire.
//! 2. **The domain rule is a DECODER obligation, stated in the wire section.**
//!    "`LeaderTerm`, `DatabaseEpoch` and `GrantGeneration` encode as `u64` but
//!    their constructors admit only `1..=i64::MAX` ... a decoder refuses an
//!    out-of-domain value before comparing or storing it." A constructor that
//!    lives in a crate the decoder may not depend on is a rule the decoder
//!    re-derives, and two derivations drift.
//! 3. **`LeaderTerm`, `RelationGeneration` and `PrimeBatchId` have no home in the
//!    entity work at all.** They are CDC-only. Splitting one uniform family of
//!    `u64` newtypes across two crates by which proposal happened to name each
//!    one first is arbitrary.
//! 4. **The spellings are adopted, not invented.** `ds_` and `dbs_` come from the
//!    decoupling proposal's own entity table; `wrk_` and `rly_` from the CDC
//!    proposal; `app_` is `zeroship_core::typed_id::APP_PREFIX`.
//!
//! **What that leaves the entity work.** When Datastore/Database/Grant land they
//! must adopt these types rather than mint a second set - the prefixes are one
//! constant each ([`ids`]) precisely so that adoption is a re-export and a
//! divergence is a compile error at the boundary. Two spellings here are
//! genuinely open and are flagged where they are defined: `ds_` is two letters,
//! which contradicts the same proposal's stated three-letter rule, and
//! `ClusterId`'s form is pinned by neither document.
//!
//! # Fixed by this crate
//!
//! The proposal fixes the eleven frame tags and says "nested enums are fixed
//! too" without listing their discriminants. This crate fixes them, and they are
//! now the contract: **every nested enum starts at `0x01`**, so `0x00` is never a
//! valid discriminant anywhere in the protocol and a zero-filled buffer cannot
//! decode into a meaningful frame.
//!
//! # Reading hostile bytes
//!
//! Every decoder here reads bytes from a peer. The crate-level denies below are
//! the mechanical half of "every per-app step is total": a panic in this crate is
//! reachable from anything that can open a connection. The other half is
//! [`DecodeError`], which is closed and carries only numbers and `&'static str` -
//! no row bytes, no column name, no id text - so surfacing a decode failure
//! cannot leak the payload that caused it. [`CellValue`] and
//! [`SubscribeRequest`] implement `Debug` by hand for the same reason: the
//! derived one prints row values and a credential.
// `clippy::doc_markdown` reads "PostgreSQL" and "UUIDv7" as unbackticked code
// items. They are proper nouns in prose throughout this crate's docs, and
// backticking a proper noun makes it read as an identifier that exists
// somewhere. Allowed here rather than worked around, and narrowly: it is the
// only lint this crate relaxes, and the four below are additions.
#![allow(clippy::doc_markdown)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::panic)]

pub mod codec;
pub mod enums;
pub mod error;
pub mod frame;
pub mod ids;
pub mod limits;
pub mod problem;
pub mod request;
pub mod scalars;

pub use codec::{frame_length_prefix, Reader, Writer};
pub use enums::{
    CellValue, ChangeOp, GapReason, RegistrationOutcome, RegistrationRejectCode, RegistrationRetry,
    ResetReason, WatermarkDecision,
};
pub use error::{DecodeError, EncodeError};
pub use frame::{
    AppOutcome, AppReset, Change, Envelope, Epoch, Frame, FrameTag, Gap, Heartbeat, Hello,
    Registered, Relation, RelationPrime, Resync, Truncate,
};
pub use ids::{AppId, ClusterId, DatabaseId, DatastoreId, RelayId, WorkerId};
pub use limits::{
    MAX_APPS_PER_SHARD, MAX_CELL_BYTES, MAX_FRAME_BYTES, MAX_REQUEST_BYTES, MAX_TYPED_ID_BYTES,
    WIRE_VERSION,
};
pub use problem::{ProblemRetry, SubscribeProblem, SubscribeProblemCode};
pub use request::{AppCursor, AppRegistration, ExpectedBinding, SubscribeRequest};
pub use scalars::{
    ChangeIndex, DatabaseEpoch, GrantGeneration, LeaderTerm, Lsn, PrimeBatchId,
    RegistrationGeneration, RelationGeneration, Sequence, SystemId, Timeline,
};
