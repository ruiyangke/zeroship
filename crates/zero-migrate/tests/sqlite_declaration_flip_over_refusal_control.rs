//! THE OVER-REFUSAL CONTROL for correcting SQLite cells in `dialect-support.toml`.
//!
//! Correcting a declaration is not a documentation edit. `Op::support()` READS
//! the generated dialect table, and `IrAuthor::load_and_lower_guarded` validates
//! before it lowers, so flipping a cell to `unsupported` makes the AUTHORING GATE
//! refuse the op on the table's own say-so - before the lowerer is ever asked.
//! If the op actually works on SQLite through some path, that correction silently
//! breaks a migration that runs today.
//!
//! That risk is not hypothetical here. `dropConstraint` on SQLite has a REAL
//! rebuild lane in `render/lower.rs`: when the live snapshot carries the table
//! and the named constraint is a FOREIGN KEY, it lowers to a 12-step rebuild and
//! applies. It refuses `SqliteRebuildOnly` only for a MISSING snapshot or a
//! NON-FK constraint. The live conformance suite's representative for
//! `dropConstraint/base` drops a constraint that is not a foreign key, so it
//! measured the refusal and recorded the row as a wrong declaration. The refusal
//! is real; the conclusion "SQLite cannot drop a constraint" is not.
//!
//! WHY THE EXISTING SUITE WOULD NOT HAVE CAUGHT IT. `sqlite_rebuild_apply.rs`
//! exercises this same FK drop, but through `IrAuthor::lower_steps`, which does
//! NOT validate. An over-refusal introduced at the AUTHORING gate is invisible to
//! it - the lowerer it calls would still happily build the rebuild. This file
//! drives `load_and_lower_guarded` instead, so the validate gate is inside the
//! measurement, which is the only place the flip would bite.
//!
//! So this file pins what SQLite ACTUALLY DOES, end to end, for the ops whose
//! declarations were under review. It must pass BEFORE and AFTER any sidecar
//! correction. If a future flip of `dropConstraint/base` to `unsupported` is
//! proposed, this test is what says no.

mod support;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde_json::json;
use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::model::ir::CURRENT_IR_VERSION;

use zero_migrate::schema::query::SqlDialect;
use zero_migrate::{
    CollectionDescriptor, DeclarativeAuthor, FieldDescriptor, GuardConfig, IrAuthor, LiveSchema,
    MigrationIr, PlanStep, RenameStep, SchemaSnapshot, SqliteBackend,
};

const PROJECT: &str = "prj_control";
const APP: &str = "app_control";

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths(app_id: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{app_id}.sqlite"));
    let journal = dir.path().join(format!("zs-{app_id}.migrations.sqlite"));
    Paths {
        _dir: dir,
        app,
        journal,
    }
}

fn effective_policy() -> zero_migrate::EffectivePolicy {
    support::confined_charter()
}

fn registry() -> BTreeMap<String, String> {
    ["users", "posts"]
        .iter()
        .map(|t| ((*t).to_string(), APP.to_string()))
        .collect()
}

/// First deploy of `descs` through the real declarative author + real backend.
async fn apply_first_deploy(be: &SqliteBackend, descs: &[CollectionDescriptor]) {
    let desired = zero_migrate::desired_snapshot_for_dialect(
        PROJECT,
        descs,
        SqlDialect::Sqlite,
        &effective_policy(),
    )
    .expect("desired snapshot");
    let plan = DeclarativeAuthor::new_for_dialect(PROJECT, APP, SqlDialect::Sqlite)
        .diff(
            &desired,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
            &effective_policy(),
        )
        .expect("first-deploy diff");
    for m in &plan.all_migrations() {
        be.apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("first-deploy apply {} must succeed: {e:?}", m.name));
    }
}

/// `users` + `posts`, where `posts.author` is a REF to `users` (a real FK).
fn v1_with_fk() -> Vec<CollectionDescriptor> {
    vec![
        CollectionDescriptor {
            name: "users".into(),
            owner_app: APP.into(),
            fields: vec![FieldDescriptor {
                name: "handle".into(),
                ty: "string".into(),
                required: true,
                ..Default::default()
            }],
            indexes: vec![],
            runtime_options: Default::default(),
        },
        CollectionDescriptor {
            name: "posts".into(),
            owner_app: APP.into(),
            fields: vec![FieldDescriptor {
                name: "author".into(),
                ty: "ref".into(),
                references: Some("users".into()),
                ..Default::default()
            }],
            indexes: vec![],
            runtime_options: Default::default(),
        },
    ]
}

/// The name SQLite's catalog snapshot gives the `posts.author` foreign key.
fn live_fk_name(live: &LiveSchema) -> String {
    let snap = live
        .table_snapshots
        .get("posts")
        .expect("posts is in the live snapshot");
    snap.constraints
        .iter()
        .find(|c| c.kind == "FOREIGN KEY")
        .unwrap_or_else(|| {
            panic!(
                "posts must carry a live FOREIGN KEY constraint; got {:?}",
                snap.constraints
            )
        })
        .name
        .clone()
}

