//! PR9b (b) — SQLite renameColumn runnable IN PRODUCTION, faithful e2e through the
//! NEW catalog-sourced entry [`apply_bundle_ir_sqlite_catalog`], on a REAL temp-file
//! SQLite DB. No shims, no PG-gated skips, and — critically — NO hand-fed pre-rename
//! descriptor set.
//!
//! The headline: a production caller derives its `descriptors` from `registerModel` =
//! the POST-deploy DESIRED schema (post-rename: only `full_name` exists, `name` is
//! GONE). The OLD descriptor entry (`apply_bundle_ir_sqlite`) fails closed on exactly
//! that set (pinned by `ir_deploy_sqlite_rename_pr7.rs`). The NEW catalog entry sources
//! the rename's PRE-rename `name` column facts from a REAL pre-deploy SQLite-catalog
//! read — so the rename runs as a rebuild WITHOUT a pre-rename descriptor:
//!
//! - the seeded row SURVIVES with its value moved to the renamed `full_name` column;
//! - the old `name` column is GONE;
//! - the UNRELATED `secret` ENCRYPTED column's facet (the inline `zsenc:` sentinel +
//!   BLOB affinity) is PRESERVED on the rebuilt table — the rebuild did not drop it;
//! - the journal records the rebuild migration version.
//!
//! Contrast with `ir_deploy_sqlite_rename_pr7.rs:125`, which hand-feeds the PRE-rename
//! descriptor — this test must NOT do that (the descriptors are the post-rename desired).

use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::render::declarative::{CollectionDescriptor, FieldDescriptor};
use zero_migrate::{
    apply_bundle_ir_sqlite, apply_bundle_ir_sqlite_catalog, Approval, ExecutorConfig, GuardConfig,
    MigrationIr, PolicyProfile, SqliteBackend, SqliteIrApplyError, resolve_create_table_policy,
};

const PROJECT: &str = "prj_pr9b";
const APP: &str = "app_pr9b";

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
    migrations: PathBuf,
}

fn paths(tag: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{tag}.sqlite"));
    let journal = dir.path().join(format!("zs-{tag}.migrations.sqlite"));
    let migrations = dir.path().join("migrations");
    std::fs::create_dir_all(&migrations).expect("mkdir migrations");
    Paths { _dir: dir, app, journal, migrations }
}

fn backend(p: &Paths) -> SqliteBackend {
    SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend")
}

fn exec_cfg() -> ExecutorConfig {
    ExecutorConfig::new(PROJECT, PROJECT)
}

fn write_ir(p: &Paths, file: &str, body: &str) {
    let ir: MigrationIr = serde_json::from_str(body).expect("test IR parses");
    let resolved =
        resolve_create_table_policy(&ir, &PolicyProfile::confined()).expect("test IR resolves");
    let body = serde_json::to_string(&resolved).expect("resolved test IR serializes");
    std::fs::write(p.migrations.join(file), body).expect("write ir file");
}

fn clear_migrations(p: &Paths) {
    for entry in std::fs::read_dir(&p.migrations).expect("read migrations dir") {
        let path = entry.expect("dir entry").path();
        let _ = std::fs::remove_file(path);
    }
}

/// A plain required string field.
fn text_field(name: &str) -> FieldDescriptor {
    FieldDescriptor { name: name.into(), ty: "string".into(), required: true, ..Default::default() }
}

/// An ENCRYPTED string field (the facet the rebuild must preserve).
fn encrypted_field(name: &str) -> FieldDescriptor {
    FieldDescriptor {
        name: name.into(),
        ty: "string".into(),
        required: false,
        encrypted: Some(serde_json::json!({ "mode": "randomized", "keyId": "k1" })),
        ..Default::default()
    }
}

/// A string field carrying a SAME-AFFINITY data-transforming facet — a `default`
/// (TEXT affinity preserved, so the PR9b affinity guard does NOT catch it; the PR9c
/// LOW (ii) full-facet guard must). Used to prove the tightened guard refuses a rename
/// bundled with a facet change the rebuild's verbatim value-copy cannot certify.
fn defaulted_text_field(name: &str) -> FieldDescriptor {
    FieldDescriptor {
        name: name.into(),
        ty: "string".into(),
        required: false,
        default: Some(serde_json::json!("unknown")),
        ..Default::default()
    }
}

