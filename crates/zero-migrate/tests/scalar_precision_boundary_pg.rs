//! An integer JSON cannot carry exactly is REFUSED, not silently rounded.
//!
//! `IrScalar::Int` is documented as an exact 64-bit integer with `|v| < 2^53`
//! enforced at deserialize, and `IrScalar::Int64` carries anything larger as a
//! canonical decimal STRING (`{"int64":"…"}`). That boundary is where a migration
//! engine quietly corrupts data: `9007199254740993` in a JSON number field
//! becomes `9007199254740992` in every IEEE-754 double, and a producer or
//! consumer written in JavaScript will do exactly that without complaining.
//!
//! This pins all three points END TO END — through the load gate, the lowerer,
//! and a real PostgreSQL `bigint` column, read back as text so the comparison
//! cannot be laundered through a float on the way out:
//!
//!     2^53 - 1  as a bare JSON number   accepted, exact
//!     2^53 + 1  as a bare JSON number   REFUSED at load
//!     2^53 + 1  as {"int64":"…"}        accepted, exact
//!
//! THE MIDDLE CASE IS THE POINT. Accepting it and storing 9007199254740992 would
//! look like success at every layer: the insert applies, the row exists, the
//! column is a bigint, and the value is off by one forever. Refusing is the only
//! behaviour that cannot silently lose the number.
//!
//! GATE: `ZERO_MIGRATE_TEST_PG_URL`.

#[macro_use]
mod support;

use std::collections::BTreeMap;

use support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::apply::executor::LockMode;
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    Approval, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, MigrationEngine, MigrationIr,
    PostgresBackend, SqlDialect,
};

const OWNER: &str = "app_scalar_precision";

fn token(tag: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "scalar_{tag}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

/// Insert `literal` into a `bigInt` column and read it back as TEXT.
///
/// `Ok(text)` when the whole path accepted it, `Err(reason)` when the load gate
/// refused. Reading as text keeps the assertion away from any float.
async fn round_trip(
    session: &PgDevSession,
    tag: &str,
    column_type: &str,
    literal: &str,
) -> Result<String, String> {
    let schema = token(tag);
    let cfg = ExecutorConfig::new(
        format!("project_{schema}"),
        &schema,
        support::no_inject(&schema),
    );
    let _guard = support::SchemaGuard::arm(
        session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!("CREATE SCHEMA \"{}\"", cfg.project_schema))
        .await
        .expect("create the isolated test schema");

    let backend = PostgresBackend::new_generic(session);
    backend
        .ensure_journal(&cfg)
        .await
        .expect("ensure the migration journal");
    let author = IrAuthor::new(
        &cfg.project_schema,
        OWNER,
        SqlDialect::Postgres,
        &support::confined_charter(),
    );
    let guard_cfg = GuardConfig::from_policy(
        support::no_inject(&cfg.project_schema),
        SqlDialect::Postgres,
    );
    let registry: BTreeMap<String, String> =
        [("t".to_string(), OWNER.to_string())].into_iter().collect();

    let ddl = format!(
        r#"{{"ir_version":1,"name":"schema","ops":[{{"op":"createTable","name":"t","columns":[{{"name":"c0","type":"bigInt","nullable":false}},{{"name":"v","type":"{column_type}","nullable":true}}],"primaryKey":["c0"]}}]}}"#
    );
    let schema_artifact = author
        .load_and_lower_guarded(&ddl, OWNER, &registry, &LiveSchema::default(), &guard_cfg)
        .expect("the schema envelope lowers");
    MigrationEngine::new()
        .apply_plan(
            &schema_artifact.plan.steps,
            Approval::Approved,
            &backend,
            &cfg,
            "schema",
            LockMode::Acquire,
        )
        .await
        .expect("the schema envelope applies");

    // The data envelope lowers against the contracts the schema envelope declared.
    let mut live = LiveSchema::default();
    live.tables.insert("t".into());
    let declared: MigrationIr = serde_json::from_str(&ddl).expect("the schema envelope parses");
    live.advance_logical_columns(&declared, SqlDialect::Postgres, &cfg.project_schema, None)
        .expect("seed the declared logical column contracts");

    let dml = format!(
        r#"{{"ir_version":1,"name":"data","irreversible":"inserts one probe row","ops":[{{"op":"insert","table":"t","columns":["c0","v"],"rows":[[1,{literal}]]}}]}}"#
    );
    let data_artifact = author
        .load_and_lower_guarded(&dml, OWNER, &registry, &live, &guard_cfg)
        .map_err(|e| format!("{e:?}"))?;
    MigrationEngine::new()
        .apply_plan(
            &data_artifact.plan.steps,
            Approval::Approved,
            &backend,
            &cfg,
            "data",
            LockMode::Acquire,
        )
        .await
        // A refusal can arrive from the load gate (the scalar domain) OR from the
        // server (a value the column cannot hold). Both are "not stored", and a
        // caller distinguishing them reads the error text; panicking here would
        // make the second kind unobservable.
        .map_err(|e| format!("{e:?}"))?;

    let rows = session
        .query(
            &format!(
                "SELECT v::text AS out FROM \"{}\".\"t\" WHERE c0 = 1",
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("read the stored value back");
    Ok(rows
        .first()
        .and_then(|row| row.try_get::<_, String>("out").ok())
        .unwrap_or_else(|| "<no row>".to_string()))
}

#[compio::test]
async fn an_integer_within_the_exact_json_range_round_trips() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);

    // 2^53 - 1: the largest integer every IEEE-754 double carries exactly.
    let stored = round_trip(&session, "safe", "bigInt", "9007199254740991")
        .await
        .expect("a bare integer inside the exact range must be accepted");
    assert_eq!(
        stored, "9007199254740991",
        "an integer inside the exact JSON range must survive the round trip unchanged"
    );
}

#[compio::test]
async fn an_integer_beyond_the_exact_json_range_is_refused_as_a_bare_number() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);

    // 2^53 + 1. As an IEEE-754 double this IS 9007199254740992, so accepting it
    // would store a different number than the author wrote, with every layer
    // reporting success.
    let outcome = round_trip(&session, "unsafe", "bigInt", "9007199254740993").await;
    let Err(refusal) = outcome else {
        panic!(
            "a bare JSON number beyond 2^53 was ACCEPTED and stored as {:?}. Any \
             JavaScript producer or consumer on this path rounds it to \
             9007199254740992, so accepting it silently changes the value",
            outcome.unwrap()
        )
    };
    assert!(
        refusal.contains("Deserialize"),
        "the refusal must come from the scalar domain at deserialize, not from a \
         later accident: {refusal}"
    );
}

