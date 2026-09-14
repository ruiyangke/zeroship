//! Host startup checks and composition for the native workflow manager.

#![allow(
    clippy::future_not_send,
    reason = "compio pools and I/O belong to their owning runtime thread"
)]

use compio_postgres::{types::FromSql, Pool, PoolConfig, Row};
use std::{rc::Rc, time::Duration};
use zeroship_core::schema_name::SchemaName;
use zeroship_data_orm::{
    binding::DbBinding, encryption::ProjectKeySource, orm::Database, ConnectOptions,
};
use zeroship_workflow_manager::{
    coordinator::{Coordinator as NativeCoordinator, Options as NativeOptions},
    deployments::latest::{self, LatestDeploymentSource},
    eligibility::{self, ControlEligibility, EligibilitySource},
    retention::HoldClient,
    Options as QueueOptions, Queue,
};

pub const SCHEMA_SQL: &str = include_str!("../../zeroship-workflow-manager/schema/postgres.sql");
/// Manager tables the runtime role must read and write, and nothing more.
const MANAGER_TABLES: &[&str] = &[
    "workers",
    "queue_scopes",
    "deployment_holds",
    "jobs",
    "assignments",
    "placement_receipts",
    "management",
    "management_scopes",
    "schedule_deployments",
    "schedule_activations",
    "schedule_disables",
    "schedule_scopes",
    "schedules",
    "schedule_occurrences",
    "recovery_scopes",
    "recovery_duties",
    "capacity_demands",
    "capacity_targets",
    "capacity_intents",
];
const FINGERPRINT: &str = include_str!("../../zeroship-workflow-manager/schema/fingerprint.txt");

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
    pub(crate) queue: Queue,
    pub manager: NativeCoordinator,
    pub latest: LatestDeploymentSource,
}
impl Coordinator {
    /// Placement reads zone and enrollment facts through `eligibility`; the
    /// production host passes [`connect_eligibility`] over the same database.
    ///
    /// # Errors
    /// Rejects invalid options, connection failures and incompatible metadata schemas.
    pub async fn connect(
        url: &str,
        options: Options,
        holds: Rc<dyn HoldClient>,
        eligibility: Rc<dyn EligibilitySource>,
    ) -> Result<Self, Error> {
        options.validate()?;
        let mut config = PoolConfig::default();
        config
            .max_size(options.connections)
            .min_idle(1)
            .acquire_timeout(options.acquire_timeout)
            .command_timeout(options.command_timeout);
        let pool = Pool::connect_with_pool_config(url, config).await?;
        let binding = DbBinding::new(
            "workflow_manager",
            "workflow_manager",
            SchemaName::new("workflow_manager").map_err(|_| Error::Invalid)?,
        );
        let queue = compio::time::timeout(
            options.acquire_timeout,
            Queue::connect(
                binding,
                url,
                QueueOptions {
                    max_connections: std::num::NonZeroUsize::new(options.connections)
                        .ok_or(Error::Invalid)?,
                    transaction_timeout: options.command_timeout,
                    ..QueueOptions::default()
                },
                holds,
            ),
        )
        .await
        .map_err(|_| Error::Unavailable)??;
        let manager = NativeCoordinator::new(
            queue.clone(),
            NativeOptions {
                worker_ttl: options.worker_ttl,
                assignment_ttl: options.assignment_ttl,
                batch_limit: options.batch_limit,
                max_pending_management: options.max_pending_management,
            },
            eligibility,
        )?;
        let latest = compio::time::timeout(options.acquire_timeout, async {
            let database = Database::connect(
                DbBinding::new(
                    "platform",
                    "workflow-latest-deployment",
                    SchemaName::new("zeroship").map_err(|_| Error::Invalid)?,
                ),
                ConnectOptions::new(url, ProjectKeySource::unavailable())
                    .max_connections(
                        std::num::NonZeroUsize::new(options.connections).ok_or(Error::Invalid)?,
                    )
                    .connection_authority(),
                latest::collections().map_err(Error::from)?,
            )
            .await
            .map_err(|_| Error::Unavailable)?;
            LatestDeploymentSource::new(database).map_err(Error::from)
        })
        .await
        .map_err(|_| Error::Unavailable)??;
        let service = Self {
            pool,
            queue,
            manager,
            latest,
        };
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
                "SELECT has_schema_privilege(current_user,'workflow_manager','CREATE')
                 OR has_table_privilege(current_user,'workflow_manager.schema_version',
                    'INSERT,UPDATE,DELETE,TRUNCATE,TRIGGER,REFERENCES') AS privileged",
                &[],
            )
            .await?;
        if get::<bool>(&permissions[0], "privileged")? {
            return Err(Error::Unavailable);
        }
        for table in MANAGER_TABLES {
            let name = format!("workflow_manager.{table}");
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
        let rows = self
            .pool
            .query(
                "SELECT fingerprint FROM workflow_manager.schema_version WHERE id='manager'",
                &[],
            )
            .await?;
        let [row] = rows.as_slice() else {
            return Err(Error::Unavailable);
        };
        if get::<&str>(row, "fingerprint")? != FINGERPRINT.trim() {
            return Err(Error::Unavailable);
        }
        self.pool.batch_execute(
            "SELECT id,capacity,state,expires_at,lock_version,execution_zone_id FROM workflow_manager.workers LIMIT 0;
             SELECT id,lock_version,dispatch_cursor FROM workflow_manager.queue_scopes LIMIT 0;
             SELECT id,app_id,deployment_id,holder_id,deploy_hash,generation,state FROM workflow_manager.deployment_holds LIMIT 0;
             SELECT id,app_id,deployment_id,operation,operation_kind,run_id,management_request_id,spec_digest,available_at,dispatch_order,state,attempt,worker_id,assignment_revision,lease_deadline,outcome,settlement_digest,created_at FROM workflow_manager.jobs LIMIT 0;
             SELECT app_id,worker_id,revision,expires_at,released,refused FROM workflow_manager.assignments LIMIT 0;
             SELECT app_id,request_id,operation,worker_id,expected_revision,reason,result_revision,result_expires_at FROM workflow_manager.placement_receipts LIMIT 0;
             SELECT id,app_id,request_id,run_id,revision,actor,request,request_digest,blocks_execution,created_at,outcome FROM workflow_manager.management LIMIT 0;
             SELECT id,app_id,run_id,accepted_revision,settled_revision FROM workflow_manager.management_scopes LIMIT 0;
             SELECT id,app_id,definition,interpretation,created_at FROM workflow_manager.schedule_deployments LIMIT 0;
             SELECT id,app_id,deployment_id,revision,activated_at FROM workflow_manager.schedule_activations LIMIT 0;
             SELECT id,app_id,revision,created_at FROM workflow_manager.schedule_disables LIMIT 0;
             SELECT id,revision,activation_id,enabled FROM workflow_manager.schedule_scopes LIMIT 0;
             SELECT id,app_id,name,activation_id,revision,definition,next_at,anchor_at,catch_up_until,catch_up_remaining FROM workflow_manager.schedules LIMIT 0;
             SELECT id,app_id,schedule_id,revision,scheduled_at,run_id,job_id,activation_id FROM workflow_manager.schedule_occurrences LIMIT 0;
             SELECT id,deployment_id,activation_revision,ingress_epoch,state,closing_watermark,close_job_id,last_ingress_at FROM workflow_manager.recovery_scopes LIMIT 0;
             SELECT id,app_id,kind,next_due_at,pending_job_id FROM workflow_manager.recovery_duties LIMIT 0;
             SELECT id,execution_zone_id,recorded_at FROM workflow_manager.capacity_demands LIMIT 0;
             SELECT id,revision,desired,state,refusal,observed,attempt,attempt_deadline,retry_at,below_since,lock_version FROM workflow_manager.capacity_targets LIMIT 0;
             SELECT id,execution_zone_id,generation,state,refusal,attempt,attempt_deadline,retry_at FROM workflow_manager.capacity_intents LIMIT 0;
             SELECT id,deploy_hash FROM zeroship.apps LIMIT 0;
             SELECT id,app_id,deploy_hash,retention_state FROM zeroship.app_deploys LIMIT 0;"
        ).await?;
        Ok(())
    }
}

