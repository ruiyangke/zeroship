//! ORM search planning and execution on the captured route.
use super::{SqliteBackend, spatial, vector};
use crate::{
    driver::{DriverSession, Session},
    error::DbError,
    search::*,
};
use async_trait::async_trait;
use zeroship_data_sql::value::Value;

#[async_trait(?Send)]
impl Search for SqliteBackend {
    async fn vector_search(
        &self,
        session: Option<&Session>,
        r: VectorSearch<'_>,
    ) -> Result<Vec<Value>, DbError> {
        self.attach_app_file(r.binding.app_id()).await?;
        let auto = self.autocommit_client();
        let session: &dyn DriverSession = session.map_or(&auto as &dyn DriverSession, |s| &**s);
        self.vector_search_on(
            session,
            r.binding,
            r.collection,
            r.column,
            r.query,
            r.k,
            r.metric,
            r.filter,
            r.schema,
        )
        .await
    }
    async fn spatial_near(
        &self,
        session: Option<&Session>,
        r: SpatialSearch<'_>,
    ) -> Result<Vec<Value>, DbError> {
        self.attach_app_file(r.binding.app_id()).await?;
        let auto = self.autocommit_client();
        let session: &dyn DriverSession = session.map_or(&auto as &dyn DriverSession, |s| &**s);
        self.spatial_near_on(
            session,
            r.binding,
            r.collection,
            r.column,
            r.point,
            r.radius_m,
            r.filter,
            r.limit,
            r.schema,
        )
        .await
    }
}

impl SqliteBackend {
    /// vec0-powered top-k vector search **on `session`**. Composes a SQL of the
    /// form
    ///
    /// ```sql
    /// SELECT t.*, v.distance AS _distance
    ///   FROM "<app>"."<coll>" t
    ///   JOIN "<app>"."<coll>__vec_<col>" v ON t.rowid = v.rowid
    ///  WHERE v."<col>" MATCH x'…' AND k = ?
    ///    AND <filter>
    ///  ORDER BY v.distance;
    /// ```
    ///
    /// The compiler binds the vector as native bytes and preserves the
    /// descriptor's protected projection.
    ///
    /// **`session` is a parameter because SC-2 split the lanes.** A handle
    /// carrying a transaction lease routes onto `tx_conn`; one without mints an
    /// autocommit reservation on `op_conn`. Those are two connections, so a
    /// scan issued inside `db.transaction(fn)` that took the autocommit handle
    /// could not see the transaction's own uncommitted rows. Bound by
    /// `plugin-db/src/tests/search_tx_lane.rs`.
    ///
    /// # Errors
    ///
    /// `vector_unsupported` for an inner-product metric, the query builder's
    /// own refusals, and any error the statement raises.
    #[allow(clippy::too_many_arguments)]
    pub async fn vector_search_on(
        &self,
        session: &dyn crate::driver::DriverSession,
        binding: &zeroship_data_orm::binding::DbBinding,
        collection: &str,
        column: &str,
        query: &[f32],
        k: usize,
        metric: zeroship_data_sql::descriptors::VectorMetric,
        filter: &zeroship_data_sql::value::Value,
        schema: &zeroship_data_sql::value::Value,
    ) -> Result<Vec<zeroship_data_sql::value::Value>, DbError> {
        let app_id = binding.app_id();
        // Reject inner-product before issuing any SQL — a vec0 vtable
        // cannot be declared with `distance_metric=ip`, so no shadow
        // relation this search could join to would ever answer an IP
        // query. Refuse with the typed code rather than emitting SQL
        // that fails on a missing operator.
        vector::reject_inner_product(metric)?;

        let query = zeroship_data_sql::sqlite_search::build_vector_search(
            app_id, collection, column, query, k, filter, schema,
        )?;
        session.query(&query.sql, &query.params).await
    }
}

/// SCHEMA, not tenant: this qualifies the table the query reads.
///
/// On SQLite the two are the same string today because the ATTACH alias IS
/// the app id - see `attach_app_file`. The parameter states which of the two
/// meanings the query builder is being handed.
pub(super) fn build_spatial_near_base_query(
    schema_name: &zeroship_data_sql::SchemaName,
    collection: &str,
    filter: &zeroship_data_sql::value::Value,
    schema_hint: &zeroship_data_sql::value::Value,
) -> Result<zeroship_data_sql::compile::BuiltQuery, DbError> {
    zeroship_data_sql::compile::build_find_with_schema_and_unmask_and_soft_delete_with_dialect(
        schema_name,
        collection,
        filter,
        /* limit  */ None,
        /* offset */ None,
        /* order_by */ None,
        /* select   */ None,
        schema_hint,
        /* unmask_columns */ &[],
        /* filter_soft_deleted */ false,
        zeroship_data_sql::compile::SqlDialect::Sqlite,
    )
    .map_err(DbError::from)
}

