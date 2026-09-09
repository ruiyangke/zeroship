//! Live-PG roundtrip for `auth::identity::magic_link`.
//!
//! Skipped unless a test database is available (`PG_TEST_URL` or the TOML overlay). Each test scopes itself with a
//! random email so concurrent runs don't collide; the cleanup at the end
//! removes every row that test inserted.

use std::time::Duration;

use compio_postgres::{connect, Client, NoTls};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use zeroship_auth::identity::{magic_link, password_reset};
use zeroship_auth::ui::magic::completions_store::{self, ConsumeError};

// `compio_postgres::Client` is `!Send` — the futures inherit that
// structurally. The lint is informational, not actionable here.
#[allow(clippy::future_not_send)]
async fn pg() -> Option<compio_postgres::Client> {
    let dsn = zeroship_core::config::test_database_url_opt()?;
    Some(pg_connect(&dsn).await)
}

async fn pg_connect(dsn: &str) -> Client {
    let (client, connection) = connect(dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("magic_link test pg connection error: {e}");
        }
    })
    .detach();
    client
}

// THE TABLES THESE TRIGGERS SIT ON ARE SHARED WITH EVERY CONCURRENT RUN, so
// each install below scopes itself twice and both halves are load-bearing.
//
// The NAME is per-call. With one fixed name, a peer run's `CREATE OR REPLACE
// FUNCTION` silently rewrites the body this run's trigger executes, and its
// `DROP TRIGGER IF EXISTS` deletes this run's trigger outright - which removes
// the race window the test exists to open and turns the assertion below into a
// claim about a race that never happened.
//
// The `WHEN` clause is what stops the trigger FIRING for the peer's rows.
// Renaming alone makes the collision quieter without removing it: a uniquely
// named trigger on a shared table still sleeps 0.2s inside every insert any
// other run makes, and these triggers exist precisely to widen a window, so an
// unscoped one widens the peer's windows too.
//
// The model is `signing_key_retention_test.rs`, which has done both since it
// was written; it discriminates on `application_name` because a retirement
// UPDATE carries nothing else to key on. Here the inserted row carries the
// test's own random email or nonce, so the WHEN clause can name the row itself.
//
// MEASURED 2026-08-20, this file's four DDL-installing modules run in two
// concurrent processes against one shared database, five pairs each way:
// `wrong_code_does_not_mutate_reserved_completion` failed in 2 of 10 runs
// before, 0 of 10 after.
//
// WHAT THE `WHEN` CLAUSE DOES NOT DO. It is evaluated on every peer row
// regardless - Postgres checks the expression for each insert into the shared
// table while the trigger exists; what it saves is the 0.2s sleep, not the
// evaluation. It does not scope the ACCESS EXCLUSIVE lock `CREATE TRIGGER` and
// `DROP TRIGGER` take on that table, so a peer's writes still wait out this
// run's DDL. And neither half is RAII: a panic between install and drop leaks
// the pair. The leak is at least inert now, since a row-scoped trigger can
// never match anything again, where the old fixed names left a live trigger
// firing for every later run.
//
// Both installs return the object name; the function and the trigger share it,
// since they live in different namespaces.

async fn install_magic_links_insert_delay(client: &Client, email: &str) -> String {
    let name = format!(
        "test_sleep_before_magic_link_insert_{}",
        Uuid::new_v4().simple()
    );
    client
        .execute(
            &format!(
                "CREATE FUNCTION zeroship.{name}() \
                 RETURNS trigger LANGUAGE plpgsql AS $$ \
                 BEGIN \
                     PERFORM pg_sleep(0.2); \
                     RETURN NEW; \
                 END \
                 $$"
            ),
            &[],
        )
        .await
        .expect("create insert delay function");
    // `email` is this test's own uuid-derived address, so it carries no quote
    // to escape; no caller-supplied string ever reaches this SQL.
    client
        .execute(
            &format!(
                "CREATE TRIGGER {name} \
                 BEFORE INSERT ON zeroship.magic_links \
                 FOR EACH ROW WHEN (NEW.email = '{email}'::citext) \
                 EXECUTE FUNCTION zeroship.{name}()"
            ),
            &[],
        )
        .await
        .expect("create insert delay trigger");
    name
}

