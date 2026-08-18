//! The MySQL bare-TEXT-key gate holds inside one envelope and NOT across two.
//!
//! MySQL error 1170 - "BLOB/TEXT column used in key specification without a key
//! length" - is unconditional, and `model/validate.rs` carries a dialect-scoped rule
//! that refuses a key over a column whose RENDERED MySQL storage is a bare TEXT.
//! `mysql_storage_shapes.rs` pins that rule and passes.
//!
//! It pins it in ONE shape, though: a `createTable` and a `createIndex` in the SAME
//! `MigrationIr`. `validate_ir` takes one envelope and no live schema, so when the
//! column is declared by an EARLIER deploy the rule has nothing to key on and the
//! index sails through the gate, through lowering, and into the server, which refuses
//! it. The engine then reports a failed apply for a migration a same-envelope author
//! would have been told about before anything ran.
//!
//! This is a REAL hole in a guard a green unit test reports as covered, and nothing
//! could see it: it is only observable against a live MySQL server, and no Rust test
//! had ever driven one. `fold_roundtrip_mysql.rs` walked into it on its first run.
//!
//! # What this file is, and when to delete it
//!
//! It pins the CURRENT boundary, both halves of it, so the hole has a name and a
//! measurement instead of a review-log paragraph. It is deliberately shaped to go RED
//! when the hole is closed: [`a_text_key_from_an_earlier_envelope_reaches_the_server`]
//! asserts that lowering SUCCEEDS and the SERVER refuses, so a gate taught to see the
//! live column would fail this test at its first assertion. That is the intended
//! signal. Whoever closes it should replace that test with one asserting the load-gate
//! refusal, and keep [`a_text_key_in_the_same_envelope_is_refused_at_the_gate`] as is.
//!
//! Closing it is not a small change, which is why it is recorded rather than fixed
//! here: `validate_ir`'s signature carries no column-level live schema, and
//! `LiveSchema` carries table NAMES only - `LiveSchema::from_tables` takes a
//! `BTreeSet<String>`. The gate cannot learn the rendered storage of `notes.body`
//! without a live-schema type that does not exist yet.

mod support;

use std::collections::BTreeMap;

use support::mysql::{quote_ident, DatabaseGuard, MysqlDevSession};
use zero_migrate::apply::backend::MysqlBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::model::validate::{validate_ir, Dialect};
use zero_migrate::{
    Approval, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine,
    MigrationIr, SqlDialect,
};

const OWNER: &str = "app_mysql_text_key";

/// A `createTable` declaring a bare `text` column, and a `createIndex` over it, in
/// ONE envelope.
const SAME_ENVELOPE: &str = r#"{"ir_version":1,"name":"text_key_same_envelope","ops":[
    {"op":"createTable","name":"notes","columns":[
        {"name":"id","type":"int","nullable":false},
        {"name":"body","type":"text","nullable":true}
    ],"primaryKey":["id"]},
    {"op":"createIndex","table":"notes","name":"notes_body_idx",
     "columns":[{"kind":"column","name":"body"}]}
]}"#;

/// The SAME two operations, split across two envelopes - which is what a second
/// deploy against an existing table looks like.
const FIRST_ENVELOPE: &str = r#"{"ir_version":1,"name":"create_notes","ops":[
    {"op":"createTable","name":"notes","columns":[
        {"name":"id","type":"int","nullable":false},
        {"name":"body","type":"text","nullable":true}
    ],"primaryKey":["id"]}
]}"#;

const SECOND_ENVELOPE: &str = r#"{"ir_version":1,"name":"index_notes_body","ops":[
    {"op":"createIndex","table":"notes","name":"notes_body_idx",
     "columns":[{"kind":"column","name":"body"}]}
]}"#;