#[compio::test]
async fn the_same_integer_round_trips_exactly_in_its_string_form() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);

    // The escape hatch the refusal above points at. Same number, carried as a
    // canonical decimal string, and it must arrive intact rather than merely be
    // accepted — an `int64` that parsed through a float would land on ...992.
    let stored = round_trip(
        &session,
        "int64",
        "bigInt",
        r#"{"int64":"9007199254740993"}"#,
    )
    .await
    .expect("the int64 form must be accepted");
    assert_eq!(
        stored, "9007199254740993",
        "the int64 form exists to carry exactly the values a JSON number cannot, so \
         landing on 9007199254740992 here would defeat its whole purpose"
    );
}

#[compio::test]
async fn a_timestamp_keeps_its_microseconds_and_its_zone() {
    // Sub-second precision is the other place a migration engine loses data
    // quietly: a path that formats through seconds, or drops the offset, still
    // produces a valid-looking timestamp that is simply wrong.
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);

    let stored = round_trip(
        &session,
        "ts",
        "timestamp",
        r#""2026-02-03T04:05:06.123456Z""#,
    )
    .await
    .expect("a UTC timestamp with microseconds must be accepted");
    assert_eq!(
        stored, "2026-02-03 04:05:06.123456+00",
        "all six fractional digits and the zone must survive. Truncating to seconds \
         or dropping the offset still yields a plausible timestamp, which is what \
         makes it worth asserting exactly"
    );
}

#[compio::test]
async fn a_uuid_round_trips_byte_identical() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);

    // A UUIDv7, so the timestamp-ordered prefix is preserved rather than
    // re-generated: a path that minted its own value would still return a
    // well-formed UUID, and only comparing to the authored one catches that.
    let stored = round_trip(
        &session,
        "uuid",
        "uuid",
        r#""0191d4f4-9c1a-7f2b-8c3d-4e5f60718293""#,
    )
    .await
    .expect("a canonical UUID must be accepted");
    assert_eq!(
        stored, "0191d4f4-9c1a-7f2b-8c3d-4e5f60718293",
        "the stored UUID must be the authored one, not a re-generated or re-cased value"
    );
}

#[compio::test]
async fn a_text_value_carrying_a_nul_byte_is_not_silently_truncated() {
    // PostgreSQL `text` cannot hold a NUL byte. The dangerous outcome is storing
    // the prefix before it and reporting success, which drops everything after
    // the NUL with nothing anywhere to notice. Failing is correct; arriving whole
    // would be acceptable too. Only truncation is a defect.
    //
    // The JSON escape is ASSEMBLED here rather than written literally: char 92 is
    // the backslash, so the fixture reaching the parser is the six-character
    // escape and this source file contains no control byte of its own.
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);

    let backslash = char::from(92);
    let literal = format!("\"a{backslash}u0000b\"");
    let outcome = round_trip(&session, "nul", "text", &literal).await;

    match outcome {
        // NOT `Err(_)`. Accepting any error would let a broken fixture — a schema
        // that failed to create, a journal that failed to open — pass as though
        // the NUL had been refused. The refusal has to be about the VALUE: either
        // the scalar domain rejects it at load, or the server rejects the byte
        // (SQLSTATE 22021, `invalid byte sequence for encoding "UTF8": 0x00`).
        Err(refused) => assert!(
            refused.contains("0x00") || refused.contains("Deserialize"),
            "the write failed for a reason unrelated to the NUL, so this test did \
             not measure truncation at all: {refused}"
        ),
        Ok(stored) => assert_eq!(
            stored.chars().count(),
            3,
            "the NUL-bearing value was stored as {stored:?}, which is the truncated \
             prefix. PostgreSQL text cannot hold a NUL, so the write must fail or \
             the value must arrive whole - storing the prefix loses data while \
             every layer reports success"
        ),
    }
}
