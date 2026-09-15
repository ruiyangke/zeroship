//! Control-owned placement eligibility, read through an injected capability.
//!
//! An app belongs to exactly one execution zone, fixed when Control creates it.
//! A worker instance belongs to exactly one too: the `zone` claim of the join
//! token Control verified, resolved to an id and frozen on the instance row.
//! The manager reads both facts, and the instance's liveness, from rows no
//! worker can write. Registration carries no zone and nothing a worker sends
//! can change either fact.
//!
//! The queue's database binding covers only the manager's own schema, so the
//! facts arrive through [`EligibilitySource`], the same seam the host's join
//! callback uses. Placement consults it after taking its locks and again
//! before commit. Zones never change, and liveness only moves forward: an
//! instance is revoked, retired, or its lease runs out, and none of those
//! reverse. A change committed before the first read is seen, one committed
//! between the reads is caught by the second, and one committed after the
//! second is caught by the next renewal, delivery or placement check. That is
//! an eventual admission fence, the same one the join path has.
#![expect(
    clippy::future_not_send,
    reason = "Control reads stay on the owning compio runtime"
)]

use crate::Error;
use std::{fmt::Debug, future::Future, pin::Pin};
use zeroship_core::{app_id::AppId, typed_id, workflow_coordination::WorkerId};
use zeroship_data_orm::{
    orm::{Database, FromRow},
    schema::Schema,
};

/// The typed id prefix of `zeroship.execution_zones`.
const ZONE_PREFIX: &str = "ezn";

/// The fixed identity of the single seeded zone row.
const DEFAULT_ZONE: &str = "ezn_default000000000000000000";

/// An operator-declared execution zone: worker deployment units that share
/// creator-side connectivity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ZoneId(String);

impl ZoneId {
    /// # Errors
    /// Refuses text that is not a canonical execution zone id.
    pub fn parse(text: &str) -> Result<Self, Error> {
        typed_id::parse_with_prefix(text, ZONE_PREFIX).map_err(|_| Error::Storage)?;
        Ok(Self(text.to_owned()))
    }

    /// A fresh zone identity, for hosts that provision their own zone rows.
    #[must_use]
    pub fn mint() -> Self {
        Self(typed_id::generate(ZONE_PREFIX))
    }

    /// The zone a single-zone deployment seeds
    /// (`db/migrations-ts/20260914000450_execution_zones_default_zone.ts`).
    /// The local host composes its in-process worker into it.
    #[must_use]
    pub fn default_zone() -> Self {
        Self(DEFAULT_ZONE.to_owned())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Control's placement facts for one app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppFacts {
    pub zone: ZoneId,
    /// Terminal deletion. Archived apps stay placeable for maintenance jobs;
    /// deleted apps are abandoned and never placed.
    pub deleted: bool,
}

/// Control's placement facts for one joined worker instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerFacts {
    /// The zone recorded on the instance when it joined, and frozen there.
    pub zone: ZoneId,
    /// The instance is still live: `active` and within its lease. Revoking or
    /// purging a signer marks its instances gone in the same transaction, and
    /// a worker that stopped renewing falls out on its own.
    pub active: bool,
}

pub type EligibilityFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + 'a>>;

/// Trusted placement facts. Implementations read only authoritative platform
/// state, never anything a worker supplied.
///
/// An unavailable source returns `Unavailable`, a retryable infrastructure
/// failure. An app or instance Control has no row for is `None`, which refuses
/// placement.
pub trait EligibilitySource: Debug {
    fn app<'a>(&'a self, app: &'a AppId) -> EligibilityFuture<'a, Option<AppFacts>>;
    fn worker<'a>(&'a self, worker: &'a WorkerId) -> EligibilityFuture<'a, Option<WorkerFacts>>;
}

mod control {
    zeroship_data_orm::orm::schema! {
        pub schema {
            apps {
                #[orm(primary_key)]
                id: Text,
                execution_zone_id: Text,
                deleted_at: Nullable<Timestamp>,
            }
            worker_instances {
                #[orm(primary_key)]
                id: Text,
                status: Text,
                execution_zone_id: Text,
            }
        }
    }
}
use control::schema::{apps, worker_instances as instances};

