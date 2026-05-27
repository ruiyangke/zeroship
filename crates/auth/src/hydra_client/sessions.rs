//! Session admin endpoints (sign-out-everywhere).

use crate::error::Result;
use crate::hydra_client::HydraAdmin;

impl HydraAdmin {
    /// Invalidate hydra's login session for the given subject (`typed_id` `usr_…`).
    /// Triggers front/back-channel logout fan-out to RPs.
    ///
    /// # Errors
    /// [`AuthError::Hydra`](crate::error::AuthError::Hydra) for transport
    /// failures or non-2xx responses.
    pub async fn delete_login_sessions(&self, subject: &str) -> Result<()> {
        self.delete("/admin/oauth2/auth/sessions/login", &[("subject", subject)]).await
    }
}
