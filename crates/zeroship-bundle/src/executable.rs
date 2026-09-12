//! Executable loading from the normal app manifest and content-addressed blobs.

#![expect(
    clippy::future_not_send,
    reason = "executable I/O stays on its compio thread"
)]

use crate::{sha256_hex, validate_hash_format, BlobError, BlobStore, Manifest};
use compio::io::AsyncReadAtExt;
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum ExecutableError {
    #[error("invalid app deployment manifest")]
    InvalidManifest,
    #[error("app deployment manifest does not match its identity")]
    ManifestIdentity,
    #[error("invalid app executable or runtime descriptor")]
    InvalidExecutable,
    #[error("app executable exceeds the source budget")]
    TooLarge,
    #[error("app executable storage: {0}")]
    Storage(#[from] BlobError),
    #[error("app executable read: {0}")]
    Io(#[from] std::io::Error),
}

/// Verify the stored manifest against the deployment identity selected by its host.
/// Hash raw JSON so field presence and extension metadata retain ingest semantics.
///
/// # Errors
/// Rejects invalid JSON, manifest contracts and mismatched deployment identities.
pub fn verify_deployment_manifest(
    bytes: &[u8],
    expected_hash: &str,
) -> Result<Manifest, ExecutableError> {
    if !validate_hash_format(expected_hash) {
        return Err(ExecutableError::ManifestIdentity);
    }
    let canonical = crate::unpack::canonical_manifest_for_hash(bytes)
        .map_err(|_| ExecutableError::InvalidManifest)?;
    if sha256_hex(&canonical) != expected_hash {
        return Err(ExecutableError::ManifestIdentity);
    }
    let manifest: Manifest =
        serde_json::from_slice(bytes).map_err(|_| ExecutableError::InvalidManifest)?;
    if manifest.deploy_hash.as_deref() != Some(expected_hash) {
        return Err(ExecutableError::ManifestIdentity);
    }
    manifest
        .validate()
        .map_err(|_| ExecutableError::InvalidManifest)?;
    Ok(manifest)
}

/// An in-memory module graph loaded from an app deployment, without runtime secrets.
/// Persistence remains the normal manifest and blob store.
pub struct LoadedWorker {
    entry: String,
    modules: BTreeMap<String, String>,
    runtime_descriptor: Option<serde_json::Value>,
}
impl std::fmt::Debug for LoadedWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedWorker").finish_non_exhaustive()
    }
}
impl LoadedWorker {
    /// Read verified worker modules and the runtime descriptor within a shared budget.
    ///
    /// # Errors
    /// Rejects invalid module specifiers, missing or corrupt blobs, invalid text
    /// and descriptors, and executable data exceeding the host budget.
    pub async fn load(
        manifest: &Manifest,
        source: &dyn BlobStore,
        max_source_bytes: usize,
    ) -> Result<Self, ExecutableError> {
        manifest
            .validate()
            .map_err(|_| ExecutableError::InvalidManifest)?;
        let worker = manifest
            .worker
            .as_ref()
            .ok_or(ExecutableError::InvalidExecutable)?;
        if worker.modules.keys().any(|name| {
            name.is_empty()
                || name.starts_with('/')
                || name.contains(['\\', '\0', ':'])
                || name.split('/').any(|part| matches!(part, "" | "." | ".."))
        }) {
            return Err(ExecutableError::InvalidExecutable);
        }
        let metadata =
            serde_json::to_vec(worker).map_err(|_| ExecutableError::InvalidExecutable)?;
        let mut remaining = max_source_bytes
            .checked_sub(metadata.len())
            .ok_or(ExecutableError::TooLarge)?;
        let mut modules = BTreeMap::new();
        for (name, hash) in &worker.modules {
            let bytes = read_blob(source, hash, &mut remaining).await?;
            modules.insert(
                name.clone(),
                String::from_utf8(bytes).map_err(|_| ExecutableError::InvalidExecutable)?,
            );
        }
        let runtime_descriptor = if let Some(descriptor) = &manifest.runtime_descriptor {
            let bytes = read_blob(source, &descriptor.hash, &mut remaining).await?;
            Some(serde_json::from_slice(&bytes).map_err(|_| ExecutableError::InvalidExecutable)?)
        } else {
            None
        };
        Ok(Self {
            entry: worker.entry.clone(),
            modules,
            runtime_descriptor,
        })
    }

    #[must_use]
    pub fn entry(&self) -> &str {
        &self.entry
    }

    #[must_use]
    pub const fn modules(&self) -> &BTreeMap<String, String> {
        &self.modules
    }

    #[must_use]
    pub const fn runtime_descriptor(&self) -> Option<&serde_json::Value> {
        self.runtime_descriptor.as_ref()
    }

    #[must_use]
    pub fn into_parts(self) -> (String, BTreeMap<String, String>, Option<serde_json::Value>) {
        (self.entry, self.modules, self.runtime_descriptor)
    }
}

async fn read_blob(
    source: &dyn BlobStore,
    hash: &str,
    remaining: &mut usize,
) -> Result<Vec<u8>, ExecutableError> {
    let temporary = tempfile::NamedTempFile::new()?;
    let file = compio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(temporary.path())
        .await?;
    let written = source
        .get_blob_to_file(hash, &file, None, *remaining as u64)
        .await?;
    let length = usize::try_from(written).map_err(|_| ExecutableError::TooLarge)?;
    *remaining = remaining
        .checked_sub(length)
        .ok_or(ExecutableError::TooLarge)?;
    let (result, bytes) = file.read_exact_at(vec![0; length], 0).await.into();
    result?;
    let actual = sha256_hex(&bytes);
    if actual != hash {
        return Err(BlobError::HashMismatch {
            expected: hash.into(),
            got: actual,
        }
        .into());
    }
    Ok(bytes)
}
