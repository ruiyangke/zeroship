//! `SQLite` goodie-column coverage at parity with the PG `declarative_pg` /
//! `drift_pg` vector / geoPoint tests — the faithful path: the real
//! `DeclarativeAuthor` (`SQLite` dialect) builds the plan, applies it through the
//! real hardened `SqliteBackend` on a temp file, and the assertions read the
//! REAL DB end-state (`PRAGMA table_info`, `sqlite_master`) + a real re-diff.
//!
//! WHAT THE SQLITE ENGINE PATH ACTUALLY DOES (ground-truthed, not assumed):
//!   - a `vector(N)` field → a plain `BLOB` column + a plain B-tree index over it
//!     (the shared `def_to_column_type_for_dialect` maps vector→BLOB on `SQLite`;
//!     the engine's `SqliteEmitter::create_index` emits a plain B-tree for every
//!     index kind — there is NO `vec0` virtual table and NO metric validation on
//!     this path: sqlite-vec / vec0 vtables are the **plugin-db runtime** data
//!     plane's concern, created via `ensure_vector_index`, NOT the migrate
//!     engine, and covered by that runtime's own suite in a separate
//!     repository).
//!   - a `geoPoint` field → a packed `BLOB` column + a plain B-tree index (no
//!     PostGIS/GIST equivalent; spatial search is a haversine flat-scan in
//!     plugin-db).
//!   - an `.fts()` field → an FTS5 **virtual table** (`<coll>__fts`) over the
//!     source columns + AFTER sync triggers, emitted via the SHARED
//!     `zero_migrate::schema::fts_sqlite` builders (the SAME structure plugin-db's
//!     runtime `ensure_fts_index` builds). This is the `SQLite` FTS shape — there is
//!     NO PG `__fts` tsvector column and NO GIN index (`tsvector` has no `SQLite`
//!     spelling). The SQLite-dialect `desired_snapshot_for_dialect` models it as an
//!     `IndexSnapshot` with `access_method = "fts5"`, the `SQLite` emitter emits the
//!     vtable+triggers (run under engine mode, since the hardened authorizer permits
//!     a vtable create ONLY in engine mode), and the drift introspector recognises
//!     the live vtable (excluding its FTS5 shadow tables), so a re-diff is
//!     ZERO-drift. (See `fts_field_applies_cleanly_on_sqlite` below.)

mod support;

use std::collections::HashMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::model::migration::{
    Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId,
};
use zero_migrate::schema::query::SqlDialect;
use zero_migrate::{
    desired_snapshot_for_dialect, CollectionDescriptor, DeclarativeAuthor, FieldDescriptor,
    SchemaSnapshot, SqliteBackend,
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

fn backend(p: &Paths) -> SqliteBackend {
    SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend")
}

fn sqlite_author() -> DeclarativeAuthor {
    DeclarativeAuthor::new_for_dialect(PROJECT, APP, SqlDialect::Sqlite)
}

fn effective_policy() -> zero_migrate::EffectivePolicy {
    support::confined_charter()
}

fn ownership_of(d: &zero_migrate::DesiredSchema) -> HashMap<String, String> {
    d.ownership
        .iter()
        .map(|(t, a)| (t.clone(), a.clone()))
        .collect()
}

/// The declared `SQLite` type of `column` on `table`, via `PRAGMA table_info`
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

    let desired =
        desired_snapshot_for_dialect(PROJECT, &[mk()], SqlDialect::Sqlite, &effective_policy())
            .expect("desired");
    let plan = sqlite_author()
        .diff(
            &desired,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
            &effective_policy(),
        )
        .expect("diff");

    let p = paths("vector_blob");
    let be = backend(&p);
    for m in &plan.all_migrations() {
        be.apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("apply {} must succeed: {e:?}", m.name));
    }

    // REAL end-state: the vector column is a BLOB: on the migrate-engine path a
    // vector is a packed BLOB with no vec0 vtable.
    assert_eq!(
        column_type(&be, "docs", "embedding")
            .await
            .to_ascii_uppercase(),
        "BLOB",
        "a vector(N) field is a BLOB column on SQLite (not a vec0 virtual table)"
    );

    // The faithful live snapshot, re-diffed against the SAME desired → ZERO drift
    // (no spurious ADD/DROP/rebuild). This is the parity the PG ivfflat round-trip
    // proves on its side.
    let live = be.snapshot_schema_sqlite().await.expect("introspect live");
    let own = ownership_of(&desired);
    let desired2 =
        desired_snapshot_for_dialect(PROJECT, &[mk()], SqlDialect::Sqlite, &effective_policy())
            .expect("re-desired");
    let plan2 = sqlite_author()
        .diff(&desired2, &live, &own, &[], &effective_policy())
        .expect("re-diff must succeed");
    assert!(
        plan2.all_migrations().is_empty() && plan2.rebuilds.is_empty(),
        "a vector field must round-trip ZERO-drift; got migs={:?} rebuilds={}",
        plan2
            .all_migrations()
            .iter()
            .map(|m| m.name.clone())
            .collect::<Vec<_>>(),
        plan2.rebuilds.len()
    );
}

