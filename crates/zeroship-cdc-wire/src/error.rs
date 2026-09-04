//! The two closed error types.
//!
//! **Neither carries payload.** Every field below is a number or a
//! `&'static str`, so a decode failure can be logged, counted and returned to a
//! peer without the bytes that caused it travelling with it. That is the same
//! rule that makes [`crate::GapReason`] a closed enum rather than a string:
//! "an open reason field is where a physical column name reappears the moment
//! someone writes a helpful error message."
//!
//! Neither type is `#[non_exhaustive]`. A consumer matching exhaustively is the
//! point - a new variant must break the worker's classifier at compile time
//! rather than fall into a catch-all arm.

use core::fmt;

/// A fatal protocol violation while decoding.
///
/// Every variant closes the response. "A frame decode violation closes the
/// response, drops staged state, surfaces the closed decoder code without row
/// bytes, and does not retry the same endpoint and wire version until topology
/// revision or process version changes."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// A frame declared length zero. `frame_len` counts the tag, so the minimum
    /// legal frame is 1.
    ZeroFrameLength,
    /// A frame declared more than [`crate::MAX_FRAME_BYTES`].
    FrameTooLarge {
        /// The declared length, for the metric label. Not trusted for anything.
        declared: u32,
    },
    /// The buffer ended inside a field.
    Truncated {
        /// Bytes the field needed.
        needed: usize,
        /// Bytes that were left.
        available: usize,
    },
    /// The frame decoded but bytes were left over. A payload longer than its
    /// fields is a different frame, not a tolerable one.
    TrailingBytes {
        /// How many bytes were left.
        remaining: usize,
    },
    /// A frame tag outside the eleven.
    UnknownFrameTag {
        /// The tag byte.
        tag: u8,
    },
    /// A nested enum discriminant outside its closed set. `kind` names the enum,
    /// never the value it was decoding.
    InvalidDiscriminant {
        /// Type name of the enum that refused it.
        kind: &'static str,
        /// The discriminant byte.
        value: u8,
    },
    /// A Boolean encoded as something other than `0x00` or `0x01`.
    InvalidBoolean {
        /// The offending byte.
        value: u8,
    },
    /// A length-prefixed string that was not UTF-8. The bytes are dropped, not
    /// reported.
    InvalidUtf8,
    /// A declared length or a count multiplication did not fit `usize`.
    LengthOverflow,
    /// A vector count claimed more elements than the remaining bytes could hold
    /// even at the minimum element width. Checked BEFORE allocation, which is
    /// the point: this is the arm that stops a four-byte count from reserving
    /// four gigabytes.
    CountExceedsInput {
        /// Type name of the vector's element, for the metric label.
        kind: &'static str,
        /// The claimed count.
        count: u32,
    },
    /// A cell value longer than [`crate::MAX_CELL_BYTES`]. A conforming producer
    /// emits a keyed [`crate::Gap`] instead, so receiving one means the peer is
    /// not conforming.
    CellTooLarge {
        /// The declared cell length.
        declared: u32,
    },
    /// A typed id whose encoded length exceeded [`crate::MAX_TYPED_ID_BYTES`].
    /// Refused before the id text is examined.
    TypedIdTooLong {
        /// The declared length.
        declared: u32,
    },
    /// A typed id with the wrong prefix, the wrong length, a character outside
    /// the base62 alphabet, or a noncanonical encoding. One variant on purpose:
    /// telling a peer WHICH way its id was wrong tells it how to probe.
    MalformedTypedId {
        /// The prefix the field required.
        expected_prefix: &'static str,
    },
    /// A bounded generation outside `1..=i64::MAX`. Refused "before comparing or
    /// storing it", so an out-of-domain epoch cannot win a comparison.
    OutOfDomain {
        /// Type name of the newtype that refused it.
        kind: &'static str,
    },
    /// A `Hello` or `SubscribeRequest` naming a version other than
    /// [`crate::WIRE_VERSION`]. There is no best-effort decode of another.
    UnsupportedWireVersion {
        /// The version the peer named.
        declared: u16,
    },
    /// More than [`crate::MAX_APPS_PER_SHARD`] apps in one request or one
    /// `Registered`.
    TooManyApps {
        /// The claimed app count.
        count: u32,
    },
    /// A `SubscribeRequest` body over [`crate::MAX_REQUEST_BYTES`].
    RequestTooLarge {
        /// The body length.
        len: usize,
    },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroFrameLength => f.write_str("frame length is zero"),
            Self::FrameTooLarge { declared } => {
                write!(f, "frame length {declared} exceeds max_frame_bytes")
            }
            Self::Truncated { needed, available } => {
                write!(f, "truncated: needed {needed} bytes, {available} available")
            }
            Self::TrailingBytes { remaining } => {
                write!(f, "{remaining} trailing payload bytes")
            }
            Self::UnknownFrameTag { tag } => write!(f, "unknown frame tag {tag:#04x}"),
            Self::InvalidDiscriminant { kind, value } => {
                write!(f, "invalid {kind} discriminant {value:#04x}")
            }
            Self::InvalidBoolean { value } => write!(f, "invalid boolean {value:#04x}"),
            Self::InvalidUtf8 => f.write_str("invalid utf-8 in a length-prefixed string"),
            Self::LengthOverflow => f.write_str("declared length overflows usize"),
            Self::CountExceedsInput { kind, count } => {
                write!(f, "{kind} count {count} exceeds the remaining input")
            }
            Self::CellTooLarge { declared } => {
                write!(f, "cell length {declared} exceeds max_cell_bytes")
            }
            Self::TypedIdTooLong { declared } => {
                write!(f, "typed id length {declared} exceeds the 64-byte bound")
            }
            Self::MalformedTypedId { expected_prefix } => {
                write!(
                    f,
                    "malformed typed id, expected prefix '{expected_prefix}_'"
                )
            }
            Self::OutOfDomain { kind } => write!(f, "{kind} outside 1..=i64::MAX"),
            Self::UnsupportedWireVersion { declared } => {
                write!(f, "unsupported wire version {declared}")
            }
            Self::TooManyApps { count } => write!(f, "{count} apps exceeds the shard cap"),
            Self::RequestTooLarge { len } => {
                write!(f, "request body of {len} bytes exceeds max_request_bytes")
            }
        }
    }
}