// Drops the FUNCTION as well as the trigger. Nothing dropped the function when
// the names were fixed, which was invisible then because the next run replaced
// the one object in place; with per-run names it would leave one orphan per run
// in a database that is never dropped. Checked 2026-08-20: the shared test
// database held three such `zeroship.test_sleep_*` functions with no trigger
// referencing any of them.
async fn drop_magic_links_insert_delay(client: &Client, name: &str) {
    client
        .execute(
            &format!("DROP TRIGGER IF EXISTS {name} ON zeroship.magic_links"),
            &[],
        )
        .await
        .ok();
    client
        .execute(&format!("DROP FUNCTION IF EXISTS zeroship.{name}()"), &[])
        .await
        .ok();
}

async fn install_magic_completion_reserve_delay(client: &Client, csrf_nonce: &str) -> String {
    let name = format!(
        "test_sleep_before_magic_completion_reserve_{}",
        Uuid::new_v4().simple()
    );
    client
        .execute(
            &format!(
                "CREATE FUNCTION zeroship.{name}() \
                 RETURNS trigger LANGUAGE plpgsql AS $$ \
                 BEGIN \
                     PERFORM pg_sleep(0.2); \
                     RETURN NEW; \
                 END \
                 $$"
            ),
            &[],
        )
        .await
        .expect("create completion reserve delay function");
    // The reservation predicate moved out of the function body and into `WHEN`,
    // next to the nonce scoping, so one place decides whether this fires.
    client
        .execute(
            &format!(
                "CREATE TRIGGER {name} \
                 BEFORE UPDATE OF consumed_pending_at ON zeroship.magic_completions \
                 FOR EACH ROW WHEN (NEW.csrf_nonce = '{csrf_nonce}' \
                     AND NEW.consumed_pending_at IS NOT NULL \
                     AND OLD.consumed_pending_at IS NULL) \
                 EXECUTE FUNCTION zeroship.{name}()"
            ),
            &[],
        )
        .await
        .expect("create completion reserve delay trigger");
    name
}

async fn drop_magic_completion_reserve_delay(client: &Client, name: &str) {
    client
        .execute(
            &format!("DROP TRIGGER IF EXISTS {name} ON zeroship.magic_completions"),
            &[],
        )
        .await
        .ok();
    client
        .execute(&format!("DROP FUNCTION IF EXISTS zeroship.{name}()"), &[])
        .await
        .ok();
}

fn sha256(s: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().into()
}

#[compio::test]
async fn wrong_code_does_not_mutate_reserved_completion() {
    let dsn = match zeroship_core::config::test_database_url_opt() {
        Some(dsn) => dsn,
        None => {
            zeroship_test_support::skip("skipping magic_link_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
            return;
        }
    };
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping magic_link_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };

    let csrf_nonce = format!("completion-reserved-{}", Uuid::new_v4().simple());
    let email = format!("magic-reserved-{}@example.test", Uuid::new_v4().simple());
    let login_challenge = format!("lc-{}", Uuid::new_v4().simple());
    let code = "123456";

    client
        .execute(
            "INSERT INTO zeroship.magic_completions \
                (csrf_nonce, code, email, login_challenge, expires_at) \
             VALUES ($1, $2, $3::citext, $4, NOW() + INTERVAL '5 minutes')",
            &[&csrf_nonce, &code, &email, &login_challenge],
        )
        .await
        .expect("insert completion row");
    let reserve_delay = install_magic_completion_reserve_delay(&client, &csrf_nonce).await;

    let correct_client = pg_connect(&dsn).await;
    let wrong_client = pg_connect(&dsn).await;
    let correct_nonce = csrf_nonce.clone();
    let wrong_nonce = csrf_nonce.clone();
    let correct = compio::runtime::spawn(async move {
        completions_store::consume_pending(&correct_client, &correct_nonce, code).await
    });
    compio::time::sleep(Duration::from_millis(50)).await;
    let wrong = compio::runtime::spawn(async move {
        completions_store::consume_pending(&wrong_client, &wrong_nonce, "000000").await
    });

    let correct = correct
        .await
        .expect("join correct consume")
        .expect("correct consume");
    assert_eq!(correct.email, email);
    let wrong = wrong
        .await
        .expect("join wrong consume")
        .expect_err("wrong code must not consume reserved completion");
    assert!(
        matches!(wrong, ConsumeError::WrongCode),
        "wrong code racing a reservation should return WrongCode, got {wrong:?}"
    );

    drop_magic_completion_reserve_delay(&client, &reserve_delay).await;

    let row = client
        .query_one(
            "SELECT attempts, \
                    consumed_pending_at IS NOT NULL AS pending, \
                    consumed_at IS NOT NULL AS consumed \
             FROM zeroship.magic_completions \
             WHERE csrf_nonce = $1",
            &[&csrf_nonce],
        )
        .await
        .expect("load completion row");
    let attempts: i16 = row.get("attempts");
    let pending: bool = row.get("pending");
    let consumed: bool = row.get("consumed");
    // The winning CORRECT consume increments `attempts` to 1 as part of its
    // reservation (consistent with
    // `concurrent_correct_magic_completions_do_not_count_as_wrong_attempts`,
    // which asserts the same). The point of THIS test is that the concurrent
    // WRONG code must NOT increment it further — its wrong-code UPDATE is
    // gated on `consumed_pending_at IS NULL`, so once the correct code has
    // reserved the row the wrong code matches nothing. So attempts must be
    // exactly 1 (the correct reservation), never 2.
    assert_eq!(
        attempts, 1,
        "wrong-code update must not increment attempts beyond the correct reservation's"
    );
    assert!(pending, "correct code should reserve the completion");
    assert!(!consumed, "completion should not be finalized by consume_pending");

    client
        .execute(
            "DELETE FROM zeroship.magic_completions WHERE csrf_nonce = $1",
            &[&csrf_nonce],
        )
        .await
        .ok();
}

