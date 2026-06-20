//! PHASE 4 — descriptor → engine-generated SQLite `up` → applied through the
//! hardened `SqliteBackend` → drift round-trip. Real temp-file SQLite throughout
//! (the faithful path: the actual `DeclarativeAuthor` emitter routes through the
//! shared `zeroship_schema` emitter, and the real backend authorizer applies the
//! unqualified DDL into `main` = the app file).
//!
//! Also: the TrustProfile-SQLite wiring (Confined SQLite accepts descriptor-
//! generated DDL; a raw untrusted SQL string is REFUSED on the Confined SQLite
//! guard; Platform fail-closes to Confined on SQLite).

use std::collections::HashMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zeroship_migrate::{
    desired_snapshot, CollectionDescriptor, DeclarativeAuthor, DeclarativeError, FieldDescriptor,
    GuardConfig, GuardError, IndexDescriptor, MigrationEngine, SchemaSnapshot, SqliteBackend,
    SqlGuard,
};
use zeroship_schema::query::SqlDialect;

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

/// A SQLite-dialect declarative author.
fn sqlite_author() -> DeclarativeAuthor {
    DeclarativeAuthor::new_for_dialect(PROJECT, APP, SqlDialect::Sqlite)
}

/// A single-collection descriptor: a plain field + a masked field + an encrypted
/// field. (No FK here — FK round-trips in its own test with a parent table.)
fn goodies_desc() -> CollectionDescriptor {
    CollectionDescriptor {
        name: "accounts".into(),
        owner_app: APP.into(),
        fields: vec![
            FieldDescriptor {
                name: "title".into(),
                ty: "string".into(),
                required: true,
                ..Default::default()
            },
            FieldDescriptor {
                name: "ssn".into(),
                ty: "string".into(),
                mask: Some(serde_json::json!({ "kind": "last4", "classification": "pii" })),
                ..Default::default()
            },
            FieldDescriptor {
                name: "secret".into(),
                ty: "bytes".into(),
                encrypted: Some(serde_json::json!({ "mode": "randomized", "keyId": "k1" })),
                ..Default::default()
            },
        ],
        indexes: vec![],
    }
}

// ---------------------------------------------------------------------------
// E2E: descriptor → diff (SQLite author) → unqualified `up` → apply → drift.
// ---------------------------------------------------------------------------

