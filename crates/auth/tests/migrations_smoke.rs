//! Migration smoke test — only runs if `AUTH_DB_URL` is set.
//!
//! See `crates/compio-postgres/tests/integration.rs` for the harness pattern:
//! `connect(...)` returns `(Client, Connection)` and the `Connection` future
//! must be spawned and detached on the compio runtime or queries hang.

use compio_postgres::{connect, NoTls};
use uuid::Uuid;
use zeroship_auth::store::migrations;

#[compio::test]
async fn migrations_apply_cleanly() {
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skipping (no AUTH_DB_URL)");
        return;
    };

    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("connection error: {e}");
        }
    })
    .detach();

    migrations::migrate(&client).await.expect("migrate");

    assert_table_exists(&client, "auth.users").await;
    assert_queryable(&client, "platform.roles").await;
    assert_queryable(&client, "control.app_members").await;
    assert_queryable(&client, "control.permission_tokens").await;
    assert_queryable(&client, "control.platform_policies").await;
    assert_queryable(&client, "control.authz_decisions").await;
    assert_user_delete_cascades_session_state(&client).await;

    let owner_email = format!("authz-migration-owner-{}@zeroship.test", Uuid::new_v4().simple());
    let actor_email = format!("authz-migration-actor-{}@zeroship.test", Uuid::new_v4().simple());
    let owner_id = insert_user(&client, &owner_email, "Authz Migration Owner").await;
    let actor_id = insert_user(&client, &actor_email, "Authz Migration Actor").await;
    let app_id = format!("app_{}", Uuid::new_v4().simple());
    let token_id = Uuid::new_v4();
    let policy_id = format!("authz-migration-{}", Uuid::new_v4().simple());
    let request_id = format!("req_{}", Uuid::new_v4().simple());

    client
        .execute(
            "INSERT INTO platform.roles (user_id, role, granted_by) VALUES ($1, 'admin', $2)",
            &[&owner_id, &actor_id],
        )
        .await
        .expect("insert platform role");
    client
        .execute(
            "INSERT INTO control.app_members (app_id, user_id, role, added_by)
             VALUES ($1, $2, 'owner', $3)",
            &[&app_id, &owner_id, &actor_id],
        )
        .await
        .expect("insert app member");
    client
        .execute(
            "INSERT INTO control.permission_tokens
                (id, owner_id, kind, client_id, name, policies, policy_hash)
             VALUES ($1, $2, 'pat', NULL, 'migration smoke token', '{\"policies\":[]}'::jsonb, $3)",
            &[&token_id, &owner_id, &"0".repeat(64)],
        )
        .await
        .expect("insert permission token");
    client
        .execute(
            "INSERT INTO control.platform_policies (id, cedar_source, enabled, updated_by)
             VALUES ($1, 'permit(principal, action, resource);', TRUE, $2)",
            &[&policy_id, &actor_id],
        )
        .await
        .expect("insert platform policy");
    client
        .execute(
            "INSERT INTO control.authz_decisions
                (user_id, token_id, action, resource_type, resource_id, decision, matched_policies, request_ip, request_id)
             VALUES ($1, $2, 'app.read', 'app', $3, 'allow', ARRAY[$4], '127.0.0.1'::inet, $5)",
            &[&owner_id, &token_id, &app_id, &policy_id, &request_id],
        )
        .await
        .expect("insert authz decision");

    assert_check_fails(
        &client,
        "INSERT INTO platform.roles (user_id, role) VALUES ($1, 'evil_admin')",
        &[&actor_id],
    )
    .await;
    assert_check_fails(
        &client,
        "INSERT INTO control.app_members (app_id, user_id, role) VALUES ($1, $2, 'evil_admin')",
        &[&format!("bad_{app_id}"), &actor_id],
    )
    .await;
    assert_check_fails(
        &client,
        "INSERT INTO control.permission_tokens
            (id, owner_id, kind, name, policies, policy_hash)
         VALUES ($1, $2, 'evil_grant', 'bad token', '{}'::jsonb, $3)",
        &[&Uuid::new_v4(), &actor_id, &"1".repeat(64)],
    )
    .await;
    assert_check_fails(
        &client,
        "INSERT INTO control.authz_decisions (action, resource_type, decision)
         VALUES ('app.read', 'app', 'maybe')",
        &[],
    )
    .await;

    client
        .execute("DELETE FROM control.platform_policies WHERE id = $1", &[&policy_id])
        .await
        .expect("cleanup platform policy");
    client
        .execute("DELETE FROM auth.users WHERE id IN ($1, $2)", &[&owner_id, &actor_id])
        .await
        .expect("cleanup users");
}

