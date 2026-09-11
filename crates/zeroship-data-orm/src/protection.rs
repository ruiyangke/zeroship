//! Protection services supplied by the host, independent of connection drivers.
use crate::{encryption::KeyStore, error::DbError};
use async_trait::async_trait;
use zeroship_data_sql::catalog::LiveSchema;

/// Live catalog evidence supplies a protection floor; descriptors own model shape.
#[async_trait(?Send)]
pub trait Catalog {
    async fn introspect_schema(&self, app_id: &str) -> Result<LiveSchema, DbError>;
}

/// Column keys belong to the host's protection context.
pub trait Protection {
    fn key_store(&self) -> &KeyStore;
}

pub(crate) mod encryption_pass;
pub mod mask_pass;
pub mod mask_policy;
pub(crate) mod protection_floor;
pub mod unmask;
