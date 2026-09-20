//! Immutable identity of one `env.db` binding.
//!
//! A worker thread can keep multiple V8 isolates for the same app alive at
//! different deploys, and one app may reach more than one database. Neither the
//! app id nor the deploy token alone identifies what a CRUD receiver was minted
//! to use. `DbBinding` captures the whole identity from the active isolate once
//! and travels with every `Db` / `Collection` wrapper and asynchronous CRUD
//! continuation.
//!
//! # Two identities, carried separately
//!
//! `app_id` is the TENANT: the CDC event stamp, the SQLite `ATTACH` alias, the
//! app the usage sink attributes to. [`DbBinding::database`] is the DATABASE:
//! the physical schema tables are qualified with, the routing half that keeps
//! one app's two databases apart, and what the binding role is granted on.
//!
//! # What the session-setup batch needs
//!
//! A creator dispatch narrows with `SET LOCAL ROLE "zs_bind_<bnd>_e<E>"` as the
//! first statement of the batch. That role is composed HERE, once, from the
//! binding id and the schema epoch, so no call site can compose a second
//! spelling and no call site can compose one from the tenant. The reconciler
//! (`zeroship_migrate_server::datastore::cluster::grant_binding`) creates
//! exactly this name through the same `zeroship_core::database_derivation`
//! composer; if the two disagreed, every creator transaction would fail at
//! session setup.
//!
//! # Platform bindings narrow to nothing
//!
//! A trusted native service - auth, control's catalog, the workflow manager -
//! opens its OWN schema on a connection whose authority is the login, not a
//! per-binding role. It holds no database id and no binding role, and
//! [`DbBinding::platform`] is the only way to say so.

use zeroship_core::database_derivation;
use zeroship_core::{BindingId, DatabaseId};

use crate::error::DbError;
use crate::sql::SchemaName;

/// Deploy token used when a host does not inject `ZEROSHIP_DEPLOY_ID` (local
/// dev, raw-JS deploys, and narrow test harnesses).
pub const COLD_START_DEPLOY_TOKEN: &str = "cold_start";

/// One app's edge to one project-owned database, at one schema epoch.
///
/// The role name is composed at construction rather than on use: composing it
/// per statement would let the setup batch and the error classifier derive it
/// from different values, and the classifier's whole job is to recognise the
/// name the batch sent.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DatabaseEdge {
    database: DatabaseId,
    binding: BindingId,
    epoch: u32,
    role: String,
}

impl DatabaseEdge {
    /// The database this edge reaches.
    pub fn database(&self) -> &DatabaseId {
        &self.database
    }

    /// The edge's own identity, which the role name is derived from.
    pub fn binding(&self) -> &BindingId {
        &self.binding
    }

    /// The schema epoch the isolate holding this edge was built against.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// `zs_bind_<bnd>_e<E>`: the role the session-setup batch narrows to.
    pub fn role(&self) -> &str {
        &self.role
    }
}

/// Identity of one database binding: the tenant, the deploy, and the database.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DbBinding {
    app_id: String,
    deploy_token: String,
    schema: SchemaName,
    edge: Option<DatabaseEdge>,
}

impl DbBinding {
    /// One creator app's binding to one project-owned database.
    ///
    /// The schema and the binding role are DERIVED here, not handed in: a
    /// caller that could supply either could supply one that disagrees with the
    /// cluster, and the cluster is the authority for both.
    ///
    /// # Errors
    ///
    /// [`DbError`] carrying [`RoleNameTooLong`] when the composed binding role
    /// would not fit a `PostgreSQL` identifier. It is refused rather than
    /// shortened, because the epoch is the LAST component of the name and a
    /// truncation drops its digits first, collapsing two epochs of one binding
    /// onto one role.
    pub fn to_database(
        app_id: impl Into<String>,
        deploy_token: impl Into<String>,
        database: DatabaseId,
        binding: BindingId,
        epoch: u32,
    ) -> Result<Self, DbError> {
        let role = database_derivation::binding_role_name(&binding, epoch)?;
        let schema_text = database_derivation::schema_name(&database);
        let schema = SchemaName::new(&schema_text).map_err(|error| {
            DbError::config(
                "invalid_database_schema",
                format!("db: derived schema {schema_text} is not a legal identifier: {error}"),
            )
        })?;
        Ok(Self {
            app_id: app_id.into(),
            deploy_token: deploy_token.into(),
            schema,
            edge: Some(DatabaseEdge {
                database,
                binding,
                epoch,
                role,
            }),
        })
    }

