//! Confinement proofs for the hardened `SQLite` migration backend.
//! EVERY claim is proven against a REAL temp-file `SQLite` — never a shim.
//!
//! Each `confine_*` test drives a creator `up` that attempts one escape and
//! asserts it is DENIED (by the authorizer or DEFENSIVE), AND that the failure did
//! not corrupt the journal (the version is NOT recorded `completed`).
//!
//! The journal shares the app's own file, so what separates it from the creator's
//! tables is the `__zeroship_schema_` name fence rather than a database boundary.
//! Every arm below therefore attacks a FENCED NAME in `main`.
//!
//! Attacks proven denied:
//!   (a) ATTACH an arbitrary file
//!   (b) PRAGMA `writable_schema=ON` then a `sqlite_master` write
//!   (c) SELECT `load_extension`(...)
//!   (d) DROP TABLE `__zeroship_schema_migrations`
//!   (e) DROP TRIGGER on the journal's immutability trigger
//!   (f) INSERT INTO `__zeroship_schema_migrations` ... directly
//!   (g) CREATE TRIGGER on `app_tbl` whose body writes the journal
//!   (h) cross-tenant: a backend for app A cannot reach app B's file
//! Plus: direct UPDATE/DELETE on the journal rejected by the trigger; DETACH
//! denied; version floor satisfied.
//!
//! Two of the hardened open sequence's dbconfig settings are pinned here rather
//! than in `sqlite_dqs_hardening.rs`, because what they buy is confinement (what a
//! hostile creator `up` may reach) rather than identifier resolution:
//!   `SQLITE_DBCONFIG_DEFENSIVE` - a creator write to a virtual-table SHADOW TABLE
//!   `SQLITE_DBCONFIG_TRUSTED_SCHEMA` - a creator VIEW body invoking a virtual
//!   table, including one that reads the journal's catalog
//! Both carry a positive control on a raw connection, which ships DEFENSIVE off
//! and TRUSTED_SCHEMA on, so each of those two lines is the whole guard.

use std::path::PathBuf;

use rusqlite::config::DbConfig;
use tempfile::TempDir;
use zeroship_migrate::model::migration::{
    Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId,
};
use zeroship_migrate_sqlite::backend::actor::SqliteActorError;
use zeroship_migrate_sqlite::backend::authorizer::Mode;
use zeroship_migrate_sqlite::SqliteBackend;

