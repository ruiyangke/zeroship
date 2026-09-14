//! Deploy command values: the retried identity and binding, the verified
//! deployment it selects, and the immutable result an exact retry receives.

use serde::{Deserialize, Serialize};
use zeroship_core::{
    workflow_coordination::Revision,
    workflow_jobs::DeploymentId,
    workflow_schedules::{RegisterSchedules, ScheduleDescriptor},
    AppId, DeployCommandId, UserId,
};

/// The only artifact media type a deploy accepts, in its normalized spelling.
pub const ZSHIP_CONTENT_TYPE: &str = "application/x-zship";

/// The operation a deploy command receipt records.
pub const DEPLOY_OPERATION: &str = "deploy";

/// Normalize a request content type to the spelling receipts bind.
///
/// Anything other than a `.zship` upload is `None`. Parameters such as a
/// charset do not change the artifact, so they are not part of the binding.
#[must_use]
pub fn normalize_content_type(value: &str) -> Option<&'static str> {
    value
        .split(';')
        .next()
        .map(str::trim)
        .filter(|primary| primary.eq_ignore_ascii_case(ZSHIP_CONTENT_TYPE))
        .map(|_| ZSHIP_CONTENT_TYPE)
}

/// Everything an exact retry must repeat. The archive digest is computed by
/// Control over the bytes it consumed, never taken from the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandBinding {
    pub id: DeployCommandId,
    pub app: AppId,
    pub actor: UserId,
    pub content_type: &'static str,
    pub archive_sha256: String,
}

/// A deploy the catalog may accept: its binding, the verified deployment and
/// the blob counters of this first ingest.
#[derive(Debug, Clone)]
pub struct DeployCommand {
    pub binding: CommandBinding,
    pub deployment: VerifiedDeployment,
    pub blobs_uploaded: usize,
    pub blobs_deduped: usize,
}

/// Why a manifest cannot become a publishable deployment.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeploymentRejected {
    #[error("the manifest does not match its deployment hash")]
    Manifest,
    #[error("invalid workflow schedule declarations: {0}")]
    Schedules(String),
}

/// A manifest verified against its content hash.
///
/// It carries the runtime descriptor schema admission compares and the
/// allowlisted schedule projection the manager receives. Static schedule input
/// and unknown fields stay in the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedDeployment {
    hash: String,
    manifest_json: String,
    descriptor_sha256: Option<String>,
    schedules: Vec<ScheduleDescriptor>,
}

impl VerifiedDeployment {
    /// Verify the stored manifest bytes against their hash and project their
    /// schedules with the creator engine's declaration parser, then check the
    /// projection against the manager's schedule bounds.
    ///
    /// # Errors
    /// Refuses a manifest whose hash, contract or declarations are invalid, and
    /// a projection the manager would refuse.
    pub fn verify(manifest_json: String, hash: String) -> Result<Self, DeploymentRejected> {
        let manifest = zeroship_bundle::verify_deployment_manifest(manifest_json.as_bytes(), &hash)
            .map_err(|_| DeploymentRejected::Manifest)?;
        let schedules = zeroship_workflow::service::BundleDeclarations::parse(&manifest)
            .map_err(|_| {
                DeploymentRejected::Schedules(
                    "schedules must name declared workflows, unique schedule names and valid \
                     calendars"
                        .into(),
                )
            })?
            .manager_schedules();
        zeroship_workflow_manager::scheduling::Options::default()
            .validate(&schedules)
            .map_err(|error| DeploymentRejected::Schedules(schedule_refusal(error)))?;
        Ok(Self {
            hash,
            manifest_json,
            descriptor_sha256: manifest.runtime_descriptor.map(|entry| entry.hash),
            schedules,
        })
    }

    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    #[must_use]
    pub fn manifest_json(&self) -> &str {
        &self.manifest_json
    }

    #[must_use]
    pub fn descriptor_sha256(&self) -> Option<&str> {
        self.descriptor_sha256.as_deref()
    }

    #[must_use]
    pub fn schedules(&self) -> &[ScheduleDescriptor] {
        &self.schedules
    }

    /// The exact preparation request the publisher sends for this deployment.
    #[must_use]
    pub fn registration(&self, app: &AppId, deployment: &DeploymentId) -> RegisterSchedules {
        RegisterSchedules {
            app_id: app.clone(),
            deployment_id: deployment.clone(),
            schedules: self.schedules.clone(),
        }
    }
}

fn schedule_refusal(error: zeroship_workflow_manager::Error) -> String {
    match error {
        zeroship_workflow_manager::Error::Capacity => {
            "too many schedules, or a backfill allowance above the platform bound".into()
        }
        zeroship_workflow_manager::Error::Conflict => "schedule names must be unique".into(),
        _ => "schedule names, intervals and calendars must be valid".into(),
    }
}

/// The immutable acceptance of one deploy command. An exact retry receives it
/// unchanged, including the blob counters of the first ingest.
///
/// `lifecycle_revision` names the activation this deploy committed for
/// publication; `None` means the app was archived and the deploy only staged
/// its code. The manager acknowledges publication asynchronously, so no
/// synchronization state appears here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceResult {
    pub command_id: DeployCommandId,
    pub deploy_id: DeploymentId,
    pub deploy_hash: String,
    pub blobs_uploaded: usize,
    pub blobs_deduped: usize,
    pub lifecycle_revision: Option<Revision>,
}

/// A command's outcome: a first acceptance or the replay of an earlier one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acceptance {
    Accepted(AcceptanceResult),
    Replayed(AcceptanceResult),
}

impl Acceptance {
    #[must_use]
    pub const fn result(&self) -> &AcceptanceResult {
        match self {
            Self::Accepted(result) | Self::Replayed(result) => result,
        }
    }

    #[must_use]
    pub const fn replayed(&self) -> bool {
        matches!(self, Self::Replayed(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_normalization_accepts_only_the_zship_media_type() {
        for accepted in [
            "application/x-zship",
            "Application/X-Zship",
            " application/x-zship ; charset=utf-8",
        ] {
            assert_eq!(normalize_content_type(accepted), Some(ZSHIP_CONTENT_TYPE));
        }
        for refused in ["", "application/json", "application/x-zship2", "text/plain"] {
            assert_eq!(normalize_content_type(refused), None, "{refused}");
        }
    }
}
