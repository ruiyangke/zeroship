//! Trusted host policy snapshots. Customer SQL never supplies admission authority.

use super::AppPolicy;
use crate::WorkflowServiceError;
use futures::{
    channel::oneshot,
    future::{BoxFuture, LocalBoxFuture, Shared},
    FutureExt,
};
use std::{
    collections::BTreeMap,
    fmt,
    future::Future,
    sync::{Arc, RwLock},
    time::Instant,
};
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

/// Host-owned capabilities and source-policy high water. Creator storage cannot
/// recreate a binding, renew its authority, or remove a revocation tombstone.
#[derive(Debug, Default)]
pub struct HostPolicies {
    state: RwLock<State>,
}

#[derive(Debug, Default)]
struct State {
    generation: u64,
    entries: BTreeMap<AppId, Entry>,
}

#[derive(Debug)]
struct Entry {
    generation: u64,
    high_water: Option<(Revision, AppPolicy)>,
    current: Option<Current>,
}

#[derive(Debug)]
struct Current {
    snapshot: Option<PolicySnapshot>,
    ticket_sequence: u64,
    ticket: Option<u64>,
    epoch: Epoch,
}

struct Epoch {
    number: u64,
    notify: oneshot::Sender<()>,
    cancelled: Shared<BoxFuture<'static, ()>>,
}

impl fmt::Debug for Epoch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Epoch")
            .field("number", &self.number)
            .finish_non_exhaustive()
    }
}

impl Epoch {
    fn new(number: u64) -> Self {
        let (notify, cancelled) = oneshot::channel();
        Self {
            number,
            notify,
            cancelled: cancelled.map(|_| ()).boxed().shared(),
        }
    }

    // Sending may synchronously wake code which calls back into HostPolicies.
    // Call this only after releasing the registry lock.
    fn invalidate(self) {
        let _ = self.notify.send(());
    }
}

/// An immutable host capability. A clone keeps its original registry, app and
/// generation; it cannot borrow authority from a later replacement.
#[derive(Debug, Clone)]
pub struct PolicyBinding {
    registry: Arc<HostPolicies>,
    app: AppId,
    generation: u64,
}

/// A single metadata response slot. Starting a newer refresh supersedes it.
#[derive(Debug)]
pub struct PolicyRefresh {
    binding: PolicyBinding,
    ticket: u64,
}

impl HostPolicies {
    /// Explicitly replace this app's binding, preserving source revision/content
    /// high water. The replacement starts without installed authority.
    ///
    /// # Errors
    /// Reports unavailable state or exhausted generation identity.
    pub fn bind(self: &Arc<Self>, app: AppId) -> Result<PolicyBinding, WorkflowServiceError> {
        let mut state = self.state.write().map_err(|_| unavailable())?;
        let generation = state.generation.checked_add(1).ok_or_else(unavailable)?;
        let current = Current {
            snapshot: None,
            ticket_sequence: 0,
            ticket: None,
            epoch: Epoch::new(0),
        };
        let entry = state.entries.entry(app.clone()).or_insert_with(|| Entry {
            generation,
            high_water: None,
            current: None,
        });
        let retired = entry.current.replace(current);
        entry.generation = generation;
        state.generation = generation;
        drop(state);
        if let Some(retired) = retired {
            retired.epoch.invalidate();
        }
        Ok(PolicyBinding {
            registry: Arc::clone(self),
            app,
            generation,
        })
    }

    #[cfg(test)]
    pub(crate) fn current_binding(
        self: &Arc<Self>,
        app: &AppId,
    ) -> Result<PolicyBinding, WorkflowServiceError> {
        let state = self.state.read().map_err(|_| unavailable())?;
        let entry = state
            .entries
            .get(app)
            .filter(|entry| entry.current.is_some())
            .ok_or_else(unavailable)?;
        let generation = entry.generation;
        drop(state);
        Ok(PolicyBinding {
            registry: Arc::clone(self),
            app: app.clone(),
            generation,
        })
    }

    /// Expired current snapshots remain discoverable for explicit host recovery.
    /// Uninitialized and revoked bindings supply no maintenance authority.
    pub(crate) fn app_ids(&self) -> Result<Vec<AppId>, WorkflowServiceError> {
        let state = self.state.read().map_err(|_| unavailable())?;
        Ok(state
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry
                    .current
                    .as_ref()
                    .is_some_and(|current| current.snapshot.is_some())
            })
            .map(|(app, _)| app.clone())
            .collect())
    }

    pub(crate) fn resolve(&self, app: &AppId) -> Result<AppPolicy, WorkflowServiceError> {
        let state = self.state.read().map_err(|_| unavailable())?;
        state
            .entries
            .get(app)
            .and_then(|entry| entry.current.as_ref())
            .and_then(|current| current.snapshot.as_ref())
            .map(PolicySnapshot::effective)
            .ok_or(WorkflowServiceError::PermissionDenied)
    }
}

