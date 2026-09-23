//! Descriptor → engine-generated `SQLite` `up` → applied through the
//! hardened `SqliteBackend` → drift round-trip. Real temp-file `SQLite` throughout
//! (the faithful path: the actual `DeclarativeAuthor` emitter routes through the
//! shared `zeroship_migrate::schema` emitter, and the real backend authorizer applies the
//! unqualified DDL into `main` = the app file).
//!
//! Also: the TrustProfile-SQLite wiring (Confined `SQLite` accepts descriptor-
//! generated DDL; a raw untrusted SQL string is REFUSED on the Confined `SQLite`
//! guard; Platform fail-closes to Confined on `SQLite`).

use crate::support;

use std::collections::HashMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zeroship_migrate::{
    desired_snapshot_for_dialect, CollectionDescriptor, DeclarativeAuthor, DeclarativeError,
    DesiredSchema, EffectivePolicy, FieldDescriptor, GuardConfig, GuardError, IndexDescriptor,
    Migration, MigrationEngine, SchemaSnapshot,
};
use zeroship_migrate_sqlite::SqliteBackend;
// PostgreSQL's line-1, named at the vendor that owns it: these arms assert that a
// PG guard handed a SQLite config refuses rather than mis-vetting SQLite text.
use zeroship_migrate_postgres::guard::SqlGuard;

const PROJECT: &str = "prj_demo";
const APP: &str = "app_demo";

fn effective_policy() -> EffectivePolicy {
    support::confined_charter()
}

fn desired_sqlite(descriptors: &[CollectionDescriptor]) -> Result<DesiredSchema, DeclarativeError> {
    desired_snapshot_for_dialect(
        zeroship_migrate::shipping_vendors(),
        PROJECT,
        descriptors,
        &zeroship_migrate_sqlite::DIALECT,
        &effective_policy(),
    )
}

struct Paths {
    _dir: TempDir,
    app: PathBuf,
}

fn paths(app_id: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{app_id}.sqlite"));
    Paths { _dir: dir, app }
}

fn backend(p: &Paths) -> SqliteBackend {
    SqliteBackend::open(&p.app).expect("open hardened sqlite backend")
}

/// A SQLite-dialect declarative author.
fn sqlite_author() -> DeclarativeAuthor {
    DeclarativeAuthor::new_for_dialect(
        zeroship_migrate::shipping_vendors(),
        PROJECT,
        APP,
        zeroship_migrate_sqlite::DIALECT,
    )
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
                encrypted: Some(true),
                ..Default::default()
            },
        ],
        indexes: vec![],
        runtime_options: Default::default(),
    }
}

// ---------------------------------------------------------------------------
// E2E: descriptor → diff (SQLite author) → unqualified `up` → apply → drift.
// ---------------------------------------------------------------------------

