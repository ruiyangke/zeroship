//! Workflow-service access to Control's app and plan facts.

use super::{Error, Options, Transport};
use std::sync::Arc;
use zeroship_core::{
    app_id::AppId,
    service_identity::endpoints,
    service_peers::{CONTROL_SERVICE_NAME, ServiceAuth, WORKFLOW_SERVICE_NAME, service_issuer},
    workflow_app_facts::{AppFactsRequest, AppFactsResponse, MAX_APPS_PER_REQUEST},
};

/// Control's policy inputs and deletion marker, over the service transport.
/// The caller supplies the apps; Control supplies the facts and the watermark
/// that orders one answer against another.
#[derive(Clone, Debug)]
pub struct ControlAppFacts {
    transport: Transport,
}

impl ControlAppFacts {
    /// Bind the workflow service's own signer and Control origin.
    ///
    /// # Errors
    /// Refuses missing or foreign signers, instance credentials, invalid
    /// origins and empty exchange bounds.
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
                service_issuer(CONTROL_SERVICE_NAME).map_err(|_| Error::InvalidConfig)?,
                options,
            )?,
        })
    }

    /// One consistent read of every named app Control has a row for.
    ///
    /// An empty request is refused rather than exchanged: the answer would be
    /// an empty list with a watermark, which reads to a caller as "none of
    /// these apps exists" without anything having been asked.
    ///
    /// # Errors
    /// Refuses an empty or oversized request, transport failures, Control
    /// errors, and an answer naming an app that was not requested or naming
    /// one twice.
    pub async fn observe(&self, apps: &[AppId]) -> Result<AppFactsResponse, Error> {
        if apps.is_empty() || apps.len() > MAX_APPS_PER_REQUEST {
            return Err(Error::InvalidConfig);
        }
        let request = AppFactsRequest {
            app_ids: apps.to_vec(),
        };
        let response: AppFactsResponse = self
            .transport
            .post(endpoints::CONTROL_APP_FACTS, &request)
            .await?;
        // An answer may omit an app Control has no row for, but it may never
        // ADD one or repeat one. A repeat would let the last copy decide which
        // facts a caller indexing by id ends up holding, and an addition would
        // let Control answer about an app this caller never asked about.
        if response.apps.len() > apps.len() {
            return Err(Error::InvalidResponse);
        }
        let mut seen: Vec<&AppId> = Vec::with_capacity(response.apps.len());
        for facts in &response.apps {
            if !apps.contains(&facts.app_id) || seen.contains(&&facts.app_id) {
                return Err(Error::InvalidResponse);
            }
            seen.push(&facts.app_id);
        }
        Ok(response)
    }
}
