//! Customer-bound journal execution through the shared Rust ORM.

use crate::{
    service::{models, schema},
    WorkflowServiceError,
};
use futures::channel::oneshot;
use futures::future::{AbortHandle, Abortable};
use std::time::Duration;
use zeroship_core::app_id::AppId;
pub use zeroship_core::schema_name::SchemaName;
pub(crate) use zeroship_data_orm::Value;
use zeroship_data_orm::{
    backend::BackendHandle,
    binding::DbBinding,
    connection::ConnectionFactory,
    encryption::ProjectKeySource,
    error::DbError,
    orm::{Database, Entity, FindOptions, Operation},
    sql::registration::{POSTGRES_FAMILY, SQLITE_FAMILY},
    value, OrmContext,
};

/// Resolved app services that a host may pass to its workflow thread.
#[derive(Clone, Debug)]
pub struct HostStorage {
    pub connection: ConnectionFactory,
    pub keys: ProjectKeySource,
    pub binding: DbBinding,
    pub objects: zeroship_storage::StorageStore,
}
impl HostStorage {
    pub async fn open(&self) -> Result<OrmStore, WorkflowServiceError> {
        OrmStore::connect(self.binding.clone(), &self.connection, self.keys.clone()).await
    }
}

#[derive(Debug)]
pub(crate) struct Row(pub(crate) Value);
impl Row {
    pub(crate) fn text(&self, key: &str) -> Result<String, WorkflowServiceError> {
        self.0
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| invalid_row(key))
    }
    pub(crate) fn integer(&self, key: &str) -> Result<i64, WorkflowServiceError> {
        self.0
            .get(key)
            .and_then(Value::as_i64)
            .ok_or_else(|| invalid_row(key))
    }
    pub(crate) fn optional_text(&self, key: &str) -> Result<Option<String>, WorkflowServiceError> {
        match self.0.get(key) {
            Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_str()
                .map(|value| Some(value.to_owned()))
                .ok_or_else(|| invalid_row(key)),
            None => Err(invalid_row(key)),
        }
    }
    pub(crate) fn optional_integer(&self, key: &str) -> Result<Option<i64>, WorkflowServiceError> {
        match self.0.get(key) {
            Some(Value::Null) => Ok(None),
            Some(value) => value.as_i64().map(Some).ok_or_else(|| invalid_row(key)),
            None => Err(invalid_row(key)),
        }
    }
}
fn invalid_row(key: &str) -> WorkflowServiceError {
    WorkflowServiceError::Internal(format!("invalid workflow database field {key}"))
}

