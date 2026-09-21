//! The schema-epoch rotation an apply performs.
//!
//! `docs/proposals/2026-08-28-app-database-decoupling.md` fixes the sequence,
//! and the ordering rule it states is the whole design: **all subtraction from
//! the catalog happens before any DDL commits; all addition happens after every
//! DDL has committed.**
//!
//! ```text
//! L   the session advisory lock on the database key, held to U
//! P   preflight: lower every IR file, refuse a denied plan
//! T1  one transaction: head FOR UPDATE, reap the E-1 roles
//! D   the DDL, engine-journalled
//! T4  one transaction: mint E+1 for every live binding, advance the head to
//!     E+1 - iff the committed schema delta requires rotation
//! E   advance databases.schema_epoch on the control connection
//! U   release the lock
//! ```
//!
//! # A serving app is never fenced
//!
//! The apply mints `E+1` and drops `E-1`; it never touches `E`. Every partial
//! crash state therefore leaves apps serving on `E`, and recovery is a plain
//! retry: the engine journal skips completed DDL and the head's recorded
//! frontier decides whether the rotation is still owed. A retry after a crash
//! between the last DDL and [`rotate_if_owed`] finds every version applied and
//! still rotates, because the frontier moved and the head did not.
//!
//! # Live epochs are capped at two, fail-closed
//!
//! [`retire_previous_epoch`] runs before any creator DDL commits, so an apply
//! that cannot drop `E-1` refuses outright and never advances to `E+1`. The
//! alternative - advance anyway - leaves three live epochs, and the third is
//! one an isolate two deploys behind can still assume.
//!
//! That is a CAP and not a floor, and the asymmetry is forced rather than
//! chosen: the retirement is unconditional where the mint is not, because at
//! T1 nothing yet knows whether a delta will follow, and a retirement that
//! waited to find out would be running after the DDL it has to precede. So an
//! apply that commits nothing still retires `E-1`, which narrows the window an
//! isolate on `E-1` has to re-resolve in and fences nothing that is serving on
//! `E`.
//!
//! # The cluster's own catalog decides who gets a role
//!
//! Which bindings are live on this database is read from `pg_auth_members`
//! rather than from control. Two reasons, and neither is convenience: the
//! apply holds a session lock over a whole multi-file plan and must not make
//! that span a second server, and a role that is a member of this database's
//! capability role AND granted to the shared worker login is the definition of
//! an assumable binding rather than a report about one. A binding whose edges
//! were withdrawn is therefore not carried forward, and a binding whose role
//! the reap left standing so `SET LOCAL ROLE` answers `42501` stays standing.

use compio_postgres::{Client, NoTls};
use std::sync::Arc;
use zeroship_core::database_derivation;
use zeroship_core::database_role::DatabaseCapability;
use zeroship_core::{BindingId, DatabaseId};

use crate::apply::{quote_ident, WORKER_ROLE};
use crate::datastore::cluster::{
    self, epoch_table, ClusterError, ClusterRole, DatabaseRoles,
};
use crate::datastore::control::{ControlError, ControlStore};

/// The engine's journal of record, in the database's own schema.
///
/// Its highest `event_seq` is the schema delta's high-water mark: the column is
/// `GENERATED ALWAYS AS IDENTITY` on an append-only table, so an applied
/// migration moves it and a fully skipped apply does not. Naming the table here
/// couples this to the engine deliberately; a rename makes the rotation fail
/// with `42P01` on a table `PostgreSQL` names, rather than silently deciding no
/// rotation is owed.
///
/// The error it can make has one direction: a frontier that moved without a
/// shape changing costs a rotation nothing needed, and nothing is fenced by it.
/// The opposite - a shape that moved without the frontier - is unreachable,
/// because no DDL reaches this schema except through the engine that journals
/// it, and the apply runs as the only role that owns the schema.
const ENGINE_JOURNAL: &str = "__zeroship_schema_migrations";