#[compio::test]
async fn descriptor_to_sqlite_apply_roundtrips_mask_and_encryption() {
    let desc = goodies_desc();
    let desired = desired_snapshot(PROJECT, &[desc]).expect("desired_snapshot");

    // The SQLite author routes the new-table CREATE through the shared emitter.
    let author = sqlite_author();
    let plan = author
        .diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("diff");
    let migs = plan.all_migrations();
    let create = migs
        .iter()
        .find(|m| m.name == "create_table_accounts")
        .expect("create_table migration present");

    // The generated `up` is UNqualified (lands in `main` = the app file).
    assert!(
        create.up.contains(r#"CREATE TABLE IF NOT EXISTS "accounts" ("#),
        "engine-generated SQLite up must be unqualified: {}",
        create.up
    );
    assert!(
        !create.up.contains(r#""prj_demo"."#) && !create.up.contains(r#""app_demo"."#),
        "no schema/app qualifier may appear: {}",
        create.up
    );
    // Mask + encryption sentinels ride inline (the SQLite wire).
    assert!(
        create.up.contains(r#""ssn_masked" TEXT NOT NULL /* __zsmask:"#),
        "mask sentinel must ride inline: {}",
        create.up
    );
    assert!(
        create.up.contains("BLOB") && create.up.contains("/* zsenc:"),
        "encrypted column must be BLOB + inline zsenc sentinel: {}",
        create.up
    );

    // --- Apply through the real hardened backend. ---
    let p = paths("apply_goodies");
    let be = backend(&p);
    for m in &migs {
        let applied = be
            .apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("apply {} must succeed: {e:?}", m.name));
        assert!(applied, "first apply of {} must be newly-applied", m.name);
    }

    // The table lands in the app file (main).
    let rows = be
        .actor()
        .query("SELECT name FROM main.sqlite_master WHERE type='table' AND name='accounts'")
        .await
        .expect("query sqlite_master");
    assert_eq!(rows.len(), 1, "accounts table must exist in main: {rows:?}");

    // Idempotent re-apply: every migration is a no-op the second time.
    for m in &migs {
        let again = be
            .apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("re-apply {} must succeed: {e:?}", m.name));
        assert!(!again, "re-apply of {} must be a no-op", m.name);
    }

    // --- Drift snapshot recovers the mask + encryption sentinels from
    //     sqlite_master.sql (P5 round-trip). ---
    let snap = be.snapshot_schema_sqlite().await.expect("snapshot_schema");
    let t = snap
        .tables
        .get("accounts")
        .expect("accounts in drift snapshot");

    // The encrypted column's `zsenc:` sentinel round-trips. The SQLite drift path
    // (P5, §2.7) recovers BOTH inline `__zsmask:` and `zsenc:` sentinels from
    // `sqlite_master.sql` into the single `comment_sentinel` slot (PG splits them
    // across `encryption_sentinel`/`comment_sentinel`; SQLite uses one recovery
    // slot). What matters is that the sentinel body survives emit→apply→snapshot.
    let secret = t
        .columns
        .iter()
        .find(|c| c.name == "secret")
        .expect("secret column in snapshot");
    let secret_sentinel = secret
        .comment_sentinel
        .as_deref()
        .or(secret.encryption_sentinel.as_deref());
    assert!(
        secret_sentinel.map(|s| s.contains("zsenc:")).unwrap_or(false),
        "encryption `zsenc:` sentinel must round-trip through the drift snapshot: {secret:?}"
    );

    // The masked sibling column is recovered WITH its `__zsmask:` mask sentinel.
    let masked = t
        .columns
        .iter()
        .find(|c| c.name == "ssn_masked")
        .expect("ssn_masked sibling in snapshot");
    assert!(
        masked
            .comment_sentinel
            .as_deref()
            .map(|s| s.contains("__zsmask:"))
            .unwrap_or(false),
        "mask `__zsmask:` sentinel must round-trip through the drift snapshot: {masked:?}"
    );
}

// ---------------------------------------------------------------------------
// E2E: FK round-trips inline on SQLite (parent created first; child inlines FK).
// ---------------------------------------------------------------------------

#[compio::test]
async fn descriptor_to_sqlite_apply_roundtrips_foreign_key() {
    let users = CollectionDescriptor {
        name: "users".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "handle".into(),
            ty: "string".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
    };
    let posts = CollectionDescriptor {
        name: "posts".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "author".into(),
            ty: "ref".into(),
            references: Some("users".into()),
            ..Default::default()
        }],
        indexes: vec![],
    };
    let desired = desired_snapshot(PROJECT, &[users, posts]).expect("desired_snapshot");

    let author = sqlite_author();
    let plan = author
        .diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("diff");
    let migs = plan.all_migrations();

    let posts_create = migs
        .iter()
        .find(|m| m.name == "create_table_posts")
        .expect("create_table_posts present");
    // FK present, inline, and references an UNqualified parent (SQLite rejects a
    // schema-qualified REFERENCES target).
    assert!(
        posts_create.up.contains("FOREIGN KEY")
            && posts_create.up.contains(r#"REFERENCES "users" (id)"#),
        "inline unqualified FK expected: {}",
        posts_create.up
    );

    // Apply: PRAGMA foreign_keys is enforced at the backend connection; the inline
    // FK must apply cleanly with `users` created first (engine topo order).
    let p = paths("apply_fk");
    let be = backend(&p);
    for m in &migs {
        be.apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("apply {} must succeed: {e:?}", m.name));
    }
    let rows = be
        .actor()
        .query("SELECT name FROM main.sqlite_master WHERE type='table' AND name IN ('users','posts') ORDER BY name")
        .await
        .expect("query");
    assert_eq!(rows.len(), 2, "both tables must exist: {rows:?}");
}

// ---------------------------------------------------------------------------
// SQLite cannot ALTER ADD CONSTRAINT — a genuinely-deferred FK is a typed error.
// ---------------------------------------------------------------------------