/// Customer journal and database clock on the host's compio thread.
#[derive(Clone, Debug)]
pub struct OrmStore {
    database: Database,
    pub(crate) binding: DbBinding,
    pub(crate) backend: BackendHandle,
    pub(crate) clock: BackendHandle,
}
impl OrmStore {
    /// Bind host-owned ORM resources. The clock must use a separate pool so
    /// reading time cannot wait for the journal's own transaction lease.
    pub fn new(
        context: OrmContext,
        binding: DbBinding,
        backend: BackendHandle,
        clock: BackendHandle,
    ) -> Result<Self, WorkflowServiceError> {
        let family = backend.sql_registration().family();
        if !matches!(family, POSTGRES_FAMILY | SQLITE_FAMILY)
            || family != clock.sql_registration().family()
            || backend.connection_identity() != clock.connection_identity()
            || std::ptr::eq(
                std::ops::Deref::deref(&backend),
                std::ops::Deref::deref(&clock),
            )
        {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow journal and clock require the same database configuration".into(),
            ));
        }
        context.with(|| models::install(&binding))?;
        Ok(Self {
            database: Database::new(context, binding.clone(), backend.clone()),
            binding,
            backend,
            clock,
        })
    }

    /// Open the host's normal ORM configuration. Provisioning remains a host operation.
    pub async fn connect(
        binding: DbBinding,
        connection: &ConnectionFactory,
        keys: ProjectKeySource,
    ) -> Result<Self, WorkflowServiceError> {
        let backend = connection.connect(keys.clone()).await?;
        let clock = connection.connect(keys).await?;
        Self::new(OrmContext::new(), binding, backend, clock)
    }

    pub async fn begin(&self) -> Result<Transaction, WorkflowServiceError> {
        // Resolve lazy SQLite attachments before reserving the journal writer.
        compio::time::timeout(
            Duration::from_secs(5),
            self.clock
                .prepare_for_app(self.binding.app_id(), self.binding.schema()),
        )
        .await
        .map_err(|_| {
            WorkflowServiceError::Unavailable("workflow database clock unavailable".into())
        })??;
        let (opened, receive_database) = oneshot::channel();
        let (settle, receive_intent) = oneshot::channel();
        let (finished, receive_result) = oneshot::channel();
        let (abort, registration) = AbortHandle::new_pair();
        let abort = AbortOnDrop(abort);
        let database = self.database.clone();
        compio::runtime::spawn(async move {
            let result = Abortable::new(
                database.transaction(move |transaction| async move {
                    opened.send(transaction).map_err(|_| abandoned())?;
                    receive_intent.await.map_err(|_| abandoned())
                }),
                registration,
            )
            .await
            .unwrap_or_else(|_| Err(abandoned()));
            let _ = finished.send(result);
        })
        .detach();
        let database = match receive_database.await {
            Ok(database) => database,
            Err(_) => {
                return match receive_result.await {
                    Ok(Err(error)) => Err(error.into()),
                    _ => Err(database_error(abandoned())),
                };
            }
        };
        let tx = Transaction {
            database,
            abort,
            settle: Some(settle),
            result: receive_result,
            binding: self.binding.clone(),
            clock: self.clock.clone(),
            policies: None,
            policy_binding: None,
            observed_policy: None,
            mutation_authority: None,
        };
        if tx.dialect() == "sqlite" {
            // A no-match write obtains SQLite's writer reservation before any
            // journal read, avoiding stale-snapshot promotion across hosts.
            tx.database
                .collection(models::app_state::Entity::COLLECTION)?
                .execute(Operation::Update {
                    filter: value!({"id":"workflow-writer-reservation"}),
                    patch: value!({"$inc":{"signal_epoch":0}}),
                    many: true,
                })
                .await?;
        }
        Ok(tx)
    }

    pub async fn verify(&self) -> Result<(), WorkflowServiceError> {
        let tx = self.begin().await?;
        let rows = tx
            .database()
            .entity::<models::schema_version::Entity>()?
            .find::<models::Fingerprint>(
                models::schema_version::id.eq("workflow")?,
                FindOptions::default(),
            )
            .await
            .map_err(|_| schema::incompatible())?;
        if rows.len() != 1 || rows[0].fingerprint != schema::fingerprint(tx.dialect())? {
            return Err(schema::incompatible());
        }
        tx.commit().await
    }
}

fn abandoned() -> DbError {
    DbError::internal("workflow transaction abandoned")
}

