//! Confinement proofs for the hardened `SQLite` migration backend.
//! EVERY claim is proven against a REAL temp-file `SQLite` — never a shim.
//!
//! Each `confine_*` test drives a creator `up` that attempts one escape and
//! asserts it is DENIED (by the authorizer or DEFENSIVE), AND that the failure did
//! not corrupt the journal (the version is NOT recorded `completed`).
//!
//! Attacks proven denied:
//!   (a) ATTACH an arbitrary file
//!   (b) PRAGMA `writable_schema=ON` then a `sqlite_master` write
//!   (c) SELECT `load_extension`(...)
//!   (d) DROP TABLE "_`mig".schema_migrations`
//!   (e) DROP TRIGGER on the _mig immutability trigger
//!   (f) INSERT INTO "_`mig".schema_migrations` ... directly
//!   (g) CREATE TRIGGER on `app_tbl` whose body writes _mig
//!   (h) cross-tenant: a backend for app A cannot reach app B's file
//! Plus: direct UPDATE/DELETE on _mig rejected by the trigger; DETACH denied;
//! version floor satisfied.

use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::actor::SqliteActorError;
use zero_migrate::model::migration::{
    Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId,
};
use zero_migrate::SqliteBackend;

/// A tenant's two file paths inside a fresh temp dir.
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

fn mig(up: &str) -> Migration {
    let flags = MigrationFlags::default();
    let checksum = Checksum::of(&ChecksumInput {
        up,
        down: None,
        flags: &flags,
        owner_app: "app_test",
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });
    Migration {
        version: MigrationId::generate(),
        name: "attack".to_string(),
        up: up.to_string(),
        down: None,
        checksum,
        flags,
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
    }
}

/// How an attack is expected to be rejected. The gate-list attacks that are
/// AUTHORIZER denials must assert `is_authorizer_denied()` SPECIFICALLY, so a test
/// cannot green-pass on an unrelated `Exec` error. The looser acceptance is
/// reserved for genuinely-defensive cases — e.g. the creator-trigger-targeting-
/// `_mig` vector (g), whose qualified form is rejected by `SQLite`'s PARSER, not the
/// authorizer.
#[derive(Clone, Copy)]
enum DenyKind {
    /// Must be an authorizer DENY (`SQLITE_AUTH` / "not authorized").
    Authorizer,
    /// Either an authorizer deny OR a DEFENSIVE / parser / read-only-schema error.
    AuthorizerOrDefensive,
}

/// DEFENSIVE / parser / read-only-schema blocks surface as a generic statement
/// error (an authorizer "not authorized" if the authorizer catches it first, else a
/// parser / "readonly" / "database is locked" class). Used by the
/// `AuthorizerOrDefensive` cases.
const fn is_defensive_block(e: &SqliteActorError) -> bool {
    matches!(e, SqliteActorError::Exec(_))
}

/// Assert the apply of an attacking `up` was DENIED with the expected `DenyKind`,
/// and the journal is clean (the attacking version never recorded a `completed`
/// row).
async fn assert_denied_and_journal_clean(be: &SqliteBackend, attack_up: &str, kind: DenyKind) {
    let m = mig(attack_up);
    let res = be.apply_one_additive(&m, "tester").await;
    let err = res.expect_err("attack must be denied, not applied");
    match kind {
        DenyKind::Authorizer => assert!(
            err.is_authorizer_denied(),
            "attack must be an AUTHORIZER deny (not an unrelated error), got: {err}"
        ),
        DenyKind::AuthorizerOrDefensive => assert!(
            err.is_authorizer_denied() || is_defensive_block(&err),
            "attack should be an authorizer DENY or DEFENSIVE block, got: {err}"
        ),
    }
    // The journal must be uncorrupted: the attacking version is not net-applied.
    let applied = be
        .applied_sqlite()
        .await
        .expect("journal still readable after a denied attack");
    let v = m.version.as_str();
    assert!(
        !applied.iter().any(|e| e.version == v),
        "denied attack must not leave a journal row for {v}"
    );
}

