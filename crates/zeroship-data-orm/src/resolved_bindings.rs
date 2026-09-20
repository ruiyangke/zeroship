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
///
/// One app holds a SET: an app may bind many databases, and `env.databases`
/// reaches every one of them. The set is keyed on the app id and each member
/// on its DATABASE id, never on the creator's label - two co-resident apps
/// both calling a database `main` would compare equal.
#[derive(Default)]
pub struct SuppliedAppBindings {
    apps: RwLock<HashMap<String, Vec<ResolvedBinding>>>,
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

    /// Install one of an app's resolved bindings.
    ///
    /// Re-supplying the SAME edge is a no-op, so a second isolate for one app
    /// does not have to know whether the first already asked. Supplying a
    /// DIFFERENT edge FOR THE SAME DATABASE is refused: an app whose binding
    /// moved is a new resolution, and silently replacing it under running
    /// isolates would let one dispatch narrow to a role another dispatch's
    /// descriptor was never built against. An edge naming ANOTHER database
    /// joins the set, because that is the whole of plurality.
    ///
    /// # Errors
    ///
    /// `invalid_app_binding` on an empty app id, or on an edge that conflicts
    /// with the one already installed for that database.
    pub fn supply(&self, app_id: &str, resolved: ResolvedBinding) -> Result<(), DbError> {
        if app_id.is_empty() {
            return Err(DbError::validation(
                "invalid_app_binding",
                "an app binding needs an app id",
            ));
        }
        let mut apps = self.apps.write().map_err(|_| store_unavailable())?;
        let edges = apps.entry(app_id.to_owned()).or_default();
        match edges
            .iter()
            .find(|current| current.database == resolved.database)
        {
            Some(current) if current == &resolved => Ok(()),
            Some(_) => Err(DbError::validation(
                "invalid_app_binding",
                "app binding conflicts with the binding already installed",
            )),
            None => {
                edges.push(resolved);
                Ok(())
            }
        }
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

    /// Compose the binding one isolate of `app_id` runs under for ONE
    /// database.
    ///
    /// `None` when the host resolved nothing for that database, which is the
    /// fail-closed direction: the handle is absent rather than present and
    /// doomed.
    #[must_use]
    pub fn binding_for(
        &self,
        app_id: &str,
        deploy_token: &str,
        database: &DatabaseId,
    ) -> Option<DbBinding> {
        let resolved = self
            .apps
            .read()
            .ok()?
            .get(app_id)?
            .iter()
            .find(|edge| &edge.database == database)
            .cloned()?;
        DbBinding::to_database(
            app_id,
            deploy_token,
            resolved.database,
            resolved.binding,
            resolved.epoch,
        )
        .ok()
    }

    /// Every binding the host resolved for `app_id`, in supply order.
    #[must_use]
    pub fn bindings_for(&self, app_id: &str, deploy_token: &str) -> Vec<DbBinding> {
        let Ok(apps) = self.apps.read() else {
            return Vec::new();
        };
        apps.get(app_id)
            .map(|edges| {
                edges
                    .iter()
                    .filter_map(|edge| {
                        DbBinding::to_database(
                            app_id,
                            deploy_token,
                            edge.database.clone(),
                            edge.binding.clone(),
                            edge.epoch,
                        )
                        .ok()
                    })
                    .collect()
            })
            .unwrap_or_default()
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
    pub fn binding_for(
        &self,
        app_id: &str,
        deploy_token: &str,
        database: &DatabaseId,
    ) -> Option<DbBinding> {
        self.0.binding_for(app_id, deploy_token, database)
    }

    #[must_use]
    pub fn bindings_for(&self, app_id: &str, deploy_token: &str) -> Vec<DbBinding> {
        self.0.bindings_for(app_id, deploy_token)
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

    /// A resolved app composes the binding its sessions narrow with, for the
    /// database asked for.
    #[test]
    fn a_supplied_app_composes_its_binding() {
        let store = SuppliedAppBindings::new();
        let edge = resolved();
        store.supply("app_x", edge.clone()).expect("supply");

        let binding = store
            .binding_for("app_x", "deploy_1", &edge.database)
            .expect("a supplied app composes a binding");
        assert_eq!(binding.database(), Some(&edge.database));
        assert_eq!(binding.schema_epoch(), Some(edge.epoch));
        assert_eq!(binding.deploy_token(), "deploy_1");
    }

    /// An app binds MANY databases, and each composes its own binding. This is
    /// the whole of plurality on this surface: a store that kept one edge per
    /// app would answer the second lookup with the first's role.
    #[test]
    fn an_app_holds_one_binding_per_database_it_binds() {
        let store = SuppliedAppBindings::new();
        let main = resolved();
        let analytics = resolved();
        assert_ne!(main.database, analytics.database);
        store.supply("app_x", main.clone()).expect("supply main");
        store
            .supply("app_x", analytics.clone())
            .expect("supply analytics");

        let bound_main = store
            .binding_for("app_x", "d1", &main.database)
            .expect("main");
        let bound_analytics = store
            .binding_for("app_x", "d1", &analytics.database)
            .expect("analytics");
        assert_eq!(bound_main.database(), Some(&main.database));
        assert_eq!(bound_analytics.database(), Some(&analytics.database));
        assert_ne!(
            bound_main.session_role(),
            bound_analytics.session_role(),
            "each database narrows to its own binding role"
        );
        assert_eq!(store.bindings_for("app_x", "d1").len(), 2);
    }

    /// An unresolved app, and a database the app does not bind, both have no
    /// binding, and that is the whole refusal.
    ///
    /// Its control is the resolved app: without one, a store that answered
    /// `None` for everything would pass.
    #[test]
    fn an_unresolved_app_or_database_has_no_binding() {
        let store = SuppliedAppBindings::new();
        let edge = resolved();
        assert!(store
            .binding_for("app_absent", "deploy_1", &edge.database)
            .is_none());
        assert!(!store.is_bound("app_absent").expect("read the store"));

        store.supply("app_present", edge.clone()).expect("supply");
        assert!(store
            .binding_for("app_present", "deploy_1", &edge.database)
            .is_some());
        assert!(
            store
                .binding_for("app_present", "deploy_1", &DatabaseId::mint())
                .is_none(),
            "a database this app does not bind has no binding"
        );
    }

    /// Re-supplying the same edge is idempotent; a different edge FOR THE SAME
    /// DATABASE is refused.
    #[test]
    fn a_conflicting_edge_is_refused_and_an_equal_one_is_not() {
        let store = SuppliedAppBindings::new();
        let edge = resolved();
        store.supply("app_x", edge.clone()).expect("first supply");
        store.supply("app_x", edge.clone()).expect("an equal re-supply");

        let moved = ResolvedBinding {
            database: edge.database.clone(),
            binding: BindingId::mint(),
            epoch: edge.epoch,
        };
        let error = store
            .supply("app_x", moved)
            .expect_err("a different edge for one database must be refused");
        assert_eq!(error.code(), "invalid_app_binding");
        assert_eq!(store.bindings_for("app_x", "d1").len(), 1);
    }

    /// Retiring an app removes every binding, so a redeploy resolves afresh.
    #[test]
    fn retiring_an_app_removes_its_bindings() {
        let store = SuppliedAppBindings::new();
        let edge = resolved();
        store.supply("app_x", edge.clone()).expect("supply");
        store.supply("app_x", resolved()).expect("supply a second");
        store.remove_app("app_x").expect("remove");
        assert!(store.binding_for("app_x", "deploy_1", &edge.database).is_none());
        assert!(store.bindings_for("app_x", "deploy_1").is_empty());
    }

    /// Two deploys of one app share the resolved edge and differ only in the
    /// deploy token, which is what the descriptor caches key on.
    #[test]
    fn two_deploys_share_the_edge_and_differ_in_the_token() {
        let store = SuppliedAppBindings::new();
        let edge = resolved();
        store.supply("app_x", edge.clone()).expect("supply");
        let first = store
            .binding_for("app_x", "d1", &edge.database)
            .expect("first deploy");
        let second = store
            .binding_for("app_x", "d2", &edge.database)
            .expect("second deploy");
        assert_eq!(first.session_role(), second.session_role());
        assert_eq!(first.route(), second.route());
        assert_ne!(first.deploy_token(), second.deploy_token());
    }
}