/// The head of one database's epoch row, read under `FOR UPDATE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Head {
    epoch: i32,
    journal_frontier: i64,
}

/// What [`retire_previous_epoch`] withdrew.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retired {
    /// The head this apply runs against. Apps serving on it are untouched.
    pub epoch: i32,
    /// The `E-1` binding roles this dropped, in catalog order.
    pub roles: Vec<String>,
}

/// What [`rotate_if_owed`] did with the head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rotation {
    /// The journal moved past the frontier the head records, so the head
    /// advanced and every live binding gained a role at the new epoch.
    Advanced {
        from: i32,
        to: i32,
        minted: Vec<String>,
    },
    /// The committed schema delta is already the one the head covers. Nothing
    /// was minted and the head did not move.
    NotOwed { epoch: i32 },
}

impl Rotation {
    /// The epoch the head carries after this call, whichever arm ran.
    #[must_use]
    pub const fn epoch(&self) -> i32 {
        match self {
            Self::Advanced { to, .. } => *to,
            Self::NotOwed { epoch } => *epoch,
        }
    }
}

/// A projection of a rotated head onto the control plane that did not land.
///
/// Never fatal to an apply. The head on the cluster is the authority; control's
/// copy exists so a binding can be composed without a cross-zone read, and a
/// stale copy composes a role name that does not exist, which the caller
/// re-resolves.
#[derive(Debug, thiserror::Error)]
pub enum ProjectionError {
    #[error("connect to the control plane: {0}")]
    Connect(compio_postgres::Error),
    #[error(transparent)]
    Control(#[from] ControlError),
}

/// T1. Claim the head and drop the binding roles of the epoch it retires.
///
/// Runs BEFORE any creator DDL commits, and its failure refuses the apply. It
/// drops the roles of the epoch BEFORE the head - one shape behind what the
/// database has now, two behind whatever this apply may mint - and never the
/// head's own, so nothing serving on `E` is fenced.
///
/// A head at `0` has no predecessor and nothing is dropped.
///
/// # Errors
///
/// [`ClusterError::EpochMissing`] when the database has no head - the reap
/// cannot name what it would drop, so the apply must not proceed;
/// [`ClusterError::Query`] when the cluster refuses the read or the drop.
pub async fn retire_previous_epoch(
    admin: &Client,
    database: &DatabaseId,
) -> Result<Retired, ClusterError> {
    admin.batch_execute("BEGIN").await?;
    match retire_in_transaction(admin, database).await {
        Ok(retired) => {
            admin.batch_execute("COMMIT").await?;
            Ok(retired)
        }
        Err(error) => {
            let _ = admin.batch_execute("ROLLBACK").await;
            Err(error)
        }
    }
}

async fn retire_in_transaction(
    admin: &Client,
    database: &DatabaseId,
) -> Result<Retired, ClusterError> {
    let head = claim_head(admin, database).await?;
    if head.epoch <= 0 {
        return Ok(Retired {
            epoch: head.epoch,
            roles: Vec::new(),
        });
    }
    let retiring = head.epoch - 1;
    let mut roles = Vec::new();
    for edge in binding_edges(admin, database, retiring).await? {
        if roles.contains(&edge.role) {
            continue;
        }
        admin
            .batch_execute(&cluster::drop_binding_role_sql(&edge.role))
            .await?;
        roles.push(edge.role);
    }
    Ok(Retired {
        epoch: head.epoch,
        roles,
    })
}

/// T4. Mint `E+1` for every live binding and advance the head, iff the
/// committed schema delta requires it.
///
/// Runs after every DDL of this apply has committed. The decision is the head's
/// recorded journal frontier against the journal's frontier NOW, which is what
/// makes a retry after a crash between the last DDL and this call still rotate,
/// and a re-post of an already-applied bundle not rotate at all.
///
/// # Errors
///
/// [`ClusterError::EpochMissing`] when the database has no head,
/// [`ClusterError::EpochExhausted`] when the head has no successor,
/// [`ClusterError::RoleName`] on a name `PostgreSQL` would truncate,
/// [`ClusterError::Query`] when the cluster refuses any statement.
pub async fn rotate_if_owed(
    admin: &Client,
    database: &DatabaseId,
) -> Result<Rotation, ClusterError> {
    admin.batch_execute("BEGIN").await?;
    match rotate_in_transaction(admin, database).await {
        Ok(rotation) => {
            admin.batch_execute("COMMIT").await?;
            Ok(rotation)
        }
        Err(error) => {
            let _ = admin.batch_execute("ROLLBACK").await;
            Err(error)
        }
    }
}

async fn rotate_in_transaction(
    admin: &Client,
    database: &DatabaseId,
) -> Result<Rotation, ClusterError> {
    let head = claim_head(admin, database).await?;
    let frontier = journal_frontier(admin, &database_derivation::schema_name(database)).await?;
    if frontier == head.journal_frontier {
        return Ok(Rotation::NotOwed { epoch: head.epoch });
    }
    let next = head
        .epoch
        .checked_add(1)
        .ok_or_else(|| ClusterError::EpochExhausted {
            database: database.as_str().to_owned(),
            epoch: head.epoch,
        })?;

    let roles = DatabaseRoles::derive(database)?;
    let mut minted: Vec<String> = Vec::new();
    for edge in binding_edges(admin, database, head.epoch).await? {
        // The worker edge is half of what makes a binding assumable, and a
        // binding the reconciler is part way through revoking has lost it. The
        // rotation carries live edges forward; it does not re-open one.
        if !edge.assumable {
            continue;
        }
        let name = cluster::binding_role(&edge.binding, next)?;
        // One statement set per capability edge the head's role holds, so the
        // memberships are carried forward exactly rather than narrowed to the
        // one this code would have chosen.
        for statement in
            cluster::grant_binding_statements(&name, roles.for_capability(edge.capability))
        {
            admin.batch_execute(&statement).await?;
        }
        if !minted.contains(&name) {
            minted.push(name);
        }
    }

    admin
        .execute(
            &format!(
                "UPDATE {} SET schema_epoch = $2::int, journal_frontier = $3::bigint, \
                        updated_at = now() \
                  WHERE database_id = $1::text",
                epoch_table()
            ),
            &[&database.as_str(), &next, &frontier],
        )
        .await?;
    Ok(Rotation::Advanced {
        from: head.epoch,
        to: next,
        minted,
    })
}

/// E. Project a rotated head onto `zeroship.databases`.
///
/// The one control-plane write the apply performs, and the only step that
/// leaves the cluster. It opens its own connection: this service holds no
/// shared control client, exactly as `BindingStore` does not.
///
/// Monotone, so a late write cannot walk the projection backwards past a
/// rotation that has already happened. Returns whether the row moved.
///
/// # Errors
///
/// [`ProjectionError::Connect`] when control will not connect,
/// [`ProjectionError::Control`] when it will not take the write. Neither is
/// fatal to the apply that produced the epoch.
pub async fn project_schema_epoch(
    control_dsn: &str,
    database: &DatabaseId,
    epoch: i32,
) -> Result<bool, ProjectionError> {
    let (client, connection) = compio_postgres::connect(control_dsn, NoTls)
        .await
        .map_err(ProjectionError::Connect)?;
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            tracing::debug!(%error, "migrate-server: epoch projection connection ended");
        }
    })
    .detach();
    Ok(ControlStore::new(Arc::new(client))
        .project_schema_epoch(database, epoch)
        .await?)
}

