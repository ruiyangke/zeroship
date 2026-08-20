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
//!
//! Two of the hardened open sequence's dbconfig settings are pinned here rather
//! than in `sqlite_dqs_hardening.rs`, because what they buy is confinement (what a
//! hostile creator `up` may reach) rather than identifier resolution:
//!   `SQLITE_DBCONFIG_DEFENSIVE` - a creator write to an FTS5 SHADOW TABLE
//!   `SQLITE_DBCONFIG_TRUSTED_SCHEMA` - a creator VIEW body invoking a virtual
//!   table, including one that reads the `_mig` journal's catalog
//! Both carry a positive control on a raw connection, which ships DEFENSIVE off
//! and TRUSTED_SCHEMA on, so each of those two lines is the whole guard.
//!
//! One capability is CONCEDED rather than denied, and is pinned here on both
//! sides: `PRAGMA data_version` on the APP database, which FTS5 issues internally
//! on every route into an index. A creator can read that counter (stated as a test,
//! not a comment); the same pragma on the `_mig` journal stays denied, which is
//! what proves the grant is scoped by database rather than by pragma name. The
//! `'delete-all'` command that grant puts within a creator's reach is recorded too:
//! it wipes an FTS index past DEFENSIVE, and it is a cheaper spelling of the DROP a
//! creator could always issue.

use std::path::PathBuf;

use rusqlite::config::DbConfig;
use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::actor::SqliteActorError;
use zero_migrate::apply::backend::sqlite::authorizer::Mode;
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
    mig_with_flags(up, MigrationFlags::default())
}

