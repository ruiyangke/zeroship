//! Protection services supplied by the host, independent of connection drivers.
use crate::{driver::Session, encryption::KeyStore, error::DbError, sql::SchemaName};
use async_trait::async_trait;
use crate::sql::catalog::LiveSchema;

/// Live catalog evidence supplies a protection floor; descriptors own model shape.
#[async_trait(?Send)]
pub trait Catalog {
    /// Read the bound schema using the active session when supplied.
    /// The host must supply a session from this backend for this binding.
    /// A transaction must not compete with its own lease for pool capacity.
    async fn introspect_schema(
        &self,
        app_id: &str,
        schema: &SchemaName,
        session: Option<&Session>,
    ) -> Result<LiveSchema, DbError>;
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
