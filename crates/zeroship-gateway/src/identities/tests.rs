//! Identity mappings and relay aliases under the migrated gateway role.

#![allow(
    clippy::future_not_send,
    reason = "database cases belong to their compio runtime"
)]

use chrono::{DateTime, Utc};
use compio_postgres::{Client, Row};
use zeroship_core::{app_id::AppId, auth, typed_id, user_id::UserId};

use super::{lookup_relay_email, upsert};
use crate::db::{checkout, tests::postgres::Database};
use crate::error::GatewayError;

struct App {
    client_id: String,
    sector: &'static str,
}

impl App {
    async fn seed(admin: &Client, sector: &'static str) -> Self {
        let client_id = typed_id::app_oauth_client_id(&AppId::mint());
        let redirects = vec![format!("{sector}/__zeroship/auth/callback")];
        let scopes = vec!["openid".to_owned(), "email".to_owned()];
        admin
            .execute(
                "INSERT INTO zeroship.oauth_clients \
                 (client_id, client_name, redirect_uris, scopes, \
                  refresh_allowed, token_endpoint_auth_method, brokered) \
                 VALUES ($1, 'Identity Fixture', $2, $3, TRUE, 'client_secret_basic', TRUE)",
                &[&client_id, &redirects, &scopes],
            )
            .await
            .expect("seed the app's OAuth client");
        Self { client_id, sector }
    }

    fn subject(&self, user: &UserId) -> String {
        auth::derive_pairwise(
            &auth::derive_pairwise_salt(b"gateway-identity-fixture"),
            user,
            self.sector,
        )
    }
}

async fn seed_user(admin: &Client, label: &str) -> UserId {
    let user = UserId::mint();
    admin
        .execute(
            "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2, $3)",
            &[&user.as_str(), &format!("{label}@zeroship.test"), &label],
        )
        .await
        .unwrap();
    user
}

async fn stored(admin: &Client, app: &App, user: &UserId) -> Row {
    admin
        .query_one(
            "SELECT pairwise_sub, relay_email, revoked_at, created_at \
             FROM zeroship.app_user_identities \
             WHERE app_client_id = $1 AND global_user_id = $2",
            &[&app.client_id, &user.as_str()],
        )
        .await
        .expect("observe the committed mapping independently of tenant filtering")
}

async fn assign_alias(admin: &Client, app: &App, user: &UserId, alias: &str) {
    assert_eq!(
        admin
            .execute(
                "UPDATE zeroship.app_user_identities SET relay_email = $3 \
                 WHERE app_client_id = $1 AND global_user_id = $2",
                &[&app.client_id, &user.as_str(), &alias],
            )
            .await
            .unwrap(),
        1
    );
}

async fn revoke(admin: &Client, app: &App, user: &UserId) {
    assert_eq!(
        admin
            .execute(
                "UPDATE zeroship.app_user_identities SET revoked_at = NOW() \
                 WHERE app_client_id = $1 AND global_user_id = $2",
                &[&app.client_id, &user.as_str()],
            )
            .await
            .unwrap(),
        1
    );
}

async fn assert_unscoped(client: &Client) {
    assert!(
        client
            .query("SELECT pairwise_sub FROM zeroship.app_user_identities", &[])
            .await
            .unwrap()
            .is_empty(),
        "the previous operation must not leave its tenant visible"
    );
    assert_eq!(
        client
            .execute(
                "UPDATE zeroship.app_user_identities SET revoked_at = NOW()",
                &[]
            )
            .await
            .unwrap(),
        0,
        "an unscoped connection must not modify identity mappings"
    );
}

#[compio::test]
async fn relay_lookup_distinguishes_missing_unminted_active_and_revoked_aliases() {
    Database::migrated(async |database| {
        let app = App::seed(&database.admin, "https://relay.zeroship.test").await;
        let user = seed_user(&database.admin, "relay-user").await;
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
        assert_eq!(
            lookup_relay_email(&mut client, &app.client_id, &user)
                .await
                .unwrap(),
            None
        );

        upsert(&mut client, &app.client_id, &user, &app.subject(&user))
            .await
            .unwrap();
        let row = stored(&database.admin, &app, &user).await;
        assert_eq!(row.get::<_, String>("pairwise_sub"), app.subject(&user));
        assert!(row.get::<_, Option<String>>("relay_email").is_none());
        assert!(row.get::<_, Option<DateTime<Utc>>>("revoked_at").is_none());
        assert_eq!(
            lookup_relay_email(&mut client, &app.client_id, &user)
                .await
                .unwrap(),
            None
        );

        let alias = "consented@relay.zeroship.test";
        assign_alias(&database.admin, &app, &user, alias).await;
        assert_eq!(
            lookup_relay_email(&mut client, &app.client_id, &user)
                .await
                .unwrap()
                .as_deref(),
            Some(alias)
        );
        revoke(&database.admin, &app, &user).await;
        assert_eq!(
            lookup_relay_email(&mut client, &app.client_id, &user)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            stored(&database.admin, &app, &user)
                .await
                .get::<_, Option<String>>("relay_email")
                .as_deref(),
            Some(alias)
        );
        assert_unscoped(&client).await;
    })
    .await;
}