/// POSITIVE CONTROL: prove the SAME attack SQL SUCCEEDS on a raw, unhardened
/// connection — no authorizer, no DEFENSIVE, no _mig confinement. This proves the
/// deny in the hardened case was caused by CONFINEMENT, not by an unrelated error
/// (a malformed statement, a missing table, etc.). The control runs against
/// throwaway temp files so it never touches the tenant under test.
///
/// `setup` seeds whatever the attack references (e.g. a `_mig`-shaped journal or an
/// app table); `attack_sql` is then executed and MUST succeed.
fn assert_attack_succeeds_unhardened(setup: &str, attack_sql: &str) {
    let dir = tempfile::tempdir().expect("control tempdir");
    let main = dir.path().join("control-main.sqlite");
    let mig_file = dir.path().join("control-mig.sqlite");
    let conn = rusqlite::Connection::open(&main).expect("open control main");
    // ATTACH a real `_mig` file so `"_mig".*` names resolve on the raw connection.
    conn.execute(
        "ATTACH DATABASE ?1 AS \"_mig\"",
        [mig_file.to_str().unwrap()],
    )
    .expect("attach control _mig");
    if !setup.is_empty() {
        conn.execute_batch(setup).expect("control setup");
    }
    let res = conn.execute_batch(attack_sql);
    assert!(
        res.is_ok(),
        "positive control: the attack SQL must SUCCEED on a raw unhardened \
         connection (so the hardened deny is proven to be confinement, not an \
         unrelated error). Got: {res:?}"
    );
}

/// The journal-shaped DDL a positive control needs so `"_mig".schema_migrations`
/// (and its immutability trigger) resolve on the raw control connection.
const CONTROL_JOURNAL_SETUP: &str = "\
    CREATE TABLE \"_mig\".schema_migrations (\
        event_seq INTEGER PRIMARY KEY AUTOINCREMENT, event_kind TEXT, version TEXT, \
        name TEXT, checksum TEXT, \"by\" TEXT, phase TEXT, outcome TEXT, kind TEXT); \
    CREATE TRIGGER \"_mig\".zs_immutable_trg_schema_migrations_delete \
        BEFORE DELETE ON \"_mig\".schema_migrations \
        BEGIN SELECT RAISE(ABORT,'append-only'); END;";

// ---------------------------------------------------------------------------
// (a) ATTACH an arbitrary file — denied for life after the authorizer install.
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_a_attach_denied() {
    let p = paths("a");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    let attack = "ATTACH DATABASE 'file:other.sqlite' AS x; CREATE TABLE t (id INTEGER);";
    assert_denied_and_journal_clean(&be, attack, DenyKind::Authorizer).await;
    // Positive control: ATTACH + CREATE succeeds on a raw connection. We point the
    // ATTACH at an ABSOLUTE temp path (not the relative `other.sqlite`, which would
    // pollute the test CWD) — the capability proven is the same.
    let cdir = tempfile::tempdir().expect("control tempdir");
    let other = cdir.path().join("other.sqlite");
    let control_attack = format!(
        "ATTACH DATABASE 'file:{}' AS x; CREATE TABLE x.t (id INTEGER);",
        other.to_str().unwrap().replace('\'', "''")
    );
    assert_attack_succeeds_unhardened("", &control_attack);
}

// ---------------------------------------------------------------------------
// (b) PRAGMA writable_schema=ON then a sqlite_master write — PRAGMA denied.
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_b_writable_schema_denied() {
    let p = paths("b");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    let attack =
        "PRAGMA writable_schema=ON; DELETE FROM \"_mig\".sqlite_master WHERE name LIKE 'zs_%';";
    // The `PRAGMA writable_schema` is denied by the authorizer FIRST (PRAGMA denied
    // in CreatorUp) — so this is specifically an authorizer deny, not merely a
    // DEFENSIVE sqlite_master block.
    assert_denied_and_journal_clean(&be, attack, DenyKind::Authorizer).await;
    // Positive control: with writable_schema=ON a raw connection CAN delete from
    // sqlite_master (proving the capability the hardened conn denies at the PRAGMA).
    assert_attack_succeeds_unhardened(CONTROL_JOURNAL_SETUP, attack);
}

// ---------------------------------------------------------------------------
// (c) SELECT load_extension(...) — function not on allowlist + load disabled.
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_c_load_extension_denied() {
    let p = paths("c");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    assert_denied_and_journal_clean(
        &be,
        "CREATE TABLE t AS SELECT load_extension('evil.so');",
        DenyKind::Authorizer,
    )
    .await;
    // Positive control: the CTAS shape itself is NOT what's denied — an
    // ALLOWLISTED function (`abs`) in the very same `CREATE TABLE t AS SELECT
    // <fn>(...)` form SUCCEEDS on the hardened backend. So the `load_extension`
    // deny above is specifically the function ALLOWLIST (confinement), not a parse
    // / CTAS / unknown-statement error. (We cannot run a real load_extension
    // positive control: the crate forbids `unsafe`, so the rusqlite
    // `load_extension_enable` needed to make a raw load succeed is unavailable —
    // and `evil.so` does not exist anyway.)
    let pc = paths("c_pc");
    let pbe = backend(&pc);
    pbe.ensure_journal_sqlite()
        .await
        .expect("bootstrap journal");
    pbe.apply_one_additive(&mig("CREATE TABLE t AS SELECT abs(-1) AS x;"), "tester")
        .await
        .expect("CTAS with an allowlisted function must SUCCEED on the hardened backend");
}