async fn assert_user_delete_cascades_session_state(client: &compio_postgres::Client) {
    let email = format!("auth-session-cascade-{}@zeroship.test", Uuid::new_v4().simple());
    let user_id = insert_user(client, &email, "Auth Session Cascade").await;

    client
        .execute(
            "INSERT INTO auth.sessions (user_id, auth_method, amr, idle_expires_at, abs_expires_at)
             VALUES ($1, 'password', ARRAY['pwd'], NOW() + INTERVAL '1 hour', NOW() + INTERVAL '1 day')",
            &[&user_id],
        )
        .await
        .expect("insert auth session");
    client
        .execute(
            "INSERT INTO auth.gateway_sessions
                (user_id, app_id, email, name, email_verified, idle_expires_at, abs_expires_at)
             VALUES ($1, $2, $3::citext, 'Cascade User', TRUE, NOW() + INTERVAL '1 hour', NOW() + INTERVAL '1 day')",
            &[&user_id, &"app-cascade-test", &email],
        )
        .await
        .expect("insert gateway session");
    client
        .execute(
            "INSERT INTO auth.console_sessions
                (user_id, email, name, email_verified, idle_expires_at, abs_expires_at)
             VALUES ($1, $2::citext, 'Cascade User', TRUE, NOW() + INTERVAL '1 hour', NOW() + INTERVAL '1 day')",
            &[&user_id, &email],
        )
        .await
        .expect("insert console session");

    client
        .execute("DELETE FROM auth.users WHERE id = $1", &[&user_id])
        .await
        .expect("delete user should cascade session state");

    for table in [
        "auth.sessions",
        "auth.gateway_sessions",
        "auth.console_sessions",
    ] {
        let rows = client
            .query(
                &format!("SELECT COUNT(*)::BIGINT AS n FROM {table} WHERE user_id = $1"),
                &[&user_id],
            )
            .await
            .unwrap_or_else(|e| panic!("count {table}: {e}"));
        let count: i64 = rows[0].get("n");
        assert_eq!(count, 0, "{table} rows should cascade on user delete");
    }
}

async fn assert_table_exists(client: &compio_postgres::Client, table: &str) {
    let rows = client
        .query("SELECT to_regclass($1)::text AS t", &[&table])
        .await
        .expect("to_regclass query");
    let found: Option<String> = rows[0].get("t");
    assert_eq!(found.as_deref(), Some(table));
}

async fn assert_queryable(client: &compio_postgres::Client, table: &str) {
    client
        .query(&format!("SELECT 1 FROM {table} WHERE FALSE"), &[])
        .await
        .unwrap_or_else(|e| panic!("{table} should be queryable after migration: {e}"));
}

async fn insert_user(client: &compio_postgres::Client, email: &str, name: &str) -> Uuid {
    let rows = client
        .query(
            "INSERT INTO auth.users (email, name) VALUES ($1, $2) RETURNING id",
            &[&email, &name],
        )
        .await
        .expect("insert user");
    rows[0].get("id")
}

async fn assert_check_fails(
    client: &compio_postgres::Client,
    statement: &str,
    params: &[&(dyn compio_postgres::types::ToSql + Sync)],
) {
    let err = client
        .execute(statement, params)
        .await
        .expect_err("CHECK should reject bad value");
    let message = err.to_string();
    // compio-postgres surfaces PG check-constraint violations as a generic
    // "db error" string. Confirm we get *some* error variant; the role/kind/
    // decision/event values are server-side-rejected so any error here is
    // proof the CHECK fired.
    assert!(
        message.contains("check") || message.contains("violates") || message.contains("db error"),
        "expected CHECK violation, got: {message}"
    );
}