fn mig_with_flags(up: &str, flags: MigrationFlags) -> Migration {
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
        effect: None,
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
// `up` issuing `UPDATE main.sqlite_master ...` directly is still rejected, so
// defense-in-depth holds - the only path that reaches the allowed action is
// SQLite's own ALTER executor.
//
// WHAT REJECTS IT, measured rather than assumed. `sqlite_master` is writable only
// while `writable_schema` is ON, and it is OFF by default on every connection; the
// authorizer independently denies the `PRAGMA writable_schema=ON` that would turn
// it on (proven by `confine_b_writable_schema_denied`). So the rejection here is
// the default plus that PRAGMA deny, NOT `SQLITE_DBCONFIG_DEFENSIVE`: flipping
// DEFENSIVE to false leaves this test green and the error text byte-identical
// (`table sqlite_master may not be modified`). This test was previously named for
// DEFENSIVE, which overstated what it holds. DEFENSIVE is pinned separately by
// `confine_creator_write_to_an_fts5_shadow_table_denied_by_defensive`, on the
// vector where it is the only guard.
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_direct_sqlite_master_write_blocked_by_writable_schema_off() {
    let p = paths("master_write");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    // Seed a table so there is a schema row to target.
    be.apply_one_additive(&mig("CREATE TABLE t (id INTEGER PRIMARY KEY);"), "d")
        .await
        .expect("seed table");
    // A direct creator write to sqlite_master. The authorizer would ALLOW the
    // action, but SQLite rejects the write itself because `writable_schema` is off,
    // so this is a statement error and not a silent success.
    let attack = "UPDATE main.sqlite_master SET sql = 'CREATE TABLE t (id INTEGER, pwned TEXT)' WHERE name = 't';";
    let m = mig(attack);
    let err = be
        .apply_one_additive(&m, "attacker")
        .await
        .expect_err("direct sqlite_master write must be blocked");
    assert!(
        matches!(
            err,
            SqliteActorError::Exec(_) | SqliteActorError::Poisoned(_)
        ),
        "direct sqlite_master write must be rejected at statement execution, got: {err}"
    );
    // The error names the schema table and says it may not be modified. A bare
    // is_err would also pass on a syntax error or a missing table.
    let text = err.to_string();
    assert!(
        text.contains("sqlite_master") && text.contains("may not be modified"),
        "the rejection must be the read-only-schema one naming sqlite_master: {text}"
    );
    // The schema row is untouched: no `pwned` column was smuggled in.
    let sql = be
        .actor()
        .query("SELECT sql FROM main.sqlite_master WHERE name = 't'")
        .await
        .expect("read back the schema row");
    assert!(
        !format!("{sql:?}").contains("pwned"),
        "the rejected write must not have edited the stored schema: {sql:?}"
    );
    // Positive control: with writable_schema ON a raw connection CAN edit
    // sqlite_master - proving the hardened block is that setting being off, not an
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
// SQLITE_DBCONFIG_DEFENSIVE: a creator `up` may not write an FTS5 SHADOW TABLE.
//
// The reachable shape, end to end on the real backend. The tenant's app file holds
// an FTS5 index -- put there by an older engine or by a data-plane runtime, since
// this engine authors no FTS DDL and the authorizer now denies
// `CREATE VIRTUAL TABLE ... USING fts5(...)` in EVERY mode. The seed is therefore a
// raw connection; what matters to this test is that the objects EXIST, not who made
// them. FTS5 lays down its b-tree in ORDINARY tables next to the vtable -
// `<name>_data`, `<name>_idx`, `<name>_docsize`, `<name>_config` - in `main`. A
// LATER ordinary creator `up` then runs in CreatorUp against that same `main`,
// where the authorizer's own rules permit creator DML on any table that is neither
// a schema table nor in `_mig`. Those shadow tables are exactly that, so the
// authorizer ALLOWS the write and DEFENSIVE is the only thing that refuses it.
//
// A FAILURE HERE means a creator `up` can rewrite the internal b-tree of an FTS5
// index, which is database corruption reachable from tenant-authored SQL. The
// control demonstrates that outcome rather than asserting it: on a raw connection
// (which ships DEFENSIVE off) the same DELETE succeeds and the next MATCH query
// fails with `fts5: corruption found reading blob ...`.
// ---------------------------------------------------------------------------

/// The FTS5 shadow table holding the index b-tree for `docs__fts`. Writing it is
/// what DEFENSIVE refuses; FTS5 itself reaches it through the virtual table.
const FTS_SHADOW_TABLE: &str = "docs__fts_data";

/// Seed `docs` + its FTS5 index into the app file on a RAW connection, the way a
/// LEGACY database carries them, BEFORE the hardened backend opens it.
///
/// WHY RAW, since the engine used to author this. Full-text support was removed:
/// the engine emits no FTS DDL and the authorizer no longer admits a
/// `CREATE VIRTUAL TABLE … USING fts5(…)` in ANY mode. But removing the FEATURE
/// did not remove the DATA — databases in the field still hold these objects, put
/// there by an older engine or by a data-plane runtime that manages its own
/// indexes. DEFENSIVE is what stops a creator `up` rewriting their b-tree, and this
/// is its only test vector, so the seed moved to a raw connection rather than the
/// guard losing its pin. The DDL is spelled literally here for the same reason: the
/// shared builder it used to call no longer exists.
fn seed_legacy_fts_index(app: &std::path::Path, with_row: bool) {
    let conn = rusqlite::Connection::open(app).expect("open the app file raw to seed");
    conn.execute_batch(
        "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT);\n\
         CREATE VIRTUAL TABLE \"docs__fts\" \
         USING fts5(\"body\", content=\"docs\", content_rowid=\"rowid\");",
    )
    .expect("seed the legacy FTS5 index");
    if with_row {
        conn.execute_batch(
            "INSERT INTO docs (body) VALUES ('hello world');\n\
             INSERT INTO \"docs__fts\" (rowid, \"body\") SELECT rowid, body FROM docs;",
        )
        .expect("seed a row into the legacy index");
    }
}

#[compio::test]
async fn confine_creator_write_to_an_fts5_shadow_table_denied_by_defensive() {
    let p = paths("fts_shadow");
    seed_legacy_fts_index(&p.app, false);
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");

    // The shadow tables now exist in `main`, reachable by name from creator SQL.
    let shadow = be
        .actor()
        .query(&format!(
            "SELECT name FROM main.sqlite_master WHERE name = '{FTS_SHADOW_TABLE}'"
        ))
        .await
        .expect("read main schema");
    assert_eq!(
        shadow.len(),
        1,
        "the FTS5 shadow table must exist for this attack to be the one under test: {shadow:?}"
    );
    let before = be
        .actor()
        .query(&format!("SELECT count(*) FROM \"{FTS_SHADOW_TABLE}\""))
        .await
        .expect("read the shadow table");

    // NEIGHBOUR EXCLUSION, first half: the authorizer does NOT deny creator DML on
    // `main`. An ordinary creator write to the tenant table succeeds on this exact
    // connection, so a rejection below cannot be "creator writes are denied".
    be.apply_one_additive(
        &mig("INSERT INTO docs (body) VALUES ('hello world');"),
        "creator",
    )
    .await
    .expect("an ordinary creator write to a main table must succeed");

    // The attack: a creator `up` rewriting the FTS5 index b-tree directly.
    let attack = format!("DELETE FROM \"{FTS_SHADOW_TABLE}\";");
    let m = mig(&attack);
    let err = be
        .apply_one_additive(&m, "attacker")
        .await
        .expect_err("a creator write to an FTS5 shadow table must be refused");
    // NEIGHBOUR EXCLUSION, second half: this is NOT an authorizer deny. If it were,
    // the test would pass with DEFENSIVE off and pin nothing.
    assert!(
        !err.is_authorizer_denied(),
        "the shadow-table write must be refused by DEFENSIVE, not by the authorizer: {err}"
    );
    assert!(
        matches!(err, SqliteActorError::Exec(_)),
        "the refusal must come from statement execution, not a dead actor: {err}"
    );
    let text = err.to_string();
    assert!(
        text.contains(FTS_SHADOW_TABLE) && text.contains("may not be modified"),
        "the refusal must NAME the shadow table (a syntax or missing-table error \
         would pass a bare is_err check but proves nothing): {text}"
    );
    // The write did not land, and it did not journal.
    let after = be
        .actor()
        .query(&format!("SELECT count(*) FROM \"{FTS_SHADOW_TABLE}\""))
        .await
        .expect("read the shadow table back");
    assert_eq!(
        before, after,
        "the refused DELETE must leave the FTS5 index b-tree untouched"
    );
    let applied = be.applied_sqlite().await.expect("journal readable");
    let v = m.version.as_str();
    assert!(
        !applied.iter().any(|e| e.version == v),
        "a refused shadow-table write must not leave a journal row for {v}"
    );

    // POSITIVE CONTROL on a raw connection, which is what the engine would be
    // talking to without the DEFENSIVE line: the same DELETE succeeds and CORRUPTS
    // the index, so the setting is preventing corruption and not merely an error.
    let cdir = tempfile::tempdir().expect("control tempdir");
    let conn =
        rusqlite::Connection::open(cdir.path().join("control.sqlite")).expect("control open");
    assert_eq!(
        conn.db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE).ok(),
        Some(false),
        "the bundled library ships DEFENSIVE OFF, so the open sequence line is the whole guard"
    );
    conn.execute_batch(
        "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT); \
         INSERT INTO docs (body) VALUES ('hello world');",
    )
    .expect("control seed");
    conn.execute_batch(
        "CREATE VIRTUAL TABLE \"docs__fts\" \
         USING fts5(\"body\", content=\"docs\", content_rowid=\"rowid\");\n\
         INSERT INTO \"docs__fts\" (rowid, \"body\") SELECT rowid, body FROM docs;",
    )
    .expect("control FTS5 create + population");
    let matched: i64 = conn
        .query_row(
            "SELECT count(*) FROM \"docs__fts\" WHERE \"docs__fts\" MATCH 'hello'",
            [],
            |row| row.get(0),
        )
        .expect("control: the index answers a MATCH before the attack");
    assert_eq!(matched, 1, "control: the FTS5 index is populated");
    conn.execute_batch(&attack)
        .expect("control: with DEFENSIVE off the shadow-table DELETE SUCCEEDS");
    let corrupted = conn
        .query_row(
            "SELECT count(*) FROM \"docs__fts\" WHERE \"docs__fts\" MATCH 'hello'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect_err("control: the index is corrupt after the shadow-table write");
    assert!(
        corrupted.to_string().contains("corruption"),
        "control: the shadow-table write DEFENSIVE prevents leaves a corrupt index: {corrupted}"
    );
}

// ---------------------------------------------------------------------------
// SQLITE_DBCONFIG_TRUSTED_SCHEMA: a creator VIEW body may not invoke a virtual
// table.
//
// The setting gates USE, not creation: the `CREATE VIEW` naming a virtual table is
// ACCEPTED, and the refusal arrives when something later READS the view. That is
// the whole point - the view is stored in the tenant's schema, and its body then
// runs inside whatever statement touches it, under whatever authorizer mode that
// statement is running in. TRUSTED_SCHEMA=false is what stops a creator-authored
// schema object from being a deferred-execution device for virtual tables.
//
// The mode is set EXPLICITLY here rather than inherited from the last apply, so
// the test states which mode it is measuring instead of depending on leftover
// state. EngineJournal is the mode under test because it is the one where the
// pragma virtual table is reachable at all: the authorizer denies PRAGMA outright
// in CreatorUp, and admits a small read-only introspection allowlist (`table_info`
// among them) in engine mode.
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_creator_view_cannot_invoke_a_virtual_table() {
    let p = paths("trusted_schema");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    be.apply_one_additive(&mig("CREATE TABLE t (id INTEGER, body TEXT);"), "deployer")
        .await
        .expect("the tenant table applies");
    // Creation is ALLOWED. Asserting this is what makes the refusal below a
    // statement about USE rather than about the view being rejected outright.
    let created = be
        .apply_one_additive(
            &mig("CREATE VIEW v AS SELECT * FROM pragma_table_info('t');"),
            "creator",
        )
        .await
        .expect("TRUSTED_SCHEMA gates USE of a virtual table, so the CREATE VIEW is accepted");
    assert!(created, "the view migration must be newly-applied");

    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("select the mode under test");
    // NEIGHBOUR EXCLUSION: the SAME virtual table, in the SAME mode, invoked
    // DIRECTLY, SUCCEEDS. So the refusal below is not the vtable being unavailable,
    // not an authorizer deny, and not a missing table.
    let direct = be
        .actor()
        .query("SELECT count(*) FROM pragma_table_info('t')")
        .await
        .expect("the pragma virtual table is reachable by a direct statement in engine mode");
    assert_eq!(
        direct,
        vec![vec![Some("2".to_string())]],
        "the direct read must return the tenant table's two columns: {direct:?}"
    );

    let err = be
        .actor()
        .query("SELECT count(*) FROM v")
        .await
        .expect_err("a creator view body must not be allowed to invoke a virtual table");
    assert!(
        !err.is_authorizer_denied(),
        "the refusal must come from TRUSTED_SCHEMA, not the authorizer: {err}"
    );
    assert!(
        matches!(err, SqliteActorError::Exec(_)),
        "the refusal must come from statement execution, not a dead actor: {err}"
    );
    let text = err.to_string();
    assert!(
        text.contains("unsafe use of virtual table") && text.contains("pragma_table_info"),
        "the refusal must be the untrusted-schema one NAMING the virtual table: {text}"
    );

    // POSITIVE CONTROL: a raw connection ships TRUSTED_SCHEMA ON, so the same view
    // body executes and returns rows. That is the capability the open sequence
    // removes, and it is why a bare is_err assertion above would not be enough.
    let cdir = tempfile::tempdir().expect("control tempdir");
    let conn =
        rusqlite::Connection::open(cdir.path().join("control.sqlite")).expect("control open");
    assert_eq!(
        conn.db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA)
            .ok(),
        Some(true),
        "the bundled library ships TRUSTED_SCHEMA ON, so the open sequence line is the whole guard"
    );
    conn.execute_batch(
        "CREATE TABLE t (id INTEGER, body TEXT); \
         CREATE VIEW v AS SELECT * FROM pragma_table_info('t');",
    )
    .expect("control seed");
    let n: i64 = conn
        .query_row("SELECT count(*) FROM v", [], |row| row.get(0))
        .expect("control: with TRUSTED_SCHEMA on the view body executes the virtual table");
    assert_eq!(
        n, 2,
        "control: the view TRUSTED_SCHEMA would let run reads the schema through the vtable"
    );
}

// ---------------------------------------------------------------------------
// TRUSTED_SCHEMA, the confinement half: the creator view aimed at `_mig`.
//
// This is why the setting belongs in this file and not with the DQS pins. The
// two-argument pragma virtual table takes a SCHEMA, so a creator view can name the
// journal alias. Against that view the authorizer offers no protection in engine
// mode: the pragma-vtable route presents as an `AuthAction::Pragma`, which is on
// the engine allowlist, and never as an action whose database_name is `_mig` - so
// neither the journal-immutability arm nor the `_mig` backstop arm sees it. With
// TRUSTED_SCHEMA relaxed, reading this creator-authored view returns the journal
// catalog on the very connection that answers the creator's own direct attempt
// with an authorizer deny.
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_creator_view_cannot_read_the_mig_journal_through_a_pragma_vtable() {
    let p = paths("trusted_schema_mig");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    let probe = "SELECT count(*) FROM pragma_table_info('schema_migrations','_mig')";
    be.apply_one_additive(
        &mig("CREATE VIEW vm AS SELECT * FROM pragma_table_info('schema_migrations','_mig');"),
        "creator",
    )
    .await
    .expect("TRUSTED_SCHEMA gates USE, so the CREATE VIEW naming _mig is accepted");

    // In creator mode the DIRECT read is an authorizer deny. This is the baseline
    // the view must not be able to route around.
    be.actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("select creator mode");
    let denied = be
        .actor()
        .query(probe)
        .await
        .expect_err("a creator may not read the journal catalog directly");
    assert!(
        denied.is_authorizer_denied(),
        "the direct creator read of _mig must be an AUTHORIZER deny: {denied}"
    );

    // In engine mode the engine legitimately reads its own journal catalog, so the
    // route the view takes is open to the statement it would run inside.
    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("select engine mode");
    let engine_direct = be
        .actor()
        .query(probe)
        .await
        .expect("the engine reads its own journal catalog directly");
    let columns: i64 = engine_direct[0][0]
        .as_deref()
        .expect("a count row")
        .parse()
        .expect("a numeric count");
    assert!(
        columns > 0,
        "the journal catalog read must return the schema_migrations columns: {engine_direct:?}"
    );

    // The pin: the creator's stored view cannot take that route.
    let err = be
        .actor()
        .query("SELECT count(*) FROM vm")
        .await
        .expect_err("a creator view must not read the journal catalog through a virtual table");
    assert!(
        !err.is_authorizer_denied(),
        "the refusal must come from TRUSTED_SCHEMA, not the authorizer: {err}"
    );
    assert!(
        matches!(err, SqliteActorError::Exec(_)),
        "the refusal must come from statement execution, not a dead actor: {err}"
    );
    let text = err.to_string();
    assert!(
        text.contains("unsafe use of virtual table") && text.contains("pragma_table_info"),
        "the refusal must be the untrusted-schema one NAMING the virtual table: {text}"
    );

    // POSITIVE CONTROL: on a raw connection with a journal-shaped `_mig` attached,
    // the identical view reads the journal's catalog. That is the leak the setting
    // closes.
    let cdir = tempfile::tempdir().expect("control tempdir");
    let conn =
        rusqlite::Connection::open(cdir.path().join("control-main.sqlite")).expect("control open");
    conn.execute(
        "ATTACH DATABASE ?1 AS \"_mig\"",
        [cdir
            .path()
            .join("control-mig.sqlite")
            .to_str()
            .expect("control path")],
    )
    .expect("attach control _mig");
    conn.execute_batch(CONTROL_JOURNAL_SETUP)
        .expect("control journal setup");
    conn.execute_batch(
        "CREATE VIEW vm AS SELECT * FROM pragma_table_info('schema_migrations','_mig');",
    )
    .expect("control view");
    let leaked: i64 = conn
        .query_row("SELECT count(*) FROM vm", [], |row| row.get(0))
        .expect("control: with TRUSTED_SCHEMA on the view reads the journal catalog");
    assert_eq!(
        leaked, 9,
        "control: the view reports every column of the control journal table"
    );
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
async fn create_table_with_policy_injected_indexes_applies_under_creator_up() {
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
// The denial diagnostic: a refused statement says WHAT was refused.
//
// A denied migration used to surface as the bare `Exec("authorization denied")`,
// which named no action, no database and no mode. The authorizer now records the
// last DENY and the actor appends it. These two tests pin the two halves that
// matter: the message must NAME the refused action, and it must never claim a
// denial that did not happen.
// ---------------------------------------------------------------------------

/// The message a user gets for a KNOWN denial names the action, the database and
/// the mode. `PRAGMA writable_schema=ON` is the denial `confine_b` already proves;
/// this asserts the DIAGNOSTIC on it.
#[compio::test]
async fn a_denied_up_names_the_refused_action() {
    let p = paths("denial_named");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    let err = be
        .apply_one_additive(&mig("PRAGMA writable_schema=ON;"), "tester")
        .await
        .expect_err("PRAGMA is denied in CreatorUp");
    assert!(
        err.is_authorizer_denied(),
        "the fixture must be an authorizer deny for the diagnostic to apply: {err}"
    );
    let text = err.to_string();
    assert!(
        text.contains("[denied: PRAGMA writable_schema"),
        "the error must NAME the refused action, not just say 'authorization denied': {text}"
    );
    assert!(
        text.contains("mode=CreatorUp"),
        "the error must name the MODE in force: {text}"
    );
    // SQLite passes no database name for an unqualified connection-wide PRAGMA, so
    // the diagnostic reports the absence rather than inventing `main`.
    assert!(
        text.contains(" on \"main\"") || text.contains(" on <unqualified>"),
        "the error must say WHICH database the action targeted: {text}"
    );
}

/// A failure that is NOT an authorizer denial never gets a denial appended: not a
/// plain missing-table error, and not one whose own wording reads like a denial
/// while a real denial sits one statement behind it.
///
/// The second half is the leak catcher. `is_authorizer_denied` classifies by
/// message wording, so a trigger raising "not authorized ..." enters the append
/// path; it runs IMMEDIATELY after a genuine `PRAGMA writable_schema=ON` deny on
/// the same connection, so a slot that is not cleared per statement would attach
/// that stale denial to it.
#[compio::test]
async fn an_unrelated_failure_never_carries_a_denial() {
    let p = paths("denial_leak");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    be.actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("creator mode");

    // A missing table: not a denial by any reading, so it comes back verbatim.
    let missing = be
        .actor()
        .exec("SELECT 1 FROM no_such_table")
        .await
        .expect_err("a missing table must fail");
    assert!(
        !missing.is_authorizer_denied(),
        "a missing table is not an authorizer deny: {missing}"
    );
    let missing_text = missing.to_string();
    assert!(
        !missing_text.contains("[denied:"),
        "a non-authorizer failure must carry no denial: {missing_text}"
    );

    // A creator trigger that aborts with denial-shaped wording of its own.
    be.actor()
        .exec(
            "CREATE TABLE probe (id INTEGER PRIMARY KEY); \
             CREATE TRIGGER probe_guard BEFORE INSERT ON probe \
             BEGIN SELECT RAISE(ABORT, 'not authorized by the app'); END;",
        )
        .await
        .expect("the probe table and trigger are ordinary creator DDL on main");

    // A REAL denial, recorded on this connection.
    let denied = be
        .actor()
        .exec("PRAGMA writable_schema=ON")
        .await
        .expect_err("PRAGMA is denied in CreatorUp");
    assert!(
        denied.to_string().contains("writable_schema"),
        "the real denial must name its action: {denied}"
    );

    // The very next statement fails for an unrelated reason.
    let raised = be
        .actor()
        .exec("INSERT INTO probe DEFAULT VALUES")
        .await
        .expect_err("the trigger aborts the insert");
    let text = raised.to_string();
    assert!(
        text.contains("not authorized by the app"),
        "the probe must fail through its own RAISE, or it tests nothing: {text}"
    );
    assert!(
        raised.is_authorizer_denied(),
        "the probe must be CLASSIFIED as a denial, or the append path is never \
         reached and the leak would go unnoticed: {text}"
    );
    assert!(
        !text.contains("[denied:"),
        "a stale denial leaked onto an unrelated failure: {text}"
    );
    assert!(
        !text.contains("writable_schema"),
        "the previous statement's denial leaked into this error: {text}"
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

// ---------------------------------------------------------------------------
// PRAGMA data_version: the one pragma a creator can reach, and the exact edge it
// stops at.
//
// FTS5's `xUpdate` issues `PRAGMA data_version` internally on the first access to
// an index object on a connection, so the authorizer had to concede it or no write
// could ever reach an FTS5 index (proven end to end in `sqlite_goodies.rs`). The
// arm allows it ONLY when the action's database is the app file, in BOTH modes,
// and is matched ahead of the general pragma arm.
//
// The scoping is the load-bearing half. `data_version` counters are PER SCHEMA, so
// a name-only allow (the shape `is_engine_allowed_pragma` would have given) hands a
// creator `PRAGMA "_mig".data_version` - a monotone counter of the ENGINE's commits
// to the immutable journal - and this arm sits AHEAD of the CreatorUp `_mig`
// backstop, so that arm would be the thing conceding it rather than a gap in the
// backstop.
//
// The authorizer cannot tell FTS5's internal call from a creator typing the pragma:
// both present identically, and a creator can forge the qualified spelling. So the
// grant is real, and the second test states it as a capability rather than leaving
// a comment claiming it is narrower than it is.
// ---------------------------------------------------------------------------

/// A creator `up` reading the JOURNAL's change counter is denied. This is the
/// assertion that proves the scoping is real rather than incidental: the same
/// pragma, one database over, applies cleanly on this very connection.
#[compio::test]
async fn confine_creator_pragma_data_version_on_the_journal_denied() {
    let p = paths("data_version_mig");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");

    let attack = "PRAGMA \"_mig\".data_version;";
    assert_denied_and_journal_clean(&be, attack, DenyKind::Authorizer).await;
    // The diagnostic must name the JOURNAL, since the database is the whole
    // difference between this deny and the allow below.
    let err = be
        .apply_one_additive(&mig(attack), "attacker")
        .await
        .expect_err("the journal's counter must stay out of reach");
    let text = err.to_string();
    assert!(
        text.contains("[denied: PRAGMA data_version on \"_mig\"")
            && text.contains("mode=CreatorUp"),
        "the refusal must name the pragma AND the journal schema: {text}"
    );

    // NEIGHBOUR EXCLUSION: the same pragma on the APP file applies on this exact
    // connection, so the deny above is the DATABASE and not the pragma name.
    be.apply_one_additive(&mig("PRAGMA main.data_version;"), "creator")
        .await
        .expect("the app file's counter is conceded (FTS5 needs it) and must apply");

    // Positive control: on a raw connection with a `_mig` attached, the qualified
    // pragma succeeds - so the hardened refusal is confinement, not a parse error or
    // an unresolvable schema name.
    assert_attack_succeeds_unhardened(CONTROL_JOURNAL_SETUP, attack);
}

/// WHAT THE GRANT CONCEDES, as a test rather than a comment: a creator can read the
/// app file's `data_version`, and what it reads is a live counter of OTHER
/// connections' commits to that file - not a constant. That is a genuine
/// observation channel onto the tenant's own database, which the creator already
/// writes at will. The journal's counter, refused on the same connection in the
/// same mode, is the boundary that matters.
#[compio::test]
async fn a_creator_can_observe_the_app_files_data_version_counter() {
    let p = paths("data_version_main");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    be.apply_one_additive(&mig("CREATE TABLE t (id INTEGER PRIMARY KEY);"), "deployer")
        .await
        .expect("seed a table another connection can commit to");

    async fn counter(be: &SqliteBackend) -> i64 {
        be.actor()
            .query("PRAGMA main.data_version")
            .await
            .expect("a creator may read the app file's change counter")[0][0]
            .as_deref()
            .expect("a value row")
            .parse()
            .expect("a numeric counter")
    }

    be.actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("creator mode");
    let before = counter(&be).await;

    // Another connection commits to the app file. `data_version` reports EXTERNAL
    // commits, so this is what makes the value move.
    {
        let conn = rusqlite::Connection::open(&p.app).expect("second connection on the app file");
        conn.execute_batch("INSERT INTO t (id) VALUES (1);")
            .expect("the second connection commits");
    }
    let after = counter(&be).await;
    assert!(
        after > before,
        "the conceded read is a LIVE counter of other connections' commits to the \
         app file, not a constant: {before} -> {after}"
    );

    // The boundary: the journal's counter is refused on this same connection, in
    // this same mode. Both modes, because the grant spans both.
    for mode in [Mode::CreatorUp, Mode::EngineJournal] {
        be.actor().set_mode(mode).await.expect("select the mode");
        let err = be
            .actor()
            .query("PRAGMA \"_mig\".data_version")
            .await
            .expect_err("the journal's counter is never conceded");
        assert!(
            err.is_authorizer_denied(),
            "the journal counter must be an AUTHORIZER deny in {mode:?}: {err}"
        );
    }
}

// ---------------------------------------------------------------------------
// WHAT THIS SUITE NO LONGER CATCHES, measured rather than left to be discovered.
//
// `confine_creator_write_to_an_fts5_shadow_table_denied_by_defensive` above proves
// DEFENSIVE refuses a creator write to the FTS5 SHADOW TABLES. It does not, and
// never did, cover the VTABLE: FTS5 reaches its own shadow tables through the
// virtual table, so the `'delete-all'` command wipes the index and DEFENSIVE does
// not see it. With `data_version` conceded, that command now reaches FTS5 at all,
// which it previously did not.
//
// This is NOT a new capability class, and the test proves that rather than
// asserting it: a creator `up` can already DROP the index outright, and the sync
// triggers and the base table with it. A creator that wants the index gone has
// always been able to remove it. The wipe is a cheaper spelling of a capability the
// confinement model already grants, on objects the tenant owns.
// ---------------------------------------------------------------------------

/// The FTS5 `'delete-all'` command a creator can now issue, and the DROP that was
/// always available. Both succeed; the point of the test is that this is stated in
/// the suite instead of being found later.
#[compio::test]
async fn a_creator_can_wipe_an_fts_index_through_the_vtable() {
    let p = paths("fts_delete_all");
    seed_legacy_fts_index(&p.app, true);
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");

    // The index holds the row. Read on a REOPENED RAW connection: the engine never
    // issues an FTS5 MATCH, so `match` is deliberately absent from the authorizer's
    // function allowlist.
    let matches = |term: &str| -> i64 {
        let conn = rusqlite::Connection::open(&p.app).expect("reopen the app file raw");
        conn.query_row(
            "SELECT count(*) FROM \"docs__fts\" WHERE \"docs__fts\" MATCH ?1",
            [term],
            |row| row.get(0),
        )
        .expect("the index answers a MATCH")
    };
    assert_eq!(matches("hello"), 1, "the index must start populated");

    // The wipe, as an ordinary creator `up`. It SUCCEEDS.
    let applied = be
        .apply_one_additive(
            &mig("INSERT INTO \"docs__fts\"(\"docs__fts\") VALUES('delete-all');"),
            "creator",
        )
        .await
        .expect("the FTS5 delete-all command is not refused by DEFENSIVE or the authorizer");
    assert!(applied, "the wiping migration must be newly-applied");
    assert_eq!(
        matches("hello"),
        0,
        "the delete-all command empties the index - this is the capability the \
         confinement suite does not catch"
    );

    // The capability class was already there: the creator can drop the whole index.
    be.apply_one_additive(&mig("DROP TABLE \"docs__fts\";"), "creator")
        .await
        .expect("a creator up can drop the FTS index outright - a DROP TABLE on main");
    let gone = be
        .actor()
        .query("SELECT name FROM main.sqlite_master WHERE name = 'docs__fts'")
        .await
        .expect("read main schema");
    assert!(
        gone.is_empty(),
        "the index is gone, so the wipe above conceded nothing the creator lacked: {gone:?}"
    );
}