#[compio::test]
async fn concurrent_issue_leaves_one_active_token() {
    let dsn = match zeroship_core::config::test_database_url_opt() {
        Some(dsn) => dsn,
        None => {
            zeroship_test_support::skip("skipping magic_link_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
            return;
        }
    };
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping magic_link_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };

    let email = format!("magic-concurrent-{}@example.test", Uuid::new_v4().simple());
    let insert_delay = install_magic_links_insert_delay(&client, &email).await;

    let client_a = pg_connect(&dsn).await;
    let client_b = pg_connect(&dsn).await;
    let email_a = email.clone();
    let email_b = email.clone();
    let issue_a =
        compio::runtime::spawn(async move { magic_link::issue(&client_a, &email_a, "login").await });
    let issue_b =
        compio::runtime::spawn(async move { magic_link::issue(&client_b, &email_b, "login").await });

    issue_a.await.expect("join issue A").expect("issue A");
    issue_b.await.expect("join issue B").expect("issue B");

    drop_magic_links_insert_delay(&client, &insert_delay).await;

    let active_count: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM zeroship.magic_links \
             WHERE email = $1::citext AND purpose = $2 AND consumed_at IS NULL",
            &[&email, &"login"],
        )
        .await
        .expect("count active magic links")
        .get(0);
    assert_eq!(
        active_count, 1,
        "concurrent issue must leave exactly one active magic-link token"
    );

    client
        .execute(
            "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn issue_then_redeem_happy_path() {
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping magic_link_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };

    let email = format!("magic-happy-{}@example.test", Uuid::new_v4().simple());
    let issued = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue");
    assert!(!issued.raw.is_empty(), "raw token must be non-empty");
    assert!(
        !issued.csrf_nonce.is_empty(),
        "csrf nonce must be non-empty"
    );

    let redeemed = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect("redeem")
        .expect("redeem should return Some on first call");
    assert_eq!(redeemed.email, email);
    assert_eq!(redeemed.csrf_nonce, issued.csrf_nonce);
    assert_eq!(redeemed.purpose, "login");
    assert!(
        magic_link::finalize_consume(&client, &redeemed.token_hash, &redeemed.reserved_at)
            .await
            .expect("finalize consume"),
        "finalize should update the pending row"
    );

    // Cleanup.
    client
        .execute(
            "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn second_redeem_returns_none() {
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping magic_link_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };

    let email = format!("magic-once-{}@example.test", Uuid::new_v4().simple());
    let issued = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue");

    let first = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect("redeem 1");
    let first = first.expect("first redeem must succeed");
    assert!(
        magic_link::finalize_consume(&client, &first.token_hash, &first.reserved_at)
            .await
            .expect("finalize consume"),
        "finalize should update the pending row"
    );

    let second = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect("redeem 2");
    assert!(
        second.is_none(),
        "second redeem must return None (single-use)"
    );

    client
        .execute(
            "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn pending_consume_can_be_cleared_and_retried_before_finalize() {
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping magic_link_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };

    let email = format!("magic-pending-retry-{}@example.test", Uuid::new_v4().simple());
    let issued = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue");

    let first = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect("redeem pending 1")
        .expect("first redeem should reserve token");
    assert_eq!(first.email, email);

    let rows = client
        .query(
            "SELECT consumed_pending_at IS NOT NULL AS pending, \
                    consumed_at IS NOT NULL AS consumed \
             FROM zeroship.magic_links \
             WHERE token_hash = $1",
            &[&first.token_hash.as_slice()],
        )
        .await
        .expect("load pending row");
    assert_eq!(rows.len(), 1, "magic link row should exist");
    let pending: bool = rows[0].get("pending");
    let consumed: bool = rows[0].get("consumed");
    assert!(pending, "redeem_pending should set consumed_pending_at");
    assert!(!consumed, "redeem_pending must not finalize consumed_at");

    assert!(
        magic_link::clear_consume_pending(&client, &first.token_hash, Some(&first.reserved_at))
            .await
            .expect("clear consume pending"),
        "clear should update the pending row"
    );

    let second = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect("redeem pending 2")
        .expect("same token should be retriable after clear");
    assert_eq!(second.email, email);

    assert!(
        magic_link::finalize_consume(&client, &second.token_hash, &second.reserved_at)
            .await
            .expect("finalize consume"),
        "finalize should set consumed_at"
    );

    let third = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect("redeem after finalize");
    assert!(third.is_none(), "finalized token should not redeem again");

    client
        .execute(
            "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn stale_magic_link_reservation_cannot_finalize_or_clear_newer_reservation() {
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping magic_link_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };

    let email = format!("magic-stale-reservation-{}@example.test", Uuid::new_v4().simple());
    let issued = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue");

    let first = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect("redeem pending 1")
        .expect("first redeem should reserve token");
    // Age the reservation past PENDING_STALE_SECONDS (5s for magic_links —
    // unlike magic_completions' 60s retry window). magic_links BURN stale
    // reservations rather than letting them be retried (the secure default,
    // documented on `redeem_pending` and covered by
    // `stale_pending_redeem_burns_link_as_consumed`).
    client
        .execute(
            "UPDATE zeroship.magic_links \
             SET consumed_pending_at = NOW() - INTERVAL '61 seconds' \
             WHERE token_hash = $1",
            &[&first.token_hash.as_slice()],
        )
        .await
        .expect("age first reservation");

    // The second redeem observes the stale reservation and BURNS the link
    // (marks consumed_at). It does NOT hand back a fresh reservation.
    let burned = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect_err("redeem pending 2 should burn the stale reservation");
    assert!(
        matches!(burned, magic_link::RedeemError::AlreadyConsumed),
        "stale magic_link reservation must be burned (AlreadyConsumed), got {burned:?}"
    );

    // Now the original (stale) owner cannot finalize or clear the row: it has
    // been consumed by the burn, so its `consumed_at IS NULL` guard fails.
    assert!(
        !magic_link::finalize_consume(&client, &first.token_hash, &first.reserved_at)
            .await
            .expect("stale finalize"),
        "stale owner must not finalize a burned reservation"
    );
    assert!(
        !magic_link::clear_consume_pending(&client, &first.token_hash, Some(&first.reserved_at))
            .await
            .expect("stale clear"),
        "stale owner must not clear a burned reservation"
    );

    // And the row is indeed consumed (the burn stuck).
    let row = client
        .query_one(
            "SELECT consumed_at IS NOT NULL AS consumed FROM zeroship.magic_links \
             WHERE token_hash = $1",
            &[&first.token_hash.as_slice()],
        )
        .await
        .expect("load burned row");
    let consumed: bool = row.get("consumed");
    assert!(consumed, "burned stale reservation must be marked consumed_at");

    client
        .execute(
            "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn second_redeem_while_pending_returns_in_flight() {
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping magic_link_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };

    let email = format!("magic-inflight-{}@example.test", Uuid::new_v4().simple());
    let issued = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue");

    let first = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect("redeem pending 1")
        .expect("first redeem should reserve token");

    let err = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect_err("second redeem during pending window should be in-flight");
    assert!(
        matches!(err, magic_link::RedeemError::InFlight),
        "second pending redeem should return InFlight, got {err:?}"
    );

    magic_link::clear_consume_pending(&client, &first.token_hash, Some(&first.reserved_at))
        .await
        .expect("clear consume pending");

    client
        .execute(
            "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn stale_pending_redeem_burns_link_as_consumed() {
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping magic_link_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };

    let email = format!("magic-stale-pending-{}@example.test", Uuid::new_v4().simple());
    let issued = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue");

    let first = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect("redeem pending 1")
        .expect("first redeem should reserve token");

    client
        .execute(
            "UPDATE zeroship.magic_links \
             SET consumed_pending_at = NOW() - INTERVAL '6 seconds' \
             WHERE token_hash = $1",
            &[&first.token_hash.as_slice()],
        )
        .await
        .expect("age pending reservation");

    let err = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect_err("stale pending redeem should burn link");
    assert!(
        matches!(err, magic_link::RedeemError::AlreadyConsumed),
        "stale pending redeem should return AlreadyConsumed, got {err:?}"
    );

    let rows = client
        .query(
            "SELECT consumed_at IS NOT NULL AS consumed \
             FROM zeroship.magic_links \
             WHERE token_hash = $1",
            &[&first.token_hash.as_slice()],
        )
        .await
        .expect("load burned row");
    assert_eq!(rows.len(), 1, "magic link row should exist");
    let consumed: bool = rows[0].get("consumed");
    assert!(consumed, "stale pending redeem should mark consumed_at");

    client
        .execute(
            "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn redeem_rejects_reset_purpose_row_without_consuming_it() {
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping magic_link_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };

    let email = format!("magic-reset-purpose-{}@example.test", Uuid::new_v4().simple());
    let raw_token = format!("reset-token-{}", Uuid::new_v4().simple());
    let token_hash = sha256(&raw_token);
    let csrf_nonce = "reset-no-csrf-nonce";
    let purpose = "reset";

    client
        .execute(
            "INSERT INTO zeroship.magic_links \
                (token_hash, email, csrf_nonce, purpose, expires_at) \
             VALUES ($1, $2::citext, $3, $4, NOW() + INTERVAL '60 minutes')",
            &[&token_hash.as_slice(), &email, &csrf_nonce, &purpose],
        )
        .await
        .expect("insert reset-purpose row");

    let attempt = magic_link::redeem_pending(&client, &raw_token)
        .await
        .expect("redeem reset-purpose row via magic login");
    assert!(
        attempt.is_none(),
        "reset-purpose row must not redeem through magic-link login"
    );

    let rows = client
        .query(
            "SELECT consumed_at IS NULL AS still_unconsumed \
             FROM zeroship.magic_links \
             WHERE token_hash = $1 AND email = $2::citext AND purpose = $3",
            &[&token_hash.as_slice(), &email, &purpose],
        )
        .await
        .expect("load reset-purpose row after rejected redeem");
    assert_eq!(rows.len(), 1, "test row must still exist");
    let still_unconsumed: bool = rows[0].get("still_unconsumed");
    assert!(
        still_unconsumed,
        "rejected reset-purpose row must remain unconsumed"
    );

    client
        .execute(
            "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn expired_token_returns_none() {
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping magic_link_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };

    let email = format!("magic-expired-{}@example.test", Uuid::new_v4().simple());
    let issued = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue");

    // Force the row's expiry into the past.
    client
        .execute(
            "UPDATE zeroship.magic_links SET expires_at = NOW() - INTERVAL '1 minute' \
             WHERE email = $1::citext AND consumed_at IS NULL",
            &[&email],
        )
        .await
        .expect("force expiry");

    let attempt = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect("redeem");
    assert!(attempt.is_none(), "expired token must NOT redeem");

    client
        .execute(
            "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn new_issue_supersedes_previous_unconsumed() {
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping magic_link_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };

    let email = format!("magic-super-{}@example.test", Uuid::new_v4().simple());
    let first = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue 1");
    let _second = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue 2");

    // The first raw token must no longer be redeemable.
    let attempt = magic_link::redeem_pending(&client, &first.raw)
        .await
        .expect("redeem old");
    assert!(
        attempt.is_none(),
        "previous unconsumed token must be invalidated by a fresh issue"
    );

    client
        .execute(
            "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn login_issue_does_not_supersede_reset_token() {
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping magic_link_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };

    let email = format!("magic-reset-kept-{}@example.test", Uuid::new_v4().simple());
    // A reset token binds to the issuing user's immutable id (security finding
    // L4), so `issue` only writes a row when the email maps to a real user —
    // mirroring the production `/forgot` caller, which guards on `find_by_email`.
    let user = zeroship_auth::store::users::create(&client, &email, "Test", None)
        .await
        .expect("seed user");
    let reset = password_reset::issue(&client, &email)
        .await
        .expect("issue reset token");
    let _login = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue login token");

    let redeemed_reset = password_reset::redeem(&client, &reset.raw)
        .await
        .expect("redeem reset token after login issue");
    assert!(
        redeemed_reset.is_some(),
        "login-purpose magic issue must not consume reset-purpose tokens"
    );

    client
        .execute(
            "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
    client
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id.as_str()])
        .await
        .ok();
}

