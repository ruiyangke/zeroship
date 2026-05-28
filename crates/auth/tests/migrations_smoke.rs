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
