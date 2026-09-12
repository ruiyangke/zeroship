//! Load workflow declarations alongside the normal app executable.

#![expect(
    clippy::future_not_send,
    reason = "bundle reads use the host's compio runtime"
)]

use super::{DeployRegistration, ScheduleRegistration};
use std::collections::BTreeSet;
use zeroship_bundle::{BlobStore, ExecutableError, LoadedWorker, Manifest};

/// Executable and declarations without an app identity or host credentials.
pub struct BundleExecutable {
    executable: LoadedWorker,
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
    /// Persistence remains the normal app manifest and content-addressed blobs.
    ///
    /// # Errors
    /// Rejects invalid manifests, declarations, missing or corrupt blobs and
    /// sources exceeding the host budget.
    pub async fn load(
        manifest: &Manifest,
        source: &dyn BlobStore,
        max_source_bytes: usize,
    ) -> Result<Self, ExecutableError> {
        manifest
            .validate()
            .map_err(|_| ExecutableError::InvalidManifest)?;
        let workflows: BTreeSet<String> = manifest
            .workflows
            .clone()
            .map_or_else(|| Ok(BTreeSet::new()), serde_json::from_value)
            .map_err(|_| ExecutableError::InvalidManifest)?;
        let mut schedules: Vec<ScheduleRegistration> = manifest
            .schedules
            .iter()
            .cloned()
            .map(serde_json::from_value)
            .collect::<Result<_, _>>()
            .map_err(|_| ExecutableError::InvalidManifest)?;
        schedules.sort_by(|left, right| left.name.cmp(&right.name));
        let metadata = serde_json::to_vec(&(&workflows, &schedules))
            .map_err(|_| ExecutableError::InvalidManifest)?;
        let remaining = max_source_bytes
            .checked_sub(metadata.len())
            .ok_or(ExecutableError::TooLarge)?;
        let executable = LoadedWorker::load(manifest, source, remaining).await?;
        Ok(Self {
            executable,
            workflows,
            schedules,
        })
    }

    #[must_use]
    pub const fn executable(&self) -> &LoadedWorker {
        &self.executable
    }

    #[must_use]
    pub fn into_executable(self) -> LoadedWorker {
        self.executable
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