#[compio::test]
async fn magic_completion_invalidates_after_five_wrong_codes() {
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping magic_link_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };

    let csrf_nonce = format!("completion-attempts-{}", Uuid::new_v4().simple());
    let email = format!("magic-complete-{}@example.test", Uuid::new_v4().simple());
    let login_challenge = format!("lc-{}", Uuid::new_v4().simple());
    let code = "123456";

    client
        .execute(
            "INSERT INTO zeroship.magic_completions \
                (csrf_nonce, code, email, login_challenge, expires_at) \
             VALUES ($1, $2, $3::citext, $4, NOW() + INTERVAL '5 minutes')",
            &[&csrf_nonce, &code, &email, &login_challenge],
        )
        .await
        .expect("insert completion row");

    for i in 1..=4 {
        let err = completions_store::consume_pending(&client, &csrf_nonce, "000000")
            .await
            .expect_err("wrong code must fail before invalidation");
        assert!(
            matches!(err, ConsumeError::WrongCode),
            "wrong attempt {i} should return WrongCode, got {err:?}"
        );
    }

    let err = completions_store::consume_pending(&client, &csrf_nonce, "000000")
        .await
        .expect_err("fifth wrong code must fail and invalidate");
    assert!(
        matches!(err, ConsumeError::WrongCode),
        "fifth wrong attempt should return WrongCode, got {err:?}"
    );

    let rows = client
        .query(
            "SELECT consumed_at IS NOT NULL AS consumed \
             FROM zeroship.magic_completions \
             WHERE csrf_nonce = $1",
            &[&csrf_nonce],
        )
        .await
        .expect("load completion row");
    assert_eq!(rows.len(), 1, "completion row should still exist");
    let consumed: bool = rows[0].get("consumed");
    assert!(
        consumed,
        "fifth wrong completion attempt must invalidate the row"
    );

    let err = completions_store::consume_pending(&client, &csrf_nonce, code)
        .await
        .expect_err("correct code must not redeem after invalidation");
    assert!(
        matches!(err, ConsumeError::WrongCode),
        "correct code after invalidation should return WrongCode, got {err:?}"
    );

    client
        .execute(
            "DELETE FROM zeroship.magic_completions WHERE csrf_nonce = $1",
            &[&csrf_nonce],
        )
        .await
        .ok();
}

