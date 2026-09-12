//! Resolve a deploy archive into the executable retained by its customer host.

#![expect(
    clippy::future_not_send,
    reason = "bundle reads use the host's compio runtime"
)]

use super::{DeployRegistration, ExecutableSnapshot, ScheduleRegistration};
use crate::WorkflowServiceError;
use std::collections::BTreeSet;
use zeroship_bundle::{BlobStore, ExecutableError, LoadedWorker, Manifest};

/// Executable and declarations without an app identity or host credentials.
pub struct BundleExecutable {
    snapshot: ExecutableSnapshot,
    workflows: BTreeSet<String>,
    schedules: Vec<ScheduleRegistration>,
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
        let metadata = serde_json::to_vec(&(&workflows, &schedules)).map_err(|_| invalid())?;
        let remaining = max_source_bytes
            .checked_sub(metadata.len())
            .ok_or(WorkflowServiceError::PayloadTooLarge)?;
        let executable = LoadedWorker::load(manifest, source, remaining)
            .await
            .map_err(|error| match error {
                ExecutableError::TooLarge => WorkflowServiceError::PayloadTooLarge,
                ExecutableError::InvalidManifest
                | ExecutableError::InvalidExecutable
                | ExecutableError::ManifestIdentity => invalid(),
                ExecutableError::Io(_) | ExecutableError::Storage(_) => unavailable(),
            })?;
        let (entry, modules, descriptor) = executable.into_parts();
        let snapshot = ExecutableSnapshot::new(entry, modules, descriptor)?;
        Ok(Self {
            snapshot,
            workflows,
            schedules,
        })
    }

    #[must_use]
    pub const fn snapshot(&self) -> &ExecutableSnapshot {
        &self.snapshot
    }

    /// The host chooses deployment identity and supplies the normal manifest hash.
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

fn invalid() -> WorkflowServiceError {
    WorkflowServiceError::InvalidRequest("invalid workflow deploy artifact".into())
}
fn unavailable() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("workflow deploy artifact could not be read".into())
}
