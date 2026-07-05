//! **Migration-first P1 — the ROUND-TRIP ORACLE for `fold_ops` on real Postgres
//! (`:5440`).**
//!
//! This is the headline correctness net for the offline ops→snapshot fold. It
//! proves `fold_ops(ops) == snapshot_schema(live_after_apply)` — i.e. the fold
//! agrees with reality:
//!
//!   1. APPLY a representative `Op` corpus to a fresh per-test schema through the
//!      REAL pipeline (`load_ir_document` → `IrAuthor::lower_plan` →
//!      `MigrationEngine::apply_plan` under the least-priv migrator role). The
//!      corpus is applied doc-by-doc, re-introspecting the live schema before each
//!      doc so the live facts (`LiveSchema`) the lower consumes (FK inlining,
//!      rename type reconciliation, dropIndex gating) are AUTHORITATIVE — exactly
//!      what the deploy path does.
//!   2. INTROSPECT the final live schema via `snapshot_schema`.
//!   3. FOLD the SAME ordered op stream offline via `fold_ops` (NO DB).
//!   4. ASSERT structural equality (after canonicalizing introspection-only noise).
//!
//! Because the engine APPLIES the same ops the fold replays, and both routes share
//! the `build_table_snapshot` snapshot-builder, this is the load-bearing proof that
//! the fold matches what the differ + introspection produce.
//!
//! # What this oracle does NOT cover (mig-first P1 critique, Finding #3)
//!
//! `ColumnSnapshot`'s `Eq` compares only `name` / `data_type` / `nullable`. The
//! EMISSION-ONLY fields — `default`, `encryption_sentinel`, `comment_sentinel` —
//! are deliberately EXCLUDED from equality (introspection always leaves them
//! `None`; only the desired/folded snapshot populates them). So this round-trip
//! STRUCTURALLY CANNOT validate the fold's emission metadata — the exact fields P2
//! `gen-types` reads to drive AEAD encrypt/decrypt + the column `DEFAULT` clause.
//! Those are validated by the NO-DB unit goldens in `src/fold.rs`
//! (`fold_emission_metadata_matches_builder_golden`,
//! `fold_add_column_emission_metadata_matches_builder_golden`), which assert the
//! fold's emitted `default` / sentinels match `build_table_snapshot` DIRECTLY (not
//! via this Eq-blind oracle). The fold's fail-closed refusal of an encryption-
//! contract-changing `setColumnType` (the apply path cannot re-stamp the
//! sentinel) is likewise a `src/fold.rs` unit concern.
//!
//! Requires `:5440` (the `*_pg` suite convention) + `MIGRATE_REQUIRE_DB=1`; run
//! with `--test-threads=1`. This crate is the ONLY PG-test runner active.

use compio_postgres::Client;
use zeroship_migrate::{
    apply::drift::snapshot_schema,
    apply::executor::LockMode,
    fold_ops,
    model::ir::Op,
    provision_migrator,
    apply::role::deprovision_migrator,
    Approval, ExecutorConfig, IrAuthor, LiveSchema, MigrationEngine, PostgresBackend, SchemaSnapshot,
    SqlDialect,
};

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

fn dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

/// Honor the `*_pg` skip-or-fail convention: when `MIGRATE_REQUIRE_DB` is unset
/// the suite SKIPS (returns `true`); when set it must run (a connect failure is a
/// hard error, never a silent skip).
fn require_db() -> bool {
    std::env::var("MIGRATE_REQUIRE_DB").is_ok()
}

async fn pg() -> Client {
    let (client, conn) = compio_postgres::connect(&dsn(), compio_postgres::NoTls)
        .await
        .expect("connect to zeroship_migrate_test on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}_{n}")
}

const APP: &str = "app_test";

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.pg.meta_schema = format!("meta_{tok}");
    let role = zeroship_migrate::migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}

async fn setup(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{}\"", cfg.project_schema))
        .await
        .expect("create project schema");
    provision_migrator(conn, cfg).await.expect("provision migrator role");
}

