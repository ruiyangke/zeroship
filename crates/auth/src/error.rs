//! Auth-wide error type.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("database: {0}")]
    Db(String),

    #[error("database[{code}]: {message}")]
    DbCode { code: String, message: String },

    #[error("hydra admin: {0}")]
    Hydra(String),

    #[error("bootstrap: {0}")]
    Bootstrap(String),

    #[error("config: {0}")]
    Config(String),

    #[error("internal: {0}")]
    Internal(String),
}

impl AuthError {
    #[must_use]
    pub fn db_code(&self) -> Option<&str> {
        match self {
            Self::DbCode { code, .. } => Some(code.as_str()),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, AuthError>;
