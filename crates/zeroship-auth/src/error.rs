//! Auth-wide error type.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error(transparent)]
    Orm(Box<zeroship_data_orm::orm::DbError>),

    #[error("database: {0}")]
    Db(String),

    #[error("database[{code}]: {message}")]
    DbCode { code: String, message: String },

    #[error("bootstrap: {0}")]
    Bootstrap(String),

    #[error("config: {0}")]
    Config(String),

    #[error("internal: {0}")]
    Internal(String),
}

impl From<zeroship_data_orm::orm::DbError> for AuthError {
    fn from(error: zeroship_data_orm::orm::DbError) -> Self {
        Self::Orm(Box::new(error))
    }
}

pub type Result<T> = std::result::Result<T, AuthError>;