async fn teardown(conn: &Client, cfg: &ExecutorConfig) {
    let _ = deprovision_migrator(conn, cfg).await;
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

fn registry(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
    pairs.iter().map(|(t, o)| (t.to_string(), o.to_string())).collect()
}

/// Build the `LiveSchema` the lower consumes from a real introspected snapshot:
/// the live table set (FK inlining), the unique-index names (dropIndex gate), the
/// per-table snapshots + ownership (rename type reconciliation). This mirrors the
/// deploy path's authoritative pre-deploy introspection.
fn live_from_snapshot(snap: &SchemaSnapshot, owner: &str) -> LiveSchema {
    let mut live = LiveSchema::from_tables(snap.tables.keys().cloned().collect());
    live.unique_indexes = snap
        .tables
        .values()
        .flat_map(|t| t.indexes.iter())
        .filter(|i| i.unique)
        .map(|i| i.name.clone())
        .collect();
    live.table_snapshots = snap.tables.clone();
    live.partitions = snap.partitions.clone();
    live.table_ownership = snap.tables.keys().map(|t| (t.clone(), owner.to_string())).collect();
    live
}

/// Apply one IR document through the REAL pipeline against the current live state,
/// returning the document's ordered `Op` list (so the caller accumulates the full
/// stream the fold replays). The live facts come from a fresh introspection so the
/// lower sees the authoritative pre-apply schema (FK inline, rename reconcile).
async fn apply_doc(
    conn: &Client,
    cfg: &ExecutorConfig,
    ir: &str,
    reg: &std::collections::BTreeMap<String, String>,
    approval: Approval,
) -> Vec<Op> {
    let live_snap = snapshot_schema(conn, &cfg.project_schema)
        .await
        .expect("introspect live schema before applying the doc");
    let live = live_from_snapshot(&live_snap, APP);

    let author = IrAuthor::new(cfg.project_schema.clone(), APP, SqlDialect::Postgres);
    let document = zeroship_migrate::model::load::load_ir_document(
        ir,
        APP,
        zeroship_migrate::model::validate::Dialect::Postgres,
        reg,
        None,
        None,
    )
    .expect("load gate");
    let ops = document.ops.clone();
    let plan = author.lower_plan(&document, &live).expect("lower the doc plan on PG");
    let engine = MigrationEngine::new();
    engine
        .apply_plan(
            &plan.steps,
            approval,
            &PostgresBackend::new(conn),
            cfg,
            APP,
            LockMode::Acquire,
        )
        .await
        .expect("apply the authored plan on PG");
    ops
}

/// Canonicalize introspection-only noise before the structural compare. The ONLY
/// field that can phantom-diff between the fold and live is a CHECK constraint's
/// `definition` (`pg_get_constraintdef` normalizes a CHECK body, e.g. `CHECK
/// ((age > 0))`), and `ConstraintSnapshot` has full `Eq` INCLUDING `definition`.
/// The P1 corpus carries no stand-alone CHECK (validate rejects it today), but a
/// per-field min/max/enum CHECK from a `createTable` would. Blank the CHECK
/// `definition` on BOTH sides so a CHECK compares on name+kind only (the cleaner
/// stance — `definition` is introspection-noise for CHECKs). Every other field
/// (FK definition via the proven `fk_definition_pg`, UNIQUE/PK bodies) compares
/// byte-for-byte.
fn canonicalize(mut snap: SchemaSnapshot) -> SchemaSnapshot {
    snap.roles.clear();
    snap.schemas.clear();
    snap.extensions.clear();
    for t in snap.tables.values_mut() {
        for c in &mut t.constraints {
            if c.kind == "CHECK" {
                c.definition = String::new();
            }
        }
    }
    snap
}

/// THE ORACLE — apply a representative corpus on real PG, introspect, fold the
/// SAME ops offline, assert structural equality.
#[compio::test]
async fn fold_equals_introspect_pg() {
    if !require_db() {
        eprintln!("SKIP fold_equals_introspect_pg (MIGRATE_REQUIRE_DB unset)");
        return;
    }
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let mut all_ops: Vec<Op> = Vec::new();

    // (1) The FK-target table.
    let parents = r#"{"ir_version":1,"name":"create_parents","ops":[
        {"op":"createTable","name":"parents","columns":[
            {"name":"label","type":"text","nullable":false}
        ]}
    ]}"#;
    all_ops.extend(apply_doc(&conn, &cfg, parents, &registry(&[]), Approval::None).await);

    // (1b) A neutral enum definition plus an enum-typed column. PG materializes the
    //      enum as a user-defined type, so this pins the fold's canonical data_type
    //      against live `format_type(...)` introspection.
    let plans = r#"{"ir_version":1,"name":"create_plan_enum","ops":[
        {"op":"createEnum","name":"plan_tier","values":["free","pro"]},
        {"op":"createTable","name":"plans","columns":[
            {"name":"tier","type":{"enum":{"name":"plan_tier"}},"nullable":false}
        ]}
    ]}"#;
    all_ops.extend(apply_doc(&conn, &cfg, plans, &registry(&[]), Approval::None).await);

    // (2) `users` with varied column types + a `ref parents` + an encrypted
    //     column, a TABLE-LEVEL UNIQUE, a single-`id` FK to parents, and an extra
    //     index. (A `vector(N)` / `geoPoint` column needs the pgvector / PostGIS
    //     extension, which is not guaranteed in the bare test DB; its `vector(N)`
    //     spelling round-trip is differ-proven + fold-unit-covered. The encrypted
    //     column exercises the BYTEA + `zsenc` sentinel path the shared builder
    //     stamps — the sentinel is emission-only / excluded from snapshot equality.)
    let users = r#"{"ir_version":1,"name":"create_users","ops":[
        {"op":"createTable","name":"users","columns":[
            {"name":"email","type":"text","nullable":false},
            {"name":"age","type":"int","nullable":true},
            {"name":"score","type":"double","nullable":true},
            {"name":"active","type":"boolean","nullable":false},
            {"name":"meta","type":"json","nullable":true},
            {"name":"joined","type":"timestamp","nullable":true},
            {"name":"parent_id","type":{"ref":{"references":"parents"}},"nullable":true},
            {"name":"secret","type":{"encrypted":{"of":"text"}},"nullable":true}
        ],
        "constraints":[
            {"name":"users_email_uq","kind":{"kind":"unique","columns":["email"]}},
            {"name":"users_parent_fk","kind":{"kind":"fk","columns":["parent_id"],
                "referencesTable":"parents","referencesColumns":["id"],"onDelete":"cascade"}}
        ],
        "indexes":[
            {"name":"users_age_x_idx","columns":[{"kind":"column","name":"age"}]}
        ]}
    ]}"#;
    all_ops.extend(
        apply_doc(&conn, &cfg, users, &registry(&[("parents", APP)]), Approval::None).await,
    );

    // (2b) `notes` with an FK to parents whose onDelete is SET NULL + onUpdate is
    //      RESTRICT — exercises the non-cascade referential-action spellings the
    //      shared `fk_definition_pg` must render byte-identically on both sides
    //      (step 2's FK is onDelete CASCADE only; this covers the SET NULL / RESTRICT
    //      arms `pg_get_constraintdef` reports). SET NULL requires the local column
    //      be nullable, so `parent_id` is nullable.
    let notes = r#"{"ir_version":1,"name":"create_notes","ops":[
        {"op":"createTable","name":"notes","columns":[
            {"name":"body","type":"text","nullable":false},
            {"name":"parent_id","type":{"ref":{"references":"parents"}},"nullable":true}
        ],
        "constraints":[
            {"name":"notes_parent_fk","kind":{"kind":"fk","columns":["parent_id"],
                "referencesTable":"parents","referencesColumns":["id"],
                "onDelete":"setNull","onUpdate":"restrict"}}
        ]}
    ]}"#;
    all_ops.extend(
        apply_doc(&conn, &cfg, notes, &registry(&[("parents", APP)]), Approval::None).await,
    );

    // The full project registry for the remaining docs (every created table is
    // owned by APP — subsequent ops that target `users`/`parents`/`notes` must see
    // it).
    let full = registry(&[("parents", APP), ("users", APP), ("notes", APP)]);

    // (3) addColumn (nullable + not-null).
    let add_cols = r#"{"ir_version":1,"name":"add_cols","ops":[
        {"op":"addColumn","table":"users","column":"nickname","type":"text","nullable":true},
        {"op":"addColumn","table":"users","column":"rank","type":"int","nullable":false,
            "default":{"literal":{"value":0}}}
    ]}"#;
    all_ops.extend(apply_doc(&conn, &cfg, add_cols, &full, Approval::None).await);

    // (4) dropColumn.
    let drop_col = r#"{"ir_version":1,"name":"drop_col","ops":[
        {"op":"dropColumn","table":"users","column":"rank"}
    ]}"#;
    all_ops.extend(apply_doc(&conn, &cfg, drop_col, &full, Approval::Approved).await);

    // NOTE on renameColumn: the IR `renameColumn` lowers to an ONLINE expand-
    //   contract on PG (E1 ADD <to> / backfill / E3) whose CONTRACT (drop the `from`
    //   column) is a SEPARATE later pending-contract deploy. So a single-deploy live
    //   DB still carries BOTH the `from` and `to` columns, whereas `fold_ops`
    //   collapses a rename to the final single column by design (the spec's "both
    //   the online + plain cases collapse to the final column name"). A round-trip
    //   over a mid-expand live state therefore cannot match the fold by construction
    //   — that is a property of the two-phase online rename, not a fold bug. The
    //   rename fold semantics (collapse-to-final-name, type/nullability preserved,
    //   to-collision error) are covered by the `fold.rs` unit tests. The oracle
    //   stays on the SINGLE-deploy ops whose live end-state the fold reproduces.

    // (5) setColumnType + setColumnNotNull (PG-only).
    let alters = r#"{"ir_version":1,"name":"alters","ops":[
        {"op":"setColumnType","table":"users","column":"age","toType":"bigInt"},
        {"op":"setColumnNotNull","table":"users","column":"nickname"}
    ]}"#;
    all_ops.extend(apply_doc(&conn, &cfg, alters, &full, Approval::Approved).await);

    // (6) addConstraint UNIQUE(nickname) — exercises the UNIQUE constraint + its
    //     IMPLICIT unique index the fold must mirror — then dropConstraint the
    //     createTable UNIQUE (which also drops its implicit index).
    let add_uq = r#"{"ir_version":1,"name":"add_uq","ops":[
        {"op":"addConstraint","table":"users",
            "constraint":{"name":"users_nick_uq","kind":{"kind":"unique","columns":["nickname"]}}}
    ]}"#;
    all_ops.extend(apply_doc(&conn, &cfg, add_uq, &full, Approval::Approved).await);

    let drop_con = r#"{"ir_version":1,"name":"drop_con","ops":[
        {"op":"dropConstraint","table":"users","name":"users_email_uq"}
    ]}"#;
    all_ops.extend(apply_doc(&conn, &cfg, drop_con, &full, Approval::Approved).await);

    // (6b) A STANDALONE addConstraint(UNIQUE) → dropConstraint pair (distinct from
    //      step 6, which drops a createTable-LEVEL UNIQUE). This proves a unique
    //      constraint that was ADDED post-create (so its implicit index entered the
    //      fold via the addConstraint arm, not the createTable arm) folds away to
    //      base — both the constraint AND its implicit index — matching introspection
    //      after the DROP CONSTRAINT also drops PG's implicit index.
    let add_uq2 = r#"{"ir_version":1,"name":"add_uq2","ops":[
        {"op":"addConstraint","table":"users",
            "constraint":{"name":"users_email_uq2","kind":{"kind":"unique","columns":["email"]}}}
    ]}"#;
    all_ops.extend(apply_doc(&conn, &cfg, add_uq2, &full, Approval::Approved).await);

    let drop_uq2 = r#"{"ir_version":1,"name":"drop_uq2","ops":[
        {"op":"dropConstraint","table":"users","name":"users_email_uq2"}
    ]}"#;
    all_ops.extend(apply_doc(&conn, &cfg, drop_uq2, &full, Approval::Approved).await);

    // (7) createIndex, then dropIndex it.
    let mk_idx = r#"{"ir_version":1,"name":"mk_idx","ops":[
        {"op":"createIndex","table":"users","columns":[{"kind":"column","name":"nickname"}],"name":"users_nick_idx"}
    ]}"#;
    all_ops.extend(apply_doc(&conn, &cfg, mk_idx, &full, Approval::None).await);

    let drop_idx = r#"{"ir_version":1,"name":"drop_idx","ops":[
        {"op":"dropIndex","name":"users_nick_idx","table":"users"}
    ]}"#;
    all_ops.extend(apply_doc(&conn, &cfg, drop_idx, &full, Approval::None).await);

    // (7b) dropTable(notes) → recreate `notes` with a DIFFERENT shape (no FK, new
    //      columns). Proves the fold drops the old table state ENTIRELY (its FK
    //      constraint + the implicit FK metadata) and rebuilds from the new
    //      descriptor — matching introspection of the recreated table, not a merge of
    //      old+new. dropTable needs `Approved` (destructive); the recreated `notes`
    //      is still APP-owned and already in `full`.
    let drop_notes = r#"{"ir_version":1,"name":"drop_notes","ops":[
        {"op":"dropTable","table":"notes"}
    ]}"#;
    all_ops.extend(apply_doc(&conn, &cfg, drop_notes, &full, Approval::Approved).await);

    let recreate_notes = r#"{"ir_version":1,"name":"recreate_notes","ops":[
        {"op":"createTable","name":"notes","columns":[
            {"name":"title","type":"text","nullable":false},
            {"name":"pinned","type":"boolean","nullable":false}
        ]}
    ]}"#;
    all_ops.extend(apply_doc(&conn, &cfg, recreate_notes, &full, Approval::None).await);

    // (8) DML — Insert + Update — to prove they are schema no-ops (the folded schema
    //     is unchanged by them). They apply cleanly and add nothing structural. The
    //     insert supplies the NOT-NULL columns (`id`/`created_at`/`updated_at`/
    //     `version` system fields + the now-NOT-NULL `nickname`) explicitly — the
    //     engine DML path does not auto-fill them (that is plugin-db's runtime job),
    //     so a faithful apply provides them.
    let dml = r#"{"ir_version":1,"name":"dml","ops":[
        {"op":"insert","table":"users",
            "columns":["id","created_at","updated_at","version","email","active","nickname"],
            "rows":[["usr_1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,"a@example.com",true,"alice"]]},
        {"op":"update","table":"users","set":{"active":{"node":"literal","value":false}},
            "where":{"node":"binOp","op":"eq","lhs":{"node":"colRef","name":"email"},
                "rhs":{"node":"literal","value":"a@example.com"}}}
    ]}"#;
    all_ops.extend(apply_doc(&conn, &cfg, dml, &full, Approval::None).await);

    // INTROSPECT the final live schema.
    let live = snapshot_schema(&conn, &cfg.project_schema)
        .await
        .expect("introspect final live schema");

    // FOLD the SAME ordered op stream offline (no DB).
    let folded = fold_ops(&all_ops, SqlDialect::Postgres, &cfg.project_schema)
        .expect("fold the corpus offline");

    // ASSERT structural equality (CHECK-definition noise canonicalized — the corpus
    // carries none, so this is a no-op here, but it keeps the oracle robust).
    assert_eq!(
        canonicalize(folded),
        canonicalize(live),
        "fold_ops(corpus) must equal the live introspected snapshot"
    );

    teardown(&conn, &cfg).await;
}

