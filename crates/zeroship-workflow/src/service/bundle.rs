//! Resolve a deploy archive into the executable retained by its customer host.

#![expect(
    clippy::future_not_send,
    reason = "bundle reads use the host's compio runtime"
)]

use super::{types::digest, DeployRegistration, ExecutableSnapshot, ScheduleRegistration};
use crate::WorkflowServiceError;
use compio::io::AsyncReadAtExt;
use std::collections::{BTreeMap, BTreeSet};
use zeroship_bundle::{sha256_hex, BlobStore, Manifest};

/// Executable and declarations without an app identity or host credentials.
/// The local content identity excludes packaging timestamps and static assets.
pub struct BundleExecutable {
    snapshot: ExecutableSnapshot,
    workflows: BTreeSet<String>,
    schedules: Vec<ScheduleRegistration>,
    content_hash: String,
}
impl std::fmt::Debug for BundleExecutable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BundleExecutable").finish_non_exhaustive()
    }
}
impl BundleExecutable {
    /// Load verified modules and the runtime descriptor with a source-byte budget.
    /// The host supplies an authorized source store; successful activation copies
    /// this image into customer snapshot storage before admitting work.
    ///
    /// # Errors
    /// Rejects invalid manifests, declarations, missing or corrupt blobs and
    /// sources exceeding the host budget.
    pub async fn load(
        manifest: &Manifest,
        source: &dyn BlobStore,
        max_source_bytes: usize,
    ) -> Result<Self, WorkflowServiceError> {
        manifest.validate().map_err(|_| invalid())?;
        let worker = manifest.worker.as_ref().ok_or_else(invalid)?;
        let workflows: BTreeSet<String> = manifest
            .workflows
            .clone()
            .map_or_else(|| Ok(BTreeSet::new()), serde_json::from_value)
            .map_err(|_| invalid())?;
        let mut schedules: Vec<ScheduleRegistration> = manifest
            .schedules
            .iter()
            .cloned()
            .map(serde_json::from_value)
            .collect::<Result<_, _>>()
            .map_err(|_| invalid())?;
        schedules.sort_by(|left, right| left.name.cmp(&right.name));
        let metadata = serde_json::to_vec(&(&workflows, &schedules, &worker.modules))
            .map_err(|_| invalid())?;
        let mut remaining = max_source_bytes
            .checked_sub(metadata.len())
            .ok_or(WorkflowServiceError::PayloadTooLarge)?;
        let mut modules = BTreeMap::new();
        for (name, hash) in &worker.modules {
            let bytes = read_blob(source, hash, &mut remaining).await?;
            modules.insert(
                name.clone(),
                String::from_utf8(bytes).map_err(|_| invalid())?,
            );
        }
        let descriptor = if let Some(descriptor) = &manifest.runtime_descriptor {
            let bytes = read_blob(source, &descriptor.hash, &mut remaining).await?;
            Some(serde_json::from_slice(&bytes).map_err(|_| invalid())?)
        } else {
            None
        };
        let snapshot = ExecutableSnapshot::new(worker.entry.clone(), modules, descriptor)?;
        let content_hash = digest(&(&snapshot, &workflows, &schedules))?;
        Ok(Self {
            snapshot,
            workflows,
            schedules,
            content_hash,
        })
    }

    /// Stable local deployment identity across rebuilds of the same executable.
    #[must_use]
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    #[must_use]
    pub const fn snapshot(&self) -> &ExecutableSnapshot {
        &self.snapshot
    }

    /// The host chooses deployment identity. Production uses deployment metadata;
    /// local development can use `content_hash` for repeatable activation.
    #[must_use]
    pub fn registration(&self, id: String, hash: String) -> DeployRegistration {
        DeployRegistration {
            id,
            hash,
            workflows: self.workflows.clone(),
            schedules: self.schedules.clone(),
        }
    }
}

async fn read_blob(
    source: &dyn BlobStore,
    hash: &str,
    remaining: &mut usize,
) -> Result<Vec<u8>, WorkflowServiceError> {
    let temporary = tempfile::NamedTempFile::new().map_err(|_| unavailable())?;
    let file = compio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(temporary.path())
        .await
        .map_err(|_| unavailable())?;
    let written = source
        .get_blob_to_file(hash, &file, None, *remaining as u64)
        .await
        .map_err(|_| unavailable())?;
    let length = usize::try_from(written).map_err(|_| WorkflowServiceError::PayloadTooLarge)?;
    *remaining = remaining
        .checked_sub(length)
        .ok_or(WorkflowServiceError::PayloadTooLarge)?;
    let (result, bytes) = file.read_exact_at(vec![0; length], 0).await.into();
    result.map_err(|_| unavailable())?;
    if sha256_hex(&bytes) != hash {
        return Err(unavailable());
    }
    Ok(bytes)
}

fn invalid() -> WorkflowServiceError {
    WorkflowServiceError::InvalidRequest("invalid workflow deploy artifact".into())
}
fn unavailable() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("workflow deploy artifact could not be read".into())
}