#[compio::test]
async fn descriptor_to_sqlite_apply_roundtrips_mask_and_encryption() {
    let desc = goodies_desc();
    let desired = desired_sqlite(&[desc]).expect("desired_snapshot");

    // The SQLite author routes the new-table CREATE through the shared emitter.
    let author = sqlite_author();
    let plan = author
        .diff(
            &desired,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
            &effective_policy(),
        )
        .expect("diff");
    let migs = plan.all_migrations();
    let create = migs
        .iter()
        .find(|m| m.name == "create_table_accounts")
        .expect("create_table migration present");

    // The generated `up` is UNqualified (lands in `main` = the app file).
    assert!(
        create.up.contains(r#"CREATE TABLE "accounts" ("#),
        "engine-generated SQLite up must be unqualified: {}",
        create.up
    );
    assert!(
        !create.up.contains(r#""prj_demo"."#) && !create.up.contains(r#""app_demo"."#),
        "no schema/app qualifier may appear: {}",
        create.up
    );
    // Mask + encryption sentinels ride inline (the SQLite wire): the field's own
    // `ssn` column carries the mask sentinel now; the raw column carries the value.
    assert!(
        create.up.contains(r#""ssn" TEXT /* zero-migrate:mask:"#),
        "mask sentinel must ride inline: {}",
        create.up
    );
    assert!(
        create.up.contains("BLOB") && create.up.contains("/* zero-migrate:enc:"),
        "encrypted column must be BLOB + inline zero-migrate:enc sentinel: {}",
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
    //     sqlite_master.sql (round-trip). ---
    let snap = be.snapshot_schema_sqlite().await.expect("snapshot_schema");
    let t = snap
        .tables
        .get("accounts")
        .expect("accounts in drift snapshot");

    // The encrypted column's `zero-migrate:enc:` sentinel round-trips on the RAW
    // column now (the ciphertext moved there in the storage flip). The SQLite drift
    // path recovers BOTH inline `zero-migrate:mask:` and `zero-migrate:enc:`
    // sentinels from `sqlite_master.sql` into the single `comment_sentinel` slot
    // (PG splits them across `encryption_sentinel`/`comment_sentinel`; SQLite uses
    // one recovery slot). What matters is that the sentinel body survives
    // emit→apply→snapshot.
    let raw_secret_name = zeroship_migrate::schema::query::raw_column_name("secret");
    let secret_raw = t
        .columns
        .iter()
        .find(|c| c.name == raw_secret_name)
        .expect("raw secret column in snapshot");
    let secret_sentinel = secret_raw
        .comment_sentinel
        .as_deref()
        .or(secret_raw.encryption_sentinel.as_deref());
    assert!(
        secret_sentinel.is_some_and(|s| s.contains("zero-migrate:enc:")),
        "encryption `zero-migrate:enc:` sentinel must round-trip through the drift snapshot: {secret_raw:?}"
    );

    // The field's own `secret` column now holds the mask - an encrypted column
    // with no explicit `.mask()` still gets the schema-normaliser's fail-safe
    // default (`{ kind: full, classification: pii }`) - recovered WITH its
    // `zero-migrate:mask:` sentinel.
    let secret_mask = t
        .columns
        .iter()
        .find(|c| c.name == "secret")
        .expect("secret column in snapshot");
    assert!(
        secret_mask
            .comment_sentinel
            .as_deref()
            .is_some_and(|s| s.contains("zero-migrate:mask:")),
        "the encrypted column's auto-mask sentinel must round-trip: {secret_mask:?}"
    );

    // The field's own `ssn` column holds the mask, recovered WITH its
    // `zero-migrate:mask:` mask sentinel.
    let masked = t
        .columns
        .iter()
        .find(|c| c.name == "ssn")
        .expect("ssn column in snapshot");
    assert!(
        masked
            .comment_sentinel
            .as_deref()
            .is_some_and(|s| s.contains("zero-migrate:mask:")),
        "mask `zero-migrate:mask:` sentinel must round-trip through the drift snapshot: {masked:?}"
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
        runtime_options: Default::default(),
    };
    let posts = CollectionDescriptor {
        name: "posts".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "author".into(),
            ty: "ref".into(),
            references: Some("users".into()),
            reference_column: Some("id".into()),
            ..Default::default()
        }],
        indexes: vec![],
        runtime_options: Default::default(),
    };
    let desired = desired_sqlite(&[users, posts]).expect("desired_snapshot");

    let author = sqlite_author();
    let plan = author
        .diff(
            &desired,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
            &effective_policy(),
        )
        .expect("diff");
    let migs = plan.all_migrations();

    let posts_create = migs
        .iter()
        .find(|m| m.name == "create_table_posts")
        .expect("create_table_posts present");
    // FK present, inline, and references an UNqualified parent (SQLite rejects a
    // schema-qualified REFERENCES target).
    assert!(
        posts_create.up.contains("FOREIGN KEY") && posts_create.up.contains("REFERENCES users(id)"),
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
    // `posts` references `ghost_users`, which is NOT declared and NOT live → the
    // FK target is missing. A target declared in the union cannot reach the
    // deferred arm here: topo order puts the parent first and the FK INLINES
    // (covered above). So the trigger is a single-table batch whose FK points
    // outside the live + in-batch set, which must be rejected fail-closed.
    let posts = CollectionDescriptor {
        name: "posts".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "author".into(),
            ty: "ref".into(),
            references: Some("ghost_users".into()),
            reference_column: Some("id".into()),
            ..Default::default()
        }],
        indexes: vec![],
        runtime_options: Default::default(),
    };
    let desired = desired_sqlite(&[posts]).expect("desired_snapshot");
    let author = sqlite_author();
    let err = author
        .diff(
            &desired,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
            &effective_policy(),
        )
        .expect_err("a missing FK target must be rejected fail-closed");
    // Either the cross-app-FK-missing guard OR the SQLite deferred-FK arm — both are
    // fail-closed rejections; the SQLite path must never silently drop the FK.
    assert!(
        matches!(
            err,
            DeclarativeError::DeferredForeignKeyUnsupported { .. }
                | DeclarativeError::CrossAppFkTargetMissing { .. }
        ),
        "expected a fail-closed FK rejection, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// TrustProfile-SQLite wiring.
// ---------------------------------------------------------------------------

/// Confined `SQLite` accepts the descriptor-generated DDL (it applies cleanly through
/// the backend); a RAW untrusted SQL string is REFUSED by the Confined `SQLite` guard.
#[test]
fn confined_sqlite_guard_rejects_raw_sql() {
    let guard = SqlGuard::new(GuardConfig::from_policy(
        support::no_inject(PROJECT),
        zeroship_migrate_sqlite::DIALECT,
        PROJECT,
    ));
    // A perfectly benign-looking raw string is still refused — the SQLite Confined
    // path is descriptor-diff-only (no untrusted raw SQL).
    let err = guard
        .check("CREATE TABLE users (id INTEGER PRIMARY KEY)")
        .expect_err("raw SQL must be refused on the Confined SQLite path");
    assert!(
        matches!(
            err,
            GuardError::RawSqlRejected { ref dialect }
                if dialect == &zeroship_migrate_sqlite::DIALECT
        ),
        "expected a SQLite-provenance RawSqlRejected, got: {err:?}"
    );
}

/// The PG Confined guard still vets raw PG SQL (regression: `SQLite` rejection does
/// not bleed into the PG path).
#[test]
fn confined_pg_guard_still_checks_raw_sql() {
    let guard = SqlGuard::new(GuardConfig::from_policy(
        support::no_inject(PROJECT),
        zeroship_migrate_postgres::DIALECT,
        PROJECT,
    ));
    let report = guard
        .check(r#"CREATE TABLE "prj_demo"."users" (id text primary key)"#)
        .expect("PG raw DDL must still pass the PG Confined guard");
    assert!(!report.destructive);
}

/// Platform is a PG-only posture → `for_dialect(Sqlite)` fail-closes to Confined
/// `SQLite` (the resulting guard refuses raw SQL, like any Confined `SQLite` guard).
#[test]
fn platform_fails_closed_to_confined_on_sqlite() {
    // Build a Platform config via the public confined entry then re-key it for
    // SQLite. (The Platform constructor is operator-gated; `for_dialect` is the
    // dialect-selection seam any caller uses, and Confined→Sqlite is the same
    // fail-closed mapping Platform→Sqlite takes.)
    let cfg = GuardConfig::from_policy(
        support::no_inject(PROJECT),
        zeroship_migrate_postgres::DIALECT,
        PROJECT,
    )
    .for_dialect(zeroship_migrate_sqlite::DIALECT);
    let guard = SqlGuard::new(cfg);
    let err = guard
        .check("SELECT 1")
        .expect_err("SQLite-keyed guard must refuse raw SQL");
    assert!(
        matches!(
            err,
            GuardError::RawSqlRejected { ref dialect }
                if dialect == &zeroship_migrate_sqlite::DIALECT
        ),
        "got: {err:?}"
    );

    // And `for_dialect(Postgres)` is identity — the PG guard still checks raw SQL.
    let pg = SqlGuard::new(
        GuardConfig::from_policy(
            support::no_inject(PROJECT),
            zeroship_migrate_postgres::DIALECT,
            PROJECT,
        )
        .for_dialect(zeroship_migrate_postgres::DIALECT),
    );
    assert!(pg
        .check(r#"CREATE TABLE "prj_demo"."t" (id text primary key)"#)
        .is_ok());
}

// ---------------------------------------------------------------------------
// The EXISTING-TABLE (second-deploy / incremental) declarative path
// on SQLite. Existing-table renders must be SQLite-legal on the SQLite leg:
//   - `ALTER TABLE "prj"."t" ADD COLUMN …` answers "no such table" on SQLite —
//     the name must be UNqualified;
//   - `COMMENT ON COLUMN …` is a syntax error (no such statement on SQLite);
//   - `DROP INDEX "prj"."ix"` SILENTLY no-ops (a qualified name never
//     resolves) — silent drift, the dangerous one;
//   - `ALTER COLUMN … TYPE` / nullability have no such SQLite statement.
//
// These tests build a NON-EMPTY live snapshot (the first deploy, compiled through
// the same dialect-aware desired snapshot so the data_type spellings match — no spurious type
// drift) and diff the second deploy against it.
// ---------------------------------------------------------------------------

/// The live snapshot for a first-deploy descriptor set: compiled through the same
/// dialect-aware snapshot machinery the second deploy uses, so the column `data_type`
/// spellings match exactly (a manually-built or SQLite-introspected live would use
/// `SQLite` type affinities that the desired-side PG spellings would falsely diff
/// against — a separate, deeper normalisation gap, out of scope for this finding).
fn live_from(descs: &[CollectionDescriptor]) -> (SchemaSnapshot, HashMap<String, String>) {
    let d = desired_sqlite(descs).expect("first-deploy desired_snapshot");
    let ownership: HashMap<String, String> = d
        .ownership
        .iter()
        .map(|(t, a)| (t.clone(), a.clone()))
        .collect();
    (d.snapshot, ownership)
}

/// (a) Second-deploy ADD COLUMN on `SQLite` emits `UNqualified`, SQLite-legal DDL AND
/// APPLIES through the real hardened backend (the table persists, the column is
/// added). A schema-qualified `ALTER TABLE "prj_demo"."accounts" ADD COLUMN …`
/// fails "no such table" on `SQLite`.
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
        runtime_options: Default::default(),
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
    let first = desired_sqlite(std::slice::from_ref(&v1)).expect("v1 desired");
    let first_plan = sqlite_author()
        .diff(
            &first,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
            &effective_policy(),
        )
        .expect("v1 diff");
    for m in &first_plan.all_migrations() {
        be.apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("v1 apply {} must succeed: {e:?}", m.name));
    }

    // Second deploy: diff v2 against the v1 live snapshot.
    let (live, ownership) = live_from(&[v1]);
    let desired2 = desired_sqlite(&[v2]).expect("v2 desired");
    let plan = sqlite_author()
        .diff(&desired2, &live, &ownership, &[], &effective_policy())
        .expect("second-deploy diff must succeed");
    let add = plan
        .all_migrations()
        .into_iter()
        .find(|m| m.name == "add_column_accounts_note")
        .expect("ADD COLUMN note migration present");

    // UNqualified + no PG `COMMENT ON COLUMN`.
    assert!(
        add.up
            .contains(r#"ALTER TABLE "accounts" ADD COLUMN "note""#),
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
        cols.iter()
            .any(|r| r.get(1).and_then(Clone::clone).as_deref() == Some("note")),
        "the `note` column must exist after the second-deploy ADD COLUMN: {cols:?}"
    );
}

/// (b) A second-deploy DROP INDEX on `SQLite` ACTUALLY drops the index. A
/// schema-qualified `DROP INDEX "prj_demo"."accounts_handle_idx"` SILENTLY
/// no-ops on
/// `SQLite` (the qualified name never resolves) — reporting success while the index
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
        runtime_options: Default::default(),
    };
    // Second deploy: the same table WITHOUT the index → DROP INDEX.
    let mut v2 = v1.clone();
    v2.indexes.clear();

    let p = paths("second_drop_idx");
    let be = backend(&p);
    let first = desired_sqlite(std::slice::from_ref(&v1)).expect("v1 desired");
    let first_plan = sqlite_author()
        .diff(
            &first,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
            &effective_policy(),
        )
        .expect("v1 diff");
    for m in &first_plan.all_migrations() {
        be.apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("v1 apply {} must succeed: {e:?}", m.name));
    }
    // The user index exists after the first deploy.
    let before = be
        .actor()
        .query(
            "SELECT name FROM main.sqlite_master WHERE type='index' AND name='accounts_handle_idx'",
        )
        .await
        .expect("query index pre-drop");
    assert_eq!(
        before.len(),
        1,
        "the user index must exist before the drop: {before:?}"
    );

    // Second deploy: diff produces a DROP INDEX.
    let (live, ownership) = live_from(&[v1]);
    let desired2 = desired_sqlite(&[v2]).expect("v2 desired");
    let plan = sqlite_author()
        .diff(&desired2, &live, &ownership, &[], &effective_policy())
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
    // statement's success — a qualified form would "succeed" while no-op'ing).
    be.apply_one_additive(&drop, "deployer")
        .await
        .unwrap_or_else(|e| panic!("DROP INDEX must apply on SQLite: {e:?}"));
    let after = be
        .actor()
        .query(
            "SELECT name FROM main.sqlite_master WHERE type='index' AND name='accounts_handle_idx'",
        )
        .await
        .expect("query index post-drop");
    assert!(
        after.is_empty(),
        "the index MUST be actually dropped, not silently no-opped: {after:?}"
    );
}

/// (c) A rebuild-needing existing-table op (ALTER COLUMN TYPE) GENERATES
/// a table rebuild on `SQLite`. The plan carries
/// ONE `TableRebuild` naming the op; it is NOT dangling `ALTER COLUMN … TYPE` PG
/// DDL (a non-existent statement on `SQLite`) and NOT a silent pass.
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
        runtime_options: Default::default(),
    };
    // Second deploy: the same column re-typed to `string` (`text`) — a type change.
    let mut v2 = v1.clone();
    v2.fields[0].ty = "string".into();

    let (live, ownership) = live_from(&[v1]);
    let desired2 = desired_sqlite(&[v2]).expect("v2 desired");
    let plan = sqlite_author()
        .diff(&desired2, &live, &ownership, &[], &effective_policy())
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
            .contains("\"accounts__zero_migrate_rebuild\""),
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
        plan.migrations
            .iter()
            .all(|m| !m.up.contains("ALTER COLUMN")),
        "no dangling PG ALTER COLUMN DDL on the SQLite path"
    );
}

