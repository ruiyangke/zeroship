//! ORM search planning and execution on the captured route.
use super::{PostgresBackend, pg_error};
use crate::{driver::Session, error::DbError, executor::ScopedExecutor, search::*};
use async_trait::async_trait;
use crate::sql::descriptors::{GeoPoint, VectorMetric};
use crate::value::Value;

#[async_trait(?Send)]
impl Search for PostgresBackend {
    async fn vector_search(
        &self,
        session: Option<&Session>,
        r: VectorSearch<'_>,
    ) -> Result<Vec<Value>, DbError> {
        let q = self
            .plan_vector_search(
                r.binding,
                r.collection,
                r.column,
                r.query,
                r.k,
                r.metric,
                r.filter,
                r.schema,
            )
            .await?;
        match session {
            Some(session) => session.query(&q.sql, &q.params).await,
            None => {
                ScopedExecutor::query(
                    self,
                    r.binding.app_id(),
                    r.binding.schema(),
                    &q.sql,
                    &q.params,
                )
                .await
            }
        }
    }
    async fn spatial_near(
        &self,
        session: Option<&Session>,
        r: SpatialSearch<'_>,
    ) -> Result<Vec<Value>, DbError> {
        let q = self
            .plan_spatial_near(
                r.binding,
                r.collection,
                r.column,
                r.point,
                r.radius_m,
                r.filter,
                r.limit,
                r.schema,
            )
            .await?;
        match session {
            Some(session) => session.query(&q.sql, &q.params).await,
            None => {
                ScopedExecutor::query(
                    self,
                    r.binding.app_id(),
                    r.binding.schema(),
                    &q.sql,
                    &q.params,
                )
                .await
            }
        }
    }
}
impl PostgresBackend {
    /// Check (and cache) whether the `vector` extension is installed on
    /// the connected database. The probe runs at most once per backend
    /// instance — pgvector is provisioned at admin time and stays present
    /// for the life of the process.
    ///
    /// Returns `Ok(())` when present; `Err(DbError::Configuration)` with
    /// code `vector_extension_missing` otherwise. Connection failures
    /// during the probe surface as `DbError::Transient` so callers can
    /// distinguish "extension missing" from "database unreachable".
    async fn ensure_pgvector_available(&self) -> Result<(), DbError> {
        // Fast path: cached result.
        if let Some(present) = *self.pgvector_available.borrow() {
            if present {
                return Ok(());
            }
            return Err(DbError::config_hinted(
                "vector_extension_missing",
                "pgvector is not installed on this database",
                "run `CREATE EXTENSION vector;` (Postgres superuser) or \
                 swap the database image to `pgvector/pgvector:pg16` \
                 (see docs/runbooks/docker-compose.md)",
            ));
        }

        let empty: Vec<&str> = Vec::new();
        let rows = self
            .pool()
            .query_text_params("SELECT 1 FROM pg_extension WHERE extname='vector'", &empty)
            .await
            .map_err(|e| pg_error::classify(&e))?;
        let present = !rows.is_empty();
        *self.pgvector_available.borrow_mut() = Some(present);
        if present {
            Ok(())
        } else {
            Err(DbError::config_hinted(
                "vector_extension_missing",
                "pgvector is not installed on this database",
                "run `CREATE EXTENSION vector;` (Postgres superuser) or \
                 swap the database image to `pgvector/pgvector:pg16` \
                 (see docs/runbooks/docker-compose.md)",
            ))
        }
    }
}