#[compio::test]
async fn upsert_reactivates_the_same_mapping_without_replacing_its_alias() {
    Database::migrated(async |database| {
        let app = App::seed(&database.admin, "https://regrant.zeroship.test").await;
        let user = seed_user(&database.admin, "regrant-user").await;
        let pool = checkout(&database.config_as("zeroship_gateway", 1))
            .await
            .unwrap();
        let mut client = pool.acquire().await.unwrap();
        let subject = app.subject(&user);
        upsert(&mut client, &app.client_id, &user, &subject)
            .await
            .unwrap();
        let created: DateTime<Utc> = stored(&database.admin, &app, &user).await.get("created_at");
        assign_alias(&database.admin, &app, &user, "retained@relay.zeroship.test").await;
        revoke(&database.admin, &app, &user).await;
        assert!(stored(&database.admin, &app, &user)
            .await
            .get::<_, Option<DateTime<Utc>>>("revoked_at")
            .is_some());

        for _ in 0..2 {
            upsert(&mut client, &app.client_id, &user, &subject)
                .await
                .unwrap();
            let row = stored(&database.admin, &app, &user).await;
            assert_eq!(row.get::<_, String>("pairwise_sub"), subject);
            assert_eq!(row.get::<_, DateTime<Utc>>("created_at"), created);
            assert!(row.get::<_, Option<DateTime<Utc>>>("revoked_at").is_none());
            assert_eq!(
                row.get::<_, Option<String>>("relay_email").as_deref(),
                Some("retained@relay.zeroship.test")
            );
            assert_eq!(
                lookup_relay_email(&mut client, &app.client_id, &user)
                    .await
                    .unwrap()
                    .as_deref(),
                Some("retained@relay.zeroship.test")
            );
            assert_unscoped(&client).await;
        }
        assert_eq!(
            database
                .admin
                .query_one("SELECT count(*) FROM zeroship.app_user_identities", &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            1
        );
    })
    .await;
}

#[compio::test]
async fn upsert_refuses_subject_drift_without_reviving_or_overwriting_the_mapping() {
    Database::migrated(async |database| {
        let app = App::seed(&database.admin, "https://binding.zeroship.test").await;
        let user = seed_user(&database.admin, "binding-user").await;
        let subject = app.subject(&user);
        let changed_subject = auth::derive_pairwise(
            &auth::derive_pairwise_salt(b"gateway-identity-fixture"), &user, "https://changed.zeroship.test"
        );
        assert_ne!(subject, changed_subject);
        let pool = checkout(&database.config_as("zeroship_gateway", 1)).await.unwrap();
        let mut client = pool.acquire().await.unwrap();
        upsert(&mut client, &app.client_id, &user, &subject).await.unwrap();
        upsert(&mut client, &app.client_id, &user, &subject).await.unwrap();
        assign_alias(&database.admin, &app, &user, "bound@relay.zeroship.test").await;
        revoke(&database.admin, &app, &user).await;
        let before = stored(&database.admin, &app, &user).await;

        let error = upsert(&mut client, &app.client_id, &user, &changed_subject).await.expect_err("a stored subject cannot be rebound");
        assert!(matches!(error, GatewayError::Db(ref message) if message == "app_user_identities pairwise binding changed"));
        let after = stored(&database.admin, &app, &user).await;
        assert_eq!(after.get::<_, String>("pairwise_sub"), subject);
        assert_eq!(after.get::<_, Option<String>>("relay_email"), before.get::<_, Option<String>>("relay_email"));
        assert_eq!(after.get::<_, Option<DateTime<Utc>>>("revoked_at"), before.get::<_, Option<DateTime<Utc>>>("revoked_at"));
        assert_eq!(after.get::<_, DateTime<Utc>>("created_at"), before.get::<_, DateTime<Utc>>("created_at"));
        assert_unscoped(&client).await;
        assert_eq!(lookup_relay_email(&mut client, &app.client_id, &user).await.unwrap(), None);
        upsert(&mut client, &app.client_id, &user, &subject).await.unwrap();
        assert_eq!(lookup_relay_email(&mut client, &app.client_id, &user).await.unwrap().as_deref(), Some("bound@relay.zeroship.test"));
    }).await;
}

#[compio::test]
async fn relay_lookup_is_scoped_to_the_app_and_user_on_a_reused_connection() {
    Database::migrated(async |database| {
        let app = App::seed(&database.admin, "https://first.zeroship.test").await;
        let other_app = App::seed(&database.admin, "https://second.zeroship.test").await;
        let user = seed_user(&database.admin, "first-user").await;
        let other_user = seed_user(&database.admin, "second-user").await;
        let pool = checkout(&database.config_as("zeroship_gateway", 1))
            .await
            .unwrap();
        for (app, user, alias) in [
            (&app, &user, "first@relay.zeroship.test"),
            (&other_app, &user, "other-app@relay.zeroship.test"),
            (&app, &other_user, "other-user@relay.zeroship.test"),
        ] {
            let mut client = pool.acquire().await.unwrap();
            upsert(&mut client, &app.client_id, user, &app.subject(user))
                .await
                .unwrap();
            assign_alias(&database.admin, app, user, alias).await;
            assert_unscoped(&client).await;
        }
        for (app, user, alias) in [
            (&app, &user, "first@relay.zeroship.test"),
            (&other_app, &user, "other-app@relay.zeroship.test"),
            (&app, &other_user, "other-user@relay.zeroship.test"),
        ] {
            let mut client = pool.acquire().await.unwrap();
            assert_eq!(
                lookup_relay_email(&mut client, &app.client_id, user)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(alias)
            );
            assert_unscoped(&client).await;
        }
        revoke(&database.admin, &app, &user).await;
        let mut client = pool.acquire().await.unwrap();
        assert_eq!(
            lookup_relay_email(&mut client, &app.client_id, &user)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            lookup_relay_email(&mut client, &other_app.client_id, &user)
                .await
                .unwrap()
                .as_deref(),
            Some("other-app@relay.zeroship.test")
        );
        assert_eq!(
            lookup_relay_email(&mut client, &app.client_id, &other_user)
                .await
                .unwrap()
                .as_deref(),
            Some("other-user@relay.zeroship.test")
        );
        assert_unscoped(&client).await;
    })
    .await;
}

#[compio::test]
async fn relay_lookup_reports_database_errors_and_recovers_after_repair() {
    Database::migrated(async |database| {
        let app = App::seed(&database.admin, "https://repair.zeroship.test").await;
        let user = seed_user(&database.admin, "repair-user").await;
        let pool = checkout(&database.config_as("zeroship_gateway", 1))
            .await
            .unwrap();
        let mut client = pool.acquire().await.unwrap();
        upsert(&mut client, &app.client_id, &user, &app.subject(&user))
            .await
            .unwrap();
        assign_alias(&database.admin, &app, &user, "repaired@relay.zeroship.test").await;
        database
            .admin
            .batch_execute(
                "ALTER TABLE zeroship.app_user_identities RENAME TO fixture_unavailable_identities",
            )
            .await
            .unwrap();
        let error = lookup_relay_email(&mut client, &app.client_id, &user)
            .await
            .expect_err("database failure must not become an absent alias");
        assert!(matches!(error, GatewayError::Db(_)));
        database
            .admin
            .batch_execute(
                "ALTER TABLE zeroship.fixture_unavailable_identities RENAME TO app_user_identities",
            )
            .await
            .unwrap();
        assert_eq!(
            lookup_relay_email(&mut client, &app.client_id, &user)
                .await
                .unwrap()
                .as_deref(),
            Some("repaired@relay.zeroship.test")
        );
        assert_unscoped(&client).await;
    })
    .await;
}

#[compio::test]
async fn competing_first_writes_preserve_the_winning_subject() {
    use futures::{join, FutureExt};
    use std::panic::AssertUnwindSafe;

    Database::migrated(async |database| {
        let app = App::seed(&database.admin, "https://concurrent.zeroship.test").await;
        let user = seed_user(&database.admin, "concurrent-user").await;
        let first_subject = app.subject(&user);
        let second_subject = auth::derive_pairwise(
            &auth::derive_pairwise_salt(b"gateway-identity-fixture"),
            &user,
            "https://concurrent-other.zeroship.test",
        );
        assert_ne!(first_subject, second_subject);
        let pool = checkout(&database.config_as("zeroship_gateway", 2)).await.unwrap();
        let mut first = pool.acquire().await.unwrap();
        let mut second = pool.acquire().await.unwrap();
        let first_pid: i32 = first.query_one("SELECT pg_backend_pid()", &[]).await.unwrap().get(0);
        let second_pid: i32 = second.query_one("SELECT pg_backend_pid()", &[]).await.unwrap().get(0);
        assert_ne!(first_pid, second_pid);
        database.admin.batch_execute("BEGIN; LOCK TABLE zeroship.app_user_identities IN SHARE MODE").await.unwrap();
        let outcome = AssertUnwindSafe(async {
            let writes = async {
                join!(
                    upsert(&mut first, &app.client_id, &user, &first_subject),
                    upsert(&mut second, &app.client_id, &user, &second_subject),
                )
            };
            let release = async {
                database.wait_until_blocked(&[first_pid, second_pid]).await;
                database.admin.batch_execute("COMMIT").await.unwrap();
            };
            join!(writes, release).0
        }).catch_unwind().await;
        if outcome.is_err() {
            database.admin.batch_execute("ROLLBACK").await.expect("release the fixture lock after a failed barrier");
        }
        let results = outcome.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        let (winner, refusal) = match results {
            (Ok(()), Err(error)) => (first_subject, error),
            (Err(error), Ok(())) => (second_subject, error),
            other => panic!("competing subjects must produce a winner and a refusal: {other:?}"),
        };
        assert!(matches!(refusal, GatewayError::Db(ref message) if message == "app_user_identities pairwise binding changed"));
        assert_eq!(stored(&database.admin, &app, &user).await.get::<_, String>("pairwise_sub"), winner);
        assert_unscoped(&first).await;
        assert_unscoped(&second).await;
    }).await;
}
