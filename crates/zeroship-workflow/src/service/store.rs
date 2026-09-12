//! Customer-bound journal execution through the shared Rust ORM.

use crate::{service::schema, WorkflowServiceError};
pub use zeroship_core::schema_name::SchemaName;
pub(crate) use zeroship_data_orm::Value;
use zeroship_data_orm::{
    backend::BackendHandle, binding::DbBinding, error::DbError, exec, sql::compile::SqlDialect,
    transaction::AtomicWriteFrame, tx_route::CapturedRoute, ConnectOptions, OrmContext,
};

/// Resolved app services that a host may pass to its workflow thread.
#[derive(Clone, Debug)]
pub struct HostStorage {
    pub connection: zeroship_data_orm::connection::ConnectionFactory,
    pub keys: zeroship_data_orm::encryption::ProjectKeySource,
    pub binding: DbBinding,
    pub objects: zeroship_storage::StorageStore,
}
impl HostStorage {
    pub async fn open(&self) -> Result<OrmStore, WorkflowServiceError> {
        let backend = self
            .connection
            .connect(self.keys.clone())
            .await
            .map_err(database_error)?;
        OrmStore::new(OrmContext::new(), self.binding.clone(), backend)
    }
}

#[derive(Debug)]
pub(crate) struct Row(Value);
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

/// An already resolved customer database. Clones share the ORM's local owner.
/// Connections and transactions stay on the host's compio thread.
#[derive(Clone, Debug)]
pub struct OrmStore {
    pub(crate) context: OrmContext,
    pub(crate) binding: DbBinding,
    pub(crate) backend: BackendHandle,
    namespace: String,
}
impl OrmStore {
    /// Reuse host-owned ORM resources without opening another backend.
    pub fn new(
        context: OrmContext,
        binding: DbBinding,
        backend: BackendHandle,
    ) -> Result<Self, WorkflowServiceError> {
        let namespace = SchemaName::new(backend.namespace(binding.app_id(), binding.schema()))
            .map_err(|_| {
                WorkflowServiceError::InvalidRequest("invalid workflow database binding".into())
            })?;
        let namespace = zeroship_data_orm::sql::compile::quote_ident(namespace.as_str());
        Ok(Self {
            context,
            binding,
            backend,
            namespace,
        })
    }

    /// Open normal ORM configuration. Provisioning remains a host operation.
    pub async fn connect(
        binding: DbBinding,
        options: ConnectOptions,
    ) -> Result<Self, WorkflowServiceError> {
        let backend = options.connect().await.map_err(database_error)?;
        Self::new(OrmContext::new(), binding, backend)
    }

    pub async fn begin(&self) -> Result<Transaction, WorkflowServiceError> {
        let route = CapturedRoute::capture(
            None,
            self.binding.app_id(),
            self.binding.schema().clone(),
            self.backend.dialect(),
        )
        .bind(self.backend.clone());
        let frame = self
            .context
            .scope(AtomicWriteFrame::begin(route))
            .await
            .map_err(database_error)?;
        let mut tx = Transaction {
            frame: Some(frame),
            context: self.context.clone(),
            namespace: self.namespace.clone(),
            dialect: self.backend.dialect(),
            policies: None,
        };
        if tx.dialect == SqlDialect::Sqlite {
            // The ORM opens a deferred transaction. Acquire its writer lock
            // before reading journal state so concurrent hosts cannot promote
            // stale read snapshots into claims. PostgreSQL locks app rows.
            tx.execute(
                &format!(
                    "UPDATE {} SET fingerprint=fingerprint WHERE id='workflow'",
                    tx.table("schema_version")
                ),
                &[],
            )
            .await?;
        }
        Ok(tx)
    }

    pub async fn verify(&self) -> Result<(), WorkflowServiceError> {
        let mut tx = self.begin().await?;
        let query = format!(
            "SELECT fingerprint FROM {} WHERE id = 'workflow'",
            tx.table("schema_version")
        );
        let row = tx
            .query(&query, &[])
            .await
            .map_err(|_| schema::incompatible())?;
        if row.len() != 1 || row[0].text("fingerprint")? != schema::fingerprint(tx.dialect())? {
            return Err(schema::incompatible());
        }
        tx.commit().await
    }
}

