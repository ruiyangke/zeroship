//! Runtime registration and routed backend extensions for the shared ORM.
use crate::sql::{descriptors::GeoPoint, SchemaName};
use crate::value::Value;
use crate::{
    backend::Backend,
    binding::DbBinding,
    capability::{ScalarRead, UnmaskAuditRow},
    error::DbError,
    search::{SpatialSearch, VectorSearch},
    tx_route::TxRoute,
};
use std::{any::Any, ops::Deref, rc::Rc, sync::Arc};

pub use crate::crud::internal::AUDIT_UNMASK_TABLE;

/// A registered backend, erased once at the host boundary. Models never name it.
#[derive(Clone, Debug)]
pub struct BackendHandle(
    Rc<dyn Backend>,
    Rc<()>,
    Arc<crate::sql::registration::SqlRegistration>,
    Option<crate::connection::ConnectionIdentity>,
);
impl BackendHandle {
    pub fn new<B: Backend>(backend: Rc<B>) -> Self {
        let registration = backend.sql_registration();
        Self(backend, Rc::new(()), Arc::new(registration), None)
    }
    pub fn with_sql<B: Backend>(
        backend: Rc<B>,
        registration: crate::sql::registration::SqlRegistration,
    ) -> Result<Self, DbError> {
        if backend.sql_registration().family() != registration.family() {
            return Err(DbError::config(
                "backend_sql_mismatch",
                "backend execution and SQL registration use different SQL families",
            ));
        }
        Ok(Self(backend, Rc::new(()), Arc::new(registration), None))
    }
    pub fn sql_registration(&self) -> &crate::sql::registration::SqlRegistration {
        &self.2
    }
    pub fn connection_identity(&self) -> Option<crate::connection::ConnectionIdentity> {
        self.3
    }
    pub(crate) fn bind_connection_identity(
        mut self,
        identity: crate::connection::ConnectionIdentity,
    ) -> Self {
        self.3 = Some(identity);
        self
    }
    pub async fn open_tx_session(
        &self,
        app_id: &str,
        schema: &SchemaName,
        begin: crate::error::BeginIntent,
    ) -> Result<crate::driver::Session, crate::error::OpenSessionError> {
        Ok(self
            .0
            .open_tx_session(app_id, schema, begin)
            .await?
            .bind_driver(self.1.clone()))
    }
    pub(crate) fn validate_session(&self, session: &crate::driver::Session) -> Result<(), DbError> {
        session.validate_driver(&self.1)
    }
    /// Host-only access to backend-specific diagnostics and lifecycle services.
    pub fn get<T: 'static>(&self) -> Option<&T> {
        (self.0.as_ref() as &dyn Any).downcast_ref()
    }
    pub fn get_rc<T: 'static>(&self) -> Option<Rc<T>> {
        (self.0.clone() as Rc<dyn Any>).downcast().ok()
    }
    pub async fn append_unmask_audit(
        &self,
        schema: &SchemaName,
        attach_alias: &str,
        row: &UnmaskAuditRow<'_>,
    ) -> Result<(), DbError> {
        let namespace = SchemaName::new(self.namespace(attach_alias, schema))
            .map_err(|error| DbError::internal(format!("invalid backend namespace: {error}")))?;
        let params = vec![
            row.actor_id.into(),
            row.actor_role.into(),
            row.claimed_actor.into(),
            row.collection.into(),
            row.row_pk.into(),
            row.column.into(),
            row.classification.into(),
            row.reason.into(),
            row.outcome.into(),
        ];
        let query =
            crate::crud::internal::unmask_audit(&namespace, params, self.sql_registration())?;
        self.query(attach_alias, schema, &query.sql, &query.params)
            .await?;
        Ok(())
    }
}
impl Deref for BackendHandle {
    type Target = dyn Backend;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn routed_vector_search(
    route: &TxRoute,
    binding: &DbBinding,
    query: crate::sql::compiler::CompiledQuery,
) -> Result<Vec<Value>, DbError> {
    read_on_route(route, async {
        let lane = if route.in_tx() {
            Some(crate::exec::take_tx_lane(route)?)
        } else {
            None
        };
        if let Some(lane) = &lane {
            route.backend().validate_session(lane.client())?;
        }
        route
            .backend()
            .vector_search(
                lane.as_ref().map(|l| l.client()),
                VectorSearch { binding, query },
            )
            .await
    })
    .await
}
#[allow(clippy::too_many_arguments)]
pub async fn routed_spatial_near(
    route: &TxRoute,
    binding: &DbBinding,
    query: crate::sql::compiler::CompiledQuery,
    column: &str,
    point: GeoPoint,
    radius_m: f64,
    limit: usize,
) -> Result<Vec<Value>, DbError> {
    read_on_route(route, async {
        let lane = if route.in_tx() {
            Some(crate::exec::take_tx_lane(route)?)
        } else {
            None
        };
        if let Some(lane) = &lane {
            route.backend().validate_session(lane.client())?;
        }
        route
            .backend()
            .spatial_near(
                lane.as_ref().map(|l| l.client()),
                SpatialSearch {
                    binding,
                    query,
                    column,
                    point,
                    radius_m,
                    limit,
                },
            )
            .await
    })
    .await
}
async fn read_on_route<T>(
    route: &TxRoute,
    read: impl std::future::Future<Output = Result<T, DbError>>,
) -> Result<T, DbError> {
    if route.in_tx() {
        route.check_scope()?;
        crate::transaction::driver::execute_operation(route.app_id(), read).await
    } else {
        read.await
    }
}

pub(crate) async fn read_raw_column_value(
    route: &TxRoute,
    collection: &str,
    raw_column: &str,
    row_pk: &str,
    schema: &Value,
) -> Result<ScalarRead<Value>, DbError> {
    let namespace = SchemaName::new(route.backend().namespace(route.app_id(), route.schema()))
        .map_err(|error| DbError::internal(format!("invalid backend namespace: {error}")))?;
    let key_column = "id";
    let key_value = match schema[key_column]["type"].as_str() {
        Some("int" | "integer" | "bigInt" | "bigint") => {
            Value::from(row_pk.parse::<i64>().map_err(|_| {
                DbError::validation("invalid_row_identity", "row identity must be an integer")
            })?)
        }
        _ => Value::from(row_pk),
    };
    let query = crate::crud::internal::raw_column(
        &namespace,
        collection,
        raw_column,
        key_value,
        schema,
        route.sql_registration(),
    )?;
    let rows = crate::exec::run_sql(route, &query.sql, &query.params).await?;
    Ok(native_scalar(rows, "_raw"))
}
pub(crate) async fn read_raw_column_bytes(
    route: &TxRoute,
    collection: &str,
    raw_column: &str,
    row_pk: &str,
    schema: &Value,
) -> Result<ScalarRead<Vec<u8>>, DbError> {
    match read_raw_column_value(route, collection, raw_column, row_pk, schema).await? {
        ScalarRead::NoRow => Ok(ScalarRead::NoRow),
        ScalarRead::Null => Ok(ScalarRead::Null),
        ScalarRead::Value(Value::Bytes(bytes)) => Ok(ScalarRead::Value(bytes)),
        ScalarRead::Value(_) => Err(DbError::internal(
            "unmask: expected a binary encrypted column",
        )),
    }
}
fn native_scalar(rows: Vec<Value>, column: &str) -> ScalarRead<Value> {
    match rows.into_iter().next() {
        None => ScalarRead::NoRow,
        Some(mut row) => match row
            .as_object_mut()
            .and_then(|fields| fields.shift_remove(column))
        {
            None | Some(Value::Null) => ScalarRead::Null,
            Some(value) => ScalarRead::Value(value),
        },
    }
}

#[cfg(test)]
mod routed_read_tests {
    //! SQLite transaction routing for protected raw-column reads.

