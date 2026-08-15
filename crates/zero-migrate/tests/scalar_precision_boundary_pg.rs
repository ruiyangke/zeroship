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
async fn round_trip(session: &PgDevSession, tag: &str, literal: &str) -> Result<String, String> {
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

    let ddl = r#"{"ir_version":1,"name":"schema","ops":[{"op":"createTable","name":"t","columns":[{"name":"c0","type":"bigInt","nullable":false},{"name":"v","type":"bigInt","nullable":true}],"primaryKey":["c0"]}]}"#;
    let schema_artifact = author
        .load_and_lower_guarded(ddl, OWNER, &registry, &LiveSchema::default(), &guard_cfg)
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
    let declared: MigrationIr = serde_json::from_str(ddl).expect("the schema envelope parses");
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
        .expect("the data envelope applies once it has been accepted");

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
    let stored = round_trip(&session, "safe", "9007199254740991")
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
    let outcome = round_trip(&session, "unsafe", "9007199254740993").await;
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
    let stored = round_trip(&session, "int64", r#"{"int64":"9007199254740993"}"#)
        .await
        .expect("the int64 form must be accepted");
    assert_eq!(
        stored, "9007199254740993",
        "the int64 form exists to carry exactly the values a JSON number cannot, so \
         landing on 9007199254740992 here would defeat its whole purpose"
    );
}