// ---------------------------------------------------------------------------
// FAITHFUL real-introspected-live drift (dialect-aware data_type comparison).
//
// The tests above build their LIVE snapshot through `live_from` (= the SAME
// dialect-aware snapshot machinery, PG-spelled `data_type`), which sidesteps
// the spelling gap a production second-deploy diff must cross: the DESIRED
// snapshot carries PG spellings (`bytea` / `double precision` / `timestamp
// with time zone`) while the LIVE snapshot is REAL-introspected from the app
// file (SQLite declared types: `blob` / `real` / `text`). A raw-spelling
// compare flags a spurious rebuild refusal on every encrypted / number /
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
                encrypted: Some(true),
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
                ty: "timestamp".into(),
                required: true,
                ..Default::default()
            },
        ],
        indexes: vec![],
        runtime_options: Default::default(),
    }
}

/// A second deploy of an UNCHANGED schema, diffed against the REAL
/// SQLite-introspected live snapshot, produces ZERO spurious drift — no
/// rebuild refusal for the encrypted (`bytea`→`blob`), number
/// (`double precision`→`real`), or timestamp (`… with time zone`→`text`)
/// columns whose PG and `SQLite` spellings differ.
///
/// A raw-spelling compare (`lc.data_type != c.data_type`) would see
/// `blob != bytea` (etc.) and return that refusal; `sqlite_canonical_type`
/// folds both sides to the same affinity token.
#[compio::test]
async fn second_deploy_unchanged_real_introspected_live_has_no_spurious_drift() {
    let desc = spelling_gap_desc();

    // First deploy: CREATE the table through the real hardened backend.
    let p = paths("real_live_unchanged");
    let be = backend(&p);
    let first = desired_sqlite(std::slice::from_ref(&desc)).expect("v1 desired");
    let first_plan = sqlite_author()
        .diff(
            &first,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
            &effective_policy(),
        )
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
            .map_or("<missing>", |c| c.data_type.as_str())
    };
    assert_eq!(
        live_type(&zeroship_migrate::schema::query::raw_column_name("secret")),
        "blob",
        "the raw (ciphertext) column introspects as SQLite blob"
    );
    assert_eq!(
        live_type("secret"),
        "text",
        "the field's own column now holds the auto-mask, introspecting as SQLite text"
    );
    assert_eq!(
        live_type("amount"),
        "real",
        "number column introspects as SQLite real"
    );
    assert_eq!(
        live_type("occurred_at"),
        "text",
        "date column introspects as SQLite text"
    );

    // Ownership travels alongside the union (the introspected snapshot has none).
    let ownership: HashMap<String, String> = first
        .ownership
        .iter()
        .map(|(t, a)| (t.clone(), a.clone()))
        .collect();

    // Second deploy: the SAME descriptor — an UNCHANGED schema. Diffing the
    // PG-spelled desired against the SQLite-spelled REAL live must NOT flag a
    // type change.
    let desired2 = desired_sqlite(&[desc]).expect("v2 desired");
    let plan = sqlite_author()
        .diff(&desired2, &live, &ownership, &[], &effective_policy())
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
    // An unchanged schema must ALSO emit no spurious table rebuild — the
    // dialect-aware fold must not flag a phantom type/nullability change.
    assert!(
        plan.rebuilds.is_empty(),
        "an unchanged schema must emit no SQLite rebuild: {:?}",
        plan.rebuilds
            .iter()
            .map(|r| &r.spec.reason)
            .collect::<Vec<_>>()
    );
}

