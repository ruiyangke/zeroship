//! Load workflow declarations alongside the normal app executable.

#![expect(
    clippy::future_not_send,
    reason = "bundle reads use the host's compio runtime"
)]

use super::{DeployRegistration, ScheduleCatchUp, ScheduleRegistration};
use crate::validation;
use std::collections::BTreeSet;
use zeroship_bundle::{BlobStore, ExecutableError, LoadedWorker, Manifest};
use zeroship_core::workflow_schedules::ScheduleDescriptor;

/// Canonical creator declarations and their input-free manager projection.
#[derive(Clone, PartialEq, Eq)]
pub struct BundleDeclarations {
    workflows: BTreeSet<String>,
    schedules: Vec<ScheduleRegistration>,
}

impl std::fmt::Debug for BundleDeclarations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BundleDeclarations").finish_non_exhaustive()
    }
}

impl BundleDeclarations {
    /// Parse declarations without loading executable blobs or applying host limits.
    ///
    /// # Errors
    /// Rejects invalid manifests, declaration names, duplicates, missing workflow
    /// declarations, malformed calendars and nonpositive backfill allowances.
    pub fn parse(manifest: &Manifest) -> Result<Self, ExecutableError> {
        manifest
            .validate()
            .map_err(|_| ExecutableError::InvalidManifest)?;
        let declared: Vec<String> = manifest
            .workflows
            .clone()
            .map_or_else(|| Ok(Vec::new()), serde_json::from_value)
            .map_err(|_| ExecutableError::InvalidManifest)?;
        let mut workflows = BTreeSet::new();
        for name in declared {
            validation::workflow_name(&name).map_err(|_| ExecutableError::InvalidManifest)?;
            if !workflows.insert(name) {
                return Err(ExecutableError::InvalidManifest);
            }
        }
        let mut schedules: Vec<ScheduleRegistration> = manifest
            .schedules
            .iter()
            .cloned()
            .map(serde_json::from_value)
            .collect::<Result<_, _>>()
            .map_err(|_| ExecutableError::InvalidManifest)?;
        schedules.sort_by(|left, right| left.name.cmp(&right.name));
        let mut previous = None;
        for schedule in &schedules {
            validation::workflow_name(&schedule.name)
                .map_err(|_| ExecutableError::InvalidManifest)?;
            validation::workflow_name(&schedule.workflow_name)
                .map_err(|_| ExecutableError::InvalidManifest)?;
            if previous == Some(&schedule.name)
                || !workflows.contains(&schedule.workflow_name)
                || matches!(schedule.catch_up, ScheduleCatchUp::Backfill { max: 0 })
            {
                return Err(ExecutableError::InvalidManifest);
            }
            schedule
                .schedule
                .next_after(0, 0)
                .map_err(|_| ExecutableError::InvalidManifest)?;
            previous = Some(&schedule.name);
        }
        Ok(Self {
            workflows,
            schedules,
        })
    }

    #[must_use]
    pub const fn workflows(&self) -> &BTreeSet<String> {
        &self.workflows
    }

    /// Preserve full creator input under the host's deployment identity and hash.
    #[must_use]
    pub fn registration(&self, id: String, hash: String) -> DeployRegistration {
        DeployRegistration {
            id,
            hash,
            workflows: self.workflows.clone(),
            schedules: self.schedules.clone(),
        }
    }

    /// Project only calendar and workflow identities onto manager metadata.
    /// Creator input remains in the ordinary manifest and creator registration.
    #[must_use]
    pub fn manager_schedules(&self) -> Vec<ScheduleDescriptor> {
        self.schedules
            .iter()
            .map(|schedule| ScheduleDescriptor {
                name: schedule.name.clone(),
                workflow_name: schedule.workflow_name.clone(),
                schedule: schedule.schedule.clone(),
                overlap: schedule.overlap,
                catch_up: schedule.catch_up,
            })
            .collect()
    }
}

/// Executable and declarations without an app identity or host credentials.
pub struct BundleExecutable {
    executable: LoadedWorker,
    declarations: BundleDeclarations,
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
        let declarations = BundleDeclarations::parse(manifest)?;
        let metadata = serde_json::to_vec(&(&declarations.workflows, &declarations.schedules))
            .map_err(|_| ExecutableError::InvalidManifest)?;
        let remaining = max_source_bytes
            .checked_sub(metadata.len())
            .ok_or(ExecutableError::TooLarge)?;
        let executable = LoadedWorker::load(manifest, source, remaining).await?;
        Ok(Self {
            executable,
            declarations,
        })
    }

    #[must_use]
    pub const fn executable(&self) -> &LoadedWorker {
        &self.executable
    }

    /// The declarations parsed from this executable's manifest.
    #[must_use]
    pub const fn declarations(&self) -> &BundleDeclarations {
        &self.declarations
    }

    #[must_use]
    pub fn into_executable(self) -> LoadedWorker {
        self.executable
    }

    /// The host chooses deployment identity and supplies the normal manifest hash.
    #[must_use]
    pub fn registration(&self, id: String, hash: String) -> DeployRegistration {
        self.declarations.registration(id, hash)
    }
}

#[cfg(test)]
mod tests;
