//! Runtime registration and routed backend extensions for the shared ORM.
use crate::{
    binding::DbBinding,
    capability::{ScalarRead, UnmaskAuditRow},
    driver::{Backend, SpatialSearch, VectorSearch},
    error::DbError,
    tx_route::TxRoute,
};
use std::{any::Any, ops::Deref, rc::Rc};
use zeroship_data_sql::{
    SchemaName,
    compile::SqlDialect,
    descriptors::{GeoPoint, VectorMetric},
    value::Value,
};

pub use zeroship_data_sql::internal::AUDIT_UNMASK_TABLE;

/// A registered backend, erased once at the host boundary. Models never name it.
#[derive(Clone, Debug)]
pub struct BackendHandle(Rc<dyn Backend>, Rc<()>);
impl BackendHandle {
    pub fn new<B: Backend>(backend: Rc<B>) -> Self {
        Self(backend, Rc::new(()))
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
        let namespace = match self.dialect() {
            SqlDialect::Sqlite => attach_alias,
            _ => schema.as_str(),
        };
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
        let query = zeroship_data_sql::internal::unmask_audit(namespace, self.dialect(), params);
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
    collection: &str,
    column: &str,
    query: &[f32],
    k: usize,
    metric: VectorMetric,
    filter: &Value,
    schema: &Value,
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
                VectorSearch {
                    binding,
                    collection,
                    column,
                    query,
                    k,
                    metric,
                    filter,
                    schema,
                },
            )
            .await
    })
    .await
}
#[allow(clippy::too_many_arguments)]
pub async fn routed_spatial_near(
    route: &TxRoute,
    binding: &DbBinding,
    collection: &str,
    column: &str,
    point: GeoPoint,
    radius_m: f64,
    filter: &Value,
    limit: Option<usize>,
    schema: &Value,
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
                    collection,
                    column,
                    point,
                    radius_m,
                    filter,
                    limit,
                    schema,
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
        crate::transaction::driver::execute_operation(route.app_id(), read).await
    } else {
        read.await
    }
}

pub async fn read_raw_column_value(
    route: &TxRoute,
    collection: &str,
    raw_column: &str,
    row_pk: &str,
) -> Result<ScalarRead<Value>, DbError> {
    let namespace = match route.dialect() {
        SqlDialect::Sqlite => route.app_id(),
        _ => route.schema().as_str(),
    };
    let query = zeroship_data_sql::internal::raw_column(
        namespace,
        collection,
        raw_column,
        row_pk,
        route.dialect(),
    );
    let rows = crate::exec::run_sql(route, &query.sql, &query.params).await?;
    Ok(native_scalar(rows, raw_column))
}
pub async fn read_raw_column_bytes(
    route: &TxRoute,
    collection: &str,
    raw_column: &str,
    row_pk: &str,
) -> Result<ScalarRead<Vec<u8>>, DbError> {
    match read_raw_column_value(route, collection, raw_column, row_pk).await? {
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
    //! The SQLite half of the routed raw-column read.
    //!
    //! The PostgreSQL half is bound live by
    //! `zeroship-plugin-db/tests/unmask_tx_lane.rs`, which needs a server.
    //! SQLite needs none, and it is the tier `pnpm dev` runs on - so the arm
    //! that would otherwise ship unbound is this one. It is a REAL divergence
    //! there and not a formality: SC-2 Decision 1 gave the session actor a
    //! shared `op_conn` plus a transaction connection per app, so an unmask
    //! sent to `op_conn` inside a transaction cannot see that transaction's
    //! writes, exactly as on PostgreSQL.

    use std::path::PathBuf;
    use std::rc::Rc;

    use super::*;
    use crate::storage::SqlExecutor;
    use crate::tx_route::CapturedRoute;

    /// A raw-sibling read inside a transaction must see that transaction's own
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
                    crate::encryption::LocalKeySource::env_var(),
                )
                .expect("open sqlite backend"),
            );
            backend
                .attach_app_file(app)
                .await
                .expect("attach the app file");
            backend
                .pool_exec(
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

            let admission = crate::transaction::TxAdmission::acquire(app.to_owned()).await;
            crate::transaction::exec_begin_or_savepoint(false, None, app,
                zeroship_data_sql::SchemaName::new(app).unwrap(), handle.clone()).await.unwrap();
            admission.handed_to_reducer();
            crate::transaction::driver::run_operation(app,
                &format!(r#"INSERT INTO "{app}"."people" (id, ssn, "__zs_raw__ssn") VALUES ('p1', '***', '123-45-6789')"#), &[])
                .await.expect("INSERT on the transaction connection");

            // CONTROL: a pool-lane read cannot see the uncommitted row.
            let outside = read_raw_column_value(
                &CapturedRoute::pool_for_tests(app, crate::compile::SqlDialect::Sqlite)
                    .bind(handle.clone()),
                "people",
                "__zs_raw__ssn",
                "p1",
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
                &CapturedRoute::tx_for_tests(app, crate::compile::SqlDialect::Sqlite)
                    .bind(handle.clone()),
                "people",
                "__zs_raw__ssn",
                "p1",
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
