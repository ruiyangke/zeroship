//! Account-deletion / GDPR-erase lifecycle - live PG (ISS-12).
//!
//! Requires a live PostgreSQL (`PG_TEST_URL` or the TOML overlay). A run
//! that cannot reach one is REFUSED, not skipped. These drive the
//! REAL store transactions, the REAL undo token, the REAL HTTP routes and the
//! REAL reaper tick - no shims - so a green run exercises the same code path
//! `/me/delete`, `/me/delete/cancel` and the `account_reaper` cron take in
//! production.
//!
//! Each test scopes itself with a random tag and cleans up the rows it
//! inserted.
//!
//! `account_reaper::tick` is a FLEET-WIDE due-scan: it erases EVERY past-grace
//! user and returns aggregate counts, so two tick-driving tests each see the
//! other's due user and the `report.erased` assertions break. That was stated
//! here and then guarded with a process-wide `Mutex`, which is right about the
//! mechanism and one boundary short: every run on this migration set is a
//! different PROCESS against the same database. MEASURED 2026-08-20, six copies
//! of this file started together against one database - 4 of 6 red, then 2 of
//! 6, then 0 of 6, every failure on those counts. The lease is a session
//! advisory lock, so it excludes peer runs too; see [`common::lease_sweep`].
//! The counts are FLOORS now rather than figures, because a lease excludes a
//! live peer and not a row a crashed one left past its grace, and the exact
//! claim - what happened to this run's own user - is the row check under each.

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;

use zeroship_auth::cron::account_reaper::{self, ControlAccess};
use zeroship_auth::identity::deletion_cancel;
use zeroship_auth::store::users;

use crate::common;
use crate::common::mock_control::{Answer, MockControl};

#[allow(clippy::future_not_send)]
async fn pg() -> Client {
    let dsn = crate::common::test_database_url();
    open(&dsn).await
}

#[allow(clippy::future_not_send)]
async fn open(dsn: &str) -> Client {
    let (client, connection) = connect(dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("account_deletion test pg connection error: {e}");
        }
    })
    .detach();
    client
}

/// The test DSN rewritten to connect as the REAL `zeroship_auth` role instead
/// of the superuser every other test in this suite uses.
///
/// This is not fastidiousness. The reaper's deleted `user_has_financial_history`
/// read three control-owned tables on this connection, and MEASURED
/// `zeroship_auth` holds no privilege on any of them - so the erasure it gated
/// did not degrade under the real role, it raised `42501` and failed. A suite
/// that only ever connects as `postgres` cannot see that class of defect, which
/// is exactly how it survived.
///
/// The credentials are the ones the corpus creates
/// (`db/migrations-ts/20260702000100_schema_roles_extensions.ts`).
fn as_auth_role(dsn: &str) -> String {
    let mut url = url::Url::parse(dsn).expect("test DSN parses");
    url.set_username("zeroship_auth").expect("set username");
    url.set_password(Some("zeroship_auth")).expect("set password");
    url.to_string()
}

/// A control plane that always says "erasure may proceed", for the tests whose
/// subject is something else.
#[allow(clippy::future_not_send)]
async fn clear_control() -> (MockControl, ControlAccess) {
    let mock = MockControl::start(Answer::Clear).await;
    let access = ControlAccess {
        control_url: mock.base.clone(),
        keyring: mock.keyring(),
    };
    (mock, access)
}

/// Force a user's scheduled erasure into the past so the reaper's due-scan
/// selects it without waiting out the 30-day window.
#[allow(clippy::future_not_send)]
async fn backdate_schedule(db: &Client, user_id: Uuid) {
    db.execute(
        "UPDATE zeroship.users \
         SET deletion_scheduled_for = NOW() - INTERVAL '1 minute' \
         WHERE id = $1",
        &[&user_id],
    )
    .await
    .expect("backdate schedule");
}

#[allow(clippy::future_not_send)]
async fn cleanup(db: &Client, ids: &[Uuid]) {
    for id in ids {
        let _ = db
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[id])
            .await;
    }
}

/// Rows in `zeroship.audit_events` of one type for one user.
#[allow(clippy::future_not_send)]
async fn audit_detail(db: &Client, user_id: Uuid, event_type: &str) -> Vec<serde_json::Value> {
    db.query(
        "SELECT detail FROM zeroship.audit_events \
         WHERE actor_user_id = $1 AND event_type = $2 ORDER BY id",
        &[&user_id, &event_type],
    )
    .await
    .expect("read audit events")
    .iter()
    .map(|row| row.get::<_, serde_json::Value>("detail"))
    .collect()
}

