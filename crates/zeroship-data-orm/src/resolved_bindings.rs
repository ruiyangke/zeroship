//! The app-to-database bindings a trusted host resolved and delivered.
//!
//! A worker isolate cannot compose its own binding: the database id, the edge
//! id and the schema epoch are control-plane facts, and the role the session
//! narrows to is derived from two of them. Deriving any of them inside the
//! isolate would name objects no reconciler created, so the isolate READS what
//! the host resolved and refuses when the host resolved nothing.
//!
//! # Why this is not the environment map
//!
//! `crates/zeroship-worker/src/cache.rs` states the rule and a live test binds
//! it: an identifier naming a resource shared with another tenant - a database
//! id once databases are shared - must never enter the app's env map, because
//! two apps under one actor that read equal values have confirmed
//! co-residency. This store is native, process-wide and never rendered into
//! creator-visible state.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use zeroship_core::{BindingId, DatabaseId};

use crate::binding::{DbBinding, DatabaseEdge};
use crate::error::DbError;

/// One app's resolved edge, before a deploy token is attached to it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedBinding {
    pub database: DatabaseId,
    pub binding: BindingId,
    pub epoch: u32,
}

/// App bindings supplied through the trusted Rust host boundary.
#[derive(Default)]
pub struct SuppliedAppBindings {
    apps: RwLock<HashMap<String, ResolvedBinding>>,
}

impl std::fmt::Debug for SuppliedAppBindings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SuppliedAppBindings").finish_non_exhaustive()
    }
}

impl SuppliedAppBindings {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install an app's resolved binding.
    ///
    /// Re-supplying the SAME edge is a no-op, so a second isolate for one app
    /// does not have to know whether the first already asked. Supplying a
    /// DIFFERENT edge is refused: an app whose binding moved is a new
    /// resolution, and silently replacing it under running isolates would let
    /// one dispatch narrow to a role another dispatch's descriptor was never
    /// built against.
    ///
    /// # Errors
    ///
    /// `invalid_app_binding` on an empty app id, or on an edge that conflicts
    /// with the one already installed.
    pub fn supply(&self, app_id: &str, resolved: ResolvedBinding) -> Result<(), DbError> {
        let mut apps = self.apps.write().map_err(|_| store_unavailable())?;
        if app_id.is_empty() || apps.get(app_id).is_some_and(|current| current != &resolved) {
            return Err(DbError::validation(
                "invalid_app_binding",
                "app binding conflicts with the binding already installed",
            ));
        }
        apps.insert(app_id.to_owned(), resolved);
        Ok(())
    }

    /// Whether this app's binding has been resolved.
    ///
    /// # Errors
    ///
    /// `app_binding_store_unavailable` when the lock is poisoned.
    pub fn is_bound(&self, app_id: &str) -> Result<bool, DbError> {
        Ok(self
            .apps
            .read()
            .map_err(|_| store_unavailable())?
            .contains_key(app_id))
    }

    /// Compose the binding one isolate of `app_id` runs under.
    ///
    /// `None` when the host resolved nothing, which is the fail-closed
    /// direction: `env.db` is absent rather than present and doomed.
    #[must_use]
    pub fn binding_for(&self, app_id: &str, deploy_token: &str) -> Option<DbBinding> {
        let resolved = self.apps.read().ok()?.get(app_id).cloned()?;
        DbBinding::to_database(
            app_id,
            deploy_token,
            resolved.database,
            resolved.binding,
            resolved.epoch,
        )
        .ok()
    }

    /// Retire a deleted app's binding.
    ///
    /// # Errors
    ///
    /// `app_binding_store_unavailable` when the lock is poisoned.
    pub fn remove_app(&self, app_id: &str) -> Result<(), DbError> {
        self.apps
            .write()
            .map_err(|_| store_unavailable())?
            .remove(app_id);
        Ok(())
    }
}

/// A local handle to the bindings the host supplied. The default has none.
#[derive(Clone, Debug, Default)]
pub struct AppBindingSource(Arc<SuppliedAppBindings>);

impl AppBindingSource {
    #[must_use]
    pub fn unavailable() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn supplied(bindings: Arc<SuppliedAppBindings>) -> Self {
        Self(bindings)
    }

    #[must_use]
    pub fn binding_for(&self, app_id: &str, deploy_token: &str) -> Option<DbBinding> {
        self.0.binding_for(app_id, deploy_token)
    }
}

fn store_unavailable() -> DbError {
    DbError::internal("db: the app binding store is unavailable")
}

/// The edge a resolved binding carries, for hosts that already hold one.
impl From<&DatabaseEdge> for ResolvedBinding {
    fn from(edge: &DatabaseEdge) -> Self {
        Self {
            database: edge.database().clone(),
            binding: edge.binding().clone(),
            epoch: edge.epoch(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved() -> ResolvedBinding {
        ResolvedBinding {
            database: DatabaseId::mint(),
            binding: BindingId::mint(),
            epoch: 3,
        }
    }

    /// A resolved app composes the binding its sessions narrow with.
    #[test]
    fn a_supplied_app_composes_its_binding() {
        let store = SuppliedAppBindings::new();
        let edge = resolved();
        store.supply("app_x", edge.clone()).expect("supply");

        let binding = store
            .binding_for("app_x", "deploy_1")
            .expect("a supplied app composes a binding");
        assert_eq!(binding.database(), Some(&edge.database));
        assert_eq!(binding.schema_epoch(), Some(edge.epoch));
        assert_eq!(binding.deploy_token(), "deploy_1");
    }

    /// An unresolved app has no binding, and that is the whole refusal.
    ///
    /// Its control is the resolved app above: without one, a store that
    /// answered `None` for everything would pass.
    #[test]
    fn an_unresolved_app_has_no_binding() {
        let store = SuppliedAppBindings::new();
        assert!(store.binding_for("app_absent", "deploy_1").is_none());
        assert!(!store.is_bound("app_absent").expect("read the store"));

        store.supply("app_present", resolved()).expect("supply");
        assert!(store.binding_for("app_present", "deploy_1").is_some());
    }

    /// Re-supplying the same edge is idempotent; a different edge is refused.
    #[test]
    fn a_conflicting_edge_is_refused_and_an_equal_one_is_not() {
        let store = SuppliedAppBindings::new();
        let edge = resolved();
        store.supply("app_x", edge.clone()).expect("first supply");
        store.supply("app_x", edge).expect("an equal re-supply");

        let error = store
            .supply("app_x", resolved())
            .expect_err("a different edge must be refused");
        assert_eq!(error.code(), "invalid_app_binding");
    }

    /// Retiring an app removes its binding, so a redeploy resolves afresh.
    #[test]
    fn retiring_an_app_removes_its_binding() {
        let store = SuppliedAppBindings::new();
        store.supply("app_x", resolved()).expect("supply");
        store.remove_app("app_x").expect("remove");
        assert!(store.binding_for("app_x", "deploy_1").is_none());
    }

    /// Two deploys of one app share the resolved edge and differ only in the
    /// deploy token, which is what the descriptor caches key on.
    #[test]
    fn two_deploys_share_the_edge_and_differ_in_the_token() {
        let store = SuppliedAppBindings::new();
        store.supply("app_x", resolved()).expect("supply");
        let first = store.binding_for("app_x", "d1").expect("first deploy");
        let second = store.binding_for("app_x", "d2").expect("second deploy");
        assert_eq!(first.session_role(), second.session_role());
        assert_eq!(first.route(), second.route());
        assert_ne!(first.deploy_token(), second.deploy_token());
    }
}