/// Read one database's head and lock the row for the rest of the transaction.
///
/// `FOR UPDATE` and not a plain read: the head is what every binding role name
/// this transaction composes is derived from, and a concurrent writer moving it
/// between the read and the mint would leave roles at an epoch the row does not
/// name.
async fn claim_head(admin: &Client, database: &DatabaseId) -> Result<Head, ClusterError> {
    let row = admin
        .query_opt(
            &format!(
                "SELECT schema_epoch, journal_frontier FROM {} \
                  WHERE database_id = $1::text FOR UPDATE",
                epoch_table()
            ),
            &[&database.as_str()],
        )
        .await?
        .ok_or_else(|| ClusterError::EpochMissing {
            database: database.as_str().to_owned(),
        })?;
    Ok(Head {
        epoch: row.get("schema_epoch"),
        journal_frontier: row.get("journal_frontier"),
    })
}

/// The engine journal's high-water mark in one schema.
async fn journal_frontier(admin: &Client, schema: &str) -> Result<i64, ClusterError> {
    let row = admin
        .query_one(
            &format!(
                "SELECT coalesce(max(event_seq), 0)::bigint AS frontier FROM {}.{}",
                quote_ident(schema),
                quote_ident(ENGINE_JOURNAL)
            ),
            &[],
        )
        .await?;
    Ok(row.get("frontier"))
}