// ---------------------------------------------------------------------------
// The request, and the undo credential it mints
// ---------------------------------------------------------------------------

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn request_marks_deletion_schedules_and_mints_one_undo_token() {
    let mut db = pg().await;
    let tag = Uuid::new_v4().simple().to_string();
    let email = format!("acctdel-req-{tag}@zeroship.test");
    let user = users::create(&db, &email, "Req User", Some("phc")).await.unwrap();

    let req = users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .expect("request_deletion")
        .expect("user existed");

    // The request returns the contact details the confirm/undo email needs.
    assert_eq!(req.email, email);
    assert!(!req.cancel_token.is_empty(), "the undo token is minted here");

    // Re-read: deletion requested and scheduled, without changing an
    // independent administrative disable.
    let row = db
        .query_one(
            "SELECT disabled_at, deletion_requested_at, deletion_scheduled_for \
             FROM zeroship.users WHERE id = $1",
            &[&user.id],
        )
        .await
        .unwrap();
    let disabled: Option<chrono::DateTime<chrono::Utc>> = row.get("disabled_at");
    let requested: Option<chrono::DateTime<chrono::Utc>> = row.get("deletion_requested_at");
    let scheduled: Option<chrono::DateTime<chrono::Utc>> = row.get("deletion_scheduled_for");
    assert!(disabled.is_none(), "request must not set an administrative disable");
    assert!(requested.is_some(), "deletion_requested_at must be set");
    let scheduled = scheduled.expect("scheduled set");
    // Scheduled ~30 days out.
    let delta = scheduled - requested.unwrap();
    assert!(
        delta.num_days() >= 29 && delta.num_days() <= 31,
        "scheduled ~30 days after request, got {} days",
        delta.num_days()
    );

    // Exactly ONE live undo token, and it expires WITH the window rather than
    // on a TTL of its own. A token outliving the schedule would let someone
    // "cancel" an account the reaper already erased; one expiring early would
    // shorten the window the email promises.
    let tokens = db
        .query(
            "SELECT expires_at FROM zeroship.magic_links \
             WHERE user_id = $1 AND purpose = 'deletion_cancel' AND consumed_at IS NULL",
            &[&user.id],
        )
        .await
        .unwrap();
    assert_eq!(tokens.len(), 1, "one live undo token per pending request");
    assert_eq!(
        tokens[0].get::<_, chrono::DateTime<chrono::Utc>>("expires_at"),
        scheduled,
        "the undo window IS the grace window"
    );

    cleanup(&db, &[user.id]).await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn the_emailed_token_cancels_within_grace_and_only_once() {
    let mut db = pg().await;
    let tag = Uuid::new_v4().simple().to_string();
    let email = format!("acctdel-cancel-{tag}@zeroship.test");
    let user = users::create(&db, &email, "Cancel User", Some("phc")).await.unwrap();

    let req = users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    let cancelled = deletion_cancel::redeem(&db, &req.cancel_token)
        .await
        .expect("redeem")
        .expect("an in-flight request within grace must cancel");
    assert_eq!(cancelled.user_id, user.id);

    let row = db
        .query_one(
            "SELECT disabled_at, deletion_requested_at, deletion_scheduled_for \
             FROM zeroship.users WHERE id = $1",
            &[&user.id],
        )
        .await
        .unwrap();
    let disabled: Option<chrono::DateTime<chrono::Utc>> = row.get("disabled_at");
    let requested: Option<chrono::DateTime<chrono::Utc>> = row.get("deletion_requested_at");
    let scheduled: Option<chrono::DateTime<chrono::Utc>> = row.get("deletion_scheduled_for");
    assert!(disabled.is_none(), "cancel must leave the administrative state unchanged");
    assert!(requested.is_none() && scheduled.is_none(), "cancel clears the schedule");

    // Single use. The same token presented again is not a second cancel, and
    // (the control that makes this claim mean something) it is refused even
    // though a fresh deletion request is now in flight.
    users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    assert!(
        deletion_cancel::redeem(&db, &req.cancel_token)
            .await
            .expect("redeem")
            .is_none(),
        "a spent token must not cancel a later request"
    );
    let still_pending: bool = db
        .query_one(
            "SELECT deletion_requested_at IS NOT NULL FROM zeroship.users WHERE id = $1",
            &[&user.id],
        )
        .await
        .unwrap()
        .get(0);
    assert!(still_pending, "the second request is untouched by the spent token");

    cleanup(&db, &[user.id]).await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reissuing_a_request_supersedes_the_previous_undo_token() {
    let mut db = pg().await;
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(
        &db,
        &format!("acctdel-reissue-{tag}@zeroship.test"),
        "Reissue User",
        Some("phc"),
    )
    .await
    .unwrap();

    let first = users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    let second = users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(first.cancel_token, second.cancel_token);
    // The link in the OLDER message stops working the moment a newer one is
    // issued; otherwise two live undo credentials exist for one window.
    assert!(
        deletion_cancel::redeem(&db, &first.cancel_token)
            .await
            .expect("redeem")
            .is_none(),
        "the superseded token must not cancel"
    );
    assert!(
        deletion_cancel::redeem(&db, &second.cancel_token)
            .await
            .expect("redeem")
            .is_some(),
        "the current token must cancel"
    );

    cleanup(&db, &[user.id]).await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn a_token_past_the_grace_window_is_refused() {
    let mut db = pg().await;
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(
        &db,
        &format!("acctdel-expired-{tag}@zeroship.test"),
        "Expired Token",
        Some("phc"),
    )
    .await
    .unwrap();
    let req = users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    db.execute(
        "UPDATE zeroship.magic_links SET expires_at = NOW() - INTERVAL '1 minute' \
         WHERE user_id = $1 AND purpose = 'deletion_cancel'",
        &[&user.id],
    )
    .await
    .unwrap();

    assert!(
        deletion_cancel::redeem(&db, &req.cancel_token)
            .await
            .expect("redeem")
            .is_none(),
        "an expired undo token must not cancel"
    );
    let still_pending: bool = db
        .query_one(
            "SELECT deletion_requested_at IS NOT NULL FROM zeroship.users WHERE id = $1",
            &[&user.id],
        )
        .await
        .unwrap()
        .get(0);
    assert!(still_pending);

    cleanup(&db, &[user.id]).await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn cancellation_preserves_an_independent_administrative_disable() {
    let mut db = pg().await;
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(
        &db,
        &format!("acctdel-disabled-{tag}@zeroship.test"),
        "Disabled User",
        Some("phc"),
    )
    .await
    .unwrap();
    db.execute(
        "UPDATE zeroship.users SET disabled_at = NOW() - INTERVAL '1 day' WHERE id = $1",
        &[&user.id],
    )
    .await
    .unwrap();

    let req = users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    assert!(deletion_cancel::redeem(&db, &req.cancel_token)
        .await
        .unwrap()
        .is_some());

    let disabled: bool = db
        .query_one(
            "SELECT disabled_at IS NOT NULL FROM zeroship.users WHERE id = $1",
            &[&user.id],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        disabled,
        "cancelling deletion must not erase an administrative disable"
    );

    cleanup(&db, &[user.id]).await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn cancellation_does_not_restore_pre_deletion_app_credentials() {
    let mut db = pg().await;
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(
        &db,
        &format!("acctdel-recall-{tag}@zeroship.test"),
        "Recall User",
        Some("phc"),
    )
    .await
    .unwrap();
    let app_id = Uuid::new_v4();
    let client_id = format!("oac_acctdel_{tag}");
    // DERIVED through the production function, not invented. The deletion
    // cascade copies whatever subject it finds stored, so "seed X, assert
    // marker == X" holds for any X - including one no live token carries.
    let pairwise_sub = zeroship_core::auth::derive_pairwise(
        &zeroship_core::crypto::derive_key("account-deletion-test-salt"),
        &user.id.to_string(),
        &format!("https://{client_id}.zeroship.localhost"),
    );

    db.execute(
        "INSERT INTO zeroship.plans \
            (id, name, runtime_limits_json, assignable_by_creator) \
         VALUES ('free', 'Free', '{}'::jsonb, TRUE) \
         ON CONFLICT (id) DO NOTHING",
        &[],
    )
    .await
    .unwrap();
    // An app row needs a project, and a project needs an organization. Account
    // deletion is what this file is about, not authority, so the organization
    // is left member-less.
    let project_id = common::unowned_project(&db).await;
    db.execute(
        "INSERT INTO zeroship.apps (id, name, plan_id, project_id, organization_id) \
         SELECT $1, $2, 'free', p.id, p.organization_id \
           FROM zeroship.projects p WHERE p.id = $3",
        &[&app_id, &format!("acctdel-app-{tag}"), &project_id],
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, redirect_uris, scopes) \
         VALUES ($1, $2, $3, $4)",
        &[
            &client_id,
            &format!("Account deletion {tag}"),
            &vec![format!("https://acctdel-{tag}.test/callback")],
            &vec!["openid".to_string()],
        ],
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO zeroship.app_user_identities \
            (app_client_id, global_user_id, pairwise_sub) \
         VALUES ($1, $2, $3)",
        &[&client_id, &user.id, &pairwise_sub],
    )
    .await
    .unwrap();
    let anchor_id = Uuid::new_v4();
    db.execute(
        "INSERT INTO zeroship.app_session_anchors \
            (id, app_id, client_id, global_user_id, refresh_token_enc, \
             refresh_family_id, abs_expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, NOW() + INTERVAL '30 days')",
        &[
            &anchor_id,
            &app_id,
            &client_id,
            &user.id,
            &b"enc-refresh".to_vec(),
            &format!("rfam_{tag}"),
        ],
    )
    .await
    .unwrap();

    let req = users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    assert!(deletion_cancel::redeem(&db, &req.cancel_token)
        .await
        .unwrap()
        .is_some());

    // Read the marker's `sub` back rather than counting rows that match the
    // seed: a count cannot tell "no marker" from "a marker under some other
    // subject", and a marker keyed on anything but the subject the app's tokens
    // carry revokes nothing.
    let marker = db
        .query(
            "SELECT sub FROM zeroship.token_revocations WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .unwrap();
    assert_eq!(
        marker.len(),
        1,
        "deletion must durably revoke access tokens without refresh families"
    );
    let marker_sub: String = marker[0].get("sub");
    assert_eq!(
        marker_sub, pairwise_sub,
        "the deletion marker must be keyed on the per-app pairwise subject the \
         app's access tokens carry"
    );
    let platform_marker = db
        .query(
            "SELECT 1 FROM zeroship.token_revocations \
             WHERE client_id = 'zeroship-cli' AND sub = $1",
            &[&user.id.to_string()],
        )
        .await
        .unwrap();
    assert_eq!(
        platform_marker.len(),
        1,
        "deletion must durably revoke platform tokens after cancellation"
    );
    let anchor_revoked: bool = db
        .query_one(
            "SELECT revoked_at IS NOT NULL FROM zeroship.app_session_anchors \
             WHERE id = $1",
            &[&anchor_id],
        )
        .await
        .unwrap()
        .get(0);
    assert!(anchor_revoked, "deletion must durably revoke recovery anchors");

    cleanup(&db, &[user.id]).await;
    let _ = db.execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id]).await;
    let _ = db
        .execute(
            "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await;
}

// ---------------------------------------------------------------------------
// The route, over HTTP
// ---------------------------------------------------------------------------

/// `POST /me/delete/cancel` driven through the REAL ntex route table, because
/// the defect this closes was a route nothing could reach. Every store-level
/// test above would have passed just as happily while the HTTP surface stayed
/// dead: the three predicates that made it unreachable
/// (`credential_version`, `deletion_requested_at`, `idp_sessions.revoked_at`)
/// live in `sessions::validate`, which only a request through the router calls.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn the_cancel_route_is_reachable_over_http_and_restores_the_account() {
    let Some(dsn) = zeroship_core::config::test_database_url_opt() else {
        return;
    };
    let mut db = open(&dsn).await;
    let fixture = boot_cancel_server(&dsn).await;

    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(
        &db,
        &format!("acctdel-http-{tag}@zeroship.test"),
        "Http Cancel",
        Some("phc"),
    )
    .await
    .unwrap();
    let req = users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();

    // 1. The link in the email. It carries NO cookie - the request revoked
    //    every session - and it must still render the form and hand out a CSRF
    //    pair.
    let http = cyper::Client::new();
    let get = http
        .get(format!(
            "{}/me/delete/cancel?token={}",
            fixture.base, req.cancel_token
        ))
        .expect("build GET")
        .send()
        .await
        .expect("GET cancel");
    assert_eq!(get.status().as_u16(), 200, "the undo link must render");
    let csrf = common::read_set_cookie(&get, "__Host-zsidp_csrf").expect("csrf cookie");
    let body = get.text().await.expect("body");
    assert!(
        body.contains(&req.cancel_token),
        "the form must carry the token from GET to POST"
    );

    // 2. The form submit.
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("token", &req.cancel_token)
        .finish();
    let post = http
        .post(format!("{}/me/delete/cancel", fixture.base))
        .expect("build POST")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
        .expect("cookie")
        .body(form)
        .send()
        .await
        .expect("POST cancel");
    assert_eq!(post.status().as_u16(), 200, "the undo must be accepted");

    let pending: bool = db
        .query_one(
            "SELECT deletion_requested_at IS NOT NULL FROM zeroship.users WHERE id = $1",
            &[&user.id],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!pending, "the route, not just the store, cancels the deletion");

    cleanup(&db, &[user.id]).await;
    drop(fixture);
}

/// The control differing in one variable: same route, same absent cookie, a
/// token that was never issued. Without this, the test above would pass on a
/// handler that cancelled whatever it was pointed at.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn the_cancel_route_refuses_a_token_it_never_issued() {
    let Some(dsn) = zeroship_core::config::test_database_url_opt() else {
        return;
    };
    let mut db = open(&dsn).await;
    let fixture = boot_cancel_server(&dsn).await;

    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(
        &db,
        &format!("acctdel-httpbad-{tag}@zeroship.test"),
        "Http Bad Token",
        Some("phc"),
    )
    .await
    .unwrap();
    users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();

    let http = cyper::Client::new();
    let forged = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let get = http
        .get(format!("{}/me/delete/cancel?token={forged}", fixture.base))
        .expect("build GET")
        .send()
        .await
        .expect("GET cancel");
    let csrf = common::read_set_cookie(&get, "__Host-zsidp_csrf").expect("csrf cookie");
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("token", forged)
        .finish();
    let post = http
        .post(format!("{}/me/delete/cancel", fixture.base))
        .expect("build POST")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
        .expect("cookie")
        .body(form)
        .send()
        .await
        .expect("POST cancel");
    assert_eq!(post.status().as_u16(), 400, "a token nobody issued is refused");

    let pending: bool = db
        .query_one(
            "SELECT deletion_requested_at IS NOT NULL FROM zeroship.users WHERE id = $1",
            &[&user.id],
        )
        .await
        .unwrap()
        .get(0);
    assert!(pending, "the pending deletion survives a forged token");

    cleanup(&db, &[user.id]).await;
    drop(fixture);
}

struct CancelServer {
    base: String,
    _srv: ntex::web::test::TestServer,
}

/// Stand the REAL route table up. `server::configure` is what production calls;
/// pointing a bespoke `web::resource` at the handler would test the handler and
/// not the routing, and the routing is half of what was broken.
#[allow(clippy::future_not_send)]
async fn boot_cancel_server(dsn: &str) -> CancelServer {
    use std::sync::Arc;

    let cfg = Arc::new(common::test_auth_config(dsn));
    let client = open(dsn).await;
    let db = Arc::new(client);
    let refresh_pool = zeroship_auth::oidc::refresh::RefreshSessionPool::new(dsn.to_owned(), 2);
    let mailer: Arc<dyn zeroship_mailer::Mailer> = Arc::new(common::CapturingMailer::default());
    let srv = ntex::web::test::server(move || {
        let cfg = cfg.clone();
        let db = db.clone();
        let refresh_pool = refresh_pool.clone();
        let mailer = mailer.clone();
        async move {
            ntex::web::App::new()
                .state(cfg)
                .state(db)
                .state(refresh_pool)
                .state(mailer)
                .configure(zeroship_auth::server::configure(false, false))
        }
    })
    .await;
    let base = srv.url("").trim_end_matches('/').to_string();
    CancelServer { base, _srv: srv }
}

// ---------------------------------------------------------------------------
// The reaper
// ---------------------------------------------------------------------------

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reaper_erases_a_due_user_and_cascades() {
    let mut db = pg().await;
    let _reaper = common::lease_sweep(common::sweep_lock::ACCOUNT_REAPER).await;
    let (_mock, control) = clear_control().await;
    let tag = Uuid::new_v4().simple().to_string();
    let email = format!("acctdel-hard-{tag}@zeroship.test");
    let user = users::create(&db, &email, "Hard Delete", Some("phc")).await.unwrap();

    // A CASCADE dependent (federated identity) proves the cascade fires.
    db.execute(
        "INSERT INTO zeroship.federated_identities (user_id, provider, subject) \
         VALUES ($1, 'google', $2)",
        &[&user.id, &format!("sub-{tag}")],
    )
    .await
    .unwrap();

    users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, user.id).await;

    let report = account_reaper::tick(&mut db, &control).await.expect("reaper tick");
    // A FLOOR, not a figure: the lease keeps a peer run's due user out of this
    // window, but a user a crashed run left past its grace is durable in a
    // database nothing drops and this scan is fleet-wide. What happened to this
    // run's own user is the row checks below.
    assert!(report.erased >= 1, "a due user is erased: {report:?}");

    let remaining = db
        .query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .unwrap();
    assert!(remaining.is_empty(), "users row is gone");
    let idents = db
        .query(
            "SELECT 1 FROM zeroship.federated_identities WHERE user_id = $1",
            &[&user.id],
        )
        .await
        .unwrap();
    assert!(idents.is_empty(), "CASCADE dependents are gone");

    cleanup(&db, &[user.id]).await;
}

/// A MONEY RECORD OUTLIVES THE HUMAN IT NAMES. GDPR Art. 17(3)(b): the erasure
/// right yields to a legal obligation, and an invoice is one.
///
/// THIS ASSERTION WAS DELETED AND NOTHING REPLACED IT. It used to ride on the
/// anonymize branch, and when the organization work replaced the mechanism that
/// enforced retention - `invoices` stopped naming a user at all and now reaches
/// one only as `invoices -> organization_billing -> organizations` - the guard
/// went out with the code it was attached to. The PROPERTY survived the change;
/// only its witness did. A tree-wide search for Art. 17, `retained`, or
/// `legal-obligation` found nothing, so this was the one legally-motivated
/// property of the subsystem with no test standing behind it.
///
/// What makes it hold now is structural rather than intentional, which is
/// exactly why it needs a witness: the erasure deletes the `users` row, and the
/// only edge from an organization back to a human is
/// `organizations.personal_owner_id`, which is `ON DELETE SET NULL`. The
/// invoice hangs off the ORGANIZATION and never sees the delete. A future change
/// that made that edge `CASCADE` - the tempting spelling, since it reads as
/// "clean up after the person" - would destroy billing history and pass every
/// other test in this file.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn erasing_a_sole_owner_retains_the_organizations_invoice() {
    let mut db = pg().await;
    let _reaper = common::lease_sweep(common::sweep_lock::ACCOUNT_REAPER).await;
    let (_mock, control) = clear_control().await;
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(
        &db,
        &format!("acctdel-retain-{tag}@zeroship.test"),
        "Billed Human",
        None,
    )
    .await
    .unwrap();

    // A PERSONAL organization: the shape where the human and the billed party
    // are as close as this model allows, so it is the hardest case for
    // retention rather than the easiest.
    let organization_id = zeroship_core::typed_id::generate("org");
    db.execute(
        "INSERT INTO zeroship.organizations \
             (id, slug, name, billing_email, personal_owner_id, created_by) \
         VALUES ($1, $2, 'Retention Fixture', $3::citext, $4, $4)",
        &[
            &organization_id,
            &format!("retain-{tag}"),
            &format!("acctdel-retain-{tag}@zeroship.test"),
            &user.id,
        ],
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO zeroship.organization_billing (organization_id) VALUES ($1)",
        &[&organization_id],
    )
    .await
    .unwrap();
    let invoice_id = zeroship_core::typed_id::generate("inv");
    db.execute(
        "INSERT INTO zeroship.invoices \
             (id, organization_id, period, status, currency, \
              subtotal_cents, credit_cents, tax_cents, total_cents) \
         VALUES ($1, $2, DATE '2026-01-01', 'finalized', 'usd', 1000, 0, 0, 1000)",
        &[&invoice_id, &organization_id],
    )
    .await
    .unwrap();

    users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, user.id).await;
    account_reaper::tick(&mut db, &control).await.expect("reaper tick");

    let gone = db
        .query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .unwrap();
    assert!(gone.is_empty(), "the human is erased");

    let invoice = db
        .query(
            "SELECT total_cents FROM zeroship.invoices WHERE id = $1",
            &[&invoice_id],
        )
        .await
        .unwrap();
    assert_eq!(
        invoice.len(),
        1,
        "the invoice is RETAINED (GDPR Art. 17(3)(b)): erasing the human must not \
         destroy the money record that names their organization"
    );

    // The organization survives too, with its pointer cleared rather than the
    // row removed - the personal-to-shared conversion this model already
    // describes, reached here by erasure instead of by choice.
    let organization = db
        .query(
            "SELECT personal_owner_id FROM zeroship.organizations WHERE id = $1",
            &[&organization_id],
        )
        .await
        .unwrap();
    assert_eq!(organization.len(), 1, "the organization outlives its owner");
    let owner: Option<Uuid> = organization[0].get("personal_owner_id");
    assert!(owner.is_none(), "the pointer to the erased human is cleared");

    db.execute(
        "DELETE FROM zeroship.invoices WHERE id = $1",
        &[&invoice_id],
    )
    .await
    .ok();
    db.execute(
        "DELETE FROM zeroship.organization_billing WHERE organization_id = $1",
        &[&organization_id],
    )
    .await
    .ok();
    db.execute(
        "DELETE FROM zeroship.organizations WHERE id = $1",
        &[&organization_id],
    )
    .await
    .ok();
    cleanup(&db, &[user.id]).await;
}

