//! The cluster reconciler: one loop per datastore.
//!
//! Datastore bootstrap, database provisioning and binding grants are the same
//! problem - control declares, a cluster must be made to match - and all three
//! need the same privileged connection to the same cluster. They are ONE loop,
//! not three: three loops would mean three connections, three failure reports
//! and three places for a disagreement with control to be resolved differently.
//!
//! ```text
//!   register this cluster by its own identity          -> a datastores row
//!   apply the datastore bootstrap corpus               pending -> active
//!   converge every database declared on it             schema, roles, grants
//!   converge every binding to those databases          the two role edges
//!   reap the binding roles no declaration names
//! ```
//!
//! It lives in `migrate-server` because that service already holds the
//! privileged DSN per cluster and already never executes creator code.
//!
//! # Granting is not one transaction, and never was
//!
//! Control's database and the tenant cluster are different servers, so nothing
//! spans a control row and the role DDL it describes. That is why
//! `generation` / `observed_generation` exist on a binding, and why every write
//! here is ordered cluster-first: the cluster is made to match, and only then
//! does the control row say so. The reverse order would publish a binding whose
//! roles are not there yet.
//!
//! Nothing downstream trusts a half-converged row. Placement admits
//! `status = 'active'` datastores only, and a deploy requires a binding whose
//! `observed_generation` has caught up to its `generation`.
//!
//! # One row's failure is not the pass's failure
//!
//! A cluster briefly refusing one grant is routine, and a pass that aborted on
//! it would stall every other app on that cluster. Per-row failures are
//! recorded in the row's `last_error` and the pass carries on. What DOES abort
//! a pass is the posture check: a worker login already holding a database role
//! directly has a fence that is already open, and converging more bindings onto
//! it makes that worse rather than better.
//!
//! # The reap is the one destructive step, and it is gated on a positive read
//!
//! A sweep that matches nothing reports success. The declaration set the reap
//! compares against is read over the network from another server, and a read
//! that failed is indistinguishable downstream from a cluster with nothing
//! declared on it - so a naive reap would answer a transport error by dropping
//! every binding role on the cluster, which revokes every live app's access to
//! its own data. Three gates stand in front of it:
//!
//! 1. [`control::Declarations`] has no constructor but a COMPLETE read, so a
//!    failed read leaves the reap with no value to sweep against.
//! 2. The datastore row must be present and `active`.
//! 3. A role is a candidate only if its name recomposes byte for byte through
//!    `zeroship_core::database_role`. A `zs_`-prefixed name that does not is
//!    reported and left alone - the platform logins live on the same cluster.

pub mod cluster;
pub mod control;
pub mod identity;

use std::time::Duration;

use compio_postgres::{Client, NoTls};
use zeroship_core::database_role::DatabaseCapability;
use zeroship_core::{BindingId, DatabaseId};
use zeroship_id::DatastoreId;

use cluster::{ClusterError, ClusterRole};
use control::{
    BindingDeclaration, ControlError, ControlStore, Declarations, BINDING_STATUS_ACTIVE,
    BINDING_STATUS_PENDING, BINDING_STATUS_REVOKED, BINDING_STATUS_REVOKING,
    DATABASE_STATUS_ACTIVE, DATABASE_STATUS_DELETING, DATABASE_STATUS_PROVISIONING,
    DATASTORE_STATUS_ACTIVE, DATASTORE_STATUS_PENDING,
};
use identity::{read_cluster_identity, UnsupportedServerVersion};

