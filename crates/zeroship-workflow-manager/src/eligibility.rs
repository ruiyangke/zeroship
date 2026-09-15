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
//! before commit. Zones never change, and status only moves forward: an
//! instance is revoked or retired and neither reverses. A change committed
//! before the first read is seen, one committed between the reads is caught by
//! the second, and one committed after the second is caught by the next
//! renewal, delivery or placement check. That is an eventual admission fence,
//! the same one the join path has.
//!
//! An instance's lease is read too, but as a liveness hint rather than part of
//! that fence: see `LEASE_SKEW`. A lease is the one fact here that can move
//! backwards and forwards, because a worker renews it.
#![expect(
    clippy::future_not_send,
    reason = "Control reads stay on the owning compio runtime"
)]

use crate::Error;
use std::{
    fmt::Debug,
    future::Future,
    pin::Pin,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zeroship_core::{app_id::AppId, typed_id, workflow_coordination::WorkerId};
use zeroship_data_orm::{
    orm::{Database, FromRow, UtcInstant},
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
    /// The instance is still live: `active`, and within its lease as this
    /// process's clock reads it. The lease half is a hint, not a fence - see
    /// [`LEASE_SKEW`]. Revoking or purging a signer marks its instances gone in
    /// the same transaction, and a worker that stopped renewing falls out on
    /// its own once its lease runs out.
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
                expires_at: Timestamp,
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
    deleted_at: Option<UtcInstant>,
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

/// One instance's zone and liveness. All of it is on the instance row, so this
/// is one read of one table: the zone is the join token's verified claim frozen
/// there, and a signer's revoke or purge marks its instances `gone` in the same
/// transaction, so a signer's own state needs no second read.
#[derive(FromRow)]
#[orm(entity = instances)]
struct InstanceRow {
    status: String,
    execution_zone_id: String,
    /// The instance's Control lease. See [`LEASE_SKEW`].
    expires_at: UtcInstant,
}

/// How far past an instance's lease this process still treats it as live.
///
/// THIS IS A LIVENESS HINT, NOT AN AUTHORIZATION FENCE. The fence is Control's:
/// it refuses an expired instance on every call it authenticates, against its
/// own database `now()`, and that is what actually stops a lapsed worker. What
/// the manager needs the lease for is different - it should not hand an app to
/// a worker whose lease has run out, because that worker can no longer fetch
/// the app's environment or project data key, so the placement would stall
/// instead of failing cleanly somewhere a caller can see it.
///
/// A hint tolerates a poor clock, which is why this compares against this
/// process's wall clock rather than the manager's database `Clock`. The worst
/// case is that two replicas disagree by their clock skew about when one worker
/// stops being a candidate, and Control refuses it either way. The allowance
/// runs in the permissive direction for the same reason: a clock running fast
/// should not shed a worker whose renewal is in flight. Reaching for the exact
/// answer would thread a database clock through this seam and buy no authority.
const LEASE_SKEW: Duration = Duration::from_secs(30);

/// This process's wall clock, in the microseconds a timestamp column decodes
/// into. The lease is a `timestamp` in Control's schema, so the comparison is
/// against [`UtcInstant::unix_micros`] and never a bare epoch integer.
///
/// Before the epoch is not a time any lease carries, so it reads as expired.
fn wall_clock_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_micros()).ok())
        .unwrap_or(i64::MAX)
}

/// Whether `expires_at` is still within [`LEASE_SKEW`] of `now_micros`.
///
/// Both sides are microseconds, which is what the column carries and what
/// [`UtcInstant`] hands back. A skew added in any other unit would move the
/// edge by three orders of magnitude while every value involved still looked
/// like a plausible epoch integer, so the unit is what the tests below bind.
fn within_lease(expires_at: UtcInstant, now_micros: i64) -> bool {
    i64::try_from(LEASE_SKEW.as_micros())
        .ok()
        .and_then(|skew| expires_at.unix_micros().checked_add(skew))
        .unwrap_or(i64::MAX)
        > now_micros
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
                    active: row.status == "active"
                        && within_lease(row.expires_at, wall_clock_micros()),
                })
            })
            .transpose()
        })
    }
}

