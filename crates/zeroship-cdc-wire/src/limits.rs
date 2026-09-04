//! Protocol constants.
//!
//! "`max_frame_bytes = 1_048_576` and `max_cell_bytes = 524_288` are protocol
//! constants, not per-relay tunables, so every encoder makes the same
//! `Gap(OversizeValue)` decision." Nothing here is configurable, and nothing here
//! reads an environment variable: a limit an operator can move is a limit two
//! peers can disagree about.

use core::time::Duration;

/// The one supported handshake version.
///
/// "The request and `Hello` name the one exact supported handshake version;
/// there is no best-effort decode of another" - so both decoders refuse any
/// other value outright rather than negotiating down.
pub const WIRE_VERSION: u16 = 1;

/// Largest whole frame (tag plus payload) the protocol admits. A declared frame
/// length above this is a fatal protocol violation, not a resize.
pub const MAX_FRAME_BYTES: u32 = 1_048_576;

/// Largest single cell value.
///
/// A producer that would exceed it emits a keyed [`crate::Gap`] with
/// [`crate::GapReason::OversizeValue`] instead, which is why this is a constant
/// rather than a knob: two relays with different values would make that decision
/// differently for the same row.
pub const MAX_CELL_BYTES: u32 = 524_288;

/// Largest `SubscribeRequest` body. "body one binary `SubscribeRequest` capped at
/// 1 MiB and 1,024 apps."
pub const MAX_REQUEST_BYTES: u32 = 1_048_576;

/// Largest app count in one shard. The worker "splits into deterministic
/// contiguous shards of at most 1,024 apps"; a request or a `Registered` naming
/// more is refused.
pub const MAX_APPS_PER_SHARD: u32 = 1_024;

/// Byte ceiling on an encoded typed id.
///
/// From "a typed id is its canonical ASCII rendering, bounded to 64 bytes". The
/// bound is checked BEFORE the id text is parsed, so an oversized length prefix
/// never reaches the parser.
pub const MAX_TYPED_ID_BYTES: usize = 64;

/// Width of the `u32_be` frame-length prefix that precedes every frame.
pub const FRAME_LENGTH_PREFIX_BYTES: usize = 4;

/// Lowest value the bounded generation types admit. Zero is not a generation;
/// the columns that persist these are `bigint ... check (> 0)`.
pub const GENERATION_MIN: u64 = 1;

/// Highest value the bounded generation types admit. `i64::MAX`, "matching the
/// `bigint` columns that persist them".
pub const GENERATION_MAX: u64 = i64::MAX as u64;

/// Media type for both directions of the subscribe exchange.
pub const CDC_CONTENT_TYPE: &str = "application/vnd.zeroship.cdc.v1";

/// The one subscribe endpoint.
pub const SUBSCRIBE_PATH: &str = "/internal/v1/cdc/subscribe";

/// Exact audience the relay's service assertion must carry. A verifier that
/// accepts a different audience accepts a token minted for another service.
pub const SERVICE_ASSERTION_AUDIENCE: &str = "spiffe://zeroship.ai/svc/cdc";

/// Domain separator for the term-permit commitment. The trailing NUL is part of
/// it: a domain that is a prefix of another domain lets one preimage serve two
/// purposes.
pub const PERMIT_COMMITMENT_DOMAIN: &[u8] = b"zs-cdc-subscribe-v1\0";

/// Floor of the full-jitter retry window for the four retryable `503`s.
pub const RETRY_JITTER_MIN: Duration = Duration::from_millis(250);

/// Ceiling of the full-jitter retry window for the four retryable `503`s.
pub const RETRY_JITTER_MAX: Duration = Duration::from_secs(5);
