//! Errors shared by schema metadata codecs and runtime query validation.
//! The ORM converts these errors to its database error contract.

/// A malformed protection sentinel, with a diagnostic message.
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