/// The binding for `db/migrations-ts/20260907000000_user_erasure_edges.ts`.
///
/// Every one of these references BLOCKED a hard delete before that migration -
/// read out of `pg_constraint` as `confdeltype` in (`a`, `r`) - and only
/// `oauth_clients.created_by` was on the reaper's hand-maintained list. Three of
/// them (`identity_links`, `principal_grants`, `app_schema_applies`) were also
/// `NOT NULL`, so the `SET NULL` that list performed was not a spelling they
/// accepted: a creator who had signed in through the CLI or deployed a schema
/// could not be erased at all.
///
/// Seed one of each, then erase. What proves the migration is not that the
/// DELETE succeeds but WHICH way each dependent went: identity edges gone,
/// attribution edges surviving with a NULL.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reaper_erases_a_user_holding_every_previously_blocking_reference() {
    let mut db = pg().await;
    let _reaper = common::lease_sweep(common::sweep_lock::ACCOUNT_REAPER).await;
    let (_mock, control) = clear_control().await;
    let tag = Uuid::new_v4().simple().to_string();
    let victim = users::create(
        &db,
        &format!("acctdel-edges-{tag}@zeroship.test"),
        "Every Edge",
        None,
    )
    .await
    .unwrap();

    // identity edge -> must CASCADE.
    db.execute(
        "INSERT INTO zeroship.identity_links (principal_id, provider, provider_subject) \
         VALUES ($1, 'zeroship', $2)",
        &[&victim.id, &format!("edge-{tag}")],
    )
    .await
    .unwrap();
    // identity edge -> must CASCADE.
    db.execute(
        "INSERT INTO zeroship.principal_grants (principal_id, grant_name) VALUES ($1, 'apps:read')",
        &[&victim.id],
    )
    .await
    .unwrap();
    // identity edge -> must CASCADE. A device grant left behind with a NULL
    // principal and `status = 'approved'` would be a grant authorised by nobody.
    let device_code_hash = format!("dch-{tag}");
    db.execute(
        "INSERT INTO zeroship.device_grants \
            (device_code_hash, user_code, status, principal_id, provider, expires_at) \
         VALUES ($1, $2, 'approved', $3, 'zeroship', NOW() + INTERVAL '10 minutes')",
        &[&device_code_hash, &format!("uc-{tag}"), &victim.id],
    )
    .await
    .unwrap();
    // attribution edge -> must SET NULL, and the row must survive.
    let client_id = format!("acctdel-edges-client-{tag}");
    db.execute(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, redirect_uris, scopes, created_by) \
         VALUES ($1, 'Edge Probe', ARRAY['https://probe.zeroship.test/cb'], \
                 ARRAY['apps:read'], $2)",
        &[&client_id, &victim.id],
    )
    .await
    .unwrap();
    // attribution edge -> must SET NULL. `submitted_by` was NOT NULL and
    // RESTRICT; the schema apply record belongs to the app, not the human.
    db.execute(
        "INSERT INTO zeroship.plans \
            (id, name, runtime_limits_json, assignable_by_creator) \
         VALUES ('free', 'Free', '{}'::jsonb, TRUE) ON CONFLICT (id) DO NOTHING",
        &[],
    )
    .await
    .unwrap();
    let app_id = Uuid::new_v4();
    let project_id = common::unowned_project(&db).await;
    db.execute(
        "INSERT INTO zeroship.apps (id, name, plan_id, project_id, organization_id) \
         SELECT $1, $2, 'free', p.id, p.organization_id \
           FROM zeroship.projects p WHERE p.id = $3",
        &[&app_id, &format!("acctdel-edges-app-{tag}"), &project_id],
    )
    .await
    .unwrap();
    let migration_id = Uuid::new_v4();
    db.execute(
        "INSERT INTO zeroship.app_schema_applies \
            (app_id, migration_id, status, request_body, effective_profile, \
             ceiling_id, ceiling_version, descriptor_sha256, submitted_by) \
         VALUES ($1, $2, 'applied', '{}'::jsonb, '{}'::jsonb, 'managed', 1, $3, $4)",
        &[&app_id, &migration_id, &format!("sha-{tag}"), &victim.id],
    )
    .await
    .unwrap();

    users::request_deletion(&mut db, victim.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, victim.id).await;

    let report = account_reaper::tick(&mut db, &control).await.expect("reaper tick");
    assert!(report.erased >= 1, "the user is erased: {report:?}");
    assert!(
        db.query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&victim.id])
            .await
            .unwrap()
            .is_empty(),
        "no inbound reference blocks the delete"
    );

    for (table, column) in [
        ("identity_links", "principal_id"),
        ("principal_grants", "principal_id"),
        ("device_grants", "principal_id"),
    ] {
        let sql = format!("SELECT 1 FROM zeroship.{table} WHERE {column} = $1");
        assert!(
            db.query(&sql, &[&victim.id]).await.unwrap().is_empty(),
            "{table}.{column} is an identity edge and must CASCADE"
        );
    }
    let created_by: Option<Uuid> = db
        .query_one(
            "SELECT created_by FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .expect("the oauth client survives its creator")
        .get("created_by");
    assert!(created_by.is_none(), "oauth_clients.created_by is SET NULL");
    let submitted_by: Option<Uuid> = db
        .query_one(
            "SELECT submitted_by FROM zeroship.app_schema_applies \
             WHERE app_id = $1 AND migration_id = $2",
            &[&app_id, &migration_id],
        )
        .await
        .expect("the schema apply record survives its submitter")
        .get("submitted_by");
    assert!(submitted_by.is_none(), "app_schema_applies.submitted_by is SET NULL");

    let _ = db
        .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id])
        .await;
    let _ = db
        .execute(
            "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await;
    cleanup(&db, &[victim.id]).await;
}

