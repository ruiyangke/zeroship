//! OAuth2 client CRUD via hydra admin API.

use crate::error::{AuthError, Result};
use crate::hydra_client::types::OAuth2Client;
use crate::hydra_client::HydraAdmin;

impl HydraAdmin {
    pub async fn create_client(&self, client: &OAuth2Client) -> Result<OAuth2Client> {
        self.post("/admin/clients", client).await
    }

    pub async fn get_client(&self, client_id: &str) -> Result<Option<OAuth2Client>> {
        match self.get::<OAuth2Client>(&format!("/admin/clients/{client_id}"), &[]).await {
            Ok(c) => Ok(Some(c)),
            Err(AuthError::Hydra(msg)) if msg.contains("→ 404") => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn update_client(&self, client: &OAuth2Client) -> Result<OAuth2Client> {
        let path = format!("/admin/clients/{}", client.client_id);
        self.put(&path, &[], client).await
    }

    pub async fn delete_client(&self, client_id: &str) -> Result<()> {
        self.delete(&format!("/admin/clients/{client_id}"), &[]).await
    }
}
