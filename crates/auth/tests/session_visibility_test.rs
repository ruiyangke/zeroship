//! Live-PG roundtrip for ISS-10 — active-session visibility + single-session
//! revoke (`store::sessions::list_by_user` + `revoke_one_for_user`).
//!
//! Skipped unless `AUTH_DB_URL` is set. The repo convention is to run the
//! auth DB suite with `--test-threads=1` (one shared DB). Each test scopes
//! itself with random emails so a serial run leaves no residue; an explicit
//! cleanup removes every row each test inserted.
//!
//! The security crux this file pins: a user can list/revoke ONLY their own
//! sessions. `revoke_one_for_user` filters on `user_id = <caller>`, so
//! passing another user's `session_id` revokes nothing (no IDOR).

#![allow(clippy::future_not_send)]

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;

use zeroship_auth::store::sessions::{self, SessionKind};
use zeroship_auth::store::users;

async fn pg() -> Option<Client> {
    let dsn = std::env::var("AUTH_DB_URL").ok()?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("session_visibility_test pg connection error: {e}");
        }
    })
    .detach();
    Some(client)
}

/// Seed a real `zeroship.apps` row (gateway_sessions.app_id FKs into it) and
/// return its id.
async fn seed_app(client: &Client) -> Uuid {
    let app_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO zeroship.apps (id, name, api_key) VALUES ($1, $2, $3)",
            &[
                &app_id,
                &format!("iss10-app-{}", app_id.simple()),
                &format!("k-{}", app_id.simple()),
            ],
        )
        .await
        .expect("seed app");
    app_id
}

/// Seed one live gateway session for `user_id`@`app_id`, returning its id.
async fn seed_gateway_session(client: &Client, user_id: Uuid, app_id: Uuid, email: &str) -> Uuid {
    let rows = client
        .query(
            "INSERT INTO zeroship.gateway_sessions \
                (user_id, app_id, email, name, email_verified, idle_expires_at, abs_expires_at) \
             VALUES ($1, $2, $3::citext, $4, true, \
                     NOW() + INTERVAL '30 minutes', NOW() + INTERVAL '12 hours') \
             RETURNING id",
            &[&user_id, &app_id, &email, &"Test"],
        )
        .await
        .expect("seed gateway session");
    rows.first().expect("gateway session id").get("id")
}

async fn cleanup(client: &Client, email: &str) {
    let _ = client
        .execute(
            "DELETE FROM zeroship.gateway_sessions WHERE user_id IN \
             (SELECT id FROM zeroship.users WHERE email = $1::citext)",
            &[&email],
        )
        .await;
    let _ = client
        .execute(
            "DELETE FROM zeroship.idp_sessions WHERE user_id IN \
             (SELECT id FROM zeroship.users WHERE email = $1::citext)",
            &[&email],
        )
        .await;
    let _ = client
        .execute(
            "DELETE FROM zeroship.users WHERE email = $1::citext",
            &[&email],
        )
        .await;
}

/// Delete any apps we seeded (cascades the gateway sessions, but we already
/// cleaned those). Best-effort.
async fn cleanup_app(client: &Client, app_id: Uuid) {
    let _ = client
        .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id])
        .await;
}

// ─── list_by_user ────────────────────────────────────────────────────────

