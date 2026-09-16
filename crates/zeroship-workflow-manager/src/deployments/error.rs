use zeroship_data_orm::error::DbError;

/// Deployment retention failures, independent of the customer workflow engine.
/// Database diagnostics are reduced to these categories at the storage boundary.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid deployment retention request: {0}")]
    InvalidRequest(String),
    #[error("deployment retention authentication is required")]
    Unauthenticated,
    #[error("deployment retention operation is not permitted")]
    PermissionDenied,
    #[error("deployment retention conflict: {0}")]
    Conflict(String),
    #[error("deployment retention capacity exhausted: {0}")]
    ResourceExhausted(String),
    #[error("deployment retention is unavailable: {0}")]
    Unavailable(String),
    #[error("deployment retention operation timed out")]
    Timeout,
    #[error("deployment retention failed: {0}")]
    Internal(String),
}

impl From<DbError> for Error {
    fn from(error: DbError) -> Self {
        match error {
            DbError::PermissionDenied { .. } => Self::PermissionDenied,
            DbError::Transient { .. }
            | DbError::Serialization { .. }
            | DbError::LockContention { .. } => {
                Self::Unavailable("deployment database temporarily unavailable".into())
            }
            _ => Self::Internal("deployment database operation failed".into()),
        }
    }
}

impl From<zeroship_core::workflow_deployments::Error> for Error {
    fn from(error: zeroship_core::workflow_deployments::Error) -> Self {
        match error {
            zeroship_core::workflow_deployments::Error::InvalidHolder => {
                Self::InvalidRequest("invalid deployment holder".into())
            }
            zeroship_core::workflow_deployments::Error::GenerationExhausted => {
                Self::ResourceExhausted("deployment hold generation exhausted".into())
            }
        }
    }
}