/// A tenant's file inside a fresh temp dir. One file: the journal is in it.
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
/// journal vector (g), whose qualified form is rejected by `SQLite`'s PARSER, not the
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
/// connection — no authorizer, no DEFENSIVE, no name fence. This proves the
/// deny in the hardened case was caused by CONFINEMENT, not by an unrelated error
/// (a malformed statement, a missing table, etc.). The control runs against
/// throwaway temp files so it never touches the tenant under test.
///
/// `setup` seeds whatever the attack references (a journal-shaped set of fenced
/// tables, or an app table); `attack_sql` is then executed and MUST succeed.
fn assert_attack_succeeds_unhardened(setup: &str, attack_sql: &str) {
    let dir = tempfile::tempdir().expect("control tempdir");
    let main = dir.path().join("control-main.sqlite");
    let conn = rusqlite::Connection::open(&main).expect("open control main");
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

/// The journal-shaped DDL a positive control needs so
/// `__zeroship_schema_migrations` (and its immutability trigger) resolve on the
/// raw control connection - spelled EXACTLY as `ensure_journal` spells them, since
/// what the hardened arms deny is these names.
const CONTROL_JOURNAL_SETUP: &str = "\
    CREATE TABLE main.\"__zeroship_schema_migrations\" (\
        event_seq INTEGER PRIMARY KEY AUTOINCREMENT, event_kind TEXT, version TEXT, \
        name TEXT, checksum TEXT, \"by\" TEXT, phase TEXT, outcome TEXT, kind TEXT); \
    CREATE TRIGGER main.\"__zeroship_schema_migrations_immutable_delete\" \
        BEFORE DELETE ON \"__zeroship_schema_migrations\" \
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
        "PRAGMA writable_schema=ON; DELETE FROM main.sqlite_master WHERE name LIKE 'zs_%';";
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
// (d) DROP TABLE `__zeroship_schema_migrations` — denied at prepare, on the
//     action's own `table_name` (DropTable carries no database field, and the
//     database would say `main` for a creator table too).
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_d_drop_mig_table_denied() {
    let p = paths("d");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    let attack = "DROP TABLE main.\"__zeroship_schema_migrations\";";
    assert_denied_and_journal_clean(&be, attack, DenyKind::Authorizer).await;
    // Positive control: dropping the journal table succeeds on a raw connection.
    assert_attack_succeeds_unhardened(CONTROL_JOURNAL_SETUP, attack);
    // And the journal table still exists + is readable.
    be.applied_sqlite()
        .await
        .expect("schema_migrations survived the denied DROP TABLE");
}

// ---------------------------------------------------------------------------
// (e) DROP TRIGGER on the journal's immutability trigger — denied (DropTrigger
//     carries both the trigger name and its table; either one being fenced denies).
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_e_drop_mig_trigger_denied() {
    let p = paths("e");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    let attack = "DROP TRIGGER main.\"__zeroship_schema_migrations_immutable_delete\";";
    assert_denied_and_journal_clean(&be, attack, DenyKind::Authorizer).await;
    // Positive control: dropping the immutability trigger succeeds on a raw conn.
    assert_attack_succeeds_unhardened(CONTROL_JOURNAL_SETUP, attack);
}

// ---------------------------------------------------------------------------
// (f) Direct INSERT INTO `__zeroship_schema_migrations` — journal-forge denied.
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_f_direct_journal_insert_denied() {
    let p = paths("f");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    let attack = "INSERT INTO main.\"__zeroship_schema_migrations\" \
         (event_kind, version, name, checksum, \"by\", phase, outcome, kind) \
         VALUES ('applied', 'forged', 'x', 'x', 'attacker', 'completed', 'success', 'apply');";
    assert_denied_and_journal_clean(&be, attack, DenyKind::Authorizer).await;
    // Positive control: the forged INSERT succeeds on a raw connection (no
    // authorizer; INSERT is not blocked by the append-only trigger).
    assert_attack_succeeds_unhardened(CONTROL_JOURNAL_SETUP, attack);
}

// ---------------------------------------------------------------------------
// (g) CREATE TRIGGER on an app table whose body writes the journal — denied at
//     the trigger's CREATE-prepare time (accessor set + a fenced table name).
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

    // This vector is REAL now, and it was not before. While the journal sat in a
    // second attached database, a trigger body could not name it at all: SQLite's
    // parser rejects a qualified table name inside a trigger, and an UNQUALIFIED
    // name resolves to the trigger's own database - which was `main`, not the
    // journal. The accessor rule was belt-and-suspenders over an impossibility.
    //
    // With the journal in `main`, an unqualified `__zeroship_schema_migrations` in
    // a trigger body RESOLVES, and the only thing between a creator trigger and a
    // forged journal row is the authorizer denying the fenced name at the trigger's
    // CREATE-prepare time. Both forms are asserted below, and g2's deny is now an
    // authorizer DENY rather than a resolution failure.

    // (g1) Qualified body — still parser-rejected before the authorizer sees it,
    // so the classification stays `AuthorizerOrDefensive` rather than claiming a
    // DENY that did not happen.
    let qualified = "CREATE TRIGGER t1 AFTER INSERT ON app_tbl BEGIN \
            INSERT INTO main.\"__zeroship_schema_migrations\" \
            (event_kind, version, name, checksum, \"by\", phase, outcome, kind) \
            VALUES ('applied', 'forged', 'x', 'x', 'attacker', 'completed', 'success', 'apply'); \
         END;";
    assert_denied_and_journal_clean(&be, qualified, DenyKind::AuthorizerOrDefensive).await;

    // (g2) UNQUALIFIED body naming the journal table, which now RESOLVES. SQLite
    // authorizes a trigger's body when the TRIGGERING statement is prepared, not
    // when the trigger is created, so the CREATE lands and the deny arrives at
    // fire time - with `accessor` naming this trigger and the body's own fenced
    // table on the action. That is the arm the accessor rule exists for, and until
    // the journal moved into this file nothing could reach it.
    be.apply_one_additive(
        &mig(
            "CREATE TRIGGER t2 AFTER INSERT ON app_tbl BEGIN \
                INSERT INTO \"__zeroship_schema_migrations\" \
                (event_kind, version, name, checksum, \"by\", phase, outcome, kind) \
                VALUES ('applied', 'forged2', 'x', 'x', 'attacker', 'completed', 'success', 'apply'); \
             END;",
        ),
        "tester",
    )
    .await
    .expect("SQLite authorizes a trigger body at fire time, so the CREATE itself lands");
    let fired = be
        .apply_one_additive(&mig("INSERT INTO app_tbl (id) VALUES (7);"), "attacker")
        .await
        .expect_err("firing the trigger must be denied");
    assert!(
        fired.is_authorizer_denied(),
        "the body's write to a fenced table must be an AUTHORIZER deny at fire time, got: {fired}"
    );

    // The control that makes g2 attributable to the FENCE and not to trigger
    // bodies being refused wholesale. It runs on a SECOND backend, because t2 is
    // still installed on this one and would deny every later insert into
    // `app_tbl` - a control sharing that table would be measuring t2, not itself.
    let q = paths("g_control");
    let control = backend(&q);
    control
        .ensure_journal_sqlite()
        .await
        .expect("bootstrap journal");
    control
        .apply_one_additive(
            &mig("CREATE TABLE app_tbl (id INTEGER); \
                 CREATE TABLE app_log (event_kind TEXT); \
                 CREATE TRIGGER t3 AFTER INSERT ON app_tbl BEGIN \
                    INSERT INTO app_log (event_kind) VALUES ('applied'); \
                 END;"),
            "tester",
        )
        .await
        .expect("a creator trigger over a creator table is not the fence's business");
    control
        .apply_one_additive(&mig("INSERT INTO app_tbl (id) VALUES (1);"), "tester")
        .await
        .expect("and firing it is allowed");
    control
        .actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("creator mode");
    let logged = control
        .actor()
        .query("SELECT count(*) FROM app_log")
        .await
        .expect("read the creator's own log");
    assert_eq!(
        logged[0][0].as_deref(),
        Some("1"),
        "the control trigger must actually have written a row"
    );

    let net = be.applied_sqlite().await.expect("journal readable");
    assert!(
        !net.iter()
            .any(|e| e.version == "forged" || e.version == "forged2"),
        "no creator trigger can forge a journal row"
    );
}