/// DIVERGENCE PIN — on the `SQLite` **migrate-engine** path, an `innerProduct`
/// metric does NOT raise a `vector_unsupported_metric` error (that validation
/// lives in the plugin-db runtime vector index builder, where sqlite-vec supports
/// only cosine + L2). On the engine path the metric is metadata that rides into
/// the (PG-shaped) ivfflat index snapshot; the `SQLite` leg emits a plain BLOB
/// column + plain B-tree index, so ANY metric token applies cleanly. This pins
/// that reality so a future "reject ip on `SQLite` at author time" change is a
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
    let desired =
        desired_snapshot_for_dialect(PROJECT, &[mk()], SqlDialect::Sqlite, &effective_policy())
            .expect("an innerProduct vector descriptor compiles (no author-time metric refusal)");
    let plan = sqlite_author()
        .diff(
            &desired,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
            &effective_policy(),
        )
        .expect("diff with innerProduct metric must succeed on the SQLite engine path");

    let p = paths("vector_ip");
    let be = backend(&p);
    for m in &plan.all_migrations() {
        be.apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("apply {} must succeed: {e:?}", m.name));
    }
    assert_eq!(
        column_type(&be, "docs", "embedding")
            .await
            .to_ascii_uppercase(),
        "BLOB",
        "innerProduct vector applies as a plain BLOB on the SQLite engine path"
    );
}

// ===========================================================================
// GEOPOINT — a `geoPoint` field applies as a packed BLOB column on SQLite
// (no spatial index AM; the runtime does a haversine flat-scan). Drift
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
    let desired =
        desired_snapshot_for_dialect(PROJECT, &[mk()], SqlDialect::Sqlite, &effective_policy())
            .expect("desired");
    let plan = sqlite_author()
        .diff(
            &desired,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
            &effective_policy(),
        )
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
    let desired2 =
        desired_snapshot_for_dialect(PROJECT, &[mk()], SqlDialect::Sqlite, &effective_policy())
            .expect("re-desired");
    let plan2 = sqlite_author()
        .diff(&desired2, &live, &own, &[], &effective_policy())
        .expect("re-diff must succeed");
    assert!(
        plan2.all_migrations().is_empty() && plan2.rebuilds.is_empty(),
        "a geoPoint field must round-trip ZERO-drift; got migs={:?} rebuilds={}",
        plan2
            .all_migrations()
            .iter()
            .map(|m| m.name.clone())
            .collect::<Vec<_>>(),
        plan2.rebuilds.len()
    );
}

// ===========================================================================
// FTS — an `.fts()` field applies on SQLite as an FTS5 external-content VIRTUAL
// TABLE (`<coll>__fts`) + AFTER sync triggers (the SAME structure plugin-db's
// runtime `ensure_fts_index` builds, via the shared `zero_migrate::schema::fts_sqlite`
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
    let desired =
        desired_snapshot_for_dialect(PROJECT, &[mk()], SqlDialect::Sqlite, &effective_policy())
            .expect("desired");
    let plan = sqlite_author()
        .diff(
            &desired,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
            &effective_policy(),
        )
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

    // WHAT THIS TEST DOES NOT COVER, and where the coverage now lives. It diffs
    // against an EMPTY base table, so the engine's own initial-population
    // `INSERT ... SELECT` selects zero rows, FTS5's `xUpdate` is never entered, and
    // the three triggers above are created and never fired. The structure is
    // asserted; nothing here proves a row can be indexed or found, and that blind
    // spot is what let the FTS index ship unable to index anything.
    //
    // The two behaviour tests below close it, and they are kept separate because
    // population and trigger-sync run under DIFFERENT authorizer modes:
    // `fts_index_over_a_populated_table_indexes_the_rows_already_there` (the engine's
    // population batch, EngineJournal) and
    // `a_creator_insert_reaches_the_fts_index_through_the_sync_trigger` (a creator
    // write, CreatorUp). Both assert a MATCH COUNT on a REOPENED RAW connection,
    // never `is_ok()` - a zero-row MATCH against an unpopulated index is the silent
    // failure in play, and the engine never issues an FTS5 MATCH in production, so
    // the query belongs on a raw connection rather than in the authorizer's
    // allowlist.

    // A re-diff against the REAL introspected live snapshot → ZERO drift (the FTS5
    // vtable is recognised as the fts5 IndexSnapshot; its shadow tables are excluded;
    // no spurious ADD/DROP).
    let live = be.snapshot_schema_sqlite().await.expect("introspect live");
    let own = ownership_of(&desired);
    let desired2 =
        desired_snapshot_for_dialect(PROJECT, &[mk()], SqlDialect::Sqlite, &effective_policy())
            .expect("re-desired");
    let plan2 = sqlite_author()
        .diff(&desired2, &live, &own, &[], &effective_policy())
        .expect("re-diff must succeed");
    assert!(
        plan2.all_migrations().is_empty() && plan2.rebuilds.is_empty(),
        "an FTS field must round-trip ZERO-drift; got migs={:?} rebuilds={}",
        plan2
            .all_migrations()
            .iter()
            .map(|m| m.name.clone())
            .collect::<Vec<_>>(),
        plan2.rebuilds.len()
    );
}

