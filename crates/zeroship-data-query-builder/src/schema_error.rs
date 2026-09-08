//! Leaf-crate errors for the schema layer.
//!
//! `zeroship-data-query-builder` cannot depend on plugin-db's runtime-coupled `DbError`.
//! The sentinel codec therefore returns the small, self-contained error below;
//! plugin-db maps it into its neutral data-plane error at the call boundary.

/// Error from parsing a `zero-migrate:mask:` sentinel string
/// ([`crate::mask_codec::parse_mask_sentinel`]).
///
/// Carries the human-readable rejection message. plugin-db's
/// `From<MaskSentinelError> for DbError` re-creates the exact
/// `DbError::internal("mask_sentinel_malformed: …")` the SDK round-trips,
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