#[compio::test]
async fn concurrent_correct_magic_completions_do_not_count_as_wrong_attempts() {
    let Some(seed_client) = pg().await else {
        zeroship_test_support::skip("skipping magic_link_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };
    let dsn = zeroship_core::config::test_database_url_opt()
        .expect("test database URL present after pg");

    let csrf_nonce = format!("completion-race-{}", Uuid::new_v4().simple());
    let email = format!("magic-race-{}@example.test", Uuid::new_v4().simple());
    let login_challenge = format!("lc-{}", Uuid::new_v4().simple());
    let code = "123456";

    seed_client
        .execute(
            "INSERT INTO zeroship.magic_completions \
                (csrf_nonce, code, email, login_challenge, expires_at) \
             VALUES ($1, $2, $3::citext, $4, NOW() + INTERVAL '5 minutes')",
            &[&csrf_nonce, &code, &email, &login_challenge],
        )
        .await
        .expect("insert completion row");

    let mut handles = Vec::new();
    for _ in 0..5 {
        let client = pg_connect(&dsn).await;
        let csrf_nonce = csrf_nonce.clone();
        handles.push(compio::runtime::spawn(async move {
            completions_store::consume_pending(&client, &csrf_nonce, code).await
        }));
    }

    let mut accepted = 0;
    let mut in_flight = 0;
    let mut wrong = 0;
    for handle in handles {
        match handle.await.expect("completion task panicked") {
            Ok(_) => accepted += 1,
            Err(ConsumeError::InFlight) => in_flight += 1,
            Err(ConsumeError::WrongCode) => wrong += 1,
            Err(ConsumeError::Store(err)) => panic!("completion store error: {err}"),
        }
    }

    assert_eq!(accepted, 1, "one correct completion should reserve");
    assert_eq!(in_flight, 4, "other correct completions should see in-flight");
    assert_eq!(wrong, 0, "correct completions must not hit wrong-code path");

    let rows = seed_client
        .query(
            "SELECT attempts, consumed_at IS NOT NULL AS consumed \
             FROM zeroship.magic_completions \
             WHERE csrf_nonce = $1",
            &[&csrf_nonce],
        )
        .await
        .expect("load completion row");
    assert_eq!(rows.len(), 1, "completion row should still exist");
    let attempts: i16 = rows[0].get("attempts");
    let consumed: bool = rows[0].get("consumed");
    assert_eq!(
        attempts, 1,
        "only the winning correct consume should increment attempts"
    );
    assert!(
        !consumed,
        "concurrent correct submissions must not consume before finalize"
    );

    seed_client
        .execute(
            "DELETE FROM zeroship.magic_completions WHERE csrf_nonce = $1",
            &[&csrf_nonce],
        )
        .await
        .ok();
}

#[compio::test]
async fn stale_magic_completion_reservation_cannot_finalize_newer_reservation() {
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping magic_link_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };

    let csrf_nonce = format!("completion-stale-{}", Uuid::new_v4().simple());
    let email = format!("magic-completion-stale-{}@example.test", Uuid::new_v4().simple());
    let login_challenge = format!("lc-{}", Uuid::new_v4().simple());
    let code = "123456";

    client
        .execute(
            "INSERT INTO zeroship.magic_completions \
                (csrf_nonce, code, email, login_challenge, expires_at) \
             VALUES ($1, $2, $3::citext, $4, NOW() + INTERVAL '5 minutes')",
            &[&csrf_nonce, &code, &email, &login_challenge],
        )
        .await
        .expect("insert completion row");

    let first = completions_store::consume_pending(&client, &csrf_nonce, code)
        .await
        .expect("first consume");
    client
        .execute(
            "UPDATE zeroship.magic_completions \
             SET consumed_pending_at = NOW() - INTERVAL '61 seconds' \
             WHERE csrf_nonce = $1",
            &[&csrf_nonce],
        )
        .await
        .expect("age first reservation");
    let second = completions_store::consume_pending(&client, &csrf_nonce, code)
        .await
        .expect("second consume after stale reservation");

    assert!(
        !completions_store::finalize_consume(&client, &csrf_nonce, &first.reserved_at)
            .await
            .expect("stale completion finalize"),
        "stale owner must not finalize the newer completion reservation"
    );
    assert!(
        !completions_store::clear_consume_pending(&client, &csrf_nonce, Some(&first.reserved_at))
            .await
            .expect("stale completion clear"),
        "stale owner must not clear the newer completion reservation"
    );
    assert!(
        completions_store::finalize_consume(&client, &csrf_nonce, &second.reserved_at)
            .await
            .expect("current completion finalize"),
        "current completion owner should finalize"
    );

    client
        .execute(
            "DELETE FROM zeroship.magic_completions WHERE csrf_nonce = $1",
            &[&csrf_nonce],
        )
        .await
        .ok();
}