fn descriptor(table: &str, fields: Vec<FieldDescriptor>) -> CollectionDescriptor {
    CollectionDescriptor { name: table.into(), owner_app: APP.into(), fields, indexes: vec![], runtime_options: Default::default() }
}

// The headline production-path proof.
#[compio::test]
async fn renamecolumn_runs_in_production_via_catalog_without_prerename_descriptor() {
    let p = paths("catalog_rename");
    let be = backend(&p);

    // Deploy #1 (routine createTable): users(name text, secret encrypted).
    // The PRE-rename descriptor set is used ONLY for this createTable deploy.
    let v1 = [descriptor("users", vec![text_field("name"), encrypted_field("secret")])];
    let create = r#"{"ir_version":1,"name":"create_users","ops":[
        {"op":"createTable","name":"users","columns":[
            {"name":"name","type":"text","nullable":false},
            {"name":"secret","type":{"encrypted":{"of":"text"}},"nullable":true}
        ]}
    ]}"#;
    write_ir(&p, "0001_create_users.ir.json", create);
    apply_bundle_ir_sqlite(
        &be, PROJECT, APP, &v1, &p.migrations, &exec_cfg(),
        &GuardConfig::confined(PROJECT), &PolicyProfile::confined(), Approval::None,
    )
    .await
    .expect("createTable users(name, secret encrypted) must succeed");

    // Sanity: the live table is name(text) + secret(BLOB + inline zsenc sentinel).
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    let create_sql = be
        .actor()
        .query("SELECT sql FROM main.sqlite_master WHERE type='table' AND name='users'")
        .await
        .expect("read create sql");
    let create_text = create_sql[0][0].clone().unwrap_or_default();
    assert!(
        create_text.to_lowercase().contains("secret") && create_text.contains("zsenc:"),
        "deploy #1 must create `secret` as an encrypted column with a zsenc sentinel: {create_text}"
    );

    // Seed a row with BOTH columns set — `secret` carries a NON-NULL encrypted BLOB
    // (a `zsenc:`-tagged ciphertext blob, the shape plugin-db writes at runtime). PR9b
    // LOW (iii): the prior seed left `secret` NULL, so the rebuild's encrypted-column
    // VALUE-COPY was never exercised with a real blob (copying NULL→NULL is vacuous).
    // We seed a concrete blob so the rebuild's `INSERT … SELECT` must carry the bytes
    // across the renamed table, and assert below it reads back BYTE-IDENTICAL.
    //
    // The blob is the literal AEAD-on-disk shape: the `zsenc:v1:` tag prefix + raw
    // ciphertext bytes. We write it as a SQLite blob literal (X'…') so the stored bytes
    // are deterministic and the post-rebuild assertion can compare them exactly.
    be.actor()
        .exec(
            "INSERT INTO main.users (id, created_at, updated_at, version, name, secret) \
             VALUES ('u1','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z',1, \
                     'Ada Lovelace', X'7a73656e633a76313a0011223344aabbcc')",
        )
        .await
        .expect("seed user with a non-null encrypted secret blob");

    // Capture the seeded blob's exact bytes (as lower-hex) so the post-rebuild check is a
    // byte-identical comparison, not a "non-null" smoke test.
    let seeded_secret_hex = be
        .actor()
        .query("SELECT lower(hex(secret)) FROM main.users WHERE id='u1'")
        .await
        .expect("read seeded secret hex")[0][0]
        .clone()
        .expect("seeded secret must be non-null");
    assert_eq!(
        seeded_secret_hex, "7a73656e633a76313a0011223344aabbcc",
        "the seeded encrypted blob is stored verbatim before the rebuild"
    );

    // Deploy #2 (APPROVED rename via the CATALOG entry): renameColumn name → full_name.
    // The descriptor set is the POST-deploy DESIRED schema — `full_name` (NOT `name`)
    // + `secret`. A production caller passes exactly this; the OLD descriptor entry
    // would fail closed on it, but the NEW catalog entry sources the live `name`
    // column facts from the catalog read.
    clear_migrations(&p);
    let rename = r#"{"ir_version":1,"name":"rename_name","ops":[
        {"op":"renameColumn","table":"users","from":"name","to":"full_name","type":"text"}
    ]}"#;
    write_ir(&p, "0002_rename_name.ir.json", rename);
    let post_rename_desired =
        [descriptor("users", vec![text_field("full_name"), encrypted_field("secret")])];

    let out = apply_bundle_ir_sqlite_catalog(
        &be,
        PROJECT,
        APP,
        &post_rename_desired,
        &p.migrations,
        &exec_cfg(),
        &GuardConfig::confined(PROJECT),
        &PolicyProfile::confined(), Approval::Approved,
    )
    .await
    .expect(
        "the catalog-sourced entry must RUN the rename as a rebuild WITHOUT a pre-rename \
         descriptor (live `name` facts come from the catalog)",
    );
    assert_eq!(out.applied.len(), 1, "exactly the rebuild migration applied: {out:?}");

    // The seeded row SURVIVES with its value moved to the renamed `full_name` column.
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    let rows = be
        .actor()
        .query("SELECT full_name FROM main.users ORDER BY id")
        .await
        .expect("read full_name");
    assert_eq!(
        rows.iter().filter_map(|r| r[0].clone()).collect::<Vec<_>>(),
        vec!["Ada Lovelace"],
        "the seeded row survives, value carried across to the renamed `full_name`"
    );

    // The old `name` column is GONE.
    let info = be.actor().query("PRAGMA main.table_info(users)").await.expect("table_info");
    assert!(
        info.iter().all(|r| r[1].as_deref() != Some("name")),
        "the old `name` column is dropped by the rebuild"
    );
    assert!(
        info.iter().any(|r| r[1].as_deref() == Some("full_name")),
        "the renamed `full_name` column exists"
    );

    // The UNRELATED `secret` ENCRYPTED facet is PRESERVED on the rebuilt table: the
    // rebuilt CREATE still carries the `zsenc:` sentinel + the BLOB affinity for it.
    let rebuilt_sql = be
        .actor()
        .query("SELECT sql FROM main.sqlite_master WHERE type='table' AND name='users'")
        .await
        .expect("read rebuilt create sql");
    let rebuilt_text = rebuilt_sql[0][0].clone().unwrap_or_default();
    assert!(
        rebuilt_text.contains("zsenc:"),
        "the rebuilt table must PRESERVE the `secret` encryption sentinel (facet sourced \
         from the descriptor, not dropped by the rebuild): {rebuilt_text}"
    );
    let secret_type = info
        .iter()
        .find(|r| r[1].as_deref() == Some("secret"))
        .and_then(|r| r[2].clone())
        .unwrap_or_default()
        .to_lowercase();
    assert_eq!(
        secret_type, "blob",
        "the rebuilt `secret` column keeps its encrypted BLOB affinity"
    );

    // PR9b LOW (iii): the encrypted-column VALUE-COPY round-trips a REAL blob. The rebuild
    // copies `secret` across the new table via `INSERT … SELECT`; the seeded ciphertext
    // bytes must survive BYTE-IDENTICAL (no truncation, no affinity coercion, no re-tag).
    // Pre-fix this was vacuous (the seed left `secret` NULL); now it proves the rebuild
    // does not corrupt an encrypted blob.
    let post_secret_hex = be
        .actor()
        .query("SELECT lower(hex(secret)) FROM main.users WHERE id='u1'")
        .await
        .expect("read post-rebuild secret hex")[0][0]
        .clone()
        .expect("post-rebuild secret must still be non-null (the value-copy preserved it)");
    assert_eq!(
        post_secret_hex, seeded_secret_hex,
        "the rebuild's value-copy preserved the encrypted `secret` blob byte-identically \
         (pre={seeded_secret_hex}, post={post_secret_hex})"
    );

    // The journal records the rebuild migration version.
    let applied = be.actor().query(
        "SELECT version FROM _mig.schema_migrations WHERE phase='completed'",
    )
    .await
    .unwrap_or_default();
    let versions: Vec<String> = applied.iter().filter_map(|r| r[0].clone()).collect();
    assert!(
        versions.iter().any(|v| out.applied.contains(v)),
        "the rebuild migration version is journaled completed: journal={versions:?} applied={:?}",
        out.applied
    );
}

