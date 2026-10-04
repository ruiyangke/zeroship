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
    /// Another transaction holds the app's queue lock, and this caller asked
    /// for it without waiting. PostgreSQL alone reports it: SQLite has no row
    /// locks, so its lock waits end as `Unavailable` or `Timeout`.
    #[error("workflow app queue is contended")]
    Contended,
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
            // A lock wait that ran out, on either backend. Only the zone
            // claim's non-waiting scope lock reports `Contended`, at the one
            // site that asks for a lock without waiting for it.
            DbError::LockContention { .. }
            | DbError::Transient { .. }
            | DbError::Serialization { .. } => Self::Unavailable,
            // This binding uses the platform service's database authority.
            // Storage permission failures say nothing about its caller's authority.
            _ => Self::Storage,
        }
    }
}
