//! The control-plane side of the reconciler: what a cluster is told to be.
//!
//! Control DECLARES and a cluster is made to match. Nothing spans the two -
//! control's database and a tenant cluster are different servers - so every
//! statement here is its own commit and `generation` / `observed_generation`
//! is how a reader tells a declared binding from a live one. A design that
//! claimed one transaction over both would be claiming a distributed
//! transaction it does not have.
//!
//! # Control holds no DSN, and this module writes no DSN
//!
//! A `zeroship.datastores` row carries the cluster's own identity, its zone and
//! its status. The credential stays in the config of the service that holds it,
//! which is this one. [`ControlStore::register_datastore`] is the only inserting
//! path into that table, and it is reached only by a process that has already
//! connected to the cluster it is registering.
//!
//! # Why a [`Declarations`] value exists at all
//!
//! The reap drops roles no declaration names, so the declaration set is a
//! deletion authority. A read that failed, or one that returned early, is
//! indistinguishable downstream from a cluster with nothing declared on it, and
//! reaping against that would revoke every live app's access to its own data.
//! [`Declarations`] is therefore constructible only by
//! [`ControlStore::read_declarations`], which returns it only when BOTH reads
//! completed. A caller that could not read has no value to reap against, so the
//! sweep is unreachable rather than merely discouraged.

use std::sync::Arc;

use compio_postgres::Client;
use zeroship_core::database_role::DatabaseCapability;
use zeroship_core::{BindingId, DatabaseId};
use zeroship_id::DatastoreId;

use super::identity::ClusterIdentity;

/// The status a registered cluster starts at.
pub const DATASTORE_STATUS_PENDING: &str = "pending";
/// The status a bootstrapped cluster carries. Placement admits this and
/// nothing else.
pub const DATASTORE_STATUS_ACTIVE: &str = "active";

/// The status a declared database starts at.
pub const DATABASE_STATUS_PROVISIONING: &str = "provisioning";
/// The status a converged database carries.
pub const DATABASE_STATUS_ACTIVE: &str = "active";
/// The one declaration that authorizes destroying a schema.
///
/// A schema is NEVER dropped by absence. A reaped role is recoverable - the
/// next pass re-grants it - and a dropped schema is not, so the two cannot
/// share a rule: a read that failed looks exactly like a cluster with nothing
/// declared on it, and answering that by dropping schemas destroys tenant data.
/// This status is the explicit statement that a particular database is to go.
pub const DATABASE_STATUS_DELETING: &str = "deleting";

/// The status a declared binding starts at.
pub const BINDING_STATUS_PENDING: &str = "pending";
/// The status a converged binding carries.
pub const BINDING_STATUS_ACTIVE: &str = "active";
/// An operator or surface has asked for this binding's edges to be withdrawn.
pub const BINDING_STATUS_REVOKING: &str = "revoking";
/// The edges are withdrawn, and the ROLE SURVIVES.
///
/// The data plane's error taxonomy reads `42501` (the role exists, this session
/// may not assume it) as the terminal revocation, so dropping the role on
/// revoke would answer `22023` (no such role) instead - the generic refusal an
/// unconverged database also produces.
pub const BINDING_STATUS_REVOKED: &str = "revoked";

/// Reader and writer for the three control-plane tables this reconciler drives.
#[derive(Clone)]
pub struct ControlStore {
    client: Arc<Client>,
}

impl std::fmt::Debug for ControlStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ControlStore").finish_non_exhaustive()
    }
}

/// A control-plane read or write that did not complete.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    /// The control database refused or could not answer.
    #[error("control-plane query: {0}")]
    Query(#[from] compio_postgres::Error),
    /// A stored id is not a canonical typed id.
    #[error("control-plane row carries an unusable {column} `{value}`")]
    UnusableId { column: &'static str, value: String },
    /// This cluster is already registered in a different execution zone.
    ///
    /// Registration is keyed on the cluster's identity, so this is a service
    /// configured against a cluster another zone already claims. Silently
    /// moving the row would move placement for every database on it.
    #[error(
        "cluster {system_identifier} is registered as {datastore} in execution zone \
         {recorded_zone}, and this service declares zone {declared_zone}; a datastore does not \
         change zones"
    )]
    ZoneConflict {
        system_identifier: i64,
        datastore: String,
        recorded_zone: String,
        declared_zone: String,
    },
    /// The insert reported a conflict and the conflicting row then vanished.
    #[error("cluster {system_identifier} conflicted on registration and has no row")]
    RegistrationVanished { system_identifier: i64 },
}

/// The registry row for one cluster, as it stands after registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatastoreRegistration {
    pub datastore: DatastoreId,
    pub execution_zone_id: String,
    pub status: String,
    /// This pass created the row. A second pass over a registered cluster
    /// reports `false` and writes nothing at all.
    pub created: bool,
}

