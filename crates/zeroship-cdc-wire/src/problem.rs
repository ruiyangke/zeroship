//! The closed HTTP failure surface.
//!
//! "HTTP failures use `application/problem+json` with only a closed `code`."
//! Fourteen codes across eight statuses, and **none of them begins a binary body
//! or emits `Hello`** - a worker that has read a `Hello` is past this surface for
//! the life of the response.
//!
//! The classification is what the worker acts on: "the four `503`s retry with
//! full jitter from 250 ms to 5 s, minting a fresh assertion each attempt. Other
//! HTTP errors are terminal until topology or code changes and are never
//! converted into an empty successful registration."
//!
//! That last clause is why [`SubscribeProblemCode`] is not folded into the
//! registration outcomes: a `400` is not "every app was rejected". A worker that
//! flattened them would report an empty successful registration and stop
//! retrying a condition it never observed.

use serde::{Deserialize, Serialize};

use crate::limits::{RETRY_JITTER_MAX, RETRY_JITTER_MIN};

/// Media type of the failure body.
pub const PROBLEM_CONTENT_TYPE: &str = "application/problem+json";

/// Response header carrying the relay's supported wire version alongside a
/// `426`.
///
/// **Named here, not in the proposal**, which says only "`426
/// UnsupportedWireVersion` plus a version header". Both sides need one spelling;
/// this is it.
pub const WIRE_VERSION_HEADER: &str = "ZeroShip-CDC-Wire-Version";

/// What a worker does about an HTTP failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProblemRetry {
    /// Retry with full jitter between [`crate::limits::RETRY_JITTER_MIN`] and
    /// [`crate::limits::RETRY_JITTER_MAX`], minting a fresh assertion each
    /// attempt.
    JitterRetry,
    /// Terminal until topology revision or process version changes. Retrying the
    /// same request against the same peer asks a question already answered.
    Terminal,
}

impl ProblemRetry {
    /// The retry window, for the jittered arm.
    #[must_use]
    pub const fn window(self) -> Option<(core::time::Duration, core::time::Duration)> {
        match self {
            Self::JitterRetry => Some((RETRY_JITTER_MIN, RETRY_JITTER_MAX)),
            Self::Terminal => None,
        }
    }
}

/// The closed set of subscribe failures.
///
/// Serialized as the bare variant name, which IS the `code` value on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SubscribeProblemCode {
    /// `401`. The service assertion did not verify, or its `jti` could not be
    /// claimed. Refused "before its body is parsed".
    AuthenticationFailed,
    /// `400`. One app id appeared twice. Poisons the whole candidate generation.
    DuplicateApp,
    /// `400`. A generation below the highest admitted. "Disturbs no response."
    NonMonotonicRegistrationGeneration,
    /// `400`. A shard key already admitted with different body bytes. Poisons the
    /// whole candidate generation.
    ConflictingShard,
    /// `400`. The body did not decode.
    MalformedRequest,
    /// `406`. The `Accept` header did not name the CDC media type.
    NotAcceptable,
    /// `409`. The request named another relay's cluster. Checked before the
    /// assertion and before any app id.
    ClusterMismatch,
    /// `413`. The body exceeded [`crate::MAX_REQUEST_BYTES`].
    RequestTooLarge,
    /// `415`. The `Content-Type` was not the CDC media type.
    UnsupportedMediaType,
    /// `426`. The request named a wire version this relay does not speak.
    /// Accompanied by [`WIRE_VERSION_HEADER`].
    UnsupportedWireVersion,
    /// `503`. This process does not hold the leader lock. "A non-leader answers
    /// `503 NotLeader` with no redirect URL."
    NotLeader,
    /// `503`. The shared replay table is unreachable, so single-use cannot be
    /// proven. "Coordination PostgreSQL availability is therefore also an
    /// authentication dependency."
    AuthenticationStoreUnavailable,
    /// `503`. The term authority is unreachable.
    TermAuthorityUnavailable,
    /// `503`. No egress permit was available. "Before sending `200` a
    /// subscription acquires one full `egress_bytes_per_connection` permit from a
    /// global byte semaphore; without one it gets `503
    /// ConnectionCapacityExceeded` and no queue is allocated."
    ConnectionCapacityExceeded,
}

