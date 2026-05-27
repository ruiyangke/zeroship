//! Migration smoke test — only runs if `AUTH_DB_URL` is set.
//!
//! See `crates/compio-postgres/tests/integration.rs` for the harness pattern:
//! `connect(...)` returns `(Client, Connection)` and the `Connection` future
//! must be spawned and detached on the compio runtime or queries hang.

use compio_postgres::{connect, NoTls};
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

    let rows = client
        .query("SELECT to_regclass('auth.users')::text AS t", &[])
        .await
        .expect("query");
    let table: Option<String> = rows[0].get("t");
    assert_eq!(table.as_deref(), Some("auth.users"));
}

#[compio::test]
async fn legacy_migration_no_op_on_clean_db() {
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skip");
        return;
    };
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("conn err: {e}");
        }
    })
    .detach();

    // Ensure no legacy table.
    client
        .execute("DROP TABLE IF EXISTS public.auth_users CASCADE", &[])
        .await
        .ok();
    client
        .execute("DROP TABLE IF EXISTS public.auth_app_consents CASCADE", &[])
        .await
        .ok();
    client
        .execute("DROP TABLE IF EXISTS public.auth_sessions CASCADE", &[])
        .await
        .ok();

    // Run migration — should succeed without error.
    migrations::migrate(&client).await.expect("migrate clean db");

    // Sanity: auth.users still exists.
    let row = client
        .query_one("SELECT to_regclass('auth.users')::text AS t", &[])
        .await
        .expect("regclass");
    let table: Option<String> = row.get("t");
    assert_eq!(table.as_deref(), Some("auth.users"));
}

#[compio::test]
async fn legacy_migration_moves_rows() {
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skip");
        return;
    };
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("conn err: {e}");
        }
    })
    .detach();

    // Run main migrations FIRST so the new schema exists.
    migrations::migrate(&client).await.expect("migrate");

    // Seed a legacy table.
    client
        .execute(
            "CREATE TABLE IF NOT EXISTS public.auth_users (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                email TEXT UNIQUE,
                name TEXT,
                avatar_url TEXT,
                password_hash TEXT,
                email_verified BOOLEAN DEFAULT FALSE,
                created_at TIMESTAMPTZ DEFAULT NOW(),
                last_login TIMESTAMPTZ
            )",
            &[],
        )
        .await
        .expect("create legacy");

    let unique_email = format!("legacy-{}@test", uuid::Uuid::new_v4().simple());
    client
        .execute(
            "INSERT INTO public.auth_users (email, name, email_verified) VALUES ($1, $2, TRUE)",
            &[&unique_email, &"Legacy User"],
        )
        .await
        .expect("seed legacy");

    // Now run the legacy-aware migration.
    migrations::migrate_legacy_auth_users(&client)
        .await
        .expect("legacy migrate");

    // public.auth_users should be gone.
    let row = client
        .query_one("SELECT to_regclass('public.auth_users')::text AS t", &[])
        .await
        .expect("regclass");
    let table: Option<String> = row.get("t");
    assert!(table.is_none(), "public.auth_users must be dropped");

    // The row should be in auth.users.
    let row = client
        .query_one(
            "SELECT name, email_verified_at IS NOT NULL AS verified \
             FROM auth.users WHERE email = $1::citext",
            &[&unique_email],
        )
        .await
        .expect("find migrated user");
    let name: String = row.get("name");
    let verified: bool = row.get("verified");
    assert_eq!(name, "Legacy User");
    assert!(verified, "verified flag should map to email_verified_at NOT NULL");

    // Cleanup.
    client
        .execute(
            "DELETE FROM auth.users WHERE email = $1::citext",
            &[&unique_email],
        )
        .await
        .ok();
}