/// A reconciliation pass that could not run at all.
///
/// Distinct from the per-row failures a pass records and carries: these are
/// conditions under which converging anything would be wrong.
#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    /// The cluster would not accept the privileged connection.
    #[error("cluster connect: {0}")]
    Connect(#[source] compio_postgres::Error),
    /// The cluster would not answer what it is.
    #[error("cluster identity: {0}")]
    Identity(#[source] compio_postgres::Error),
    /// The cluster is older than the tenant fence exists on.
    #[error(transparent)]
    UnsupportedServerVersion(#[from] UnsupportedServerVersion),
    /// A control-plane read or write did not complete.
    #[error(transparent)]
    Control(#[from] ControlError),
    /// A cluster-side step did not complete.
    #[error(transparent)]
    Cluster(#[from] ClusterError),
    /// The zone this service registers into could not be resolved.
    #[error("{0}")]
    Zone(String),
}

/// What one pass observed and changed.
///
/// Every field counts an OBSERVABLE transition rather than a statement issued.
/// The convergence statements all state a desired state, so a converged cluster
/// runs them again and nothing moves; what proves idempotency is this report
/// coming back empty on the second pass, next to a catalog and a set of control
/// rows that did not change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PassReport {
    /// This pass inserted the `zeroship.datastores` row.
    pub registered: bool,
    /// This pass moved the datastore from `pending` to `active`.
    pub datastore_activated: bool,
    /// Databases this pass moved from `provisioning` to `active`.
    pub databases_activated: Vec<DatabaseId>,
    /// Bindings whose row this pass moved to `active` at its declared
    /// generation.
    pub bindings_activated: Vec<BindingId>,
    /// Bindings whose row this pass moved to `revoked`.
    pub bindings_revoked: Vec<BindingId>,
    /// Databases this pass tore down, on an explicit `deleting` declaration.
    pub databases_deleted: Vec<DatabaseId>,
    /// Binding roles this pass dropped because no declaration named them.
    pub roles_reaped: Vec<String>,
    /// `zs_`-prefixed roles the reap could not attribute. Never dropped.
    pub unattributed_roles: Vec<String>,
    /// Database roles and `db_` schemas no declaration names.
    ///
    /// REPORTED AND LEFT STANDING. A binding role is reaped by absence because
    /// the next pass re-grants it; a schema holds tenant data and a database
    /// role owns that schema, so nothing here is destroyed on an absence. An
    /// orphan left alone is a cleanup task; an orphan dropped on a read that
    /// silently returned nothing is an outage.
    pub undeclared_database_objects: Vec<String>,
    /// Per-row failures, recorded on the row and carried past.
    pub failures: Vec<String>,
}

impl PassReport {
    /// Nothing observable moved.
    ///
    /// The failures are part of this: a pass that recorded an error changed a
    /// row, and calling that a no-op would let a permanently failing binding
    /// read as converged.
    #[must_use]
    pub const fn changed_nothing(&self) -> bool {
        !self.registered
            && !self.datastore_activated
            && self.databases_activated.is_empty()
            && self.databases_deleted.is_empty()
            && self.bindings_activated.is_empty()
            && self.bindings_revoked.is_empty()
            && self.roles_reaped.is_empty()
            && self.failures.is_empty()
    }
}

/// One cluster's reconciler.
#[derive(Debug)]
pub struct Reconciler {
    control: ControlStore,
    cluster_dsn: String,
    execution_zone_id: Option<String>,
}

impl Reconciler {
    /// Build a reconciler for one cluster.
    ///
    /// `execution_zone_id` is the operator's declaration of which zone this
    /// service's cluster belongs to. `None` resolves it from the control
    /// plane's own zone table, which is unambiguous exactly when a deployment
    /// declares one zone - the same condition `Registry::create_app` already
    /// requires before it will choose a zone for an app.
    #[must_use]
    pub fn new(
        control: ControlStore,
        cluster_dsn: impl Into<String>,
        execution_zone_id: Option<String>,
    ) -> Self {
        Self {
            control,
            cluster_dsn: cluster_dsn.into(),
            execution_zone_id: execution_zone_id.filter(|zone| !zone.trim().is_empty()),
        }
    }

    /// Run one pass.
    ///
    /// # Errors
    ///
    /// [`ReconcileError`] when the pass could not run. Per-row failures are
    /// recorded on their rows and reported in [`PassReport::failures`] instead.
    pub async fn reconcile_once(&self) -> Result<(DatastoreId, PassReport), ReconcileError> {
        let mut admin = self.connect_cluster().await?;
        let identity = read_cluster_identity(&admin)
            .await
            .map_err(ReconcileError::Identity)?;
        let zone = self.resolve_zone().await?;

        // REGISTER FIRST, THEN GATE. A cluster this platform has no fence on is
        // still a cluster an operator pointed a service at, and a registry row
        // stuck at `pending` with the version refusal on it is what makes that
        // visible. Placement admits `active` only, so the row is inert.
        let registration = self.control.register_datastore(identity, &zone).await?;
        let datastore = registration.datastore.clone();
        let mut report = PassReport {
            registered: registration.created,
            ..PassReport::default()
        };

        if let Err(unsupported) = identity.require_supported() {
            self.control
                .record_datastore_error(&datastore, &unsupported.to_string())
                .await?;
            return Err(ReconcileError::UnsupportedServerVersion(unsupported));
        }

        // `draining`, `retired` and `failed` are the operator's lever for
        // taking a cluster out of rotation without a deploy. Converging onto
        // one would put databases back on a cluster somebody is emptying.
        if registration.status != DATASTORE_STATUS_PENDING
            && registration.status != DATASTORE_STATUS_ACTIVE
        {
            return Ok((datastore, report));
        }

        cluster::apply_bootstrap_corpus(&admin).await?;
        if registration.status == DATASTORE_STATUS_PENDING {
            report.datastore_activated = self.control.activate_datastore(&datastore).await?;
        }

        // The posture check aborts the pass. One direct membership hands the
        // shared worker login every co-tenant binding's privileges on that
        // database at once and survives revoking any single binding, so more
        // convergence on top of it is strictly worse than stopping.
        if let Err(broken) = cluster::require_no_direct_database_memberships(&admin).await {
            self.control
                .record_datastore_error(&datastore, &broken.to_string())
                .await?;
            return Err(ReconcileError::Cluster(broken));
        }

        // THE ONE COMPLETE READ. Everything below reads this value and nothing
        // re-queries control for a declaration, so the converge pass and the
        // reap cannot disagree about what is declared - and a role this pass
        // created is in the set the reap compares against.
        let declarations = self.control.read_declarations(&datastore).await?;

        self.converge_databases(&mut admin, &declarations, &mut report)
            .await?;
        self.converge_bindings(&admin, &declarations, &mut report)
            .await?;

        // Gate 2. A cluster that is not in rotation is one whose declarations
        // may still be moving; nothing is dropped on it.
        let in_rotation = report.datastore_activated
            || registration.status == DATASTORE_STATUS_ACTIVE;
        if in_rotation {
            self.reap(&admin, &declarations, &mut report).await?;
        }

        if report.failures.is_empty() {
            self.control.record_datastore_error(&datastore, "").await.ok();
        }
        Ok((datastore, report))
    }

    /// Run passes until the process ends.
    ///
    /// A pass that could not run is logged and retried on the next tick: a
    /// cluster that is briefly unreachable must not end the loop, because the
    /// loop is what would notice it coming back.
    pub async fn run(self, interval: Duration) {
        loop {
            match self.reconcile_once().await {
                Ok((datastore, report)) => {
                    if !report.changed_nothing() {
                        tracing::info!(
                            datastore = %datastore.as_str(),
                            registered = report.registered,
                            datastore_activated = report.datastore_activated,
                            databases_activated = report.databases_activated.len(),
                            bindings_activated = report.bindings_activated.len(),
                            bindings_revoked = report.bindings_revoked.len(),
                            roles_reaped = report.roles_reaped.len(),
                            failures = report.failures.len(),
                            "cluster reconciler pass"
                        );
                    }
                    for failure in &report.failures {
                        tracing::warn!(
                            datastore = %datastore.as_str(),
                            failure = %failure,
                            "cluster reconciler recorded a per-row failure"
                        );
                    }
                }
                Err(error) => {
                    tracing::error!(error = %error, "cluster reconciler pass did not run");
                }
            }
            compio::time::sleep(interval).await;
        }
    }

    async fn connect_cluster(&self) -> Result<Client, ReconcileError> {
        let (client, connection) = compio_postgres::connect(&self.cluster_dsn, NoTls)
            .await
            .map_err(ReconcileError::Connect)?;
        compio::runtime::spawn(async move {
            if let Err(error) = connection.run().await {
                tracing::debug!(%error, "cluster reconciler connection ended");
            }
        })
        .detach();
        Ok(client)
    }

    /// The zone this service registers its cluster into.
    ///
    /// Configured wins. Unconfigured is resolved from `zeroship.execution_zones`
    /// and refuses to CHOOSE: a deployment declaring more than one zone has to
    /// say which one this cluster is in, because reaching a cluster proves the
    /// cluster exists and proves nothing at all about which zone it serves.
    async fn resolve_zone(&self) -> Result<String, ReconcileError> {
        if let Some(zone) = &self.execution_zone_id {
            return Ok(zone.clone());
        }
        let zones = self.control.sole_active_zone().await?;
        match zones {
            control::SoleZone::One(zone) => Ok(zone),
            control::SoleZone::None => Err(ReconcileError::Zone(
                "this deployment declares no active execution zone, so there is none to \
                 register a cluster into"
                    .to_owned(),
            )),
            control::SoleZone::Many(count) => Err(ReconcileError::Zone(format!(
                "this deployment declares {count} active execution zones; set \
                 migrate_server.execution_zone to the one this cluster serves"
            ))),
        }
    }

    async fn converge_databases(
        &self,
        admin: &mut Client,
        declarations: &Declarations,
        report: &mut PassReport,
    ) -> Result<(), ReconcileError> {
        for declaration in declarations.databases() {
            if declaration.status == DATABASE_STATUS_DELETING {
                if let Err(failure) = self.tear_down(admin, declarations, declaration, report).await?
                {
                    report.failures.push(format!(
                        "database {}: {failure}",
                        declaration.database.as_str()
                    ));
                }
                continue;
            }
            if declaration.status != DATABASE_STATUS_PROVISIONING
                && declaration.status != DATABASE_STATUS_ACTIVE
            {
                // `draining` is leaving rotation rather than being destroyed.
                // Nothing to converge and nothing to remove.
                continue;
            }
            match cluster::converge_database(admin, &declaration.database, declaration.schema_epoch)
                .await
            {
                Ok(_epoch) => {
                    if declaration.status == DATABASE_STATUS_PROVISIONING
                        && self.control.activate_database(&declaration.database).await?
                    {
                        report
                            .databases_activated
                            .push(declaration.database.clone());
                    }
                }
                Err(error) => report.failures.push(format!(
                    "database {}: {error}",
                    declaration.database.as_str()
                )),
            }
        }
        Ok(())
    }

    /// Destroy one database, on its own explicit declaration.
    ///
    /// The precondition is read from the SAME complete declaration set the rest
    /// of the pass uses: a database any binding still names is not torn down,
    /// and the composite foreign key refuses the row removal for the same
    /// reason if one appeared in between. The inner `Result` is the row's
    /// outcome; the outer one ends the pass.
    async fn tear_down(
        &self,
        admin: &mut Client,
        declarations: &Declarations,
        declaration: &control::DatabaseDeclaration,
        report: &mut PassReport,
    ) -> Result<Result<(), String>, ReconcileError> {
        let still_bound: Vec<&str> = declarations
            .bindings()
            .iter()
            .filter(|binding| binding.database == declaration.database)
            .map(|binding| binding.binding.as_str())
            .collect();
        if !still_bound.is_empty() {
            return Ok(Err(format!(
                "still bound by {}; a database is destroyed only once no app reaches it",
                still_bound.join(", ")
            )));
        }
        if let Err(error) = cluster::drop_database(admin, &declaration.database).await {
            return Ok(Err(error.to_string()));
        }
        if self
            .control
            .remove_deleted_database(&declaration.database)
            .await?
        {
            report.databases_deleted.push(declaration.database.clone());
        }
        Ok(Ok(()))
    }

    async fn converge_bindings(
        &self,
        admin: &Client,
        declarations: &Declarations,
        report: &mut PassReport,
    ) -> Result<(), ReconcileError> {
        for declaration in declarations.bindings() {
            if let Err(failure) = self
                .converge_one_binding(admin, declaration, report)
                .await?
            {
                self.control
                    .record_binding_error(&declaration.binding, &failure)
                    .await?;
                report.failures.push(format!(
                    "binding {}: {failure}",
                    declaration.binding.as_str()
                ));
            }
        }
        Ok(())
    }

    /// Converge one binding. The inner `Result` is the ROW's outcome; the outer
    /// one is a control-plane failure that ends the pass.
    async fn converge_one_binding(
        &self,
        admin: &Client,
        declaration: &BindingDeclaration,
        report: &mut PassReport,
    ) -> Result<Result<(), String>, ReconcileError> {
        let Some(capability) = declaration.capability else {
            // The role name is composed FROM the capability, so a value this
            // cannot read is a binding whose role cannot be named. Refusing is
            // the only fail-closed answer; guessing a capability would hand an
            // app whichever one guessed wrong.
            return Ok(Err(format!(
                "capability `{}` is not one of `{}` or `{}`",
                declaration.capability_text,
                DatabaseCapability::ReadWrite.as_wire(),
                DatabaseCapability::ReadOnly.as_wire()
            )));
        };

        // THE EPOCH COMES FROM THE CLUSTER. Control's `schema_epoch` is a
        // projection kept so a binding can be composed without a cross-zone
        // read; the row in the cluster's own admin schema is the authority,
        // because it moves inside the transaction that mints the epoch's roles.
        let epoch = match cluster::live_schema_epoch(admin, &declaration.database).await {
            Ok(epoch) => epoch,
            Err(error) => return Ok(Err(error.to_string())),
        };

        let revoking = declaration.status == BINDING_STATUS_REVOKING
            || declaration.status == BINDING_STATUS_REVOKED;
        if revoking {
            if let Err(error) = cluster::revoke_binding(
                admin,
                &declaration.binding,
                &declaration.database,
                capability,
                epoch,
            )
            .await
            {
                return Ok(Err(error.to_string()));
            }
            if self
                .control
                .observe_binding(
                    &declaration.binding,
                    declaration.generation,
                    BINDING_STATUS_REVOKED,
                )
                .await?
            {
                report.bindings_revoked.push(declaration.binding.clone());
            }
            return Ok(Ok(()));
        }

        if declaration.status != BINDING_STATUS_PENDING
            && declaration.status != BINDING_STATUS_ACTIVE
        {
            return Ok(Ok(()));
        }
        if let Err(error) = cluster::grant_binding(
            admin,
            &declaration.binding,
            &declaration.database,
            capability,
            epoch,
        )
        .await
        {
            return Ok(Err(error.to_string()));
        }
        if self
            .control
            .observe_binding(
                &declaration.binding,
                declaration.generation,
                BINDING_STATUS_ACTIVE,
            )
            .await?
        {
            report.bindings_activated.push(declaration.binding.clone());
        }
        Ok(Ok(()))
    }

    /// Drop the binding roles no declaration names.
    ///
    /// The declaration set carries EVERY status, `revoked` included, because a
    /// revoked binding's role has to survive: the data plane separates a
    /// revoked binding (`42501`) from a retired schema epoch (`22023`) by which
    /// error `SET LOCAL ROLE` returns, and sweeping the role would report the
    /// first as the second.
    ///
    /// A binding role at an epoch other than the live one is NOT swept either.
    /// Retiring an epoch is the apply path's job and it is ordered: an apply
    /// that cannot drop `E-1` refuses before any DDL commits. A reap that
    /// dropped one out of band would break an isolate that is still serving.
    async fn reap(
        &self,
        admin: &Client,
        declarations: &Declarations,
        report: &mut PassReport,
    ) -> Result<(), ReconcileError> {
        let declared: Vec<&BindingId> = declarations
            .bindings()
            .iter()
            .map(|binding| &binding.binding)
            .collect();
        for (name, classified) in cluster::classify_platform_roles(admin).await? {
            match classified {
                ClusterRole::Binding { binding, .. } => {
                    if declared.iter().any(|declared| **declared == binding) {
                        continue;
                    }
                    cluster::drop_binding_role(admin, &name).await?;
                    report.roles_reaped.push(name);
                }
                ClusterRole::Database { database } => {
                    if !declarations
                        .databases()
                        .iter()
                        .any(|declared| declared.database == database)
                    {
                        // REPORTED, NEVER DROPPED. This role owns a schema that
                        // holds tenant data, and an absence is not a statement
                        // that the data may go.
                        report.undeclared_database_objects.push(name);
                    }
                }
                ClusterRole::Unattributed => report.unattributed_roles.push(name),
            }
        }
        for (schema, database) in cluster::database_schemas(admin).await? {
            if !declarations
                .databases()
                .iter()
                .any(|declared| declared.database == database)
            {
                report.undeclared_database_objects.push(schema);
            }
        }
        Ok(())
    }
}