// ===========================================================================
// FTS BEHAVIOUR - a row reaches the index, on both routes into FTS5's xUpdate.
//
// The structural test above proves the vtable and its triggers EXIST. These two
// prove the index actually holds rows, which is the property that was broken:
// FTS5 fires `PRAGMA data_version` internally on first access to an index object
// on a connection, and the authorizer's pragma arm refused it, so every write that
// reached the index died with `Exec("authorization denied")`.
//
// The two routes run under DIFFERENT authorizer modes and are therefore separate
// tests - collapsing them would let one mode's fix hide the other's breakage:
//   - the engine's initial-population `INSERT ... SELECT`, under EngineJournal;
//   - a creator `INSERT` mirrored by the `__fts_ai` trigger, under CreatorUp.
//
// Both read the index back on a REOPENED RAW `rusqlite::Connection` and assert a
// COUNT. The engine never issues an FTS5 MATCH in production, so `MATCH` is
// deliberately absent from the authorizer's surface; the raw reopen is the same
// positive-control pattern `sqlite_confinement.rs` uses.
// ===========================================================================

/// The `posts` collection, with `body` either plain or `.fts()`-indexed. Diffing
/// the plain form first and the FTS form second against the REAL live snapshot is
/// how an FTS index arrives on a table that already holds rows - and it drives the
/// engine's own emitter both times rather than hand-assembling a lookalike batch.
fn posts_descriptor(fts: bool) -> CollectionDescriptor {
    CollectionDescriptor {
        name: "posts".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "body".into(),
            ty: "string".into(),
            fts,
            ..Default::default()
        }],
        indexes: vec![],
        runtime_options: Default::default(),
    }
}

/// An ordinary creator `up` (no engine-goodie flag), so the statement runs under
/// the confined `CreatorUp` mode exactly as tenant-authored SQL does.
fn creator_up(up: &str) -> Migration {
    let flags = MigrationFlags::default();
    let checksum = Checksum::of(&ChecksumInput {
        up,
        down: None,
        flags: &flags,
        owner_app: APP,
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });
    Migration {
        version: MigrationId::generate(),
        name: "creator_write".to_string(),
        up: up.to_string(),
        down: None,
        checksum,
        flags,
        owner_app: APP.to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
    }
}

/// Apply the base `posts` table (no FTS index yet) and return the ownership map the
/// follow-up diff needs.
async fn apply_base_posts_table(be: &SqliteBackend) -> HashMap<String, String> {
    let desired = desired_snapshot_for_dialect(
        PROJECT,
        &[posts_descriptor(false)],
        SqlDialect::Sqlite,
        &effective_policy(),
    )
    .expect("desired without the fts index");
    let plan = sqlite_author()
        .diff(
            &desired,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
            &effective_policy(),
        )
        .expect("diff");
    for m in &plan.all_migrations() {
        be.apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("apply {} must succeed: {e:?}", m.name));
    }
    ownership_of(&desired)
}

