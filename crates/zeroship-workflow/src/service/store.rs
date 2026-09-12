//! Transaction adapters. Workflow decisions belong to the shared service.

use crate::{service::schema, WorkflowServiceError};
use async_trait::async_trait;
use bytes::BytesMut;
use compio_postgres::{
    types::{IsNull, ToSql, Type},
    Client, NoTls,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
pub use zeroship_data_sql::SchemaName;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    Null,
    Text(String),
    Integer(i64),
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::Text(value.into())
    }
}
impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}
impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}
impl From<Option<i64>> for Value {
    fn from(value: Option<i64>) -> Self {
        value.map_or(Self::Null, Self::Integer)
    }
}
impl From<Option<String>> for Value {
    fn from(value: Option<String>) -> Self {
        value.map_or(Self::Null, Self::Text)
    }
}

impl ToSql for Value {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Send + Sync>> {
        match self {
            Self::Null => Ok(IsNull::Yes),
            Self::Text(value) => value.to_sql_checked(ty, out),
            Self::Integer(value) => value.to_sql_checked(ty, out),
        }
    }
    fn accepts(_: &Type) -> bool {
        true
    }
    fn to_sql_checked(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Send + Sync>> {
        self.to_sql(ty, out)
    }
}

impl rusqlite::ToSql for Value {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(match self {
            Self::Null => rusqlite::types::ToSqlOutput::Owned(rusqlite::types::Value::Null),
            Self::Text(value) => value.as_str().into(),
            Self::Integer(value) => (*value).into(),
        })
    }
}

#[derive(Debug)]
pub(crate) struct Row(BTreeMap<String, Value>);
impl Row {
    pub(crate) fn text(&self, key: &str) -> Result<String, WorkflowServiceError> {
        match self.0.get(key) {
            Some(Value::Text(value)) => Ok(value.clone()),
            _ => Err(invalid_row(key)),
        }
    }
    pub(crate) fn integer(&self, key: &str) -> Result<i64, WorkflowServiceError> {
        match self.0.get(key) {
            Some(Value::Integer(value)) => Ok(*value),
            _ => Err(invalid_row(key)),
        }
    }
    pub(crate) fn optional_text(&self, key: &str) -> Result<Option<String>, WorkflowServiceError> {
        match self.0.get(key) {
            Some(Value::Text(value)) => Ok(Some(value.clone())),
            Some(Value::Null) => Ok(None),
            _ => Err(invalid_row(key)),
        }
    }
    pub(crate) fn optional_integer(&self, key: &str) -> Result<Option<i64>, WorkflowServiceError> {
        match self.0.get(key) {
            Some(Value::Integer(value)) => Ok(Some(*value)),
            Some(Value::Null) => Ok(None),
            _ => Err(invalid_row(key)),
        }
    }
}
fn invalid_row(key: &str) -> WorkflowServiceError {
    WorkflowServiceError::Internal(format!("invalid workflow database field {key}"))
}