impl core::error::Error for DecodeError {}

/// A value that cannot be represented on this wire.
///
/// Encoding failures are a producer's own bug or an oversize row, never a peer's
/// input, so these are separate from [`DecodeError`] rather than folded into one
/// enum: a metric that counts both together cannot tell "we were attacked" from
/// "we tried to send something too big".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// The encoded frame would exceed [`crate::MAX_FRAME_BYTES`]. The producer's
    /// answer is a keyed [`crate::Gap`]; a `Gap` that also does not fit is an
    /// `AppReset(AppDegraded)` and quarantine, which is relay policy rather than
    /// anything this crate decides.
    FrameTooLarge {
        /// The encoded length.
        len: usize,
    },
    /// A cell value exceeds [`crate::MAX_CELL_BYTES`]. Refused here so the
    /// oversize decision is made once, in one place, by every encoder.
    CellTooLarge {
        /// The value length.
        len: usize,
    },
    /// A byte string or UTF-8 string longer than `u32::MAX`.
    ValueTooLarge {
        /// The value length.
        len: usize,
    },
    /// A vector longer than `u32::MAX`.
    TooManyElements {
        /// The element count.
        count: usize,
    },
    /// More than [`crate::MAX_APPS_PER_SHARD`] apps in one request or one
    /// `Registered`.
    TooManyApps {
        /// The app count.
        count: usize,
    },
    /// The encoded `SubscribeRequest` exceeds [`crate::MAX_REQUEST_BYTES`].
    RequestTooLarge {
        /// The encoded length.
        len: usize,
    },
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameTooLarge { len } => {
                write!(f, "encoded frame of {len} bytes exceeds max_frame_bytes")
            }
            Self::CellTooLarge { len } => {
                write!(f, "cell of {len} bytes exceeds max_cell_bytes")
            }
            Self::ValueTooLarge { len } => write!(f, "value of {len} bytes exceeds u32"),
            Self::TooManyElements { count } => write!(f, "{count} elements exceeds u32"),
            Self::TooManyApps { count } => write!(f, "{count} apps exceeds the shard cap"),
            Self::RequestTooLarge { len } => {
                write!(f, "request of {len} bytes exceeds max_request_bytes")
            }
        }
    }
}

impl core::error::Error for EncodeError {}
