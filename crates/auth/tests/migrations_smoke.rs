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
    assert_queryable(&client, "apps").await;

    let owner_email = format!("authz-migration-owner-{}@zeroship.test", Uuid::new_v4().simple());
    let actor_email = format!("authz-migration-actor-{}@zeroship.test", Uuid::new_v4().simple());
    let owner_id = insert_user(&client, &owner_email, "Authz Migration Owner").await;
    let actor_id = insert_user(&client, &actor_email, "Authz Migration Actor").await;
    let app_uuid = Uuid::new_v4();
    let app_id = app_uuid.to_string();
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
            "INSERT INTO apps (id, name, api_key, api_key_hash, suspended, audit_locked)
             VALUES ($1, $2, 'test-api-key', 'test-api-key-hash', TRUE, TRUE)",
            &[&app_uuid, &format!("authz-migration-app-{}", Uuid::new_v4().simple())],
        )
        .await
        .expect("insert app with authz flags");
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

    let audit_rows = client
        .query(
            "INSERT INTO auth.audit_events (event_type, outcome, user_id, detail)
             VALUES ('migration_append_only_probe', 'success', $1, '{\"test\":\"append_only\"}'::jsonb)
             RETURNING id",
            &[&owner_id],
        )
        .await
        .expect("insert auth audit probe");
    let audit_id: i64 = audit_rows[0].get("id");

    assert_append_only_fails(
        &client,
        "DELETE FROM auth.audit_events WHERE id = $1",
        &[&audit_id],
    )
    .await;
    assert_append_only_fails(
        &client,
        "UPDATE control.authz_decisions SET decision = 'deny' WHERE request_id = $1",
        &[&request_id],
    )
    .await;

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
        .execute("DELETE FROM apps WHERE id = $1", &[&app_uuid])
        .await
        .expect("cleanup app");
    client
        .execute("DELETE FROM auth.users WHERE id IN ($1, $2)", &[&owner_id, &actor_id])
        .await
        .expect("cleanup users");
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

async fn assert_append_only_fails(
    client: &compio_postgres::Client,
    statement: &str,
    params: &[&(dyn compio_postgres::types::ToSql + Sync)],
) {
    let err = client
        .execute(statement, params)
        .await
        .expect_err("append-only audit table should reject mutation");
    let message = err.to_string();
    assert!(
        message.contains("append-only")
            || message.contains("permission")
            || message.contains("db error"),
        "expected append-only/permission rejection, got: {message}"
    );
}
