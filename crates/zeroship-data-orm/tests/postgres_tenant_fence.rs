//! The `PostgreSQL` behaviours the app-database decoupling's tenant fence rests
//! on, measured against the major the platform deploys.
//!
//! `docs/proposals/2026-08-28-app-database-decoupling.md` carries a set of
//! claims about what the server does - `WITH SET FALSE`, `WITH INHERIT FALSE`
//! versus the `NOINHERIT` role attribute, the `22023`/`42501` split, column
//! grants adding rather than subtracting, logical decoding ignoring the ACL,
//! and the publication column list versus `REPLICA IDENTITY FULL`. A claim in
//! a document is something nothing re-runs, so each one is an arm here
//! instead, and Open 6's driver question rides the same target because the
//! taxonomy it feeds is composed by
//! `zeroship_data_orm::backend::postgres::pg_session_sql` and read by
//! `pg_error::classify_pg_per_app_session_setup`.
//!
//! **Every arm carries its control.** A denial arm that ran against a fixture
//! which granted nothing passes for the wrong reason, so each denial is paired
//! with the grant shape that must still succeed. Two arms have controls that
//! are not merely a second assertion but the whole point: the inherit arm is
//! three grant shapes of which two must return the row, and the SQLSTATE arm
//! runs the same two statements as a superuser, where the split disappears.
//!
//! **The server is a throwaway container, and that is not a convenience.**
//! `pg_authid` and `pg_auth_members` are cluster-shared, so the role DDL below
//! would be visible to every database on a shared instance. Each arm owns its
//! own server through the repository's `PostgreSQL` fixture.

#[path = "../../../tests/fixtures/postgres/mod.rs"]
mod postgres_fixture;

use compio_postgres::error::{DbError, SqlState};
use compio_postgres::Pool;
use std::rc::Rc;
use zeroship_data_orm::budgets::{DB_LOCK_TIMEOUT_MS, DB_STATEMENT_TIMEOUT_MS};

/// The deploy pin is `postgres:16` (`deploy/compose/docker-compose.yml`), and
/// `inherit_option` - the whole inherit arm - does not exist below it. An arm
/// that ran on an older server would report that this design has no fence.
const MINIMUM_SERVER_VERSION_NUM: i32 = 160_000;

/// The password every fixture login carries. The container is thrown away with
/// the test.
const FIXTURE_PASSWORD: &str = "fixture";

/// Connect as the fixture's superuser and refuse a server older than the pin.
async fn superuser(postgres: &postgres_fixture::Postgres) -> Rc<Pool> {
    let pool = Rc::new(
        Pool::connect(&postgres.url(), 4)
            .await
            .expect("the fixture's PostgreSQL accepts its superuser"),
    );
    let rows = pool
        .query(
            "SELECT current_setting('server_version_num')::int AS version_num",
            &[],
        )
        .await
        .expect("the server reports its version");
    assert_eq!(rows.len(), 1, "server_version_num returns exactly one row");
    let version_num: i32 = rows[0].get("version_num");
    assert!(
        version_num >= MINIMUM_SERVER_VERSION_NUM,
        "these arms describe PostgreSQL {MINIMUM_SERVER_VERSION_NUM} and above; \
         the fixture server reports server_version_num {version_num}"
    );
    pool
}

/// Connect as one of the fixture's login roles.
async fn login(postgres: &postgres_fixture::Postgres, role: &str) -> Rc<Pool> {
    let mut url = url::Url::parse(&postgres.url()).expect("the fixture URL parses");
    url.set_username(role)
        .expect("the fixture URL accepts a username");
    url.set_password(Some(FIXTURE_PASSWORD))
        .expect("the fixture URL accepts a password");
    Rc::new(
        Pool::connect(url.as_str(), 2)
            .await
            .unwrap_or_else(|error| panic!("login role {role} connects: {error}")),
    )
}

/// The server's `ErrorResponse` behind a driver error, or a panic naming what
/// arrived instead. A transport or protocol failure carries no SQLSTATE, and
/// silently treating one as "some error" is how a denial arm stops measuring
/// the denial.
fn server_error(error: &compio_postgres::Error) -> &DbError {
    error
        .as_db_error()
        .unwrap_or_else(|| panic!("expected a server error response, got: {error}"))
}

