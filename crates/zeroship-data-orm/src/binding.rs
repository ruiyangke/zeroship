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
//! A creator dispatch narrows with `SET LOCAL ROLE "zs_bind_<bnd>"` as the
//! first statement of the batch. That role is composed HERE, once, from the
//! binding id, so no call site can compose a second
//! spelling and no call site can compose one from the tenant. The reconciler
//! (`zeroship_migrate_server::datastore::cluster::grant_binding`) creates
//! exactly this name through the same `zeroship_core::database_derivation`
//! composer; if the two disagreed, every creator transaction would fail at
//! session setup.
//!
//! The audited raw-column read assumes a SECOND role for the length of its own
//! statement - `zs_db_<dbs>_unmask`, the only role holding `SELECT` on a masked
//! field's real-value column - and it is carried on the same terms and for the
//! same reason. It derives from the DATABASE and not from the edge, because the
//! columns it reaches are the schema's: one database's masked columns are one
//! grant however many apps bind to it, and it is the BINDING's membership in
//! that role that keeps the reachability revocable.
//!
//! # Platform bindings narrow to nothing
//!
//! A trusted native service - auth, control's catalog, the workflow manager -
//! opens its OWN schema on a connection whose authority is the login, not a
//! per-binding role. It holds no database id and no binding role, and
//! [`DbBinding::platform`] is the only way to say so.
//!
//! # The capability is carried, not derived
//!
//! The database and the binding id derive the schema and the role name, but a
//! binding's CAPABILITY is a control-plane fact about the edge and derives from
//! nothing here. It is the spelling [`zeroship_core::database_role`] owns
//! ([`DatabaseCapability`]) rather than a second enum, because the reconciler
//! composes a capability ROLE from the same value and a second spelling would
//! name a role nothing created. The unabbreviated name is also what keeps it
//! apart from this crate's own `Capability` vocabulary, which is about what a
//! FIELD supports.

use zeroship_core::database_derivation;
use zeroship_core::database_role::DatabaseCapability;
use zeroship_core::{BindingId, DatabaseId};

use crate::error::DbError;
use crate::sql::SchemaName;

/// Deploy token used when a host does not inject `ZEROSHIP_DEPLOY_ID` (local
/// dev, raw-JS deploys, and narrow test harnesses).
pub const COLD_START_DEPLOY_TOKEN: &str = "cold_start";

