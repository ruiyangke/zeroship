//! Session storage contracts against the migrated gateway role.

#![allow(
    clippy::future_not_send,
    reason = "database cases run inside their owning compio runtime"
)]

use chrono::{DateTime, TimeDelta, Utc};
use compio_postgres::{Client, Row};
use uuid::Uuid;

use super::{
    ABSOLUTE_HOURS, IDLE_MINUTES, NewSession, create, latest_sid_for_user,
    revoke_app_sessions_for_sid, revoke_app_sessions_for_user,
};
use crate::db::{checkout, tests::postgres::Database};
use zeroship_core::{app_id::AppId, user_id::UserId};

struct Principals {
    app: AppId,
    other_app: AppId,
    user: UserId,
    other_user: UserId,
}

impl Principals {
    async fn seed(admin: &Client) -> Self {
        let organization = zeroship_core::typed_id::generate("org");
        let project = zeroship_core::typed_id::generate("prj");
        admin
            .execute(
                "INSERT INTO zeroship.organizations (id, slug, name, billing_email) \
                 VALUES ($1, 'session-fixture', 'Session Fixture', 'fixture@zeroship.test')",
                &[&organization],
            )
            .await
            .unwrap();
        admin
            .execute(
                "INSERT INTO zeroship.projects (id, organization_id, slug, name) \
                 VALUES ($1, $2, 'default', 'Default')",
                &[&project, &organization],
            )
            .await
            .unwrap();
        admin
            .execute(
                "INSERT INTO zeroship.plans (id, name, runtime_limits_json, assignable_by_creator) \
                 VALUES ('free', 'Free', '{}'::jsonb, TRUE) ON CONFLICT (id) DO NOTHING",
                &[],
            )
            .await
            .unwrap();
        let principals = Self {
            app: AppId::mint(),
            other_app: AppId::mint(),
            user: UserId::mint(),
            other_user: UserId::mint(),
        };
        for app in [&principals.app, &principals.other_app] {
            admin
                .execute(
                    "INSERT INTO zeroship.apps (id, name, project_id, organization_id) \
                     VALUES ($1, $1, $2, $3)",
                    &[&app.as_str(), &project, &organization],
                )
                .await
                .unwrap();
        }
        for user in [&principals.user, &principals.other_user] {
            admin
                .execute(
                    "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2, 'Session User')",
                    &[&user.as_str(), &format!("{}@zeroship.test", user.as_str())],
                )
                .await
                .unwrap();
        }
        principals
    }
}

fn new_session<'a>(app: &'a AppId, user: &'a UserId, sid: Option<&'a str>) -> NewSession<'a> {
    NewSession {
        app_id: app,
        user_id: user,
        sid,
        email: None,
        name: None,
        avatar_url: None,
        email_verified: false,
        granted_scopes: &[],
        auth_time: None,
        amr: &[],
    }
}

async fn stored_row(admin: &Client, id: Uuid) -> Row {
    admin
        .query_one(
            "SELECT *, email::text AS email_text FROM zeroship.gateway_sessions WHERE id = $1",
            &[&id],
        )
        .await
        .expect("observe the committed session independently of tenant filtering")
}

async fn revoked_at(admin: &Client, id: Uuid) -> Option<DateTime<Utc>> {
    stored_row(admin, id).await.get("revoked_at")
}

async fn assert_unscoped(client: &Client) {
    assert!(
        client
            .query("SELECT id FROM zeroship.gateway_sessions", &[])
            .await
            .unwrap()
            .is_empty(),
        "a completed operation must not leave another caller's tenant visible"
    );
    assert_eq!(
        client
            .execute(
                "UPDATE zeroship.gateway_sessions SET revoked_at = NOW()",
                &[]
            )
            .await
            .unwrap(),
        0,
        "an unscoped connection must not change session rows"
    );
}