    /// A trusted native service's own schema, opened under the login's own
    /// authority.
    ///
    /// It holds no database id and no binding role, so a session opened on it
    /// narrows to nothing. `tenant` is the service's own name rather than an
    /// app id, and `label` distinguishes two stores in one process.
    pub fn platform(
        tenant: impl Into<String>,
        label: impl Into<String>,
        schema: SchemaName,
    ) -> Self {
        Self {
            app_id: tenant.into(),
            deploy_token: label.into(),
            schema,
            edge: None,
        }
    }

    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    pub fn deploy_token(&self) -> &str {
        &self.deploy_token
    }

    /// The physical schema this binding's statements are qualified with.
    pub fn schema(&self) -> &SchemaName {
        &self.schema
    }

    /// The database edge, or `None` for a platform binding.
    pub fn edge(&self) -> Option<&DatabaseEdge> {
        self.edge.as_ref()
    }

    /// The database this binding addresses, or `None` for a platform binding.
    pub fn database(&self) -> Option<&DatabaseId> {
        self.edge.as_ref().map(DatabaseEdge::database)
    }

    /// The role the session-setup batch narrows to, or `None` when this binding
    /// narrows to nothing.
    pub fn session_role(&self) -> Option<&str> {
        self.edge.as_ref().map(DatabaseEdge::role)
    }

    /// The schema epoch this binding was resolved at, or `None` for a platform
    /// binding.
    pub fn schema_epoch(&self) -> Option<u32> {
        self.edge.as_ref().map(DatabaseEdge::epoch)
    }

    /// The key every per-thread resource this binding's work touches is held
    /// under.
    #[must_use]
    pub fn route(&self) -> DbRoute {
        DbRoute {
            app_id: self.app_id.clone(),
            database: self.database().cloned(),
        }
    }
}

/// The key a transaction lane, a captured route and a thread-local connection
/// are held under: the TENANT and the DATABASE together.
///
/// **The database half is the database id and never a creator label.** Two
/// co-resident apps may both call a database `main`, and two databases on one
/// project may both declare a collection `users`; a key carrying either would
/// compare equal across them and route one app's statement onto the other's
/// open transaction.
///
/// The tenant half is not redundant with it. One database may be bound by many
/// apps, and a transaction frame belongs to one app's callback: two apps
/// sharing a database must still never share a lane.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DbRoute {
    app_id: String,
    database: Option<DatabaseId>,
}

impl DbRoute {
    /// The key for a creator app's work against one database.
    #[must_use]
    pub fn new(app_id: impl Into<String>, database: Option<DatabaseId>) -> Self {
        Self {
            app_id: app_id.into(),
            database,
        }
    }

    /// The key a trusted native service's own store is held under.
    #[must_use]
    pub fn platform(tenant: impl Into<String>) -> Self {
        Self::new(tenant, None)
    }

    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    pub fn database(&self) -> Option<&DatabaseId> {
        self.database.as_ref()
    }

    /// The printed database id, or the empty string for a platform route.
    ///
    /// Hosts that carry the route through an untyped channel - the V8
    /// continuation-preserved slot is the only one - encode it with this and
    /// decode it with [`DbRoute::decoded`].
    #[must_use]
    pub fn database_text(&self) -> &str {
        self.database.as_ref().map_or("", DatabaseId::as_str)
    }

