#![expect(
    clippy::future_not_send,
    reason = "database clock connections stay on their owning compio thread"
)]

use crate::error::Error;
use std::time::{Duration, Instant};
use zeroship_data_orm::{
    backend::BackendHandle,
    binding::DbBinding,
    encryption::ProjectKeySource,
    sql::registration::{POSTGRES_FAMILY, SQLITE_FAMILY},
    ConnectOptions,
};

/// Independent connection to the queue's database clock; it reads no tables.
#[derive(Clone, Debug)]
pub struct Clock {
    backend: BackendHandle,
    binding: DbBinding,
    timeout: Duration,
}

/// Anchoring before the query keeps its elapsed I/O inside the lease budget.
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub millis: i64,
    pub started: Instant,
}

impl Clock {
    pub async fn connect(binding: DbBinding, url: &str, timeout: Duration) -> Result<Self, Error> {
        let backend = ConnectOptions::new(url, ProjectKeySource::unavailable())
            .max_connections(std::num::NonZeroUsize::MIN)
            .connection_authority()
            .connect()
            .await?;
        if !matches!(
            backend.sql_registration().family(),
            POSTGRES_FAMILY | SQLITE_FAMILY
        ) {
            return Err(Error::Invalid);
        }
        compio::time::timeout(timeout, backend.prepare_for_app(binding.app_id()))
            .await
            .map_err(|_| Error::Timeout)??;
        Ok(Self {
            backend,
            binding,
            timeout,
        })
    }

    pub async fn now(&self) -> Result<i64, Error> {
        Ok(self.sample().await?.millis)
    }

    pub async fn sample(&self) -> Result<Sample, Error> {
        let sql = match self.backend.sql_registration().family() {
            POSTGRES_FAMILY => {
                "SELECT CAST(FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000) AS BIGINT) AS now"
            }
            SQLITE_FAMILY => {
                "SELECT CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER) AS now"
            }
            _ => return Err(Error::Storage),
        };
        let started = Instant::now();
        let rows = compio::time::timeout(
            self.timeout,
            self.backend
                .query(self.binding.app_id(), self.binding.schema(), sql, &[]),
        )
        .await
        .map_err(|_| Error::Timeout)??;
        let millis = rows
            .first()
            .and_then(|row| row["now"].as_i64())
            .filter(|now| *now >= 0)
            .ok_or(Error::Storage)?;
        Ok(Sample { millis, started })
    }
}
