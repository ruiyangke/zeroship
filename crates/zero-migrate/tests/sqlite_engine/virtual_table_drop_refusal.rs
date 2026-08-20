//! The drop pass refuses to drop a live VIRTUAL table, and still drops an
//! ordinary one.
//!
//! WHY THIS EXISTS. The differ's drop pass authors `DROP TABLE` for every live
//! table absent from the desired union. A virtual table is not an ordinary table:
//! `fts5` and `vec0` keep their real payload in auto-created SHADOW tables, and
//! dropping the vtable CASCADES those away. So "tidy up an undeclared table"
//! silently destroys a search or vector index.
//!
//! The pre-existing ownership guard does NOT close this. It fails closed only when
//! the caller cannot confirm an owner; an orchestrator that maps every live table
//! to the deploying app — the shape a data-plane host naturally supplies — resolves
//! cleanly and reaches the drop. That is why the virtual-table check sits UPSTREAM
//! of ownership, and why the second test below (an ordinary table still drops under
//! exactly that ownership map) is load-bearing: a guard that refused everything
//! would pass the first test and be useless.
//!
//! The guard is keyed on the `CREATE VIRTUAL TABLE` token shape, never on a module
//! allowlist or a `__fts` name convention, so a module nobody has thought of is
//! covered on the same terms as the two we know about.

use crate::support;

use std::collections::HashMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::schema::query::SqlDialect;
use zero_migrate::{
    desired_snapshot_for_dialect, CollectionDescriptor, DeclarativeAuthor, DeclarativeError,
    FieldDescriptor, SqliteBackend,
};

const PROJECT: &str = "prj_demo";
const APP: &str = "app_demo";

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

/// The declared shape: a `posts` collection and nothing else. Anything else found
/// live is a drop candidate.
fn posts_descriptor() -> CollectionDescriptor {
    CollectionDescriptor {
        name: "posts".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "body".into(),
            ty: "string".into(),
            ..Default::default()
        }],
        indexes: vec![],
        runtime_options: Default::default(),
    }
}

/// Seed the app file on a RAW connection, before the hardened backend opens it.
///
/// It has to be raw for the virtual-table cases: the authorizer now denies
/// `CREATE VIRTUAL TABLE` in EVERY mode, so the engine cannot create the object
/// this guard exists to protect. That is the point rather than an obstacle — the
/// scenario under test is a database the engine did NOT build, which is exactly how
/// a legacy or runtime-managed index arrives.
fn seed(app: &std::path::Path, statements: &[&str]) {
    let conn = rusqlite::Connection::open(app).expect("open the app file raw to seed");
    for sql in statements {
        conn.execute_batch(sql)
            .unwrap_or_else(|e| panic!("seed statement must apply: {sql}\nerror: {e:?}"));
    }
}

/// Diff `desired` against the real live snapshot, with EVERY live table mapped to
/// the deploying app — the ownership map that sails through the ownership guard.
async fn diff_with_total_ownership(
    be: &SqliteBackend,
) -> Result<zero_migrate::DeclarativePlan, DeclarativeError> {
    let live = be
        .snapshot_schema_sqlite()
        .await
        .expect("introspect the live schema");
    let desired = desired_snapshot_for_dialect(
        PROJECT,
        &[posts_descriptor()],
        SqlDialect::Sqlite,
        &effective_policy(),
    )
    .expect("desired");
    let ownership: HashMap<String, String> = live
        .tables
        .keys()
        .map(|t| (t.clone(), APP.to_string()))
        .collect();
    DeclarativeAuthor::new_for_dialect(PROJECT, APP, SqlDialect::Sqlite).diff(
        &desired,
        &live,
        &ownership,
        &[],
        &effective_policy(),
    )
}