// REGRESSION (PR9b LOW): the catalog entry rebuilds the new table's CREATE from the
// descriptor-sourced `to` field, while the value-copy carries the live `from` bytes
// across un-transformed. A rename PRESERVES facets by contract — so a descriptor whose
// `to` field DIVERGES in affinity from the live `from` (e.g. a rename bundled with an
// encryption/affinity change in the SAME descriptor) must FAIL CLOSED, not silently
// rebuild the column under a different affinity with the old bytes copied across.
//
// Here deploy #1 creates `users(name TEXT)`; deploy #2 renames `name → full_name`, but
// the post-rename descriptor declares `full_name` as an ENCRYPTED field (BLOB affinity).
// Pre-fix, option (2) accepted the descriptor `to` Value as-is and rebuilt `full_name`
// with BLOB affinity while value-copying the old TEXT bytes — a silent shape skew. The
// fix asserts the descriptor `to` affinity equals the live `from` affinity and refuses.
// Fails RED pre-fix (the rebuild ran with the divergent affinity).
#[compio::test]
async fn catalog_entry_fails_closed_when_descriptor_to_affinity_diverges_from_live_from() {
    let p = paths("catalog_affinity_skew");
    let be = backend(&p);

    // Deploy #1: createTable users(name TEXT). Seed a row.
    let v1 = [descriptor("users", vec![text_field("name")])];
    let create = r#"{"ir_version":1,"name":"create_users","ops":[
        {"op":"createTable","name":"users","columns":[
            {"name":"name","type":"text","nullable":false}
        ]}
    ]}"#;
    write_ir(&p, "0001_create_users.ir.json", create);
    apply_bundle_ir_sqlite(
        &be, PROJECT, APP, &v1, &p.migrations, &exec_cfg(),
        &GuardConfig::confined(PROJECT), &PolicyProfile::confined(), Approval::None,
    )
    .await
    .expect("createTable users(name TEXT) must succeed");
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec(
            "INSERT INTO main.users (id, created_at, updated_at, version, name) \
             VALUES ('u1','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z',1,'Ada')",
        )
        .await
        .expect("seed");

    // Deploy #2: renameColumn name → full_name, BUT the post-rename descriptor declares
    // `full_name` as an ENCRYPTED field (BLOB affinity) — diverging from the live `name`
    // (TEXT affinity). The descriptor-sourced `to` CREATE would render BLOB while the
    // value-copy carries the old TEXT bytes: a silent affinity skew. Must FAIL CLOSED.
    clear_migrations(&p);
    let rename = r#"{"ir_version":1,"name":"rename_name","ops":[
        {"op":"renameColumn","table":"users","from":"name","to":"full_name","type":"text"}
    ]}"#;
    write_ir(&p, "0002_rename_name.ir.json", rename);
    let divergent_desired =
        [descriptor("users", vec![encrypted_field("full_name")])];

    let err = apply_bundle_ir_sqlite_catalog(
        &be, PROJECT, APP, &divergent_desired, &p.migrations, &exec_cfg(),
        &GuardConfig::confined(PROJECT), &PolicyProfile::confined(), Approval::Approved,
    )
    .await
    .expect_err(
        "a rename whose descriptor `to` affinity diverges from the live `from` affinity \
         must FAIL CLOSED rather than silently rebuild under a different affinity",
    );
    assert!(
        matches!(err, SqliteIrApplyError::Ir { .. }),
        "expected a fail-closed IR lower error on the affinity divergence, got {err:?}"
    );
    // The refusal names a type mismatch (the same equality the snapshot-path
    // RenameHintTypeMismatch guard enforces).
    assert!(
        err.to_string().to_lowercase().contains("type")
            || err.to_string().to_lowercase().contains("affinit"),
        "the fail-closed error should describe a type/affinity mismatch, got: {err}"
    );

    // NO DATA LOSS / NO SKEW: the live table is untouched — still `name` TEXT, row intact.
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    let info = be.actor().query("PRAGMA main.table_info(users)").await.expect("table_info");
    assert!(
        info.iter().any(|r| r[1].as_deref() == Some("name")),
        "the original `name` column is intact (no rebuild ran)"
    );
    assert!(
        info.iter().all(|r| r[1].as_deref() != Some("full_name")),
        "no `full_name` column was created (the rebuild was refused)"
    );
    let vals = be
        .actor()
        .query("SELECT name FROM main.users ORDER BY id")
        .await
        .expect("read name");
    assert_eq!(
        vals.iter().filter_map(|r| r[0].clone()).collect::<Vec<_>>(),
        vec!["Ada"],
        "the seeded row is intact — fail-closed means zero data loss / no affinity skew"
    );
}

