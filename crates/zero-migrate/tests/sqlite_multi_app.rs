//! Multi-app UNION + per-table ownership coverage on the `SQLite` leg (Tier-2).
//!
//! REACHABILITY (investigated, not assumed): the dev `SQLite` tier runs ONE app
//! file per app (`run_sqlite_via_engine`, `app_id` = the app), so a *single
//! deploy* never mixes two apps. BUT the ownership / conflict / union machinery
//! lives in `desired_snapshot` + `DeclarativeAuthor::diff`, which are
//! dialect-agnostic and DO run on the `SQLite` author: the author carries one
//! `owner_app`, and `diff` enforces it against the caller's `live_ownership` map
//! regardless of dialect. So these scenarios ARE reachable through the `SQLite`
//! author and are tested here:
//!   - identical re-declaration → idempotent union (owner = lexicographically
//!     smallest), and the merged table APPLIES on a real `SQLite` file + re-diffs
//!     ZERO-drift (faithful end-state, not just a pure-function check);
//!   - conflicting declaration → `ConflictingDeclaration` (fail-closed);
//!   - drop of a table owned by ANOTHER app → `NotTableOwner` (fail-closed);
//!   - drop of a live table whose owner is unknown → `DropOfUnownedTable`
//!     (fail-closed, the partial-union guard).
//!
//! N-A on the `SQLite` leg (NOT tested — documented, not written): cross-slice FK
//! `depends_on` topo across DISTINCT owners in ONE apply is not a single-deploy
//! `SQLite` shape (one app file per app; a cross-app FK target lives in another
//! app's file, which the engine does not ATTACH on the confined leg) — the
//! cross-app FK *target-missing* guard is already covered dialect-neutrally in
//! `declarative_sqlite::sqlite_deferred_fk_is_typed_error`.

use std::collections::HashMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::{
    desired_snapshot, CollectionDescriptor, DeclarativeAuthor, DeclarativeError, FieldDescriptor,
    SchemaSnapshot, SqliteBackend,
};
use zero_migrate::schema::query::SqlDialect;

const PROJECT: &str = "prj_demo";

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths(id: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{id}.sqlite"));
    let journal = dir.path().join(format!("zs-{id}.migrations.sqlite"));
    Paths {
        _dir: dir,
        app,
        journal,
    }
}

fn backend(p: &Paths) -> SqliteBackend {
    SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend")
}

/// A `SQLite` author deploying AS `owner_app`.
fn author_as(owner_app: &str) -> DeclarativeAuthor {
    DeclarativeAuthor::new_for_dialect(PROJECT, owner_app, SqlDialect::Sqlite)
}

fn coll(name: &str, owner: &str, fields: Vec<FieldDescriptor>) -> CollectionDescriptor {
    CollectionDescriptor {
        name: name.into(),
        owner_app: owner.into(),
        fields,
        indexes: vec![],
    runtime_options: Default::default(),
    }
}

