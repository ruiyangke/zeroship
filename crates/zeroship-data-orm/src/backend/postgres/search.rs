//! ORM search planning and execution on the captured route.
use super::{pg_error, PostgresBackend};
use crate::value::Value;
use crate::{driver::Session, error::DbError, executor::ScopedExecutor, search::*};
use async_trait::async_trait;

#[async_trait(?Send)]
impl Search for PostgresBackend {
    async fn vector_search(
        &self,
        session: Option<&Session>,
        r: VectorSearch<'_>,
    ) -> Result<Vec<Value>, DbError> {
        self.ensure_pgvector_available().await?;
        match session {
            Some(session) => session.query(r.query.sql(), r.query.params()).await,
            None => {
                ScopedExecutor::query(
                    self,
                    r.binding.app_id(),
                    r.binding.schema(),
                    r.query.sql(),
                    r.query.params(),
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
        self.ensure_postgis_available().await?;
        match session {
            Some(session) => session.query(r.query.sql(), r.query.params()).await,
            None => {
                ScopedExecutor::query(
                    self,
                    r.binding.app_id(),
                    r.binding.schema(),
                    r.query.sql(),
                    r.query.params(),
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