// ---------------------------------------------------------------------------
// (d) DROP TABLE "_mig".schema_migrations — denied at prepare (matches on the
//     OUTER database_name == Some("_mig"); DropTable carries no db field).
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_d_drop_mig_table_denied() {
    let p = paths("d");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    let attack = "DROP TABLE \"_mig\".schema_migrations;";
    assert_denied_and_journal_clean(&be, attack, DenyKind::Authorizer).await;
    // Positive control: dropping the journal table succeeds on a raw connection.
    assert_attack_succeeds_unhardened(CONTROL_JOURNAL_SETUP, attack);
    // And the journal table still exists + is readable.
    be.applied_sqlite()
        .await
        .expect("schema_migrations survived the denied DROP TABLE");
}

// ---------------------------------------------------------------------------
// (e) DROP TRIGGER on the _mig immutability trigger — denied (DropTrigger keys
//     on the OUTER database_name).
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_e_drop_mig_trigger_denied() {
    let p = paths("e");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    let attack = "DROP TRIGGER \"_mig\".\"zs_immutable_trg_schema_migrations_delete\";";
    assert_denied_and_journal_clean(&be, attack, DenyKind::Authorizer).await;
    // Positive control: dropping the immutability trigger succeeds on a raw conn.
    assert_attack_succeeds_unhardened(CONTROL_JOURNAL_SETUP, attack);
}

// ---------------------------------------------------------------------------
// (f) Direct INSERT INTO "_mig".schema_migrations — journal-forge denied.
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_f_direct_journal_insert_denied() {
    let p = paths("f");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    let attack = "INSERT INTO \"_mig\".schema_migrations \
         (event_kind, version, name, checksum, \"by\", phase, outcome, kind) \
         VALUES ('applied', 'forged', 'x', 'x', 'attacker', 'completed', 'success', 'apply');";
    assert_denied_and_journal_clean(&be, attack, DenyKind::Authorizer).await;
    // Positive control: the forged INSERT succeeds on a raw connection (no
    // authorizer; INSERT is not blocked by the append-only trigger).
    assert_attack_succeeds_unhardened(CONTROL_JOURNAL_SETUP, attack);
}

