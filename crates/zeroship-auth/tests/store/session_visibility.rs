//! Active-session visibility and revocation under the auth role in owned databases.

#![allow(clippy::future_not_send)]

use compio_postgres::Client;
use uuid::Uuid;

use crate::common::{self, database::Database};

use zeroship_auth::store::sessions::{self, SessionKind};
use zeroship_auth::store::users;

/// Seed the app and control-plane records referenced by gateway sessions.
async fn seed_app(database: &Database) -> zeroship_core::AppId {
    let client = database.connect().await;
    let app_id = zeroship_core::AppId::mint();
    let plan_id = "session-visibility-test-plan";
    client
        .execute(
            "INSERT INTO zeroship.plans \
                 (id, name, base_fee_cents, included_units, spend_limit_default_cents, \
                  runtime_limits_json) \
             VALUES ($1, 'Session Visibility Test Plan', 0, 0, 0, \
                     '{\"cpu_ms\":1000,\"wall_ms\":5000,\"memory_mb\":128,\"concurrency\":10}'::jsonb)",
            &[&plan_id],
        )
        .await
        .expect("seed session visibility test plan");
    // An app row needs a project, and a project needs an organization. Nothing
    // here asserts on authority, so the organization is left member-less.
    let project_id = common::unowned_project(&client).await;
    client
        .execute(
            "INSERT INTO zeroship.apps (id, name, plan_id, project_id, organization_id) \
             SELECT $1, $2, $3, p.id, p.organization_id FROM zeroship.projects p WHERE p.id = $4",
            &[
                &app_id.as_str(),
                &format!("iss10-app-{}", app_id.as_str()),
                &plan_id,
                &project_id,
            ],
        )
        .await
        .expect("seed app");
    app_id
}

/// Seed one live gateway session for `user_id`@`app_id`, returning its id.
async fn seed_gateway_session(
    database: &Database,
    user_id: &zeroship_core::UserId,
    app_id: &zeroship_core::AppId,
    email: &str,
) -> Uuid {
    let client = database.connect().await;
    let rows = client
        .query(
            "INSERT INTO zeroship.gateway_sessions \
                (user_id, app_id, email, name, email_verified, idle_expires_at, abs_expires_at) \
             VALUES ($1, $2, $3::citext, $4, true, \
                     NOW() + INTERVAL '30 minutes', NOW() + INTERVAL '12 hours') \
             RETURNING id",
            &[&user_id.as_str(), &app_id.as_str(), &email, &"Test"],
        )
        .await
        .expect("seed gateway session");
    rows.first().expect("gateway session id").get("id")
}

async fn seed_idp_session(client: &Client, user_id: &zeroship_core::UserId) -> sessions::Session {
    sessions::create(
        client,
        &sessions::CreateSession {
            user_id: user_id.clone(),
            auth_method: "password",
            amr: vec!["pwd".into()],
            acr: None,
            expected_credential_version: None,
            idle_minutes: 30,
            absolute_hours: 12,
        },
    )
    .await
    .expect("seed IDP session")
}

// ─── list_by_user ────────────────────────────────────────────────────────

/// Sessions of both kinds are ordered together and retain their kind and app identity.
#[compio::test]
async fn list_returns_idp_and_gateway_sessions() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let client = database.connect_as_auth().await;
        let email = format!("iss10-list-{}@zeroship.test", Uuid::new_v4().simple());
        let user = users::create(&orm, &email, "Test", None)
            .await
            .expect("seed user");

        let idp = seed_idp_session(&client, &user.id).await;
        let seed = database.connect().await;
        seed.execute(
            "UPDATE zeroship.idp_sessions SET auth_time = NOW() - INTERVAL '1 day' WHERE id = $1",
            &[&idp.id],
        )
        .await
        .expect("put the IDP session before the gateway session");

        let app_id = seed_app(database).await;
        let gw_id = seed_gateway_session(database, &user.id, &app_id, &email).await;

        let list = sessions::list_by_user(&client, &user.id)
            .await
            .expect("list_by_user");

        assert_eq!(
            list.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![gw_id, idp.id],
            "sessions are listed newest first across both kinds"
        );

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
        assert_eq!(
            gw_row.app_id,
            Some(app_id.clone()),
            "gateway session carries app_id"
        );
    })
    .await;
}

