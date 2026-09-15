//! Observe database traffic at the public backend and session boundaries.
use async_trait::async_trait;
use futures::future::LocalBoxFuture;
use std::{cell::RefCell, rc::Rc};
use zeroship_data_orm::sql::{catalog::LiveSchema, SchemaName};
use zeroship_data_orm::value::Value;
use zeroship_data_orm::{
    backend::{Backend, BackendHandle},
    connection::{BackendFactory, ConnectionFactory},
    driver::{CancellationHandle, DriverSession, Session},
    encryption::{KeyStore, ProjectKeySource},
    error::{BeginIntent, CleanupAck, DbError, OpenSessionError, SettleIntent, TerminalResult},
    executor::ScopedExecutor,
    protection::{Catalog, Protection},
    search::{Search, SpatialSearch, VectorSearch},
};

thread_local! {
    static QUERIES: RefCell<Vec<RecordedQuery>> = const { RefCell::new(Vec::new()) };
}

#[derive(Clone, Debug)]
pub(crate) struct RecordedQuery {
    pub(crate) sql: String,
    pub(crate) params: Vec<Value>,
}

fn record(sql: &str, params: &[Value]) {
    QUERIES.with_borrow_mut(|queries| {
        queries.push(RecordedQuery {
            sql: sql.to_owned(),
            params: params.to_vec(),
        })
    });
}
pub(crate) fn clear() {
    QUERIES.with_borrow_mut(Vec::clear);
}
pub(crate) fn bulk_statements() -> Vec<String> {
    QUERIES.with_borrow(|queries| {
        queries
            .iter()
            .filter(|query| query.sql.starts_with("UPDATE ") || query.sql.starts_with("DELETE "))
            .map(|query| query.sql.clone())
            .collect()
    })
}
/// Identifier-only reads expose target resolution and upsert conflict probes.
pub(crate) fn id_probes() -> Vec<RecordedQuery> {
    QUERIES.with_borrow(|queries| {
        queries
            .iter()
            .filter(|query| {
                query
                    .sql
                    .starts_with("SELECT \"target\".\"id\" AS \"id\" FROM ")
            })
            .cloned()
            .collect()
    })
}

pub(crate) fn connection(url: &str) -> ConnectionFactory {
    let inner = ConnectionFactory::for_url(url).expect("valid fixture database configuration");
    ConnectionFactory::new(url, RecordingFactory(inner))
}
struct RecordingFactory(ConnectionFactory);
impl BackendFactory for RecordingFactory {
    fn sql_registration(&self) -> zeroship_data_orm::sql::registration::SqlRegistration {
        self.0.sql_registration().clone()
    }
    fn connect(
        &self,
        keys: ProjectKeySource,
    ) -> LocalBoxFuture<'_, Result<BackendHandle, DbError>> {
        Box::pin(async move {
            Ok(BackendHandle::new(Rc::new(RecordingBackend(
                self.0.connect(keys).await?,
            ))))
        })
    }
}
#[derive(Debug)]
struct RecordingBackend(BackendHandle);
impl Backend for RecordingBackend {
    fn sql_registration(&self) -> zeroship_data_orm::sql::registration::SqlRegistration {
        self.0.sql_registration().clone()
    }

    fn publishes_committed_changes(&self) -> bool {
        self.0.publishes_committed_changes()
    }

    fn admits_concurrent_transactions(&self) -> bool {
        self.0.admits_concurrent_transactions()
    }
}
#[async_trait(?Send)]
impl ScopedExecutor for RecordingBackend {
    fn namespace<'a>(&self, app_id: &'a str, schema: &'a SchemaName) -> &'a str {
        self.0.namespace(app_id, schema)
    }
    fn pool_counts(&self) -> Option<(usize, usize, usize)> {
        self.0.pool_counts()
    }
    async fn prepare_for_app(&self, app_id: &str, schema: &SchemaName) -> Result<(), DbError> {
        self.0.prepare_for_app(app_id, schema).await
    }
    async fn query(
        &self,
        app_id: &str,
        schema: &SchemaName,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<Value>, DbError> {
        record(sql, params);
        self.0.query(app_id, schema, sql, params).await
    }
    async fn exec(
        &self,
        app_id: &str,
        schema: &SchemaName,
        sql: &str,
        params: &[Value],
    ) -> Result<u64, DbError> {
        record(sql, params);
        self.0.exec(app_id, schema, sql, params).await
    }
    async fn check_connection(&self) -> Result<(), DbError> {
        self.0.check_connection().await
    }
    async fn open_tx_session(
        &self,
        app_id: &str,
        schema: &SchemaName,
        begin: BeginIntent,
    ) -> Result<Session, OpenSessionError> {
        Ok(Session::new(RecordingSession(
            self.0.open_tx_session(app_id, schema, begin).await?,
        )))
    }
}
#[async_trait(?Send)]
impl Catalog for RecordingBackend {
    async fn introspect_schema(
        &self,
        app_id: &str,
        schema: &SchemaName,
        session: Option<&Session>,
    ) -> Result<LiveSchema, DbError> {
        self.0
            .introspect_schema(app_id, schema, unwrap_session(session))
            .await
    }
}
impl Protection for RecordingBackend {
    fn key_store(&self) -> &KeyStore {
        self.0.key_store()
    }
}
fn unwrap_session(session: Option<&Session>) -> Option<&Session> {
    session.map(|session| {
        &session
            .get::<RecordingSession>()
            .expect("recording session")
            .0
    })
}
#[async_trait(?Send)]
impl Search for RecordingBackend {
    async fn vector_search(
        &self,
        session: Option<&Session>,
        request: VectorSearch<'_>,
    ) -> Result<Vec<Value>, DbError> {
        self.0.vector_search(unwrap_session(session), request).await
    }
    async fn spatial_near(
        &self,
        session: Option<&Session>,
        request: SpatialSearch<'_>,
    ) -> Result<Vec<Value>, DbError> {
        self.0.spatial_near(unwrap_session(session), request).await
    }
}
#[derive(Debug)]
struct RecordingSession(Session);
#[async_trait(?Send)]
impl DriverSession for RecordingSession {
    fn server_process_id(&self) -> Option<i32> {
        self.0.server_process_id()
    }
    async fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Value>, DbError> {
        record(sql, params);
        self.0.query(sql, params).await
    }
    async fn exec(&self, sql: &str, params: &[Value]) -> Result<u64, DbError> {
        record(sql, params);
        self.0.exec(sql, params).await
    }
    async fn settle(&self, intent: SettleIntent) -> (TerminalResult, Option<DbError>) {
        self.0.settle(intent).await
    }
    async fn cleanup(&self) -> CleanupAck {
        self.0.cleanup().await
    }
    fn canceller(&self) -> Option<CancellationHandle> {
        self.0.canceller()
    }
    fn discard(self: Box<Self>) {
        self.0.discard();
    }
}
