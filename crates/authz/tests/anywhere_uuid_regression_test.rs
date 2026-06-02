//! Regression test for H2: `load_principal_app_resources` (eval.rs) read the
//! `app_members.app_id` UUID column into a Rust `String`, which makes
//! compio-postgres panic with `WrongType { postgres: Uuid, rust: String }`.
//!
//! `is_authorized_anywhere` calls `load_principal_app_resources`
//! UNCONDITIONALLY, so any principal with >=1 `app_members` row panicked the
//! moment the consent screen or PAT-minting path invoked the check.
//!
//! This is the sibling of the already-fixed `entities.rs:183` bug.
//!
//! The test seeds ONE app + ONE membership row (binding `app_id` as a real
//! `Uuid`, mirroring production writes), then drives `is_authorized_anywhere`.
//! Pre-fix it panics at the `row.get("app_id")` String read; post-fix it
//! returns `true` (the owner can grant an action on their own app).

use compio_postgres::{connect, Client, NoTls};
use std::future::Future;
use uuid::Uuid;
use zeroship_authz::{is_authorized_anywhere, load_platform_policies, Action, AuthzContext, Resource};

#[test]
fn is_authorized_anywhere_with_app_membership_reads_uuid_app_id() {
    run_db_test(|pg| async move {
        let user_id = Uuid::new_v4();
        let app_uuid = Uuid::new_v4();
        let email = format!("h2-{user_id}@example.com");
        let app_name = format!("h2-app-{}", Uuid::new_v4().simple());

        pg.execute(
            "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2::citext, $3)",
            &[&user_id, &email, &"h2"],
        )
        .await
        .expect("insert user");
        pg.execute(
            "INSERT INTO zeroship.apps (id, name, api_key, api_key_hash) VALUES ($1, $2, $3, $4)",
            &[&app_uuid, &app_name, &"test-api-key", &"test-api-key-hash"],
        )
        .await
        .expect("insert app");
        // Bind `app_id` as a real `Uuid` — this is how production writes it
        // (the column is `UUID`). Seeding it as a `String` would fail at the
        // INSERT and never reach the code under test.
        pg.execute(
            "INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ($1, $2, 'owner')",
            &[&app_uuid, &user_id],
        )
        .await
        .expect("insert app member");

        let policies = load_platform_policies().unwrap();
        let ctx = AuthzContext {
            principal_id: user_id,
            token_id: None,
            token_policy: None,
            action: Action::AppsDeploy,
            resource: Resource::Any,
            now: 12 * 60 * 60,
            request_ip: None,
            mfa_verified: false,
            mfa_age_seconds: None,
            request_id: None,
        };

        // Pre-fix: this panics inside `load_principal_app_resources` with
        // `WrongType { postgres: Uuid, rust: String }` reading `app_id`.
        // Post-fix: the app owner can grant `apps:deploy` on their own app.
        let allowed = is_authorized_anywhere(&pg, &policies, &ctx)
            .await
            .expect("is_authorized_anywhere should not error");
        assert!(
            allowed,
            "app owner should be authorized to grant apps:deploy on an owned app"
        );

        // Cleanup.
        let _ = pg
            .execute(
                "DELETE FROM zeroship.app_members WHERE user_id = $1",
                &[&user_id],
            )
            .await;
        let _ = pg
            .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_uuid])
            .await;
        let _ = pg
            .execute(
                "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
                &[&user_id],
            )
            .await;
        let _ = pg
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
            .await;
    });
}

fn run_db_test<F, Fut>(test: F)
where
    F: FnOnce(Client) -> Fut,
    Fut: Future<Output = ()>,
{
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skipping (no AUTH_DB_URL)");
        return;
    };
    compio::runtime::Runtime::new()
        .expect("create compio runtime")
        .block_on(async move {
            let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
            compio::runtime::spawn(async move {
                let _ = connection.run().await;
            })
            .detach();
            test(client).await;
        });
}
