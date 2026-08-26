//! Error types for the schema layer.
//!
//! The schema layer keeps a minimal dependency surface (`serde_json` + `sha2`).
//! It therefore does NOT depend on a data plane's `DbError`, which is
//! built on a runtime `OpError`. Instead the fallible surface this layer exposes -
//! the mask-sentinel codec - returns a small,
//! self-contained error type. A data-plane consumer maps it back into its
//! own error at the call boundary via a `From` impl, so the wire shape and
//! SQLSTATE-derived `.code` the SDK sees stay byte-identical to before the extraction.

/// Error from parsing a `zero-migrate:mask:` sentinel string
/// (`crate::schema::mask_codec::parse_mask_sentinel`).
///
/// Carries the human-readable rejection message. plugin-db's
/// `From<MaskSentinelError> for DbError` re-creates the exact
/// `DbError::internal("mask_sentinel_malformed: ...")` the SDK round-trips,
/// so the `mask_sentinel_malformed` code-discriminator is preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskSentinelError {
    /// The full rejection message, including the `mask_sentinel_malformed: `
    /// prefix the introspection layer + SDK contract expect.
    pub message: String,
}

impl MaskSentinelError {
    /// Construct from a fully-formed message (caller includes the
    /// `mask_sentinel_malformed: ` prefix).
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Borrow the message body.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for MaskSentinelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MaskSentinelError {}
