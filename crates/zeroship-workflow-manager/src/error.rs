use zeroship_data_orm::error::DbError;

/// Closed failures contain no database credentials or customer metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid workflow manager configuration or request")]
    Invalid,
    #[error("workflow manager authority is missing or expired")]
    Denied,
    #[error("workflow metadata identity or revision conflicts")]
    Conflict,
    #[error("workflow manager capacity or metadata exceeds its bound")]
    Capacity,
    #[error("workflow manager operation timed out")]
    Timeout,
    #[error("workflow manager storage is temporarily unavailable")]
    Unavailable,
    #[error("workflow manager storage contract failed")]
    Storage,
}

impl From<DbError> for Error {
    fn from(error: DbError) -> Self {
        match error {
            DbError::UniqueViolation { .. }
            | DbError::SchemaRefused {
                code: "unique_violation",
                ..
            } => Self::Conflict,
            DbError::Transient { .. }
            | DbError::Serialization { .. }
            | DbError::LockContention { .. } => Self::Unavailable,
            // This binding uses the platform service's database authority.
            // Storage permission failures do not classify its caller's placement.
            _ => Self::Storage,
        }
    }
}