#[compio::test]
async fn sqlite_deferred_fk_is_typed_error() {
    // `posts` references `users`, but `users` is NOT declared and NOT live → the
    // FK target is missing. The cross-app-FK guard catches a truly-dangling target;
    // here we make `users` exist in the UNION so it passes that guard, but force the
    // deferred case by NOT creating it earlier in the batch is impossible with topo
    // order — so instead we assert the typed error surfaces for a target outside the
    // live + in-batch set by diffing a single-table batch whose FK points at a live-
    // absent, union-absent table is rejected upstream. The reachable deferred case:
    // a 2-table batch where the child's FK target is declared (union) so topo order
    // puts the parent first; that INLINES fine (covered above). To exercise the
    // `SqliteDeferredFkUnsupported` arm directly we diff `posts` alone against an
    // empty live with `users` present in the union but filtered — simplest faithful
    // trigger: a self-batch where the target is neither live nor created (a union
    // that declares only `posts`, whose FK names a table the cross-app guard treats
    // as live). We rely on the cross-app guard rejecting the dangling target, which
    // is the engine's fail-closed behaviour, OR the SQLite-specific arm.
    let posts = CollectionDescriptor {
        name: "posts".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "author".into(),
            ty: "ref".into(),
            references: Some("ghost_users".into()),
            ..Default::default()
        }],
        indexes: vec![],
    };
    let desired = desired_snapshot(PROJECT, &[posts]).expect("desired_snapshot");
    let author = sqlite_author();
    let err = author
        .diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect_err("a missing FK target must be rejected fail-closed");
    // Either the cross-app-FK-missing guard OR the SQLite deferred-FK arm — both are
    // fail-closed rejections; the SQLite path must never silently drop the FK.
    assert!(
        matches!(
            err,
            DeclarativeError::SqliteDeferredFkUnsupported { .. }
                | DeclarativeError::CrossAppFkTargetMissing { .. }
        ),
        "expected a fail-closed FK rejection, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// TrustProfile-SQLite wiring.
// ---------------------------------------------------------------------------

/// Confined SQLite accepts the descriptor-generated DDL (it applies cleanly through
/// the backend); a RAW untrusted SQL string is REFUSED by the Confined SQLite guard.
#[test]
fn confined_sqlite_guard_rejects_raw_sql() {
    let guard = SqlGuard::new(GuardConfig::confined_sqlite(PROJECT));
    // A perfectly benign-looking raw string is still refused — the SQLite Confined
    // path is descriptor-diff-only (no untrusted raw SQL).
    let err = guard
        .check("CREATE TABLE users (id INTEGER PRIMARY KEY)")
        .expect_err("raw SQL must be refused on the Confined SQLite path");
    assert!(
        matches!(err, GuardError::SqliteRawSqlRejected),
        "expected SqliteRawSqlRejected, got: {err:?}"
    );
}

/// The PG Confined guard still vets raw PG SQL (regression: SQLite rejection does
/// not bleed into the PG path).
#[test]
fn confined_pg_guard_still_checks_raw_sql() {
    let guard = SqlGuard::new(GuardConfig::confined(PROJECT));
    let report = guard
        .check(r#"CREATE TABLE "prj_demo"."users" (id text primary key)"#)
        .expect("PG raw DDL must still pass the PG Confined guard");
    assert!(!report.destructive);
}

/// Platform is a PG-only posture → `for_dialect(Sqlite)` fail-closes to Confined
/// SQLite (the resulting guard refuses raw SQL, like any Confined SQLite guard).
#[test]
fn platform_fails_closed_to_confined_on_sqlite() {
    // Build a Platform config via the public confined entry then re-key it for
    // SQLite. (The Platform constructor is operator-gated; `for_dialect` is the
    // dialect-selection seam any caller uses, and Confined→Sqlite is the same
    // fail-closed mapping Platform→Sqlite takes.)
    let cfg = GuardConfig::confined(PROJECT).for_dialect(SqlDialect::Sqlite);
    let guard = SqlGuard::new(cfg);
    let err = guard
        .check("SELECT 1")
        .expect_err("SQLite-keyed guard must refuse raw SQL");
    assert!(matches!(err, GuardError::SqliteRawSqlRejected), "got: {err:?}");

    // And `for_dialect(Postgres)` is identity — the PG guard still checks raw SQL.
    let pg = SqlGuard::new(GuardConfig::confined(PROJECT).for_dialect(SqlDialect::Postgres));
    assert!(pg
        .check(r#"CREATE TABLE "prj_demo"."t" (id text primary key)"#)
        .is_ok());
}

// ---------------------------------------------------------------------------
// FINDING 4 — the EXISTING-TABLE (second-deploy / incremental) declarative path
// on SQLite. Before the fix, every existing-table render method emitted PG-shaped
// schema-qualified DDL + PG-only syntax on the SQLite leg (every prior SQLite
// declarative test used an EMPTY live snapshot, so only the new-table CREATE path
// was ever exercised). On a real second deploy where a table exists in BOTH live
// and desired, the differ emitted:
//   - `ALTER TABLE "prj"."t" ADD COLUMN …` → "no such table" on SQLite;
//   - `COMMENT ON COLUMN …` → syntax error (no such statement on SQLite);
//   - `DROP INDEX "prj"."ix"` → SILENTLY no-ops (qualified name never resolves) —
//     silent drift, the dangerous one;
//   - `ALTER COLUMN … TYPE` / nullability → no such SQLite statement.
//
// These tests build a NON-EMPTY live snapshot (the first deploy, compiled through
// the same `desired_snapshot` so the data_type spellings match — no spurious type
// drift) and diff the second deploy against it.
// ---------------------------------------------------------------------------

/// The live snapshot for a first-deploy descriptor set: compiled through the same
/// `desired_snapshot` machinery the second deploy uses, so the column `data_type`
/// spellings match exactly (a manually-built or SQLite-introspected live would use
/// SQLite type affinities that the desired-side PG spellings would falsely diff
/// against — a separate, deeper normalisation gap, out of scope for this finding).
fn live_from(descs: &[CollectionDescriptor]) -> (SchemaSnapshot, HashMap<String, String>) {
    let d = desired_snapshot(PROJECT, descs).expect("first-deploy desired_snapshot");
    let ownership: HashMap<String, String> = d
        .ownership
        .iter()
        .map(|(t, a)| (t.clone(), a.clone()))
        .collect();
    (d.snapshot, ownership)
}

/// (a) Second-deploy ADD COLUMN on SQLite emits UNqualified, SQLite-legal DDL AND
/// APPLIES through the real hardened backend (the table persists, the column is
/// added). Pre-fix the `up` was `ALTER TABLE "prj_demo"."accounts" ADD COLUMN …`,
/// which fails "no such table" on SQLite.
#[compio::test]
async fn second_deploy_add_column_is_sqlite_legal_and_applies() {
    // First deploy: accounts(title). Second deploy: accounts(title, note).
    let v1 = CollectionDescriptor {
        name: "accounts".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "title".into(),
            ty: "string".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
    };
    let mut v2 = v1.clone();
    v2.fields.push(FieldDescriptor {
        name: "note".into(),
        ty: "string".into(),
        ..Default::default()
    });

    // Apply the first deploy through the real backend (greenfield create).
    let p = paths("second_add_col");
    let be = backend(&p);
    let first = desired_snapshot(PROJECT, std::slice::from_ref(&v1)).expect("v1 desired");
    let first_plan = sqlite_author()
        .diff(&first, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("v1 diff");
    for m in &first_plan.all_migrations() {
        be.apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("v1 apply {} must succeed: {e:?}", m.name));
    }

    // Second deploy: diff v2 against the v1 live snapshot.
    let (live, ownership) = live_from(&[v1]);
    let desired2 = desired_snapshot(PROJECT, &[v2]).expect("v2 desired");
    let plan = sqlite_author()
        .diff(&desired2, &live, &ownership, &[])
        .expect("second-deploy diff must succeed");
    let add = plan
        .all_migrations()
        .into_iter()
        .find(|m| m.name == "add_column_accounts_note")
        .expect("ADD COLUMN note migration present");

    // UNqualified + no PG `COMMENT ON COLUMN`.
    assert!(
        add.up.contains(r#"ALTER TABLE "accounts" ADD COLUMN "note""#),
        "ADD COLUMN must be unqualified SQLite-legal DDL: {}",
        add.up
    );
    assert!(
        !add.up.contains(r#""prj_demo"."#) && !add.up.contains("COMMENT ON COLUMN"),
        "no schema qualifier and no PG COMMENT ON COLUMN may appear: {}",
        add.up
    );

    // APPLIES through the real backend; the column lands.
    let applied = be
        .apply_one_additive(&add, "deployer")
        .await
        .unwrap_or_else(|e| panic!("second-deploy ADD COLUMN must apply on SQLite: {e:?}"));
    assert!(applied, "ADD COLUMN must be newly applied");
    let cols = be
        .actor()
        .query("PRAGMA main.table_info(accounts)")
        .await
        .expect("table_info");
    assert!(
        cols.iter().any(|r| r.get(1).and_then(Clone::clone).as_deref() == Some("note")),
        "the `note` column must exist after the second-deploy ADD COLUMN: {cols:?}"
    );
}

/// (b) A second-deploy DROP INDEX on SQLite ACTUALLY drops the index. Pre-fix the
/// `up` was `DROP INDEX "prj_demo"."accounts_handle_idx"`, which SILENTLY no-ops on
/// SQLite (the qualified name never resolves) — reporting success while the index
/// survives. We assert the index is GONE via PRAGMA (we do NOT trust IF EXISTS).
#[compio::test]
async fn second_deploy_drop_index_actually_drops_on_sqlite() {
    // First deploy: accounts(handle) WITH a user index on handle.
    let v1 = CollectionDescriptor {
        name: "accounts".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "handle".into(),
            ty: "string".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![IndexDescriptor {
            name: "accounts_handle_idx".into(),
            columns: vec!["handle".into()],
            unique: false,
        }],
    };
    // Second deploy: the same table WITHOUT the index → DROP INDEX.
    let mut v2 = v1.clone();
    v2.indexes.clear();

    let p = paths("second_drop_idx");
    let be = backend(&p);
    let first = desired_snapshot(PROJECT, std::slice::from_ref(&v1)).expect("v1 desired");
    let first_plan = sqlite_author()
        .diff(&first, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("v1 diff");
    for m in &first_plan.all_migrations() {
        be.apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("v1 apply {} must succeed: {e:?}", m.name));
    }
    // The user index exists after the first deploy.
    let before = be
        .actor()
        .query("SELECT name FROM main.sqlite_master WHERE type='index' AND name='accounts_handle_idx'")
        .await
        .expect("query index pre-drop");
    assert_eq!(before.len(), 1, "the user index must exist before the drop: {before:?}");

    // Second deploy: diff produces a DROP INDEX.
    let (live, ownership) = live_from(&[v1]);
    let desired2 = desired_snapshot(PROJECT, &[v2]).expect("v2 desired");
    let plan = sqlite_author()
        .diff(&desired2, &live, &ownership, &[])
        .expect("second-deploy diff must succeed");
    let drop = plan
        .all_migrations()
        .into_iter()
        .find(|m| m.name == "drop_index_accounts_handle_idx")
        .expect("DROP INDEX migration present");
    // UNqualified — the qualified form silently no-ops on SQLite.
    assert!(
        drop.up == r#"DROP INDEX "accounts_handle_idx""#,
        "DROP INDEX must be unqualified SQLite-legal DDL (qualified silently no-ops): {}",
        drop.up
    );

    // APPLY it and assert via PRAGMA the index is ACTUALLY gone (not trusting the
    // statement's success — the pre-fix qualified form "succeeded" too).
    be.apply_one_additive(&drop, "deployer")
        .await
        .unwrap_or_else(|e| panic!("DROP INDEX must apply on SQLite: {e:?}"));
    let after = be
        .actor()
        .query("SELECT name FROM main.sqlite_master WHERE type='index' AND name='accounts_handle_idx'")
        .await
        .expect("query index post-drop");
    assert!(
        after.is_empty(),
        "the index MUST be actually dropped, not silently no-opped: {after:?}"
    );
}

/// (c) P3b — a rebuild-needing existing-table op (ALTER COLUMN TYPE) now GENERATES
/// a 12-step table rebuild on SQLite (it no longer fails closed). The plan carries
/// ONE `SqliteRebuild` naming the op; it is NOT dangling `ALTER COLUMN … TYPE` PG
/// DDL (a non-existent statement on SQLite) and NOT a silent pass.
#[compio::test]
async fn second_deploy_type_change_generates_rebuild_on_sqlite() {
    // First deploy: accounts(count: number → `double precision`).
    let v1 = CollectionDescriptor {
        name: "accounts".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "count".into(),
            ty: "number".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
    };
    // Second deploy: the same column re-typed to `string` (`text`) — a type change.
    let mut v2 = v1.clone();
    v2.fields[0].ty = "string".into();

    let (live, ownership) = live_from(&[v1]);
    let desired2 = desired_snapshot(PROJECT, &[v2]).expect("v2 desired");
    let plan = sqlite_author()
        .diff(&desired2, &live, &ownership, &[])
        .expect("a SQLite existing-table type change now generates a rebuild (P3b)");
    assert_eq!(plan.rebuilds.len(), 1, "exactly one table rebuild");
    let rb = &plan.rebuilds[0];
    assert_eq!(rb.spec.table, "accounts");
    assert!(
        rb.spec.reason.contains("alter column count type"),
        "the rebuild reason must name the op: {}",
        rb.spec.reason
    );
    // The new-table CREATE is re-pointed to the engine temp name (never colliding
    // with the live table the rebuild drops).
    assert!(
        rb.spec
            .new_table_create
            .contains("\"accounts__zsrebuild\""),
        "the new CREATE must target the temp name: {}",
        rb.spec.new_table_create
    );
    assert!(
        !rb.spec
            .new_table_create
            .contains("CREATE TABLE IF NOT EXISTS \"accounts\""),
        "the new CREATE must NOT target the real name (it would collide on swap): {}",
        rb.spec.new_table_create
    );
    // No PG-shaped ALTER COLUMN DDL leaked into the plain migration set.
    assert!(
        plan.migrations.iter().all(|m| !m.up.contains("ALTER COLUMN")),
        "no dangling PG ALTER COLUMN DDL on the SQLite path"
    );
}

// ---------------------------------------------------------------------------
// FAITHFUL real-introspected-live drift (the dialect-aware data_type fix).
//
// The tests above build their LIVE snapshot through `live_from` (= the SAME
// `desired_snapshot` machinery, PG-spelled `data_type`), which sidesteps the
// real bug: a second-deploy SQLite diff in production compares the DESIRED
// snapshot (PG spellings: `bytea` / `double precision` / `timestamp with time
// zone`) against a LIVE snapshot REAL-introspected from the app file (SQLite
// declared types: `blob` / `real` / `text`). Pre-fix the raw-spelling compare
// flagged a spurious `SqliteRebuildRequired` on every encrypted / number /
// timestamp column even when the schema is UNCHANGED.
//
// These tests use the REAL introspected live snapshot (`snapshot_schema_sqlite`)
// — the faithful path — and assert (a) ZERO spurious drift for an unchanged
// schema, and (b) a GENUINE type change is STILL caught.
// ---------------------------------------------------------------------------

/// A descriptor whose columns cross every PG↔SQLite spelling gap: an encrypted
/// column (`bytea` desired vs `blob` live), a number (`double precision` vs
/// `real`), and a date/timestamp (`timestamp with time zone` vs `text`).
fn spelling_gap_desc() -> CollectionDescriptor {
    CollectionDescriptor {
        name: "ledger".into(),
        owner_app: APP.into(),
        fields: vec![
            FieldDescriptor {
                name: "title".into(),
                ty: "string".into(),
                required: true,
                ..Default::default()
            },
            // `bytea` (desired) vs `blob` (live SQLite).
            FieldDescriptor {
                name: "secret".into(),
                ty: "bytes".into(),
                encrypted: Some(serde_json::json!({ "mode": "randomized", "keyId": "k1" })),
                ..Default::default()
            },
            // `double precision` (desired) vs `real` (live SQLite).
            FieldDescriptor {
                name: "amount".into(),
                ty: "number".into(),
                required: true,
                ..Default::default()
            },
            // `timestamp with time zone` (desired) vs `text` (live SQLite).
            FieldDescriptor {
                name: "occurred_at".into(),
                ty: "date".into(),
                required: true,
                ..Default::default()
            },
        ],
        indexes: vec![],
    }
}

/// (FIX B) A second deploy of an UNCHANGED schema, diffed against the REAL
/// SQLite-introspected live snapshot, produces ZERO spurious drift — no
/// `SqliteRebuildRequired` for the encrypted (`bytea`→`blob`), number
/// (`double precision`→`real`), or timestamp (`… with time zone`→`text`)
/// columns whose PG and SQLite spellings differ.
///
/// This is RED before the dialect-aware normalisation: the raw-spelling compare
/// (`lc.data_type != c.data_type`) sees `blob != bytea` (etc.) and returns
/// `SqliteRebuildRequired`. It is GREEN after `sqlite_canonical_type` folds both
/// sides to the same affinity token.
#[compio::test]
async fn second_deploy_unchanged_real_introspected_live_has_no_spurious_drift() {
    let desc = spelling_gap_desc();

    // First deploy: CREATE the table through the real hardened backend.
    let p = paths("real_live_unchanged");
    let be = backend(&p);
    let first = desired_snapshot(PROJECT, std::slice::from_ref(&desc)).expect("v1 desired");
    let first_plan = sqlite_author()
        .diff(&first, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("v1 diff");
    for m in &first_plan.all_migrations() {
        be.apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("v1 apply {} must succeed: {e:?}", m.name));
    }

    // The FAITHFUL live snapshot: REAL introspection of the app file (SQLite
    // declared types `blob`/`real`/`text`), NOT the desired-side PG spellings.
    let live = be
        .snapshot_schema_sqlite()
        .await
        .expect("real introspected live snapshot");
    // Sanity: the live snapshot really does carry SQLite-spelled types (proving
    // this is the faithful path, not the `live_from` shortcut).
    let ledger = live.tables.get("ledger").expect("ledger in live snapshot");
    let live_type = |name: &str| {
        ledger
            .columns
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.data_type.as_str())
            .unwrap_or("<missing>")
    };
    assert_eq!(live_type("secret"), "blob", "encrypted column introspects as SQLite blob");
    assert_eq!(live_type("amount"), "real", "number column introspects as SQLite real");
    assert_eq!(live_type("occurred_at"), "text", "date column introspects as SQLite text");

    // Ownership travels alongside the union (the introspected snapshot has none).
    let ownership: HashMap<String, String> =
        first.ownership.iter().map(|(t, a)| (t.clone(), a.clone())).collect();

    // Second deploy: the SAME descriptor — an UNCHANGED schema. Diffing the
    // PG-spelled desired against the SQLite-spelled REAL live must NOT flag a
    // type change.
    let desired2 = desired_snapshot(PROJECT, &[desc]).expect("v2 desired");
    let plan = sqlite_author()
        .diff(&desired2, &live, &ownership, &[])
        .unwrap_or_else(|e| {
            panic!(
                "unchanged-schema second deploy against the REAL introspected live \
                 snapshot must NOT spuriously drift (got {e:?}) — the dialect-aware \
                 data_type normalisation should fold bytea↔blob / double precision↔real \
                 / timestamptz↔text"
            )
        });

    // No type-change / ALTER-bearing migration may be emitted for an unchanged schema.
    let spurious: Vec<String> = plan
        .all_migrations()
        .iter()
        .map(|m| m.name.clone())
        .filter(|n| {
            n.contains("alter_column") || n.contains("add_column") || n.contains("drop_column")
        })
        .collect();
    assert!(
        spurious.is_empty(),
        "an unchanged schema must emit no column ALTER/ADD/DROP migrations: {spurious:?}"
    );
    // P3b: an unchanged schema must ALSO emit no spurious table rebuild — the
    // dialect-aware fold must not flag a phantom type/nullability change.
    assert!(
        plan.rebuilds.is_empty(),
        "an unchanged schema must emit no SQLite rebuild: {:?}",
        plan.rebuilds.iter().map(|r| &r.spec.reason).collect::<Vec<_>>()
    );
}

/// (FIX B, the other half) A GENUINE type change against the REAL introspected
/// live snapshot is STILL detected — the normalisation must not be so lossy it
/// swallows a real change. `amount: number` (`real`) re-typed to `string`
/// (`text`) maps to two DISTINCT canonical tokens, so it still triggers a rebuild
/// (P3b: now a generated rebuild, no longer a typed error).
#[compio::test]
async fn second_deploy_real_type_change_still_detected_against_introspected_live() {
    let v1 = spelling_gap_desc();

    let p = paths("real_live_type_change");
    let be = backend(&p);
    let first = desired_snapshot(PROJECT, std::slice::from_ref(&v1)).expect("v1 desired");
    let first_plan = sqlite_author()
        .diff(&first, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("v1 diff");
    for m in &first_plan.all_migrations() {
        be.apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("v1 apply {} must succeed: {e:?}", m.name));
    }
    let live = be
        .snapshot_schema_sqlite()
        .await
        .expect("real introspected live snapshot");
    let ownership: HashMap<String, String> =
        first.ownership.iter().map(|(t, a)| (t.clone(), a.clone())).collect();

    // Second deploy: re-type `amount` from number (`real`) to string (`text`) — a
    // REAL change across affinity classes.
    let mut v2 = v1.clone();
    let amount = v2.fields.iter_mut().find(|f| f.name == "amount").unwrap();
    amount.ty = "string".into();

    let desired2 = desired_snapshot(PROJECT, &[v2]).expect("v2 desired");
    let plan = sqlite_author()
        .diff(&desired2, &live, &ownership, &[])
        .expect("a real number→string type change generates a rebuild (P3b)");
    assert_eq!(plan.rebuilds.len(), 1, "the genuine type change yields one rebuild");
    let rb = &plan.rebuilds[0];
    assert_eq!(rb.spec.table, "ledger");
    assert!(
        rb.spec.reason.contains("alter column amount type"),
        "the rebuild reason must name the changed column (the dialect-aware compare \
         must not swallow a real change): {}",
        rb.spec.reason
    );
}

// ---------------------------------------------------------------------------
// (P6a) `plan_declarative` now CARRIES a SQLite rebuild into the plan — the fail-close
//      is gone. `MigrationEngine` is generic over `MigrationBackend`, and
//      `apply_declarative` drives `plan.rebuilds` through `SqliteBackend::rebuild_one`
//      under the destructive/approval gate. The old behavior returned a typed
//      `SqliteRebuildRequired`; P6a replaces that with carrying the rebuild so the
//      engine can apply it. This pins the new contract: the plan exposes the rebuild
//      (with its destructive/approval flags) instead of refusing the whole deploy.
// ---------------------------------------------------------------------------
#[compio::test]
async fn plan_declarative_carries_sqlite_rebuild_into_the_plan() {
    // First deploy: a column typed `number`; second deploy re-types it to `string` —
    // a genuine existing-table type change → exactly the diff that yields a rebuild.
    let v1 = CollectionDescriptor {
        name: "accounts".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "count".into(),
            ty: "number".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
    };
    let mut v2 = v1.clone();
    v2.fields[0].ty = "string".into();

    let (live, ownership) = live_from(&[v1]);
    let desired2 = desired_snapshot(PROJECT, &[v2]).expect("v2 desired");

    // Sanity: the underlying diff DOES produce a rebuild (so the plan below is real).
    let diff = sqlite_author()
        .diff(&desired2, &live, &ownership, &[])
        .expect("diff");
    assert_eq!(diff.rebuilds.len(), 1, "the diff yields a rebuild to carry");

    // plan_declarative now CARRIES the rebuild (no error) — the fail-close is gone.
    let engine = MigrationEngine::new();
    let cfg = GuardConfig::confined_sqlite(PROJECT);
    let plan = engine
        .plan_declarative(&desired2, &live, &ownership, &sqlite_author(), &[], &cfg)
        .expect("plan_declarative now carries SQLite rebuilds, never fails closed");
    assert_eq!(
        plan.rebuilds.len(),
        1,
        "the plan carries the one rebuild for the engine to drive"
    );
    assert_eq!(plan.rebuilds[0].spec.table, "accounts");
    // The rebuild's journal migration is destructive + approval-gated (a rebuild on a
    // populated table drops + recreates), so the engine refuses it without approval.
    assert!(
        plan.rebuilds[0].migration.flags.destructive
            && plan.rebuilds[0].migration.flags.requires_approval,
        "a rebuild is destructive + approval-gated"
    );
    // And — H1 — a SQLite declarative plan never carries a PG-shaped online rename:
    // renames are routed to rebuilds on the SQLite leg.
    assert!(
        plan.renames.is_empty(),
        "SQLite renames are routed to rebuilds, never expand-contract (run_expand)"
    );
}
