use zeroship_data_orm::error::DbError;

/// Closed failures contain no database credentials or customer metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid workflow queue configuration or request")]
    Invalid,
    #[error("workflow queue authority is missing or expired")]
    Denied,
    #[error("workflow job identity or delivery conflicts")]
    Conflict,
    #[error("workflow queue metadata exceeds its bound")]
    Capacity,
    #[error("workflow queue operation timed out")]
    Timeout,
    #[error("workflow queue storage is temporarily unavailable")]
    Unavailable,
    #[error("workflow queue storage contract failed")]
    Storage,
}

impl From<DbError> for Error {
    fn from(error: DbError) -> Self {
        match error {
            DbError::PermissionDenied { .. } => Self::Denied,
            DbError::UniqueViolation { .. }
            | DbError::SchemaRefused {
                code: "unique_violation",
                ..
            } => Self::Conflict,
            DbError::Transient { .. }
            | DbError::Serialization { .. }
            | DbError::LockContention { .. } => Self::Unavailable,
            _ => Self::Storage,
        }
    }
}