/// The same erasure, executed by the ROLE the auth service actually runs as.
///
/// Every other test in this file connects as the superuser, and that is how a
/// permission-denied predicate survived in `erase_one_tx` for as long as it did.
/// The cascades this depends on run with the CONSTRAINT OWNER's privileges, not
/// the caller's, which is exactly why declaring the edges works where the
/// reaper's own `UPDATE ... SET col = NULL` on a control-owned table would not.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reaper_erases_as_the_real_auth_role() {
    let Some(dsn) = zeroship_core::config::test_database_url_opt() else {
        return;
    };
    let db = open(&dsn).await;
    let _reaper = common::lease_sweep(common::sweep_lock::ACCOUNT_REAPER).await;
    let (_mock, control) = clear_control().await;
    let mut as_auth = open(&as_auth_role(&dsn)).await;

    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(
        &db,
        &format!("acctdel-role-{tag}@zeroship.test"),
        "Real Role",
        None,
    )
    .await
    .unwrap();
    // One edge into a table `zeroship_auth` cannot write directly
    // (`identity_links`: no grant at all). If the erasure needed the caller's
    // privileges rather than the constraint owner's, this is where it fails.
    db.execute(
        "INSERT INTO zeroship.identity_links (principal_id, provider, provider_subject) \
         VALUES ($1, 'zeroship', $2)",
        &[&user.id, &format!("role-{tag}")],
    )
    .await
    .unwrap();

    users::request_deletion(&mut as_auth, user.id, account_reaper::GRACE_DAYS)
        .await
        .expect("the real role can open a deletion window")
        .expect("user exists");
    backdate_schedule(&db, user.id).await;

    let report = account_reaper::tick(&mut as_auth, &control)
        .await
        .expect("the real role can run a reaper tick");
    assert!(report.erased >= 1, "erased under the real role: {report:?}");
    // `report.failed` is a FLEET-WIDE count and this run owns one user in a
    // shared database, so the per-user claim is the pair below: the row is
    // gone, and nothing recorded a failure against it. A `failed == 0`
    // assertion here would be reporting on rows a crashed peer left behind.
    assert!(
        db.query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user.id])
            .await
            .unwrap()
            .is_empty(),
        "the users row is gone"
    );
    assert!(
        audit_detail(&db, user.id, "account_erasure_failed").await.is_empty(),
        "the real role hit no permission or constraint failure"
    );

    cleanup(&db, &[user.id]).await;
}

