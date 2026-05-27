//! JWK set admin endpoints (used by bootstrap + rotation).

use crate::error::{AuthError, Result};
use crate::hydra_client::types::{CreateJsonWebKeySetRequest, JsonWebKeySet};
use crate::hydra_client::HydraAdmin;

impl HydraAdmin {
    pub async fn get_jwks(&self, set: &str) -> Result<Option<JsonWebKeySet>> {
        match self.get::<JsonWebKeySet>(&format!("/admin/keys/{set}"), &[]).await {
            Ok(j) => Ok(Some(j)),
            Err(AuthError::Hydra(msg)) if msg.contains("→ 404") => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn create_jwk(&self, set: &str, alg: &str) -> Result<JsonWebKeySet> {
        let kid = format!("kid_{}", uuid::Uuid::new_v4().simple());
        let body = CreateJsonWebKeySetRequest { alg: alg.to_string(), use_: "sig".into(), kid };
        self.post(&format!("/admin/keys/{set}"), &body).await
    }

    pub async fn delete_jwk(&self, set: &str, kid: &str) -> Result<()> {
        self.delete(&format!("/admin/keys/{set}/{kid}"), &[]).await
    }
}
