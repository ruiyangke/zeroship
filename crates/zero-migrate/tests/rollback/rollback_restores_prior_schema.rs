//! Rolling back an ENGINE-GENERATED migration restores the schema it changed.
//!
//! `sqlite_rollback.rs` covers the rollback MECHANISM — reverse order, locking,
//! re-pending, refusing a malicious `down` — using hand-built `mig(name, up,
//! down)` values whose `down` is a literal SQL string chosen by the test. Nothing
//! asked whether the `down` the ENGINE ITSELF renders for each op kind actually
//! puts the schema back.
//!
//! That is the same gap `plan_rollbackable.rs` has: the mechanism is tested with
//! synthetic input and the real op corpus is unexamined.
//!
//! The oracle is the whole of `sqlite_master` — type, name, AND the stored DDL
//! text — captured before the apply and compared after the rollback. Comparing
//! table NAMES alone would pass a rollback that left a column or an index behind.
//!
//! WHAT IS NOT COVERED, stated rather than implied. Of the ops that render a
//! `down`, this exercises `createTable`, `addColumn`, `createIndex` and
//! `renameTable`. `addConstraint`, `setColumnNotNull` and `dropColumnNotNull`
//! are absent because SQLite REFUSES them (`SqliteRebuildOnly`,
//! `NativeAlterColumn`), so reaching them needs a PostgreSQL arm and a live
//! database — worth adding, not done here.
//!
//! Ops without a `down` are deliberately absent for a different reason: the
//! `drop*` family renders `down: None` by design, because rollback for
//! declarative DDL is a re-diff toward the desired snapshot rather than a stored
//! reversing statement. Including them would manufacture a failure and then
//! "fix" it by inventing a `down` the architecture does not want.

use crate::support;
use std::collections::BTreeMap;
use zero_migrate::apply::executor::LockMode;
use zero_migrate::render::step::PlanStep;
use zero_migrate::{
    Approval, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, MigrationEngine, SqlDialect,
    SqliteBackend,
};
const PROJECT: &str = "prj_ir";
const APP: &str = "app_ir";

async fn schema_of(be: &SqliteBackend) -> Vec<Vec<Option<String>>> {
    be.actor()
        .query("SELECT type, name, COALESCE(sql,'') FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name")
        .await
        .unwrap_or_default()
}

#[compio::test]
async fn an_engine_rendered_down_restores_the_schema_its_up_changed() {
    let seed = r#"{"op":"createTable","name":"t1","columns":[{"name":"c0","type":"bigInt","nullable":false}],"primaryKey":["c0"]}"#;
    for (label, op, needs_seed) in [
        (
            "createTable",
            r#"{"op":"createTable","name":"t2","columns":[{"name":"c0","type":"bigInt","nullable":false}],"primaryKey":["c0"]}"#,
            false,
        ),
        // A rename's reverse is the one most likely to be subtly wrong: the down
        // has to rename BACK, and a down that re-runs the forward direction, or
        // renames to the wrong side, still leaves exactly one table standing.
        // Only the stored DDL text distinguishes the two.
        (
            "renameTable",
            r#"{"op":"renameTable","table":"t1","to":"t1_renamed"}"#,
            true,
        ),
        (
            "addColumn",
            r#"{"op":"addColumn","table":"t1","column":"n1","type":"text","nullable":true}"#,
            true,
        ),
        (
            "createIndex",
            r#"{"op":"createIndex","name":"ix1","table":"t1","columns":[{"kind":"column","name":"c0"}]}"#,
            true,
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let be = SqliteBackend::open(
            &dir.path().join("r.sqlite"),
            &dir.path().join("r.migrations.sqlite"),
        )
        .expect("open");
        let cfg = ExecutorConfig::new(PROJECT, PROJECT, support::no_inject(PROJECT));
        let eng = MigrationEngine::new();
        let mut live = LiveSchema::default();
        let reg: BTreeMap<String, String> = [
            ("t1".to_string(), APP.to_string()),
            ("t2".to_string(), APP.to_string()),
        ]
        .into_iter()
        .collect();
        let author = IrAuthor::new(
            PROJECT,
            APP,
            SqlDialect::Sqlite,
            &support::confined_charter(),
        );
        let gc = GuardConfig::from_policy(support::no_inject(PROJECT), SqlDialect::Sqlite);
        if needs_seed {
            let s = format!(r#"{{"ir_version":1,"name":"seed","ops":[{seed}]}}"#);
            let a = author
                .load_and_lower_guarded(&s, APP, &reg, &LiveSchema::default(), &gc)
                .expect("seed lowers");
            eng.apply_plan(
                &a.plan.steps,
                Approval::Approved,
                &be,
                &cfg,
                "seed",
                LockMode::Acquire,
            )
            .await
            .expect("seed applies");
            live.tables.insert("t1".into());
        }
        let before = schema_of(&be).await;
        let bytes = format!(r#"{{"ir_version":1,"name":"rb_{label}","ops":[{op}]}}"#);
        let art = author
            .load_and_lower_guarded(&bytes, APP, &reg, &live, &gc)
            .expect("lowers");
        eng.apply_plan(
            &art.plan.steps,
            Approval::Approved,
            &be,
            &cfg,
            "rb",
            LockMode::Acquire,
        )
        .await
        .expect("applies");
        let after_apply = schema_of(&be).await;
        // roll back every DDL step in reverse
        let mut errs = Vec::new();
        for step in art.plan.steps.iter().rev() {
            if let PlanStep::Ddl(m) = step {
                if let Err(e) = be.rollback_one_additive(m, "operator").await {
                    errs.push(format!("{e:?}").chars().take(50).collect::<String>());
                }
            }
        }
        let after_rb = schema_of(&be).await;
        assert!(
            errs.is_empty(),
            "{label}: rolling back the engine-rendered down failed: {errs:?}"
        );
        assert_ne!(
            before, after_apply,
            "{label}: the apply changed nothing, so the rollback proves nothing"
        );
        assert_eq!(
            before, after_rb,
            "{label}: the engine rendered a down that does NOT restore the schema. \
             Compared over sqlite_master type/name/sql, so a leftover column or index \
             shows up here even when the table list looks right"
        );
    }
}
