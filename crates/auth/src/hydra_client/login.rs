//! Login challenge admin endpoints.

use crate::error::Result;
use crate::hydra_client::types::{AcceptLoginRequest, LoginRequest, RedirectResponse, RejectRequest};
use crate::hydra_client::HydraAdmin;

impl HydraAdmin {
    pub async fn get_login(&self, challenge: &str) -> Result<LoginRequest> {
        self.get("/admin/oauth2/auth/requests/login", &[("login_challenge", challenge)]).await
    }

    pub async fn accept_login(&self, challenge: &str, body: &AcceptLoginRequest) -> Result<RedirectResponse> {
        self.put("/admin/oauth2/auth/requests/login/accept", &[("login_challenge", challenge)], body).await
    }

    pub async fn reject_login(&self, challenge: &str, body: &RejectRequest) -> Result<RedirectResponse> {
        self.put("/admin/oauth2/auth/requests/login/reject", &[("login_challenge", challenge)], body).await
    }
}