/// One database control has placed on this cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseDeclaration {
    pub database: DatabaseId,
    pub status: String,
}

/// One app's declared edge to a database on this cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingDeclaration {
    pub binding: BindingId,
    pub database: DatabaseId,
    /// `None` when the stored text is not one of the two the CHECK admits.
    /// The row still DECLARES its role, so the reap must not sweep it.
    pub capability: Option<DatabaseCapability>,
    pub capability_text: String,
    pub status: String,
    pub generation: i32,
    pub observed_generation: i32,
}

/// Everything declared on one datastore, read completely.
///
/// There is no constructor but [`ControlStore::read_declarations`], and it
/// returns this only when both underlying reads completed. That is the whole
/// point of the type: an empty `Declarations` means "control declares nothing
/// here", never "the read did not finish".
#[derive(Debug, Clone)]
pub struct Declarations {
    datastore: DatastoreId,
    databases: Vec<DatabaseDeclaration>,
    bindings: Vec<BindingDeclaration>,
}

impl Declarations {
    #[must_use]
    pub const fn datastore(&self) -> &DatastoreId {
        &self.datastore
    }

    #[must_use]
    pub fn databases(&self) -> &[DatabaseDeclaration] {
        &self.databases
    }

    #[must_use]
    pub fn bindings(&self) -> &[BindingDeclaration] {
        &self.bindings
    }
}

impl ControlStore {
    #[must_use]
    pub const fn new(client: Arc<Client>) -> Self {
        Self { client }
    }

    /// Register this cluster, or find the row that already registers it.
    ///
    /// Keyed on the cluster's own `system_identifier`, so two services holding
    /// one cluster's credential converge on one row. The insert is
    /// `ON CONFLICT DO NOTHING` rather than an upsert: a second pass over a
    /// registered cluster must write NOTHING, and an upsert that only touched
    /// `updated_at` would still be a write on every pass.
    ///
    /// # Errors
    ///
    /// [`ControlError::ZoneConflict`] when the cluster is already registered in
    /// another zone, [`ControlError::Query`] on any database failure.
    pub async fn register_datastore(
        &self,
        identity: ClusterIdentity,
        execution_zone_id: &str,
    ) -> Result<DatastoreRegistration, ControlError> {
        let minted = DatastoreId::mint();
        let rows = self
            .client
            .query(
                "WITH inserted AS ( \
                     INSERT INTO zeroship.datastores \
                         (id, system_identifier, execution_zone_id, status) \
                     VALUES ($1::text, $2::bigint, $3::text, $4::text) \
                     ON CONFLICT (system_identifier) DO NOTHING \
                     RETURNING id, execution_zone_id, status \
                 ) \
                 SELECT id, execution_zone_id, status, true AS created FROM inserted \
                 UNION ALL \
                 SELECT id, execution_zone_id, status, false AS created \
                   FROM zeroship.datastores \
                  WHERE system_identifier = $2::bigint \
                    AND NOT EXISTS (SELECT 1 FROM inserted)",
                &[
                    &minted.as_str(),
                    &identity.system_identifier(),
                    &execution_zone_id,
                    &DATASTORE_STATUS_PENDING,
                ],
            )
            .await?;
        let row = rows
            .first()
            .ok_or(ControlError::RegistrationVanished {
                system_identifier: identity.system_identifier(),
            })?;
        let id: String = row.get("id");
        let recorded_zone: String = row.get("execution_zone_id");
        if recorded_zone != execution_zone_id {
            return Err(ControlError::ZoneConflict {
                system_identifier: identity.system_identifier(),
                datastore: id,
                recorded_zone,
                declared_zone: execution_zone_id.to_owned(),
            });
        }
        Ok(DatastoreRegistration {
            datastore: parse_id(DatastoreId::parse, "datastores.id", &id)?,
            execution_zone_id: recorded_zone,
            status: row.get("status"),
            created: row.get("created"),
        })
    }

    /// Move a bootstrapped cluster into rotation.
    ///
    /// Guarded on `pending`, so a pass over an already-active cluster reports
    /// `false` and writes nothing. `draining`, `retired` and `failed` are the
    /// operator's lever and this never overrides one.
    ///
    /// # Errors
    /// [`ControlError::Query`] on any database failure.
    pub async fn activate_datastore(&self, datastore: &DatastoreId) -> Result<bool, ControlError> {
        let affected = self
            .client
            .execute(
                "UPDATE zeroship.datastores \
                    SET status = $2::text, last_error = NULL, updated_at = now() \
                  WHERE id = $1::text AND status = $3::text",
                &[
                    &datastore.as_str(),
                    &DATASTORE_STATUS_ACTIVE,
                    &DATASTORE_STATUS_PENDING,
                ],
            )
            .await?;
        Ok(affected == 1)
    }

