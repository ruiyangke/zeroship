//! PostgreSQL execution for import-time identity synchronization.
//!
//! This is targeted execution validation, not schema-drift introspection. It
//! resolves only the owned sequence for the authored table/column, reads that
//! sequence's direction and state, and advances it only when its next candidate
//! is not already beyond the imported data's directional extreme.
//!
//! PostgreSQL sequence changes are deliberately non-transactional: `setval`
//! survives a later transaction rollback. That is safe here because the update
//! is monotonic and retryable. A retry after a journal/commit failure observes
//! the already-advanced sequence, leaves it in place, and records completion.

use std::time::Instant;

use crate::apply::executor::ApplyError;
use crate::apply::journal::{self, JournalError, Phase};
use crate::conn::ExecutorConfig;
use crate::driver::SqlSession;
use crate::render::dml::quote_ident_checked;
use crate::render::step::SynchronizeIdentityStep;

use super::session;

#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnedSequence {
    oid: i64,
    schema: String,
    name: String,
    increment: i64,
    min_value: i64,
    max_value: i64,
    cycle: bool,
    column_is_integer: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SequenceState {
    last_value: i64,
    is_called: bool,
}

/// Apply one structured synchronization. The generic plan engine normally
/// performs the net-applied check first; repeating it here protects direct
/// backend callers and makes a post-`setval`, pre-journal retry idempotent.
pub(super) async fn apply<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    step: &SynchronizeIdentityStep,
    applied_by: &str,
) -> Result<bool, ApplyError> {
    if step.writes_quiesced.trim().is_empty() {
        return Err(sync_error(
            step,
            "writesQuiesced must contain non-whitespace text naming the no-concurrent-writer window or invariant",
        ));
    }

    let marker = &step.migration;
    let completed = journal::applied(conn, cfg)
        .await?
        .into_iter()
        .filter(|entry| matches!(entry.phase, Phase::Completed))
        .find(|entry| entry.version == marker.version.as_str());
    if let Some(entry) = completed {
        if entry.checksum != marker.checksum.as_str() {
            return Err(ApplyError::ChecksumDrift {
                version: marker.version.as_str().to_string(),
                recorded: entry.checksum,
                expected: marker.checksum.as_str().to_string(),
            });
        }
        return Ok(false);
    }

    // Quote every authored/engine identifier before BEGIN so a malformed name
    // cannot strand an open transaction on an early return.
    let table_q = format!(
        "{}.{}",
        quote_ident_checked(&step.schema)?,
        quote_ident_checked(&step.table)?
    );
    let column_q = quote_ident_checked(&step.column)?;
    let session_sql = session::set_local_session_sql(cfg, marker)?;
    let role_sql = session::set_local_role_sql(cfg)?;
    let meta_q = quote_ident_checked(&cfg.pg.meta_schema)?;

    conn.batch("BEGIN").await?;
    let started = Instant::now();
    let result = apply_inside_transaction(
        conn,
        cfg,
        step,
        &table_q,
        &column_q,
        &session_sql,
        role_sql.as_deref(),
        &meta_q,
        applied_by,
        started,
    )
    .await;

    match result {
        Ok(()) => {
            conn.batch("COMMIT").await?;
            Ok(true)
        }
        Err(error) => {
            if let Err(rollback_error) = conn.batch("ROLLBACK").await {
                tracing::warn!(
                    error = %rollback_error,
                    version = %marker.version.as_str(),
                    "zero-migrate: ROLLBACK failed after PostgreSQL identity synchronization"
                );
            }
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn apply_inside_transaction<D: SqlSession>(
    conn: &D,
    _cfg: &ExecutorConfig,
    step: &SynchronizeIdentityStep,
    table_q: &str,
    column_q: &str,
    session_sql: &str,
    role_sql: Option<&str>,
    meta_q: &str,
    applied_by: &str,
    started: Instant,
) -> Result<(), ApplyError> {
    conn.batch(session_sql).await?;

    // The authored writesQuiesced assertion remains load-bearing: it covers
    // direct nextval users and cached allocations held by other sessions, which
    // a table lock cannot prove absent. This lock additionally makes the table
    // extreme stable against ordinary INSERT/UPDATE writers for this critical
    // section.
    conn.batch(&format!("LOCK TABLE {table_q} IN SHARE ROW EXCLUSIVE MODE"))
        .await?;

    let sequence = read_owned_sequence(conn, step).await?;
    if !sequence.column_is_integer {
        return Err(sync_error(
            step,
            "the owned sequence is attached to a non-integer column; PostgreSQL identity synchronization requires smallint, integer, or bigint",
        ));
    }
    if sequence.increment == 0 {
        return Err(sync_error(
            step,
            "the owned sequence reports an invalid zero increment",
        ));
    }
    if sequence.cycle {
        return Err(sync_error(
            step,
            "the owned sequence is CYCLE; a cycling generator cannot satisfy the monotonic no-backward synchronization contract",
        ));
    }

    let extreme = read_data_extreme(conn, table_q, column_q, sequence.increment).await?;
    if let Some(extreme) = extreme {
        let state = read_sequence_state(conn, &sequence).await?;
        if generator_is_behind(sequence.increment, state, extreme) {
            // Explicitly imported values may legitimately sit outside custom
            // sequence bounds. That is harmless when the generator is already
            // directionally ahead, but an out-of-bounds setval target cannot be
            // reached and must fail clearly instead of moving anything.
            if extreme < sequence.min_value || extreme > sequence.max_value {
                return Err(sync_error(
                    step,
                    format!(
                        "imported directional extreme {extreme} is outside owned sequence {}.{} bounds {}..={}",
                        sequence.schema, sequence.name, sequence.min_value, sequence.max_value
                    ),
                ));
            }
            if let Some(set_role) = role_sql {
                conn.batch(set_role).await?;
            }
            let row = conn
                .query_one(
                    "SELECT setval(($1::bigint)::oid::regclass, $2::bigint, true) AS synchronized_to",
                    &[sequence.oid.into(), extreme.into()],
                )
                .await
                .map_err(|source| ApplyError::MigrationFailed {
                    version: step.migration.version.as_str().to_string(),
                    source: source.into(),
                })?;
            let synchronized_to: i64 = row.try_get("synchronized_to")?;
            if synchronized_to != extreme {
                return Err(sync_error(
                    step,
                    format!("PostgreSQL setval returned {synchronized_to}, expected {extreme}"),
                ));
            }
            if role_sql.is_some() {
                conn.batch("RESET ROLE").await?;
            }
        }
    }

    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let inserted = conn
        .exec(
            &format!(
                "INSERT INTO {meta_q}.schema_migrations
                     (event_kind, version, name, checksum, \"by\", exec_ms, phase, outcome, kind)
                 VALUES ('{applied}', $1, $2, $3, $4, $5, 'completed', 'success', 'apply')",
                applied = journal::EventKind::Applied.as_str()
            ),
            &[
                step.migration.version.as_str().into(),
                (&step.migration.name).into(),
                step.migration.checksum.as_str().into(),
                applied_by.into(),
                exec_ms.into(),
            ],
        )
        .await
        .map_err(|error| ApplyError::Journal(JournalError::Db(error.into())))?;
    if inserted != 1 {
        return Err(ApplyError::Journal(JournalError::Backend(format!(
            "PostgreSQL identity-synchronization journal insert affected {inserted} rows, expected 1"
        ))));
    }
    Ok(())
}

async fn read_owned_sequence<D: SqlSession>(
    conn: &D,
    step: &SynchronizeIdentityStep,
) -> Result<OwnedSequence, ApplyError> {
    // pg_get_serial_sequence is intentionally used for both serial and identity:
    // PostgreSQL documents it as the owned-sequence lookup for both forms. The
    // first argument is parsed as a qualified SQL name, so format('%I.%I', ...)
    // preserves exact case and prevents search_path/name-parsing ambiguity.
    let rows = conn
        .query(
            "SELECT seq.oid::bigint AS sequence_oid,
                    seq_ns.nspname::text AS sequence_schema,
                    seq.relname::text AS sequence_name,
                    params.seqincrement AS increment_by,
                    params.seqmin AS min_value,
                    params.seqmax AS max_value,
                    params.seqcycle AS cycle,
                    (attr.atttypid IN ('int2'::regtype, 'int4'::regtype, 'int8'::regtype))
                      AS column_is_integer
             FROM pg_catalog.pg_class tbl
             JOIN pg_catalog.pg_namespace tbl_ns ON tbl_ns.oid = tbl.relnamespace
             JOIN pg_catalog.pg_attribute attr
               ON attr.attrelid = tbl.oid
              AND attr.attname = $3::text
              AND attr.attnum > 0
              AND NOT attr.attisdropped
             JOIN pg_catalog.pg_class seq
               ON seq.oid = pg_catalog.pg_get_serial_sequence(
                    pg_catalog.format('%I.%I', $1::text, $2::text), $3::text
                  )::regclass
              AND seq.relkind = 'S'
             JOIN pg_catalog.pg_namespace seq_ns ON seq_ns.oid = seq.relnamespace
             JOIN pg_catalog.pg_sequence params ON params.seqrelid = seq.oid
             WHERE tbl_ns.nspname = $1::text
               AND tbl.relname = $2::text
               AND tbl.relkind IN ('r', 'p')",
            &[
                step.schema.as_str().into(),
                step.table.as_str().into(),
                step.column.as_str().into(),
            ],
        )
        .await?;

    let Some(row) = rows.first() else {
        return Err(sync_error(
            step,
            "column has no owned PostgreSQL identity/serial sequence",
        ));
    };
    if rows.len() != 1 {
        return Err(sync_error(
            step,
            format!(
                "owned-sequence lookup returned {} rows, expected exactly one",
                rows.len()
            ),
        ));
    }
    Ok(OwnedSequence {
        oid: row.try_get("sequence_oid")?,
        schema: row.try_get("sequence_schema")?,
        name: row.try_get("sequence_name")?,
        increment: row.try_get("increment_by")?,
        min_value: row.try_get("min_value")?,
        max_value: row.try_get("max_value")?,
        cycle: row.try_get("cycle")?,
        column_is_integer: row.try_get("column_is_integer")?,
    })
}

async fn read_data_extreme<D: SqlSession>(
    conn: &D,
    table_q: &str,
    column_q: &str,
    increment: i64,
) -> Result<Option<i64>, ApplyError> {
    let aggregate = if increment > 0 { "MAX" } else { "MIN" };
    let row = conn
        .query_one(
            &format!("SELECT {aggregate}({column_q})::bigint AS data_extreme FROM {table_q}"),
            &[],
        )
        .await?;
    Ok(row.try_get("data_extreme")?)
}

async fn read_sequence_state<D: SqlSession>(
    conn: &D,
    sequence: &OwnedSequence,
) -> Result<SequenceState, ApplyError> {
    let sequence_q = format!(
        "{}.{}",
        quote_ident_checked(&sequence.schema)?,
        quote_ident_checked(&sequence.name)?
    );
    let row = conn
        .query_one(
            &format!("SELECT last_value::bigint AS last_value, is_called FROM {sequence_q}"),
            &[],
        )
        .await?;
    Ok(SequenceState {
        last_value: row.try_get("last_value")?,
        is_called: row.try_get("is_called")?,
    })
}

/// Whether the generator must be moved to the imported directional extreme.
///
/// `is_called=false` means the next `nextval` returns `last_value` itself;
/// `is_called=true` means the next candidate is `last_value + increment`.
/// Compare that actual candidate, not merely `last_value`, so a non-unit
/// increment that already jumps beyond the data remains an exact no-op.
fn generator_is_behind(increment: i64, state: SequenceState, extreme: i64) -> bool {
    let next = if state.is_called {
        i128::from(state.last_value) + i128::from(increment)
    } else {
        i128::from(state.last_value)
    };
    if increment > 0 {
        next <= i128::from(extreme)
    } else {
        next >= i128::from(extreme)
    }
}

fn sync_error(step: &SynchronizeIdentityStep, detail: impl std::fmt::Display) -> ApplyError {
    ApplyError::Backend(format!(
        "synchronizeIdentity PostgreSQL precondition failed for {}.{}.{}: {detail}",
        step.schema, step.table, step.column
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    use crate::driver::{Bind, DbError, Row, Value};
    use crate::model::migration::{
        Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId,
    };

    struct RecordingSession {
        log: RefCell<Vec<String>>,
        extreme: Option<i64>,
        state: SequenceState,
        increment: i64,
        min_value: i64,
        max_value: i64,
        owned: bool,
    }

    impl RecordingSession {
        fn new(extreme: Option<i64>, state: SequenceState, increment: i64) -> Self {
            Self {
                log: RefCell::new(Vec::new()),
                extreme,
                state,
                increment,
                min_value: i64::MIN + 1,
                max_value: i64::MAX,
                owned: true,
            }
        }

        fn owned_row(&self) -> Row {
            Row::new(
                vec![
                    "sequence_oid".into(),
                    "sequence_schema".into(),
                    "sequence_name".into(),
                    "increment_by".into(),
                    "min_value".into(),
                    "max_value".into(),
                    "cycle".into(),
                    "column_is_integer".into(),
                ],
                vec![
                    Value::Int(42),
                    Value::Text("app".into()),
                    Value::Text("items_id_seq".into()),
                    Value::Int(self.increment),
                    Value::Int(self.min_value),
                    Value::Int(self.max_value),
                    Value::Bool(false),
                    Value::Bool(true),
                ],
            )
        }
    }

    impl SqlSession for RecordingSession {
        async fn batch(&self, sql: &str) -> Result<(), DbError> {
            self.log.borrow_mut().push(format!("batch: {sql}"));
            Ok(())
        }

        async fn exec(&self, sql: &str, _binds: &[Bind]) -> Result<u64, DbError> {
            self.log.borrow_mut().push(format!("exec: {sql}"));
            Ok(1)
        }

        async fn exec_text(&self, sql: &str, _params: &[Option<String>]) -> Result<u64, DbError> {
            self.log.borrow_mut().push(format!("exec_text: {sql}"));
            Ok(1)
        }

        async fn query(&self, sql: &str, _binds: &[Bind]) -> Result<Vec<Row>, DbError> {
            self.log.borrow_mut().push(format!("query: {sql}"));
            if sql.contains("union_all") && sql.contains("schema_migrations_inflight") {
                Ok(Vec::new())
            } else if sql.contains("pg_get_serial_sequence") {
                Ok(if self.owned {
                    vec![self.owned_row()]
                } else {
                    Vec::new()
                })
            } else {
                Err(DbError::message(format!("unexpected query: {sql}")))
            }
        }

        async fn query_one(&self, sql: &str, binds: &[Bind]) -> Result<Row, DbError> {
            self.log.borrow_mut().push(format!("query_one: {sql}"));
            if sql.contains("AS data_extreme") {
                Ok(Row::new(
                    vec!["data_extreme".into()],
                    vec![self.extreme.map_or(Value::Null, Value::Int)],
                ))
            } else if sql.contains("is_called FROM") {
                Ok(Row::new(
                    vec!["last_value".into(), "is_called".into()],
                    vec![
                        Value::Int(self.state.last_value),
                        Value::Bool(self.state.is_called),
                    ],
                ))
            } else if sql.contains("SELECT setval") {
                let target = match binds.get(1) {
                    Some(Bind::Int(value)) => *value,
                    other => {
                        return Err(DbError::message(format!(
                            "unexpected setval target bind: {other:?}"
                        )))
                    }
                };
                Ok(Row::new(
                    vec!["synchronized_to".into()],
                    vec![Value::Int(target)],
                ))
            } else {
                Err(DbError::message(format!("unexpected query_one: {sql}")))
            }
        }
    }

    fn cfg() -> ExecutorConfig {
        let mut cfg = ExecutorConfig::new(
            "identity_project",
            "app",
            crate::test_fixtures::no_inject("app"),
        );
        cfg.pg.meta_schema = "meta".into();
        cfg
    }

    fn step() -> SynchronizeIdentityStep {
        let flags = MigrationFlags::default();
        let up = "-- structured identity synchronization";
        let checksum = Checksum::of(&ChecksumInput {
            up,
            down: None,
            flags: &flags,
            owner_app: "app",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        SynchronizeIdentityStep {
            migration: Migration {
                version: MigrationId::generate(),
                name: "synchronize items identity".into(),
                checksum,
                up: up.into(),
                down: None,
                flags,
                owner_app: "app".into(),
                depends_on: Vec::new(),
                supersedes: Vec::new(),
                preconditions: Vec::new(),
                existence_guard: None,
            },
            schema: "app".into(),
            table: "items".into(),
            column: "id".into(),
            writes_quiesced: "items_import_window".into(),
        }
    }

    #[test]
    fn directional_comparison_uses_the_actual_next_candidate() {
        assert!(generator_is_behind(
            5,
            SequenceState {
                last_value: 1,
                is_called: false,
            },
            21
        ));
        assert!(!generator_is_behind(
            5,
            SequenceState {
                last_value: 21,
                is_called: true,
            },
            21
        ));
        // Non-unit increment already leaps past the imported maximum: no setval.
        assert!(!generator_is_behind(
            5,
            SequenceState {
                last_value: 20,
                is_called: true,
            },
            22
        ));

        assert!(generator_is_behind(
            -5,
            SequenceState {
                last_value: -1,
                is_called: false,
            },
            -21
        ));
        assert!(!generator_is_behind(
            -5,
            SequenceState {
                last_value: -21,
                is_called: true,
            },
            -21
        ));
    }

    #[compio::test]
    async fn recording_apply_advances_a_behind_sequence_and_journals() {
        let session = RecordingSession::new(
            Some(21),
            SequenceState {
                last_value: 1,
                is_called: false,
            },
            5,
        );
        assert!(apply(&session, &cfg(), &step(), "tester")
            .await
            .expect("synchronize behind sequence"));
        let log = session.log.borrow().join("\n");
        assert!(log.contains("LOCK TABLE \"app\".\"items\" IN SHARE ROW EXCLUSIVE MODE"));
        assert!(log.contains("SELECT MAX(\"id\")::bigint AS data_extreme"));
        assert!(log.contains("SELECT setval(($1::bigint)::oid::regclass, $2::bigint, true)"));
        assert!(log.contains("INSERT INTO \"meta\".schema_migrations"));
        assert!(log.contains("batch: COMMIT"));
    }

    #[compio::test]
    async fn recording_apply_never_calls_setval_when_generator_is_ahead() {
        let session = RecordingSession::new(
            Some(21),
            SequenceState {
                last_value: 101,
                is_called: true,
            },
            5,
        );
        assert!(apply(&session, &cfg(), &step(), "tester")
            .await
            .expect("journal monotonic no-op"));
        let log = session.log.borrow().join("\n");
        assert!(!log.contains("SELECT setval"));
        assert!(log.contains("INSERT INTO \"meta\".schema_migrations"));
    }

    #[compio::test]
    async fn recording_apply_allows_out_of_bounds_extreme_when_generator_is_ahead() {
        let mut ascending = RecordingSession::new(
            Some(50),
            SequenceState {
                last_value: 100,
                is_called: false,
            },
            5,
        );
        ascending.min_value = 100;
        assert!(apply(&ascending, &cfg(), &step(), "tester")
            .await
            .expect("ascending generator is already beyond the imported maximum"));
        assert!(!ascending.log.borrow().join("\n").contains("SELECT setval"));

        let mut descending = RecordingSession::new(
            Some(-50),
            SequenceState {
                last_value: -100,
                is_called: false,
            },
            -5,
        );
        descending.max_value = -100;
        assert!(apply(&descending, &cfg(), &step(), "tester")
            .await
            .expect("descending generator is already beyond the imported minimum"));
        assert!(!descending.log.borrow().join("\n").contains("SELECT setval"));
    }

    #[compio::test]
    async fn recording_apply_rejects_a_column_without_an_owned_sequence() {
        let mut session = RecordingSession::new(
            Some(21),
            SequenceState {
                last_value: 1,
                is_called: false,
            },
            1,
        );
        session.owned = false;
        let error = apply(&session, &cfg(), &step(), "tester")
            .await
            .expect_err("unowned column must fail");
        assert!(error
            .to_string()
            .contains("no owned PostgreSQL identity/serial sequence"));
        assert!(session
            .log
            .borrow()
            .iter()
            .any(|line| line == "batch: ROLLBACK"));
    }
}
