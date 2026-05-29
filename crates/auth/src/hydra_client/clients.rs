//! `OAuth2` client CRUD via hydra admin API.

use crate::error::{AuthError, Result};
use crate::hydra_client::types::OAuth2Client;
use crate::hydra_client::HydraAdmin;

impl HydraAdmin {
    /// # Errors
    /// [`AuthError::Hydra`] for transport failures, non-2xx responses, or
    /// decode errors from the admin API.
    pub async fn create_client(&self, client: &OAuth2Client) -> Result<OAuth2Client> {
        self.post("/admin/clients", client).await
    }

    /// Returns `Ok(None)` on a 404 (client absent). All other failures map to
    /// [`AuthError::Hydra`].
    ///
    /// # Errors
    /// [`AuthError::Hydra`] for transport failures or non-2xx-non-404
    /// responses.
    pub async fn get_client(&self, client_id: &str) -> Result<Option<OAuth2Client>> {
        match self.get::<OAuth2Client>(&format!("/admin/clients/{client_id}"), &[]).await {
            Ok(c) => Ok(Some(c)),
            Err(AuthError::Hydra(msg)) if msg.contains("→ 404") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// # Errors
    /// [`AuthError::Hydra`] for transport failures, non-2xx responses, or
    /// decode errors.
    pub async fn update_client(&self, client: &OAuth2Client) -> Result<OAuth2Client> {
        let path = format!("/admin/clients/{}", client.client_id);
        self.put(&path, &[], client).await
    }

    /// # Errors
    /// [`AuthError::Hydra`] for transport failures or non-2xx responses.
    pub async fn delete_client(&self, client_id: &str) -> Result<()> {
        self.delete(&format!("/admin/clients/{client_id}"), &[]).await
    }
}
