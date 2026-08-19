//! Apply-time MySQL AUTO_INCREMENT synchronization.
//!
//! Imported identity values can leave the table's allocator behind its live
//! maximum. This structured operation validates the exact AUTO_INCREMENT
//! association, compares an uncached live counter with `MAX(column)` while a
//! target-table write lock is held, and raises the counter only when required.
//! MySQL DDL auto-commits, so an actual counter change uses the backend's normal
//! started-marker -> DDL -> completed-event recovery protocol.

use std::time::Instant;

use crate::apply::executor::ApplyError;
use crate::apply::journal::Phase;
use crate::conn::ExecutorConfig;
use crate::driver::SqlSession;
use crate::render::step::SynchronizeIdentityStep;

use super::{journal_sql, session};

fn identity_error(step: &SynchronizeIdentityStep, message: impl std::fmt::Display) -> ApplyError {
    ApplyError::Backend(format!(
        "mysql synchronizeIdentity {}.{}.{}: {message}",
        step.schema, step.table, step.column
    ))
}

fn parse_nonnegative(
    step: &SynchronizeIdentityStep,
    field: &str,
    value: &str,
) -> Result<i128, ApplyError> {
    let parsed = value.trim().parse::<u128>().map_err(|_| {
        identity_error(
            step,
            format!("live {field} value {value:?} is not a non-negative integer"),
        )
    })?;
    i128::try_from(parsed).map_err(|_| {
        identity_error(
            step,
            format!("live {field} value {value:?} exceeds the supported integer range"),
        )
    })
}

fn parse_maximum(step: &SynchronizeIdentityStep, value: &str) -> Result<i128, ApplyError> {
    value.trim().parse::<i128>().map_err(|_| {
        identity_error(
            step,
            format!(
                "live MAX({}) value {value:?} is not an integer",
                step.column
            ),
        )
    })
}

async fn resolve_counter_advance<D: SqlSession>(
    conn: &D,
    step: &SynchronizeIdentityStep,
) -> Result<Option<String>, ApplyError> {
    let column_rows = conn
        .query(
            "SELECT EXTRA AS extra \
               FROM information_schema.COLUMNS \
              WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = ?",
            &[
                step.schema.as_str().into(),
                step.table.as_str().into(),
                step.column.as_str().into(),
            ],
        )
        .await?;
    let Some(column_row) = column_rows.first() else {
        return Err(identity_error(step, "target column does not exist"));
    };
    if column_rows.len() != 1 {
        return Err(identity_error(
            step,
            format!(
                "catalog returned {} rows for the target column; expected exactly one",
                column_rows.len()
            ),
        ));
    }
    let extra: String = column_row.try_get("extra")?;
    if !extra
        .split_ascii_whitespace()
        .any(|part| part.eq_ignore_ascii_case("auto_increment"))
    {
        return Err(identity_error(step, "target column is not AUTO_INCREMENT"));
    }

    // information_schema_stats_expiry is pinned to zero by configure_session,
    // making TABLES.AUTO_INCREMENT a fresh current value rather than a cached
    // estimate. Cast integer metadata to text so unsigned BIGINT counters retain
    // their full range across the driver-neutral seam.
    let counter_rows = conn
        .query(
            "SELECT CAST(AUTO_INCREMENT AS CHAR) AS auto_increment, \
                    CAST(@@SESSION.auto_increment_increment AS CHAR) \
                        AS auto_increment_increment \
               FROM information_schema.TABLES \
              WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND TABLE_TYPE = 'BASE TABLE'",
            &[step.schema.as_str().into(), step.table.as_str().into()],
        )
        .await?;
    let Some(counter_row) = counter_rows.first() else {
        return Err(identity_error(step, "target base table does not exist"));
    };
    if counter_rows.len() != 1 {
        return Err(identity_error(
            step,
            format!(
                "catalog returned {} rows for the target table; expected exactly one",
                counter_rows.len()
            ),
        ));
    }
    let current = counter_row
        .try_get::<_, Option<String>>("auto_increment")?
        .ok_or_else(|| identity_error(step, "AUTO_INCREMENT has no live next value"))?;
    let increment: String = counter_row.try_get("auto_increment_increment")?;
    let current = parse_nonnegative(step, "AUTO_INCREMENT", &current)?;
    let increment = parse_nonnegative(step, "auto_increment_increment", &increment)?;
    if increment == 0 {
        return Err(identity_error(
            step,
            "@@SESSION.auto_increment_increment must be greater than zero",
        ));
    }

    let schema_q = journal_sql::quote_ident_mysql(&step.schema)?;
    let table_q = journal_sql::quote_ident_mysql(&step.table)?;
    let column_q = journal_sql::quote_ident_mysql(&step.column)?;
    let maximum_row = conn
        .query_one(
            &format!(
                "SELECT CAST(MAX({column_q}) AS CHAR) AS max_value \
                   FROM {schema_q}.{table_q}"
            ),
            &[],
        )
        .await?;
    let Some(maximum) = maximum_row.try_get::<_, Option<String>>("max_value")? else {
        return Ok(None);
    };
    let maximum = parse_maximum(step, &maximum)?;
    let desired = maximum.checked_add(increment).ok_or_else(|| {
        identity_error(
            step,
            format!(
                "MAX({}) + auto_increment_increment overflows the supported integer range",
                step.column
            ),
        )
    })?;

    // ALTER TABLE permits lowering AUTO_INCREMENT while still above MAX(column)
    // on some MySQL versions. Never issue it unless the live next value is truly
    // behind the required floor.
    if desired <= current {
        return Ok(None);
    }
    if desired <= 0 {
        return Ok(None);
    }
    Ok(Some(format!(
        "ALTER TABLE {schema_q}.{table_q} AUTO_INCREMENT = {desired}"
    )))
}

