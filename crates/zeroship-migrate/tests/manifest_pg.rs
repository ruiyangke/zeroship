//! Faithful pre-apply **integrity-manifest gate** tests (v3 Plan F) against a
//! REAL Postgres (no shims).
//!
//! These exercise [`MigrationEngine::apply_verified`]: a correct expected
//! [`ManifestHash`] applies the set normally; a mismatched expected (the bundle
//! was tampered — reordered / content-edited / inserted-into / removed-from)
//! refuses with [`EngineError::Manifest`] and applies NOTHING — no project table
//! created, no completed journal rows, and (critically) the refusal happens
//! BEFORE the advisory lock / any DDL.
//!
//! Requires the dedicated database `zeroship_migrate_test` on :5440 and a
//! connecting role with `CREATEROLE` (the runbook uses `postgres`). Set
//! `MIGRATE_TEST_DB` to override the DSN. Each test uses uniquely-suffixed
//! schemas + project id (→ a unique role), so tests are isolated + re-runnable.

use std::collections::HashMap;

use compio_postgres::Client;
use zeroship_migrate::{
    author::{AuthorRequest, Column, DeterministicAuthor, MigrationAuthor},
    compute_manifest, desired_snapshot, migrator_role_name, provision_migrator,
    role::deprovision_migrator, snapshot_schema, Approval, CollectionDescriptor,
    DeclarativeApplyError, DeclarativeAuthor, EngineError, ExecutorConfig, FieldDescriptor,
    GuardConfig, ManifestHash, Migration, MigrationEngine, RenameHint, SchemaSnapshot,
};

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

fn dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
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

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.meta_schema = format!("meta_{tok}");
    let role = migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}

fn guard_cfg(cfg: &ExecutorConfig) -> GuardConfig {
    GuardConfig {
        project_schema: cfg.project_schema.clone(),
        extension_allowlist: Vec::new(),
    }
}

async fn ensure_project_schema(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{}\"",
        cfg.project_schema
    ))
    .await
    .expect("create project schema");
}

async fn setup(conn: &Client, cfg: &ExecutorConfig) {
    ensure_project_schema(conn, cfg).await;
    provision_migrator(conn, cfg)
        .await
        .expect("provision migrator role");
}

async fn teardown(conn: &Client, cfg: &ExecutorConfig) {
    let _ = deprovision_migrator(conn, cfg).await;
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.meta_schema
        ))
        .await;
}

async fn table_exists(conn: &Client, schema: &str, table: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.tables WHERE table_schema = $1 AND table_name = $2",
            &[&schema, &table],
        )
        .await
        .expect("query table existence");
    !rows.is_empty()
}

/// Does the meta schema's journal table exist at all? (Created by `ensure_journal`
/// on the FIRST apply that takes the lock. If the manifest gate refuses BEFORE the
/// lock, the journal table is never created.)
async fn journal_table_exists(conn: &Client, cfg: &ExecutorConfig) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.tables \
             WHERE table_schema = $1 AND table_name = 'schema_migrations'",
            &[&cfg.meta_schema],
        )
        .await
        .expect("query journal table existence");
    !rows.is_empty()
}

fn det(cfg: &ExecutorConfig) -> DeterministicAuthor {
    DeterministicAuthor::new(cfg.project_schema.clone(), "app_test")
}

/// A safe two-migration additive set (create + add column) authored cleanly.
fn additive_set(cfg: &ExecutorConfig) -> Vec<Migration> {
    let create = det(cfg)
        .author(&AuthorRequest::CreateTable {
            name: "orders".into(),
            columns: vec![Column {
                name: "id".into(),
                ty: "bigint".into(),
                nullable: false,
            }],
        })
        .unwrap();
    let add = det(cfg)
        .author(&AuthorRequest::AddColumn {
            table: "orders".into(),
            column: Column {
                name: "note".into(),
                ty: "text".into(),
                nullable: true,
            },
        })
        .unwrap();
    create.into_iter().chain(add).collect()
}

