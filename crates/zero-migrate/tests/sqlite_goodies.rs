//! SQLite goodie-column coverage at parity with the PG `declarative_pg` /
//! `drift_pg` vector / geoPoint tests — the faithful path: the real
//! `DeclarativeAuthor` (SQLite dialect) builds the plan, applies it through the
//! real hardened `SqliteBackend` on a temp file, and the assertions read the
//! REAL DB end-state (`PRAGMA table_info`, `sqlite_master`) + a real re-diff.
//!
//! WHAT THE SQLITE ENGINE PATH ACTUALLY DOES (ground-truthed, not assumed):
//!   - a `vector(N)` field → a plain `BLOB` column + a plain B-tree index over it
//!     (the shared `def_to_column_type_for_dialect` maps vector→BLOB on SQLite;
//!     the engine's `SqliteEmitter::create_index` emits a plain B-tree for every
//!     index kind — there is NO `vec0` virtual table and NO metric validation on
//!     this path: sqlite-vec / vec0 vtables are the **plugin-db runtime** data
//!     plane's concern, created via `ensure_vector_index`, NOT the migrate engine.
//!     See `crates/plugin-db/tests/sqlite_integration.rs` and the
//!     `sqlite_*_column_ddl` register_model follow-up note there).
//!   - a `geoPoint` field → a packed `BLOB` column + a plain B-tree index (no
//!     PostGIS/GIST equivalent; spatial search is a haversine flat-scan in
//!     plugin-db per `docs/reference/sqlite-divergences.md`).
//!   - an `.fts()` field → an FTS5 **virtual table** (`<coll>__fts`) over the
//!     source columns + AFTER sync triggers, emitted via the SHARED
//!     `zero_migrate_schema::fts_sqlite` builders (the SAME structure plugin-db's
//!     runtime `ensure_fts_index` builds). This is the SQLite FTS shape — there is
//!     NO PG `__fts` tsvector column and NO GIN index (`tsvector` has no SQLite
//!     spelling). The SQLite-dialect `desired_snapshot_for_dialect` models it as an
//!     `IndexSnapshot` with `access_method = "fts5"`, the SQLite emitter emits the
//!     vtable+triggers (run under engine mode, since the hardened authorizer permits
//!     a vtable create ONLY in engine mode), and the drift introspector recognises
//!     the live vtable (excluding its FTS5 shadow tables), so a re-diff is
//!     ZERO-drift. (See `fts_field_applies_cleanly_on_sqlite` below.)

use std::collections::HashMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::{
    desired_snapshot, desired_snapshot_for_dialect, CollectionDescriptor, DeclarativeAuthor,
    FieldDescriptor, SchemaSnapshot, SqliteBackend,
};
use zero_migrate_schema::query::SqlDialect;

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

fn backend(p: &Paths) -> SqliteBackend {
    SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend")
}

fn sqlite_author() -> DeclarativeAuthor {
    DeclarativeAuthor::new_for_dialect(PROJECT, APP, SqlDialect::Sqlite)
}

fn ownership_of(d: &zero_migrate::DesiredSchema) -> HashMap<String, String> {
    d.ownership.iter().map(|(t, a)| (t.clone(), a.clone())).collect()
}

/// The declared SQLite type of `column` on `table`, via `PRAGMA table_info`
/// (engine mode lets the test issue the PRAGMA on `main`).
async fn column_type(be: &SqliteBackend, table: &str, column: &str) -> String {
    be.actor()
        .set_mode(zero_migrate::apply::backend::sqlite::Mode::EngineJournal)
        .await
        .expect("engine mode");
    let info = be
        .actor()
        .query(&format!("PRAGMA main.table_info({table})"))
        .await
        .expect("table_info");
    info.iter()
        .find(|r| r[1].as_deref() == Some(column))
        .and_then(|r| r[2].clone())
        .unwrap_or_default()
}

// ===========================================================================
// VECTOR — a `vector(N)` field applies as a BLOB column (+ a plain B-tree index)
// on the SQLite engine path; a re-diff is ZERO-drift. (PG has the ivfflat test in
// `declarative_pg`; SQLite had none.)
// ===========================================================================
#[compio::test]
async fn vector_field_applies_as_blob_and_redfiff_is_zero_drift() {
    let mk = || CollectionDescriptor {
        name: "docs".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "embedding".into(),
            ty: "vector".into(),
            vector_dims: Some(768),
            vector_metric: Some("cosine".into()),
            mask: None,
            ..Default::default()
        }],
        indexes: vec![],
    runtime_options: Default::default(),
    };

    let desired = desired_snapshot(PROJECT, &[mk()]).expect("desired");
    let plan = sqlite_author()
        .diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("diff");

    let p = paths("vector_blob");
    let be = backend(&p);
    for m in &plan.all_migrations() {
        be.apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("apply {} must succeed: {e:?}", m.name));
    }

    // REAL end-state: the vector column is a BLOB (sqlite-divergences: vector is a
    // packed BLOB, no vec0 vtable on the migrate-engine path).
    assert_eq!(
        column_type(&be, "docs", "embedding").await.to_ascii_uppercase(),
        "BLOB",
        "a vector(N) field is a BLOB column on SQLite (not a vec0 virtual table)"
    );

    // The faithful live snapshot, re-diffed against the SAME desired → ZERO drift
    // (no spurious ADD/DROP/rebuild). This is the parity the PG ivfflat round-trip
    // proves on its side.
    let live = be.snapshot_schema_sqlite().await.expect("introspect live");
    let own = ownership_of(&desired);
    let desired2 = desired_snapshot(PROJECT, &[mk()]).expect("re-desired");
    let plan2 = sqlite_author()
        .diff(&desired2, &live, &own, &[])
        .expect("re-diff must succeed");
    assert!(
        plan2.all_migrations().is_empty() && plan2.rebuilds.is_empty(),
        "a vector field must round-trip ZERO-drift; got migs={:?} rebuilds={}",
        plan2.all_migrations().iter().map(|m| m.name.clone()).collect::<Vec<_>>(),
        plan2.rebuilds.len()
    );
}