/// Run one statement under `role` inside an explicit transaction, the way the
/// data plane narrows (`SET LOCAL ROLE` first, reverted at rollback).
async fn under_role(
    pool: &Pool,
    role: &str,
    sql: &str,
) -> Result<Vec<compio_postgres::Row>, compio_postgres::Error> {
    let mut connection = pool.acquire().await?;
    let transaction = connection.transaction().await?;
    transaction
        .simple_query(&format!("SET LOCAL ROLE \"{role}\""))
        .await?;
    let rows = transaction.query(sql, &[]).await?;
    transaction.rollback().await?;
    Ok(rows)
}

/// Read the fixture row a binding is supposed to reach.
async fn read_total(pool: &Pool, role: &str) -> Result<i32, compio_postgres::Error> {
    let rows = under_role(pool, role, "SELECT total FROM db_one.orders WHERE id = 1").await?;
    assert_eq!(rows.len(), 1, "the fixture row must be present to be read");
    Ok(rows[0].get("total"))
}

/// Release the pools before the container that serves them, and wait for the
/// sockets. A socket owned by a detached driver task outlives the runtime that
/// would have closed it.
async fn drain(pools: Vec<Rc<Pool>>) {
    drop(pools);
    assert!(
        compio_postgres::drain_connections(std::time::Duration::from_secs(5)).await,
        "the test's PostgreSQL connections must close before the container stops"
    );
}

/// **Arm 1.** `WITH SET FALSE` on the binding-to-database edge.
///
/// The ladder is the design's: a database role carrying the schema
/// privileges, two binding roles that inherit it but may not assume it, and
/// one shared worker login that may assume either binding. Without
/// `WITH SET FALSE` the worker reaches the database role directly and every
/// edge below it is decorative.
#[compio::test]
async fn a_worker_login_reaches_a_shared_database_only_through_a_live_binding_role() {
    let postgres = postgres_fixture::Postgres::start();
    let admin = superuser(&postgres).await;

    admin
        .batch_execute(
            "CREATE ROLE zs_db_one_rw NOLOGIN;
             CREATE ROLE zs_bind_a_e1 NOLOGIN;
             CREATE ROLE zs_bind_b_e1 NOLOGIN;
             CREATE ROLE zeroship_worker LOGIN PASSWORD 'fixture'
               NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
             CREATE SCHEMA db_one;
             CREATE TABLE db_one.orders (id int PRIMARY KEY, total int NOT NULL);
             INSERT INTO db_one.orders VALUES (1, 42);
             GRANT USAGE ON SCHEMA db_one TO zs_db_one_rw;
             GRANT SELECT ON db_one.orders TO zs_db_one_rw;
             GRANT zs_db_one_rw TO zs_bind_a_e1 WITH SET FALSE;
             GRANT zs_db_one_rw TO zs_bind_b_e1 WITH SET FALSE;
             GRANT zs_bind_a_e1 TO zeroship_worker WITH INHERIT FALSE;
             GRANT zs_bind_b_e1 TO zeroship_worker WITH INHERIT FALSE",
        )
        .await
        .expect("the binding ladder is legal DDL");

    let worker = login(&postgres, "zeroship_worker").await;

    // The worker cannot assume the database role, so no statement it issues
    // can carry the privileges of every binding on that database at once.
    let denied = read_total(&worker, "zs_db_one_rw")
        .await
        .expect_err("the worker must not assume the database role");
    let denied = server_error(&denied);
    assert_eq!(denied.code(), &SqlState::INSUFFICIENT_PRIVILEGE);
    assert_eq!(
        denied.message(),
        "permission denied to set role \"zs_db_one_rw\"",
        "the refusal must name the role that was refused"
    );

    // Control: both bindings read through the same login before anything is
    // revoked. Without this the denial above and the denial below would be
    // satisfied by a fixture that granted nothing at all.
    assert_eq!(
        read_total(&worker, "zs_bind_a_e1").await.unwrap(),
        42,
        "binding A reads its database"
    );
    assert_eq!(
        read_total(&worker, "zs_bind_b_e1").await.unwrap(),
        42,
        "co-tenant binding B reads the same database"
    );

    admin
        .batch_execute("REVOKE zs_db_one_rw FROM zs_bind_a_e1")
        .await
        .expect("revoking one binding's database membership");

    let revoked = read_total(&worker, "zs_bind_a_e1")
        .await
        .expect_err("binding A must lose the database when its membership is revoked");
    let revoked = server_error(&revoked);
    assert_eq!(revoked.code(), &SqlState::INSUFFICIENT_PRIVILEGE);
    assert_eq!(
        revoked.message(),
        "permission denied for schema db_one",
        "the revoked binding must fail on the schema, not on SET ROLE"
    );

    // The co-tenant is the control for the revoke: revocation has to bite one
    // binding and only one, or it is a database-wide outage rather than a
    // fence.
    assert_eq!(
        read_total(&worker, "zs_bind_b_e1").await.unwrap(),
        42,
        "revoking binding A must not disturb co-tenant binding B"
    );

    drain(vec![worker, admin]).await;
}