/// Bind Control's zone and enrollment rows for placement and verify the
/// manager role's column grants on them.
///
/// # Errors
/// Returns `Unavailable` for an unreachable database or missing grants.
pub async fn connect_eligibility(url: &str, options: Options) -> Result<ControlEligibility, Error> {
    options.validate()?;
    compio::time::timeout(options.acquire_timeout, async {
        let database = Database::connect(
            DbBinding::new(
                "platform",
                "workflow-eligibility",
                SchemaName::new("zeroship").map_err(|_| Error::Invalid)?,
            ),
            ConnectOptions::new(url, ProjectKeySource::unavailable())
                .max_connections(
                    std::num::NonZeroUsize::new(options.connections).ok_or(Error::Invalid)?,
                )
                .connection_authority(),
            eligibility::collections().map_err(Error::from)?,
        )
        .await
        .map_err(|_| Error::Unavailable)?;
        let source = ControlEligibility::new(database).map_err(Error::from)?;
        source.ready().await.map_err(Error::from)?;
        Ok(source)
    })
    .await
    .map_err(|_| Error::Unavailable)?
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

impl From<zeroship_workflow_manager::Error> for Error {
    fn from(error: zeroship_workflow_manager::Error) -> Self {
        use zeroship_workflow_manager::Error as NativeError;
        match error {
            NativeError::Invalid => Self::Invalid,
            NativeError::Denied => Self::Denied,
            NativeError::Conflict => Self::Conflict,
            NativeError::Capacity => Self::Capacity,
            NativeError::Timeout | NativeError::Unavailable | NativeError::Storage => {
                Self::Unavailable
            }
        }
    }
}