/// DIVERGENCE PIN — on the SQLite **migrate-engine** path, an `innerProduct`
/// metric does NOT raise a `vector_unsupported_metric` error (that validation
/// lives in the plugin-db runtime vector index builder, where sqlite-vec supports
/// only cosine + L2). On the engine path the metric is metadata that rides into
/// the (PG-shaped) ivfflat index snapshot; the SQLite leg emits a plain BLOB
/// column + plain B-tree index, so ANY metric token applies cleanly. This pins
/// that reality so a future "reject ip on SQLite at author time" change is a
/// visible, deliberate behaviour flip rather than a silent one.
#[compio::test]
async fn vector_inner_product_metric_applies_no_metric_error_on_engine_path() {
    let mk = || CollectionDescriptor {
        name: "docs".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "embedding".into(),
            ty: "vector".into(),
            vector_dims: Some(3),
            vector_metric: Some("innerProduct".into()),
            mask: None,
            ..Default::default()
        }],
        indexes: vec![],
    runtime_options: Default::default(),
    };
    let desired = desired_snapshot(PROJECT, &[mk()])
        .expect("an innerProduct vector descriptor compiles (no author-time metric refusal)");
    let plan = sqlite_author()
        .diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("diff with innerProduct metric must succeed on the SQLite engine path");

    let p = paths("vector_ip");
    let be = backend(&p);
    for m in &plan.all_migrations() {
        be.apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("apply {} must succeed: {e:?}", m.name));
    }
    assert_eq!(
        column_type(&be, "docs", "embedding").await.to_ascii_uppercase(),
        "BLOB",
        "innerProduct vector applies as a plain BLOB on the SQLite engine path"
    );
}

// ===========================================================================
// GEOPOINT — a `geoPoint` field applies as a packed BLOB column on SQLite
// (no spatial index AM; haversine flat-scan per sqlite-divergences). Drift
// round-trips. (PG emits a GIST spatial index; SQLite has none.)
// ===========================================================================
#[compio::test]
async fn geopoint_field_applies_as_blob_and_drift_round_trips() {
    let mk = || CollectionDescriptor {
        name: "places".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "loc".into(),
            ty: "geoPoint".into(),
            ..Default::default()
        }],
        indexes: vec![],
    runtime_options: Default::default(),
    };
    let desired = desired_snapshot(PROJECT, &[mk()]).expect("desired");
    let plan = sqlite_author()
        .diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("diff");

    let p = paths("geo_blob");
    let be = backend(&p);
    for m in &plan.all_migrations() {
        be.apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("apply {} must succeed: {e:?}", m.name));
    }

    // REAL end-state: the geoPoint column is a packed BLOB.
    assert_eq!(
        column_type(&be, "places", "loc").await.to_ascii_uppercase(),
        "BLOB",
        "a geoPoint field is a packed BLOB column on SQLite"
    );
    // No PostGIS/GIST: SQLite has no `USING gist` access method — the column is a
    // plain BLOB. (The plugin-db data plane does a haversine flat-scan; there is no
    // spatial index object to assert here. We DO assert there is no *unexpected*
    // spatial vtable.)
    be.actor()
        .set_mode(zero_migrate::apply::backend::sqlite::Mode::EngineJournal)
        .await
        .expect("engine mode");
    let vtables = be
        .actor()
        .query("SELECT name FROM main.sqlite_master WHERE type='table' AND sql LIKE '%USING%'")
        .await
        .expect("scan for virtual tables");
    assert!(
        vtables.is_empty(),
        "geoPoint must NOT create a virtual/spatial table on the engine path: {vtables:?}"
    );

    // A re-diff against the REAL introspected live snapshot → ZERO drift.
    let live = be.snapshot_schema_sqlite().await.expect("introspect live");
    let own = ownership_of(&desired);
    let desired2 = desired_snapshot(PROJECT, &[mk()]).expect("re-desired");
    let plan2 = sqlite_author()
        .diff(&desired2, &live, &own, &[])
        .expect("re-diff must succeed");
    assert!(
        plan2.all_migrations().is_empty() && plan2.rebuilds.is_empty(),
        "a geoPoint field must round-trip ZERO-drift; got migs={:?} rebuilds={}",
        plan2.all_migrations().iter().map(|m| m.name.clone()).collect::<Vec<_>>(),
        plan2.rebuilds.len()
    );
}