/// A journal transaction owned and settled by the ORM transaction protocol.
pub struct Transaction {
    frame: Option<AtomicWriteFrame>,
    context: OrmContext,
    namespace: String,
    dialect: SqlDialect,
    pub(crate) policies: Option<std::sync::Arc<super::HostPolicies>>,
}
impl std::fmt::Debug for Transaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction")
            .field("dialect", &self.dialect)
            .finish_non_exhaustive()
    }
}
impl Transaction {
    /// Filter assigned apps before limiting background candidate scans.
    pub(crate) fn host_app_scope(&self) -> Result<(&'static str, Value), WorkflowServiceError> {
        let apps = self
            .policies
            .as_ref()
            .ok_or_else(|| {
                WorkflowServiceError::Internal("workflow host policy was not bound".into())
            })?
            .app_ids()?;
        let encoded = serde_json::to_string(&apps).map_err(|_| {
            WorkflowServiceError::Internal("workflow app scope could not be encoded".into())
        })?;
        let query = match self.dialect {
            SqlDialect::Postgres => "SELECT jsonb_array_elements_text($1::text::jsonb)",
            SqlDialect::Sqlite => "SELECT value FROM json_each($1)",
        };
        Ok((query, encoded.into()))
    }
    pub(crate) fn dialect(&self) -> &'static str {
        match self.dialect {
            SqlDialect::Postgres => "postgres",
            SqlDialect::Sqlite => "sqlite",
        }
    }
    pub(crate) fn table(&self, name: &str) -> String {
        debug_assert!(name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        format!("{}.\"__zeroship_workflow_{name}\"", self.namespace)
    }
    pub(crate) fn lock_clause(&self) -> &'static str {
        match self.dialect {
            SqlDialect::Postgres => " FOR UPDATE",
            SqlDialect::Sqlite => "",
        }
    }
    pub(crate) async fn now(&mut self) -> Result<i64, WorkflowServiceError> {
        let sql = match self.dialect {
            SqlDialect::Postgres => {
                "SELECT CAST(FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000) AS BIGINT) AS now"
            }
            SqlDialect::Sqlite => {
                "SELECT CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER) AS now"
            }
        };
        self.query(sql, &[])
            .await?
            .first()
            .ok_or_else(|| invalid_row("now"))?
            .integer("now")
    }
    pub(crate) async fn execute(
        &mut self,
        sql: &str,
        params: &[Value],
    ) -> Result<u64, WorkflowServiceError> {
        let sql = self.sql(sql);
        self.context
            .scope(exec::run_statement(
                self.frame.as_ref().expect("open transaction").route(),
                &sql,
                params,
            ))
            .await
            .map_err(database_error)
    }
    pub(crate) async fn query(
        &mut self,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<Row>, WorkflowServiceError> {
        let sql = self.sql(sql);
        self.context
            .scope(exec::run_sql(
                self.frame.as_ref().expect("open transaction").route(),
                &sql,
                params,
            ))
            .await
            .map(|rows| rows.into_iter().map(Row).collect())
            .map_err(database_error)
    }
    fn sql<'a>(&self, sql: &'a str) -> std::borrow::Cow<'a, str> {
        match self.dialect {
            SqlDialect::Postgres => sql.into(),
            SqlDialect::Sqlite => sqlite_placeholders(sql).into(),
        }
    }
    pub async fn commit(mut self) -> Result<(), WorkflowServiceError> {
        let frame = self.frame.take().expect("open transaction");
        self.context
            .scope(frame.finish(Ok(())))
            .await
            .map_err(database_error)
    }
}
impl Drop for Transaction {
    fn drop(&mut self) {
        self.context.with(|| drop(self.frame.take()));
    }
}

pub(crate) fn database_error(error: DbError) -> WorkflowServiceError {
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

// SQLite indexes ?n by position; $n is named and assigned in appearance order.
// Kept only for journal operations awaiting collection-API support.
fn sqlite_placeholders(sql: &str) -> String {
    let mut output = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut quote = None;
    while let Some(ch) = chars.next() {
        if let Some(delimiter) = quote {
            output.push(ch);
            if ch == delimiter {
                if chars.peek() == Some(&delimiter) {
                    output.push(chars.next().expect("peeked quote"));
                } else {
                    quote = None;
                }
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
            output.push(ch);
        } else if ch == '$' && chars.peek().is_some_and(char::is_ascii_digit) {
            output.push('?');
        } else {
            output.push(ch);
        }
    }
    output
}