/// **Arm 2.** `WITH INHERIT FALSE` on the worker-to-binding edge, against the
/// `NOINHERIT` role attribute that looks like a substitute.
///
/// This arm is the entire tenant fence. Three grant shapes differing in one
/// variable, and **two of them must return the row**: if only the denial were
/// asserted, a fixture that granted nothing would pass this test while the
/// design had no isolation at all.
#[compio::test]
async fn only_a_noninheriting_membership_keeps_a_binding_out_of_ambient_login_privileges() {
    let postgres = postgres_fixture::Postgres::start();
    let admin = superuser(&postgres).await;

    admin
        .batch_execute(
            "CREATE ROLE zs_db_one_rw NOLOGIN;
             CREATE ROLE zs_bind_a_e1 NOLOGIN;
             CREATE SCHEMA db_one;
             CREATE TABLE db_one.orders (id int PRIMARY KEY, total int NOT NULL);
             INSERT INTO db_one.orders VALUES (1, 42);
             GRANT USAGE ON SCHEMA db_one TO zs_db_one_rw;
             GRANT SELECT ON db_one.orders TO zs_db_one_rw;
             GRANT zs_db_one_rw TO zs_bind_a_e1 WITH SET FALSE;
             -- One variable differs across the three logins: the shape of the
             -- worker-to-binding grant, and nothing else.
             CREATE ROLE w_plain_grant LOGIN PASSWORD 'fixture' INHERIT NOSUPERUSER;
             CREATE ROLE w_role_attribute LOGIN PASSWORD 'fixture' INHERIT NOSUPERUSER;
             CREATE ROLE w_inherit_false LOGIN PASSWORD 'fixture' INHERIT NOSUPERUSER;
             GRANT zs_bind_a_e1 TO w_plain_grant;
             GRANT zs_bind_a_e1 TO w_role_attribute;
             GRANT zs_bind_a_e1 TO w_inherit_false WITH INHERIT FALSE;
             ALTER ROLE w_role_attribute NOINHERIT",
        )
        .await
        .expect("the three grant shapes are legal DDL");

    // The catalog records the option per membership at grant time, and
    // flipping the attribute afterwards does not rewrite the row that already
    // exists. That is the mechanism the next three reads observe.
    let catalog = admin
        .query(
            "SELECT member.rolname AS login, member.rolinherit, grant_row.inherit_option
               FROM pg_auth_members grant_row
               JOIN pg_roles member ON member.oid = grant_row.member
              WHERE grant_row.roleid = 'zs_bind_a_e1'::regrole
              ORDER BY member.rolname",
            &[],
        )
        .await
        .expect("reading the membership catalog");
    let recorded: Vec<(String, bool, bool)> = catalog
        .iter()
        .map(|row| {
            (
                row.get::<_, String>("login"),
                row.get::<_, bool>("rolinherit"),
                row.get::<_, bool>("inherit_option"),
            )
        })
        .collect();
    assert_eq!(
        recorded,
        vec![
            ("w_inherit_false".to_string(), true, false),
            ("w_plain_grant".to_string(), true, true),
            ("w_role_attribute".to_string(), false, true),
        ],
        "only the membership granted WITH INHERIT FALSE records inherit_option = false; \
         flipping the role attribute leaves the existing membership inheriting"
    );

    // (a) A plain grant to an inheriting login: the privilege is ambient, so
    //     the row comes back with no SET ROLE at all. MUST SUCCEED.
    let plain = login(&postgres, "w_plain_grant").await;
    let rows = plain
        .query("SELECT total FROM db_one.orders WHERE id = 1", &[])
        .await
        .expect("a plain grant to an inheriting login is ambient");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, i32>("total"), 42);

    // (b) The same grant, with the NOINHERIT attribute applied afterwards.
    //     MUST STILL SUCCEED - this is why the attribute is not a substitute.
    let attribute = login(&postgres, "w_role_attribute").await;
    let rows = attribute
        .query("SELECT total FROM db_one.orders WHERE id = 1", &[])
        .await
        .expect("the NOINHERIT attribute does not retract an existing inheriting membership");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get::<_, i32>("total"),
        42,
        "ALTER ROLE ... NOINHERIT must not be mistaken for a fence"
    );

    // (c) The grant shape the design uses. The privilege is not ambient.
    let fenced = login(&postgres, "w_inherit_false").await;
    let denied = fenced
        .query("SELECT total FROM db_one.orders WHERE id = 1", &[])
        .await
        .expect_err("a membership granted WITH INHERIT FALSE must not be ambient");
    let denied = server_error(&denied);
    assert_eq!(denied.code(), &SqlState::INSUFFICIENT_PRIVILEGE);
    assert_eq!(denied.message(), "permission denied for schema db_one");

    // Control for (c): the membership exists and is assumable. Without this
    // the denial is indistinguishable from a login that was granted nothing.
    assert_eq!(
        read_total(&fenced, "zs_bind_a_e1").await.unwrap(),
        42,
        "the fenced login must still reach the database by assuming the binding"
    );

    drain(vec![fenced, attribute, plain, admin]).await;
}

