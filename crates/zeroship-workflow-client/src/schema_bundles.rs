//! Sending a platform schema bundle to the migration service.
//!
//! Domain-neutral on purpose: this client posts a [`SchemaBundle`] and reads the
//! outcome. It does not build one, and it does not know what any bundle
//! describes - the caller that OWNS the schema owns its artifacts. A second
//! platform-owned schema reuses this unchanged.

use std::sync::Arc;

use zeroship_core::{
    schema_bundle::{SchemaBundle, SchemaBundleOutcome},
    service_identity::endpoints,
    service_peers::{service_issuer, ServiceAuth, MIGRATE_SERVICE_NAME, WORKFLOW_SERVICE_NAME},
};

use super::{Error, Options, Transport};

/// The request bound a bundle needs.
///
/// A bundle carries a whole ordered series as SQL, so the bound has to admit the
/// platform's own generated artifacts. It is still a bound: the body is
/// generated in this repository, never authored by a creator.
pub const MAX_BUNDLE_BYTES: usize = 4 * 1024 * 1024;

/// The workflow manager's channel to the migration service.
#[derive(Clone, Debug)]
pub struct SchemaBundles {
    transport: Transport,
}

impl SchemaBundles {
    /// Bind the workflow service's own signer and the migration service origin.
    ///
    /// # Errors
    /// Refuses a missing or foreign signer, an invalid origin, and empty
    /// exchange bounds.
    pub fn new(url: &str, auth: Arc<ServiceAuth>, options: Options) -> Result<Self, Error> {
        let (issuer, _) = auth.signing_identity().ok_or(Error::Unauthenticated)?;
        let role = service_issuer(WORKFLOW_SERVICE_NAME).map_err(|_| Error::InvalidConfig)?;
        if issuer != &role {
            return Err(Error::Unauthenticated);
        }
        Ok(Self {
            transport: Transport::new(
                url,
                auth,
                service_issuer(MIGRATE_SERVICE_NAME).map_err(|_| Error::InvalidConfig)?,
                Options {
                    max_request_bytes: options.max_request_bytes.max(MAX_BUNDLE_BYTES),
                    ..options
                },
            )?,
        })
    }

    /// Install or upgrade the schema the bundle names.
    ///
    /// Idempotent at the far end, so a caller may send the same bundle on every
    /// registration without checking first.
    ///
    /// # Errors
    /// Transport failures and every refusal the migration service answers with,
    /// including a bundle behind the installed schema and a corrupted schema.
    pub async fn apply(&self, bundle: &SchemaBundle) -> Result<SchemaBundleOutcome, Error> {
        let outcome: SchemaBundleOutcome = self
            .transport
            .post(endpoints::MIGRATE_SCHEMA_BUNDLE, bundle)
            .await?;
        // The answer must describe the request. A reply naming another schema or
        // another version is not a weaker success, it is evidence the exchange
        // reached something other than what was asked.
        if outcome.schema != bundle.schema
            || outcome.bundle != bundle.bundle
            || outcome.version != bundle.version
        {
            return Err(Error::InvalidResponse);
        }
        Ok(outcome)
    }
}