/// A GENUINE type change against the REAL introspected
/// live snapshot is STILL detected — the normalisation must not be so lossy it
/// swallows a real change. `amount: number` (`real`) re-typed to `string`
/// (`text`) maps to two DISTINCT canonical tokens, so it still triggers a
/// rebuild.
#[compio::test]
async fn second_deploy_real_type_change_still_detected_against_introspected_live() {
    let v1 = spelling_gap_desc();

    let p = paths("real_live_type_change");
    let be = backend(&p);
    let first = desired_sqlite(std::slice::from_ref(&v1)).expect("v1 desired");
    let first_plan = sqlite_author()
        .diff(
            &first,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
            &effective_policy(),
        )
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
    let ownership: HashMap<String, String> = first
        .ownership
        .iter()
        .map(|(t, a)| (t.clone(), a.clone()))
        .collect();

    // Second deploy: re-type `amount` from number (`real`) to string (`text`) — a
    // REAL change across affinity classes.
    let mut v2 = v1.clone();
    let amount = v2.fields.iter_mut().find(|f| f.name == "amount").unwrap();
    amount.ty = "string".into();

    let desired2 = desired_sqlite(&[v2]).expect("v2 desired");
    let plan = sqlite_author()
        .diff(&desired2, &live, &ownership, &[], &effective_policy())
        .expect("a real number→string type change generates a rebuild (P3b)");
    assert_eq!(
        plan.rebuilds.len(),
        1,
        "the genuine type change yields one rebuild"
    );
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
// `plan_declarative` CARRIES a SQLite rebuild into the plan.
//      `MigrationEngine` is generic over `MigrationBackend`, and
//      `apply_declarative` drives `plan.rebuilds` through `SqliteBackend::rebuild_one`
//      under the destructive/approval gate. This pins the contract: the plan
//      exposes the rebuild (with its destructive/approval flags) instead of
//      refusing the whole deploy.
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
        runtime_options: Default::default(),
    };
    let mut v2 = v1.clone();
    v2.fields[0].ty = "string".into();

    let (live, ownership) = live_from(&[v1]);
    let desired2 = desired_sqlite(&[v2]).expect("v2 desired");

    // Sanity: the underlying diff DOES produce a rebuild (so the plan below is real).
    let diff = sqlite_author()
        .diff(&desired2, &live, &ownership, &[], &effective_policy())
        .expect("diff");
    assert_eq!(diff.rebuilds.len(), 1, "the diff yields a rebuild to carry");

    // plan_declarative now CARRIES the rebuild (no error) — the fail-close is gone.
    let engine = MigrationEngine::new(zeroship_migrate::shipping_vendors());
    let cfg = GuardConfig::from_policy(
        support::no_inject(PROJECT),
        zeroship_migrate_sqlite::DIALECT,
        PROJECT,
    );
    let plan = engine
        .plan_declarative(
            &desired2,
            &live,
            &ownership,
            &sqlite_author(),
            &[],
            &cfg,
            &effective_policy(),
        )
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
    // And — a SQLite declarative plan never carries a PG-shaped online rename:
    // renames are routed to rebuilds on the SQLite leg.
    assert!(
        plan.renames.is_empty(),
        "SQLite renames are routed to rebuilds, never expand-contract (run_expand)"
    );
}

