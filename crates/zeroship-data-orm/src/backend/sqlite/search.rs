//! ORM search planning and execution on the captured route.
use super::{spatial, SqliteBackend};
use crate::value::Value;
use crate::{
    driver::{DriverSession, Session},
    error::DbError,
    search::*,
};
use async_trait::async_trait;

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
        session.query(r.query.sql(), r.query.params()).await
    }
    async fn spatial_near(
        &self,
        session: Option<&Session>,
        r: SpatialSearch<'_>,
    ) -> Result<Vec<Value>, DbError> {
        self.attach_app_file(r.binding.app_id()).await?;
        let auto = self.autocommit_client();
        let session: &dyn DriverSession = session.map_or(&auto as &dyn DriverSession, |s| &**s);
        self.spatial_near_on(session, r.query, r.column, r.point, r.radius_m, r.limit)
            .await
    }
}

impl SqliteBackend {
    /// Filter in SQL, then rank matching file-backed rows by haversine distance.
    #[allow(clippy::too_many_arguments)]
    pub async fn spatial_near_on(
        &self,
        session: &dyn crate::driver::DriverSession,
        query: crate::sql::compiler::CompiledQuery,
        column: &str,
        point: crate::sql::descriptors::GeoPoint,
        radius_m: f64,
        limit: usize,
    ) -> Result<Vec<crate::value::Value>, DbError> {
        let rows = session.query(query.sql(), query.params()).await?;
        let mut scored = Vec::new();
        for mut row in rows {
            let identity = row
                .as_object_mut()
                .ok_or_else(|| DbError::internal("spatial search returned a non-record"))?
                .shift_remove(crate::sql::compiler::SQLITE_SPATIAL_IDENTITY_ALIAS)
                .ok_or_else(|| DbError::internal("spatial search returned no ranking identity"))?;
            if !matches!(identity, Value::String(_)) && identity.as_i64().is_none() {
                return Err(DbError::row_decode(
                    "id",
                    "spatial search requires a text or integer identity",
                ));
            }
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
                scored.push((distance, identity, row));
            }
        }
        scored.sort_by(|a, b| {
            a.0.total_cmp(&b.0)
                .then_with(|| compare_identity(&a.1, &b.1))
        });
        scored.truncate(limit);
        let mut out = Vec::with_capacity(scored.len());
        for (distance, _, mut row) in scored {
            row.as_object_mut()
                .ok_or_else(|| DbError::internal("spatial search returned a non-record"))?
                .insert(
                    "_distance_m".into(),
                    crate::value::Number::from_f64(distance).map_or(Value::Null, Value::Number),
                );
            out.push(row);
        }
        Ok(out)
    }
}

fn compare_identity(left: &Value, right: &Value) -> std::cmp::Ordering {
    match (left, right) {
        (Value::String(left), Value::String(right)) => left.cmp(right),
        (Value::String(_), _) => std::cmp::Ordering::Greater,
        (_, Value::String(_)) => std::cmp::Ordering::Less,
        (left, right) => left
            .as_i64()
            .expect("validated spatial identity")
            .cmp(&right.as_i64().expect("validated spatial identity")),
    }
}

#[cfg(test)]
mod tests {
    use super::compare_identity;
    use crate::value;

    #[test]
    fn equal_spatial_distances_use_identity_order() {
        assert!(compare_identity(&value!(2), &value!(10)).is_lt());
        assert!(compare_identity(&value!("b"), &value!("c")).is_lt());
    }
}
