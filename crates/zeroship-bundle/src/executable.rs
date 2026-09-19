//! Executable loading from the normal app manifest and content-addressed blobs.

#![expect(
    clippy::future_not_send,
    reason = "executable I/O stays on its compio thread"
)]

use crate::{sha256_hex, validate_hash_format, BlobError, BlobStore, Manifest};
use compio::io::AsyncReadAtExt;
use std::collections::BTreeMap;
use zeroship_id::DatabaseId;

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

/// Compute the normal deployment identity, preserving manifest extensions and
/// excluding the embedded `deploy_hash` field just as archive ingestion does.
///
/// # Errors
/// Rejects a manifest that is not a JSON object.
pub fn deployment_manifest_hash(bytes: &[u8]) -> Result<String, ExecutableError> {
    let canonical = crate::unpack::canonical_manifest_for_hash(bytes)
        .map_err(|_| ExecutableError::InvalidManifest)?;
    Ok(sha256_hex(&canonical))
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
    if deployment_manifest_hash(bytes)? != expected_hash {
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
#[derive(Clone, PartialEq, Eq)]
pub struct LoadedWorker {
    entry: String,
    modules: BTreeMap<String, String>,
    databases: Vec<LoadedDatabase>,
}

/// One database the deployment declares, with its descriptor blob resolved.
///
/// The label is the creator's own name for the database and reaches the
/// isolate as the member name on `env.databases`; `database_id` is what every
/// server-side map keys on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedDatabase {
    pub label: String,
    pub database_id: DatabaseId,
    pub primary: bool,
    pub schema: serde_json::Value,
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
        let mut databases = Vec::with_capacity(manifest.runtime_descriptor.len());
        for entry in &manifest.runtime_descriptor {
            let bytes = read_blob(source, &entry.hash, &mut remaining).await?;
            databases.push(LoadedDatabase {
                label: entry.label.clone(),
                database_id: entry.database_id.clone(),
                primary: entry.primary,
                schema: serde_json::from_slice(&bytes)
                    .map_err(|_| ExecutableError::InvalidExecutable)?,
            });
        }
        Ok(Self {
            entry: worker.entry.clone(),
            modules,
            databases,
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

    /// Every database this deployment declares, in manifest order.
    #[must_use]
    pub fn databases(&self) -> &[LoadedDatabase] {
        &self.databases
    }

    /// The schema of the app's PRIMARY database, the one `env.db` reaches.
    ///
    /// `Manifest::validate` admits exactly one primary in a non-empty set, so
    /// this is `None` only for an app that declares no database at all.
    #[must_use]
    pub fn primary_schema(&self) -> Option<&serde_json::Value> {
        self.databases
            .iter()
            .find(|database| database.primary)
            .map(|database| &database.schema)
    }

    #[must_use]
    pub fn into_parts(self) -> (String, BTreeMap<String, String>, Vec<LoadedDatabase>) {
        (self.entry, self.modules, self.databases)
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
        .await
        .map_err(|error| match error {
            BlobError::TooLarge => ExecutableError::TooLarge,
            other => ExecutableError::Storage(other),
        })?;
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