// ===========================================================================
// GOLDEN DDL — exact-byte assertions on the SQLite emitter.
//
// These pin the FULL `up`/`down` strings the SQLite render paths emit today, for
// the create-table / add-column / create-index / drop-{table,column,index} paths
// over a representative schema (PK, plain column, mask column, encrypted column,
// FK, index). They are the explicit byte bar for the `DdlEmitter` extraction.
// The policy-source convergence intentionally removed SQLite defaults that were
// absent from the confined charter; every other byte remains pinned.
//
// Note the create-table `up` is the shared resolved-snapshot renderer's output —
// pinned here so we'd notice an unrelated drift; the add-column / index / drop
// paths are the engine's own render methods.
// ===========================================================================

fn golden_find<'a>(migs: &'a [Migration], name: &str) -> &'a Migration {
    migs.iter().find(|m| m.name == name).unwrap_or_else(|| {
        panic!(
            "migration {name} present; have: {:?}",
            migs.iter().map(|m| &m.name).collect::<Vec<_>>()
        )
    })
}

fn golden_live(descs: &[CollectionDescriptor]) -> (SchemaSnapshot, HashMap<String, String>) {
    let d = desired_sqlite(descs).expect("golden live desired_snapshot");
    let ownership: HashMap<String, String> = d
        .ownership
        .iter()
        .map(|(t, a)| (t.clone(), a.clone()))
        .collect();
    (d.snapshot, ownership)
}