// ---------------------------------------------------------------------------
// Happy path — a correct expected manifest applies normally
// ---------------------------------------------------------------------------

#[compio::test]
async fn correct_manifest_applies_the_set_normally() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let set = additive_set(&cfg);
    // The control plane stamps this at review time.
    let expected = compute_manifest(&set);

    let engine = MigrationEngine::new();
    let outcome = engine
        .apply_verified(&set, &guard_cfg(&cfg), Some(&expected), Approval::None, &conn, &cfg, "app_test")
        .await
        .expect("a matching manifest must apply the set");

    assert_eq!(outcome.applied.len(), 2, "both migrations applied");
    assert!(table_exists(&conn, &cfg.project_schema, "orders").await);
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn none_expected_applies_unverified() {
    // expected = None ⇒ apply unverified (internal-caller back-compat-free path).
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let set = additive_set(&cfg);
    let engine = MigrationEngine::new();
    let outcome = engine
        .apply_verified(&set, &guard_cfg(&cfg), None, Approval::None, &conn, &cfg, "app_test")
        .await
        .expect("None expected applies unverified");
    assert_eq!(outcome.applied.len(), 2);
    assert!(table_exists(&conn, &cfg.project_schema, "orders").await);
    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// Tamper rejection — every mutation refuses, applying NOTHING, before the lock
// ---------------------------------------------------------------------------

/// Assert the refusal applied nothing AND happened before any DDL/journal work:
/// no project table, and the journal table itself was never created (it is
/// created by `ensure_journal` only AFTER the advisory lock is taken — i.e.
/// strictly after the manifest gate would have passed).
async fn assert_refused_before_apply(conn: &Client, cfg: &ExecutorConfig, err: EngineError) {
    assert!(
        matches!(err, EngineError::Manifest(_)),
        "expected a manifest mismatch, got {err:?}"
    );
    assert!(
        !table_exists(conn, &cfg.project_schema, "orders").await,
        "no project table may exist after a refused apply"
    );
    assert!(
        !journal_table_exists(conn, cfg).await,
        "the journal table must NOT exist — the gate refused BEFORE the lock / ensure_journal / any DDL"
    );
}

#[compio::test]
async fn cosmetic_slice_reorder_verifies_and_applies(/* M2 */) {
    // M2: the manifest is folded over the CANONICAL EXECUTED order, so a pure
    // SLICE reorder of an ADDITIVE set (no depends_on) is INVARIANT — the executor
    // re-sorts by version regardless, so the bundle arriving in a different slice
    // order than the control plane stamped must NOT false-mismatch. End-to-end:
    // the reordered slice verifies against the stamped hash and applies normally.
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let set = additive_set(&cfg);
    // The control plane stamps the manifest over the authored slice order.
    let expected = compute_manifest(&set);
    // The bundle arrives with the two migrations in a different slice order. They
    // carry no depends_on, so the canonical executed order (version order) is
    // identical — same hash.
    let reordered = vec![set[1].clone(), set[0].clone()];

    let engine = MigrationEngine::new();
    let outcome = engine
        .apply_verified(&reordered, &guard_cfg(&cfg), Some(&expected), Approval::None, &conn, &cfg, "app_test")
        .await
        .expect("a cosmetic slice reorder must verify against the stamped manifest (M2)");
    assert_eq!(outcome.applied.len(), 2, "both migrations applied");
    assert!(table_exists(&conn, &cfg.project_schema, "orders").await);
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn inserted_migration_is_refused_before_apply() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let set = additive_set(&cfg);
    let expected = compute_manifest(&set);
    // Attacker inserts an extra (even guard-safe) migration into the bundle.
    let extra = det(&cfg)
        .author(&AuthorRequest::CreateTable {
            name: "smuggled".into(),
            columns: vec![Column {
                name: "id".into(),
                ty: "bigint".into(),
                nullable: false,
            }],
        })
        .unwrap();
    let mut tampered = set.clone();
    tampered.extend(extra);

    let engine = MigrationEngine::new();
    let err = engine
        .apply_verified(&tampered, &guard_cfg(&cfg), Some(&expected), Approval::None, &conn, &cfg, "app_test")
        .await
        .expect_err("an inserted migration must be refused");
    assert_refused_before_apply(&conn, &cfg, err).await;
    // The smuggled table must NOT exist either.
    assert!(!table_exists(&conn, &cfg.project_schema, "smuggled").await);
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn removed_migration_is_refused_before_apply() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let set = additive_set(&cfg);
    let expected = compute_manifest(&set);
    // Attacker drops a migration from the reviewed bundle.
    let tampered = vec![set[0].clone()];

    let engine = MigrationEngine::new();
    let err = engine
        .apply_verified(&tampered, &guard_cfg(&cfg), Some(&expected), Approval::None, &conn, &cfg, "app_test")
        .await
        .expect_err("a removed migration must be refused");
    assert_refused_before_apply(&conn, &cfg, err).await;
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn content_edited_migration_is_refused_before_apply() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let set = additive_set(&cfg);
    let expected = compute_manifest(&set);
    // Attacker edits the FIRST migration's `up` in place (same version), so its
    // per-migration checksum changes ⇒ the manifest changes. Keep the version,
    // recompute the checksum to mirror a tampered-but-self-consistent bundle.
    let mut edited = set[0].clone();
    edited.up = format!(
        "CREATE TABLE \"{}\".\"orders\" (id bigint NOT NULL, evil text)",
        cfg.project_schema
    );
    edited.checksum =
        zeroship_migrate::Checksum::of(&zeroship_migrate::ChecksumInput::from_migration(&edited));
    let tampered = vec![edited, set[1].clone()];

    let engine = MigrationEngine::new();
    let err = engine
        .apply_verified(&tampered, &guard_cfg(&cfg), Some(&expected), Approval::None, &conn, &cfg, "app_test")
        .await
        .expect_err("a content-edited migration must be refused");
    assert_refused_before_apply(&conn, &cfg, err).await;
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn garbage_expected_hash_fails_closed() {
    // An expected hash that cannot match (wrong/short) refuses every set.
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let set = additive_set(&cfg);
    let garbage = ManifestHash::from_hex("deadbeef");

    let engine = MigrationEngine::new();
    let err = engine
        .apply_verified(&set, &guard_cfg(&cfg), Some(&garbage), Approval::None, &conn, &cfg, "app_test")
        .await
        .expect_err("a garbage expected hash must fail closed");
    assert_refused_before_apply(&conn, &cfg, err).await;
    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// M1 — the manifest is verified over the RAW supplied set, BEFORE plan()/guard
// filtering. A guard denial must NOT shrink the verified set into a spurious
// membership mismatch; the manifest matches the stamp and the denial is surfaced
// by the SEPARATE denial gate.
// ---------------------------------------------------------------------------

/// A guard-DENIED migration (COPY … TO PROGRAM = shell RCE) with a valid,
/// self-consistent checksum, so it is a legitimate MEMBER of the stamped set.
fn denied_migration(cfg: &ExecutorConfig) -> Migration {
    let up = format!("COPY \"{}\".\"orders\" TO PROGRAM 'sh -c id'", cfg.project_schema);
    let flags = zeroship_migrate::MigrationFlags::default();
    let checksum = zeroship_migrate::Checksum::of(&zeroship_migrate::ChecksumInput {
        up: &up,
        down: None,
        flags: &flags,
        owner_app: "app_test",
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });
    Migration {
        version: zeroship_migrate::MigrationId::generate(),
        name: "exfiltrate".into(),
        up,
        down: None,
        checksum,
        flags,
        owner_app: "app_test".into(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
    }
}

#[compio::test]
async fn guard_denied_member_matches_manifest_and_is_refused_by_the_denial_gate() {
    // M1: the verified set is the RAW supplied set, so a guard-denied migration is
    // STILL part of the manifest (matches the stamp) — there is NO spurious
    // manifest-membership mismatch from the denial shrinking the set. The denial is
    // surfaced by the separate, correct denial gate (EngineError::Denied), not by
    // the manifest gate.
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    // The reviewed bundle: one legitimate additive migration that the control
    // plane stamped ALONGSIDE a (guard-denied) migration. The stamp is over BOTH.
    let mut set = additive_set(&cfg);
    // Mint the denied member with a version that sorts FIRST so it is not dropped
    // by ordering; membership is what matters here.
    set.push(denied_migration(&cfg));
    let expected = compute_manifest(&set);

    let engine = MigrationEngine::new();
    let err = engine
        .apply_verified(&set, &guard_cfg(&cfg), Some(&expected), Approval::None, &conn, &cfg, "app_test")
        .await
        .expect_err("a guard-denied member must be refused");
    // The refusal is a DENIAL (the separate gate), NOT a manifest mismatch: the
    // raw set matched the stamp; verifying post-plan `items` would instead have
    // shrunk the set (dropping the denied migration) and false-mismatched.
    assert!(
        matches!(err, EngineError::Denied(_)),
        "expected EngineError::Denied (manifest matched the raw set; the denial is the separate gate), got {err:?}"
    );
    // Nothing applied: a denied plan applies nothing.
    assert!(!table_exists(&conn, &cfg.project_schema, "orders").await);
    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// H1 — the DECLARATIVE deploy path also has a set-level integrity gate.
//
// `apply_verified` gated the VERSIONED (hand-shipped) path. The declarative
// (AI-driven) path — `plan_declarative` → `apply_declarative` — had guard +
// least-priv role + denial/approval gate but NO manifest gate: between the
// control plane STAMPING the generated `DeclarativeDeployPlan` and APPLYING
// that same plan, a reorder / insert / remove / content-flip of its effective
// migration set was undetected. `apply_declarative_verified` closes that gap:
// it computes the manifest over the plan's FULL EFFECTIVE set (plain migrations
// PLUS, per rename, its expand AND contract migrations) and verifies it == the
// stamped hash BEFORE the outer advisory lock or any DDL. The stamp side uses
// the SAME `DeclarativeDeployPlan::manifest()` helper, so stamp + verify cannot
// diverge.
//
// Determinism caveat: declarative versions are freshly minted per
// `plan_declarative` call, so a stamp is only valid for ONE generated plan
// INSTANCE — the control plane must generate ONCE, then stamp + apply THAT
// plan. These tests honour that (one `plan_declarative`, then `.manifest()` it).
// ---------------------------------------------------------------------------

fn decl_author(cfg: &ExecutorConfig) -> DeclarativeAuthor {
    DeclarativeAuthor::new(cfg.project_schema.clone(), "app_test")
}

/// A plain (no-rename) additive declarative deploy plan: create one table.
fn plain_declarative_plan(
    engine: &MigrationEngine,
    cfg: &ExecutorConfig,
) -> zeroship_migrate::DeclarativeDeployPlan {
    let desc = CollectionDescriptor {
        name: "ledger".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "amount".into(),
            ty: "number".into(),
            required: false,
            unique: false,
            references: None,
        }],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot");
    engine
        .plan_declarative(
            &desired,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &decl_author(cfg),
            &[],
            &guard_cfg(cfg),
        )
        .expect("plan_declarative")
}

#[compio::test]
async fn declarative_verified_correct_manifest_applies_normally() {
    // Generate ONCE, stamp via the SAME helper the verify side uses, then apply
    // that plan verified — it applies normally (drift clean).
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let engine = MigrationEngine::new();
    let plan = plain_declarative_plan(&engine, &cfg);
    // The control plane stamps the manifest over the generated plan's effective
    // set using the SAME single implementation the verify side calls.
    let expected = plan.manifest();

    let outcome = engine
        .apply_declarative_verified(&plan, &expected, Approval::None, &conn, &cfg, "app_test")
        .await
        .expect("a matching declarative manifest must apply the plan");
    assert!(!outcome.applied.applied.is_empty(), "the plain set applied");
    assert!(table_exists(&conn, &cfg.project_schema, "ledger").await);
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn declarative_verified_tampered_plain_set_is_refused_before_apply() {
    // Tamper the plan's effective set AFTER the stamp: drop a plain migration
    // (membership shrink). Verifying the tampered plan against the ORIGINAL stamp
    // refuses — EngineError::Manifest — applying NOTHING (no table, no journal).
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let engine = MigrationEngine::new();
    let plan = plain_declarative_plan(&engine, &cfg);
    let expected = plan.manifest();
    assert!(!plan.plain.items.is_empty(), "plain set is non-empty");

    // Tamper: remove the last plain migration from the plan.
    let mut tampered = plan.clone();
    tampered.plain.items.pop();

    let err = engine
        .apply_declarative_verified(&tampered, &expected, Approval::None, &conn, &cfg, "app_test")
        .await
        .expect_err("a tampered (shrunk) plain set must be refused");
    assert!(
        matches!(err, DeclarativeApplyError::Plain(EngineError::Manifest(_))),
        "expected a manifest mismatch, got {err:?}"
    );
    // Nothing applied, and the journal table itself was never created (refused
    // BEFORE the outer lock / ensure_journal / any DDL).
    assert!(!table_exists(&conn, &cfg.project_schema, "ledger").await);
    assert!(
        !journal_table_exists(&conn, &cfg).await,
        "the journal table must NOT exist — refused BEFORE the lock / any DDL"
    );
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn declarative_verified_content_flipped_plain_migration_is_refused() {
    // Tamper: flip a field the Plan-F per-migration checksum covers (the `up`
    // text, recomputing the checksum to mirror a self-consistent tampered plan).
    // The manifest folds the per-migration checksum, so this is caught.
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let engine = MigrationEngine::new();
    let plan = plain_declarative_plan(&engine, &cfg);
    let expected = plan.manifest();

    let mut tampered = plan.clone();
    let item = &mut tampered.plain.items[0];
    item.migration.up = format!(
        "CREATE TABLE \"{}\".\"smuggled\" (id bigint)",
        cfg.project_schema
    );
    item.migration.checksum = zeroship_migrate::Checksum::of(
        &zeroship_migrate::ChecksumInput::from_migration(&item.migration),
    );

    let err = engine
        .apply_declarative_verified(&tampered, &expected, Approval::None, &conn, &cfg, "app_test")
        .await
        .expect_err("a content-flipped plain migration must be refused");
    assert!(
        matches!(err, DeclarativeApplyError::Plain(EngineError::Manifest(_))),
        "expected a manifest mismatch, got {err:?}"
    );
    assert!(!table_exists(&conn, &cfg.project_schema, "ledger").await);
    assert!(!table_exists(&conn, &cfg.project_schema, "smuggled").await);
    assert!(!journal_table_exists(&conn, &cfg).await);
    teardown(&conn, &cfg).await;
}

/// Build a rename-bearing declarative plan: create `users(email)`, then desire
/// `users(email_address)` with a matching rename hint — the rename is carried
/// STRUCTURED in `plan.renames` (expand + contract), the plain set is empty.
async fn rename_declarative_plan(
    engine: &MigrationEngine,
    cfg: &ExecutorConfig,
    conn: &Client,
) -> zeroship_migrate::DeclarativeDeployPlan {
    let author = decl_author(cfg);
    let v1 = CollectionDescriptor {
        name: "users".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "email".into(),
            ty: "string".into(),
            required: false,
            unique: false,
            references: None,
        }],
        indexes: vec![],
    };
    let d1 = desired_snapshot(&cfg.project_schema, &[v1]).expect("desired_snapshot");
    let create = engine
        .plan_declarative(&d1, &SchemaSnapshot::default(), &HashMap::new(), &author, &[], &guard_cfg(cfg))
        .expect("plan create");
    engine
        .apply(&create.plain, Approval::None, conn, cfg, "app_test")
        .await
        .expect("create users");

    let v2 = CollectionDescriptor {
        name: "users".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "email_address".into(),
            ty: "string".into(),
            required: false,
            unique: false,
            references: None,
        }],
        indexes: vec![],
    };
    let d2 = desired_snapshot(&cfg.project_schema, &[v2]).expect("desired_snapshot");
    let live = snapshot_schema(conn, &cfg.project_schema).await.expect("snap");
    let hints = vec![RenameHint {
        table: "users".into(),
        from: "email".into(),
        to: "email_address".into(),
    }];
    let plan = engine
        .plan_declarative(&d2, &live, &HashMap::new(), &author, &hints, &guard_cfg(cfg))
        .expect("plan rename");
    assert_eq!(plan.renames.len(), 1, "one structured rename");
    plan
}

#[compio::test]
async fn declarative_verified_rename_manifest_covers_expand_and_contract() {
    // The effective set the manifest covers must include each rename's expand AND
    // its DEFERRED contract migrations. A correct stamp applies the rename's
    // EXPAND normally and surfaces the deferred contract.
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let engine = MigrationEngine::new();
    let plan = rename_declarative_plan(&engine, &cfg, &conn).await;
    let expected = plan.manifest();

    let outcome = engine
        .apply_declarative_verified(&plan, &expected, Approval::Approved, &conn, &cfg, "app_test")
        .await
        .expect("a matching rename-bearing manifest must apply the expand");
    assert_eq!(outcome.pending_contract.len(), 2, "contract C1+C2 deferred");
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn declarative_verified_tampered_rename_expand_is_refused_before_apply() {
    // Tamper a RENAME's EXPAND migration (flip its `up`) AFTER the stamp. Because
    // the effective set the manifest covers includes the rename's expand AND
    // contract, this is caught — refused before any DDL, applying nothing.
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let engine = MigrationEngine::new();
    let plan = rename_declarative_plan(&engine, &cfg, &conn).await;
    let expected = plan.manifest();

    let mut tampered = plan.clone();
    // Flip the first expand migration's `up` (recompute its checksum so the
    // tampered plan is self-consistent — the manifest still differs).
    let exp = &mut tampered.renames[0].expand[0];
    exp.up = format!("{}\n-- tampered", exp.up);
    exp.checksum =
        zeroship_migrate::Checksum::of(&zeroship_migrate::ChecksumInput::from_migration(exp));

    let err = engine
        .apply_declarative_verified(&tampered, &expected, Approval::Approved, &conn, &cfg, "app_test")
        .await
        .expect_err("a tampered rename expand must be refused");
    assert!(
        matches!(err, DeclarativeApplyError::Plain(EngineError::Manifest(_))),
        "expected a manifest mismatch over the rename's effective set, got {err:?}"
    );
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn declarative_verified_tampered_rename_contract_is_refused_before_apply() {
    // The DEFERRED contract is part of the stamped effective set too. Tampering a
    // contract migration (the part that will only be applied in deploy N+1) is
    // caught at deploy N's verify — the stamp covers the WHOLE generated plan,
    // including the deferred drop.
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let engine = MigrationEngine::new();
    let plan = rename_declarative_plan(&engine, &cfg, &conn).await;
    let expected = plan.manifest();

    let mut tampered = plan.clone();
    let con = &mut tampered.renames[0].contract[0];
    con.up = format!("{}\n-- tampered contract", con.up);
    con.checksum =
        zeroship_migrate::Checksum::of(&zeroship_migrate::ChecksumInput::from_migration(con));

    let err = engine
        .apply_declarative_verified(&tampered, &expected, Approval::Approved, &conn, &cfg, "app_test")
        .await
        .expect_err("a tampered DEFERRED contract must be refused at deploy N");
    assert!(
        matches!(err, DeclarativeApplyError::Plain(EngineError::Manifest(_))),
        "the stamp must cover the deferred contract, got {err:?}"
    );
    teardown(&conn, &cfg).await;
}