    /// Rebuild a route from a host's untyped observation.
    ///
    /// An unparseable database half yields `None` rather than a platform route:
    /// silently widening a creator route into one that narrows to nothing is
    /// the direction that must not be available.
    #[must_use]
    pub fn decoded(app_id: String, database_text: &str) -> Option<Self> {
        if database_text.is_empty() {
            return Some(Self {
                app_id,
                database: None,
            });
        }
        DatabaseId::parse(database_text).ok().map(|database| Self {
            app_id,
            database: Some(database),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge_fixture() -> (DatabaseId, BindingId) {
        (DatabaseId::mint(), BindingId::mint())
    }

    /// The schema and the role both come from the ids, and they agree with the
    /// composers the cluster reconciler creates the objects through.
    ///
    /// Two oracles per line: the binding's own answer and the `zeroship-core`
    /// composer the reconciler calls. A binding that derived its own spelling
    /// would narrow to a role no reconciler ever created.
    #[test]
    fn a_database_binding_derives_its_schema_and_role_from_the_ids() {
        let (database, binding) = edge_fixture();
        let bound = DbBinding::to_database("app_x", "deploy_x", database.clone(), binding.clone(), 7)
            .expect("the fixture ids compose");

        assert_eq!(
            bound.schema().as_str(),
            database_derivation::schema_name(&database)
        );
        assert_eq!(
            bound.session_role(),
            Some(
                database_derivation::binding_role_name(&binding, 7)
                    .expect("the fixture role name fits")
                    .as_str()
            )
        );
        assert_eq!(bound.schema_epoch(), Some(7));
        assert_eq!(bound.database(), Some(&database));
    }

    /// The epoch is part of the role name, so two epochs of one binding are two
    /// roles. Without this the fence catches a revoked binding and nothing else.
    #[test]
    fn the_epoch_changes_the_role_and_nothing_else() {
        let (database, binding) = edge_fixture();
        let at_one =
            DbBinding::to_database("app_x", "d", database.clone(), binding.clone(), 1).unwrap();
        let at_two = DbBinding::to_database("app_x", "d", database.clone(), binding, 2).unwrap();

        assert_ne!(at_one.session_role(), at_two.session_role());
        assert_eq!(at_one.schema(), at_two.schema());
        assert_eq!(at_one.route(), at_two.route());
    }

    /// A platform store narrows to nothing and carries no database.
    #[test]
    fn a_platform_binding_holds_no_database_and_no_role() {
        let platform = DbBinding::platform(
            "platform",
            "auth",
            SchemaName::new("zeroship").expect("fixture schema"),
        );
        assert_eq!(platform.database(), None);
        assert_eq!(platform.session_role(), None);
        assert_eq!(platform.schema_epoch(), None);
        assert_eq!(platform.route(), DbRoute::platform("platform"));
    }

    /// One app on two databases is two routes. The control is the same app on
    /// the same database, which must be one route - otherwise the inequality
    /// below would hold for a key that varies with something else.
    #[test]
    fn one_app_on_two_databases_is_two_routes() {
        let first = DatabaseId::mint();
        let second = DatabaseId::mint();
        assert_ne!(first, second, "the control: two mints are two databases");

        let to_first = DbRoute::new("app_x", Some(first.clone()));
        let to_second = DbRoute::new("app_x", Some(second));
        assert_ne!(to_first, to_second);
        assert_eq!(to_first, DbRoute::new("app_x", Some(first)));
    }

    /// Two apps sharing one database are two routes.
    #[test]
    fn two_apps_on_one_database_are_two_routes() {
        let shared = DatabaseId::mint();
        assert_ne!(
            DbRoute::new("app_a", Some(shared.clone())),
            DbRoute::new("app_b", Some(shared))
        );
    }

    /// The untyped host channel round-trips, and an unparseable database half
    /// refuses rather than decoding into a platform route.
    #[test]
    fn a_route_round_trips_through_the_untyped_host_channel() {
        let database = DatabaseId::mint();
        let route = DbRoute::new("app_x", Some(database));
        assert_eq!(
            DbRoute::decoded(route.app_id().to_owned(), route.database_text()),
            Some(route.clone())
        );

        let platform = DbRoute::platform("platform");
        assert_eq!(
            DbRoute::decoded("platform".to_owned(), platform.database_text()),
            Some(platform)
        );

        assert_eq!(
            DbRoute::decoded("app_x".to_owned(), "not-a-database-id"),
            None,
            "a malformed database half must refuse, never widen into a platform route"
        );
    }
}