/// Validate and monotonically advance one MySQL AUTO_INCREMENT allocator.
pub(super) async fn synchronize_identity<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    step: &SynchronizeIdentityStep,
    applied_by: &str,
) -> Result<bool, ApplyError> {
    if step.writes_quiesced.trim().is_empty() {
        return Err(identity_error(
            step,
            "writesQuiesced must name the maintenance window or invariant",
        ));
    }

    let entries = journal_sql::applied(conn, cfg).await?;
    let mut had_inflight = false;
    for entry in entries
        .iter()
        .filter(|entry| entry.version == step.migration.version.as_str())
    {
        if entry.checksum != step.migration.checksum.as_str() {
            return Err(ApplyError::ChecksumDrift {
                version: step.migration.version.as_str().to_string(),
                recorded: entry.checksum.clone(),
                expected: step.migration.checksum.as_str().to_string(),
            });
        }
        match entry.phase {
            Phase::Completed => return Ok(false),
            Phase::Started => had_inflight = true,
        }
    }

    let snapshot = session::snapshot_session(conn).await?;
    let result = async {
        session::configure_session(conn, cfg, &step.migration).await?;

        if had_inflight {
            session::apply_two_phase(
                conn,
                cfg,
                &step.migration,
                applied_by,
                true,
                &[],
                "apply",
            )
            .await?;
            unreachable!("MySQL two-phase apply always refuses an inflight marker");
        }

        // The authored writesQuiesced assertion covers application writers.
        // This explicit write lock additionally keeps MySQL-side allocation and
        // external DDL stable across the catalog/MAX comparison and ALTER.
        let schema_q = journal_sql::quote_ident_mysql(&step.schema)?;
        let table_q = journal_sql::quote_ident_mysql(&step.table)?;
        let meta_q = journal_sql::quote_ident_mysql(&cfg.pg.meta_schema)?;
        let inflight_q = journal_sql::quote_ident_mysql("schema_migrations_inflight")?;
        conn.batch(&format!(
            "LOCK TABLES {schema_q}.{table_q} WRITE, {meta_q}.{inflight_q} WRITE"
        ))
        .await?;

        let ddl = match resolve_counter_advance(conn, step).await {
            Ok(ddl) => ddl,
            Err(error) => {
                if let Err(unlock) = conn.batch("UNLOCK TABLES").await {
                    return Err(ApplyError::Backend(format!(
                        "{error}; additionally failed to release MySQL identity-synchronization lock: {unlock}"
                    )));
                }
                return Err(error);
            }
        };
        if let Err(error) = journal_sql::record_started(
            conn,
            cfg,
            step.migration.version.as_str(),
            &step.migration.name,
            step.migration.checksum.as_str(),
            applied_by,
        )
        .await
        {
            if let Err(unlock) = conn.batch("UNLOCK TABLES").await {
                return Err(ApplyError::Backend(format!(
                    "{error}; additionally failed to release MySQL identity-synchronization lock: {unlock}"
                )));
            }
            return Err(error.into());
        }

        let started = Instant::now();
        let ddl_result = match ddl.as_deref() {
            Some(sql) => conn.batch(sql).await,
            None => Ok(()),
        };
        let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        let unlock_result = conn.batch("UNLOCK TABLES").await;
        if let Err(source) = ddl_result {
            if let Err(unlock) = unlock_result {
                return Err(ApplyError::Backend(format!(
                    "MySQL identity synchronization failed ({source}) and its explicit table lock could not be released ({unlock}); the inflight marker was retained"
                )));
            }
            return Err(ApplyError::MigrationFailed {
                version: step.migration.version.as_str().to_string(),
                source: source.into(),
            });
        }
        if let Err(unlock) = unlock_result {
            return Err(ApplyError::Backend(format!(
                "MySQL identity synchronization completed but explicit table-lock cleanup failed: {unlock}; the inflight marker was retained for recovery"
            )));
        }

        session::finalize_started_structured_ddl(
            conn,
            cfg,
            &step.migration,
            applied_by,
            exec_ms,
        )
        .await?;
        Ok(true)
    }
    .await;

    let restored = session::restore_session(conn, &snapshot).await;
    match (result, restored) {
        (Err(error), Err(restore)) => {
            tracing::warn!(
                error = %restore,
                version = %step.migration.version.as_str(),
                "zero-migrate: failed to restore MySQL session after identity synchronization error"
            );
            Err(error)
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(ran), Ok(())) => Ok(ran),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use crate::apply::backend::MigrationBackend;
    use crate::driver::{Bind, DbError, Row, Value};
    use crate::model::migration::{
        Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId,
    };

    use super::*;
    use crate::apply::backend::mysql::MysqlBackend;

    struct RecordingSession {
        log: RefCell<Vec<String>>,
        binds: RefCell<Vec<Vec<Bind>>>,
        column_extra: Option<String>,
        current_counter: Option<String>,
        increment: String,
        maximum: Option<String>,
        journal: Option<(String, String, Phase)>,
        in_transaction: bool,
    }

    impl RecordingSession {
        fn new(current_counter: Option<i128>, maximum: Option<i128>, increment: u64) -> Self {
            Self {
                log: RefCell::new(Vec::new()),
                binds: RefCell::new(Vec::new()),
                column_extra: Some("auto_increment".into()),
                current_counter: current_counter.map(|value| value.to_string()),
                increment: increment.to_string(),
                maximum: maximum.map(|value| value.to_string()),
                journal: None,
                in_transaction: false,
            }
        }

        fn without_auto_increment(mut self) -> Self {
            self.column_extra = Some("DEFAULT_GENERATED".into());
            self
        }

        fn with_journal(mut self, step: &SynchronizeIdentityStep, phase: Phase) -> Self {
            self.journal = Some((
                step.migration.version.as_str().to_string(),
                step.migration.checksum.as_str().to_string(),
                phase,
            ));
            self
        }

        fn rows_for(&self, sql: &str) -> Vec<Row> {
            if sql.contains("WITH ranked AS") {
                self.journal.as_ref().map_or_else(Vec::new, |entry| {
                    vec![Row::new(
                        vec![
                            "version".into(),
                            "checksum".into(),
                            "mig_kind".into(),
                            "event_seq".into(),
                            "phase".into(),
                            // The applied read selects the stored reverse now.
                            "down".into(),
                        ],
                        vec![
                            Value::Text(entry.0.clone()),
                            Value::Text(entry.1.clone()),
                            if entry.2 == Phase::Completed {
                                Value::Text("apply".into())
                            } else {
                                Value::Null
                            },
                            Value::Int(1),
                            Value::Text(entry.2.as_str().into()),
                            Value::Null,
                        ],
                    )]
                })
            } else if sql.contains("@@SESSION.sql_mode AS sql_mode") {
                vec![Row::new(
                    vec![
                        "sql_mode".into(),
                        "time_zone".into(),
                        "max_execution_time".into(),
                        "innodb_lock_wait_timeout".into(),
                        "information_schema_stats_expiry".into(),
                        "autocommit".into(),
                        "foreign_key_checks".into(),
                        "unique_checks".into(),
                        "transaction_tracking_enabled".into(),
                        "in_transaction".into(),
                    ],
                    vec![
                        Value::Text("ANSI_QUOTES".into()),
                        Value::Text("SYSTEM".into()),
                        Value::Int(17),
                        Value::Int(23),
                        Value::Int(47),
                        Value::Int(0),
                        Value::Int(1),
                        Value::Int(0),
                        Value::Int(1),
                        Value::Int(i64::from(self.in_transaction)),
                    ],
                )]
            } else if sql.contains("FROM information_schema.COLUMNS") {
                self.column_extra.as_ref().map_or_else(Vec::new, |extra| {
                    vec![Row::new(
                        vec!["extra".into()],
                        vec![Value::Text(extra.clone())],
                    )]
                })
            } else if sql.contains("FROM information_schema.TABLES") {
                vec![Row::new(
                    vec!["auto_increment".into(), "auto_increment_increment".into()],
                    vec![
                        self.current_counter
                            .as_ref()
                            .map_or(Value::Null, |value| Value::Text(value.clone())),
                        Value::Text(self.increment.clone()),
                    ],
                )]
            } else if sql.contains("AS max_value") {
                vec![Row::new(
                    vec!["max_value".into()],
                    vec![self
                        .maximum
                        .as_ref()
                        .map_or(Value::Null, |value| Value::Text(value.clone()))],
                )]
            } else {
                Vec::new()
            }
        }

        fn alter_statements(&self) -> Vec<String> {
            self.log
                .borrow()
                .iter()
                .filter_map(|entry| {
                    entry
                        .strip_prefix("batch: ALTER TABLE ")
                        .map(str::to_string)
                })
                .collect()
        }
    }

    impl SqlSession for RecordingSession {
        async fn batch(&self, sql: &str) -> Result<(), DbError> {
            self.log.borrow_mut().push(format!("batch: {sql}"));
            Ok(())
        }

        async fn exec(&self, sql: &str, params: &[Bind]) -> Result<u64, DbError> {
            self.log.borrow_mut().push(format!("exec: {sql}"));
            self.binds.borrow_mut().push(params.to_vec());
            Ok(1)
        }

        async fn exec_text(&self, sql: &str, _params: &[Option<String>]) -> Result<u64, DbError> {
            self.log.borrow_mut().push(format!("exec_text: {sql}"));
            Ok(1)
        }

        async fn query(&self, sql: &str, params: &[Bind]) -> Result<Vec<Row>, DbError> {
            self.log.borrow_mut().push(format!("query: {sql}"));
            self.binds.borrow_mut().push(params.to_vec());
            Ok(self.rows_for(sql))
        }

        async fn query_one(&self, sql: &str, params: &[Bind]) -> Result<Row, DbError> {
            self.log.borrow_mut().push(format!("query_one: {sql}"));
            self.binds.borrow_mut().push(params.to_vec());
            self.rows_for(sql)
                .into_iter()
                .next()
                .ok_or_else(|| DbError::message("query_one: no canned row"))
        }
    }

    fn step() -> SynchronizeIdentityStep {
        let flags = MigrationFlags::default();
        let version = MigrationId::generate();
        let up = "-- structured synchronize identity";
        let checksum = Checksum::of(&ChecksumInput {
            up,
            down: None,
            flags: &flags,
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        SynchronizeIdentityStep {
            migration: Migration {
                version,
                name: "synchronize_orders_id".into(),
                up: up.into(),
                down: None,
                checksum,
                flags,
                owner_app: "app_test".into(),
                depends_on: Vec::new(),
                supersedes: Vec::new(),
                preconditions: Vec::new(),
                existence_guard: None,
                effect: None,
            },
            schema: "app".into(),
            table: "orders".into(),
            column: "id".into(),
            writes_quiesced: "maintenance-window-2026-07-17".into(),
        }
    }

    fn cfg() -> ExecutorConfig {
        ExecutorConfig::new(
            "prj_identity",
            "identity_meta",
            crate::test_fixtures::no_inject("identity_meta"),
        )
    }

    #[compio::test]
    async fn advances_a_behind_counter_by_the_session_increment() {
        let step = step();
        let rec = RecordingSession::new(Some(10), Some(20), 3);
        let backend = MysqlBackend::new_generic(&rec);

        let ran = backend
            .synchronize_identity(&cfg(), &step, "tester")
            .await
            .expect("identity synchronization succeeds");

        assert!(ran);
        assert_eq!(
            rec.alter_statements(),
            vec!["`app`.`orders` AUTO_INCREMENT = 23".to_string()]
        );
        let all = rec.log.borrow().join("\n");
        assert!(
            all.contains("'NO_AUTO_VALUE_ON_ZERO'")
                && all.contains("SESSION information_schema_stats_expiry = 0"),
            "the import and uncached-metadata invariants are pinned: {all}"
        );
        let lock = all.find("LOCK TABLES `app`.`orders` WRITE").unwrap();
        let maximum = all.find("MAX(`id`)").unwrap();
        let alter = all
            .find("ALTER TABLE `app`.`orders` AUTO_INCREMENT = 23")
            .unwrap();
        let unlock = all[alter..].find("UNLOCK TABLES").unwrap() + alter;
        let restore = all
            .rfind("SESSION information_schema_stats_expiry = 47")
            .unwrap();
        assert!(lock < maximum && maximum < alter && alter < unlock && unlock < restore);
    }

    #[compio::test]
    async fn already_ahead_counter_is_never_lowered() {
        let step = step();
        let rec = RecordingSession::new(Some(100), Some(20), 3);
        let backend = MysqlBackend::new_generic(&rec);

        let ran = backend
            .synchronize_identity(&cfg(), &step, "tester")
            .await
            .expect("ahead generator is a journaled no-op");

        assert!(ran);
        assert!(
            rec.alter_statements().is_empty(),
            "an already-higher allocator must never receive a lowering ALTER: {:?}",
            rec.log.borrow()
        );
        let all = rec.log.borrow().join("\n");
        assert!(
            all.contains("'completed', 'success'")
                && all
                    .contains("DELETE FROM `identity_meta_migrations`.schema_migrations_inflight"),
            "the monotonic no-op is still journaled: {all}"
        );
    }

    #[compio::test]
    async fn rejects_a_non_auto_increment_column_before_journaling() {
        let step = step();
        let rec = RecordingSession::new(Some(10), Some(20), 1).without_auto_increment();
        let backend = MysqlBackend::new_generic(&rec);

        let result = backend.synchronize_identity(&cfg(), &step, "tester").await;

        assert!(
            matches!(result, Err(ApplyError::Backend(ref message))
                if message.contains("mysql synchronizeIdentity app.orders.id")
                    && message.contains("not AUTO_INCREMENT")),
            "{result:?}"
        );
        let all = rec.log.borrow().join("\n");
        assert!(
            all.contains("UNLOCK TABLES"),
            "validation lock is released: {all}"
        );
        assert!(
            !rec.log.borrow().iter().any(|entry| {
                entry.starts_with("exec: INSERT INTO ")
                    && entry.contains("schema_migrations_inflight")
            }) && !all.contains("ALTER TABLE"),
            "invalid targets must fail before a marker or ALTER: {all}"
        );
    }

    #[compio::test]
    async fn completed_identity_step_skips_without_touching_the_allocator() {
        let step = step();
        let rec =
            RecordingSession::new(Some(10), Some(20), 1).with_journal(&step, Phase::Completed);
        let backend = MysqlBackend::new_generic(&rec);

        let ran = backend
            .synchronize_identity(&cfg(), &step, "tester")
            .await
            .expect("completed step skips");

        assert!(!ran);
        let all = rec.log.borrow().join("\n");
        assert!(!all.contains("LOCK TABLES") && !all.contains("ALTER TABLE"));
    }
}
