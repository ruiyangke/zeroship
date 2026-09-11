//! Trusted host contract for a project's column encryption key.

use crate::project_id::ProjectId;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// Delivered only to the host, never to app environments or deploy artifacts.
#[derive(Serialize, Deserialize)]
pub struct ProjectDataKey {
    pub project_id: ProjectId,
    key: [u8; 32],
}

impl ProjectDataKey {
    #[must_use]
    pub const fn new(project_id: ProjectId, key: [u8; 32]) -> Self {
        Self { project_id, key }
    }

    #[must_use]
    pub fn generate(project_id: ProjectId) -> Self {
        use aes_gcm::aead::rand_core::{OsRng, RngCore};
        let mut material = Self::new(project_id, [0; 32]);
        OsRng.fill_bytes(&mut material.key);
        material
    }

    #[must_use]
    pub const fn key(&self) -> &[u8; 32] {
        &self.key
    }

    /// Scrub the HTTP response buffer after decoding host key material.
    ///
    /// # Errors
    /// Returns the decoding error if the response does not match the host contract.
    pub fn from_json(json: String) -> Result<Self, serde_json::Error> {
        serde_json::from_str(&zeroize::Zeroizing::new(json))
    }
}

impl std::fmt::Debug for ProjectDataKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectDataKey")
            .field("project_id", &self.project_id)
            .finish_non_exhaustive()
    }
}

impl Drop for ProjectDataKey {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}
