//! OAuth 2.0 Device Authorization Grant admin endpoints.

use crate::error::Result;
use crate::hydra_client::types::{AcceptDeviceUserCodeRequest, RedirectResponse};
use crate::hydra_client::HydraAdmin;

impl HydraAdmin {
    /// Accept a verified device-flow user code.
    ///
    /// # Errors
    /// [`AuthError::Hydra`](crate::error::AuthError::Hydra) for transport
    /// failures, non-2xx responses, or decode errors.
    pub async fn accept_device_user_code(
        &self,
        device_challenge: &str,
        body: &AcceptDeviceUserCodeRequest,
    ) -> Result<RedirectResponse> {
        self.put(
            "/admin/oauth2/auth/requests/device/accept",
            &[("device_challenge", device_challenge)],
            body,
        )
        .await
    }
}
