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
use zeroship_core::types::LiveBinding;
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

    /// Replace every binding this app holds with the set a host just resolved.
    ///
    /// The ISOLATE-REPLACEMENT path, where [`Self::supply`] is the resolution
    /// path. `supply` refuses an edge that disagrees with the one installed,
    /// and that refusal is what keeps a rebind away from an isolate already
    /// running on the edge before it; here the isolate is being replaced, so
    /// the set it was built from goes with it and the one its successor is
    /// built from takes its place whole. A database the app has stopped
    /// binding leaves, a rebound database follows its new edge, and a narrowed
    /// capability takes effect - none of which `supply` can express.
    ///
    /// The whole set is swapped under ONE write, so no reader ever observes
    /// the app between its old bindings and its new ones. An app resolved to
    /// an EMPTY set holds no bindings at all, the state it was in before any
    /// host resolved one: control serves no live binding for it, and an
    /// `env.db` composed from a withdrawn edge is worse than an absent one.
    ///
    /// # Errors
    ///
    /// `invalid_app_binding` on an empty app id, or on a set naming one
    /// database twice - two edges for one database disagree about which edge
    /// the app has, and this call cannot choose between them.
    /// `app_binding_store_unavailable` when the lock is poisoned.
    pub fn replace_app(&self, app_id: &str, resolved: Vec<ResolvedBinding>) -> Result<(), DbError> {
        if app_id.is_empty() {
            return Err(DbError::validation(
                "invalid_app_binding",
                "an app binding needs an app id",
            ));
        }
        for (position, edge) in resolved.iter().enumerate() {
            if resolved[..position]
                .iter()
                .any(|earlier| earlier.database == edge.database)
            {
                return Err(DbError::validation(
                    "invalid_app_binding",
                    "app bindings name one database twice",
                ));
            }
        }
        {
            let mut apps = self.apps.write().map_err(|_| store_unavailable())?;
            if resolved.is_empty() {
                apps.remove(app_id);
            } else {
                apps.insert(app_id.to_owned(), resolved);
            }
        }
        Ok(())
    }

    /// The edge and capability this store holds for each of an app's databases.
    ///
    /// The comparable projection of the set, and the same shape
    /// `zeroship_core::types::AppVersionInfo::live_bindings` carries: a host
    /// that knows which set control now serves reads this to decide whether the
    /// store already agrees, and re-resolves only when it does not. An app the
    /// store holds nothing for answers with the empty map, which is the same
    /// answer as an app control serves no live binding for - the two are the
    /// same state.
    ///
    /// The EDGE is in the projection because it is what the session role is
    /// derived from, so a database rebound onto a fresh edge at the same
    /// capability is a store that no longer agrees - and a projection that
    /// dropped the edge would report agreement across exactly that move.
    #[must_use]
    pub fn live_bindings_for(
        &self,
        app_id: &str,
    ) -> std::collections::BTreeMap<DatabaseId, LiveBinding> {
        let Ok(apps) = self.apps.read() else {
            return std::collections::BTreeMap::new();
        };
        apps.get(app_id)
            .map(|edges| {
                edges
                    .iter()
                    .map(|edge| {
                        (
                            edge.database.clone(),
                            LiveBinding {
                                binding: edge.binding.clone(),
                                capability: edge.capability,
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
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

    /// The entry [`SuppliedAppBindings::live_bindings_for`] projects one edge
    /// to, keyed by its database.
    fn projected(edge: &ResolvedBinding) -> (DatabaseId, LiveBinding) {
        (
            edge.database.clone(),
            LiveBinding {
                binding: edge.binding.clone(),
                capability: edge.capability,
            },
        )
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

    /// Replacing an app's set installs a gained database, drops a withdrawn
    /// one, and follows a rebound database onto its new edge - the three things
    /// `supply` cannot express.
    ///
    /// Its control is the binding that did NOT move: it must still compose the
    /// same role afterwards, so this cannot pass over a `replace_app` that
    /// simply cleared the app.
    #[test]
    fn replacing_an_apps_set_installs_gains_drops_withdrawals_and_follows_a_rebind() {
        let store = SuppliedAppBindings::new();
        let kept = resolved();
        let withdrawn = resolved();
        let rebound = resolved();
        store.supply("app_x", kept.clone()).expect("supply kept");
        store
            .supply("app_x", withdrawn.clone())
            .expect("supply withdrawn");
        store
            .supply("app_x", rebound.clone())
            .expect("supply rebound");
        let kept_binding = store
            .binding_for("app_x", "d1", &kept.database)
            .expect("the kept edge composes a binding");
        let kept_role = kept_binding.session_role();

        let gained = resolved();
        let moved = ResolvedBinding {
            binding: BindingId::mint(),
            ..rebound.clone()
        };
        assert_ne!(moved.binding, rebound.binding, "the control: two mints");
        store
            .replace_app("app_x", vec![kept.clone(), moved.clone(), gained.clone()])
            .expect("the isolate-replacement path installs the whole set");

        assert_eq!(
            store.live_bindings_for("app_x"),
            std::collections::BTreeMap::from([
                projected(&kept),
                projected(&moved),
                projected(&gained),
            ]),
            "the store holds exactly the set the host resolved, each database at \
             the EDGE the host resolved for it"
        );
        assert!(
            store
                .binding_for("app_x", "d1", &withdrawn.database)
                .is_none(),
            "a withdrawn database has no binding at all"
        );
        assert_eq!(
            store
                .binding_for("app_x", "d1", &gained.database)
                .expect("the gained database composes a binding")
                .database(),
            Some(&gained.database)
        );
        assert_ne!(
            store
                .binding_for("app_x", "d1", &moved.database)
                .expect("the rebound database composes a binding")
                .session_role(),
            DbBinding::to_database(
                "app_x",
                "d1",
                rebound.database.clone(),
                rebound.binding.clone(),
                rebound.capability,
            )
            .expect("the retired edge composes a role")
            .session_role(),
            "a rebound database narrows to the NEW edge's role, which is the \
             thing `supply` refuses rather than follows"
        );
        assert_eq!(
            store
                .binding_for("app_x", "d1", &kept.database)
                .expect("the control: the untouched edge still composes")
                .session_role(),
            kept_role,
        );
    }

    /// An app resolved to the EMPTY set holds nothing, which is the state it
    /// was in before any host resolved a binding for it.
    ///
    /// This is how a withdrawn last binding reaches the store. Its control is
    /// the populated store before the call - without it, this would pass over a
    /// `replace_app` that had never installed anything.
    #[test]
    fn replacing_an_app_with_the_empty_set_unbinds_it() {
        let store = SuppliedAppBindings::new();
        let edge = resolved();
        store.supply("app_x", edge.clone()).expect("supply");
        assert!(store.is_bound("app_x").expect("read the store"));
        assert_eq!(store.live_bindings_for("app_x").len(), 1);

        store
            .replace_app("app_x", Vec::new())
            .expect("control serves no live binding for this app");

        assert!(
            !store.is_bound("app_x").expect("read the store"),
            "an app control serves no live binding for is unresolved here, so \
             the next resolution reads for it again"
        );
        assert!(store.live_bindings_for("app_x").is_empty());
        assert!(store.binding_for("app_x", "d1", &edge.database).is_none());
    }

    /// A set naming one database twice is refused, and the store keeps what it
    /// had: two edges for one database disagree about which edge the app holds
    /// and this call cannot choose between them.
    ///
    /// Its control is the same set with the duplicate removed, which installs.
    #[test]
    fn replacing_an_app_with_one_database_twice_is_refused() {
        let store = SuppliedAppBindings::new();
        let installed = resolved();
        store.supply("app_x", installed.clone()).expect("supply");

        let database = DatabaseId::mint();
        let first = ResolvedBinding {
            database: database.clone(),
            ..resolved()
        };
        let second = ResolvedBinding {
            database,
            ..resolved()
        };
        let error = store
            .replace_app("app_x", vec![first.clone(), second])
            .expect_err("one database cannot hold two edges");
        assert_eq!(error.code(), "invalid_app_binding");
        assert_eq!(
            store.live_bindings_for("app_x"),
            std::collections::BTreeMap::from([projected(&installed)]),
            "the refusal leaves the store serving what it had"
        );

        store
            .replace_app("app_x", vec![first.clone()])
            .expect("the control: the same set without the duplicate installs");
        assert_eq!(
            store.live_bindings_for("app_x"),
            std::collections::BTreeMap::from([projected(&first)])
        );
    }

    /// The projection carries each database's OWN edge and capability, and an
    /// app the store holds nothing for projects the empty map.
    ///
    /// The edge is asserted against the edge that was SUPPLIED, per database: a
    /// projection that carried one app-wide edge, or dropped it, would report a
    /// store that agrees with control across a rebind it does not hold.
    ///
    /// The empty answer is the control: the map is what a host compares against
    /// control's feed, so a projection that answered empty for everything would
    /// leave every comparison reading "control serves none".
    #[test]
    fn the_projection_carries_each_databases_own_edge_and_capability() {
        let store = SuppliedAppBindings::new();
        let writes = ResolvedBinding {
            capability: DatabaseCapability::ReadWrite,
            ..resolved()
        };
        let reads = ResolvedBinding {
            capability: DatabaseCapability::ReadOnly,
            ..resolved()
        };
        assert_ne!(
            writes.binding, reads.binding,
            "the premise: two databases, two edges"
        );
        store.supply("app_x", writes.clone()).expect("supply rw");
        store.supply("app_x", reads.clone()).expect("supply ro");

        assert_eq!(
            store.live_bindings_for("app_x"),
            std::collections::BTreeMap::from([
                (
                    writes.database,
                    LiveBinding {
                        binding: writes.binding,
                        capability: DatabaseCapability::ReadWrite,
                    }
                ),
                (
                    reads.database,
                    LiveBinding {
                        binding: reads.binding,
                        capability: DatabaseCapability::ReadOnly,
                    }
                ),
            ]),
        );
        assert!(store.live_bindings_for("app_absent").is_empty());
    }
}