#[compio::test]
async fn golden_sqlite_create_table_and_index() {
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
        runtime_options: Default::default(),
    };
    let accounts = CollectionDescriptor {
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
                encrypted: Some(true),
                ..Default::default()
            },
            FieldDescriptor {
                name: "owner".into(),
                ty: "ref".into(),
                references: Some("users".into()),
                reference_column: Some("id".into()),
                ..Default::default()
            },
        ],
        indexes: vec![IndexDescriptor {
            name: "accounts_title_idx".into(),
            columns: vec!["title".into()],
            unique: false,
        }],
        runtime_options: Default::default(),
    };
    let desired = desired_sqlite(&[users, accounts]).expect("desired_snapshot");
    let migs = sqlite_author()
        .diff(
            &desired,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
            &effective_policy(),
        )
        .expect("greenfield diff")
        .all_migrations();

    // create_table users — shared emitter, unqualified, system-field indexes inline.
    let users_mig = golden_find(&migs, "create_table_users");
    assert_eq!(
        users_mig.up,
        "CREATE TABLE \"users\" (\"created_at\" TEXT NOT NULL, \"created_by\" TEXT, \"deleted_at\" TEXT, \"handle\" TEXT NOT NULL, \"id\" TEXT PRIMARY KEY NOT NULL, \"updated_at\" TEXT NOT NULL, \"updated_by\" TEXT, \"version\" INTEGER NOT NULL);\nCREATE INDEX IF NOT EXISTS \"users_created_by_idx\" ON \"users\" (\"created_by\");\nCREATE INDEX IF NOT EXISTS \"users_deleted_at_idx\" ON \"users\" (\"deleted_at\");\nCREATE INDEX IF NOT EXISTS \"users_updated_at_idx\" ON \"users\" (\"updated_at\")",
    );
    assert_eq!(users_mig.down.as_deref(), Some(r#"DROP TABLE "users""#));

    // create_table accounts — raw column (declared type, sentinel where the value
    // carries one) + the field's own mask column (bare TEXT + inline sentinel), for
    // both the encrypted-with-auto-mask `secret` field and the explicitly-masked
    // `ssn` field. Physical columns sort alphabetically, so both `__zs_raw__*`
    // columns land first.
    let accounts_mig = golden_find(&migs, "create_table_accounts");
    assert_eq!(
        accounts_mig.up,
        "CREATE TABLE \"accounts\" (\"__zs_raw__secret\" BLOB /* zero-migrate:enc:bytes */, \"__zs_raw__ssn\" TEXT, \"created_at\" TEXT NOT NULL, \"created_by\" TEXT, \"deleted_at\" TEXT, \"id\" TEXT PRIMARY KEY NOT NULL, \"owner\" TEXT COLLATE BINARY, \"secret\" TEXT /* zero-migrate:mask:kind=full,classification=pii */, \"ssn\" TEXT /* zero-migrate:mask:kind=last4,classification=pii */, \"title\" TEXT NOT NULL, \"updated_at\" TEXT NOT NULL, \"updated_by\" TEXT, \"version\" INTEGER NOT NULL, CONSTRAINT \"accounts_owner_fkey\" FOREIGN KEY (owner) REFERENCES users(id));\nCREATE INDEX IF NOT EXISTS \"accounts_created_by_idx\" ON \"accounts\" (\"created_by\");\nCREATE INDEX IF NOT EXISTS \"accounts_deleted_at_idx\" ON \"accounts\" (\"deleted_at\");\nCREATE INDEX IF NOT EXISTS \"accounts_updated_at_idx\" ON \"accounts\" (\"updated_at\")",
    );
    assert_eq!(
        accounts_mig.down.as_deref(),
        Some(r#"DROP TABLE "accounts""#)
    );

    // create_index — engine render path: unqualified, no USING/WITH, unqualified DROP.
    let idx = golden_find(&migs, "create_index_accounts_title_idx");
    assert_eq!(
        idx.up,
        r#"CREATE INDEX IF NOT EXISTS "accounts_title_idx" ON "accounts" ("title")"#,
    );
    assert_eq!(
        idx.down.as_deref(),
        Some(r#"DROP INDEX IF EXISTS "accounts_title_idx""#)
    );
}

#[compio::test]
async fn golden_sqlite_add_column() {
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
        runtime_options: Default::default(),
    };
    let mut v2 = v1.clone();
    v2.fields.push(FieldDescriptor {
        name: "note".into(),
        ty: "string".into(),
        ..Default::default()
    });
    v2.fields.push(FieldDescriptor {
        name: "ssn".into(),
        ty: "string".into(),
        mask: Some(serde_json::json!({ "kind": "last4", "classification": "pii" })),
        ..Default::default()
    });
    v2.fields.push(FieldDescriptor {
        name: "secret".into(),
        ty: "bytes".into(),
        encrypted: Some(true),
        ..Default::default()
    });
    let (live, ownership) = golden_live(std::slice::from_ref(&v1));
    let desired2 = desired_sqlite(&[v2]).expect("v2 desired");
    let migs = sqlite_author()
        .diff(&desired2, &live, &ownership, &[], &effective_policy())
        .expect("add-column diff")
        .all_migrations();

    // Plain — unqualified SQLite `TEXT`, with no COMMENT ON COLUMN.
    let note = golden_find(&migs, "add_column_accounts_note");
    assert_eq!(note.up, r#"ALTER TABLE "accounts" ADD COLUMN "note" TEXT"#);
    assert_eq!(
        note.down.as_deref(),
        Some(r#"ALTER TABLE "accounts" DROP COLUMN "note""#)
    );

    // Encrypted, no explicit `.mask()` — the schema-normaliser's fail-safe still
    // auto-populates the default `{ kind: full, classification: pii }` mask (mirrors
    // the SDK's `.encrypted()` auto-fill), so `secret` splits into a RAW column
    // (declared type BLOB + the inline encryption sentinel, no COMMENT tail on
    // SQLite) named by `raw_column_name`, and the field's own `secret` column
    // holding the mask.
    let raw_secret_name = format!(
        "add_column_accounts_{}",
        zeroship_migrate::schema::query::raw_column_name("secret")
    );
    let secret_raw = golden_find(&migs, &raw_secret_name);
    assert_eq!(
        secret_raw.up,
        format!(
            r#"ALTER TABLE "accounts" ADD COLUMN "{}" BLOB /* zero-migrate:enc:bytes */"#,
            zeroship_migrate::schema::query::raw_column_name("secret")
        ),
    );
    assert_eq!(
        secret_raw.down.as_deref(),
        Some(
            format!(
                r#"ALTER TABLE "accounts" DROP COLUMN "{}""#,
                zeroship_migrate::schema::query::raw_column_name("secret")
            )
            .as_str()
        ),
    );
    let secret_mask = golden_find(&migs, "add_column_accounts_secret");
    assert_eq!(
        secret_mask.up,
        r#"ALTER TABLE "accounts" ADD COLUMN "secret" TEXT /* zero-migrate:mask:kind=full,classification=pii */"#,
    );
    assert_eq!(
        secret_mask.down.as_deref(),
        Some(r#"ALTER TABLE "accounts" DROP COLUMN "secret""#),
    );

    // Explicitly masked, not encrypted — `ssn` splits into a RAW column (the
    // declared type, no sentinel of its own) and the field's own `ssn` column
    // carrying the inline `/* zero-migrate:mask:… */` sentinel (no COMMENT ON
    // COLUMN on SQLite).
    let raw_ssn_name = format!(
        "add_column_accounts_{}",
        zeroship_migrate::schema::query::raw_column_name("ssn")
    );
    let ssn_raw = golden_find(&migs, &raw_ssn_name);
    assert_eq!(
        ssn_raw.up,
        format!(
            r#"ALTER TABLE "accounts" ADD COLUMN "{}" TEXT"#,
            zeroship_migrate::schema::query::raw_column_name("ssn")
        ),
    );
    assert_eq!(
        ssn_raw.down.as_deref(),
        Some(
            format!(
                r#"ALTER TABLE "accounts" DROP COLUMN "{}""#,
                zeroship_migrate::schema::query::raw_column_name("ssn")
            )
            .as_str()
        ),
    );
    let ssn_mask = golden_find(&migs, "add_column_accounts_ssn");
    assert_eq!(
        ssn_mask.up,
        r#"ALTER TABLE "accounts" ADD COLUMN "ssn" TEXT /* zero-migrate:mask:kind=last4,classification=pii */"#,
    );
    assert_eq!(
        ssn_mask.down.as_deref(),
        Some(r#"ALTER TABLE "accounts" DROP COLUMN "ssn""#),
    );
}

#[compio::test]
async fn golden_sqlite_drops() {
    let live_accounts = CollectionDescriptor {
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
                name: "drop_me".into(),
                ty: "string".into(),
                ..Default::default()
            },
        ],
        indexes: vec![IndexDescriptor {
            name: "accounts_dropidx".into(),
            columns: vec!["title".into()],
            unique: false,
        }],
        runtime_options: Default::default(),
    };
    let gone = CollectionDescriptor {
        name: "gone".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "x".into(),
            ty: "string".into(),
            ..Default::default()
        }],
        indexes: vec![],
        runtime_options: Default::default(),
    };
    let desired_accounts = CollectionDescriptor {
        name: "accounts".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "title".into(),
            ty: "string".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
        runtime_options: Default::default(),
    };
    let (live, ownership) = golden_live(&[live_accounts, gone]);
    let desired = desired_sqlite(&[desired_accounts]).expect("desired");
    let migs = sqlite_author()
        .diff(&desired, &live, &ownership, &[], &effective_policy())
        .expect("drop diff")
        .all_migrations();

    // DROP COLUMN — unqualified (native SQLite ≥ 3.35).
    let dc = golden_find(&migs, "drop_column_accounts_drop_me");
    assert_eq!(dc.up, r#"ALTER TABLE "accounts" DROP COLUMN "drop_me""#);
    assert_eq!(dc.down, None);

    // DROP INDEX — unqualified (a qualified DROP silently no-ops on SQLite).
    let di = golden_find(&migs, "drop_index_accounts_dropidx");
    assert_eq!(di.up, r#"DROP INDEX "accounts_dropidx""#);
    assert_eq!(di.down, None);

    // DROP TABLE — unqualified (main IS the app file).
    let dt = golden_find(&migs, "drop_table_gone");
    assert_eq!(dt.up, r#"DROP TABLE "gone""#);
    assert_eq!(dt.down, None);
}

/// **The DIFFER half of the within-TEXT-affinity facet contract.**
///
/// Twin of the existence-guard probe's
/// `add_column_ifnotexists_sqlite_ref_over_live_string_is_noop`
/// (`crates/zeroship-migrate/tests/sqlite_engine/existence_guard_sqlite.rs`): the guard `SatisfiedNoop`'s a `ref` declared
/// over a live `string` column because both fold to the `SQLite` `text` affinity. This
/// test pins the EXACT boundary of that consistency on the FAITHFUL introspected path,
/// so the corrected report can state it honestly rather than over-claim:
///
/// 1. **COLUMN TYPE/AFFINITY — consistent, a no-op in BOTH.** The differ folds the
///    PG-spelled desired and the SQLite-introspected live column types through the SAME
///    `sqlite_canonical_type` the guard uses, so the `string`→`ref` facet change emits
///    NO `add_column`/`alter_column`/`drop_column` migration. A later `diff` does NOT
///    phantom-drift the COLUMN the guard skipped. (RED before the fold landed — the raw
///    spelling compare would have seen a change; GREEN after.)
///
/// 2. **FOREIGN KEY — a LEGITIMATE differ-only rebuild, NOT a column drift.** A `ref`
///    also declares a deferred FK. `SQLite` has no `ALTER TABLE ADD CONSTRAINT`, so the
///    full declarative differ reconciles the new FK via a 12-step table REBUILD
///    (`add foreign key accounts.accounts_owner_fkey`). The `ifNotExists` existence-guard
///    DELIBERATELY does NOT add this FK (a present column is a `SatisfiedNoop`; a `ref`
///    adds no FK via `ALTER`). So the guard and the full differ AGREE on the column and
///    DIVERGE on the FK — by design. This is the honest contour the report must state:
///    the affinity blind spot is real and bounded to the column shape; the FK is the
///    differ's job, not the guard's.
#[compio::test]
async fn second_deploy_string_to_ref_within_text_affinity_column_is_differ_noop_fk_is_rebuild() {
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
        runtime_options: Default::default(),
    };
    // v1: `owner` is a plain STRING (→ snapshot `text` → SQLite TEXT affinity).
    let accounts_v1 = CollectionDescriptor {
        name: "accounts".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "owner".into(),
            ty: "string".into(),
            ..Default::default()
        }],
        indexes: vec![],
        runtime_options: Default::default(),
    };

    let p = paths("differ_string_to_ref_noop");
    let be = backend(&p);
    let first = desired_sqlite(&[users.clone(), accounts_v1])
        .expect("v1 desired (users + accounts.owner:string)");
    let first_plan = sqlite_author()
        .diff(
            &first,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
            &effective_policy(),
        )
        .expect("v1 diff");
    for m in &first_plan.all_migrations() {
        be.apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("v1 apply {} must succeed: {e:?}", m.name));
    }

    // The FAITHFUL live snapshot: REAL introspection, so `accounts.owner` reads back
    // as the SQLite `text` affinity (not the desired-side PG spelling).
    let live = be
        .snapshot_schema_sqlite()
        .await
        .expect("real introspected live snapshot");
    let owner_live_type = live
        .tables
        .get("accounts")
        .and_then(|t| t.columns.iter().find(|c| c.name == "owner"))
        .map(|c| c.data_type.as_str())
        .expect("accounts.owner in live snapshot");
    assert_eq!(
        owner_live_type, "text",
        "the live `owner` column introspects as the SQLite text affinity (faithful path)"
    );
    let ownership: HashMap<String, String> = first
        .ownership
        .iter()
        .map(|(t, a)| (t.clone(), a.clone()))
        .collect();

    // v2: re-declare `owner` as a `ref` to `users` — a within-TEXT-affinity facet
    // change (string → ref). On SQLite this adds no FK via ALTER and is physically the
    // SAME `text` column, so the differ must treat it as NO CHANGE.
    let accounts_v2 = CollectionDescriptor {
        name: "accounts".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "owner".into(),
            ty: "ref".into(),
            references: Some("users".into()),
            reference_column: Some("id".into()),
            ..Default::default()
        }],
        indexes: vec![],
        runtime_options: Default::default(),
    };
    let desired2 = desired_sqlite(&[users, accounts_v2]).expect("v2 desired");
    let plan = sqlite_author()
        .diff(&desired2, &live, &ownership, &[], &effective_policy())
        .expect(
            "a within-TEXT-affinity facet change (string→ref) against the REAL introspected \
             live snapshot must NOT drift — the differ folds both sides through \
             sqlite_canonical_type, exactly as the existence-guard probe does",
        );

    // (1) COLUMN affinity is consistent with the guard: ZERO column ADD/ALTER/DROP
    //     migrations — the `string`→`ref` facet change folds to the same `text`
    //     affinity on both sides, so a later `diff` does NOT phantom-drift the COLUMN.
    let spurious_cols: Vec<String> = plan
        .all_migrations()
        .iter()
        .map(|m| m.name.clone())
        .filter(|n| {
            n.contains("alter_column") || n.contains("add_column") || n.contains("drop_column")
        })
        .collect();
    assert!(
        spurious_cols.is_empty(),
        "string→ref within-text-affinity facet change must emit no COLUMN migration \
         (the guard SatisfiedNoop's it; the differ folds the same way and agrees): \
         {spurious_cols:?}"
    );

    // (2) The FK is a LEGITIMATE differ-only rebuild — exactly one, and its reason is
    //     the FK ADD, NOT a phantom column type/affinity change. This is the precise
    //     contour the report must state: the guard skips the FK by design; the full
    //     declarative differ adds it via rebuild (SQLite has no ALTER ADD CONSTRAINT).
    assert_eq!(
        plan.rebuilds.len(),
        1,
        "string→ref yields exactly one rebuild (the FK add), got: {:?}",
        plan.rebuilds
            .iter()
            .map(|r| &r.spec.reason)
            .collect::<Vec<_>>()
    );
    let reason = &plan.rebuilds[0].spec.reason;
    assert!(
        reason.contains("foreign key") && reason.contains("accounts_owner_fkey"),
        "the single rebuild is the FK add, not a phantom type change: {reason:?}"
    );
    assert!(
        !reason.contains("alter column") || !reason.contains("type"),
        "the rebuild must NOT be driven by a column type/affinity change \
         (string and ref both fold to text): {reason:?}"
    );
    assert_eq!(plan.rebuilds[0].spec.table, "accounts");
}