/// DIRECTION 1 — a live FTS5 virtual table is REFUSED, by name and by module, even
/// though its ownership resolves to the deploying app.
///
/// Seeded as a real FTS5 external-content vtable, which SQLite backs with four
/// auto-created shadow tables. Before the guard this planned five `DROP TABLE`s;
/// the first cascaded the other four away and the remaining four then failed
/// `no such table`, committing the destruction and reporting an unrelated error.
#[compio::test]
async fn a_live_virtual_table_is_refused_by_name_and_module() {
    let p = paths("vtable_refusal");
    seed(
        &p.app,
        &[
            r#"CREATE TABLE "posts" ("body" TEXT)"#,
            r#"CREATE VIRTUAL TABLE IF NOT EXISTS "posts__fts" USING fts5("body", content="posts", content_rowid="rowid")"#,
        ],
    );
    let be = SqliteBackend::open(&p.app, &p.journal).expect("open backend");

    // The shadow tables really are there — the thing a drop would have cascaded.
    let live = be.snapshot_schema_sqlite().await.expect("introspect");
    for shadow in [
        "posts__fts_config",
        "posts__fts_data",
        "posts__fts_docsize",
        "posts__fts_idx",
    ] {
        assert!(
            live.tables.contains_key(shadow),
            "the FTS5 shadow table {shadow} must be visible in the live snapshot, \
             or this test is not exercising the case it claims to"
        );
    }

    match diff_with_total_ownership(&be).await {
        Err(DeclarativeError::DropOfVirtualTable { table, module }) => {
            assert_eq!(table, "posts__fts");
            assert_eq!(module, "fts5");
        }
        Err(other) => {
            panic!("a live virtual table must be refused as DropOfVirtualTable, not {other:?}")
        }
        Ok(plan) => panic!(
            "a live virtual table must be REFUSED, but the diff authored: {:?}",
            plan.all_migrations()
                .iter()
                .map(|m| m.name.clone())
                .collect::<Vec<_>>()
        ),
    }
}

/// The refusal message must let an operator act without guessing: it names the
/// table, says VIRTUAL TABLE, and names the module.
#[compio::test]
async fn the_refusal_names_the_table_the_kind_and_the_module() {
    let p = paths("vtable_refusal_msg");
    seed(
        &p.app,
        &[
            r#"CREATE TABLE "posts" ("body" TEXT)"#,
            r#"CREATE VIRTUAL TABLE IF NOT EXISTS "posts__fts" USING fts5("body", content="posts", content_rowid="rowid")"#,
        ],
    );
    let be = SqliteBackend::open(&p.app, &p.journal).expect("open backend");
    let err = diff_with_total_ownership(&be)
        .await
        .expect_err("must be refused");
    let msg = err.to_string();
    for needle in ["posts__fts", "VIRTUAL TABLE", "fts5"] {
        assert!(
            msg.contains(needle),
            "the refusal must name {needle:?} so an operator need not guess; got: {msg}"
        );
    }
}

/// DIRECTION 2 — THE HALF THAT MATTERS. An ORDINARY undeclared table, under the
/// same total-ownership map, still drops exactly as before. A guard that refused
/// every undeclared table would pass the tests above and be worthless.
#[compio::test]
async fn an_ordinary_undeclared_table_still_drops() {
    let p = paths("vtable_refusal_control");
    seed(
        &p.app,
        &[
            r#"CREATE TABLE "posts" ("body" TEXT)"#,
            r#"CREATE TABLE "abandoned" ("x" TEXT)"#,
        ],
    );
    let be = SqliteBackend::open(&p.app, &p.journal).expect("open backend");
    let plan = diff_with_total_ownership(&be)
        .await
        .expect("an ordinary undeclared table must still plan cleanly");
    let dropped: Vec<String> = plan
        .all_migrations()
        .iter()
        .filter(|m| m.name.starts_with("drop_table"))
        .map(|m| m.name.clone())
        .collect();
    assert_eq!(
        dropped,
        vec!["drop_table_abandoned".to_string()],
        "the ordinary undeclared table must still be dropped — the guard must not \
         have turned into a blanket refusal"
    );
}

/// A table whose NAME merely looks like the FTS convention, but which is an
/// ordinary table, still drops. The guard reads the stored `CREATE`, not the name —
/// keying it on a `__fts` suffix would both over-refuse here and under-refuse for
/// any module that does not follow the convention.
#[compio::test]
async fn an_ordinary_table_named_like_an_index_still_drops() {
    let p = paths("vtable_refusal_lookalike");
    seed(
        &p.app,
        &[
            r#"CREATE TABLE "posts" ("body" TEXT)"#,
            r#"CREATE TABLE "notes__fts" ("x" TEXT)"#,
        ],
    );
    let be = SqliteBackend::open(&p.app, &p.journal).expect("open backend");
    let plan = diff_with_total_ownership(&be)
        .await
        .expect("an ordinary table with an index-shaped name must still plan cleanly");
    assert!(
        plan.all_migrations()
            .iter()
            .any(|m| m.name == "drop_table_notes__fts"),
        "a PLAIN table named `notes__fts` is not a virtual table and must still drop"
    );
}