/// Diff the FTS-bearing descriptor against the REAL live snapshot and apply the one
/// migration that comes out. Returns nothing but asserts, on the way, that what was
/// applied really is the emitter's FTS batch (create + population + triggers) and
/// not some other migration that happened to be planned.
async fn apply_the_emitters_fts_batch(be: &SqliteBackend, ownership: &HashMap<String, String>) {
    let live = be
        .snapshot_schema_sqlite()
        .await
        .expect("introspect the live schema");
    let desired = desired_snapshot_for_dialect(
        PROJECT,
        &[posts_descriptor(true)],
        SqlDialect::Sqlite,
        &effective_policy(),
    )
    .expect("desired with the fts index");
    let plan = sqlite_author()
        .diff(&desired, &live, ownership, &[], &effective_policy())
        .expect("diff must succeed");
    let migrations = plan.all_migrations();
    assert_eq!(
        migrations.len(),
        1,
        "adding `.fts()` to an existing collection must plan exactly the FTS batch: {:?}",
        migrations
            .iter()
            .map(|m| m.name.clone())
            .collect::<Vec<_>>()
    );
    let batch = &migrations[0];
    assert!(
        batch.flags.engine_goodie_ddl,
        "the FTS batch is engine-authored DDL and must carry the engine-goodie flag, \
         or it would run under CreatorUp and be denied its vtable create"
    );
    assert!(
        batch.up.contains("USING fts5")
            && batch.up.contains("INSERT INTO \"posts__fts\" (rowid,")
            && batch
                .up
                .contains("CREATE TRIGGER IF NOT EXISTS \"posts__fts_ai\""),
        "the batch under test must be the emitter's create + population + triggers, \
         not a lookalike: {}",
        batch.up
    );
    be.apply_one_additive(batch, "deployer")
        .await
        .expect("the engine's FTS batch must apply to a table that already holds rows");
}

/// Read the index back the way a data plane would: a REOPENED RAW connection on the
/// app file, running the FTS5 MATCH the engine deliberately never issues.
fn raw_match_count(app: &std::path::Path, term: &str) -> i64 {
    let conn = rusqlite::Connection::open(app).expect("reopen the app file raw");
    conn.query_row(
        "SELECT count(*) FROM \"posts__fts\" WHERE \"posts__fts\" MATCH ?1",
        [term],
        |row| row.get(0),
    )
    .expect("the FTS5 index answers a MATCH")
}

/// The population route: rows that were ALREADY in the base table when the index
/// was added are searchable. A non-empty base table is what makes the engine's
/// initial-population `INSERT ... SELECT` do work and enter FTS5's `xUpdate`; the
/// structural test above is green precisely because it skips this.
#[compio::test]
async fn fts_index_over_a_populated_table_indexes_the_rows_already_there() {
    let p = paths("fts_populated");
    let be = backend(&p);
    let ownership = apply_base_posts_table(&be).await;

    // The row goes in through the REAL backend, as an ordinary creator `up`, BEFORE
    // the index exists - so it can only reach the index via the population batch.
    be.apply_one_additive(
        &creator_up(
            "INSERT INTO posts (id, created_at, updated_at, version, body) \
             VALUES ('p1', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 1, 'hello world');",
        ),
        "creator",
    )
    .await
    .expect("an ordinary creator INSERT into the base table must apply");

    apply_the_emitters_fts_batch(&be, &ownership).await;

    assert_eq!(
        raw_match_count(&p.app, "hello"),
        1,
        "the row that predated the index must be searchable: the population batch \
         either ran or was refused, and a zero count is the silent failure"
    );
}

/// The trigger route: a creator write AFTER the index exists reaches it through the
/// `__fts_ai` sync trigger, which runs under `CreatorUp` rather than the engine's
/// mode.
///
/// The index is built over an EMPTY base table here on purpose. The population
/// statement then selects no rows and never enters `xUpdate`, so nothing in the
/// setup can satisfy this test on the population route - the only write that
/// reaches FTS5 is the creator INSERT below, under CreatorUp.
///
/// The INSERT runs on a FRESH backend. FTS5's internal pragma is a prepared
/// statement cached per connection, so a connection that already touched the index
/// would not issue it again and a mode-specific refusal would go unseen.
#[compio::test]
async fn a_creator_insert_reaches_the_fts_index_through_the_sync_trigger() {
    let p = paths("fts_trigger");
    {
        let be = backend(&p);
        let ownership = apply_base_posts_table(&be).await;
        apply_the_emitters_fts_batch(&be, &ownership).await;
    }

    let fresh = backend(&p);
    fresh
        .apply_one_additive(
            &creator_up(
                "INSERT INTO posts (id, created_at, updated_at, version, body) \
                 VALUES ('p2', '2026-01-02T00:00:00Z', '2026-01-02T00:00:00Z', 1, 'goodbye moon');",
            ),
            "creator",
        )
        .await
        .expect("an ordinary creator INSERT into an FTS-indexed table must apply");

    assert_eq!(
        raw_match_count(&p.app, "goodbye"),
        1,
        "the creator's row must have reached the index through the sync trigger"
    );
}