// ---------------------------------------------------------------------------
// (h) Cross-tenant: a backend opened for app A cannot reach app B's file. `main`
//     is the only bound database; ATTACH of B is denied, and even naming a
//     foreign alias cannot compile.
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_h_cross_tenant_denied() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a_app = dir.path().join("zs-A.sqlite");
    let b_app = dir.path().join("zs-B.sqlite");

    // Pre-create B with a secret table by opening a plain (un-hardened) connection.
    {
        let conn = rusqlite::Connection::open(&b_app).expect("open B");
        conn.execute_batch("CREATE TABLE secret (id INTEGER); INSERT INTO secret VALUES (42);")
            .expect("seed B");
    }

    let be = SqliteBackend::open(&a_app).expect("open A backend");
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
// (i) a creator `up` READING the journal — `SELECT … FROM
//     __zeroship_schema_migrations` — is denied. A plain top-level read is an
//     `AuthAction::Read { accessor: None }`, and the trigger-body arm
//     requires `accessor.is_some()`, so the backstop arm is what must deny any
//     fenced action in CreatorUp, Read included; otherwise a creator
//     could exfiltrate the immutable journal into an app table.
//     End-to-end on the REAL hardened backend.
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_i_creator_read_of_mig_journal_denied() {
    let p = paths("read_journal");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    // A creator `up` that copies the journal into an app table — the SELECT issues a
    // Read on `"_mig".schema_migrations`, which must be denied at prepare.
    let attack = "CREATE TABLE stolen AS SELECT * FROM main.\"__zeroship_schema_migrations\";";
    assert_denied_and_journal_clean(&be, attack, DenyKind::Authorizer).await;
    // Positive control: the SAME read-into-table SUCCEEDS on a raw connection (no
    // authorizer), proving the hardened deny is the M1 confinement rule and not a
    // parse / missing-table / CTAS error.
    assert_attack_succeeds_unhardened(CONTROL_JOURNAL_SETUP, attack);
    // And the journal is still readable by the engine itself (EngineJournal reads
    // are unaffected by the creator-mode fence deny).
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
    let attack = "DETACH DATABASE \"other\";";
    assert_denied_and_journal_clean(&be, attack, DenyKind::Authorizer).await;
    // Positive control: DETACH of an attached db succeeds on a raw connection. The
    // setup binds the alias first, so the control proves the DETACH capability
    // rather than re-proving that an unbound name has nothing to detach.
    assert_attack_succeeds_unhardened("ATTACH DATABASE ':memory:' AS \"other\";", attack);
}