/// list_by_user returns BOTH an idp and a gateway session for the user,
/// newest-first, and tags each with the right kind + (gateway) app_id.
#[compio::test]
async fn list_returns_idp_and_gateway_sessions() {
    let Some(client) = pg().await else {
        eprintln!("skipping session_visibility_test (no AUTH_DB_URL)");
        return;
    };
    let email = format!("iss10-list-{}@zeroship.test", Uuid::new_v4().simple());
    let user = users::create(&client, &email, "Test", None)
        .await
        .expect("seed user");

    let idp = sessions::create(
        &client,
        &sessions::CreateSession {
            user_id: user.id,
            auth_method: "password",
            amr: vec!["pwd".to_string()],
            acr: None,
            expected_credential_version: None,
            idle_minutes: 30,
            absolute_hours: 12,
        },
    )
    .await
    .expect("seed idp session");

    let app_id = seed_app(&client).await;
    let gw_id = seed_gateway_session(&client, user.id, app_id, &email).await;

    let list = sessions::list_by_user(&client, user.id)
        .await
        .expect("list_by_user");

    assert_eq!(list.len(), 2, "expected exactly one idp + one gateway row");

    let idp_row = list
        .iter()
        .find(|s| s.kind == SessionKind::Idp)
        .expect("idp row present");
    assert_eq!(idp_row.id, idp.id);
    assert!(idp_row.app_id.is_none(), "idp session has no app_id");

    let gw_row = list
        .iter()
        .find(|s| s.kind == SessionKind::App)
        .expect("gateway row present");
    assert_eq!(gw_row.id, gw_id);
    assert_eq!(gw_row.app_id, Some(app_id), "gateway session carries app_id");

    cleanup(&client, &email).await;
    cleanup_app(&client, app_id).await;
}

/// list_by_user excludes revoked and expired sessions of BOTH kinds.
#[compio::test]
async fn list_excludes_revoked_and_expired() {
    let Some(client) = pg().await else {
        eprintln!("skipping session_visibility_test (no AUTH_DB_URL)");
        return;
    };
    let email = format!("iss10-excl-{}@zeroship.test", Uuid::new_v4().simple());
    let user = users::create(&client, &email, "Test", None)
        .await
        .expect("seed user");

    // A live idp session (should appear).
    let live = sessions::create(
        &client,
        &sessions::CreateSession {
            user_id: user.id,
            auth_method: "password",
            amr: vec!["pwd".to_string()],
            acr: None,
            expected_credential_version: None,
            idle_minutes: 30,
            absolute_hours: 12,
        },
    )
    .await
    .expect("seed live idp session");

    // A revoked idp session (should be hidden).
    let revoked = sessions::create(
        &client,
        &sessions::CreateSession {
            user_id: user.id,
            auth_method: "password",
            amr: vec!["pwd".to_string()],
            acr: None,
            expected_credential_version: None,
            idle_minutes: 30,
            absolute_hours: 12,
        },
    )
    .await
    .expect("seed revoked idp session");
    client
        .execute(
            "UPDATE zeroship.idp_sessions SET revoked_at = NOW() WHERE id = $1",
            &[&revoked.id],
        )
        .await
        .expect("revoke idp session");

    // An expired idp session (idle window already past).
    client
        .execute(
            "INSERT INTO zeroship.idp_sessions \
                (user_id, auth_method, amr, idle_expires_at, abs_expires_at) \
             VALUES ($1, 'password', ARRAY['pwd'], \
                     NOW() - INTERVAL '1 minute', NOW() + INTERVAL '12 hours')",
            &[&user.id],
        )
        .await
        .expect("seed expired idp session");

    // An expired gateway session.
    let app_id = seed_app(&client).await;
    client
        .execute(
            "INSERT INTO zeroship.gateway_sessions \
                (user_id, app_id, email, name, email_verified, idle_expires_at, abs_expires_at) \
             VALUES ($1, $2, $3::citext, $4, true, \
                     NOW() - INTERVAL '1 minute', NOW() + INTERVAL '12 hours')",
            &[&user.id, &app_id, &email, &"Test"],
        )
        .await
        .expect("seed expired gateway session");

    let list = sessions::list_by_user(&client, user.id)
        .await
        .expect("list_by_user");

    assert_eq!(
        list.len(),
        1,
        "only the single live idp session should be listed, got {list:?}"
    );
    assert_eq!(list[0].id, live.id);

    cleanup(&client, &email).await;
    cleanup_app(&client, app_id).await;
}

