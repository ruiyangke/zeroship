//! The app-to-database bindings a trusted host resolved and delivered.
//!
//! A worker isolate cannot compose its own binding: the database id and the
//! edge id are control-plane facts, and the role the session narrows to is
//! derived from the edge. Deriving either inside the isolate would name objects
//! no reconciler created, so the isolate READS what the host resolved and
//! refuses when the host resolved nothing.
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
use std::sync::RwLock;

use zeroship_core::database_role::DatabaseCapability;
use zeroship_core::{BindingId, DatabaseId};

use crate::binding::{DatabaseEdge, DbBinding};
use crate::error::DbError;

/// One app's resolved edge, before a deploy token is attached to it.
///
/// The capability is the control-plane spelling,
/// [`zeroship_core::database_role::DatabaseCapability`], and not a second enum
/// local to the data plane: the cluster reconciler composes the capability ROLE
/// from the same value, so two spellings of it would let this store and the
/// cluster disagree about which text names which capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedBinding {
    pub database: DatabaseId,
    pub binding: BindingId,
    pub capability: DatabaseCapability,
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
        f.debug_struct("SuppliedAppBindings")
            .finish_non_exhaustive()
    }
}

impl SuppliedAppBindings {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install one of an app's resolved bindings.
    ///
    /// An edge naming ANOTHER database joins the set, because that is the whole
    /// of plurality. For a database the app already holds, the answer splits on
    /// what actually differs:
    ///
    /// - A DIFFERENT BINDING is refused. Two edges for one database disagree
    ///   about which edge the app has, and installing either under running
    ///   isolates would let one dispatch narrow to a role another dispatch's
    ///   descriptor was never built against. So is the SAME binding at a
    ///   different CAPABILITY: control declares both off one row, so two
    ///   readings that disagree are two readings of something that cannot have
    ///   both values, and choosing either is choosing which host was wrong.
    /// - The SAME BINDING at the SAME CAPABILITY is a no-op, so a second
    ///   isolate for one app does not have to know whether the first already
    ///   asked.
    ///
    /// # Errors
    ///
    /// `invalid_app_binding` on an empty app id, or on an edge naming a
    /// different binding or capability for a database this app already holds --
    /// both say some host resolved something this store cannot reconcile with
    /// what it has.
    pub fn supply(&self, app_id: &str, resolved: ResolvedBinding) -> Result<(), DbError> {
        if app_id.is_empty() {
            return Err(DbError::validation(
                "invalid_app_binding",
                "an app binding needs an app id",
            ));
        }
        let mut apps = self.apps.write().map_err(|_| store_unavailable())?;
        let edges = apps.entry(app_id.to_owned()).or_default();
        let Some(position) = edges
            .iter()
            .position(|current| current.database == resolved.database)
        else {
            edges.push(resolved);
            return Ok(());
        };
        let current = &mut edges[position];
        if current.binding != resolved.binding {
            return Err(DbError::validation(
                "invalid_app_binding",
                "app binding conflicts with the binding already installed",
            ));
        }
        if current.capability != resolved.capability {
            return Err(DbError::validation(
                "invalid_app_binding",
                "app binding names a capability other than the one already installed",
            ));
        }
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
            resolved.capability,
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
                            edge.capability,
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

fn store_unavailable() -> DbError {
    DbError::internal("db: the app binding store is unavailable")
}

/// The edge a resolved binding carries, for hosts that already hold one.
impl From<&DatabaseEdge> for ResolvedBinding {
    fn from(edge: &DatabaseEdge) -> Self {
        Self {
            database: edge.database().clone(),
            binding: edge.binding().clone(),
            capability: edge.database_capability(),
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
            capability: DatabaseCapability::ReadWrite,
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
    /// DATABASE is refused, and the refusal leaves the installed role standing.
    ///
    /// The assertion is on the composed ROLE and not only on the count: the
    /// role is what `SET LOCAL ROLE` sends, so a store that had accepted the
    /// second edge in place would leave every session narrowing to a role this
    /// app was never granted.
    #[test]
    fn a_conflicting_edge_is_refused_and_an_equal_one_is_not() {
        let store = SuppliedAppBindings::new();
        let edge = resolved();
        store.supply("app_x", edge.clone()).expect("first supply");
        store
            .supply("app_x", edge.clone())
            .expect("an equal re-supply");
        let installed = store
            .binding_for("app_x", "d1", &edge.database)
            .expect("the resolved edge composes a binding");

        let moved = ResolvedBinding {
            database: edge.database.clone(),
            binding: BindingId::mint(),
            capability: edge.capability,
        };
        assert_ne!(moved.binding, edge.binding, "the control: two mints");
        let error = store
            .supply("app_x", moved)
            .expect_err("a different edge for one database must be refused");
        assert_eq!(error.code(), "invalid_app_binding");
        assert_eq!(store.bindings_for("app_x", "d1").len(), 1);
        assert_eq!(
            store
                .binding_for("app_x", "d1", &edge.database)
                .expect("the refused supply leaves the store serving")
                .session_role(),
            installed.session_role(),
        );
    }

    /// Retiring an app removes every binding.
    #[test]
    fn retiring_an_app_removes_its_bindings() {
        let store = SuppliedAppBindings::new();
        let edge = resolved();
        store.supply("app_x", edge.clone()).expect("supply");
        store.supply("app_x", resolved()).expect("supply a second");
        store.remove_app("app_x").expect("remove");
        assert!(store
            .binding_for("app_x", "deploy_1", &edge.database)
            .is_none());
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

    /// The capability a host resolved reaches the composed binding, and both
    /// values do.
    ///
    /// The store is the only thing between control's response and the value
    /// `env.db` operations consult, so a store that dropped the field would
    /// leave every binding claiming whichever capability `DbBinding` defaulted
    /// to. Both arms, because one of them alone would pass over a store that
    /// answered the same way for everything.
    #[test]
    fn each_supplied_capability_reaches_the_composed_binding() {
        let mut seen = 0;
        for capability in [DatabaseCapability::ReadWrite, DatabaseCapability::ReadOnly] {
            let store = SuppliedAppBindings::new();
            let edge = ResolvedBinding {
                capability,
                ..resolved()
            };
            store.supply("app_x", edge.clone()).expect("supply");

            let bound = store
                .binding_for("app_x", "d1", &edge.database)
                .expect("a supplied app composes a binding");
            assert_eq!(bound.database_capability(), Some(capability));
            assert_eq!(bound.permits_writes(), capability.permits_writes());
            assert_eq!(
                store.bindings_for("app_x", "d1")[0].database_capability(),
                Some(capability),
                "the plural accessor must carry the capability the singular one does"
            );
            seen += 1;
        }
        assert_eq!(seen, 2, "both capabilities must be exercised");
    }

    /// A second reading that disagrees about the CAPABILITY of an edge the app
    /// already holds is refused, and the store keeps what it had.
    ///
    /// Control declares the binding id and the capability off one row, so two
    /// readings that differ cannot both be of that row. Its control is the
    /// equal re-supply above it, which is accepted - without that, a store that
    /// had begun refusing every re-supply would pass this.
    #[test]
    fn a_capability_that_disagrees_with_the_installed_edge_is_refused() {
        let store = SuppliedAppBindings::new();
        let edge = ResolvedBinding {
            capability: DatabaseCapability::ReadWrite,
            ..resolved()
        };
        store.supply("app_x", edge.clone()).expect("first supply");
        store
            .supply("app_x", edge.clone())
            .expect("the control: an equal re-supply is accepted");

        let widened = ResolvedBinding {
            capability: DatabaseCapability::ReadOnly,
            ..edge.clone()
        };
        let error = store
            .supply("app_x", widened)
            .expect_err("one edge cannot hold two capabilities");
        assert_eq!(error.code(), "invalid_app_binding");
        assert_eq!(
            store
                .binding_for("app_x", "d1", &edge.database)
                .expect("the refused supply leaves the store serving")
                .database_capability(),
            Some(DatabaseCapability::ReadWrite),
            "the refusal must leave the installed capability standing"
        );
    }
}
