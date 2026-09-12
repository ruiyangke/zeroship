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
    /// Cache whether the connected database provides pgvector.
    async fn ensure_pgvector_available(&self) -> Result<(), DbError> {
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
    /// Cache whether the connected database provides PostGIS.
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
