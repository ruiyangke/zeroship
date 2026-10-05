//! Host startup checks and composition for the native workflow manager.

#![allow(
    clippy::future_not_send,
    reason = "compio pools and I/O belong to their owning runtime thread"
)]

use compio_postgres::{types::FromSql, Pool, PoolConfig, Row};
use std::{rc::Rc, time::Duration};
use zeroship_core::schema_name::SchemaName;
use zeroship_data_orm::binding::DbBinding;
use zeroship_workflow_manager::{
    coordinator::{Coordinator as NativeCoordinator, Options as NativeOptions},
    recovery::{Options as RecoveryOptions, Recovery},
    retention::HoldClient,
    Options as QueueOptions, Queue,
};

pub const SCHEMA_SQL: &str = include_str!("../../zeroship-workflow-manager/schema/postgres.sql");
/// Manager tables the runtime role must read and write, and nothing more.
const MANAGER_TABLES: &[&str] = &[
    "queue_scopes",
    "deployment_holds",
    "jobs",
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
    "capacity_targets",
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
            Self::Denied => "coordination request denied",
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
    /// The metadata database's wait budget, `workflow.database_acquire_timeout_ms`.
    /// It bounds each checkout from the coordinator's pool and, as
    /// [`Options::startup_timeout`], each step of opening the database.
    pub acquire_timeout: Duration,
    /// The budget for one queue transaction, `workflow.database_command_timeout_ms`.
    /// It bounds the manager's claim, heartbeat, settlement and scheduling
    /// work. The coordinator's own pool serves [`Coordinator::verify`] alone,
    /// and `connect` calls it as a startup step, so that pool's queries are
    /// bounded by [`Options::startup_timeout`] instead: a short queue
    /// transaction budget must not shorten metadata verification.
    pub command_timeout: Duration,
    pub batch_limit: usize,
    pub max_pending_management: usize,
    /// The server's work budget inside the wait a batch claim states,
    /// `workflow.claim_budget_ms`.
    pub claim_budget: Duration,
    /// How long a claimed delivery stays leased without a heartbeat,
    /// `workflow.delivery_lease_ms`. A worker that stops renewing loses the job
    /// to redelivery one lease after its last renewal.
    pub lease: Duration,
    /// How far heartbeats may extend one attempt, `workflow.max_attempt_ms`. At
    /// least [`Options::lease`].
    pub max_attempt: Duration,
}
impl Default for Options {
    fn default() -> Self {
        let queue = QueueOptions::default();
        Self {
            connections: 8,
            acquire_timeout: Duration::from_secs(5),
            command_timeout: Duration::from_secs(10),
            batch_limit: 128,
            max_pending_management: 1024,
            claim_budget: Duration::from_secs(5),
            lease: queue.lease,
            max_attempt: queue.max_attempt,
        }
    }
}
impl Options {
    /// The budget for each step of opening the metadata database: the service's
    /// authentication connection, constructing the coordinator's pool,
    /// verifying its metadata, binding its queue, opening the journal and
    /// opening the policy ledger.
    ///
    /// It is [`Options::acquire_timeout`]. One operator setting bounds startup
    /// and checkouts alike, so a database that accepts connections and never
    /// answers fails startup within the budget the operator chose, not within
    /// a default of the pool's or the queue transaction budget.
    #[must_use]
    pub const fn startup_timeout(&self) -> Duration {
        self.acquire_timeout
    }

    /// Everything [`Coordinator::connect`] would refuse before it opens a
    /// connection, so a configuration check refuses what startup would.
    ///
    /// The queue and the native coordinator judge their own options; this
    /// builds exactly the options `connect` hands them and asks each.
    ///
    /// # Errors
    /// Rejects empty limits, durations that cannot be represented in storage,
    /// an empty claim budget and an attempt cap shorter than the lease.
    pub fn validate(&self) -> Result<(), Error> {
        if self.acquire_timeout.is_zero() {
            return Err(Error::Invalid);
        }
        self.queue_options()?.validate()?;
        self.native_options().validate()?;
        Ok(())
    }

    fn queue_options(&self) -> Result<QueueOptions, Error> {
        Ok(QueueOptions {
            max_connections: std::num::NonZeroUsize::new(self.connections).ok_or(Error::Invalid)?,
            transaction_timeout: self.command_timeout,
            lease: self.lease,
            max_attempt: self.max_attempt,
            ..QueueOptions::default()
        })
    }

    const fn native_options(&self) -> NativeOptions {
        NativeOptions {
            batch_limit: self.batch_limit,
            max_pending_management: self.max_pending_management,
            claim_budget: self.claim_budget,
        }
    }
}

