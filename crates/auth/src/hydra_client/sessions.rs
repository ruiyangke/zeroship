//! Session admin endpoints (sign-out-everywhere).

use crate::error::Result;
use crate::hydra_client::HydraAdmin;

impl HydraAdmin {
    /// Invalidate hydra's login session for the given subject (typed_id usr_…).
    /// Triggers front/back-channel logout fan-out to RPs.
    pub async fn delete_login_sessions(&self, subject: &str) -> Result<()> {
        self.delete("/admin/oauth2/auth/sessions/login", &[("subject", subject)]).await
    }
}