/// **Migration-first P2a — the fold-and-RECOVER seam over REAL PG.** Apply a
/// createTable that authors an explicit `t.id({ prefix })` PK (the §2b
/// declared-only facet), then assert:
///   (1) `fold_ops` STILL agrees with live introspection (the `id`→descriptor-`"id"`
///       mapping FOLDS into the system PK — it does NOT create a phantom second
///       column, so the structural round-trip stays clean on real PG); and
///   (2) `fold_to_field_defs` RECOVERS the prefix from the applied shape (the §2b
///       carried IR field).
///
/// This is the faithful (real-runtime + real-PG) regression for the P2a `id_prefix`
/// carry: it is RED pre-change — the carried IR field did not exist, and the
/// explicit `id` createTable would have been REJECTED as a second `id` column by
/// the shared builder before the §2b descriptor mapping. (The CHECK-lift
/// enum/min/max recovery is covered by the NO-DB fold unit tests: a CHECK
/// constraint's Expr→SQL lowering is a deferred wave, so a CHECK-bearing createTable
/// cannot yet be APPLIED through the real PG pipeline.)
#[compio::test]
async fn fold_recovers_facets_pg() {
    if !require_db() {
        eprintln!("SKIP fold_recovers_facets_pg (MIGRATE_REQUIRE_DB unset)");
        return;
    }
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    // A createTable authoring an explicit `t.id({prefix})` PK. (No vector column —
    // pgvector is not guaranteed in the bare test DB; the vector_metric recovery is
    // covered by the NO-DB fold unit test.)
    let posts = r#"{"ir_version":1,"name":"create_posts","ops":[
        {"op":"createTable","name":"posts","columns":[
            {"name":"id","type":"uuid","nullable":false,"idPrefix":"post"},
            {"name":"title","type":"text","nullable":false}
        ]}
    ]}"#;
    let ops = apply_doc(&conn, &cfg, posts, &registry(&[]), Approval::None).await;

    // (1) `fold_ops` still agrees with live introspection — the explicit `id`
    //     createTable FOLDED into the system PK (no phantom second column).
    let live = snapshot_schema(&conn, &cfg.project_schema)
        .await
        .expect("introspect posts");
    let folded =
        fold_ops(&ops, SqlDialect::Postgres, &cfg.project_schema).expect("fold posts offline");
    // The live table really did get a single `id` PK column (not two) — checked
    // BEFORE the canonicalize move consumes `live`.
    let id_cols = live.tables["posts"].columns.iter().filter(|c| c.name == "id").count();
    assert_eq!(id_cols, 1, "exactly ONE id column (the prefix folds into the system PK)");
    assert_eq!(
        canonicalize(folded),
        canonicalize(live),
        "an explicit t.id({{prefix}}) createTable folds into the system PK and round-trips"
    );

    // (2) The recovery seam lifts the declared-only prefix from the applied shape.
    let defs = zeroship_migrate::fold_to_field_defs(&ops, SqlDialect::Postgres, &cfg.project_schema)
        .expect("reconstruct posts FieldDefs");
    let posts_def = &defs["posts"];
    assert_eq!(
        posts_def["id"].get("idPrefix").and_then(|v| v.as_str()),
        Some("post"),
        "the typed-id prefix is recovered onto the reconstructed FieldDef: {posts_def}"
    );

    teardown(&conn, &cfg).await;
}