/// A thread-local, bounded compio pool. Construct on the HTTP thread that uses it.
#[derive(Debug, Clone)]
pub struct Coordinator {
    pool: Pool,
    pub(crate) queue: Queue,
    pub manager: NativeCoordinator,
}
impl Coordinator {
    /// Constructing the pool and binding the queue are each bounded by
    /// [`Options::startup_timeout`].
    ///
    /// # Errors
    /// Rejects invalid options, connection failures and incompatible metadata
    /// schemas. A startup step that outlasts its budget is `Unavailable`.
    pub async fn connect(
        url: &str,
        options: Options,
        holds: Rc<dyn HoldClient>,
    ) -> Result<Self, Error> {
        options.validate()?;
        let mut config = PoolConfig::default();
        config
            .max_size(options.connections)
            .min_idle(1)
            .acquire_timeout(options.acquire_timeout)
            .warm_up_timeout(options.startup_timeout())
            // This pool serves verification and readiness alone, both startup
            // steps, so a query is bounded by the startup budget rather than
            // by the queue transaction budget that [`Options::command_timeout`]
            // states.
            .command_timeout(options.startup_timeout());
        let pool = Pool::connect_with_pool_config(url, config).await?;
        let binding = DbBinding::platform(
            "workflow_manager",
            "workflow_manager",
            SchemaName::new("workflow_manager").map_err(|_| Error::Invalid)?,
        );
        let queue = compio::time::timeout(
            options.startup_timeout(),
            Queue::connect(binding, url, options.queue_options()?, holds),
        )
        .await
        .map_err(|_| Error::Unavailable)??;
        let manager = NativeCoordinator::new(queue.clone(), options.native_options())?;
        let service = Self {
            pool,
            queue,
            manager,
        };
        service.verify().await?;
        Ok(service)
    }

    /// Recovery responsibility over this coordinator's queue.
    ///
    /// The run service establishes ingress epochs through it, so acceptance
    /// and the maintenance lanes that close a scope act on the same rows.
    ///
    /// # Errors
    /// Rejects invalid recovery options.
    pub fn recovery(&self, options: RecoveryOptions) -> Result<Recovery, Error> {
        Ok(Recovery::new(self.queue.clone(), options)?)
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
        // Every column this process reads, and nothing beyond it. `/readyz`
        // answers from this call alone.
        self.pool.batch_execute(
            "SELECT id,execution_zone_id,lock_version,dispatch_cursor FROM workflow_manager.queue_scopes LIMIT 0;
             SELECT id,app_id,deployment_id,holder_id,deploy_hash,generation,state,held_at FROM workflow_manager.deployment_holds LIMIT 0;
             SELECT id,app_id,deployment_id,operation,operation_kind,run_id,management_request_id,spec_digest,available_at,dispatch_order,state,attempt,execution_attempts,executed_attempt,worker_id,lease_deadline,leased_at,deferred_until,deferrals,outcome,settlement_digest,created_at FROM workflow_manager.jobs LIMIT 0;
             SELECT id,app_id,request_id,run_id,revision,actor,request,request_digest,blocks_execution,created_at,outcome FROM workflow_manager.management LIMIT 0;
             SELECT id,app_id,run_id,accepted_revision,settled_revision FROM workflow_manager.management_scopes LIMIT 0;
             SELECT id,app_id,definition,interpretation,created_at FROM workflow_manager.schedule_deployments LIMIT 0;
             SELECT id,app_id,deployment_id,revision,activated_at FROM workflow_manager.schedule_activations LIMIT 0;
             SELECT id,app_id,revision,created_at FROM workflow_manager.schedule_disables LIMIT 0;
             SELECT id,revision,activation_id,enabled FROM workflow_manager.schedule_scopes LIMIT 0;
             SELECT id,app_id,name,activation_id,revision,definition,next_at,anchor_at,catch_up_until,catch_up_remaining FROM workflow_manager.schedules LIMIT 0;
             SELECT id,app_id,schedule_id,revision,scheduled_at,run_id,job_id,activation_id FROM workflow_manager.schedule_occurrences LIMIT 0;
             SELECT id,deployment_id,activation_revision,ingress_epoch,state,closing_watermark,close_job_id,active_at,close_after,close_attempts FROM workflow_manager.recovery_scopes LIMIT 0;
             SELECT id,app_id,kind,next_due_at,pending_job_id FROM workflow_manager.recovery_duties LIMIT 0;
             SELECT id,revision,desired,state,refusal,backlog_depth,oldest_available_at,exhausted_jobs,backed_off_jobs,withheld_jobs,attempt,attempt_deadline,retry_at,below_since,lock_version FROM workflow_manager.capacity_targets LIMIT 0;"
        ).await?;
        Ok(())
    }
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
            NativeError::Timeout
            | NativeError::Unavailable
            | NativeError::Contended
            | NativeError::Storage => {
                Self::Unavailable
            }
        }
    }
}
