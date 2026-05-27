//! Gateway-wide error type.
//!
//! Today this is consumed only by `sessions` (P3-U4); other modules
//! continue to surface their own local error enums or `ntex` responses
//! until the gateway's error story is consolidated.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("database: {0}")]
    Db(String),
}

pub type Result<T> = std::result::Result<T, GatewayError>;