/// A blocker that appeared DURING the grace window must stop the delete, and
/// the refusal must be durable and name the user. Logging and continuing is
/// what this replaces.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reaper_refuses_and_records_when_the_preflight_names_a_blocker() {
    let mut db = pg().await;
    let _reaper = common::lease_sweep(common::sweep_lock::ACCOUNT_REAPER).await;
    let mock = MockControl::start(Answer::Clear).await;
    let control = ControlAccess {
        control_url: mock.base.clone(),
        keyring: mock.keyring(),
    };
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(
        &db,
        &format!("acctdel-blocked-{tag}@zeroship.test"),
        "Blocked",
        None,
    )
    .await
    .unwrap();
    users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, user.id).await;

    // The window opened clear; by the time the reaper runs, this human is the
    // last owner of something.
    mock.set(Answer::SoleOwnerOf {
        slug: format!("late-{tag}"),
    });
    let report = account_reaper::tick(&mut db, &control).await.expect("tick");
    assert!(report.failed >= 1, "the blocked user counts as failed: {report:?}");

    assert!(
        !db.query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user.id])
            .await
            .unwrap()
            .is_empty(),
        "a re-blocked user is left pending, not half-erased"
    );
    let details = audit_detail(&db, user.id, "account_erasure_failed").await;
    assert_eq!(details.len(), 1, "one durable record, on this user");
    assert_eq!(details[0]["stage"], "preflight");
    assert!(
        details[0]["reason"]
            .as_str()
            .expect("reason")
            .contains(&format!("late-{tag}")),
        "the record names the organization: {}",
        details[0]
    );
    assert!(
        mock.asked().contains(&user.id.to_string()),
        "the reaper really asked about this principal"
    );

    // Teardown: the audit trail is append-only to this role, so the user row
    // goes and the audit row stays.
    cleanup(&db, &[user.id]).await;
}