// ===========================================================================
// FTS — an `.fts()` field applies on SQLite as an FTS5 external-content VIRTUAL
// TABLE (`<coll>__fts`) + AFTER sync triggers (the SAME structure plugin-db's
// runtime `ensure_fts_index` builds, via the shared `zero_migrate_schema::fts_sqlite`
// builders), NOT the PG `__fts` tsvector column + GIN index (`tsvector` has no
// SQLite spelling). The SQLite-dialect `desired_snapshot_for_dialect` models it as
// an `IndexSnapshot { access_method: "fts5" }`; the SQLite emitter emits the
// vtable+triggers (run under engine mode — the hardened authorizer permits a vtable
// create ONLY in engine mode); and the drift introspector recognises the live
// vtable (excluding its FTS5 shadow tables), so a re-diff is ZERO-drift.
//
// REGRESSION: pre-fix this was `#[ignore]`d — the dialect-blind `desired_snapshot`
// emitted a PG-shaped `__fts` (tsvector/GIN) index over a column the SQLite
// create-table never materialised → apply failed with `no such column: "__fts"`.
#[compio::test]
async fn fts_field_applies_cleanly_on_sqlite() {
    let mk = || CollectionDescriptor {
        name: "posts".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "body".into(),
            ty: "string".into(),
            fts: true,
            ..Default::default()
        }],
        indexes: vec![],
    runtime_options: Default::default(),
    };
    // DIALECT-AWARE desired: on SQLite the FTS index is the FTS5 vtable, not the PG
    // `__fts` GIN index. (Using the PG-default `desired_snapshot` here is exactly the
    // pre-fix bug — it would emit the broken `__fts` index.)
    let desired = desired_snapshot_for_dialect(PROJECT, &[mk()], SqlDialect::Sqlite)
        .expect("desired");
    let plan = sqlite_author()
        .diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("diff");

    let p = paths("fts_sqlite");
    let be = backend(&p);
    for m in &plan.all_migrations() {
        be.apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("apply {} must succeed: {e:?}", m.name));
    }

    // REAL end-state: the FTS5 vtable + its three AFTER triggers exist.
    be.actor()
        .set_mode(zero_migrate::apply::backend::sqlite::Mode::EngineJournal)
        .await
        .expect("engine mode");
    let vtables = be
        .actor()
        .query(
            "SELECT name FROM main.sqlite_master \
             WHERE type='table' AND name='posts__fts'",
        )
        .await
        .expect("scan for the fts vtable");
    assert_eq!(
        vtables.len(),
        1,
        "the FTS5 virtual table posts__fts must exist: {vtables:?}"
    );
    let triggers = be
        .actor()
        .query(
            "SELECT name FROM main.sqlite_master \
             WHERE type='trigger' AND name IN \
             ('posts__fts_ai','posts__fts_ad','posts__fts_au') ORDER BY name",
        )
        .await
        .expect("scan for the fts triggers");
    assert_eq!(
        triggers.len(),
        3,
        "the three FTS sync triggers must exist: {triggers:?}"
    );

    // NOTE on queryability: actual FTS reads/writes (the tokenising INSERT through
    // the trigger, a `MATCH` query) are the **plugin-db data plane's** job, NOT the
    // migrate engine's — and the engine's hardened SQLite authorizer deliberately
    // does NOT allowlist the fts5 runtime tokenizer functions (a creator/AI `up`
    // has no business running them). So the faithful in-LAYER assertions here are
    // (a) the vtable + triggers exist, and (b) the structure round-trips zero-drift.
    // The end-to-end MATCH-finds-the-mirrored-row queryability is exercised against
    // the runtime backend in `plugin-db/tests/sqlite_integration.rs` (the data-plane
    // connection, which shares these exact FTS5 builders via `zero_migrate_schema`).

    // A re-diff against the REAL introspected live snapshot → ZERO drift (the FTS5
    // vtable is recognised as the fts5 IndexSnapshot; its shadow tables are excluded;
    // no spurious ADD/DROP).
    let live = be.snapshot_schema_sqlite().await.expect("introspect live");
    let own = ownership_of(&desired);
    let desired2 = desired_snapshot_for_dialect(PROJECT, &[mk()], SqlDialect::Sqlite)
        .expect("re-desired");
    let plan2 = sqlite_author()
        .diff(&desired2, &live, &own, &[])
        .expect("re-diff must succeed");
    assert!(
        plan2.all_migrations().is_empty() && plan2.rebuilds.is_empty(),
        "an FTS field must round-trip ZERO-drift; got migs={:?} rebuilds={}",
        plan2.all_migrations().iter().map(|m| m.name.clone()).collect::<Vec<_>>(),
        plan2.rebuilds.len()
    );
}