// ---------------------------------------------------------------------------
// The authorizer ALLOWS a write to `sqlite_master` / `sqlite_temp_master` as an
// action, so SQLite's INTERNAL ALTER machinery can run `ALTER TABLE … DROP
// COLUMN`. That must NOT open a DIRECT-write hole: a creator `up` issuing
// `UPDATE main.sqlite_master ...` directly is still rejected, and the only path
// that reaches the allowed action is SQLite's own ALTER executor.
//
// WHAT REJECTS IT. `sqlite_master` is writable only while `writable_schema` is
// ON, and it is OFF by default on every connection; the authorizer independently
// denies the `PRAGMA writable_schema=ON` that would turn it on (proven by
// `confine_b_writable_schema_denied`). So the rejection here is the default plus
// that PRAGMA deny, NOT `SQLITE_DBCONFIG_DEFENSIVE`: flipping DEFENSIVE to false
// leaves this test green and the error text byte-identical
// (`table sqlite_master may not be modified`). DEFENSIVE is pinned separately by
// `confine_creator_write_to_a_vtable_shadow_table_denied_by_defensive`, on the
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
// SQLITE_DBCONFIG_DEFENSIVE: a creator `up` may not write a virtual-table SHADOW
// TABLE. A raw connection creates an rtree solely as a bundled, deterministic
// source of shadow tables. The authorizer permits creator DML on ordinary `main`
// tables, so DEFENSIVE is the only guard that refuses the direct shadow write.
// ---------------------------------------------------------------------------

const VTABLE_SHADOW_TABLE: &str = "docs_spatial_node";

fn seed_vtable_shadow(app: &std::path::Path) {
    let conn = rusqlite::Connection::open(app).expect("open the app file raw to seed");
    conn.execute_batch(
        "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT);\n\
         CREATE VIRTUAL TABLE docs_spatial \
         USING rtree(id, min_x, max_x, min_y, max_y);\n\
         INSERT INTO docs_spatial VALUES (1, 0, 1, 0, 1);",
    )
    .expect("seed a virtual table and its shadow storage");
}

