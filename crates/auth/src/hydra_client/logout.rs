//! Logout challenge admin endpoints.

use crate::error::Result;
use crate::hydra_client::types::{LogoutRequest, RedirectResponse};
use crate::hydra_client::HydraAdmin;

impl HydraAdmin {
    pub async fn get_logout(&self, challenge: &str) -> Result<LogoutRequest> {
        self.get("/admin/oauth2/auth/requests/logout", &[("logout_challenge", challenge)]).await
    }

    pub async fn accept_logout(&self, challenge: &str) -> Result<RedirectResponse> {
        self.put("/admin/oauth2/auth/requests/logout/accept",
                 &[("logout_challenge", challenge)],
                 &serde_json::json!({})).await
    }
}