/// list_by_user never returns ANOTHER user's sessions.
#[compio::test]
async fn list_excludes_other_users_sessions() {
    let Some(client) = pg().await else {
        eprintln!("skipping session_visibility_test (no AUTH_DB_URL)");
        return;
    };
    let email_a = format!("iss10-a-{}@zeroship.test", Uuid::new_v4().simple());
    let email_b = format!("iss10-b-{}@zeroship.test", Uuid::new_v4().simple());
    let user_a = users::create(&client, &email_a, "A", None)
        .await
        .expect("seed user a");
    let user_b = users::create(&client, &email_b, "B", None)
        .await
        .expect("seed user b");

    // user_b has both an idp and a gateway session.
    sessions::create(
        &client,
        &sessions::CreateSession {
            user_id: user_b.id,
            auth_method: "password",
            amr: vec!["pwd".to_string()],
            acr: None,
            expected_credential_version: None,
            idle_minutes: 30,
            absolute_hours: 12,
        },
    )
    .await
    .expect("seed user_b idp session");
    let app_id = seed_app(&client).await;
    seed_gateway_session(&client, user_b.id, app_id, &email_b).await;

    // user_a has nothing.
    let list_a = sessions::list_by_user(&client, user_a.id)
        .await
        .expect("list_by_user a");
    assert!(
        list_a.is_empty(),
        "user_a must not see user_b's sessions, got {list_a:?}"
    );

    cleanup(&client, &email_a).await;
    cleanup(&client, &email_b).await;
    cleanup_app(&client, app_id).await;
}

// ─── revoke_one_for_user ───────────────────────────────────────────────────

/// revoke_one_for_user revokes the targeted idp session and returns true; the
/// session then disappears from the list.
#[compio::test]
async fn revoke_one_idp_session_succeeds() {
    let Some(client) = pg().await else {
        eprintln!("skipping session_visibility_test (no AUTH_DB_URL)");
        return;
    };
    let email = format!("iss10-revidp-{}@zeroship.test", Uuid::new_v4().simple());
    let user = users::create(&client, &email, "Test", None)
        .await
        .expect("seed user");
    let s = sessions::create(
        &client,
        &sessions::CreateSession {
            user_id: user.id,
            auth_method: "password",
            amr: vec!["pwd".to_string()],
            acr: None,
            expected_credential_version: None,
            idle_minutes: 30,
            absolute_hours: 12,
        },
    )
    .await
    .expect("seed idp session");

    let revoked = sessions::revoke_one_for_user(&client, user.id, s.id, SessionKind::Idp)
        .await
        .expect("revoke_one_for_user");
    assert!(revoked, "revoking the user's own idp session returns true");

    let list = sessions::list_by_user(&client, user.id)
        .await
        .expect("list_by_user");
    assert!(
        list.iter().all(|x| x.id != s.id),
        "revoked idp session must not be listed"
    );

    cleanup(&client, &email).await;
}

/// revoke_one_for_user revokes the targeted gateway session and returns true.
#[compio::test]
async fn revoke_one_gateway_session_succeeds() {
    let Some(client) = pg().await else {
        eprintln!("skipping session_visibility_test (no AUTH_DB_URL)");
        return;
    };
    let email = format!("iss10-revgw-{}@zeroship.test", Uuid::new_v4().simple());
    let user = users::create(&client, &email, "Test", None)
        .await
        .expect("seed user");
    let app_id = seed_app(&client).await;
    let gw_id = seed_gateway_session(&client, user.id, app_id, &email).await;

    let revoked = sessions::revoke_one_for_user(&client, user.id, gw_id, SessionKind::App)
        .await
        .expect("revoke_one_for_user gateway");
    assert!(revoked, "revoking the user's own gateway session returns true");

    let list = sessions::list_by_user(&client, user.id)
        .await
        .expect("list_by_user");
    assert!(
        list.iter().all(|x| x.id != gw_id),
        "revoked gateway session must not be listed"
    );

    cleanup(&client, &email).await;
    cleanup_app(&client, app_id).await;
}