/// Revoked and expired sessions are excluded from the active list.
#[compio::test]
async fn list_excludes_revoked_and_expired() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let client = database.connect_as_auth().await;
        let email = format!("iss10-excl-{}@zeroship.test", Uuid::new_v4().simple());
        let user = users::create(&orm, &email, "Test", None)
            .await
            .expect("seed user");

        // A live idp session (should appear).
        let live = seed_idp_session(&client, &user.id).await;

        // A revoked idp session (should be hidden).
        let revoked = seed_idp_session(&client, &user.id).await;
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
                &[&user.id.as_str()],
            )
            .await
            .expect("seed expired idp session");

        // An expired gateway session.
        let app_id = seed_app(database).await;
        let live_gateway = seed_gateway_session(database, &user.id, &app_id, &email).await;
        let revoked_gateway = seed_gateway_session(database, &user.id, &app_id, &email).await;
        let seed = database.connect().await;
        seed.execute(
            "UPDATE zeroship.gateway_sessions SET revoked_at = NOW() WHERE id = $1",
            &[&revoked_gateway],
        ).await.expect("seed a revoked gateway audit row");
        seed
            .execute(
                "INSERT INTO zeroship.gateway_sessions \
                    (user_id, app_id, email, name, email_verified, idle_expires_at, abs_expires_at) \
                 VALUES ($1, $2, $3::citext, $4, true, \
                         NOW() - INTERVAL '1 minute', NOW() + INTERVAL '12 hours')",
                &[&user.id.as_str(), &app_id.as_str(), &email, &"Test"],
            )
            .await
            .expect("seed expired gateway session");

        let list = sessions::list_by_user(&client, &user.id)
            .await
            .expect("list_by_user");

        let mut actual: Vec<_> = list.iter().map(|row| row.id).collect();
        actual.sort_unstable();
        let mut expected = [live.id, live_gateway];
        expected.sort_unstable();
        assert_eq!(actual, expected, "only live sessions of either kind are listed");

    })
    .await;
}

/// Each user sees their own sessions while another user's sessions stay private.
#[compio::test]
async fn list_excludes_other_users_sessions() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let client = database.connect_as_auth().await;
        let email_a = format!("iss10-a-{}@zeroship.test", Uuid::new_v4().simple());
        let email_b = format!("iss10-b-{}@zeroship.test", Uuid::new_v4().simple());
        let user_a = users::create(&orm, &email_a, "A", None)
            .await
            .expect("seed user a");
        let user_b = users::create(&orm, &email_b, "B", None)
            .await
            .expect("seed user b");

        // user_b has both an idp and a gateway session.
        seed_idp_session(&client, &user_b.id).await;
        let app_id = seed_app(database).await;
        seed_gateway_session(database, &user_b.id, &app_id, &email_b).await;

        let a_idp = seed_idp_session(&client, &user_a.id).await;
        let list_a = sessions::list_by_user(&client, &user_a.id)
            .await
            .expect("list_by_user a");
        assert_eq!(
            list_a.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![a_idp.id],
            "user_a sees their own session and no session belonging to user_b"
        );
        assert_eq!(
            sessions::list_by_user(&client, &user_b.id)
                .await
                .unwrap()
                .len(),
            2,
            "the excluded user's sessions exist and remain active"
        );
    })
    .await;
}

// ─── revoke_one_for_user ───────────────────────────────────────────────────

/// Revoking an IDP session leaves the user's other sessions active.
#[compio::test]
async fn revoke_one_idp_session_succeeds() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let client = database.connect_as_auth().await;
        let email = format!("iss10-revidp-{}@zeroship.test", Uuid::new_v4().simple());
        let user = users::create(&orm, &email, "Test", None)
            .await
            .expect("seed user");
        let s = seed_idp_session(&client, &user.id).await;
        let other = seed_idp_session(&client, &user.id).await;

        let revoked = sessions::revoke_one_for_user(&client, &user.id, s.id, SessionKind::Idp)
            .await
            .expect("revoke_one_for_user")
            .expect("revoking the user's own idp session reports what it ended");
        assert_eq!(revoked.kind, SessionKind::Idp);
        assert!(revoked.app_id.is_none(), "an idp session has no app_id");

        let list = sessions::list_by_user(&client, &user.id)
            .await
            .expect("list_by_user");
        assert_eq!(
            list.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![other.id],
            "only the targeted IDP session is revoked"
        );
    })
    .await;
}