/// Native metadata for the Control binding [`ControlEligibility`] reads. The
/// projections name only the columns the manager's role is granted.
///
/// # Errors
/// Refuses invalid native model declarations.
pub fn collections() -> Result<Schema, Error> {
    let schema = control::schema::schema();
    schema.validate()?;
    Ok(schema)
}

#[derive(FromRow)]
#[orm(entity = apps)]
struct AppRow {
    execution_zone_id: String,
    deleted_at: Option<i64>,
}

/// Production facts: Control's own rows over the platform binding, under the
/// manager role's column grants.
#[derive(Debug, Clone)]
pub struct ControlEligibility {
    database: Database,
}

impl ControlEligibility {
    /// Bind the host's provisioned Control schema.
    ///
    /// # Errors
    /// Rejects missing or incompatible native metadata.
    pub fn new(database: Database) -> Result<Self, Error> {
        database.entity::<apps::Entity>()?;
        database.entity::<instances::Entity>()?;
        Ok(Self { database })
    }

    /// Verify the provisioned columns and the manager's read grants.
    ///
    /// # Errors
    /// Refuses unavailable or incompatible source storage.
    pub async fn ready(&self) -> Result<(), Error> {
        async {
            self.database
                .entity::<apps::Entity>()?
                .query()
                .first::<AppRow>()
                .await?;
            self.database
                .entity::<instances::Entity>()?
                .query()
                .first::<InstanceRow>()
                .await?;
            Ok::<_, zeroship_data_orm::error::DbError>(())
        }
        .await
        .map_err(|_| Error::Unavailable)
    }
}

/// One instance's zone and liveness. Both are on the instance row, so this is
/// one read of one table: the zone is the join token's verified claim frozen
/// there, and a signer's revoke or purge marks its instances `gone` in the same
/// transaction, so a signer's own state needs no second read.
#[derive(FromRow)]
#[orm(entity = instances)]
struct InstanceRow {
    status: String,
    execution_zone_id: String,
}

impl EligibilitySource for ControlEligibility {
    fn app<'a>(&'a self, app: &'a AppId) -> EligibilityFuture<'a, Option<AppFacts>> {
        Box::pin(async move {
            let row = self
                .database
                .entity::<apps::Entity>()
                .map_err(|_| Error::Unavailable)?
                .query()
                .filter(apps::id.eq(app.as_str()).map_err(|_| Error::Unavailable)?)
                .first::<AppRow>()
                .await
                .map_err(|_| Error::Unavailable)?;
            row.map(|row| {
                Ok(AppFacts {
                    zone: ZoneId::parse(&row.execution_zone_id)?,
                    deleted: row.deleted_at.is_some(),
                })
            })
            .transpose()
        })
    }

    fn worker<'a>(&'a self, worker: &'a WorkerId) -> EligibilityFuture<'a, Option<WorkerFacts>> {
        Box::pin(async move {
            let row = self
                .database
                .entity::<instances::Entity>()
                .map_err(|_| Error::Unavailable)?
                .query()
                .filter(
                    instances::id
                        .eq(worker.as_str())
                        .map_err(|_| Error::Unavailable)?,
                )
                .first::<InstanceRow>()
                .await
                .map_err(|_| Error::Unavailable)?;
            row.map(|row| {
                Ok(WorkerFacts {
                    zone: ZoneId::parse(&row.execution_zone_id)?,
                    active: row.status == "active",
                })
            })
            .transpose()
        })
    }
}

/// Trusted facts for a host with exactly one zone.
///
/// Every app and every worker of this host is in `zone` and active. The local
/// host uses it for its in-process worker; it performs no enrollment and reads
/// no Control rows.
#[derive(Debug, Clone)]
pub struct LocalEligibility {
    zone: ZoneId,
}

impl LocalEligibility {
    #[must_use]
    pub const fn new(zone: ZoneId) -> Self {
        Self { zone }
    }
}

impl EligibilitySource for LocalEligibility {
    fn app<'a>(&'a self, _: &'a AppId) -> EligibilityFuture<'a, Option<AppFacts>> {
        Box::pin(async move {
            Ok(Some(AppFacts {
                zone: self.zone.clone(),
                deleted: false,
            }))
        })
    }

    fn worker<'a>(&'a self, _: &'a WorkerId) -> EligibilityFuture<'a, Option<WorkerFacts>> {
        Box::pin(async move {
            Ok(Some(WorkerFacts {
                zone: self.zone.clone(),
                active: true,
            }))
        })
    }
}