/// One app's edge to one project-owned database.
///
/// Both role names are composed at construction rather than on use: composing
/// them per statement would let the setup batch and the error classifier derive
/// them from different values, and the classifier's whole job is to recognise
/// the name the batch sent.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DatabaseEdge {
    database: DatabaseId,
    binding: BindingId,
    capability: DatabaseCapability,
    role: String,
    unmask_role: String,
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

    /// The privilege set control declared for this edge.
    ///
    /// Read, never composed: the binding role is granted membership in the
    /// database's role for exactly this capability, so the value here and the
    /// grant on the cluster come from one control-plane row.
    pub fn database_capability(&self) -> DatabaseCapability {
        self.capability
    }

    /// `zs_bind_<bnd>`: the role the session-setup batch narrows to.
    pub fn role(&self) -> &str {
        &self.role
    }

    /// `zs_db_<dbs>_unmask`: the role an audited raw-column read assumes for
    /// exactly that statement.
    ///
    /// Two edges to one database name one role here, which is the whole
    /// difference from [`Self::role`]: the grant is on the schema's columns, so
    /// it is stated once per database, and it is each edge's own membership -
    /// not this name - that a revoke withdraws.
    pub fn unmask_role(&self) -> &str {
        &self.unmask_role
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
    /// shortened, because the binding id is the LAST component of the name and
    /// a truncation drops the bytes that tell two bindings apart, collapsing
    /// them onto one role.
    pub fn to_database(
        app_id: impl Into<String>,
        deploy_token: impl Into<String>,
        database: DatabaseId,
        binding: BindingId,
        capability: DatabaseCapability,
    ) -> Result<Self, DbError> {
        let role = database_derivation::binding_role_name(&binding)?;
        let unmask_role = database_derivation::unmask_role_name(&database)?;
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
                capability,
                role,
                unmask_role,
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

    /// The role an audited raw-column read assumes, or `None` when this binding
    /// narrows to nothing.
    ///
    /// A `None` here is a REFUSAL and never a read left under whatever role the
    /// session already holds, on the terms
    /// `crate::backend::postgres::pg_session_sql::unbound_session` states for
    /// the narrowing itself.
    pub fn unmask_role(&self) -> Option<&str> {
        self.edge.as_ref().map(DatabaseEdge::unmask_role)
    }

    /// The capability control declared for this edge, or `None` for a platform
    /// binding.
    pub fn database_capability(&self) -> Option<DatabaseCapability> {
        self.edge.as_ref().map(DatabaseEdge::database_capability)
    }

    /// Whether an operation that modifies rows is one this binding was declared
    /// to make.
    ///
    /// **This is not an authorization boundary and must not be presented as
    /// one.** The process asking runs creator code, so a check it can reach is
    /// a check creator code is on the wrong side of. `PostgreSQL` is the
    /// authority: the reconciler grants the binding role membership in exactly
    /// one of the database's two capability roles, so a session narrowed to a
    /// read-only binding cannot write whatever this answers. What reading it
    /// buys is a refusal naming the binding instead of `42501 permission denied
    /// for table ...` from the server.
    ///
    /// A PLATFORM binding answers `true` and holds no capability: it narrows to
    /// nothing, its authority is the login rather than a per-binding role, and
    /// there is no control-plane edge to have declared one. That is the state a
    /// trusted native service's own schema is in, never a creator app's.
    pub fn permits_writes(&self) -> bool {
        self.edge
            .as_ref()
            .is_none_or(|edge| edge.capability.permits_writes())
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
        let bound = DbBinding::to_database(
            "app_x",
            "deploy_x",
            database.clone(),
            binding.clone(),
            DatabaseCapability::ReadWrite,
        )
        .expect("the fixture ids compose");

        assert_eq!(
            bound.schema().as_str(),
            database_derivation::schema_name(&database)
        );
        assert_eq!(
            bound.session_role(),
            Some(
                database_derivation::binding_role_name(&binding)
                    .expect("the fixture role name fits")
                    .as_str()
            )
        );
        assert_eq!(
            bound.unmask_role(),
            Some(
                database_derivation::unmask_role_name(&database)
                    .expect("the fixture unmask role name fits")
                    .as_str()
            )
        );
        assert_eq!(bound.database(), Some(&database));
    }

    /// The two roles a creator binding carries come from DIFFERENT ids, and the
    /// one the raw read assumes comes from the database.
    ///
    /// Two edges to one database: the binding role must differ and the unmask
    /// role must not. Without the second half a composer that took the binding
    /// id would satisfy the first and name a role no reconciler created for the
    /// database's columns; without the first the two would be one name.
    #[test]
    fn the_unmask_role_follows_the_database_while_the_binding_role_follows_the_edge() {
        let database = DatabaseId::mint();
        let other_database = DatabaseId::mint();
        assert_ne!(
            database, other_database,
            "the control: two mints are two databases"
        );
        let compose = |database: &DatabaseId| {
            DbBinding::to_database(
                "app_x",
                "d",
                database.clone(),
                BindingId::mint(),
                DatabaseCapability::ReadWrite,
            )
            .expect("the fixture ids compose")
        };

        let mine = compose(&database);
        let sibling = compose(&database);
        assert_ne!(
            mine.session_role(),
            sibling.session_role(),
            "two edges to one database are two binding roles"
        );
        assert_eq!(
            mine.unmask_role(),
            sibling.unmask_role(),
            "two edges to one database reach one set of masked columns, so they \
             name one unmask role"
        );
        assert_ne!(
            mine.unmask_role(),
            compose(&other_database).unmask_role(),
            "a second database is a second unmask role"
        );
        assert_ne!(
            mine.unmask_role(),
            mine.session_role(),
            "the elevation would be a no-op if the two names were one"
        );
    }

    /// Two edges to ONE database are two roles, and the schema they qualify
    /// with is the same one. The role is what a revoke withdraws, so two apps
    /// on one database sharing a role name would make either revoke withdraw
    /// both.
    #[test]
    fn the_edge_changes_the_role_and_nothing_else() {
        let (database, binding) = edge_fixture();
        let other = BindingId::mint();
        assert_ne!(other, binding, "the control: two mints are two edges");
        let mine = DbBinding::to_database(
            "app_x",
            "d",
            database.clone(),
            binding,
            DatabaseCapability::ReadWrite,
        )
        .unwrap();
        let theirs = DbBinding::to_database(
            "app_x",
            "d",
            database.clone(),
            other,
            DatabaseCapability::ReadWrite,
        )
        .unwrap();

        assert_ne!(mine.session_role(), theirs.session_role());
        assert_eq!(mine.schema(), theirs.schema());
        assert_eq!(mine.route(), theirs.route());
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
        assert_eq!(platform.unmask_role(), None);
        assert_eq!(platform.route(), DbRoute::platform("platform"));

        // The control: a creator binding beside it answers both, or the three
        // `None`s above would hold for accessors that answered `None` always.
        let (database, binding) = edge_fixture();
        let creator = DbBinding::to_database(
            "app_x",
            "d",
            database,
            binding,
            DatabaseCapability::ReadWrite,
        )
        .expect("the fixture ids compose");
        assert!(creator.session_role().is_some());
        assert!(creator.unmask_role().is_some());
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

    /// The capability is CARRIED: what goes in comes out, for both values, and
    /// it changes nothing the ids derive.
    ///
    /// The second half is the one that would go unnoticed: a capability that
    /// leaked into the role name would make two bindings on one edge narrow to
    /// two roles, and only one of them was ever created.
    #[test]
    fn a_binding_carries_its_capability_without_changing_what_the_ids_derive() {
        let (database, binding) = edge_fixture();
        let compose = |capability| {
            DbBinding::to_database(
                "app_x",
                "deploy_x",
                database.clone(),
                binding.clone(),
                capability,
            )
            .expect("the fixture ids compose")
        };
        let writable = compose(DatabaseCapability::ReadWrite);
        let read_only = compose(DatabaseCapability::ReadOnly);

        assert_eq!(
            writable.database_capability(),
            Some(DatabaseCapability::ReadWrite)
        );
        assert_eq!(
            read_only.database_capability(),
            Some(DatabaseCapability::ReadOnly)
        );
        assert!(writable.permits_writes());
        assert!(!read_only.permits_writes());

        assert_eq!(
            writable.session_role(),
            read_only.session_role(),
            "the role name is derived from the edge; the capability must not \
             reach it"
        );
        assert_eq!(
            writable.unmask_role(),
            read_only.unmask_role(),
            "the unmask role is derived from the database; the capability must \
             not reach it either"
        );
        assert_eq!(writable.schema(), read_only.schema());
        assert_eq!(writable.route(), read_only.route());
    }

    /// A platform binding holds no capability and writes anyway.
    ///
    /// Its control is the read-only creator binding beside it: without one,
    /// this would pass over a `permits_writes` that answered `true` for
    /// everything.
    #[test]
    fn a_platform_binding_holds_no_capability_and_still_writes() {
        let platform = DbBinding::platform(
            "platform",
            "auth",
            SchemaName::new("zeroship").expect("fixture schema"),
        );
        assert_eq!(platform.database_capability(), None);
        assert!(platform.permits_writes());

        let (database, binding) = edge_fixture();
        let read_only = DbBinding::to_database(
            "app_x",
            "d",
            database,
            binding,
            DatabaseCapability::ReadOnly,
        )
        .expect("the fixture ids compose");
        assert!(!read_only.permits_writes());
    }
}
