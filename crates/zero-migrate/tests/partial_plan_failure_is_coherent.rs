//! When a plan fails halfway, the database and the journal must agree.
//!
//! A multi-step plan whose second step fails at EXECUTION leaves the first step
//! applied — per-migration atomicity, which is the normal contract. The property
//! that actually matters operationally is what the JOURNAL says afterwards: if it
//! omits the step that succeeded, a retry re-runs it and fails on an object that
//! already exists, and the deployment is wedged with no way forward that does not
//! involve editing the journal by hand.
//!
//! FORCING A REAL EXECUTION FAILURE. The failing table is created OUT OF BAND,
//! directly on the backend, so the engine lowers the plan happily — its
//! `LiveSchema` does not know the table exists — and the failure happens where it
//! matters, in the middle of applying. A plan rejected at lowering would be
//! testing the gate, not this.

mod support;

use std::collections::BTreeMap;

use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::apply::executor::LockMode;
use zero_migrate::{
    Approval, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, MigrationEngine, SqlDialect,
    SqliteBackend,
};

const PROJECT: &str = "prj_ir";
const APP: &str = "app_ir";

const TWO_STEP_PLAN: &str = r#"{"ir_version":1,"name":"partial","ops":[
  {"op":"createTable","name":"tA","columns":[{"name":"c0","type":"bigInt","nullable":false}],"primaryKey":["c0"]},
  {"op":"createTable","name":"tB","columns":[{"name":"c0","type":"bigInt","nullable":false}],"primaryKey":["c0"]}
]}"#;

#[compio::test]
async fn a_plan_that_fails_halfway_leaves_the_journal_agreeing_with_the_database() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = SqliteBackend::open(
        &dir.path().join("p.sqlite"),
        &dir.path().join("p.migrations.sqlite"),
    )
    .expect("open the hardened sqlite backend");
    let cfg = ExecutorConfig::new(PROJECT, PROJECT, support::no_inject(PROJECT));

    // OUT OF BAND, so the plan lowers and the failure lands mid-apply.
    backend
        .actor()
        .query("CREATE TABLE tB (x integer)")
        .await
        .expect("seed the conflicting table outside the engine");

    let registry: BTreeMap<String, String> = [
        ("tA".to_string(), APP.to_string()),
        ("tB".to_string(), APP.to_string()),
    ]
    .into_iter()
    .collect();
    let artifact = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    )
    .load_and_lower_guarded(
        TWO_STEP_PLAN,
        APP,
        &registry,
        &LiveSchema::default(),
        &GuardConfig::from_policy(support::no_inject(PROJECT), SqlDialect::Sqlite),
    )
    .expect("the two-step plan lowers");
    assert_eq!(
        artifact.plan.steps.len(),
        2,
        "the fixture needs two steps for one of them to fail halfway"
    );

    let applied = MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            &backend,
            &cfg,
            "partial",
            LockMode::Acquire,
        )
        .await;
    assert!(
        applied.is_err(),
        "the second step must FAIL, or this test is not measuring a partial failure"
    );

    let journal = backend.applied(&cfg).await.expect("read the journal");
    assert_eq!(
        journal.len(),
        1,
        "the journal must record EXACTLY the step that succeeded: one entry, not \
         zero — a retry would re-run it and fail on an existing table — and not two, \
         since the failed step must never be recorded as applied. Got: {journal:?}"
    );

    let tables = backend
        .actor()
        .query(
            "SELECT name FROM sqlite_master WHERE type='table' \
             AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .await
        .expect("read the live table list");
    assert!(
        tables
            .iter()
            .any(|row| row.first() == Some(&Some("tA".to_string()))),
        "the step that succeeded must still be present: {tables:?}"
    );

    // THE RESUME. Re-applying must not trip over the step already journaled: it
    // fails again on the same conflicting table, and the journal still holds one
    // entry rather than gaining a duplicate.
    let retried = MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            &backend,
            &cfg,
            "partial-retry",
            LockMode::Acquire,
        )
        .await;
    assert!(
        retried.is_err(),
        "the retry must still fail on the same conflict, since nothing resolved it"
    );

    let journal_after_retry = backend.applied(&cfg).await.expect("re-read the journal");
    assert_eq!(
        journal_after_retry.len(),
        1,
        "the retry must not duplicate the entry for the step that already applied, \
         and must not record the step that failed again: {journal_after_retry:?}"
    );
    assert_eq!(
        journal.first().map(|entry| entry.version.clone()),
        journal_after_retry
            .first()
            .map(|entry| entry.version.clone()),
        "the retry must not re-journal the completed step under a new version"
    );
}