impl SubscribeProblemCode {
    /// Every code, for the table-driven arm the acceptance suite requires.
    pub const ALL: &'static [Self] = &[
        Self::AuthenticationFailed,
        Self::DuplicateApp,
        Self::NonMonotonicRegistrationGeneration,
        Self::ConflictingShard,
        Self::MalformedRequest,
        Self::NotAcceptable,
        Self::ClusterMismatch,
        Self::RequestTooLarge,
        Self::UnsupportedMediaType,
        Self::UnsupportedWireVersion,
        Self::NotLeader,
        Self::AuthenticationStoreUnavailable,
        Self::TermAuthorityUnavailable,
        Self::ConnectionCapacityExceeded,
    ];

    /// The HTTP status this code is served with.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::AuthenticationFailed => 401,
            Self::DuplicateApp
            | Self::NonMonotonicRegistrationGeneration
            | Self::ConflictingShard
            | Self::MalformedRequest => 400,
            Self::NotAcceptable => 406,
            Self::ClusterMismatch => 409,
            Self::RequestTooLarge => 413,
            Self::UnsupportedMediaType => 415,
            Self::UnsupportedWireVersion => 426,
            Self::NotLeader
            | Self::AuthenticationStoreUnavailable
            | Self::TermAuthorityUnavailable
            | Self::ConnectionCapacityExceeded => 503,
        }
    }

    /// Whether the worker retries.
    ///
    /// Keyed on the code, not on the status class: the four retryable ones are
    /// exactly the `503`s, and stating it as a match over codes means adding a
    /// code forces a decision instead of inheriting one from its status.
    #[must_use]
    pub const fn retry(self) -> ProblemRetry {
        match self {
            Self::NotLeader
            | Self::AuthenticationStoreUnavailable
            | Self::TermAuthorityUnavailable
            | Self::ConnectionCapacityExceeded => ProblemRetry::JitterRetry,
            Self::AuthenticationFailed
            | Self::DuplicateApp
            | Self::NonMonotonicRegistrationGeneration
            | Self::ConflictingShard
            | Self::MalformedRequest
            | Self::NotAcceptable
            | Self::ClusterMismatch
            | Self::RequestTooLarge
            | Self::UnsupportedMediaType
            | Self::UnsupportedWireVersion => ProblemRetry::Terminal,
        }
    }

    /// The exact `code` string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AuthenticationFailed => "AuthenticationFailed",
            Self::DuplicateApp => "DuplicateApp",
            Self::NonMonotonicRegistrationGeneration => "NonMonotonicRegistrationGeneration",
            Self::ConflictingShard => "ConflictingShard",
            Self::MalformedRequest => "MalformedRequest",
            Self::NotAcceptable => "NotAcceptable",
            Self::ClusterMismatch => "ClusterMismatch",
            Self::RequestTooLarge => "RequestTooLarge",
            Self::UnsupportedMediaType => "UnsupportedMediaType",
            Self::UnsupportedWireVersion => "UnsupportedWireVersion",
            Self::NotLeader => "NotLeader",
            Self::AuthenticationStoreUnavailable => "AuthenticationStoreUnavailable",
            Self::TermAuthorityUnavailable => "TermAuthorityUnavailable",
            Self::ConnectionCapacityExceeded => "ConnectionCapacityExceeded",
        }
    }
}

/// The whole failure body.
///
/// Two fields, both closed. There is no `detail`, and that is the point: a
/// free-text field on this surface is where a schema name, a column name or an
/// app id reaches an unauthenticated caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscribeProblem {
    /// The HTTP status, repeated in the body as RFC 9457 allows.
    pub status: u16,
    /// The closed code.
    pub code: SubscribeProblemCode,
}

impl SubscribeProblem {
    /// Build the body for a code, taking the status from the code itself so the
    /// two cannot disagree.
    #[must_use]
    pub const fn new(code: SubscribeProblemCode) -> Self {
        Self {
            status: code.status(),
            code,
        }
    }

    /// Render the body.
    ///
    /// # Errors
    /// `serde_json::Error` is unreachable for two scalar fields and is returned
    /// rather than unwrapped, because this crate denies `unwrap`.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Parse a body. Unknown fields and unknown codes are both refused.
    ///
    /// # Errors
    /// `serde_json::Error` for malformed JSON, an unknown field, or a code
    /// outside [`SubscribeProblemCode::ALL`].
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}