/// The MONEY rule, at the reaper.
///
/// This is the third enforcement point of the rule whose SQL is bound in
/// `crates/zeroship-control/tests/deletion_owes_test.rs`. It is a separate
/// point rather than a repeat of the ownership one: the blocker arrives with an
/// EMPTY `blockers` list, because the organization is dissolved and the
/// ownership rule deliberately says nothing about closed organizations. A
/// reaper that read only `blockers` would erase this human and walk away from
/// the invoice.
///
/// The refusal must also be selectable as a MONEY refusal - `stage = billing` -
/// so "whose erasure is money holding up" is one query rather than a grep of
/// reason strings. The paired control is the clear answer that erases the same
/// user under the same fixture: one variable, the answer.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reaper_refuses_and_records_billing_when_the_organization_still_owes() {
    let mut db = pg().await;
    let _reaper = common::lease_sweep(common::sweep_lock::ACCOUNT_REAPER).await;
    let mock = MockControl::start(Answer::Clear).await;
    let control = ControlAccess {
        control_url: mock.base.clone(),
        keyring: mock.keyring(),
    };
    let tag = Uuid::new_v4().simple().to_string();
    let debtor = users::create(
        &db,
        &format!("acctdel-owes-{tag}@zeroship.test"),
        "Owes",
        None,
    )
    .await
    .unwrap();
    users::request_deletion(&mut db, debtor.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, debtor.id).await;

    // The window opened clear. The billing sweep then finalized last month's
    // invoice on an organization this human had already closed - which needs
    // nobody to act, and is why the request-time check cannot stand in here.
    mock.set(Answer::OwesBilling {
        slug: format!("owing-{tag}"),
        owed_cents: 4_200,
    });
    let report = account_reaper::tick(&mut db, &control).await.expect("tick");
    assert!(report.failed >= 1, "{report:?}");
    assert_eq!(report.erased, 0, "nothing may be erased on a debt: {report:?}");

    assert!(
        !db.query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&debtor.id])
            .await
            .unwrap()
            .is_empty(),
        "a human who owes is left pending, not half-erased"
    );
    let details = audit_detail(&db, debtor.id, "account_erasure_failed").await;
    assert_eq!(details.len(), 1, "one durable record, on this user");
    assert_eq!(
        details[0]["stage"], "billing",
        "a money refusal must be selectable as one: {}",
        details[0]
    );
    let reason = details[0]["reason"].as_str().expect("reason");
    assert!(
        reason.contains(&format!("owing-{tag}")) && reason.contains("4200"),
        "the record names the organization and what it owes: {reason}"
    );
    assert!(
        mock.asked().contains(&debtor.id.to_string()),
        "the reaper really asked about this principal"
    );

    // The control: same fixture, same user shape, and the answer is the only
    // thing that moved. Without it a reaper that refused everything would pass
    // the arm above.
    let settled = users::create(
        &db,
        &format!("acctdel-settled-{tag}@zeroship.test"),
        "Settled",
        None,
    )
    .await
    .unwrap();
    users::request_deletion(&mut db, settled.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, settled.id).await;
    mock.set(Answer::Clear);
    let report = account_reaper::tick(&mut db, &control).await.expect("tick");
    assert!(report.erased >= 1, "a settled human is erased: {report:?}");
    assert!(
        db.query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&settled.id])
            .await
            .unwrap()
            .is_empty(),
        "the settled user's row is gone"
    );

    cleanup(&db, &[debtor.id, settled.id]).await;
}

/// An unanswerable preflight is a refusal too. The failure mode being ruled out
/// is the one where "control is down" and "control said yes" are the same
/// outcome.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reaper_refuses_when_the_preflight_cannot_be_answered() {
    let mut db = pg().await;
    let _reaper = common::lease_sweep(common::sweep_lock::ACCOUNT_REAPER).await;
    let mock = MockControl::start(Answer::Unavailable).await;
    let control = ControlAccess {
        control_url: mock.base.clone(),
        keyring: mock.keyring(),
    };
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(
        &db,
        &format!("acctdel-unavail-{tag}@zeroship.test"),
        "Unavailable",
        None,
    )
    .await
    .unwrap();
    users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, user.id).await;

    let report = account_reaper::tick(&mut db, &control).await.expect("tick");
    assert!(report.failed >= 1, "{report:?}");
    assert!(
        !db.query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user.id])
            .await
            .unwrap()
            .is_empty(),
        "nothing is erased on an unanswerable preflight"
    );
    let details = audit_detail(&db, user.id, "account_erasure_failed").await;
    assert_eq!(details.len(), 1);
    assert_eq!(details[0]["stage"], "preflight");

    cleanup(&db, &[user.id]).await;
}

/// A credential the control plane does not trust is the deployment-fault arm.
/// It must refuse rather than proceed, and it must NOT be distinguishable from
/// a clear answer only by a log line.
///
/// The keyring here is a well-formed `svc/auth` identity under a key this mock
/// never trusted - the one variable that differs from the clear-answer fixture.
/// A missing credential cannot be expressed any more: `ServiceKeyring::load`
/// refuses the boot, so the arm that survives is a REJECTED one, and it is the
/// stronger arm because the round trip really happens.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reaper_refuses_when_the_control_plane_rejects_its_credential() {
    let mut db = pg().await;
    let _reaper = common::lease_sweep(common::sweep_lock::ACCOUNT_REAPER).await;
    let mock = MockControl::start(Answer::Clear).await;
    let control = ControlAccess {
        control_url: mock.base.clone(),
        keyring: common::mock_control::untrusted_auth_keyring(),
    };
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(
        &db,
        &format!("acctdel-nokey-{tag}@zeroship.test"),
        "No Key",
        None,
    )
    .await
    .unwrap();
    users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, user.id).await;

    let report = account_reaper::tick(&mut db, &control).await.expect("tick");
    assert!(report.failed >= 1, "{report:?}");
    assert!(
        !db.query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user.id])
            .await
            .unwrap()
            .is_empty(),
        "an unverifiable erasure does not happen"
    );
    assert!(
        mock.asked().is_empty(),
        "a refused credential must not reach the answer: the 401 comes before \
         the handler records the principal"
    );

    cleanup(&db, &[user.id]).await;
}

/// A `23503` from the DELETE has to reach a durable per-user record naming the
/// constraint. Proved by introducing a blocking reference the migration does
/// not know about - which is exactly the future defect the loud arm exists for.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn a_new_blocking_reference_is_recorded_with_its_constraint() {
    let mut db = pg().await;
    let _reaper = common::lease_sweep(common::sweep_lock::ACCOUNT_REAPER).await;
    let (_mock, control) = clear_control().await;
    let tag = Uuid::new_v4().simple().to_string();
    let probe = format!("erasure_probe_{tag}");
    db.execute(
        &format!(
            "CREATE TABLE zeroship.{probe} ( \
                 user_id uuid NOT NULL REFERENCES zeroship.users(id) ON DELETE RESTRICT)"
        ),
        &[],
    )
    .await
    .expect("create probe table");

    let user = users::create(
        &db,
        &format!("acctdel-probe-{tag}@zeroship.test"),
        "Probe",
        None,
    )
    .await
    .unwrap();
    db.execute(
        &format!("INSERT INTO zeroship.{probe} (user_id) VALUES ($1)"),
        &[&user.id],
    )
    .await
    .unwrap();
    users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, user.id).await;

    let report = account_reaper::tick(&mut db, &control).await.expect("tick");
    let details = audit_detail(&db, user.id, "account_erasure_failed").await;

    // TEARDOWN BEFORE THE ASSERTIONS, deliberately. This table is a users
    // reference with `ON DELETE RESTRICT`, which is exactly what
    // `tests/user_erasure_reachability_gate.sh` arm 1 refuses - so a probe left
    // behind by a failing assertion turns the NEXT gate run red against a defect
    // that is this test's own fixture. Every value the assertions need is
    // already read.
    db.execute(&format!("DROP TABLE IF EXISTS zeroship.{probe}"), &[])
        .await
        .expect("drop probe table");
    cleanup(&db, &[user.id]).await;

    assert!(report.failed >= 1, "the blocked delete is a failure: {report:?}");
    assert_eq!(details.len(), 1, "one record, on the user it happened to");
    assert_eq!(
        details[0]["stage"], "constraint",
        "a 23503 is classified as a constraint refusal, not a generic db error"
    );
    let constraint = details[0]["constraint"].as_str().expect("constraint recorded");
    assert!(
        constraint.contains(&probe),
        "the record names the reference that blocked: {constraint}"
    );
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reaper_skips_cancelled_request() {
    let mut db = pg().await;
    let _reaper = common::lease_sweep(common::sweep_lock::ACCOUNT_REAPER).await;
    let (_mock, control) = clear_control().await;
    let tag = Uuid::new_v4().simple().to_string();
    let email = format!("acctdel-skip-{tag}@zeroship.test");
    let user = users::create(&db, &email, "Skip User", Some("phc")).await.unwrap();

    let req = users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, user.id).await;
    // Cancel clears the schedule - even though the (now-cleared) date was in
    // the past, the reaper must not touch a cancelled request.
    deletion_cancel::redeem(&db, &req.cancel_token).await.unwrap();

    // No count assertion here, and that is the point rather than an omission:
    // "the reaper touched nobody" is a claim over the whole database, and this
    // run owns exactly one user in it. The row check below makes the same claim
    // about the only row that is this run's to speak for.
    account_reaper::tick(&mut db, &control).await.expect("reaper tick");

    let remaining = db
        .query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .unwrap();
    assert_eq!(remaining.len(), 1, "user survives a cancelled request");

    cleanup(&db, &[user.id]).await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reaper_ignores_a_schedule_without_a_deletion_request() {
    let mut db = pg().await;
    let _reaper = common::lease_sweep(common::sweep_lock::ACCOUNT_REAPER).await;
    let (_mock, control) = clear_control().await;
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(
        &db,
        &format!("acctdel-schedule-only-{tag}@zeroship.test"),
        "Schedule Only",
        Some("phc"),
    )
    .await
    .unwrap();
    db.execute(
        "UPDATE zeroship.users \
         SET deletion_scheduled_for = NOW() - INTERVAL '1 minute' \
         WHERE id = $1",
        &[&user.id],
    )
    .await
    .unwrap();

    account_reaper::tick(&mut db, &control).await.unwrap();

    let still_exists = db
        .query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .unwrap();
    assert_eq!(
        still_exists.len(),
        1,
        "a schedule alone is a deny state, not erasure authorization"
    );
    cleanup(&db, &[user.id]).await;
}

