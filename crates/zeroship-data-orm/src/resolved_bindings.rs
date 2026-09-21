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

/// [`SuppliedAppBindings::supply`] was handed the edge it already holds at an
/// EARLIER schema epoch.
///
/// A named code and not a spelling each caller repeats, because the host that
/// re-resolves bindings has to tell this refusal apart from the others: this
/// one leaves the store holding a LATER reading of the same edge and is the
/// ordinary outcome of two resolutions racing, where every other refusal says
/// the store was handed something it cannot reconcile.
pub const STALE_APP_BINDING: &str = "stale_app_binding";

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
    /// An edge naming ANOTHER database joins the set, because that is the whole
    /// of plurality. For a database the app already holds, the three answers
    /// split on what actually differs:
    ///
    /// - A DIFFERENT BINDING is refused. Two edges for one database disagree
    ///   about which edge the app has, and installing either under running
    ///   isolates would let one dispatch narrow to a role another dispatch's
    ///   descriptor was never built against.
    /// - The SAME BINDING at a HIGHER EPOCH replaces the stored edge. This is
    ///   the same edge advanced by a rotation, not a second one: an apply that
    ///   commits a schema delta mints the binding's role at the next epoch and
    ///   retires the one before the head, so the epoch a host resolved earlier
    ///   names a role the cluster drops on the apply after next. Keeping the
    ///   lower epoch composes that dropped role.
    /// - The SAME BINDING at a LOWER EPOCH is refused, and the store stays at
    ///   the higher one. The epoch is monotone on the cluster, so a response
    ///   that arrives late or out of order carries an older reading of it, and
    ///   following one backwards would walk a live binding onto a role the next
    ///   apply already retired.
    ///
    /// Re-supplying the same binding at the same epoch is a no-op, so a second
    /// isolate for one app does not have to know whether the first already
    /// asked.
    ///
    /// # Errors
    ///
    /// `invalid_app_binding` on an empty app id, or on an edge naming a
    /// different binding for a database this app already holds -- both say some
    /// host resolved something this store cannot reconcile with what it has.
    /// `stale_app_binding` on the same binding at a lower epoch, which is the
    /// ordinary outcome of a race between two resolutions rather than a
    /// disagreement about the edge, and leaves the store serving.
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
        match resolved.epoch.cmp(&current.epoch) {
            std::cmp::Ordering::Greater => {
                current.epoch = resolved.epoch;
                Ok(())
            }
            std::cmp::Ordering::Equal => Ok(()),
            std::cmp::Ordering::Less => Err(DbError::validation(
                STALE_APP_BINDING,
                "app binding names a schema epoch behind the one already installed",
            )),
        }
    }

    /// Replace every binding this app holds with the set a host just resolved.
    ///
    /// The ISOLATE-REPLACEMENT path, where [`Self::supply`] is the resolution
    /// path. `supply` refuses an edge that disagrees with the one installed,
    /// and that refusal is what keeps a rotation away from an isolate already
    /// running on the epoch before it; here the isolate is being replaced, so
    /// the set it was built from goes with it and the one its successor is
    /// built from takes its place whole. A database the app has stopped
    /// binding leaves, a rebound database follows its new edge, and a rotated
    /// epoch advances - none of which `supply` can express.
    ///
    /// The whole set is swapped under ONE write, so no reader ever observes
    /// the app between its old bindings and its new ones. An app resolved to
    /// an EMPTY set holds no bindings at all, the state it was in before any
    /// host resolved one: control serves no live binding for it, and an
    /// `env.db` composed from a retired edge is worse than an absent one.
    ///
    /// # Errors
    ///
    /// `invalid_app_binding` on an empty app id, or on a set naming one
    /// database twice - two edges for one database disagree about which edge
    /// the app has, and this call cannot choose between them.
    /// `app_binding_store_unavailable` when the lock is poisoned.
    pub fn replace_app(
        &self,
        app_id: &str,
        resolved: Vec<ResolvedBinding>,
    ) -> Result<(), DbError> {
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

    /// The schema epoch this store holds for each of an app's databases.
    ///
    /// The comparable projection of the set: a host that knows which epochs
    /// control now serves reads this to decide whether the store already
    /// agrees, and re-resolves only when it does not. An app the store holds
    /// nothing for answers with the empty map, which is the same answer as an
    /// app control serves no live binding for - the two are the same state.
    #[must_use]
    pub fn epochs_for(&self, app_id: &str) -> std::collections::BTreeMap<DatabaseId, u32> {
        let Ok(apps) = self.apps.read() else {
            return std::collections::BTreeMap::new();
        };
        apps.get(app_id)
            .map(|edges| {
                edges
                    .iter()
                    .map(|edge| (edge.database.clone(), edge.epoch))
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

    /// A rotation advances the epoch on an edge the app already holds, and the
    /// store follows it.
    ///
    /// The epoch is the last component of the binding role name, and an apply
    /// that commits a schema delta mints the role at the next epoch and retires
    /// the one before the head. A store that kept the epoch it first resolved
    /// therefore composes a role the apply after next drops, and every session
    /// this app opens is refused at `SET LOCAL ROLE` from then on. So the
    /// assertion is on the composed ROLE and not only on the stored number:
    /// the role is what the session sends.
    ///
    /// Its rejection control is the same database at a HIGHER epoch naming a
    /// DIFFERENT binding, which is still refused. Without it this test would
    /// pass over a store that had simply stopped comparing edges at all.
    #[test]
    fn a_rotation_advances_the_stored_edge_and_a_moved_binding_is_still_refused() {
        let store = SuppliedAppBindings::new();
        let edge = resolved();
        store.supply("app_x", edge.clone()).expect("supply");
        let before = store
            .binding_for("app_x", "d1", &edge.database)
            .expect("the resolved edge composes a binding");

        let rotated = ResolvedBinding {
            database: edge.database.clone(),
            binding: edge.binding.clone(),
            epoch: edge.epoch + 1,
        };
        store
            .supply("app_x", rotated.clone())
            .expect("the same edge at the next epoch is the rotation, not a conflict");

        let after = store
            .binding_for("app_x", "d1", &edge.database)
            .expect("the rotated edge composes a binding");
        assert_eq!(after.schema_epoch(), Some(rotated.epoch));
        assert_ne!(
            before.session_role(),
            after.session_role(),
            "the epoch is part of the role name, so following the rotation has to \
             change the role the session narrows to"
        );
        assert_eq!(
            store.bindings_for("app_x", "d1").len(),
            1,
            "the rotation replaces the edge rather than joining the set"
        );

        // THE CONTROL: a higher epoch does not license a different binding.
        let moved = ResolvedBinding {
            database: edge.database.clone(),
            binding: BindingId::mint(),
            epoch: rotated.epoch + 1,
        };
        let error = store
            .supply("app_x", moved)
            .expect_err("a different binding for one database stays refused");
        assert_eq!(error.code(), "invalid_app_binding");
        assert_eq!(
            store
                .binding_for("app_x", "d1", &edge.database)
                .expect("the refused supply leaves the store serving")
                .session_role(),
            after.session_role()
        );
    }

    /// The stored epoch is monotone: a response carrying an older reading of it
    /// is refused and the store stays at the higher one.
    ///
    /// The cluster only ever advances the head, so a lower epoch is a late or
    /// reordered resolution rather than news. Following one backwards would
    /// walk a live binding onto the role the next apply retires - the stranding
    /// the rotation arm above exists to avoid, reintroduced from the other
    /// side.
    ///
    /// It is a separate code from the different-binding refusal because the two
    /// say different things: that one is two hosts disagreeing about which edge
    /// the app has, this one is one edge arriving twice out of order.
    #[test]
    fn a_lower_epoch_is_refused_and_leaves_the_higher_one_installed() {
        let store = SuppliedAppBindings::new();
        let edge = resolved();
        store.supply("app_x", edge.clone()).expect("supply");
        let installed = store
            .binding_for("app_x", "d1", &edge.database)
            .expect("the resolved edge composes a binding");

        let behind = ResolvedBinding {
            database: edge.database.clone(),
            binding: edge.binding.clone(),
            epoch: edge.epoch - 1,
        };
        let error = store
            .supply("app_x", behind)
            .expect_err("an epoch behind the installed one must be refused");
        assert_eq!(error.code(), STALE_APP_BINDING);

        let still = store
            .binding_for("app_x", "d1", &edge.database)
            .expect("the refused supply leaves the store serving");
        assert_eq!(still.schema_epoch(), Some(edge.epoch));
        assert_eq!(still.session_role(), installed.session_role());
    }

    /// Replacing an app's set installs what a host resolved for the isolate
    /// that is about to be built, including the two moves `supply` refuses.
    ///
    /// The three arms are the three things a replacement has to express and a
    /// resolution must not: an edge that MOVED to a different binding, a
    /// database the app no longer binds, and an epoch that advanced. Each is
    /// asserted on the composed ROLE where a role exists, because the role is
    /// what `SET LOCAL ROLE` sends.
    #[test]
    fn replacing_an_app_installs_the_set_supply_would_refuse() {
        let store = SuppliedAppBindings::new();
        let kept = resolved();
        let dropped = resolved();
        store.supply("app_x", kept.clone()).expect("supply the first");
        store
            .supply("app_x", dropped.clone())
            .expect("supply the second");
        let before = store
            .binding_for("app_x", "d1", &kept.database)
            .expect("the installed edge composes a binding");

        // The edge MOVED and the epoch advanced, and the second database is
        // gone from the set entirely.
        let moved = ResolvedBinding {
            database: kept.database.clone(),
            binding: BindingId::mint(),
            epoch: kept.epoch + 1,
        };
        assert_ne!(moved.binding, kept.binding);
        store
            .supply("app_x", moved.clone())
            .expect_err("a resolution cannot move an edge under a live isolate");
        store
            .replace_app("app_x", vec![moved.clone()])
            .expect("a replacement installs the set the next isolate is built from");

        let after = store
            .binding_for("app_x", "d1", &kept.database)
            .expect("the replaced edge composes a binding");
        assert_eq!(after.schema_epoch(), Some(moved.epoch));
        assert_ne!(
            before.session_role(),
            after.session_role(),
            "the binding id and the epoch are both in the role name, so a \
             replacement that moved either has to move the role"
        );
        assert!(
            store
                .binding_for("app_x", "d1", &dropped.database)
                .is_none(),
            "a database the app has stopped binding leaves the set"
        );
        assert_eq!(store.bindings_for("app_x", "d1").len(), 1);
    }

    /// An empty replacement leaves the app unbound, and an app the store never
    /// held is untouched by one.
    ///
    /// Control serves no live binding for such an app, so composing one from
    /// what the store happens to still hold would narrow a fresh isolate to a
    /// role a revocation retired. Its control is the nonempty replacement
    /// beside it, without which this would pass over a call that emptied the
    /// store whatever it was handed.
    #[test]
    fn an_empty_replacement_unbinds_the_app() {
        let store = SuppliedAppBindings::new();
        let edge = resolved();
        store.supply("app_x", edge.clone()).expect("supply");
        store.supply("app_other", resolved()).expect("supply");

        store
            .replace_app("app_x", Vec::new())
            .expect("an app control serves no live binding for holds none");
        assert!(!store.is_bound("app_x").expect("read the store"));
        assert!(store.binding_for("app_x", "d1", &edge.database).is_none());
        assert!(store.epochs_for("app_x").is_empty());

        store
            .replace_app("app_absent", Vec::new())
            .expect("an app the store never held is already in this state");

        // THE CONTROL: the neighbour app is untouched, and a nonempty
        // replacement binds rather than unbinds.
        assert!(store.is_bound("app_other").expect("read the store"));
        store
            .replace_app("app_x", vec![edge.clone()])
            .expect("a nonempty replacement binds");
        assert!(store.is_bound("app_x").expect("read the store"));
    }

    /// One database twice in a replacement is refused, because the call cannot
    /// choose which of two edges the app has.
    #[test]
    fn a_replacement_naming_one_database_twice_is_refused() {
        let store = SuppliedAppBindings::new();
        let edge = resolved();
        let twin = ResolvedBinding {
            database: edge.database.clone(),
            binding: BindingId::mint(),
            epoch: edge.epoch + 1,
        };
        let error = store
            .replace_app("app_x", vec![edge.clone(), twin])
            .expect_err("two edges for one database is not a set");
        assert_eq!(error.code(), "invalid_app_binding");
        assert!(
            !store.is_bound("app_x").expect("read the store"),
            "a refused replacement installs nothing"
        );

        // The control differing in one variable: two edges naming DIFFERENT
        // databases are a set, and install.
        store
            .replace_app("app_x", vec![edge, resolved()])
            .expect("two databases are a set");
        assert_eq!(store.epochs_for("app_x").len(), 2);

        assert_eq!(
            store
                .replace_app("", Vec::new())
                .expect_err("a replacement needs an app id")
                .code(),
            "invalid_app_binding"
        );
    }

    /// The epoch projection reports one entry per database the app binds, and
    /// follows every move of the set.
    #[test]
    fn the_epoch_projection_reports_each_databases_epoch() {
        let store = SuppliedAppBindings::new();
        let main = resolved();
        let analytics = ResolvedBinding {
            database: DatabaseId::mint(),
            binding: BindingId::mint(),
            epoch: main.epoch + 4,
        };
        assert!(store.epochs_for("app_x").is_empty());

        store.supply("app_x", main.clone()).expect("supply main");
        store
            .supply("app_x", analytics.clone())
            .expect("supply analytics");
        assert_eq!(
            store.epochs_for("app_x"),
            std::collections::BTreeMap::from([
                (main.database.clone(), main.epoch),
                (analytics.database.clone(), analytics.epoch),
            ]),
            "each database reports its OWN epoch, not the set's highest"
        );

        // A rotation on the LOWER of the two moves the projection, which a
        // maximum over the set would not show.
        let rotated = ResolvedBinding {
            epoch: main.epoch + 1,
            ..main.clone()
        };
        store.supply("app_x", rotated.clone()).expect("rotate main");
        assert_eq!(
            store.epochs_for("app_x").get(&main.database),
            Some(&rotated.epoch)
        );
        assert_eq!(
            store.epochs_for("app_x").get(&analytics.database),
            Some(&analytics.epoch),
            "the database that did not rotate keeps its epoch"
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