/// Trusted facts for a host with exactly one zone.
///
/// Every app and every worker of this host is in `zone` and active. Fixtures
/// that register several workers of one host use it; a host whose capacity is
/// one process wants [`SoleWorker`] instead, because this source cannot tell a
/// live registration from a stale one.
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

/// Trusted facts for a host whose zone holds one live worker: this process.
///
/// A host that is its app's only capacity still outlives none of its own
/// registrations: the platform file survives the process, and a registration
/// survives it by the worker TTL, so a predecessor that died without draining
/// is still stored `ready`. Reporting it active would let a dead process hold
/// the app against the running one, which reads to the manager as an app that
/// already has a ready eligible owner, so it would place nothing and the host
/// could never take its own app back.
///
/// This is the fact Control supplies in production, where a dead instance stops
/// being active and the manager selects elsewhere. A host with no Control to
/// ask knows it directly: the live worker is the one this process minted.
#[derive(Debug, Clone)]
pub struct SoleWorker {
    zone: ZoneId,
    worker: WorkerId,
}

impl SoleWorker {
    #[must_use]
    pub const fn new(zone: ZoneId, worker: WorkerId) -> Self {
        Self { zone, worker }
    }
}

impl EligibilitySource for SoleWorker {
    fn app<'a>(&'a self, _: &'a AppId) -> EligibilityFuture<'a, Option<AppFacts>> {
        Box::pin(async move {
            Ok(Some(AppFacts {
                zone: self.zone.clone(),
                deleted: false,
            }))
        })
    }

    fn worker<'a>(&'a self, worker: &'a WorkerId) -> EligibilityFuture<'a, Option<WorkerFacts>> {
        Box::pin(async move {
            Ok(Some(WorkerFacts {
                zone: self.zone.clone(),
                active: *worker == self.worker,
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{LEASE_SKEW, within_lease};
    use std::time::Duration;
    use zeroship_data_orm::orm::UtcInstant;

    /// The allowance is [`LEASE_SKEW`] of wall clock, and the lease the column
    /// carries is microseconds. An allowance added in milliseconds would put
    /// the edge a thousandth of the way out and let a lease that lapsed
    /// seconds ago read as gone.
    ///
    /// This does not check that anything consults `within_lease`; the read
    /// path is covered where a real instance row is placed.
    #[test]
    fn the_allowance_is_the_declared_skew_of_microseconds() {
        let now = UtcInstant::from_unix_micros(1_789_279_200_000_000).unwrap();
        let at = |offset: Duration, sign: i64| {
            UtcInstant::from_unix_micros(
                now.unix_micros() + sign * i64::try_from(offset.as_micros()).unwrap(),
            )
            .unwrap()
        };
        // Inside the allowance, including a lease that has already lapsed.
        for lapsed in [Duration::from_secs(0), Duration::from_secs(29)] {
            assert!(
                within_lease(at(lapsed, -1), now.unix_micros()),
                "{lapsed:?} past its lease is within the allowance"
            );
        }
        assert!(within_lease(at(Duration::from_secs(3600), 1), now.unix_micros()));
        // Rejection control: one microsecond past the allowance, and well past.
        for lapsed in [
            LEASE_SKEW,
            LEASE_SKEW + Duration::from_micros(1),
            Duration::from_secs(300),
        ] {
            assert!(
                !within_lease(at(lapsed, -1), now.unix_micros()),
                "{lapsed:?} past its lease is outside the allowance"
            );
        }
        // A lease at the calendar edge saturates rather than wrapping into the
        // past, and the epoch itself is long expired against a modern clock.
        assert!(within_lease(
            UtcInstant::from_unix_micros(
                zeroship_data_orm::sql::temporal::MAX_TIMESTAMP_MICROS
            )
            .unwrap(),
            now.unix_micros()
        ));
        assert!(!within_lease(
            UtcInstant::from_unix_micros(0).unwrap(),
            now.unix_micros()
        ));
    }
}