#[compio::test]
async fn confine_creator_write_to_a_vtable_shadow_table_denied_by_defensive() {
    let p = paths("vtable_shadow");
    seed_vtable_shadow(&p.app);
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");

    // The shadow tables now exist in `main`, reachable by name from creator SQL.
    let shadow = be
        .actor()
        .query(&format!(
            "SELECT name FROM main.sqlite_master WHERE name = '{VTABLE_SHADOW_TABLE}'"
        ))
        .await
        .expect("read main schema");
    assert_eq!(
        shadow.len(),
        1,
        "the shadow table must exist for this attack to be the one under test: {shadow:?}"
    );
    let before = be
        .actor()
        .query(&format!("SELECT count(*) FROM \"{VTABLE_SHADOW_TABLE}\""))
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

    // The attack: a creator `up` rewriting a module's shadow storage directly.
    let attack = format!("DELETE FROM \"{VTABLE_SHADOW_TABLE}\";");
    let m = mig(&attack);
    let err = be
        .apply_one_additive(&m, "attacker")
        .await
        .expect_err("a creator write to a virtual-table shadow must be refused");
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
        text.contains(VTABLE_SHADOW_TABLE) && text.contains("may not be modified"),
        "the refusal must NAME the shadow table (a syntax or missing-table error \
         would pass a bare is_err check but proves nothing): {text}"
    );
    // The write did not land, and it did not journal.
    let after = be
        .actor()
        .query(&format!("SELECT count(*) FROM \"{VTABLE_SHADOW_TABLE}\""))
        .await
        .expect("read the shadow table back");
    assert_eq!(
        before, after,
        "the refused DELETE must leave the module shadow storage untouched"
    );
    let applied = be.applied_sqlite().await.expect("journal readable");
    let v = m.version.as_str();
    assert!(
        !applied.iter().any(|e| e.version == v),
        "a refused shadow-table write must not leave a journal row for {v}"
    );

    // POSITIVE CONTROL on a raw connection, which is what the engine would be
    // talking to without the DEFENSIVE line: the same DELETE succeeds, so the
    // setting is preventing the write and not merely surfacing an unrelated error.
    let cdir = tempfile::tempdir().expect("control tempdir");
    let conn =
        rusqlite::Connection::open(cdir.path().join("control.sqlite")).expect("control open");
    assert_eq!(
        conn.db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE).ok(),
        Some(false),
        "the bundled library ships DEFENSIVE OFF, so the open sequence line is the whole guard"
    );
    conn.execute_batch(
        "CREATE VIRTUAL TABLE docs_spatial \
         USING rtree(id, min_x, max_x, min_y, max_y);\n\
         INSERT INTO docs_spatial VALUES (1, 0, 1, 0, 1);",
    )
    .expect("control virtual-table seed");
    let before: i64 = conn
        .query_row(
            &format!("SELECT count(*) FROM \"{VTABLE_SHADOW_TABLE}\""),
            [],
            |row| row.get(0),
        )
        .expect("control shadow table is populated");
    assert!(before > 0, "the control shadow table must be non-empty");
    conn.execute_batch(&attack)
        .expect("control: with DEFENSIVE off the shadow-table DELETE SUCCEEDS");
    let after: i64 = conn
        .query_row(
            &format!("SELECT count(*) FROM \"{VTABLE_SHADOW_TABLE}\""),
            [],
            |row| row.get(0),
        )
        .expect("read the raw shadow table after deletion");
    assert_eq!(after, 0, "the raw shadow-table DELETE must change storage");
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
// TRUSTED_SCHEMA, the confinement half: the creator view aimed at the journal.
//
// This is why the setting belongs in this file and not with the DQS pins. The
// pragma virtual table takes the table name as a STRING ARGUMENT, so a creator
// view can name a journal table without the authorizer ever seeing a fenced
// `table_name`: the pragma-vtable route presents as an `AuthAction::Pragma`,
// which is on the engine allowlist, and never as an action naming the journal -
// so neither the journal-immutability arm nor the fenced backstop arm sees it.
// With TRUSTED_SCHEMA relaxed, reading this creator-authored view returns the
// journal catalog on the very connection that answers the creator's own direct
// attempt with an authorizer deny.
// ---------------------------------------------------------------------------
#[compio::test]
async fn confine_creator_view_cannot_read_the_mig_journal_through_a_pragma_vtable() {
    let p = paths("trusted_schema_journal");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    let probe = "SELECT count(*) FROM pragma_table_info('__zeroship_schema_migrations')";
    be.apply_one_additive(
        &mig("CREATE VIEW vm AS SELECT * FROM pragma_table_info('__zeroship_schema_migrations');"),
        "creator",
    )
    .await
    .expect("TRUSTED_SCHEMA gates USE, so the CREATE VIEW naming the journal is accepted");

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
        "the direct creator read of the journal must be an AUTHORIZER deny: {denied}"
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

    // POSITIVE CONTROL: on a raw connection carrying journal-shaped tables, the
    // identical view reads the journal's catalog. That is the leak the setting
    // closes.
    let cdir = tempfile::tempdir().expect("control tempdir");
    let conn =
        rusqlite::Connection::open(cdir.path().join("control-main.sqlite")).expect("control open");
    conn.execute_batch(CONTROL_JOURNAL_SETUP)
        .expect("control journal setup");
    conn.execute_batch(
        "CREATE VIEW vm AS SELECT * FROM pragma_table_info('__zeroship_schema_migrations');",
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
//   (1) A creator `up` may not REINDEX a FENCED index, and may REINDEX its own.
//       `SQLITE_REINDEX` fires once per index with that index's NAME, so the
//       fence is what separates the two; the deny comes from the dedicated
//       `Reindex if targets_journal` arm, which sits AHEAD of the main/temp allow
//       (the journal-immutability arm omits `Reindex` entirely).
//
//       A no-arg `REINDEX;` is ALLOWED. It used to be refused, and the refusal
//       was an accident of placement rather than a decision: the journal was a
//       second attached database, so the no-arg form named an alias that was
//       neither `main` nor `temp` and fell through to the foreign-alias deny.
//       With one database it names only `main`'s own indexes, and REINDEX
//       rebuilds an index B-tree from the table it already indexes - it reads no
//       row out, writes no row in, and fires no trigger.
//
//   (2) A real `CREATE TABLE` whose emission carries system-field indexes
//       APPLIES cleanly under CreatorUp. CREATE INDEX fires SQLITE_REINDEX
//       INTRINSICALLY (SQLite reindexes the fresh index to populate it); before
//       the REINDEX-on-main relaxation that intrinsic reindex was denied,
//       so a system-field-index-bearing CREATE TABLE failed to apply. This is the
//       regression for the relaxation's MOTIVATION.
// ---------------------------------------------------------------------------

/// (1) A creator `up` may REINDEX its own index and a no-arg `REINDEX;`, and may
/// not REINDEX a fenced one, which the engine itself still can. The pair is what makes the deny attributable to the
/// fence: an arm that only asserted the refusal would pass over an authorizer that
/// refused REINDEX outright, which would break every `CREATE INDEX` (it fires
/// `SQLITE_REINDEX` intrinsically).
#[compio::test]
async fn reindex_is_denied_on_the_journal_and_allowed_on_the_creators_own() {
    let p = paths("reindex_noarg");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    be.apply_one_additive(
        &mig(
            "CREATE TABLE app_tbl (id INTEGER PRIMARY KEY, handle TEXT); \
             CREATE INDEX app_tbl_handle_idx ON app_tbl (handle);",
        ),
        "tester",
    )
    .await
    .expect("benign app table applies");

    be.apply_one_additive(&mig("REINDEX app_tbl_handle_idx;"), "tester")
        .await
        .expect("a creator's own index is the creator's to reindex");
    be.apply_one_additive(&mig("REINDEX;"), "tester")
        .await
        .expect("and so is every index in the one database this connection has");

    // A fenced INDEX, created by the engine the way the journal's own DDL is.
    // Without this the REINDEX below would fail with "unable to identify the
    // object to be reindexed" - SQLite resolves the name before it authorizes, so
    // an arm aimed at a nonexistent index would never reach the authorizer and
    // would pass for the wrong reason.
    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("engine mode");
    be.actor()
        .exec(
            "CREATE INDEX main.\"__zeroship_schema_migrations_ix\" \
             ON \"__zeroship_schema_migrations\" (version)",
        )
        .await
        .expect("the engine may create its own fenced index");

    // The fenced name, which the dedicated arm refuses ahead of the allow above.
    let m = mig("REINDEX \"__zeroship_schema_migrations_ix\";");
    let err = be
        .apply_one_additive(&m, "attacker")
        .await
        .expect_err("a REINDEX naming a fenced index must be rejected");
    assert!(
        err.is_authorizer_denied(),
        "a fenced REINDEX must be an AUTHORIZER deny, got: {err}"
    );
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
/// the mode. `PRAGMA writable_schema=ON` is the denial
/// `confine_b_writable_schema_denied` already proves;
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