/// **Arm 3.** The two `SET ROLE` failures split by SQLSTATE and nothing else.
///
/// `SET LOCAL ROLE` is the first statement of the data plane's setup batch and
/// the epoch rides the role name, so this split is what separates "the binding
/// was revoked" from "this epoch is gone". It **must** be measured from a real
/// non-superuser: a superuser's `SET ROLE` permission is checked against
/// `session_user`, and the run below shows the split vanishing when it is.
#[compio::test]
async fn set_role_separates_a_missing_role_from_a_role_the_session_may_not_assume() {
    let postgres = postgres_fixture::Postgres::start();
    let admin = superuser(&postgres).await;

    admin
        .batch_execute(
            "CREATE ROLE zs_stranger NOLOGIN;
             CREATE ROLE zs_bind_a_e1 NOLOGIN;
             CREATE ROLE zeroship_worker LOGIN PASSWORD 'fixture'
               NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
             GRANT zs_bind_a_e1 TO zeroship_worker WITH INHERIT FALSE",
        )
        .await
        .expect("a stranger role and a role the worker may assume");

    let worker = login(&postgres, "zeroship_worker").await;

    // Control: the worker can assume the role it is a member of, so a failure
    // below is about the named role and not about SET ROLE being unavailable.
    let assumed = under_role(&worker, "zs_bind_a_e1", "SELECT current_user AS who")
        .await
        .expect("the worker assumes the binding it is a member of");
    assert_eq!(assumed[0].get::<_, String>("who"), "zs_bind_a_e1");

    let absent = under_role(&worker, "zs_absent_role", "SELECT 1")
        .await
        .expect_err("a role that does not exist cannot be assumed");
    let absent = server_error(&absent);
    assert_eq!(
        absent.code(),
        &SqlState::INVALID_PARAMETER_VALUE,
        "role-does-not-exist is the generic bad-GUC code 22023"
    );
    assert_eq!(absent.message(), "role \"zs_absent_role\" does not exist");

    let stranger = under_role(&worker, "zs_stranger", "SELECT 1")
        .await
        .expect_err("a role the session is not a member of cannot be assumed");
    let stranger = server_error(&stranger);
    assert_eq!(
        stranger.code(),
        &SqlState::INSUFFICIENT_PRIVILEGE,
        "role-exists-but-not-a-member is 42501"
    );
    assert_eq!(
        stranger.message(),
        "permission denied to set role \"zs_stranger\""
    );
    assert_ne!(
        absent.code(),
        stranger.code(),
        "the two failures must be separable, because only the code separates them"
    );

    // The superuser control. The same two statements from a superuser session
    // report one failure and one success, so an arm that ran as a superuser
    // would never observe the split at all.
    let absent_as_superuser = under_role(&admin, "zs_absent_role", "SELECT 1")
        .await
        .expect_err("a missing role is missing for everyone");
    assert_eq!(
        server_error(&absent_as_superuser).code(),
        &SqlState::INVALID_PARAMETER_VALUE
    );
    let stranger_as_superuser = under_role(&admin, "zs_stranger", "SELECT current_user AS who")
        .await
        .expect("a superuser's SET ROLE permission is checked against session_user");
    assert_eq!(
        stranger_as_superuser[0].get::<_, String>("who"),
        "zs_stranger",
        "the non-membership failure disappears entirely for a superuser"
    );

    drain(vec![worker, admin]).await;
}