// ---------------------------------------------------------------------------
// The ownership rule under concurrency
// ---------------------------------------------------------------------------

/// Wait until one backend is BLOCKED on the reaper's organization row lock.
///
/// The barrier is what makes the interleaving a fact rather than a hope: the
/// departure is committed while the erasure is provably parked on the lock, so
/// "the co-owner left after the preflight answered clear and before the erasure
/// committed" is the ordering the assertions rule on, not a timing that
/// happened to come out that way once.
///
/// It is BOUNDED and returns rather than hangs. A reaper that takes no lock
/// never blocks, and a barrier that waited forever for that would turn the
/// defect this test exists for into a hung suite instead of a red assertion.
#[allow(clippy::future_not_send)]
async fn wait_until_blocked_on_the_organization_lock(observer: &Client) -> bool {
    for _ in 0..400 {
        let waiting = observer
            .query(
                "SELECT 1 FROM pg_stat_activity \
                  WHERE datname = current_database() \
                    AND wait_event_type = 'Lock' \
                    AND query LIKE '%zeroship.organizations%FOR UPDATE%'",
                &[],
            )
            .await
            .expect("read pg_stat_activity");
        if !waiting.is_empty() {
            return true;
        }
        compio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    false
}

/// TRIGGER A, and the reason the fence has to be inside the transaction.
///
/// The preflight is an HTTP round trip, so it answers BEFORE the erasure
/// transaction opens. A co-owner who departs in that window is doing something
/// legitimate - two owners were seated when their `leave` ran - and the reaper
/// then deletes the other one. Both statements commit and the organization has
/// no owner, which `zeroship_control::organizations`'s module header names as
/// the state no route can repair.
///
/// The interleaving is forced, not raced: the departure takes the organization
/// row lock and holds it, the reaper's tick parks on that same lock, and only
/// then does the departure commit. Both orders of the same pair are safe once
/// the lock is shared - this is the one where the reaper is second.
///
/// It runs the tick as the REAL `zeroship_auth` role. The fence reads
/// control-owned tables, and a re-check that raises `42501` under the role the
/// service actually connects as would be no fence at all; a suite that only
/// ever connects as `postgres` cannot tell the two apart.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn a_departure_after_the_preflight_cannot_leave_the_organization_ownerless() {
    let dsn = crate::common::test_database_url();
    let mut db = open(&dsn).await;
    let _reaper = common::lease_sweep(common::sweep_lock::ACCOUNT_REAPER).await;
    let as_auth = open(&as_auth_role(&dsn)).await;
    let mut departing = open(&dsn).await;
    let observer = open(&dsn).await;
    let (mock, control) = clear_control().await;

    let tag = Uuid::new_v4().simple().to_string();
    let organization_id = format!("org_{}", &tag[..22]);
    let slug = format!("acctdel-{}", &tag[..12]);
    let victim = users::create(
        &db,
        &format!("acctdel-race-victim-{tag}@zeroship.test"),
        "Victim",
        None,
    )
    .await
    .unwrap();
    let co_owner = users::create(
        &db,
        &format!("acctdel-race-peer-{tag}@zeroship.test"),
        "Co Owner",
        None,
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO zeroship.organizations \
             (id, slug, name, billing_email, created_by) \
         VALUES ($1, $2::text::citext, $3, $4::text::citext, $5)",
        &[
            &organization_id,
            &slug,
            &"Shared",
            &format!("billing-{tag}@zeroship.test"),
            &victim.id,
        ],
    )
    .await
    .expect("seat the organization");
    db.execute(
        "INSERT INTO zeroship.organization_members (organization_id, user_id, role) \
         VALUES ($1, $2, 'owner'), ($1, $3, 'owner')",
        &[&organization_id, &victim.id, &co_owner.id],
    )
    .await
    .expect("seat two owners");

    users::request_deletion(&mut db, victim.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, victim.id).await;

    // The departure, in the shape `leave_organization` takes it: the
    // organization row lock FIRST, then the delete under its own owners-remain
    // predicate. Uncommitted, so the erasure has to meet it.
    let departure = departing
        .transaction()
        .await
        .expect("begin the co-owner's departure");
    departure
        .query(
            "SELECT id FROM zeroship.organizations WHERE id = $1 FOR UPDATE",
            &[&organization_id],
        )
        .await
        .expect("the departure takes the organization row lock");
    let left = departure
        .execute(
            "DELETE FROM zeroship.organization_members m \
              WHERE m.organization_id = $1 AND m.user_id = $2 \
                AND (m.role <> 'owner' \
                     OR (SELECT count(*) FROM zeroship.organization_members owners \
                          WHERE owners.organization_id = $1 AND owners.role = 'owner') > 1)",
            &[&organization_id, &co_owner.id],
        )
        .await
        .expect("the departure runs");
    assert_eq!(
        left, 1,
        "the departure is legitimate: two owners are seated when it runs"
    );

    let control_for_tick = control.clone();
    let erasure = compio::runtime::spawn(async move {
        let mut conn = as_auth;
        account_reaper::tick(&mut conn, &control_for_tick).await
    });

    let parked = wait_until_blocked_on_the_organization_lock(&observer).await;
    departure.commit().await.expect("the co-owner has left");
    let report = erasure.await.expect("join erasure").expect("tick");

    let owners = db
        .query(
            "SELECT user_id FROM zeroship.organization_members \
              WHERE organization_id = $1 AND role = 'owner'",
            &[&organization_id],
        )
        .await
        .expect("count owners");
    let victim_row = db
        .query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&victim.id])
        .await
        .expect("read the victim");
    let details = audit_detail(&db, victim.id, "account_erasure_failed").await;
    let asked = mock.asked();

    // Teardown before the assertions: the organization outlives a failing
    // assertion otherwise, and its slug is unique among live organizations.
    db.execute(
        "DELETE FROM zeroship.organization_members WHERE organization_id = $1",
        &[&organization_id],
    )
    .await
    .expect("unseat");
    db.execute(
        "DELETE FROM zeroship.organizations WHERE id = $1",
        &[&organization_id],
    )
    .await
    .expect("close the organization");
    cleanup(&db, &[victim.id, co_owner.id]).await;

    assert!(
        asked.contains(&victim.id.to_string()),
        "the preflight really answered, and answered clear, before the erasure"
    );
    assert!(
        !owners.is_empty(),
        "the organization was left with NO owner - the state no route repairs"
    );
    assert_eq!(
        victim_row.len(),
        1,
        "a refused erasure leaves the account pending, not half-erased"
    );
    assert!(report.failed >= 1, "the refusal counts as a failure: {report:?}");
    assert_eq!(details.len(), 1, "one durable record, on this user");
    assert_eq!(
        details[0]["stage"], "ownership",
        "the in-transaction fence is selectable apart from a preflight that \
         answered no: {}",
        details[0]
    );
    // Last, because it rules on the MECHANISM rather than the outcome. The
    // assertions above can all hold on a run where the erasure simply finished
    // first; this one says the ordering was imposed by the lock, so a green is
    // evidence about the fence and not about scheduling.
    assert!(
        parked,
        "the erasure never blocked on the organization row lock, so the fence \
         is not inside the transaction"
    );
}