/// **op.* table-rename follow-up — the REAL-PG round-trip for `Op::RenameTable`.**
/// A whole-table rename is a SINGLE-deploy fast catalog ALTER (NOT the online
/// column expand-contract), so — unlike `renameColumn` — its live end-state is
/// EXACTLY what the fold reproduces. Apply a `createTable` then a `renameTable`
/// through the REAL pipeline against `:5440`, then assert:
///   (1) the table is renamed in the LIVE catalog (present under the new name,
///       absent under the old), carrying its columns + index; AND
///   (2) `fold_ops(create + rename) == snapshot_schema(live)`.
///
/// RED before this change: `Op::RenameTable` did not exist (the IR could not carry
/// the op, the recorder had no `table().rename()`, and the lower/fold had no arm),
/// so the doc would fail to load / lower and the fold would have no rename to replay.
#[compio::test]
async fn fold_equals_introspect_after_rename_table_pg() {
    if !require_db() {
        eprintln!("SKIP fold_equals_introspect_after_rename_table_pg (MIGRATE_REQUIRE_DB unset)");
        return;
    }
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let mut all_ops: Vec<Op> = Vec::new();

    // (1) Create `accounts` with a couple columns + a secondary index.
    let create = r#"{"ir_version":1,"name":"create_accounts","ops":[
        {"op":"createTable","name":"accounts","columns":[
            {"name":"email","type":"text","nullable":false},
            {"name":"balance","type":"int","nullable":true}
        ],
        "indexes":[
            {"name":"accounts_email_idx","columns":[{"kind":"column","name":"email"}]}
        ]}
    ]}"#;
    all_ops.extend(apply_doc(&conn, &cfg, create, &registry(&[]), Approval::None).await);

    // (2) Rename `accounts` → `members`. A table rename is `requires_approval`
    //     (backward-incompatible), so it applies under `Approval::Approved`. The
    //     registry still owns the SOURCE name (`accounts`) — the ownership gate
    //     checks the existing table.
    let rename = r#"{"ir_version":1,"name":"rename_accounts","ops":[
        {"op":"renameTable","table":"accounts","to":"members"}
    ]}"#;
    all_ops.extend(
        apply_doc(&conn, &cfg, rename, &registry(&[("accounts", APP)]), Approval::Approved).await,
    );

    // INTROSPECT the live schema after the rename.
    let live = snapshot_schema(&conn, &cfg.project_schema)
        .await
        .expect("introspect after rename");

    // (1) The catalog really renamed the table: `members` present, `accounts` gone,
    //     with the columns + index carried across the rename.
    assert!(live.tables.contains_key("members"), "renamed table present under the NEW name");
    assert!(!live.tables.contains_key("accounts"), "OLD table name is gone in the live catalog");
    let members = &live.tables["members"];
    assert!(members.columns.iter().any(|c| c.name == "email"), "columns carried across the rename");
    assert!(members.columns.iter().any(|c| c.name == "balance"), "all columns carried");
    assert!(
        members.indexes.iter().any(|i| i.name == "accounts_email_idx"),
        "the secondary index survives the table rename (PG keeps index names across RENAME)"
    );

    // (2) The offline fold of the SAME op stream equals the live introspected snapshot.
    let folded = fold_ops(&all_ops, SqlDialect::Postgres, &cfg.project_schema)
        .expect("fold the create+rename offline");
    assert_eq!(
        canonicalize(folded),
        canonicalize(live),
        "fold_ops(create + renameTable) must equal the live introspected snapshot"
    );

    teardown(&conn, &cfg).await;
}