impl PostgresBackend {
    /// Probe the extension and render the statement, WITHOUT running it.
    ///
    /// Split out of [`crate::search::Search::vector_search`] on 2026-09-03 so the caller
    /// chooses the CONNECTION. The trait method can only reach the pool: it
    /// takes `&self` and nothing that says which lane this dispatch belongs to,
    /// so a `search` issued inside `db.transaction(fn)` scanned a pooled
    /// checkout and could not see the transaction's own uncommitted rows. The
    /// engine's routed entry point plans here and then executes on the lane
    /// `route.in_tx()` names. Bound by `plugin-db/src/tests/search_tx_lane.rs`.
    ///
    /// # Errors
    ///
    /// `vector_extension_missing` when pgvector is absent; a query-builder
    /// error when the collection, column or filter is not one the descriptor
    /// declares.
    #[allow(clippy::too_many_arguments)]
    pub async fn plan_vector_search(
        &self,
        binding: &zeroship_data_orm::binding::DbBinding,
        collection: &str,
        column: &str,
        query: &[f32],
        k: usize,
        metric: VectorMetric,
        filter: &crate::value::Value,
        schema: &crate::value::Value,
    ) -> Result<crate::sql::compile::BuiltQuery, DbError> {
        // Probe so a missing extension surfaces with the same typed
        // error shape the capability probe produces — the SDK branches
        // on `e.code === "vector_extension_missing"` regardless of
        // which entry point fired.
        self.ensure_pgvector_available().await?;

        // The projection allowlist and the `column` identifier check both come
        // off the descriptor. A collection this deploy does not declare is
        // refused here rather than searched with an unbounded projection.
        crate::sql::compile::build_vector_search(
            binding.schema(),
            collection,
            column,
            query,
            k,
            metric,
            filter,
            schema,
        )
        .map_err(DbError::from)
    }
}

// ---------------------------------------------------------------------------
// SpatialIndex — PostGIS adapter
// ---------------------------------------------------------------------------
//
// One method: `spatial_near` — `WHERE ST_DWithin(col, ST_MakePoint(lng, lat)::
// geography, radius) ORDER BY ST_Distance(...) LIMIT $4`.
//
// The GiST index it reads is NOT created here. `zeroship-migrate` authors it
// from the declared `t.geoPoint()` field
// (`zeroship-migrate-core/src/render/declarative.rs::geo_index_snapshot`,
// emitted as `USING gist ("col")`).
//
// Both probe `pg_extension WHERE extname='postgis'` on first call and
// cache on `postgis_available`. Absence surfaces as
// `DbError::Configuration { code: "postgis_extension_missing", ... }`.
// ---------------------------------------------------------------------------

impl PostgresBackend {
    /// Check (and cache) whether the `postgis` extension is installed on
    /// the connected database. Mirrors [`Self::ensure_pgvector_available`]
    /// — the probe runs at most once per backend; PostGIS is
    /// provisioned at admin time and stays present.
    async fn ensure_postgis_available(&self) -> Result<(), DbError> {
        if let Some(present) = *self.postgis_available.borrow() {
            if present {
                return Ok(());
            }
            return Err(DbError::config_hinted(
                "postgis_extension_missing",
                "PostGIS is not installed on this database",
                "run `CREATE EXTENSION postgis;` (Postgres superuser) or \
                 swap the database image to a PostGIS-bundled variant \
                 (see docs/runbooks/docker-compose.md)",
            ));
        }

        let empty: Vec<&str> = Vec::new();
        let rows = self
            .pool()
            .query_text_params("SELECT 1 FROM pg_extension WHERE extname='postgis'", &empty)
            .await
            .map_err(|e| pg_error::classify(&e))?;
        let present = !rows.is_empty();
        *self.postgis_available.borrow_mut() = Some(present);
        if present {
            Ok(())
        } else {
            Err(DbError::config_hinted(
                "postgis_extension_missing",
                "PostGIS is not installed on this database",
                "run `CREATE EXTENSION postgis;` (Postgres superuser) or \
                 swap the database image to a PostGIS-bundled variant \
                 (see docs/runbooks/docker-compose.md)",
            ))
        }
    }
}

impl PostgresBackend {
    /// Probe PostGIS and render the statement, WITHOUT running it. The spatial
    /// twin of [`Self::plan_vector_search`]; see there for why the execution
    /// is the caller's decision.
    ///
    /// # Errors
    ///
    /// `postgis_extension_missing` when PostGIS is absent; a query-builder
    /// error when the collection, column or filter is not one the descriptor
    /// declares.
    #[allow(clippy::too_many_arguments)]
    pub async fn plan_spatial_near(
        &self,
        binding: &zeroship_data_orm::binding::DbBinding,
        collection: &str,
        column: &str,
        point: GeoPoint,
        radius_m: f64,
        filter: &crate::value::Value,
        limit: Option<usize>,
        schema: &crate::value::Value,
    ) -> Result<crate::sql::compile::BuiltQuery, DbError> {
        self.ensure_postgis_available().await?;

        crate::sql::compile::build_spatial_near(
            binding.schema(),
            collection,
            column,
            point,
            radius_m,
            filter,
            limit,
            schema,
        )
        .map_err(DbError::from)
    }
}
