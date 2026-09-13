//! Trusted host policy snapshots. Customer SQL never supplies admission authority.

use super::AppPolicy;
use crate::WorkflowServiceError;
use std::{collections::BTreeMap, sync::RwLock, time::Instant};
use zeroship_core::{app_id::AppId, workflow_coordination::Revision};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Validity {
    Configuration,
    Until(Instant),
}

/// Policy already authorized by the worker's configuration or metadata provider.
/// Cloning a snapshot preserves its deadline; retrying delivery cannot refresh it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicySnapshot {
    revision: Revision,
    policy: AppPolicy,
    validity: Validity,
}
impl PolicySnapshot {
    /// Explicit host configuration has no remote metadata lease to refresh.
    ///
    /// # Errors
    /// Rejects invalid limits.
    pub fn configuration(
        revision: Revision,
        policy: AppPolicy,
    ) -> Result<Self, WorkflowServiceError> {
        policy.validate()?;
        Ok(Self {
            revision,
            policy,
            validity: Validity::Configuration,
        })
    }

    /// The host derives this monotonic deadline from authenticated metadata.
    /// Expired snapshots remain usable for history access and bounded recovery.
    ///
    /// # Errors
    /// Rejects invalid limits.
    pub fn lease(
        revision: Revision,
        policy: AppPolicy,
        valid_until: Instant,
    ) -> Result<Self, WorkflowServiceError> {
        policy.validate()?;
        Ok(Self {
            revision,
            policy,
            validity: Validity::Until(valid_until),
        })
    }

    fn effective(&self) -> AppPolicy {
        let mut policy = self.policy.clone();
        if let Validity::Until(deadline) = self.validity {
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_millis();
            if remaining == 0 {
                policy.admission = false;
                policy.dispatch = false;
                policy.ingress = false;
            } else {
                policy.lease_ms = policy
                    .lease_ms
                    .min(i64::try_from(remaining).unwrap_or(i64::MAX));
            }
        }
        policy
    }
}

/// Host-owned policy state, shared across engine handles on the same worker.
/// Reopening a journal requires the host to supply this authority again; persisted
/// customer state cannot turn admission back on.
#[derive(Debug, Default)]
pub struct HostPolicies {
    entries: RwLock<BTreeMap<AppId, PolicySnapshot>>,
}

/// Authority captured for a journal operation. Refreshing the host snapshot
/// does not extend an operation already waiting on customer storage.
pub(super) struct PolicyAuthority {
    revision: Revision,
    pub(super) deadline: Option<Instant>,
    pub(super) policy: AppPolicy,
}

impl PolicyAuthority {
    pub(super) fn check(
        &self,
        policies: &HostPolicies,
        app: &AppId,
    ) -> Result<(), WorkflowServiceError> {
        if self
            .deadline
            .is_some_and(|deadline| deadline <= Instant::now())
            || policies.authority(app)?.revision != self.revision
        {
            return Err(unavailable());
        }
        Ok(())
    }
}

impl HostPolicies {
    pub(super) fn authority(&self, app: &AppId) -> Result<PolicyAuthority, WorkflowServiceError> {
        let entries = self.entries.read().map_err(|_| unavailable())?;
        let snapshot = entries.get(app).ok_or_else(unavailable)?;
        let deadline = match snapshot.validity {
            Validity::Configuration => None,
            Validity::Until(deadline) if deadline > Instant::now() => Some(deadline),
            Validity::Until(_) => return Err(unavailable()),
        };
        let authority = PolicyAuthority {
            revision: snapshot.revision,
            deadline,
            policy: snapshot.policy.clone(),
        };
        drop(entries);
        Ok(authority)
    }

    /// Candidate discovery includes expired assignments so their leases and
    /// abandoned uploads can still be recovered. Mutation resolves policy again.
    pub(crate) fn app_ids(&self) -> Result<Vec<AppId>, WorkflowServiceError> {
        Ok(self
            .entries
            .read()
            .map_err(|_| unavailable())?
            .keys()
            .cloned()
            .collect())
    }

    pub(crate) fn install(
        &self,
        app: &AppId,
        snapshot: PolicySnapshot,
    ) -> Result<(), WorkflowServiceError> {
        let mut entries = self.entries.write().map_err(|_| unavailable())?;
        if let Some(current) = entries.get(app) {
            if snapshot.revision < current.revision {
                return Err(conflict());
            }
            if snapshot.revision == current.revision {
                if snapshot.policy != current.policy {
                    return Err(conflict());
                }
                match (&current.validity, &snapshot.validity) {
                    (Validity::Configuration, Validity::Configuration) => return Ok(()),
                    (Validity::Until(old), Validity::Until(new)) if new <= old => return Ok(()),
                    (Validity::Until(_), Validity::Until(_)) => {}
                    _ => return Err(conflict()),
                }
            }
        }
        entries.insert(app.clone(), snapshot);
        drop(entries);
        Ok(())
    }

    pub(crate) fn resolve(&self, app: &AppId) -> Result<AppPolicy, WorkflowServiceError> {
        self.entries
            .read()
            .map_err(|_| unavailable())?
            .get(app)
            .map(PolicySnapshot::effective)
            .ok_or(WorkflowServiceError::PermissionDenied)
    }
}

fn unavailable() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("workflow host policy unavailable".into())
}
fn conflict() -> WorkflowServiceError {
    WorkflowServiceError::Conflict("workflow host policy revision conflicts".into())
}