fn drop_constraint_ir(name: &str) -> MigrationIr {
    serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "drop_posts_author_fk",
        "owner_app": APP,
        "ops": [{ "op": "dropConstraint", "table": "posts", "name": name }]
    }))
    .expect("dropConstraint fixture deserializes")
}

/// Drive an IR envelope through the PRODUCTION authoring path: validate (which
/// reads the dialect table) and then lower. This is the seam a sidecar flip moves.
fn load_and_lower(ir: &MigrationIr, live: &LiveSchema) -> Vec<PlanStep> {
    let policy = effective_policy();
    let source = serde_json::to_string(ir).expect("serialize IR envelope");
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &policy);
    let guard = GuardConfig::from_policy(effective_policy(), SqlDialect::Sqlite);
    author
        .load_and_lower_guarded(&source, APP, &registry(), live, &guard)
        .expect(
            "dropConstraint of a FOREIGN KEY must clear BOTH the authoring gate and the \
             lowerer on SQLite. If this failed with an UNSUPPORTED authoring error, a \
             dialect-support.toml cell was flipped to `unsupported` for an op SQLite \
             genuinely performs - that is an OVER-REFUSAL, not a correction.",
        )
        .plan
        .steps
}

/// THE CONTROL. A foreign-key drop on SQLite works today through the full
/// production path; it must still work after any declaration is corrected.
#[compio::test]
async fn dropping_a_foreign_key_on_sqlite_still_clears_the_authoring_gate() {
    let p = paths("over_refusal_drop_fk");
    let be = SqliteBackend::open(&p.app, &p.journal).expect("open sqlite backend");
    apply_first_deploy(&be, &v1_with_fk()).await;

    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("engine mode");
    let fk_before = be
        .actor()
        .query("PRAGMA main.foreign_key_list(posts)")
        .await
        .expect("read FK list");
    assert_eq!(
        fk_before.len(),
        1,
        "the fixture must START with a real foreign key, or this control proves nothing: \
         {fk_before:?}"
    );

    let live = LiveSchema::from_catalog_snapshot(
        be.snapshot_schema_sqlite().await.expect("snapshot"),
        APP,
    );
    let fk_name = live_fk_name(&live);

    // The whole point: validate + lower, not lower alone.
    let steps = load_and_lower(&drop_constraint_ir(&fk_name), &live);
    assert_eq!(steps.len(), 1, "one constraint drop is one atomic rebuild");
    let PlanStep::OnlineRename(RenameStep::SqliteRebuild(rebuild)) = &steps[0] else {
        panic!("dropConstraint on SQLite must lower to a 12-step rebuild, got {steps:?}");
    };
    assert_eq!(rebuild.spec.table, "posts");

    // And it really applies: the FK is gone afterwards.
    be.rebuild_one(&rebuild.spec, &rebuild.migration, "deployer")
        .await
        .expect("the drop-FK rebuild applies");
    let fk_after = be
        .actor()
        .query("PRAGMA main.foreign_key_list(posts)")
        .await
        .expect("read FK list after");
    assert!(
        fk_after.is_empty(),
        "the foreign key must be GONE after the rebuild: {fk_after:?}"
    );
}

/// The sibling control for `addConstraint`, whose `unique` variant WAS corrected.
/// The correction is per-VARIANT: the foreign-key variants apply on SQLite and
/// must keep applying, so a flip that took the whole op with it is caught here.
#[compio::test]
async fn adding_a_foreign_key_on_sqlite_still_clears_the_authoring_gate() {
    let p = paths("over_refusal_add_fk");
    let be = SqliteBackend::open(&p.app, &p.journal).expect("open sqlite backend");

    // Deploy `posts.author` as a PLAIN column, then add the FK imperatively.
    let mut v1 = v1_with_fk();
    v1[1].fields[0] = FieldDescriptor {
        name: "author".into(),
        ty: "string".into(),
        required: false,
        ..Default::default()
    };
    apply_first_deploy(&be, &v1).await;

    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("engine mode");
    let live = LiveSchema::from_catalog_snapshot(
        be.snapshot_schema_sqlite().await.expect("snapshot"),
        APP,
    );

    let ir: MigrationIr = serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "add_posts_author_fk",
        "owner_app": APP,
        "ops": [{
            "op": "addConstraint",
            "table": "posts",
            "constraint": {
                "name": "posts_author_fk",
                "kind": {
                    "kind": "fk",
                    "columns": ["author"],
                    "referencesTable": "users",
                    "referencesColumns": ["id"]
                }
            }
        }]
    }))
    .expect("addConstraint fixture deserializes");

    let steps = load_and_lower(&ir, &live);
    assert_eq!(steps.len(), 1, "one FK add is one atomic rebuild");
    assert!(
        matches!(
            &steps[0],
            PlanStep::OnlineRename(RenameStep::SqliteRebuild(_))
        ),
        "addConstraint(fk) on SQLite must lower to a 12-step rebuild, got {steps:?}"
    );
}