#[test]
fn a_text_key_in_the_same_envelope_is_refused_at_the_gate() {
    // The control. Without it the test below reads as "MySQL refuses text keys",
    // which is not the finding - the finding is that the ENGINE refuses one of these
    // and not the other, from the same two operations.
    let ir: MigrationIr = serde_json::from_str(SAME_ENVELOPE).expect("same-envelope IR parses");
    let error = validate_ir(&ir, Dialect::Mysql)
        .expect_err("a key over a bare TEXT column must not pass the MySQL load gate");
    let rendered = format!("{error}");
    assert!(
        rendered.contains("body"),
        "the refusal must name the column: {rendered}"
    );

    // And the same envelope is fine on the other two dialects, so the rule is scoped
    // to the server that actually refuses it.
    validate_ir(&ir, Dialect::Postgres).expect("PostgreSQL indexes a text column");
    validate_ir(&ir, Dialect::Sqlite).expect("SQLite indexes a text column");
}

#[test]
fn the_second_envelope_alone_carries_nothing_the_gate_could_key_on() {
    // Why the split escapes: on its own, the index envelope names a column it does
    // not declare, and `validate_ir` is given no live schema to resolve it against.
    // This is the mechanism, provable with no database at all.
    let ir: MigrationIr = serde_json::from_str(SECOND_ENVELOPE).expect("index-only IR parses");
    validate_ir(&ir, Dialect::Mysql)
        .expect("the index-only envelope passes the MySQL gate - it declares no column");
}

#[compio::test]
async fn a_text_key_from_an_earlier_envelope_reaches_the_server() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("textkey");
    let cfg = ExecutorConfig::new(
        format!("project_{database}"),
        database.clone(),
        support::no_inject(&database),
    );
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);
    session
        .batch(&format!("CREATE DATABASE {}", quote_ident(&database)))
        .await
        .expect("create the isolated probe database");

    let result: Result<(), String> = async {
        apply(
            &session,
            &cfg,
            FIRST_ENVELOPE,
            &BTreeMap::new(),
            &LiveSchema::default(),
        )
        .await?;

        // The whole finding, in one call. The index envelope LOWERS - the gate the
        // control test above proved exists does not fire - so the only thing left to
        // refuse it is MySQL.
        let live = LiveSchema::from_tables(["notes".to_string()].into_iter().collect());
        let registry: BTreeMap<String, String> = [("notes".to_string(), OWNER.to_string())]
            .into_iter()
            .collect();
        let error = apply(&session, &cfg, SECOND_ENVELOPE, &registry, &live)
            .await
            .err()
            .ok_or_else(|| {
                "a bare-TEXT key applied cleanly on MySQL, which contradicts error 1170 \
                 and means this fixture no longer reproduces anything"
                    .to_string()
            })?;
        if !error.contains("apply IR plan") {
            return Err(format!(
                "the refusal must come from the SERVER during apply, not from the load \
                 gate or the lower - if it now comes from the gate, the hole is closed \
                 and this test should be replaced with one asserting that; got: {error}"
            ));
        }
        if !error.contains("key specification without a key length") {
            return Err(format!(
                "the server's refusal must be MySQL error 1170; got: {error}"
            ));
        }
        Ok(())
    }
    .await;
    result.unwrap_or_else(|error| panic!("{error}"));
}

/// Load, lower and apply one envelope, mapping every failure to a string that names
/// WHICH stage refused - the distinction this file's claim rests on.
async fn apply(
    session: &MysqlDevSession,
    cfg: &ExecutorConfig,
    source: &str,
    registry: &BTreeMap<String, String>,
    live: &LiveSchema,
) -> Result<(), String> {
    let policy = support::no_inject(&cfg.project_schema);
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Mysql, &policy);
    let guard = GuardConfig::from_policy(policy.clone(), SqlDialect::Mysql);
    let artifact = author
        .load_and_lower_guarded(source, OWNER, registry, live, &guard)
        .map_err(|error| format!("load and lower guarded IR plan: {error}"))?;

    let backend = MysqlBackend::new_generic(session);
    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            &backend,
            cfg,
            "mysql-text-key-gate",
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("apply IR plan: {error}"))?;
    Ok(())
}
