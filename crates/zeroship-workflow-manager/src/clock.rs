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

pub const RESOLUTION_MILLIS: i64 = 1;

const SQLITE_CLOCK_SQL: &str = "SELECT CAST(strftime('%s','now') AS INTEGER) * 1000 \
    + CAST(substr(strftime('%f','now'),4,3) AS INTEGER) AS now";

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
        compio::time::timeout(
            timeout,
            backend.prepare_for_app(&binding),
        )
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
            SQLITE_FAMILY => SQLITE_CLOCK_SQL,
            _ => return Err(Error::Storage),
        };
        let started = Instant::now();
        let rows = compio::time::timeout(
            self.timeout,
            self.backend
                .query(&self.binding, sql, &[]),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_clock_extraction_preserves_fractional_epoch_ticks() {
        let database = rusqlite::Connection::open_in_memory().unwrap();
        let sql = SQLITE_CLOCK_SQL.replace("'now'", "?1");
        for (instant, expected) in [
            ("1970-01-01 00:00:00.001", 1_i64),
            ("2026-09-13 06:00:00.004", 1_789_279_200_004),
            ("2026-09-13 06:00:00.999", 1_789_279_200_999),
            ("2026-09-13 06:00:01.000", 1_789_279_201_000),
        ] {
            let observed: i64 = database
                .query_row(&sql, [instant], |row| row.get(0))
                .unwrap();
            assert_eq!(observed, expected, "{instant}");
        }
    }
}