impl PolicyBinding {
    #[must_use]
    pub const fn app_id(&self) -> &AppId {
        &self.app
    }

    pub(crate) fn belongs_to(&self, registry: &Arc<HostPolicies>) -> bool {
        Arc::ptr_eq(&self.registry, registry)
    }

    pub(crate) fn same_binding(&self, other: &Self) -> bool {
        self.belongs_to(&other.registry)
            && self.app == other.app
            && self.generation == other.generation
    }

    fn current<'a>(&self, state: &'a State) -> Result<&'a Current, WorkflowServiceError> {
        state
            .entries
            .get(&self.app)
            .filter(|entry| entry.generation == self.generation)
            .and_then(|entry| entry.current.as_ref())
            .ok_or_else(unavailable)
    }

    /// Reserve a refresh before starting metadata I/O. Existing snapshot authority
    /// is unchanged, while any older response ticket becomes unusable.
    ///
    /// # Errors
    /// Rejects retired bindings and unavailable or exhausted ticket state.
    pub fn begin_refresh(&self) -> Result<PolicyRefresh, WorkflowServiceError> {
        let mut state = self.registry.state.write().map_err(|_| unavailable())?;
        let current = state
            .entries
            .get_mut(&self.app)
            .filter(|entry| entry.generation == self.generation)
            .and_then(|entry| entry.current.as_mut())
            .ok_or_else(unavailable)?;
        let ticket = current
            .ticket_sequence
            .checked_add(1)
            .ok_or_else(unavailable)?;
        current.ticket_sequence = ticket;
        current.ticket = Some(ticket);
        drop(state);
        Ok(PolicyRefresh {
            binding: self.clone(),
            ticket,
        })
    }

    /// Revoke this generation while retaining its source-policy high water.
    /// Repeating its revocation is harmless; a retired generation cannot revoke
    /// its replacement.
    ///
    /// # Errors
    /// Rejects replaced bindings and unavailable state.
    pub fn revoke(&self) -> Result<(), WorkflowServiceError> {
        let mut state = self.registry.state.write().map_err(|_| unavailable())?;
        let entry = state
            .entries
            .get_mut(&self.app)
            .filter(|entry| entry.generation == self.generation)
            .ok_or_else(unavailable)?;
        let retired = entry.current.take();
        drop(state);
        if let Some(retired) = retired {
            retired.epoch.invalidate();
        }
        Ok(())
    }

    pub(crate) fn resolve(&self) -> Result<AppPolicy, WorkflowServiceError> {
        let state = self.registry.state.read().map_err(|_| unavailable())?;
        self.current(&state)?
            .snapshot
            .as_ref()
            .map(PolicySnapshot::effective)
            .ok_or_else(unavailable)
    }

    pub(crate) fn authority(&self) -> Result<PolicyAuthority, WorkflowServiceError> {
        let state = self.registry.state.read().map_err(|_| unavailable())?;
        let current = self.current(&state)?;
        let snapshot = current.snapshot.as_ref().ok_or_else(unavailable)?;
        let deadline = live_deadline(&snapshot.validity)?;
        let authority = PolicyAuthority {
            binding: self.clone(),
            epoch: current.epoch.number,
            cancelled: current.epoch.cancelled.clone(),
            revision: snapshot.revision,
            deadline,
            policy: snapshot.policy.clone(),
        };
        drop(state);
        Ok(authority)
    }
}

impl PolicyRefresh {
    /// Install exactly this refresh response. Source revision and content are
    /// monotonic across replacements; configured and leased modes cannot switch
    /// within a binding. Expired leases may narrow authority but cannot admit work.
    ///
    /// # Errors
    /// Rejects stale/consumed tickets, retired bindings, conflicting policy values
    /// or modes, and unavailable state.
    pub fn install(self, snapshot: PolicySnapshot) -> Result<(), WorkflowServiceError> {
        let mut state = self
            .binding
            .registry
            .state
            .write()
            .map_err(|_| unavailable())?;
        let entry = state
            .entries
            .get_mut(&self.binding.app)
            .filter(|entry| entry.generation == self.binding.generation)
            .ok_or_else(unavailable)?;
        let current = entry.current.as_mut().ok_or_else(unavailable)?;
        if current.ticket != Some(self.ticket) {
            return Err(unavailable());
        }
        current.ticket = None;
        if let Some((revision, policy)) = &entry.high_water {
            if snapshot.revision < *revision
                || (snapshot.revision == *revision && snapshot.policy != *policy)
            {
                return Err(conflict());
            }
        }
        let invalidate = if let Some(previous) = &current.snapshot {
            if matches!(previous.validity, Validity::Configuration)
                != matches!(snapshot.validity, Validity::Configuration)
            {
                return Err(conflict());
            }
            previous.revision != snapshot.revision
                || matches!((&previous.validity, &snapshot.validity),
                (Validity::Until(old), Validity::Until(new)) if new < old)
        } else {
            false
        };
        let retired = if invalidate {
            let number = current
                .epoch
                .number
                .checked_add(1)
                .ok_or_else(unavailable)?;
            Some(std::mem::replace(&mut current.epoch, Epoch::new(number)))
        } else {
            None
        };
        entry.high_water = Some((snapshot.revision, snapshot.policy.clone()));
        current.snapshot = Some(snapshot);
        drop(state);
        if let Some(retired) = retired {
            retired.invalidate();
        }
        Ok(())
    }
}