    /// Record why a cluster could not be bootstrapped, leaving its status alone.
    ///
    /// The status stays `pending`, because a cluster that failed to bootstrap is
    /// a cluster that is not bootstrapped. Placement already refuses it.
    ///
    /// # Errors
    /// [`ControlError::Query`] on any database failure.
    pub async fn record_datastore_error(
        &self,
        datastore: &DatastoreId,
        message: &str,
    ) -> Result<(), ControlError> {
        self.set_datastore_error(datastore, Some(message)).await
    }

    /// Withdraw a recorded failure once a pass completed cleanly.
    ///
    /// Writes NULL rather than an empty string: `last_error` is read as "is
    /// there a failure", and an empty string is a value, so clearing to one
    /// would leave every recovered cluster looking like it had failed with a
    /// message nobody wrote.
    ///
    /// # Errors
    /// [`ControlError::Query`] on any database failure.
    pub async fn clear_datastore_error(
        &self,
        datastore: &DatastoreId,
    ) -> Result<(), ControlError> {
        self.set_datastore_error(datastore, None).await
    }

    /// Guarded on the value actually changing, so a converged pass writes
    /// nothing.
    async fn set_datastore_error(
        &self,
        datastore: &DatastoreId,
        message: Option<&str>,
    ) -> Result<(), ControlError> {
        self.client
            .execute(
                "UPDATE zeroship.datastores \
                    SET last_error = $2::text, updated_at = now() \
                  WHERE id = $1::text AND last_error IS DISTINCT FROM $2::text",
                &[&datastore.as_str(), &message],
            )
            .await?;
        Ok(())
    }

    /// Everything control declares on this datastore, read completely.
    ///
    /// Both reads must complete. A failure in either one returns an error and
    /// therefore yields no [`Declarations`], which is what keeps the reap from
    /// reading a failed read as an empty cluster.
    ///
    /// Every status is returned, not only the unconverged ones. A `revoked`
    /// binding still NAMES its role, and the reap drops roles no declaration
    /// names; filtering here would make the reap destroy the very role the
    /// revoked state exists to keep.
    ///
    /// # Errors
    /// [`ControlError::Query`] on any database failure,
    /// [`ControlError::UnusableId`] on a stored id that is not canonical.
    pub async fn read_declarations(
        &self,
        datastore: &DatastoreId,
    ) -> Result<Declarations, ControlError> {
        let database_rows = self
            .client
            .query(
                "SELECT id, status \
                   FROM zeroship.databases \
                  WHERE datastore_id = $1::text \
                  ORDER BY id",
                &[&datastore.as_str()],
            )
            .await?;
        let mut databases = Vec::with_capacity(database_rows.len());
        for row in &database_rows {
            let id: String = row.get("id");
            databases.push(DatabaseDeclaration {
                database: parse_id(DatabaseId::parse, "databases.id", &id)?,
                status: row.get("status"),
            });
        }

        let binding_rows = self
            .client
            .query(
                "SELECT binding.id, \
                        binding.database_id, \
                        binding.capability, \
                        binding.status, \
                        binding.generation, \
                        binding.observed_generation \
                   FROM zeroship.database_bindings binding \
                   JOIN zeroship.databases database ON database.id = binding.database_id \
                  WHERE database.datastore_id = $1::text \
                  ORDER BY binding.id",
                &[&datastore.as_str()],
            )
            .await?;
        let mut bindings = Vec::with_capacity(binding_rows.len());
        for row in &binding_rows {
            let id: String = row.get("id");
            let database_id: String = row.get("database_id");
            let capability_text: String = row.get("capability");
            bindings.push(BindingDeclaration {
                binding: parse_id(BindingId::parse, "database_bindings.id", &id)?,
                database: parse_id(
                    DatabaseId::parse,
                    "database_bindings.database_id",
                    &database_id,
                )?,
                capability: DatabaseCapability::from_wire(&capability_text),
                capability_text,
                status: row.get("status"),
                generation: row.get("generation"),
                observed_generation: row.get("observed_generation"),
            });
        }

        Ok(Declarations {
            datastore: datastore.clone(),
            databases,
            bindings,
        })
    }

    /// Mark a database converged.
    ///
    /// Guarded on `provisioning`, so a second pass writes nothing and a
    /// `draining` or `deleting` database is never dragged back into service.
    ///
    /// # Errors
    /// [`ControlError::Query`] on any database failure.
    pub async fn activate_database(&self, database: &DatabaseId) -> Result<bool, ControlError> {
        let affected = self
            .client
            .execute(
                "UPDATE zeroship.databases \
                    SET status = $2::text, updated_at = now() \
                  WHERE id = $1::text AND status = $3::text",
                &[
                    &database.as_str(),
                    &DATABASE_STATUS_ACTIVE,
                    &DATABASE_STATUS_PROVISIONING,
                ],
            )
            .await?;
        Ok(affected == 1)
    }