/// One binding role's membership in one database, as the catalog records it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BindingEdge {
    role: String,
    binding: BindingId,
    capability: DatabaseCapability,
    /// The shared worker login may assume this role. Without it the role exists
    /// and nothing can narrow to it, which is what a withdrawn binding leaves
    /// behind.
    assumable: bool,
}

/// Every binding role at one epoch that holds one of this database's capability
/// roles.
///
/// The catalog predicate only narrows the scan: what a name MEANS is decided by
/// [`cluster::classify_role_name`], which recomposes it and demands the same
/// bytes back, so a role that merely looks like a binding at this epoch is not
/// one.
async fn binding_edges(
    admin: &Client,
    database: &DatabaseId,
    epoch: i32,
) -> Result<Vec<BindingEdge>, ClusterError> {
    let Ok(epoch) = u32::try_from(epoch) else {
        return Ok(Vec::new());
    };
    let roles = DatabaseRoles::derive(database)?;
    let rows = admin
        .query(
            "SELECT binding_role.rolname AS binding_role, \
                    capability.rolname   AS capability_role, \
                    EXISTS ( \
                        SELECT 1 FROM pg_auth_members worker_edge \
                          JOIN pg_roles worker ON worker.oid = worker_edge.member \
                         WHERE worker_edge.roleid = binding_role.oid \
                           AND worker.rolname = $3::text \
                    ) AS assumable \
               FROM pg_auth_members capability_edge \
               JOIN pg_roles capability   ON capability.oid   = capability_edge.roleid \
               JOIN pg_roles binding_role ON binding_role.oid = capability_edge.member \
              WHERE (capability.rolname = $1::text OR capability.rolname = $2::text) \
              ORDER BY binding_role.rolname, capability.rolname",
            &[&roles.readwrite, &roles.readonly, &WORKER_ROLE],
        )
        .await?;

    let mut edges = Vec::new();
    for row in &rows {
        let role: String = row.get("binding_role");
        let ClusterRole::Binding {
            binding,
            epoch: found,
        } = cluster::classify_role_name(&role)
        else {
            continue;
        };
        if found != epoch {
            continue;
        }
        let capability_role: String = row.get("capability_role");
        let capability = if capability_role == roles.readwrite {
            DatabaseCapability::ReadWrite
        } else if capability_role == roles.readonly {
            DatabaseCapability::ReadOnly
        } else {
            continue;
        };
        edges.push(BindingEdge {
            role,
            binding,
            capability,
            assumable: row.get("assumable"),
        });
    }
    Ok(edges)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_head_after_a_rotation_is_the_epoch_that_was_minted() {
        assert_eq!(
            Rotation::Advanced {
                from: 4,
                to: 5,
                minted: vec!["zs_bind_x_e5".to_owned()],
            }
            .epoch(),
            5,
            "an advanced head reports the epoch it advanced TO, which is the one \
             a projection must carry"
        );
        assert_eq!(
            Rotation::NotOwed { epoch: 4 }.epoch(),
            4,
            "a rotation that was not owed leaves the head where it stands"
        );
    }
}