#[compio::test]
async fn create_commits_identity_claims_and_fixed_expiry_windows() {
    Database::migrated(async |database| {
        let principals = Principals::seed(&database.admin).await;
        let pool = checkout(&database.config_as("zeroship_gateway", 1))
            .await
            .unwrap();
        let mut client = pool.acquire().await.unwrap();
        let role = client
            .query_one(
                "SELECT current_user::text AS name, rolsuper, rolbypassrls \
             FROM pg_roles WHERE rolname = current_user",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(role.get::<_, String>("name"), "zeroship_gateway");
        assert!(!role.get::<_, bool>("rolsuper"));
        assert!(!role.get::<_, bool>("rolbypassrls"));

        let scopes = vec!["openid".into(), "profile".into()];
        let methods = vec!["pwd".into(), "otp".into()];
        let params = NewSession {
            email: Some("Session.User@zeroship.test"),
            name: Some("Session User"),
            avatar_url: Some("https://zeroship.test/avatar.png"),
            email_verified: true,
            granted_scopes: &scopes,
            auth_time: Some(1_700_000_000),
            amr: &methods,
            ..new_session(&principals.app, &principals.user, Some("op-session"))
        };
        let session = create(&mut client, &params).await.unwrap();
        assert_eq!(session.app_id, principals.app);
        assert_eq!(session.user_id, principals.user);
        assert_eq!(session.sid.as_deref(), params.sid);
        assert_eq!(session.email.as_deref(), params.email);
        assert_eq!(session.name.as_deref(), params.name);
        assert_eq!(session.avatar_url.as_deref(), params.avatar_url);
        assert!(session.email_verified);
        assert_eq!(session.granted_scopes, scopes);
        assert_eq!(
            session.auth_time.map(|time| time.timestamp()),
            params.auth_time
        );
        assert_eq!(session.amr, methods);

        let stored = stored_row(&database.admin, session.id).await;
        assert_eq!(stored.get::<_, String>("app_id"), principals.app.as_str());
        assert_eq!(stored.get::<_, String>("user_id"), principals.user.as_str());
        assert_eq!(
            stored.get::<_, Option<String>>("sid").as_deref(),
            params.sid
        );
        assert_eq!(
            stored.get::<_, Option<String>>("email_text").as_deref(),
            params.email
        );
        assert_eq!(
            stored.get::<_, Option<String>>("name").as_deref(),
            params.name
        );
        assert_eq!(
            stored.get::<_, Option<String>>("avatar_url").as_deref(),
            params.avatar_url
        );
        assert!(stored.get::<_, bool>("email_verified"));
        assert_eq!(stored.get::<_, Vec<String>>("granted_scopes"), scopes);
        assert_eq!(
            stored.get::<_, Option<DateTime<Utc>>>("auth_time"),
            session.auth_time
        );
        assert_eq!(stored.get::<_, Vec<String>>("amr"), methods);
        assert!(
            stored
                .get::<_, Option<DateTime<Utc>>>("revoked_at")
                .is_none()
        );
        let issued: DateTime<Utc> = stored.get("issued_at");
        assert_eq!(
            session.idle_expires_at - issued,
            TimeDelta::minutes(IDLE_MINUTES)
        );
        assert_eq!(
            session.abs_expires_at - issued,
            TimeDelta::hours(ABSOLUTE_HOURS)
        );
        assert_eq!(
            stored.get::<_, DateTime<Utc>>("idle_expires_at"),
            session.idle_expires_at
        );
        assert_eq!(
            stored.get::<_, DateTime<Utc>>("abs_expires_at"),
            session.abs_expires_at
        );
        assert_unscoped(&client).await;
    })
    .await;
}

#[compio::test]
async fn create_preserves_absent_claims_and_empty_arrays() {
    Database::migrated(async |database| {
        let principals = Principals::seed(&database.admin).await;
        let pool = checkout(&database.config_as("zeroship_gateway", 1))
            .await
            .unwrap();
        let mut client = pool.acquire().await.unwrap();
        let session = create(
            &mut client,
            &new_session(&principals.app, &principals.user, None),
        )
        .await
        .unwrap();
        assert!(session.sid.is_none());
        assert!(session.email.is_none());
        assert!(session.name.is_none());
        assert!(session.avatar_url.is_none());
        assert!(session.auth_time.is_none());
        assert!(!session.email_verified);
        assert!(session.granted_scopes.is_empty());
        assert!(session.amr.is_empty());
        let stored = stored_row(&database.admin, session.id).await;
        for column in ["sid", "email_text", "name", "avatar_url"] {
            assert!(
                stored.get::<_, Option<String>>(column).is_none(),
                "{column}"
            );
        }
        assert!(
            stored
                .get::<_, Option<DateTime<Utc>>>("auth_time")
                .is_none()
        );
        assert!(!stored.get::<_, bool>("email_verified"));
        assert!(stored.get::<_, Vec<String>>("granted_scopes").is_empty());
        assert!(stored.get::<_, Vec<String>>("amr").is_empty());
        assert_unscoped(&client).await;
    })
    .await;
}

#[compio::test]
async fn user_revocation_is_scoped_idempotent_and_rebinds_the_connection() {
    Database::migrated(async |database| {
        let principals = Principals::seed(&database.admin).await;
        let pool = checkout(&database.config_as("zeroship_gateway", 1))
            .await
            .unwrap();
        let mut client = pool.acquire().await.unwrap();
        let target = create(
            &mut client,
            &new_session(&principals.app, &principals.user, Some("first")),
        )
        .await
        .unwrap();
        let sibling = create(
            &mut client,
            &new_session(&principals.app, &principals.user, Some("second")),
        )
        .await
        .unwrap();
        let other_app = create(
            &mut client,
            &new_session(&principals.other_app, &principals.user, Some("first")),
        )
        .await
        .unwrap();
        let other_user = create(
            &mut client,
            &new_session(&principals.app, &principals.other_user, Some("first")),
        )
        .await
        .unwrap();

        assert_eq!(
            revoke_app_sessions_for_user(&mut client, &principals.app, &principals.user)
                .await
                .unwrap(),
            2
        );
        let target_revoked = revoked_at(&database.admin, target.id)
            .await
            .expect("target revoked");
        let sibling_revoked = revoked_at(&database.admin, sibling.id)
            .await
            .expect("sibling revoked");
        assert!(revoked_at(&database.admin, other_app.id).await.is_none());
        assert!(revoked_at(&database.admin, other_user.id).await.is_none());
        assert_unscoped(&client).await;
        assert_eq!(
            revoke_app_sessions_for_user(&mut client, &principals.app, &principals.user)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            revoked_at(&database.admin, target.id).await,
            Some(target_revoked)
        );
        assert_eq!(
            revoked_at(&database.admin, sibling.id).await,
            Some(sibling_revoked)
        );

        drop(client);
        let mut client = pool.acquire().await.unwrap();
        assert_unscoped(&client).await;
        assert_eq!(
            revoke_app_sessions_for_user(&mut client, &principals.other_app, &principals.user)
                .await
                .unwrap(),
            1
        );
        assert!(revoked_at(&database.admin, other_app.id).await.is_some());
        assert!(revoked_at(&database.admin, other_user.id).await.is_none());
        assert_unscoped(&client).await;
    })
    .await;
}

#[compio::test]
async fn sid_revocation_requires_the_subject_to_match_when_supplied() {
    Database::migrated(async |database| {
        let principals = Principals::seed(&database.admin).await;
        let pool = checkout(&database.config_as("zeroship_gateway", 1))
            .await
            .unwrap();
        let mut client = pool.acquire().await.unwrap();
        let target = create(
            &mut client,
            &new_session(&principals.app, &principals.user, Some("target")),
        )
        .await
        .unwrap();
        let other_sid = create(
            &mut client,
            &new_session(&principals.app, &principals.user, Some("other")),
        )
        .await
        .unwrap();
        let other_app = create(
            &mut client,
            &new_session(&principals.other_app, &principals.user, Some("target")),
        )
        .await
        .unwrap();
        let other_user = create(
            &mut client,
            &new_session(&principals.app, &principals.other_user, Some("other")),
        )
        .await
        .unwrap();

        assert!(
            revoke_app_sessions_for_sid(
                &mut client,
                &principals.app,
                "target",
                Some(&principals.other_user)
            )
            .await
            .unwrap()
            .is_empty()
        );
        assert!(revoked_at(&database.admin, target.id).await.is_none());
        assert_eq!(
            revoke_app_sessions_for_sid(
                &mut client,
                &principals.app,
                "target",
                Some(&principals.user)
            )
            .await
            .unwrap(),
            vec![principals.user.clone()]
        );
        let timestamp = revoked_at(&database.admin, target.id)
            .await
            .expect("matched sid revoked");
        for id in [other_sid.id, other_app.id, other_user.id] {
            assert!(revoked_at(&database.admin, id).await.is_none());
        }
        assert_eq!(
            revoke_app_sessions_for_sid(
                &mut client,
                &principals.app,
                "target",
                Some(&principals.user)
            )
            .await
            .unwrap(),
            vec![principals.user.clone()]
        );
        assert_eq!(
            revoked_at(&database.admin, target.id).await,
            Some(timestamp)
        );
        assert_unscoped(&client).await;
    })
    .await;
}

#[compio::test]
async fn sid_revocation_without_subject_returns_distinct_users_including_prior_revocations() {
    Database::migrated(async |database| {
        let principals = Principals::seed(&database.admin).await;
        let pool = checkout(&database.config_as("zeroship_gateway", 1))
            .await
            .unwrap();
        let mut client = pool.acquire().await.unwrap();
        let prior = create(
            &mut client,
            &new_session(&principals.app, &principals.user, Some("target")),
        )
        .await
        .unwrap();
        assert_eq!(
            revoke_app_sessions_for_user(&mut client, &principals.app, &principals.user)
                .await
                .unwrap(),
            1
        );
        let prior_timestamp = revoked_at(&database.admin, prior.id)
            .await
            .expect("previous revocation");
        let live = create(
            &mut client,
            &new_session(&principals.app, &principals.other_user, Some("target")),
        )
        .await
        .unwrap();
        let duplicate = create(
            &mut client,
            &new_session(&principals.app, &principals.other_user, Some("target")),
        )
        .await
        .unwrap();
        let other_app = create(
            &mut client,
            &new_session(&principals.other_app, &principals.user, Some("target")),
        )
        .await
        .unwrap();
        let other_sid = create(
            &mut client,
            &new_session(&principals.app, &principals.user, Some("other")),
        )
        .await
        .unwrap();

        let users = revoke_app_sessions_for_sid(&mut client, &principals.app, "target", None)
            .await
            .unwrap();
        assert_eq!(users.len(), 2);
        assert!(users.contains(&principals.user));
        assert!(users.contains(&principals.other_user));
        assert_eq!(
            revoked_at(&database.admin, prior.id).await,
            Some(prior_timestamp)
        );
        assert!(revoked_at(&database.admin, live.id).await.is_some());
        assert!(revoked_at(&database.admin, duplicate.id).await.is_some());
        assert!(revoked_at(&database.admin, other_app.id).await.is_none());
        assert!(revoked_at(&database.admin, other_sid.id).await.is_none());
        assert!(
            revoke_app_sessions_for_sid(&mut client, &principals.app, "absent", None)
                .await
                .unwrap()
                .is_empty()
        );
        assert_unscoped(&client).await;
    })
    .await;
}

#[compio::test]
async fn latest_sid_uses_the_newest_non_null_value_for_the_app_and_user() {
    Database::migrated(async |database| {
        let principals = Principals::seed(&database.admin).await;
        let pool = checkout(&database.config_as("zeroship_gateway", 1)).await.unwrap();
        let mut client = pool.acquire().await.unwrap();
        assert!(latest_sid_for_user(&mut client, &principals.app, &principals.user).await.unwrap().is_none());
        for (issued, app, user, sid) in [
            (1_700_000_000_i64, &principals.app, &principals.user, Some("old")),
            (1_700_000_010, &principals.app, &principals.user, Some("new")),
            (1_700_000_020, &principals.app, &principals.user, None),
            (1_700_000_030, &principals.other_app, &principals.user, Some("other-app")),
            (1_700_000_040, &principals.app, &principals.other_user, Some("other-user")),
        ] {
            let session = create(&mut client, &new_session(app, user, sid)).await.unwrap();
            database.admin.execute(
                "UPDATE zeroship.gateway_sessions SET issued_at = to_timestamp($1::bigint) WHERE id = $2",
                &[&issued, &session.id],
            ).await.unwrap();
        }
        assert_eq!(latest_sid_for_user(&mut client, &principals.app, &principals.user).await.unwrap().as_deref(), Some("new"));
        assert_unscoped(&client).await;
        assert_eq!(latest_sid_for_user(&mut client, &principals.other_app, &principals.user).await.unwrap().as_deref(), Some("other-app"));
        assert_eq!(latest_sid_for_user(&mut client, &principals.app, &principals.other_user).await.unwrap().as_deref(), Some("other-user"));
        assert_unscoped(&client).await;
    }).await;
}

#[compio::test]
async fn failed_create_rolls_back_and_the_connection_can_serve_another_app() {
    Database::migrated(async |database| {
        let principals = Principals::seed(&database.admin).await;
        let pool = checkout(&database.config_as("zeroship_gateway", 1))
            .await
            .unwrap();
        let mut client = pool.acquire().await.unwrap();
        let existing = create(
            &mut client,
            &new_session(&principals.app, &principals.user, None),
        )
        .await
        .unwrap();
        let unknown_app = AppId::mint();
        create(
            &mut client,
            &new_session(&unknown_app, &principals.user, Some("rejected")),
        )
        .await
        .expect_err("an unknown app must fail its foreign key");
        assert_eq!(
            database
                .admin
                .query_one("SELECT count(*) FROM zeroship.gateway_sessions", &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            1
        );
        assert_unscoped(&client).await;
        let recovered = create(
            &mut client,
            &new_session(&principals.other_app, &principals.user, Some("recovered")),
        )
        .await
        .unwrap();
        assert_eq!(
            stored_row(&database.admin, recovered.id)
                .await
                .get::<_, String>("app_id"),
            principals.other_app.as_str()
        );
        assert!(revoked_at(&database.admin, existing.id).await.is_none());
        assert_unscoped(&client).await;
    })
    .await;
}
