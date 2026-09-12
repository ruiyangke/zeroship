//! Platform metadata persistence. No customer engine or payload-store dependency.
//!
//! The HTTP host authenticates service callers before selecting these methods.
//! Worker operations additionally fence every mutation against the assignment in
//! this database. These placements never grant a customer journal task lease.

#![allow(
    clippy::future_not_send,
    reason = "compio pools and I/O belong to their owning runtime thread"
)]

mod management;
mod placement;

use compio_postgres::{types::FromSql, Pool, PoolConfig, Row, Transaction};
use std::time::Duration;
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{
        RegisterWorker, RegisteredWorker, Revision, UnixMillis, WorkerId, WorkerState,
    },
};

pub const SCHEMA_SQL: &str = include_str!("../schema/postgres.sql");
const FINGERPRINT: &str = include_str!("../schema/fingerprint.txt");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Unauthenticated,
    RequestTooLarge,
    Denied,
    Conflict,
    Capacity,
    Unavailable,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "invalid coordination request",
            Self::Unauthenticated => "coordination authentication required",
            Self::RequestTooLarge => "coordination request exceeds metadata limit",
            Self::Denied => "coordination assignment denied",
            Self::Conflict => "coordination revision or request conflict",
            Self::Capacity => "coordination capacity exhausted",
            Self::Unavailable => "coordination database unavailable",
        })
    }
}
impl std::error::Error for Error {}
impl From<compio_postgres::Error> for Error {
    fn from(_: compio_postgres::Error) -> Self {
        // Database diagnostics may include connection material. The external
        // contract is a fixed error code, never the database's rendered message.
        Self::Unavailable
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub connections: usize,
    pub acquire_timeout: Duration,
    pub command_timeout: Duration,
    pub worker_ttl: Duration,
    pub assignment_ttl: Duration,
    pub batch_limit: usize,
    pub max_pending_management: usize,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            connections: 8,
            acquire_timeout: Duration::from_secs(5),
            command_timeout: Duration::from_secs(10),
            worker_ttl: Duration::from_secs(30),
            assignment_ttl: Duration::from_secs(30),
            batch_limit: 128,
            max_pending_management: 1024,
        }
    }
}
impl Options {
    /// # Errors
    /// Rejects empty limits or durations that cannot be represented in storage.
    pub fn validate(&self) -> Result<(), Error> {
        if self.connections == 0
            || self.acquire_timeout.is_zero()
            || self.command_timeout.is_zero()
            || self.batch_limit == 0
            || self.max_pending_management == 0
            || i64::try_from(self.batch_limit).is_err()
            || i64::try_from(self.max_pending_management).is_err()
        {
            return Err(Error::Invalid);
        }
        duration_ms(self.worker_ttl)?;
        duration_ms(self.assignment_ttl)?;
        Ok(())
    }
}

/// A thread-local, bounded compio pool. Construct on the HTTP thread that uses it.
#[derive(Debug, Clone)]
pub struct Coordinator {
    pool: Pool,
    options: Options,
}
impl Coordinator {
    /// # Errors
    /// Rejects invalid options, connection failures and incompatible metadata schemas.
    pub async fn connect(url: &str, options: Options) -> Result<Self, Error> {
        options.validate()?;
        let mut config = PoolConfig::default();
        config
            .max_size(options.connections)
            .min_idle(1)
            .acquire_timeout(options.acquire_timeout)
            .command_timeout(options.command_timeout);
        let pool = Pool::connect_with_pool_config(url, config).await?;
        let service = Self { pool, options };
        service.verify().await?;
        Ok(service)
    }