/// TRIGGER C, and the reason the lock is taken over SEATS rather than owner
/// seats.
///
/// Triggers A and B both need this human to already hold an owner seat, so a
/// fence that locked exactly the organizations they own closed both. Ownership
/// is also reachable by PROMOTION: `transfer_ownership` raises a sitting member
/// with `UPDATE organization_members SET role`, and a referencing-side RI
/// trigger fires only when the key columns change - so that UPDATE takes no
/// lock on `zeroship.users`, and holding the victim's row `FOR UPDATE` does not
/// serialize it.
///
/// The victim here is a DEVELOPER when the preflight answers, which is why it
/// answers clear. A fence locking only owner seats locks nothing at all for
/// this organization, the promotion commits underneath the erasure, and the
/// cascade takes the freshly-granted owner seat with it.
///
/// The interleaving is forced the same way as trigger A: the transfer takes the
/// organization row lock and holds it, the tick parks on that lock, and only
/// then does the transfer commit. So a green says the re-check saw a promotion
/// that landed after the lock was requested - which is the whole claim.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn a_promotion_after_the_preflight_cannot_leave_the_organization_ownerless() {
    let dsn = crate::common::test_database_url();
    let mut db = open(&dsn).await;
    let _reaper = common::lease_sweep(common::sweep_lock::ACCOUNT_REAPER).await;
    let as_auth = open(&as_auth_role(&dsn)).await;
    let mut transferring = open(&dsn).await;
    let observer = open(&dsn).await;
    let (mock, control) = clear_control().await;

    let tag = Uuid::new_v4().simple().to_string();
    let organization_id = format!("org_{}", &tag[..22]);
    let slug = format!("acctdel-{}", &tag[..12]);
    let victim = users::create(
        &db,
        &format!("acctdel-promo-victim-{tag}@zeroship.test"),
        "Victim",
        None,
    )
    .await
    .unwrap();
    let sitting_owner = users::create(
        &db,
        &format!("acctdel-promo-owner-{tag}@zeroship.test"),
        "Sitting Owner",
        None,
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO zeroship.organizations \
             (id, slug, name, billing_email, created_by) \
         VALUES ($1, $2::text::citext, $3, $4::text::citext, $5)",
        &[
            &organization_id,
            &slug,
            &"Handover",
            &format!("billing-{tag}@zeroship.test"),
            &sitting_owner.id,
        ],
    )
    .await
    .expect("seat the organization");
    // The victim is a DEVELOPER. This is what makes the preflight answer clear
    // and what a role-filtered lock would decline to lock.
    db.execute(
        "INSERT INTO zeroship.organization_members (organization_id, user_id, role) \
         VALUES ($1, $2, 'developer'), ($1, $3, 'owner')",
        &[&organization_id, &victim.id, &sitting_owner.id],
    )
    .await
    .expect("seat one owner and one developer");

    users::request_deletion(&mut db, victim.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, victim.id).await;

    // The handover, in the shape `transfer_ownership` takes it: the
    // organization row lock FIRST, then promote the incoming owner and step the
    // outgoing one down. Uncommitted, so the erasure has to meet it.
    let transfer = transferring
        .transaction()
        .await
        .expect("begin the ownership transfer");
    transfer
        .query(
            "SELECT id FROM zeroship.organizations WHERE id = $1 FOR UPDATE",
            &[&organization_id],
        )
        .await
        .expect("the transfer takes the organization row lock");
    let promoted = transfer
        .execute(
            "UPDATE zeroship.organization_members \
                SET role = 'owner', changed_at = NOW(), changed_by = $3 \
              WHERE organization_id = $1 AND user_id = $2",
            &[&organization_id, &victim.id, &sitting_owner.id],
        )
        .await
        .expect("promote the incoming owner");
    assert_eq!(promoted, 1, "the promotion is legitimate when it runs");
    transfer
        .execute(
            "UPDATE zeroship.organization_members \
                SET role = 'admin', changed_at = NOW(), changed_by = $3 \
              WHERE organization_id = $1 AND user_id = $2",
            &[&organization_id, &sitting_owner.id, &sitting_owner.id],
        )
        .await
        .expect("step the outgoing owner down");

    let control_for_tick = control.clone();
    let erasure = compio::runtime::spawn(async move {
        let mut conn = as_auth;
        account_reaper::tick(&mut conn, &control_for_tick).await
    });

    let parked = wait_until_blocked_on_the_organization_lock(&observer).await;
    transfer.commit().await.expect("the handover completed");
    let report = erasure.await.expect("join erasure").expect("tick");

    let owners = db
        .query(
            "SELECT user_id FROM zeroship.organization_members \
              WHERE organization_id = $1 AND role = 'owner'",
            &[&organization_id],
        )
        .await
        .expect("count owners");
    let victim_row = db
        .query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&victim.id])
        .await
        .expect("read the victim");
    let details = audit_detail(&db, victim.id, "account_erasure_failed").await;
    let asked = mock.asked();

    db.execute(
        "DELETE FROM zeroship.organization_members WHERE organization_id = $1",
        &[&organization_id],
    )
    .await
    .expect("unseat");
    db.execute(
        "DELETE FROM zeroship.organizations WHERE id = $1",
        &[&organization_id],
    )
    .await
    .expect("close the organization");
    cleanup(&db, &[victim.id, sitting_owner.id]).await;

    assert!(
        asked.contains(&victim.id.to_string()),
        "the preflight really answered, and answered clear, before the erasure"
    );
    assert!(
        !owners.is_empty(),
        "the organization was left with NO owner - a promotion the fence never \
         locked against"
    );
    assert_eq!(
        victim_row.len(),
        1,
        "a refused erasure leaves the account pending, not half-erased"
    );
    assert!(
        report.failed >= 1,
        "the refusal counts as a failure: {report:?}"
    );
    assert_eq!(details.len(), 1, "one durable record, on this user");
    assert_eq!(
        details[0]["stage"], "ownership",
        "the in-transaction fence is selectable apart from a preflight that \
         answered no: {}",
        details[0]
    );
    assert!(
        parked,
        "the erasure never blocked on the organization row lock, so the lock \
         did not cover a seat the victim held"
    );
}