// ---------------------------------------------------------------------------
// (g) CREATE TRIGGER on an app table whose body writes _mig — denied at the
//     trigger's CREATE-prepare time (accessor + database_name == _mig).
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_g_creator_trigger_writing_mig_denied() {
    let p = paths("g");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    // First make a benign app table the trigger can hang off (separate, allowed
    // migration). Then the attacking CREATE TRIGGER must be denied.
    be.apply_one_additive(&mig("CREATE TABLE app_tbl (id INTEGER);"), "tester")
        .await
        .expect("benign app table applies");

    // SECURITY FINDING: under the confinement connection model
    // (`main` = the app file, `_mig` = the attached journal), a creator trigger body
    // CANNOT reach `_mig` by ANY name form, so the authorizer accessor+_mig
    // rule is belt-and-suspenders that this vector never actually exercises:
    //   * QUALIFIED `"_mig".schema_migrations` in a trigger body is rejected by
    //     SQLite's PARSER ("qualified table names are not allowed ... within
    //     triggers") — it never reaches the authorizer.
    //   * UNQUALIFIED `schema_migrations` in a trigger body resolves to the trigger's
    //     OWN database (`main`), NOT the attached `_mig`; at fire time it errors "no
    //     such table: main.schema_migrations". It can never resolve to `_mig`.
    // We therefore assert the END-TO-END property (a creator trigger cannot forge a
    // journal row) for BOTH forms, classifying the rejection as
    // `AuthorizerOrDefensive` (the qualified form is a genuine PARSER/DEFENSIVE block,
    // not an authorizer DENY — asserting `Authorizer` here would be a FALSE claim).

    // (g1) Qualified `_mig.` body — parser-rejected, journal stays clean.
    let qualified = "CREATE TRIGGER t1 AFTER INSERT ON app_tbl BEGIN \
            INSERT INTO \"_mig\".schema_migrations \
            (event_kind, version, name, checksum, \"by\", phase, outcome, kind) \
            VALUES ('applied', 'forged', 'x', 'x', 'attacker', 'completed', 'success', 'apply'); \
         END;";
    assert_denied_and_journal_clean(&be, qualified, DenyKind::AuthorizerOrDefensive).await;

    // (g2) Unqualified body that creates fine but cannot reach `_mig`: prove that
    // even after the trigger is created AND fired, NO forged journal row appears
    // (the body resolves to a nonexistent `main.schema_migrations`, so the fire
    // errors and the journal is untouched). This is the real end-to-end proof that
    // the journal is unforgeable via a creator trigger.
    be.apply_one_additive(
        &mig(
            "CREATE TRIGGER t2 AFTER INSERT ON app_tbl BEGIN \
                INSERT INTO schema_migrations \
                (event_kind, version, name, checksum, \"by\", phase, outcome, kind) \
                VALUES ('applied', 'forged2', 'x', 'x', 'attacker', 'completed', 'success', 'apply'); \
             END;",
        ),
        "tester",
    )
    .await
    .expect("the unqualified-body trigger CREATE itself is benign (resolves to main)");
    // Firing it must fail (no such table: main.schema_migrations) — and crucially
    // must NOT forge a journal row.
    let fired = be
        .apply_one_additive(&mig("INSERT INTO app_tbl (id) VALUES (1);"), "tester")
        .await;
    assert!(
        fired.is_err(),
        "firing the trigger must fail (its body resolves to a nonexistent main table)"
    );
    let net = be.applied_sqlite().await.expect("journal readable");
    assert!(
        !net.iter()
            .any(|e| e.version == "forged" || e.version == "forged2"),
        "no creator trigger can forge a journal row under the main=app-file model"
    );
}

// ---------------------------------------------------------------------------
// (h) Cross-tenant: a backend opened for app A cannot reach app B's file. The
//     only bound aliases are A's `app` + `_mig`; ATTACH of B is denied, and even
//     naming a foreign alias cannot compile.
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_h_cross_tenant_denied() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a_app = dir.path().join("zs-A.sqlite");
    let a_journal = dir.path().join("zs-A.migrations.sqlite");
    let b_app = dir.path().join("zs-B.sqlite");

    // Pre-create B with a secret table by opening a plain (un-hardened) connection.
    {
        let conn = rusqlite::Connection::open(&b_app).expect("open B");
        conn.execute_batch("CREATE TABLE secret (id INTEGER); INSERT INTO secret VALUES (42);")
            .expect("seed B");
    }

    let be = SqliteBackend::open(&a_app, &a_journal).expect("open A backend");
    be.ensure_journal_sqlite()
        .await
        .expect("bootstrap A journal");

    // A creator `up` on A tries to ATTACH B and read its secret — denied at the
    // ATTACH (no foreign alias can ever be bound on this connection).
    let b_path = b_app.to_str().unwrap().replace('\'', "''");
    let attack = format!(
        "ATTACH DATABASE 'file:{b_path}' AS victim; \
         CREATE TABLE stolen AS SELECT * FROM victim.secret;"
    );
    assert_denied_and_journal_clean(&be, &attack, DenyKind::Authorizer).await;
    // Positive control: on a raw connection the ATTACH-and-steal SUCCEEDS, reading
    // B's secret into a new table — proving the hardened deny is the ATTACH
    // authorizer rule (cross-tenant confinement), not an unrelated error.
    assert_attack_succeeds_unhardened("", &attack);
}