    /// # Errors
    /// Returns `Unavailable` for incompatible metadata or elevated runtime authority.
    pub async fn verify(&self) -> Result<(), Error> {
        let roles = self
            .pool
            .query(
                "SELECT rolsuper OR rolcreaterole OR rolcreatedb OR rolreplication OR rolbypassrls
               OR EXISTS(SELECT 1 FROM pg_roles other WHERE other.rolname<>current_user
                         AND pg_has_role(current_user,other.oid,'MEMBER')) AS privileged
             FROM pg_roles WHERE rolname=current_user",
                &[],
            )
            .await?;
        let [role] = roles.as_slice() else {
            return Err(Error::Unavailable);
        };
        if get::<bool>(role, "privileged")? {
            return Err(Error::Unavailable);
        }
        let permissions = self
            .pool
            .query(
                "SELECT has_schema_privilege(current_user,'workflow_coordination','CREATE')
                 OR has_table_privilege(current_user,'workflow_coordination.schema_version',
                    'INSERT,UPDATE,DELETE,TRUNCATE,TRIGGER,REFERENCES') AS privileged",
                &[],
            )
            .await?;
        if get::<bool>(&permissions[0], "privileged")? {
            return Err(Error::Unavailable);
        }
        for table in [
            "workers",
            "scopes",
            "assignments",
            "placement_receipts",
            "management",
        ] {
            let name = format!("workflow_coordination.{table}");
            let rows = self
                .pool
                .query(
                    "SELECT bool_and(has_table_privilege(current_user,$1,p)) AS writable,
                    has_table_privilege(current_user,$1,'TRUNCATE,TRIGGER,REFERENCES') AS privileged
                 FROM unnest(ARRAY['SELECT','INSERT','UPDATE','DELETE']) AS p",
                    &[&name],
                )
                .await?;
            if !get::<bool>(&rows[0], "writable")? || get::<bool>(&rows[0], "privileged")? {
                return Err(Error::Unavailable);
            }
        }
        let rows = self.pool.query(
            "SELECT fingerprint FROM workflow_coordination.schema_version WHERE id='coordination'",
            &[],
        ).await?;
        let [row] = rows.as_slice() else {
            return Err(Error::Unavailable);
        };
        if get::<&str>(row, "fingerprint")? != FINGERPRINT.trim() {
            return Err(Error::Unavailable);
        }
        self.pool.batch_execute(
            "SELECT worker_id,capacity,state,expires_at FROM workflow_coordination.workers LIMIT 0;
             SELECT app_id FROM workflow_coordination.scopes LIMIT 0;
             SELECT app_id,worker_id,revision,expires_at,released,wake_revision,next_due_at FROM workflow_coordination.assignments LIMIT 0;
             SELECT app_id,request_id,operation,worker_id,expected_revision,wake_revision,result_revision,result_expires_at FROM workflow_coordination.placement_receipts LIMIT 0;
             SELECT app_id,request_id,run_id,actor,operation,restart_name,restart_occurrence,restart_deploy,created_at,outcome,run_state,ack_worker_id,ack_revision FROM workflow_coordination.management LIMIT 0;"
        ).await?;
        Ok(())
    }