// ---------------------------------------------------------------------------
// `SpatialIndex` impl (pure-Rust haversine + flat scan)
// ---------------------------------------------------------------------------
//
// Pure-Rust over an R-tree (Q-P4-C, plan §4.3): same rationale as the
// vector path's pure-Rust-over-`sqlite-vec` decision — bundling the
// R-tree extension would require either forking the SQLite
// amalgamation per CI platform or runtime-loading a `.so`, both of
// which defeat the "no system libsqlite3" invariant. The haversine
// flat scan is acceptable at dev scale; production spatial workloads
// run on PostGIS via the PG arm.
//
// One method (the flat scan needs no index at all, so there was never
// anything for an `ensure_spatial_index` to do on this arm):
//   * `spatial_near` — SELECT all rows matching `filter` via the
//     session actor's `query_typed`, decode each row's `column` blob
//     via `spatial::blob_to_point`, compute `haversine_m(point, row_point)`,
//     filter rows with `distance <= radius_m`, sort ASC, take top-
//     `limit`, and re-emit as JSON with a synthetic `_distance_m: f64`
//     field.

impl SqliteBackend {
    /// Haversine flat scan **on `session`**, the spatial twin of
    /// [`Self::vector_search_on`]; see there for why the session is a
    /// parameter.
    ///
    /// # Errors
    ///
    /// `invalid_geo_arg` when the named column is absent from the result row or
    /// is not a BLOB, the query builder's own refusals, and any error the
    /// statement raises.
    #[allow(clippy::too_many_arguments)]
    pub async fn spatial_near_on(
        &self,
        session: &dyn crate::driver::DriverSession,
        binding: &zeroship_data_orm::binding::DbBinding,
        collection: &str,
        column: &str,
        point: zeroship_data_sql::descriptors::GeoPoint,
        radius_m: f64,
        filter: &zeroship_data_sql::value::Value,
        limit: Option<usize>,
        schema: &zeroship_data_sql::value::Value,
    ) -> Result<Vec<zeroship_data_sql::value::Value>, DbError> {
        // Build the WHERE clause via the same machinery `dispatch_find`
        // uses (the SQLite-on-PG-SQL path; `$N` placeholders bind
        // positionally on rusqlite). No ORDER BY at the SQL layer —
        // we sort in Rust by computed distance.
        let schema_hint = schema;
        let bq = build_spatial_near_base_query(binding.schema(), collection, filter, schema_hint)?;
        let param_refs = &bq.params;
        let rows = session.query(&bq.sql, param_refs).await?;
        let mut scored = Vec::new();
        for row in rows {
            let blob = match row.get(column) {
                Some(Value::Bytes(bytes)) => bytes,
                Some(Value::Null) => continue,
                _ => {
                    return Err(DbError::validation(
                        "invalid_geo_arg",
                        format!("db: geoPoint column '{column}' is absent or not binary"),
                    ));
                }
            };
            let row_point = spatial::blob_to_point(blob)?;
            let distance = spatial::haversine_m(point, row_point);
            if distance <= radius_m {
                scored.push((distance, row));
            }
        }
        scored.sort_by(|a, b| a.0.total_cmp(&b.0));
        if let Some(limit) = limit {
            scored.truncate(limit);
        }
        let mut out = Vec::with_capacity(scored.len());
        for (distance, mut row) in scored {
            row.as_object_mut()
                .ok_or_else(|| DbError::internal("spatial search returned a non-record"))?
                .insert(
                    "_distance_m".into(),
                    zeroship_data_sql::value::Number::from_f64(distance)
                        .map_or(Value::Null, Value::Number),
                );
            out.push(row);
        }
        Ok(out)
    }
}

// ===========================================================================
// Key-store accessor on SqliteBackend
// ===========================================================================
//
// Both backends resolve host-supplied project keys through the same key store.