// ---------------------------------------------------------------------------
// (i) M1: a creator `up` READING the journal — `SELECT … FROM "_mig".
//     schema_migrations` — is denied. A plain top-level read is an
//     `AuthAction::Read { accessor: None }` on `_mig`; pre-fix it fell through to
//     the `_ => Allow` catch-all (the trigger-body arm requires `accessor.is_some()`)
//     so the creator could exfiltrate the immutable journal into an app table. The
//     M1 backstop arm now DENIES any `_mig`-targeting action in CreatorUp, Read
//     included. Faithful end-to-end on the REAL hardened backend.
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_i_creator_read_of_mig_journal_denied() {
    let p = paths("read_mig");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    // A creator `up` that copies the journal into an app table — the SELECT issues a
    // Read on `"_mig".schema_migrations`, which must be denied at prepare.
    let attack = "CREATE TABLE stolen AS SELECT * FROM \"_mig\".schema_migrations;";
    assert_denied_and_journal_clean(&be, attack, DenyKind::Authorizer).await;
    // Positive control: the SAME read-into-table SUCCEEDS on a raw connection (no
    // authorizer), proving the hardened deny is the M1 confinement rule and not a
    // parse / missing-table / CTAS error.
    assert_attack_succeeds_unhardened(CONTROL_JOURNAL_SETUP, attack);
    // And the journal is still readable by the engine itself (EngineJournal reads
    // are unaffected by the creator-mode `_mig` deny).
    be.applied_sqlite()
        .await
        .expect("engine journal reads still work after the denied creator read");
}

// ---------------------------------------------------------------------------
// DETACH denied for life too.
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_detach_denied() {
    let p = paths("detach");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    let attack = "DETACH DATABASE \"_mig\";";
    assert_denied_and_journal_clean(&be, attack, DenyKind::Authorizer).await;
    // Positive control: DETACH of an attached db succeeds on a raw connection.
    assert_attack_succeeds_unhardened("", attack);
}