/// Gateway revocation preserves other sessions and identifies the app for logout.
/// The gateway's request enforcement is exercised by
/// `app_session_revoke_at_the_op_ends_the_gateway_session` in
/// `crates/zeroship-gateway/tests/oidc_rp_e2e.rs`.
#[compio::test]
async fn revoke_one_gateway_session_succeeds() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let client = database.connect_as_auth().await;
        let email = format!("iss10-revgw-{}@zeroship.test", Uuid::new_v4().simple());
        let user = users::create(&orm, &email, "Test", None)
            .await
            .expect("seed user");
        let app_id = seed_app(database).await;
        let gw_id = seed_gateway_session(database, &user.id, &app_id, &email).await;
        let other = seed_gateway_session(database, &user.id, &app_id, &email).await;

        let revoked = sessions::revoke_one_for_user(&client, &user.id, gw_id, SessionKind::App)
            .await
            .expect("revoke_one_for_user gateway")
            .expect("revoking the user's own gateway session reports what it ended");
        assert_eq!(revoked.kind, SessionKind::App);
        assert_eq!(
            revoked.app_id,
            Some(app_id.clone()),
            "the app arm must report which app to send the back-channel logout to"
        );

        let list = sessions::list_by_user(&client, &user.id)
            .await
            .expect("list_by_user");
        assert_eq!(
            list.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![other],
            "only the targeted gateway session is revoked"
        );
    })
    .await;
}

/// A caller cannot revoke another user's session; its owner can revoke the same ID.
#[compio::test]
async fn revoke_other_users_session_is_noop_idor_guard() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let client = database.connect_as_auth().await;
        let email_a = format!("iss10-idor-a-{}@zeroship.test", Uuid::new_v4().simple());
        let email_b = format!("iss10-idor-b-{}@zeroship.test", Uuid::new_v4().simple());
        let user_a = users::create(&orm, &email_a, "A", None)
            .await
            .expect("seed user a");
        let user_b = users::create(&orm, &email_b, "B", None)
            .await
            .expect("seed user b");

        // user_b's idp session.
        let b_idp = seed_idp_session(&client, &user_b.id).await;

        // user_b's gateway session.
        let app_id = seed_app(database).await;
        let b_gw = seed_gateway_session(database, &user_b.id, &app_id, &email_b).await;

        // user_a attempts to revoke BOTH of user_b's sessions by id.
        let idp_attempt =
            sessions::revoke_one_for_user(&client, &user_a.id, b_idp.id, SessionKind::Idp)
                .await
                .expect("idor idp attempt");
        let gw_attempt = sessions::revoke_one_for_user(&client, &user_a.id, b_gw, SessionKind::App)
            .await
            .expect("idor gateway attempt");

        assert!(
            idp_attempt.is_none(),
            "IDOR: user_a must NOT be able to revoke user_b's idp session"
        );
        assert!(
            gw_attempt.is_none(),
            "IDOR: user_a must NOT be able to revoke user_b's gateway session"
        );

        // user_b's sessions are still live.
        let b_list = sessions::list_by_user(&client, &user_b.id)
            .await
            .expect("list_by_user b");
        assert_eq!(
            b_list.len(),
            2,
            "user_b's two sessions must still be active after the IDOR attempt, got {b_list:?}"
        );

        assert!(
            sessions::revoke_one_for_user(&client, &user_b.id, b_idp.id, SessionKind::Idp)
                .await
                .unwrap()
                .is_some(),
            "the rightful owner can revoke the same IDP session"
        );
        assert!(
            sessions::revoke_one_for_user(&client, &user_b.id, b_gw, SessionKind::App)
                .await
                .unwrap()
                .is_some(),
            "the rightful owner can revoke the same gateway session"
        );
        assert!(sessions::list_by_user(&client, &user_b.id)
            .await
            .unwrap()
            .is_empty());
    })
    .await;
}

/// Revoking an already-revoked or nonexistent id returns no revoked session.
#[compio::test]
async fn revoke_already_revoked_or_missing_is_noop() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let client = database.connect_as_auth().await;
        let email = format!("iss10-noop-{}@zeroship.test", Uuid::new_v4().simple());
        let user = users::create(&orm, &email, "Test", None)
            .await
            .expect("seed user");
        let s = seed_idp_session(&client, &user.id).await;

        // First revoke succeeds.
        assert!(
            sessions::revoke_one_for_user(&client, &user.id, s.id, SessionKind::Idp)
                .await
                .expect("first revoke")
                .is_some()
        );
        // Second revoke of the same (already-revoked) id is a no-op.
        assert!(
            sessions::revoke_one_for_user(&client, &user.id, s.id, SessionKind::Idp)
                .await
                .expect("second revoke")
                .is_none(),
            "re-revoking an already-revoked session reports nothing ended"
        );
        // A totally unknown id is a no-op.
        assert!(
            sessions::revoke_one_for_user(&client, &user.id, Uuid::new_v4(), SessionKind::Idp)
                .await
                .expect("missing revoke")
                .is_none(),
            "revoking a nonexistent id reports nothing ended"
        );
        assert!(
            sessions::revoke_one_for_user(&client, &user.id, Uuid::new_v4(), SessionKind::App)
                .await
                .expect("missing gateway revoke")
                .is_none(),
            "revoking a nonexistent gateway id reports nothing ended"
        );
    })
    .await;
}