/// Persistence opens transactions; it does not provision its own schema.
#[async_trait(?Send)]
pub trait WorkflowStore: Send + Sync {
    async fn begin(&self) -> Result<Transaction, WorkflowServiceError>;
    async fn verify(&self) -> Result<(), WorkflowServiceError> {
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

#[derive(Clone)]
pub struct PostgresStore {
    url: String,
    schema: SchemaName,
}
impl std::fmt::Debug for PostgresStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresStore").finish_non_exhaustive()
    }
}
impl PostgresStore {
    #[must_use]
    pub const fn new(url: String, schema: SchemaName) -> Self {
        Self { url, schema }
    }
}
#[async_trait(?Send)]
impl WorkflowStore for PostgresStore {
    async fn begin(&self) -> Result<Transaction, WorkflowServiceError> {
        let (client, connection) = compio_postgres::connect(&self.url, NoTls)
            .await
            .map_err(postgres_error)?;
        compio::runtime::spawn(async move {
            if let Err(error) = connection.run().await {
                tracing::debug!(error = %error, "workflow connection ended");
            }
        })
        .detach();
        client
            .batch_execute("BEGIN")
            .await
            .map_err(postgres_error)?;
        Ok(Transaction {
            backend: Backend::Postgres(client),
            namespace: self.schema.quoted(),
            finished: false,
            policies: None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct SqliteStore {
    path: PathBuf,
}
impl SqliteStore {
    #[must_use]
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_owned(),
        }
    }
}
#[async_trait(?Send)]
impl WorkflowStore for SqliteStore {
    async fn begin(&self) -> Result<Transaction, WorkflowServiceError> {
        let conn = rusqlite::Connection::open_with_flags(
            &self.path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )
        .map_err(sqlite_error)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(sqlite_error)?;
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(sqlite_error)?;
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(sqlite_error)?;
        Ok(Transaction {
            backend: Backend::Sqlite(conn),
            namespace: "\"main\"".into(),
            finished: false,
            policies: None,
        })
    }
}

#[derive(Debug)]
enum Backend {
    Postgres(Client),
    Sqlite(rusqlite::Connection),
}
#[derive(Debug)]
pub struct Transaction {
    backend: Backend,
    namespace: String,
    finished: bool,
    pub(crate) policies: Option<std::sync::Arc<super::HostPolicies>>,
}
impl Transaction {
    pub(crate) fn dialect(&self) -> &'static str {
        match self.backend {
            Backend::Postgres(_) => "postgres",
            Backend::Sqlite(_) => "sqlite",
        }
    }
    pub(crate) fn table(&self, name: &str) -> String {
        debug_assert!(name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        format!("{}.\"__zeroship_workflow_{name}\"", self.namespace)
    }
    pub(crate) fn lock_clause(&self) -> &'static str {
        match self.backend {
            Backend::Postgres(_) => " FOR UPDATE",
            Backend::Sqlite(_) => "",
        }
    }
    pub(crate) async fn now(&mut self) -> Result<i64, WorkflowServiceError> {
        let sql = match self.backend {
            Backend::Postgres(_) => {
                "SELECT CAST(FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000) AS BIGINT) AS now"
            }
            Backend::Sqlite(_) => {
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
        match &mut self.backend {
            Backend::Postgres(client) => {
                let refs: Vec<&(dyn ToSql + Sync)> =
                    params.iter().map(|value| value as _).collect();
                client.execute(sql, &refs).await.map_err(postgres_error)
            }
            Backend::Sqlite(conn) => conn
                .execute(
                    &sqlite_placeholders(sql),
                    rusqlite::params_from_iter(params),
                )
                .map(|n| n as u64)
                .map_err(sqlite_error),
        }
    }
    pub(crate) async fn query(
        &mut self,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<Row>, WorkflowServiceError> {
        match &mut self.backend {
            Backend::Postgres(client) => {
                let refs: Vec<&(dyn ToSql + Sync)> =
                    params.iter().map(|value| value as _).collect();
                let rows = client.query(sql, &refs).await.map_err(postgres_error)?;
                rows.into_iter()
                    .map(|row| {
                        let mut values = BTreeMap::new();
                        for (index, column) in row.columns().iter().enumerate() {
                            let value = match *column.type_() {
                                Type::INT8 => Value::from(
                                    row.try_get::<_, Option<i64>>(index)
                                        .map_err(postgres_error)?,
                                ),
                                Type::TEXT | Type::VARCHAR => Value::from(
                                    row.try_get::<_, Option<String>>(index)
                                        .map_err(postgres_error)?,
                                ),
                                _ => return Err(invalid_row(column.name())),
                            };
                            values.insert(column.name().into(), value);
                        }
                        Ok(Row(values))
                    })
                    .collect()
            }
            Backend::Sqlite(conn) => {
                let mut stmt = conn
                    .prepare(&sqlite_placeholders(sql))
                    .map_err(sqlite_error)?;
                let columns: Vec<String> = stmt
                    .column_names()
                    .iter()
                    .map(|name| (*name).into())
                    .collect();
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(params), |row| {
                        let mut values = BTreeMap::new();
                        for (index, name) in columns.iter().enumerate() {
                            use rusqlite::types::ValueRef;
                            let value = match row.get_ref(index)? {
                                ValueRef::Null => Value::Null,
                                ValueRef::Integer(value) => Value::Integer(value),
                                ValueRef::Text(value) => Value::Text(
                                    std::str::from_utf8(value)
                                        .map_err(|_| rusqlite::Error::InvalidQuery)?
                                        .into(),
                                ),
                                _ => return Err(rusqlite::Error::InvalidQuery),
                            };
                            values.insert(name.clone(), value);
                        }
                        Ok(Row(values))
                    })
                    .map_err(sqlite_error)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(sqlite_error)
            }
        }
    }
    pub async fn commit(mut self) -> Result<(), WorkflowServiceError> {
        match &mut self.backend {
            Backend::Postgres(client) => client
                .batch_execute("COMMIT")
                .await
                .map_err(postgres_error)?,
            Backend::Sqlite(conn) => conn.execute_batch("COMMIT").map_err(sqlite_error)?,
        }
        self.finished = true;
        Ok(())
    }
}
// Closing a transaction-owned connection rolls back uncommitted work. No
// connection is returned to a pool with an unresolved transaction.
impl Drop for Transaction {
    fn drop(&mut self) {
        if !self.finished {
            if let Backend::Sqlite(conn) = &self.backend {
                let _ = conn.execute_batch("ROLLBACK");
            }
        }
    }
}

pub(crate) fn sqlite_error(error: rusqlite::Error) -> WorkflowServiceError {
    WorkflowServiceError::Internal(format!("workflow SQLite operation failed: {error}"))
}
fn postgres_error(error: compio_postgres::Error) -> WorkflowServiceError {
    WorkflowServiceError::Internal(format!("workflow PostgreSQL operation failed: {error}"))
}

// SQLite treats `$n` as a name and assigns slots in appearance order. Use its
// indexed `?n` spelling so shared statements preserve PostgreSQL parameter
// positions, including repeated references and UPDATE clauses before WHERE.
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