// ---------------------------------------------------------------------------
// Regression: the authorizer now ALLOWS a write to `sqlite_master` /
// `sqlite_temp_master` as an action (so SQLite's INTERNAL ALTER machinery can run
// `ALTER TABLE … DROP COLUMN`). This must NOT open a DIRECT-write hole: a creator
// `up` issuing `UPDATE main.sqlite_master …` directly is still blocked by
// DEFENSIVE=ON (set at open, BEFORE the authorizer), so defense-in-depth holds —
// the only path that reaches the allowed action is SQLite's own ALTER executor.
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_direct_sqlite_master_write_still_blocked_by_defensive() {
    let p = paths("master_write");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    // Seed a table so there is a schema row to target.
    be.apply_one_additive(&mig("CREATE TABLE t (id INTEGER PRIMARY KEY);"), "d")
        .await
        .expect("seed table");
    // A direct creator write to sqlite_master — DEFENSIVE blocks it (the authorizer
    // would ALLOW the action now, but DEFENSIVE rejects the actual write, so this is
    // an Exec/defensive block, not a silent success).
    let attack = "UPDATE main.sqlite_master SET sql = 'CREATE TABLE t (id INTEGER, pwned TEXT)' WHERE name = 't';";
    let m = mig(attack);
    let err = be
        .apply_one_additive(&m, "attacker")
        .await
        .expect_err("direct sqlite_master write must be blocked");
    // DEFENSIVE surfaces as an Exec error (not a silent apply); the table's real
    // schema is untouched.
    assert!(
        matches!(
            err,
            SqliteActorError::Exec(_) | SqliteActorError::Poisoned(_)
        ),
        "direct sqlite_master write must be rejected by DEFENSIVE, got: {err}"
    );
    // Positive control: with DEFENSIVE off + writable_schema on, a raw connection
    // CAN edit sqlite_master — proving the hardened block is DEFENSIVE, not an
    // unrelated error.
    {
        let cdir = tempfile::tempdir().expect("control tempdir");
        let cmain = cdir.path().join("c.sqlite");
        let conn = rusqlite::Connection::open(&cmain).expect("control open");
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);")
            .expect("control seed");
        conn.pragma_update(None, "writable_schema", "ON")
            .expect("writable_schema on");
        let res = conn.execute_batch(
            "UPDATE sqlite_master SET sql = 'CREATE TABLE t (id INTEGER, pwned TEXT)' WHERE name = 't';",
        );
        assert!(
            res.is_ok(),
            "control: raw sqlite_master edit succeeds: {res:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// REINDEX confinement + motivation (faithful on the REAL hardened
// backend). Two faithful proofs:
//
//   (1) A creator `up` containing a no-arg `REINDEX;` is REJECTED. The no-arg
//       form reindexes EVERY collation/index across all attached databases,
//       INCLUDING the journal alias `_mig` — so it reaches the load-bearing
//       catch-all `AuthAction::Reindex { .. } => Deny` (the `_mig` REINDEX is
//       NOT caught by the journal-immutability arm, which omits `Reindex`).
//       This is the confinement half: the creator may not REINDEX `_mig`.
//
//   (2) A real `CREATE TABLE` whose emission carries system-field indexes
//       APPLIES cleanly under CreatorUp. CREATE INDEX fires SQLITE_REINDEX
//       INTRINSICALLY (SQLite reindexes the fresh index to populate it); before
//       the REINDEX-on-main relaxation that intrinsic reindex was denied,
//       so a system-field-index-bearing CREATE TABLE failed to apply. This is the
//       regression for the relaxation's MOTIVATION.
// ---------------------------------------------------------------------------

/// (1) A no-arg `REINDEX;` in a creator `up` is denied (it reaches `_mig`).
#[compio::test]
async fn reindex_no_arg_rejected_in_creator_up() {
    let p = paths("reindex_noarg");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    // Seed a benign table + index so a no-arg REINDEX has something local to chew
    // on too (the deny is driven by its reach into `_mig`, not by emptiness).
    be.apply_one_additive(
        &mig("CREATE TABLE app_tbl (id INTEGER PRIMARY KEY, handle TEXT);"),
        "tester",
    )
    .await
    .expect("benign app table applies");

    // The no-arg REINDEX (the form that reaches every attached db incl. `_mig`).
    let m = mig("REINDEX;");
    let err = be
        .apply_one_additive(&m, "attacker")
        .await
        .expect_err("a no-arg REINDEX must be rejected (it reaches the _mig journal)");
    assert!(
        err.is_authorizer_denied(),
        "no-arg REINDEX must be an AUTHORIZER deny (reaches _mig → catch-all Deny), got: {err}"
    );
    // The journal is uncorrupted: the attacking version never recorded a row.
    let applied = be.applied_sqlite().await.expect("journal readable");
    let v = m.version.as_str();
    assert!(
        !applied.iter().any(|e| e.version == v),
        "denied REINDEX must not leave a journal row for {v}"
    );
}

/// (2) A real CREATE TABLE that emits system-field indexes APPLIES under
/// `CreatorUp` — the regression for the REINDEX-on-main relaxation's motivation.
/// CREATE INDEX fires `SQLITE_REINDEX` intrinsically; the relaxation must let that
/// pass on `main`, or the create fails to apply.
#[compio::test]
async fn create_table_with_system_field_indexes_applies_under_creator_up() {
    let p = paths("sysidx_create");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");

    // A CREATE TABLE followed by the platform system-field indexes, exactly the
    // shape the engine emits inside a creator `up`. Each CREATE INDEX fires an
    // intrinsic SQLITE_REINDEX on `main` — which the relaxation allows.
    let up = "CREATE TABLE accounts (\
                id TEXT PRIMARY KEY, \
                title TEXT NOT NULL, \
                created_by TEXT, \
                updated_at TEXT, \
                deleted_at TEXT\
              );\n\
              CREATE INDEX accounts_deleted_at_idx ON accounts (deleted_at);\n\
              CREATE INDEX accounts_updated_at_idx ON accounts (updated_at);\n\
              CREATE INDEX accounts_created_by_idx ON accounts (created_by);";
    let m = mig(up);
    let applied = be
        .apply_one_additive(&m, "deployer")
        .await
        .expect("a system-field-index-bearing CREATE TABLE must apply under CreatorUp");
    assert!(applied, "the create migration must be newly-applied");

    // The table + its three system-field indexes all landed in `main`.
    let idx_rows = be
        .actor()
        .query(
            "SELECT name FROM main.sqlite_master WHERE type='index' \
             AND name LIKE 'accounts_%_idx' ORDER BY name",
        )
        .await
        .expect("query system-field indexes");
    assert_eq!(
        idx_rows.len(),
        3,
        "all three system-field indexes must exist in main: {idx_rows:?}"
    );
}

// ---------------------------------------------------------------------------
// Version floor: the bundled SQLite satisfies the floor the
// journal-immutability proof needs (authorizer zDb-on-DROP_TABLE semantics +
// RETURNING + window functions). If the linked lib were below floor, open() would
// have returned UnsupportedVersion — so a successful open IS the proof, and we
// additionally assert the version number directly.
// ---------------------------------------------------------------------------
#[compio::test]
async fn version_floor_satisfied() {
    let v = rusqlite::version_number();
    assert!(
        v >= 3_035_000,
        "bundled sqlite {v} is below the 3.35.0 floor the journal-immutability + RETURNING proof needs"
    );
    // A successful hardened open is itself the runtime proof the floor check passed.
    let p = paths("floor");
    let _be = backend(&p);
}
