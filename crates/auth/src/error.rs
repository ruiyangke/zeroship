//! Auth-wide error type.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("database: {0}")]
    Db(String),

    #[error("hydra admin: {0}")]
    Hydra(String),

    #[error("bootstrap: {0}")]
    Bootstrap(String),

    #[error("config: {0}")]
    Config(String),

    #[error("internal: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, AuthError>;