    use std::path::PathBuf;
    use std::rc::Rc;

    use super::*;
    use crate::tests::fixtures::DatabaseFixture;
    use crate::tx_route::CapturedRoute;

    /// A raw-column read inside a transaction must see that transaction's own
    /// write; the same read outside it must not.
    ///
    /// The two arms differ in ONE token - `in_tx` on the route - so a failure
    /// cannot be a missing table, a missing ATTACH or an unwritten row.
    #[test]
    fn a_routed_raw_read_follows_the_transaction_lane_on_sqlite() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");
        runtime.block_on(async {
            let app = "sqlite_routed_raw_read";
            let dir = tempfile::tempdir().expect("tempdir");
            let backend = Rc::new(
                crate::backend_selection::new_sqlite_backend(
                    PathBuf::from(dir.path()),
                    crate::encryption::ProjectKeySource::unavailable(),
                )
                .expect("open sqlite backend"),
            );
            backend
                .attach_app_file(app)
                .await
                .expect("attach the app file");
            backend
                .execute_fixture(
                    &format!(
                        r#"CREATE TABLE "{app}"."people" (
                               id TEXT PRIMARY KEY,
                               ssn TEXT,
                               "__zs_raw__ssn" TEXT
                           )"#
                    ),
                    &[],
                )
                .await
                .expect("CREATE TABLE people");

            let handle = BackendHandle::new(Rc::clone(&backend));
            let schema = crate::value!({
                "id": {"type":"string", "primaryKey":true},
                "ssn": {
                    "type":"string",
                    "mask":{"kind":"last4", "classification":"spi"},
                    "storage":{"valueColumn":"ssn", "rawColumn":"__zs_raw__ssn"}
                }
            });

            let admission = crate::transaction::TxAdmission::acquire(app.to_owned()).await;
            crate::transaction::exec_begin_or_savepoint(false, None, app,
                crate::sql::SchemaName::new(app).unwrap(), handle.clone()).await.unwrap();
            admission.handed_to_reducer();
            crate::transaction::driver::run_operation(app,
                &format!(r#"INSERT INTO "{app}"."people" (id, ssn, "__zs_raw__ssn") VALUES ('p1', '***', '123-45-6789')"#), &[])
                .await.expect("INSERT on the transaction connection");

            // CONTROL: a pool-lane read cannot see the uncommitted row.
            let outside = read_raw_column_value(
                &CapturedRoute::pool_for_tests(
                    app,
                    crate::sql::registration::SqlRegistration::sqlite(),
                )
                    .bind(handle.clone())
                    .unwrap(),
                "people",
                "__zs_raw__ssn",
                "p1",
                &schema,
            )
            .await
            .expect("the pooled read itself must succeed");
            assert!(
                matches!(outside, ScalarRead::NoRow),
                "the row must be invisible on the autocommit lane, or the arm \
                 below rules on nothing: {outside:?}",
            );

            // SUBJECT: the same read, routed onto the transaction.
            let inside = read_raw_column_value(
                &CapturedRoute::tx_for_tests(
                    app,
                    crate::sql::registration::SqlRegistration::sqlite(),
                )
                    .bind(handle.clone())
                    .unwrap(),
                "people",
                "__zs_raw__ssn",
                "p1",
                &schema,
            )
            .await
            .expect("a routed read inside the transaction must reach the row");
            assert!(
                matches!(&inside, ScalarRead::Value(v) if v == "123-45-6789"),
                "the transaction lane must return its own uncommitted value: {inside:?}",
            );

            // The guard must have handed the session back, or the next op in
            // this transaction would find an empty slot.
            assert!(matches!(crate::transaction::exec_settle(app, false, None).await, crate::transaction::SettleOutcome::Ok));

        });
    }
}