/// **Arm 4.** Column grants add; they never subtract.
///
/// The masking design withholds a classified column by granting only the
/// columns that may be read. A table-level `GRANT SELECT` alongside that
/// column list does not narrow it - it returns the plaintext - which is why
/// the blanket grant and the prospective `ALTER DEFAULT PRIVILEGES` rules are
/// deleted rather than supplemented.
#[compio::test]
async fn a_table_level_grant_returns_the_column_a_column_list_withheld() {
    let postgres = postgres_fixture::Postgres::start();
    let admin = superuser(&postgres).await;

    admin
        .batch_execute(
            "CREATE ROLE zs_db_one_rw LOGIN PASSWORD 'fixture' NOSUPERUSER;
             CREATE SCHEMA db_one;
             CREATE TABLE db_one.people (id int PRIMARY KEY, email text, email_mask text);
             INSERT INTO db_one.people VALUES (1, 'plaintext@example.com', 'p***@example.com');
             GRANT USAGE ON SCHEMA db_one TO zs_db_one_rw;
             GRANT SELECT (id, email_mask) ON db_one.people TO zs_db_one_rw",
        )
        .await
        .expect("a column list that withholds the classified column");

    let reader = login(&postgres, "zs_db_one_rw").await;

    // Control: the column list is in force, and the granted columns read.
    let granted = reader
        .query("SELECT id, email_mask FROM db_one.people", &[])
        .await
        .expect("the granted columns read");
    assert_eq!(granted.len(), 1);
    assert_eq!(
        granted[0].get::<_, String>("email_mask"),
        "p***@example.com"
    );

    let withheld = reader
        .query("SELECT email FROM db_one.people", &[])
        .await
        .expect_err("the withheld column is unreadable while only the column list stands");
    assert_eq!(
        server_error(&withheld).code(),
        &SqlState::INSUFFICIENT_PRIVILEGE
    );

    // The claim: a table-level grant alongside the column list is additive.
    admin
        .batch_execute("GRANT SELECT ON db_one.people TO zs_db_one_rw")
        .await
        .expect("a table-level grant beside a column list is accepted");
    let exposed = reader
        .query("SELECT email FROM db_one.people", &[])
        .await
        .expect("a table-level GRANT SELECT returns the column the list withheld");
    assert_eq!(exposed.len(), 1);
    assert_eq!(
        exposed[0].get::<_, String>("email"),
        "plaintext@example.com",
        "the column list did not survive the table-level grant"
    );

    // The other half of the same property: there is no column-list form of
    // ALTER DEFAULT PRIVILEGES, so a prospective rule can only ever be the
    // widening shape.
    let prospective = admin
        .batch_execute(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA db_one \
             GRANT SELECT (id) ON TABLES TO zs_db_one_rw",
        )
        .await
        .expect_err("default privileges have no column-list form");
    let prospective = server_error(&prospective);
    assert_eq!(
        prospective.message(),
        "default privileges cannot be set for columns"
    );

    // Control for the shape above: the same statement without the column list
    // is accepted, so the refusal is about columns and not about the syntax
    // around them.
    admin
        .batch_execute(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA db_one GRANT SELECT ON TABLES TO zs_db_one_rw",
        )
        .await
        .expect("the table-wide prospective grant is the only form available");

    drain(vec![reader, admin]).await;
}