    // The whole transaction shares the pool's command deadline, including lock
    // waits and COMMIT. Cancellation recovery belongs to the driver's owned
    // lease; an uncertain session cannot be handed to the next request.
    async fn transact<T>(
        &self,
        operation: impl for<'a, 'b> AsyncFnOnce(&'a Transaction<'b>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mut lease = self.pool.acquire().await?;
        lease
            .command(async |client| {
                let tx = client.transaction().await?;
                let result = operation(&tx).await;
                if result.is_ok() {
                    tx.commit().await?;
                } else {
                    tx.rollback().await?;
                }
                Ok(result)
            })
            .await?
    }

    /// Soft liveness; renewing registration never revives an expired assignment.
    ///
    /// # Errors
    /// Returns `Unavailable` when the registration cannot be persisted.
    pub async fn register(
        &self,
        worker: &WorkerId,
        request: &RegisterWorker,
    ) -> Result<RegisteredWorker, Error> {
        self.transact(async |tx| {
            let ttl = duration_ms(self.options.worker_ttl)?;
            let capacity = i64::from(request.capacity.get());
            let state = match request.state {
                WorkerState::Ready => "ready",
                WorkerState::Draining => "draining",
            };
            let row = tx
                .query_one(
                    "INSERT INTO workflow_coordination.workers(worker_id,capacity,state,expires_at)
                 VALUES ($1,$2,$3,(floor(extract(epoch FROM clock_timestamp())*1000)::bigint)+$4)
                 ON CONFLICT (worker_id) DO UPDATE SET capacity=$2,state=$3,
                 expires_at=(floor(extract(epoch FROM clock_timestamp())*1000)::bigint)+$4
                 RETURNING worker_id,capacity,state,expires_at",
                    &[&worker.as_str(), &capacity, &state, &ttl],
                )
                .await?;
            registered(&row)
        })
        .await
    }

    /// # Errors
    /// Returns `Unavailable` when registry metadata cannot be read or decoded.
    pub async fn ready_workers(
        &self,
        after: Option<&WorkerId>,
    ) -> Result<Vec<RegisteredWorker>, Error> {
        let after = after.map(WorkerId::as_str);
        let limit = i64::try_from(self.options.batch_limit).map_err(|_| Error::Invalid)?;
        self.pool.query(
            "SELECT worker_id,capacity,state,expires_at FROM workflow_coordination.workers
             WHERE state='ready' AND expires_at>floor(extract(epoch FROM clock_timestamp())*1000)::bigint
             AND ($1::text IS NULL OR worker_id>$1) ORDER BY worker_id LIMIT $2", &[&after,&limit],
        ).await?.iter().map(registered).collect()
    }

    /// Missing owners always require a customer-journal rescan, even when the
    /// last worker died before publishing its next wake hint.
    ///
    /// # Errors
    /// Returns `Unavailable` when placement metadata cannot be read or decoded.
    pub async fn recovery_scopes(&self, after: Option<&AppId>) -> Result<Vec<AppId>, Error> {
        let after = after.map(AppId::as_str);
        let limit = i64::try_from(self.options.batch_limit).map_err(|_| Error::Invalid)?;
        self.pool.query(
            "SELECT s.app_id FROM workflow_coordination.scopes s
             WHERE ($1::text IS NULL OR s.app_id>$1) AND NOT EXISTS (
               SELECT 1 FROM workflow_coordination.assignments a JOIN workflow_coordination.workers w USING(worker_id)
               WHERE a.app_id=s.app_id AND NOT a.released
               AND a.expires_at>floor(extract(epoch FROM clock_timestamp())*1000)::bigint
               AND w.expires_at>floor(extract(epoch FROM clock_timestamp())*1000)::bigint)
             ORDER BY s.app_id LIMIT $2", &[&after, &limit],
        ).await?.iter().map(|row| AppId::parse(get(row,"app_id")?).map_err(|_| Error::Unavailable)).collect()
    }
}

fn duration_ms(value: Duration) -> Result<i64, Error> {
    i64::try_from(value.as_millis())
        .ok()
        .filter(|value| *value > 0)
        .ok_or(Error::Invalid)
}
fn get<'a, T: FromSql<'a>>(row: &'a Row, name: &str) -> Result<T, Error> {
    row.try_get(name).map_err(Into::into)
}
fn revision(value: i64) -> Result<Revision, Error> {
    value.try_into().map_err(|_| Error::Unavailable)
}
fn timestamp(value: i64) -> Result<UnixMillis, Error> {
    value.try_into().map_err(|_| Error::Unavailable)
}
async fn now(tx: &Transaction<'_>) -> Result<i64, Error> {
    let row = tx
        .query_one(
            "SELECT floor(extract(epoch FROM clock_timestamp())*1000)::bigint AS now",
            &[],
        )
        .await?;
    get(&row, "now")
}
fn deadline(now: i64, duration: Duration) -> Result<i64, Error> {
    now.checked_add(duration_ms(duration)?)
        .ok_or(Error::Unavailable)
}
async fn scope(tx: &Transaction<'_>, app: &AppId, create: bool) -> Result<(), Error> {
    if create {
        tx.execute(
            "INSERT INTO workflow_coordination.scopes(app_id) VALUES($1) ON CONFLICT DO NOTHING",
            &[&app.as_str()],
        )
        .await?;
    }
    let rows = tx
        .query(
            "SELECT app_id FROM workflow_coordination.scopes WHERE app_id=$1 FOR UPDATE",
            &[&app.as_str()],
        )
        .await?;
    if rows.is_empty() {
        return Err(Error::Denied);
    }
    Ok(())
}
fn registered(row: &Row) -> Result<RegisteredWorker, Error> {
    Ok(RegisteredWorker {
        worker_id: WorkerId::parse(get(row, "worker_id")?).map_err(|_| Error::Unavailable)?,
        capacity: u32::try_from(get::<i64>(row, "capacity")?)
            .ok()
            .and_then(std::num::NonZeroU32::new)
            .ok_or(Error::Unavailable)?,
        state: match get(row, "state")? {
            "ready" => WorkerState::Ready,
            "draining" => WorkerState::Draining,
            _ => return Err(Error::Unavailable),
        },
        expires_at: timestamp(get(row, "expires_at")?)?,
    })
}