/// A captured binding/epoch and original deadline. Newer refreshes may authorize
/// new operations, but can never extend or resurrect this authority.
#[derive(Clone)]
pub struct PolicyAuthority {
    binding: PolicyBinding,
    revision: Revision,
    epoch: u64,
    cancelled: Shared<BoxFuture<'static, ()>>,
    pub(super) deadline: Option<Instant>,
    pub(super) policy: AppPolicy,
}

impl PolicyAuthority {
    pub(crate) fn belongs_to(&self, binding: &PolicyBinding) -> bool {
        self.binding.same_binding(binding)
    }

    pub(crate) fn check(&self) -> Result<(), WorkflowServiceError> {
        if self
            .deadline
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            return Err(unavailable());
        }
        let state = self
            .binding
            .registry
            .state
            .read()
            .map_err(|_| unavailable())?;
        let current = self.binding.current(&state)?;
        let snapshot = current.snapshot.as_ref().ok_or_else(unavailable)?;
        if current.epoch.number != self.epoch || snapshot.revision != self.revision {
            return Err(unavailable());
        }
        live_deadline(&snapshot.validity)?;
        if self
            .deadline
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            return Err(unavailable());
        }
        drop(state);
        Ok(())
    }

    pub(crate) fn effective(&self) -> Result<AppPolicy, WorkflowServiceError> {
        self.check()?;
        Ok(PolicySnapshot {
            revision: self.revision,
            policy: self.policy.clone(),
            validity: self
                .deadline
                .map_or(Validity::Configuration, Validity::Until),
        }
        .effective())
    }

    pub(crate) fn cancelled(&self) -> Shared<BoxFuture<'static, ()>> {
        self.cancelled.clone()
    }

    pub(crate) fn run<'a, T: 'a>(
        &'a self,
        operation: impl Future<Output = Result<T, WorkflowServiceError>> + 'a,
    ) -> LocalBoxFuture<'a, Result<T, WorkflowServiceError>> {
        Box::pin(async move {
            self.check()?;
            let cancelled = self.cancelled().fuse();
            let expiry = async {
                if let Some(deadline) = self.deadline {
                    compio::time::sleep(deadline.saturating_duration_since(Instant::now())).await;
                } else {
                    futures::future::pending::<()>().await;
                }
            }
            .fuse();
            let operation = operation.fuse();
            futures::pin_mut!(cancelled, expiry, operation);
            futures::select_biased! {
                () = cancelled => Err(unavailable()),
                () = expiry => Err(unavailable()),
                result = operation => { self.check()?; result },
            }
        })
    }
}

/// Capture before journal I/O, while allowing exact committed receipts to be
/// read when fresh authority was already unavailable at the start.
#[derive(Clone)]
pub(super) struct CapturedPolicy(Result<PolicyAuthority, WorkflowServiceError>);

impl CapturedPolicy {
    pub(super) const fn retained(authority: PolicyAuthority) -> Self {
        Self(Ok(authority))
    }
    pub(super) fn capture(binding: &PolicyBinding) -> Self {
        Self(binding.authority())
    }

    pub(super) fn recheck(&self) -> Result<(), WorkflowServiceError> {
        self.0.as_ref().map_or(Ok(()), PolicyAuthority::check)
    }

    pub(super) fn check(&self) -> Result<(), WorkflowServiceError> {
        self.authority()?.check()
    }

    pub(super) fn authority(&self) -> Result<&PolicyAuthority, WorkflowServiceError> {
        self.0.as_ref().map_err(Clone::clone)
    }

    pub(super) fn run<'a, T: 'a>(
        &'a self,
        operation: impl Future<Output = Result<T, WorkflowServiceError>> + 'a,
    ) -> LocalBoxFuture<'a, Result<T, WorkflowServiceError>> {
        match &self.0 {
            Ok(authority) => authority.run(operation),
            Err(_) => Box::pin(operation),
        }
    }
}

fn live_deadline(validity: &Validity) -> Result<Option<Instant>, WorkflowServiceError> {
    match validity {
        Validity::Configuration => Ok(None),
        Validity::Until(deadline) if *deadline > Instant::now() => Ok(Some(*deadline)),
        Validity::Until(_) => Err(unavailable()),
    }
}

fn unavailable() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("workflow host policy unavailable".into())
}
fn conflict() -> WorkflowServiceError {
    WorkflowServiceError::Conflict("workflow host policy revision conflicts".into())
}

#[cfg(test)]
mod tests;