// REGRESSION (PR9c LOW (ii)) — affinity equality is NOT enough: a SAME-affinity
// data-transforming facet change on the renamed column must ALSO fail closed. Deploy #1
// creates `users(name TEXT)`; deploy #2 renames `name → full_name` but the post-rename
// descriptor declares `full_name` with a `default` (still TEXT affinity, so the PR9b
// affinity guard PASSES). The rebuild would render the new CREATE with that facet while
// value-copying the old un-defaulted bytes — a facet the verbatim copy cannot certify
// the live `from` already carried. The PR9c full-facet guard refuses with
// `RenameHintFacetMismatch`. Pre-fix (affinity-only) this PASSED and rebuilt silently —
// so this test FAILS RED pre-fix (the rebuild ran; no error).
#[compio::test]
async fn catalog_entry_fails_closed_on_same_affinity_facet_change_on_renamed_column() {
    let p = paths("catalog_facet_skew");
    let be = backend(&p);

    // Deploy #1: createTable users(name TEXT). Seed a row.
    let v1 = [descriptor("users", vec![text_field("name")])];
    let create = r#"{"ir_version":1,"name":"create_users","ops":[
        {"op":"createTable","name":"users","columns":[
            {"name":"name","type":"text","nullable":false}
        ]}
    ]}"#;
    write_ir(&p, "0001_create_users.ir.json", create);
    apply_bundle_ir_sqlite(
        &be, PROJECT, APP, &v1, &p.migrations, &exec_cfg(),
        &GuardConfig::confined(PROJECT), &PolicyProfile::confined(), Approval::None,
    )
    .await
    .expect("createTable users(name TEXT) must succeed");
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec(
            "INSERT INTO main.users (id, created_at, updated_at, version, name) \
             VALUES ('u1','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z',1,'Ada')",
        )
        .await
        .expect("seed");

    // Deploy #2: renameColumn name → full_name, descriptor declares `full_name` with a
    // `default` (TEXT affinity, same as live `name` — affinity guard passes). Must FAIL
    // CLOSED on the facet change.
    clear_migrations(&p);
    let rename = r#"{"ir_version":1,"name":"rename_name","ops":[
        {"op":"renameColumn","table":"users","from":"name","to":"full_name","type":"text"}
    ]}"#;
    write_ir(&p, "0002_rename_name.ir.json", rename);
    let facet_changed_desired = [descriptor("users", vec![defaulted_text_field("full_name")])];

    let err = apply_bundle_ir_sqlite_catalog(
        &be, PROJECT, APP, &facet_changed_desired, &p.migrations, &exec_cfg(),
        &GuardConfig::confined(PROJECT), &PolicyProfile::confined(), Approval::Approved,
    )
    .await
    .expect_err(
        "a rename bundled with a same-affinity facet change (a `default` on the renamed \
         column) must FAIL CLOSED — the rebuild cannot certify the verbatim-copied bytes \
         carried that facet",
    );
    assert!(
        matches!(err, SqliteIrApplyError::Ir { .. }),
        "expected a fail-closed IR lower error on the facet change, got {err:?}"
    );
    assert!(
        err.to_string().to_lowercase().contains("facet")
            || err.to_string().to_lowercase().contains("default"),
        "the fail-closed error should describe the data-transforming facet, got: {err}"
    );

    // NO DATA LOSS / NO SKEW: the live table is untouched — still `name` TEXT, row intact.
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    let info = be.actor().query("PRAGMA main.table_info(users)").await.expect("table_info");
    assert!(
        info.iter().any(|r| r[1].as_deref() == Some("name")),
        "the original `name` column is intact (no rebuild ran)"
    );
    assert!(
        info.iter().all(|r| r[1].as_deref() != Some("full_name")),
        "no `full_name` column was created (the rebuild was refused)"
    );
}