/// **op.* table-rename follow-up — INCOMING cross-table FK references must follow
/// the rename in the fold (review HIGH).**
///
/// When a table with an INCOMING FK is renamed, live PG re-renders every
/// referencing FK via `pg_get_constraintdef(oid)`, which resolves the target by
/// OID and reports the CURRENT (new) name. The offline fold must mirror that: a
/// renameTable has to rewrite OTHER tables' FK `definition` strings (and `ref`
/// column targets) from the old name to the new one, or `fold_ops` phantom-drifts
/// against live for every diff and gen-types emits a `ref` to a dead collection.
///
/// Scenario: `accounts` (FK target) + `orders` (FK `account_id → accounts`), then
/// rename `accounts → members`. Assert:
///   (1) the live `orders` FK definition now references `members` (PG followed the
///       rename by OID); AND
///   (2) `fold_ops(create accounts + create orders + rename) == live` — which is
///       FALSE pre-fix, because the folded `orders` FK still says `accounts`.
///
/// RED before the fix: the fold's RenameTable arm re-keyed only the renamed table's
/// own entry, leaving every incoming FK `definition` pointing at the old name, so
/// the canonicalized fold != live and this assert fails.
#[compio::test]
async fn fold_rewrites_incoming_fk_on_table_rename_pg() {
    if !require_db() {
        eprintln!("SKIP fold_rewrites_incoming_fk_on_table_rename_pg (MIGRATE_REQUIRE_DB unset)");
        return;
    }
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let mut all_ops: Vec<Op> = Vec::new();

    // (1) The FK-TARGET table.
    let accounts = r#"{"ir_version":1,"name":"create_accounts","ops":[
        {"op":"createTable","name":"accounts","columns":[
            {"name":"email","type":"text","nullable":false}
        ]}
    ]}"#;
    all_ops.extend(apply_doc(&conn, &cfg, accounts, &registry(&[]), Approval::None).await);

    // (2) `orders` with an INCOMING FK to `accounts` (the thing the rename must
    //     re-target). The `ref` column carries the FK target; the table-level Fk
    //     constraint carries the policy.
    let orders = r#"{"ir_version":1,"name":"create_orders","ops":[
        {"op":"createTable","name":"orders","columns":[
            {"name":"total","type":"int","nullable":false},
            {"name":"account_id","type":{"ref":{"references":"accounts"}},"nullable":true}
        ],
        "constraints":[
            {"name":"orders_account_fk","kind":{"kind":"fk","columns":["account_id"],
                "referencesTable":"accounts","referencesColumns":["id"],"onDelete":"cascade"}}
        ]}
    ]}"#;
    all_ops.extend(
        apply_doc(&conn, &cfg, orders, &registry(&[("accounts", APP)]), Approval::None).await,
    );

    // (3) Rename the FK TARGET `accounts → members`. Backward-incompatible ⇒
    //     `Approval::Approved`. The registry owns the SOURCE name.
    let rename = r#"{"ir_version":1,"name":"rename_accounts","ops":[
        {"op":"renameTable","table":"accounts","to":"members"}
    ]}"#;
    all_ops.extend(
        apply_doc(
            &conn,
            &cfg,
            rename,
            &registry(&[("accounts", APP), ("orders", APP)]),
            Approval::Approved,
        )
        .await,
    );

    let live = snapshot_schema(&conn, &cfg.project_schema)
        .await
        .expect("introspect after rename");

    // (1) Live PG followed the rename by OID: the `orders` FK now references the NEW
    //     name. (Asserted explicitly so a future PG behaviour change is loud.)
    let orders_live = &live.tables["orders"];
    let fk = orders_live
        .constraints
        .iter()
        .find(|c| c.kind == "FOREIGN KEY")
        .expect("orders carries the FK");
    assert!(
        fk.definition.contains("members") && !fk.definition.contains("accounts"),
        "live PG re-targets the incoming FK to the NEW name: {}",
        fk.definition
    );

    // (2) The offline fold of the SAME stream equals live — i.e. the renameTable arm
    //     rewrote the INCOMING FK definition from `accounts` to `members`. FALSE
    //     pre-fix (the folded `orders` FK still said `accounts`).
    let folded = fold_ops(&all_ops, SqlDialect::Postgres, &cfg.project_schema)
        .expect("fold create+create+rename offline");
    assert_eq!(
        canonicalize(folded),
        canonicalize(live),
        "fold must rewrite the INCOMING FK to the renamed table's new name"
    );

    teardown(&conn, &cfg).await;
}