struct AbortOnDrop(AbortHandle);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Keeps the ORM callback alive until the journal explicitly commits.
/// Dropping the handle cancels ORM admission or rolls back its open transaction.
pub struct Transaction {
    database: Database,
    abort: AbortOnDrop,
    settle: Option<oneshot::Sender<()>>,
    result: oneshot::Receiver<Result<(), DbError>>,
    binding: DbBinding,
    clock: BackendHandle,
    pub(crate) policies: Option<std::sync::Arc<super::HostPolicies>>,
    pub(crate) policy_binding: Option<super::PolicyBinding>,
    pub(super) observed_policy: Option<super::policy::CapturedPolicy>,
    mutation_authority: Option<super::policy::PolicyAuthority>,
}
impl std::fmt::Debug for Transaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction")
            .field("dialect", &self.dialect())
            .finish_non_exhaustive()
    }
}
impl Transaction {
    pub(crate) fn database(&self) -> &Database {
        &self.database
    }
    pub(crate) fn host_app_ids(&self) -> Result<Vec<AppId>, WorkflowServiceError> {
        if let Some(binding) = &self.policy_binding {
            return Ok(vec![binding.app_id().clone()]);
        }
        self.policies
            .as_ref()
            .ok_or_else(|| {
                WorkflowServiceError::Internal("workflow host policy was not bound".into())
            })?
            .app_ids()
    }
    pub(crate) fn check_app(&self, app: &AppId) -> Result<(), WorkflowServiceError> {
        if self
            .policy_binding
            .as_ref()
            .is_some_and(|binding| binding.app_id() != app)
        {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        Ok(())
    }
    pub(crate) fn policy(&self, app: &AppId) -> Result<super::AppPolicy, WorkflowServiceError> {
        self.check_app(app)?;
        if let Some(binding) = &self.policy_binding {
            if let Some(authority) = &self.mutation_authority {
                return authority.effective();
            }
            return binding.resolve();
        }
        self.policies
            .as_ref()
            .ok_or_else(|| {
                WorkflowServiceError::Unavailable("workflow host policy not bound".into())
            })?
            .resolve(app)
    }
    pub(crate) fn capture_mutation(&mut self, app: &AppId) -> Result<(), WorkflowServiceError> {
        self.check_app(app)?;
        if self.policy_binding.is_some() {
            if self.mutation_authority.is_none() {
                self.mutation_authority = Some(
                    self.observed_policy
                        .as_ref()
                        .ok_or_else(|| {
                            WorkflowServiceError::Unavailable(
                                "workflow policy capture missing".into(),
                            )
                        })?
                        .authority()?
                        .clone(),
                );
            }
            self.mutation_authority
                .as_ref()
                .expect("captured binding authority")
                .check()?;
        }
        Ok(())
    }
    pub(crate) fn dialect(&self) -> &'static str {
        match self.clock.sql_registration().family() {
            POSTGRES_FAMILY => "postgres",
            SQLITE_FAMILY => "sqlite",
            _ => unreachable!("validated workflow database family"),
        }
    }
    pub(crate) async fn now(&mut self) -> Result<i64, WorkflowServiceError> {
        // This query reads only the customer's database clock, never journal
        // rows. A separate pool avoids borrowing a held transaction's lease.
        let sql = match self.clock.sql_registration().family() {
            POSTGRES_FAMILY => {
                "SELECT CAST(FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000) AS BIGINT) AS now"
            }
            SQLITE_FAMILY => {
                "SELECT CAST(strftime('%s','now') AS INTEGER) * 1000 \
                    + CAST(substr(strftime('%f','now'),4,3) AS INTEGER) AS now"
            }
            _ => return Err(schema::incompatible()),
        };
        let rows = compio::time::timeout(
            Duration::from_secs(5),
            self.clock
                .query(self.binding.app_id(), self.binding.schema(), sql, &[]),
        )
        .await
        .map_err(|_| {
            WorkflowServiceError::Unavailable("workflow database clock unavailable".into())
        })??;
        rows.into_iter()
            .next()
            .ok_or_else(|| invalid_row("now"))
            .and_then(|row| Row(row).integer("now"))
    }
    pub async fn commit(mut self) -> Result<(), WorkflowServiceError> {
        if let Some(authority) = self.mutation_authority.take() {
            authority.check()?;
            return authority.run(self.commit_unchecked()).await;
        }
        self.commit_unchecked().await
    }
    #[expect(
        clippy::future_not_send,
        reason = "commit settles its owning ORM transaction"
    )]
    async fn commit_unchecked(mut self) -> Result<(), WorkflowServiceError> {
        let _abort = self.abort;
        let intent = self.settle.take().expect("workflow settlement intent");
        let _ = intent.send(());
        self.result
            .await
            .map_err(|_| database_error(abandoned()))?
            .map_err(database_error)
    }
}

impl From<DbError> for WorkflowServiceError {
    fn from(error: DbError) -> Self {
        database_error(error)
    }
}

pub(crate) fn database_error(error: DbError) -> WorkflowServiceError {
    #[cfg(test)]
    eprintln!("workflow database error: {error:?}");
    match error {
        DbError::PermissionDenied { .. } => WorkflowServiceError::PermissionDenied,
        DbError::Transient { .. }
        | DbError::Serialization { .. }
        | DbError::LockContention { .. } => {
            WorkflowServiceError::Unavailable("workflow database temporarily unavailable".into())
        }
        _ => WorkflowServiceError::Internal("workflow database operation failed".into()),
    }
}