// The genuinely-unsourceable case STILL fails closed through the catalog entry: a
// rename whose `from` column is absent from the LIVE catalog (a post-rename live DB)
// cannot be sourced from any read, so the rebuild author refuses — no wrong rebuild,
// no data loss. This keeps the catalog entry honest (it is not a blanket "always
// succeed" that the post-rename descriptor fail-closed test pins for the OTHER entry).
#[compio::test]
async fn catalog_entry_still_fails_closed_when_from_column_absent_from_live() {
    let p = paths("catalog_failclosed");
    let be = backend(&p);

    // Deploy #1: createTable users(full_name) — the live DB is ALREADY post-rename
    // (the `name` column never existed). Seed a row.
    let v1 = [descriptor("users", vec![text_field("full_name")])];
    let create = r#"{"ir_version":1,"name":"create_users","ops":[
        {"op":"createTable","name":"users","columns":[
            {"name":"full_name","type":"text","nullable":false}
        ]}
    ]}"#;
    write_ir(&p, "0001_create_users.ir.json", create);
    apply_bundle_ir_sqlite(
        &be, PROJECT, APP, &v1, &p.migrations, &exec_cfg(),
        &GuardConfig::confined(PROJECT), &PolicyProfile::confined(), Approval::None,
    )
    .await
    .expect("createTable users(full_name) must succeed");
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec(
            "INSERT INTO main.users (id, created_at, updated_at, version, full_name) \
             VALUES ('u1','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z',1,'Ada')",
        )
        .await
        .expect("seed");

    // Deploy #2: renameColumn name → full_name, but the LIVE DB has no `name` column
    // (it is already post-rename). The catalog read cannot source the `from` facts ⇒
    // FAIL CLOSED, no DDL, no data loss.
    clear_migrations(&p);
    let rename = r#"{"ir_version":1,"name":"rename_name","ops":[
        {"op":"renameColumn","table":"users","from":"name","to":"full_name","type":"text"}
    ]}"#;
    write_ir(&p, "0002_rename_name.ir.json", rename);
    let desired = [descriptor("users", vec![text_field("full_name")])];
    let err = apply_bundle_ir_sqlite_catalog(
        &be, PROJECT, APP, &desired, &p.migrations, &exec_cfg(),
        &GuardConfig::confined(PROJECT), &PolicyProfile::confined(), Approval::Approved,
    )
    .await
    .expect_err(
        "a rename whose `from` column is absent from the live catalog must FAIL CLOSED",
    );
    assert!(
        matches!(err, SqliteIrApplyError::Ir { .. }),
        "expected a fail-closed IR lower error (the live `from` column is absent from the \
         catalog), got {err:?}"
    );

    // NO DATA LOSS: the live table is untouched — still full_name, row intact.
    let vals = be
        .actor()
        .query("SELECT full_name FROM main.users ORDER BY id")
        .await
        .expect("read full_name");
    assert_eq!(
        vals.iter().filter_map(|r| r[0].clone()).collect::<Vec<_>>(),
        vec!["Ada"],
        "the seeded row is intact — fail-closed means zero data loss"
    );
}