fn field(name: &str, ty: &str, required: bool) -> FieldDescriptor {
    FieldDescriptor {
        name: name.into(),
        ty: ty.into(),
        required,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// IDENTICAL RE-DECLARATION → idempotent union. Two apps declare `shared`
// byte-identically; the union keeps ONE table owned by the lexicographically
// smallest declarer (`app_a`). It APPLIES on a real SQLite file and a re-diff is
// ZERO-drift (faithful end-state).
// ---------------------------------------------------------------------------
#[compio::test]
async fn identical_redeclaration_unions_idempotently_and_applies() {
    let from_a = coll("shared", "app_a", vec![field("label", "string", true)]);
    let from_b = coll("shared", "app_b", vec![field("label", "string", true)]);

    let desired = desired_snapshot(PROJECT, &[from_a, from_b])
        .expect("identical declarations union without conflict");
    // ONE merged table, owned by the smallest declarer.
    assert_eq!(desired.snapshot.tables.len(), 1, "the two identical decls merge to one table");
    assert_eq!(
        desired.owner_of("shared"),
        Some("app_a"),
        "the union owner is the lexicographically-smallest declarer"
    );

    // The merged table APPLIES through the real backend (the owner deploys it).
    let plan = author_as("app_a")
        .diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("diff the unioned table");
    let p = paths("redecl_apply");
    let be = backend(&p);
    for m in &plan.all_migrations() {
        be.apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("apply {} must succeed: {e:?}", m.name));
    }
    let rows = be
        .actor()
        .query("SELECT name FROM main.sqlite_master WHERE type='table' AND name='shared'")
        .await
        .expect("introspect");
    assert_eq!(rows.len(), 1, "the unioned `shared` table exists on the SQLite file");

    // A re-diff against the REAL live snapshot → ZERO drift (idempotent union).
    let live = be.snapshot_schema_sqlite().await.expect("introspect live");
    let own: HashMap<String, String> =
        desired.ownership.iter().map(|(t, a)| (t.clone(), a.clone())).collect();
    let again_desired = desired_snapshot(
        PROJECT,
        &[
            coll("shared", "app_a", vec![field("label", "string", true)]),
            coll("shared", "app_b", vec![field("label", "string", true)]),
        ],
    )
    .expect("re-union");
    let plan2 = author_as("app_a")
        .diff(&again_desired, &live, &own, &[])
        .expect("re-diff must succeed");
    assert!(
        plan2.all_migrations().is_empty() && plan2.rebuilds.is_empty(),
        "an identical re-declaration must re-diff ZERO-drift; got migs={:?} rebuilds={}",
        plan2.all_migrations().iter().map(|m| m.name.clone()).collect::<Vec<_>>(),
        plan2.rebuilds.len()
    );
}

// ---------------------------------------------------------------------------
// CONFLICTING DECLARATION → fail-closed. Two apps declare `shared` with DIFFERENT
// shapes; the union refuses with `ConflictingDeclaration` naming both apps.
// ---------------------------------------------------------------------------
#[compio::test]
async fn conflicting_declaration_is_rejected_fail_closed() {
    let from_a = coll("shared", "app_a", vec![field("label", "string", true)]);
    let from_b = coll("shared", "app_b", vec![field("count", "number", true)]);

    let err = desired_snapshot(PROJECT, &[from_a, from_b])
        .expect_err("conflicting shapes must be refused");
    match err {
        DeclarativeError::ConflictingDeclaration { table, apps } => {
            assert_eq!(table, "shared");
            assert_eq!(apps, vec!["app_a".to_string(), "app_b".to_string()],
                "the error names every conflicting declarer (sorted, deduped)");
        }
        other => panic!("expected ConflictingDeclaration, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// DROP OF AN UNOWNED TABLE (owner known, ≠ deploying app) → `NotTableOwner`. A
// live table owned by `app_other` is absent from `app_demo`'s desired set; the
// SQLite author (deploying as `app_demo`) must NOT author a drop of a table it
// does not own.
// ---------------------------------------------------------------------------
#[compio::test]
async fn drop_of_table_owned_by_another_app_is_refused() {
    // Live: a table owned by `app_other`.
    let live_desired = desired_snapshot(
        PROJECT,
        &[coll("theirs", "app_other", vec![field("x", "string", true)])],
    )
    .expect("live desired");
    let live_ownership: HashMap<String, String> =
        live_desired.ownership.iter().map(|(t, a)| (t.clone(), a.clone())).collect();
    assert_eq!(live_ownership.get("theirs"), Some(&"app_other".to_string()));

    // Desired: empty (the deploying app declares nothing) — but the differ must NOT
    // drop another app's live table.
    let desired_empty = desired_snapshot(PROJECT, &[]).expect("empty desired");
    let err = author_as("app_demo")
        .diff(&desired_empty, &live_desired.snapshot, &live_ownership, &[])
        .expect_err("a non-owner drop of a live table must be refused");
    match err {
        DeclarativeError::NotTableOwner { table, owner, deploying_app } => {
            assert_eq!(table, "theirs");
            assert_eq!(owner, "app_other");
            assert_eq!(deploying_app, "app_demo");
        }
        other => panic!("expected NotTableOwner, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// DROP OF A LIVE TABLE WHOSE OWNER IS UNKNOWN → `DropOfUnownedTable` (the
// partial-union fail-closed guard). The live table exists but the caller passed
// NO `live_ownership` entry for it, so the differ refuses to author a destructive
// drop it cannot attribute to the deploying app.
// ---------------------------------------------------------------------------
#[compio::test]
async fn drop_of_live_table_with_unknown_owner_is_refused() {
    let live_desired = desired_snapshot(
        PROJECT,
        &[coll("orphan", "app_demo", vec![field("x", "string", true)])],
    )
    .expect("live desired");

    let desired_empty = desired_snapshot(PROJECT, &[]).expect("empty desired");
    // EMPTY ownership map — the differ cannot confirm the deploying app owns
    // `orphan`, so it fails closed rather than mass-dropping.
    let err = author_as("app_demo")
        .diff(&desired_empty, &live_desired.snapshot, &HashMap::new(), &[])
        .expect_err("a drop with unknown ownership must fail closed");
    match err {
        DeclarativeError::DropOfUnownedTable { table } => assert_eq!(table, "orphan"),
        other => panic!("expected DropOfUnownedTable, got {other:?}"),
    }
}