/// THE IDOR GUARD. user_a tries to revoke user_b's session by id. Must return
/// false AND leave user_b's session active.
#[compio::test]
async fn revoke_other_users_session_is_noop_idor_guard() {
    let Some(client) = pg().await else {
        eprintln!("skipping session_visibility_test (no AUTH_DB_URL)");
        return;
    };
    let email_a = format!("iss10-idor-a-{}@zeroship.test", Uuid::new_v4().simple());
    let email_b = format!("iss10-idor-b-{}@zeroship.test", Uuid::new_v4().simple());
    let user_a = users::create(&client, &email_a, "A", None)
        .await
        .expect("seed user a");
    let user_b = users::create(&client, &email_b, "B", None)
        .await
        .expect("seed user b");

    // user_b's idp session.
    let b_idp = sessions::create(
        &client,
        &sessions::CreateSession {
            user_id: user_b.id,
            auth_method: "password",
            amr: vec!["pwd".to_string()],
            acr: None,
            expected_credential_version: None,
            idle_minutes: 30,
            absolute_hours: 12,
        },
    )
    .await
    .expect("seed user_b idp session");

    // user_b's gateway session.
    let app_id = seed_app(&client).await;
    let b_gw = seed_gateway_session(&client, user_b.id, app_id, &email_b).await;

    // user_a attempts to revoke BOTH of user_b's sessions by id.
    let idp_attempt =
        sessions::revoke_one_for_user(&client, user_a.id, b_idp.id, SessionKind::Idp)
            .await
            .expect("idor idp attempt");
    let gw_attempt = sessions::revoke_one_for_user(&client, user_a.id, b_gw, SessionKind::App)
        .await
        .expect("idor gateway attempt");

    assert!(
        !idp_attempt,
        "IDOR: user_a must NOT be able to revoke user_b's idp session"
    );
    assert!(
        !gw_attempt,
        "IDOR: user_a must NOT be able to revoke user_b's gateway session"
    );

    // user_b's sessions are still live.
    let b_list = sessions::list_by_user(&client, user_b.id)
        .await
        .expect("list_by_user b");
    assert_eq!(
        b_list.len(),
        2,
        "user_b's two sessions must still be active after the IDOR attempt, got {b_list:?}"
    );

    cleanup(&client, &email_a).await;
    cleanup(&client, &email_b).await;
    cleanup_app(&client, app_id).await;
}

/// Revoking an already-revoked or nonexistent id is a no-op (returns false).
#[compio::test]
async fn revoke_already_revoked_or_missing_is_noop() {
    let Some(client) = pg().await else {
        eprintln!("skipping session_visibility_test (no AUTH_DB_URL)");
        return;
    };
    let email = format!("iss10-noop-{}@zeroship.test", Uuid::new_v4().simple());
    let user = users::create(&client, &email, "Test", None)
        .await
        .expect("seed user");
    let s = sessions::create(
        &client,
        &sessions::CreateSession {
            user_id: user.id,
            auth_method: "password",
            amr: vec!["pwd".to_string()],
            acr: None,
            expected_credential_version: None,
            idle_minutes: 30,
            absolute_hours: 12,
        },
    )
    .await
    .expect("seed idp session");

    // First revoke succeeds.
    assert!(sessions::revoke_one_for_user(&client, user.id, s.id, SessionKind::Idp)
        .await
        .expect("first revoke"));
    // Second revoke of the same (already-revoked) id is a no-op.
    assert!(
        !sessions::revoke_one_for_user(&client, user.id, s.id, SessionKind::Idp)
            .await
            .expect("second revoke"),
        "re-revoking an already-revoked session returns false"
    );
    // A totally unknown id is a no-op.
    assert!(
        !sessions::revoke_one_for_user(&client, user.id, Uuid::new_v4(), SessionKind::Idp)
            .await
            .expect("missing revoke"),
        "revoking a nonexistent id returns false"
    );
    assert!(
        !sessions::revoke_one_for_user(&client, user.id, Uuid::new_v4(), SessionKind::App)
            .await
            .expect("missing gateway revoke"),
        "revoking a nonexistent gateway id returns false"
    );

    cleanup(&client, &email).await;
}