/// **Arm 5.** Logical decoding consults no ACL, and the publication column
/// list is what does filter it.
///
/// One slot, one change, one role: decoded under a publication with no column
/// list the withheld column's plaintext arrives; decoded under a publication
/// whose column list omits it, it does not. The reading role is refused
/// `SELECT` on that column throughout, so the ACL is measurably not the thing
/// doing the filtering.
#[compio::test]
async fn logical_decoding_ignores_the_column_acl_and_obeys_the_publication_column_list() {
    let postgres = postgres_fixture::Postgres::start();
    let admin = superuser(&postgres).await;

    // The relay, not the worker, is the role that holds REPLICATION: the
    // worker's boot posture refuses that attribute outright.
    admin
        .batch_execute(
            "CREATE ROLE zs_relay LOGIN PASSWORD 'fixture' REPLICATION NOSUPERUSER NOBYPASSRLS;
             CREATE SCHEMA db_one;
             CREATE TABLE db_one.events (id int PRIMARY KEY, secret text NOT NULL);
             GRANT USAGE ON SCHEMA db_one TO zs_relay;
             GRANT SELECT (id) ON db_one.events TO zs_relay;
             CREATE PUBLICATION zs_pub_all FOR TABLE db_one.events;
             CREATE PUBLICATION zs_pub_columns FOR TABLE db_one.events (id)",
        )
        .await
        .expect("a relay role refused the classified column, and two publications");

    let relay = login(&postgres, "zs_relay").await;

    // Control: the granted column reads and the withheld column does not, so
    // the decoded plaintext below cannot be explained by an ACL that never
    // withheld anything.
    relay
        .query("SELECT id FROM db_one.events", &[])
        .await
        .expect("the granted column reads");
    let withheld = relay
        .query("SELECT secret FROM db_one.events", &[])
        .await
        .expect_err("the relay is refused SELECT on the classified column");
    assert_eq!(
        server_error(&withheld).code(),
        &SqlState::INSUFFICIENT_PRIVILEGE
    );

    // Both publications must exist before the slot and the change: the decoder
    // resolves publications against a historical snapshot taken at the
    // change's LSN, and one created later is not there to be named.
    relay
        .query(
            "SELECT pg_create_logical_replication_slot('zs_fence_slot', 'pgoutput')",
            &[],
        )
        .await
        .expect("the relay role may create a logical slot");
    admin
        .batch_execute("INSERT INTO db_one.events VALUES (1, 'decoded-plaintext')")
        .await
        .expect("one change to decode");

    for (publication, expect_plaintext) in [("zs_pub_all", true), ("zs_pub_columns", false)] {
        let decoded = relay
            .query(
                &format!(
                    "SELECT count(*) AS messages,
                            coalesce(bool_or(position('decoded-plaintext'::bytea in data) > 0), false)
                              AS carries_plaintext,
                            coalesce(bool_or(position('secret'::bytea in data) > 0), false)
                              AS names_the_column
                       FROM pg_logical_slot_peek_binary_changes(
                              'zs_fence_slot', NULL, NULL,
                              'proto_version', '1', 'publication_names', '{publication}')"
                ),
                &[],
            )
            .await
            .unwrap_or_else(|error| panic!("decoding under {publication}: {error}"));
        let messages: i64 = decoded[0].get("messages");
        assert!(
            messages > 0,
            "{publication} decoded no messages at all, so nothing was measured"
        );
        assert_eq!(
            decoded[0].get::<_, bool>("carries_plaintext"),
            expect_plaintext,
            "{publication}: the decoded stream is filtered by the publication column list, \
             never by the reader's column ACL"
        );
        assert_eq!(
            decoded[0].get::<_, bool>("names_the_column"),
            expect_plaintext,
            "{publication}: the relation message names exactly the published columns"
        );
    }

    drain(vec![relay, admin]).await;
}