    /// Mark a binding's edges live at the generation that was converged.
    ///
    /// The generation is part of the predicate, not only of the assignment: a
    /// declaration that moved while this pass was granting must not be reported
    /// as observed. It stays behind and the next pass converges it.
    ///
    /// # Errors
    /// [`ControlError::Query`] on any database failure.
    pub async fn observe_binding(
        &self,
        binding: &BindingId,
        generation: i32,
        status: &str,
    ) -> Result<bool, ControlError> {
        let affected = self
            .client
            .execute(
                "UPDATE zeroship.database_bindings \
                    SET status = $3::text, \
                        observed_generation = $2::int, \
                        last_error = NULL, \
                        updated_at = now() \
                  WHERE id = $1::text \
                    AND generation = $2::int \
                    AND (status IS DISTINCT FROM $3::text OR observed_generation <> $2::int)",
                &[&binding.as_str(), &generation, &status],
            )
            .await?;
        Ok(affected == 1)
    }

    /// The bindings that name one database, read NOW.
    ///
    /// The teardown precondition reads this rather than the pass's declaration
    /// snapshot, because the two destructive statements it gates run after that
    /// snapshot was taken and `zeroship_control::databases::bind` carries no
    /// predicate on a database's status: an app can be bound to a database that
    /// is already `deleting`. Reading immediately before the drop narrows the
    /// window to the drop itself; closing it entirely is a predicate on `bind`,
    /// which belongs to that surface.
    ///
    /// # Errors
    /// [`ControlError::Query`] on any database failure. A failure here refuses
    /// the teardown, which is the direction that keeps data.
    pub async fn bindings_naming(&self, database: &DatabaseId) -> Result<Vec<String>, ControlError> {
        let rows = self
            .client
            .query(
                "SELECT id FROM zeroship.database_bindings \
                  WHERE database_id = $1::text ORDER BY id",
                &[&database.as_str()],
            )
            .await?;
        Ok(rows.iter().map(|row| row.get("id")).collect())
    }

    /// Remove a database row whose schema and roles are gone.
    ///
    /// Guarded on `deleting`, so this can never remove a row that still
    /// declares a live database. The row goes LAST: while it stands, a pass
    /// that died part way through the teardown re-reads it and finishes the
    /// job, and a row removed first would leave the schema an orphan nothing
    /// names.
    ///
    /// # Errors
    /// [`ControlError::Query`] on any database failure - including the binding
    /// foreign key refusing while an app still holds an edge.
    pub async fn remove_deleted_database(
        &self,
        database: &DatabaseId,
    ) -> Result<bool, ControlError> {
        let affected = self
            .client
            .execute(
                "DELETE FROM zeroship.databases WHERE id = $1::text AND status = $2::text",
                &[&database.as_str(), &DATABASE_STATUS_DELETING],
            )
            .await?;
        Ok(affected == 1)
    }

    /// The deployment's sole active execution zone, or why there is not one.
    ///
    /// # Errors
    /// [`ControlError::Query`] on any database failure.
    pub async fn sole_active_zone(&self) -> Result<SoleZone, ControlError> {
        let rows = self
            .client
            .query(
                "SELECT id FROM zeroship.execution_zones WHERE status = 'active' ORDER BY id",
                &[],
            )
            .await?;
        match rows.len() {
            0 => Ok(SoleZone::None),
            1 => Ok(SoleZone::One(rows[0].get("id"))),
            many => Ok(SoleZone::Many(many)),
        }
    }

    /// Record why a binding could not be converged, leaving its generation alone.
    ///
    /// # Errors
    /// [`ControlError::Query`] on any database failure.
    pub async fn record_binding_error(
        &self,
        binding: &BindingId,
        message: &str,
    ) -> Result<(), ControlError> {
        self.client
            .execute(
                "UPDATE zeroship.database_bindings \
                    SET last_error = $2::text, updated_at = now() \
                  WHERE id = $1::text AND last_error IS DISTINCT FROM $2::text",
                &[&binding.as_str(), &message],
            )
            .await?;
        Ok(())
    }
}

/// How many active execution zones this deployment declares.
///
/// A cluster registers itself because reaching it proves it exists. A ZONE
/// proves nothing - it gates which join signers may mint workers - so a service
/// may not invent one, and with more than one declared it may not choose
/// either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoleZone {
    None,
    One(String),
    Many(usize),
}

fn parse_id<T, E>(
    parse: impl Fn(&str) -> Result<T, E>,
    column: &'static str,
    value: &str,
) -> Result<T, ControlError> {
    parse(value).map_err(|_| ControlError::UnusableId {
        column,
        value: value.to_owned(),
    })
}
