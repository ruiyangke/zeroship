//! Unified error type for V8 ops.
//!
//! All ops return `Result<T, OpError>` for type-safe error handling.
//! The proc macro converts `OpError` to the appropriate V8 exception type.

/// Error kind — maps to JS exception types.
#[derive(Debug, Clone, Copy)]
pub enum OpErrorKind {
    /// `TypeError` — wrong argument types, missing arguments
    TypeError,
    /// `RangeError` — value out of bounds
    RangeError,
    /// Generic `Error`
    Error,
}

/// An error from a V8 op.
#[derive(Debug, Clone)]
pub struct OpError {
    pub kind: OpErrorKind,
    pub message: String,
}

impl OpError {
    pub fn type_error(msg: impl Into<String>) -> Self {
        Self {
            kind: OpErrorKind::TypeError,
            message: msg.into(),
        }
    }

    pub fn range_error(msg: impl Into<String>) -> Self {
        Self {
            kind: OpErrorKind::RangeError,
            message: msg.into(),
        }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            kind: OpErrorKind::Error,
            message: msg.into(),
        }
    }
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for OpError {}