/// **Arm 6.** A publication column list and `REPLICA IDENTITY FULL` are
/// mutually exclusive, and neither DDL step says so.
///
/// Both orders are accepted. The table then still accepts `INSERT` and refuses
/// every `UPDATE` and `DELETE`, so the symptom is not a CDC fault but a
/// creator's table silently becoming append-only.
#[compio::test]
async fn a_column_list_and_replica_identity_full_are_accepted_then_make_writes_fail() {
    let postgres = postgres_fixture::Postgres::start();
    let admin = superuser(&postgres).await;

    admin
        .batch_execute(
            "CREATE SCHEMA db_one;
             CREATE TABLE db_one.list_first (id int PRIMARY KEY, secret text);
             CREATE TABLE db_one.identity_first (id int PRIMARY KEY, secret text);
             INSERT INTO db_one.list_first VALUES (1, 'v');
             INSERT INTO db_one.identity_first VALUES (1, 'v')",
        )
        .await
        .expect("two tables, one per DDL order");

    // Control: both tables accept UPDATE and DELETE before either half of the
    // pair is applied.
    for table in ["list_first", "identity_first"] {
        admin
            .batch_execute(&format!(
                "UPDATE db_one.{table} SET secret = 'w' WHERE id = 1"
            ))
            .await
            .unwrap_or_else(|error| panic!("{table} accepts UPDATE before the conflict: {error}"));
    }

    // Order A: the column list exists, then REPLICA IDENTITY FULL is added.
    admin
        .batch_execute("CREATE PUBLICATION zs_pub_list_first FOR TABLE db_one.list_first (id)")
        .await
        .expect("a publication column list");
    admin
        .batch_execute("ALTER TABLE db_one.list_first REPLICA IDENTITY FULL")
        .await
        .expect("REPLICA IDENTITY FULL is accepted on a table that already has a column list");

    // Order B: REPLICA IDENTITY FULL exists, then the column list is created.
    admin
        .batch_execute("ALTER TABLE db_one.identity_first REPLICA IDENTITY FULL")
        .await
        .expect("REPLICA IDENTITY FULL on a table with no publication");
    admin
        .batch_execute(
            "CREATE PUBLICATION zs_pub_identity_first FOR TABLE db_one.identity_first (id)",
        )
        .await
        .expect("a column list is accepted on a table that is already REPLICA IDENTITY FULL");

    for table in ["list_first", "identity_first"] {
        let update = admin
            .batch_execute(&format!(
                "UPDATE db_one.{table} SET secret = 'x' WHERE id = 1"
            ))
            .await
            .expect_err("both halves in place must make UPDATE fail");
        let update = server_error(&update);
        assert_eq!(
            update.code(),
            &SqlState::INVALID_COLUMN_REFERENCE,
            "{table}: UPDATE must fail once the column list and the replica identity conflict"
        );
        assert_eq!(update.message(), format!("cannot update table \"{table}\""));
        assert_eq!(
            update.detail(),
            Some("Column list used by the publication does not cover the replica identity."),
            "{table}: the refusal must name the conflict rather than a CDC fault"
        );

        let delete = admin
            .batch_execute(&format!("DELETE FROM db_one.{table} WHERE id = 1"))
            .await
            .expect_err("both halves in place must make DELETE fail");
        assert_eq!(
            server_error(&delete).code(),
            &SqlState::INVALID_COLUMN_REFERENCE,
            "{table}: DELETE fails for the same reason"
        );

        // The symptom is append-only, not unavailable: INSERT still works, so
        // nothing about the write path looks broken until a row is changed.
        admin
            .batch_execute(&format!("INSERT INTO db_one.{table} VALUES (2, 'y')"))
            .await
            .unwrap_or_else(|error| panic!("{table} must still accept INSERT: {error}"));
    }

    drain(vec![admin]).await;
}

