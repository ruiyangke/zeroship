//! Process-wide lifetime of an app's credentials on this worker.
//!
//! Three things a worker fetches for an app are credentials: the project data
//! key the database service decrypts its columns with, the database bindings
//! its sessions narrow to, and the decrypted environment isolates read. Each
//! lives in a process-wide store - `SuppliedProjectKeys`, `SuppliedAppBindings`
//! and [`SharedEnvs`] - that any thread can supply into, and nothing in those
//! stores knows when the last user of an app is gone.
//!
//! [`AppResidency`] is what knows. Every holder of an app - an HTTP isolate and
//! every request in flight on it, a prepared workflow app and every execution
//! running from it, a reconcile swap - holds a [`Residency`], and dropping the
//! last one withdraws all three under the registry's lock. A worker therefore
//! holds credentials only for apps it currently holds, not for every app it
//! ever served.
//!
//! Two rules make the count mean something:
//!
//! - A holder takes its `Residency` BEFORE it checks whether the app's material
//!   is already supplied. The check is what lets a second holder skip the fetch,
//!   so a residency taken after it could find the material withdrawn by a drop
//!   that landed between the two. Taken first, the count cannot reach zero while
//!   the holder looks.
//! - Material is supplied only by a holder. A refresh that does not hold the app
//!   asks [`AppResidency::reside_held`] instead, which holds only an app
//!   something else already holds, so a refresh racing the last drop cannot
//!   resurrect what that drop withdrew.
//!
//! Withdrawal is not deletion: a deleted app's change-data teardown belongs to
//! `DbLifecycle::deprovision_app`, which the version poller runs.

use crate::sync::SharedEnvs;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};
use zeroship_core::app_id::AppId;
use zeroship_data_orm::{encryption::SuppliedProjectKeys, resolved_bindings::SuppliedAppBindings};

/// The registry of apps something on this worker holds, and the stores their
/// credentials are withdrawn from when nothing does.
#[derive(Clone)]
pub struct AppResidency {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for AppResidency {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppResidency")
            .finish_non_exhaustive()
    }
}

struct Inner {
    /// Holders per app. An app is a key exactly while its count is positive.
    held: Mutex<HashMap<AppId, usize>>,
    /// Absent on a worker with no database service, which supplies no key.
    keys: Option<Arc<SuppliedProjectKeys>>,
    /// Absent on a worker with no database service, which resolves no binding.
    bindings: Option<Arc<SuppliedAppBindings>>,
    envs: SharedEnvs,
}

impl Inner {
    /// The count map. A panic elsewhere while it was held leaves every entry a
    /// whole count - each critical section below is one insert, increment,
    /// decrement or removal - so the map is still the truth and is recovered
    /// rather than leaving every later drop unable to withdraw.
    fn held(&self) -> MutexGuard<'_, HashMap<AppId, usize>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Withdraw every credential this worker holds for `app`, under `held`, the
    /// count lock that saw its count reach zero: no holder can arrive between
    /// that and the material leaving, and the guard is released only after.
    fn withdraw(&self, app: &AppId, held: MutexGuard<'_, HashMap<AppId, usize>>) {
        if let Some(keys) = &self.keys {
            if let Err(error) = keys.remove_app(app.as_str()) {
                tracing::error!(
                    app = app.as_str(),
                    %error,
                    "worker: could not withdraw the app's project key"
                );
            }
        }
        if let Some(bindings) = &self.bindings {
            if let Err(error) = bindings.remove_app(app.as_str()) {
                tracing::error!(
                    app = app.as_str(),
                    %error,
                    "worker: could not withdraw the app's database bindings"
                );
            }
        }
        match self.envs.write() {
            Ok(mut envs) => {
                envs.remove(app);
            }
            Err(error) => {
                tracing::error!(
                    app = app.as_str(),
                    %error,
                    "worker: could not withdraw the app's environment"
                );
            }
        }
        drop(held);
    }
}

impl AppResidency {
    /// A registry over the stores this process supplies credentials into.
    #[must_use]
    pub fn new(
        keys: Option<Arc<SuppliedProjectKeys>>,
        bindings: Option<Arc<SuppliedAppBindings>>,
        envs: SharedEnvs,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                held: Mutex::new(HashMap::new()),
                keys,
                bindings,
                envs,
            }),
        }
    }

    /// Hold `app`. Take this before checking whether its material is supplied.
    #[must_use]
    pub fn reside(&self, app: AppId) -> Residency {
        *self.inner.held().entry(app.clone()).or_default() += 1;
        Residency {
            app,
            registry: self.inner.clone(),
        }
    }

    /// Hold `app` only if something already holds it.
    ///
    /// For a refresh of material a holder supplied: an app nothing holds has
    /// had its material withdrawn, and refreshing it would supply credentials
    /// no holder will ever withdraw.
    #[must_use]
    pub fn reside_held(&self, app: &AppId) -> Option<Residency> {
        *self.inner.held().get_mut(app)? += 1;
        Some(Residency {
            app: app.clone(),
            registry: self.inner.clone(),
        })
    }

    /// How many holders `app` has.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn holders(&self, app: &AppId) -> usize {
        self.inner.held().get(app).copied().unwrap_or(0)
    }
}

/// One holder's claim on an app's credentials. Dropping the last withdraws
/// them.
pub struct Residency {
    app: AppId,
    registry: Arc<Inner>,
}

impl Residency {
    /// The app this residency holds.
    #[must_use]
    pub const fn app_id(&self) -> &AppId {
        &self.app
    }
}

impl std::fmt::Debug for Residency {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Residency")
            .field("app", &self.app.as_str())
            .finish_non_exhaustive()
    }
}

impl Drop for Residency {
    fn drop(&mut self) {
        let mut held = self.registry.held();
        let Some(count) = held.get_mut(&self.app) else {
            return;
        };
        *count -= 1;
        if *count > 0 {
            return;
        }
        held.remove(&self.app);
        self.registry.withdraw(&self.app, held);
    }
}

#[cfg(test)]
mod tests;
