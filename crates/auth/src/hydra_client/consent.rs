//! Consent challenge admin endpoints.

use crate::error::Result;
use crate::hydra_client::types::{AcceptConsentRequest, ConsentRequest, RedirectResponse, RejectRequest};
use crate::hydra_client::HydraAdmin;

impl HydraAdmin {
    /// # Errors
    /// [`AuthError::Hydra`](crate::error::AuthError::Hydra) for transport
    /// failures, non-2xx responses, or decode errors.
    pub async fn get_consent(&self, challenge: &str) -> Result<ConsentRequest> {
        self.get("/admin/oauth2/auth/requests/consent", &[("consent_challenge", challenge)]).await
    }

    /// # Errors
    /// [`AuthError::Hydra`](crate::error::AuthError::Hydra) for transport
    /// failures, non-2xx responses, or decode errors.
    pub async fn accept_consent(&self, challenge: &str, body: &AcceptConsentRequest) -> Result<RedirectResponse> {
        self.put("/admin/oauth2/auth/requests/consent/accept", &[("consent_challenge", challenge)], body).await
    }

    /// # Errors
    /// [`AuthError::Hydra`](crate::error::AuthError::Hydra) for transport
    /// failures, non-2xx responses, or decode errors.
    pub async fn reject_consent(&self, challenge: &str, body: &RejectRequest) -> Result<RedirectResponse> {
        self.put("/admin/oauth2/auth/requests/consent/reject", &[("consent_challenge", challenge)], body).await
    }
}