/// **Open 6.** The driver must surface the first failing statement of the
/// setup batch by SQLSTATE, unchanged.
///
/// The data plane sends `SET LOCAL ROLE ...; SET LOCAL statement_timeout ...;
/// SET LOCAL lock_timeout ...` as one `simple_query` inside an explicit
/// transaction
/// (`crates/zeroship-data-orm/src/backend/postgres/pg_autocommit.rs`,
/// `with_scoped_transaction`). `PostgreSQL` aborts that batch at the first
/// failing statement and emits exactly one `ErrorResponse`. If
/// `compio-postgres` collapsed, dropped, reordered or masked it,
/// `GRANT_REVOKED` and `SCHEMA_EPOCH_STALE` would be one error and the epoch
/// fence could not report itself.
#[compio::test]
async fn the_driver_reports_the_setup_batch_s_first_failure_by_sqlstate() {
    let postgres = postgres_fixture::Postgres::start();
    let admin = superuser(&postgres).await;

    admin
        .batch_execute(
            "CREATE ROLE zs_stranger NOLOGIN;
             CREATE ROLE zs_bind_a_e1 NOLOGIN;
             CREATE ROLE zeroship_worker LOGIN PASSWORD 'fixture'
               NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
             GRANT zs_bind_a_e1 TO zeroship_worker WITH INHERIT FALSE",
        )
        .await
        .expect("one assumable role and one stranger");

    let worker = login(&postgres, "zeroship_worker").await;

    // The shape `pg_session_sql::autocommit_local_session_setup_sql` composes:
    // the role first, then the budgets, in one simple-query batch.
    let setup_batch = |role: &str| {
        format!(
            "SET LOCAL ROLE \"{role}\"; \
             SET LOCAL statement_timeout = {DB_STATEMENT_TIMEOUT_MS}; \
             SET LOCAL lock_timeout = {DB_LOCK_TIMEOUT_MS}"
        )
    };

    // Control: the same batch with a role the session may assume succeeds, and
    // every statement in it took effect. Without this the failures below could
    // be a batch the driver never sent.
    {
        let mut connection = worker.acquire().await.expect("a pooled connection");
        let transaction = connection.transaction().await.expect("BEGIN");
        transaction
            .simple_query(&setup_batch("zs_bind_a_e1"))
            .await
            .expect("the setup batch succeeds for an assumable role");
        let applied = transaction
            .query(
                // The server normalises a millisecond GUC to its own spelling,
                // so compare durations rather than the rendered string.
                &format!(
                    "SELECT current_user AS who,
                            current_setting('statement_timeout')::interval
                              = interval '{DB_STATEMENT_TIMEOUT_MS} ms' AS statement_timeout_applied,
                            current_setting('lock_timeout')::interval
                              = interval '{DB_LOCK_TIMEOUT_MS} ms' AS lock_timeout_applied"
                ),
                &[],
            )
            .await
            .expect("reading the settings the batch applied");
        assert_eq!(applied[0].get::<_, String>("who"), "zs_bind_a_e1");
        assert!(
            applied[0].get::<_, bool>("statement_timeout_applied"),
            "the statement after the role must have run"
        );
        assert!(
            applied[0].get::<_, bool>("lock_timeout_applied"),
            "the last statement of the batch must have run"
        );
        transaction.rollback().await.expect("ROLLBACK");
    }

    // Both failures the taxonomy splits on, each from the FIRST statement of
    // the batch, each inside an explicit transaction.
    for (role, expected, expected_message) in [
        (
            "zs_stranger",
            &SqlState::INSUFFICIENT_PRIVILEGE,
            "permission denied to set role \"zs_stranger\"",
        ),
        (
            "zs_absent_role",
            &SqlState::INVALID_PARAMETER_VALUE,
            "role \"zs_absent_role\" does not exist",
        ),
    ] {
        let mut connection = worker.acquire().await.expect("a pooled connection");
        let transaction = connection.transaction().await.expect("BEGIN");
        let failure = transaction
            .simple_query(&setup_batch(role))
            .await
            .err()
            .unwrap_or_else(|| panic!("the setup batch naming {role} must fail"));
        let failure = server_error(&failure);
        assert_eq!(
            failure.code(),
            expected,
            "the driver must carry the first statement's SQLSTATE through the batch",
        );
        assert_eq!(
            failure.message(),
            expected_message,
            "the driver must carry the first statement's message, not a later statement's",
        );

        // PostgreSQL aborts the batch at the first failure, so nothing after
        // the role statement ran and the transaction is unusable. A driver
        // that had swallowed the first error and reported a later one would
        // leave a transaction that still worked.
        let aborted = transaction
            .query("SELECT 1", &[])
            .await
            .expect_err("the batch aborted the transaction at its first statement");
        assert_eq!(
            server_error(&aborted).code(),
            &SqlState::IN_FAILED_SQL_TRANSACTION
        );
        transaction.rollback().await.expect("ROLLBACK");
    }

    drain(vec![worker, admin]).await;
}
