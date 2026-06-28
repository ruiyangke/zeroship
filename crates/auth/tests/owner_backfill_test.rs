//! Regression test for the `V0031__app_members_owner_backfill` migration
//! (red-team round-2 finding 2.0 / 6.1).
//!
//! The backfill repairs apps that predate `create_app`'s owner binding by
//! promoting a qualifying SOLE member to `owner`. Finding 2.0 showed the
//! original body would silently escalate a *delegated* lone editor/viewer
//! (one added BY a different principal) to owner via `DO UPDATE SET
//! role='owner'`. The fix adds a self-originated guard so only a
//! self-created sole member (`added_by IS NULL` or `added_by = user_id`)
//! is promoted; a delegated sole member keeps its assigned role.
//!
//! This test DRIVES THE PRODUCTION MIGRATION SQL: it reads the exact INSERT
//! statement out of `db/migrations/V0031__app_members_owner_backfill.sql`
//! (the same text the zeroship-migrate engine runs) rather than re-typing it,
//! so the assertions track whatever ships. It seeds the scenario rows itself
//! (production path:
//! hand-shaped `app_members` rows the same way a pre-binding DB would have),
//! runs the real statement inside a transaction, asserts, and ROLLS BACK so the
//! live DB is left untouched.
//!
//! Skipped unless `AUTH_DB_URL` is set.

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;

async fn pg() -> Option<Client> {
    let dsn = std::env::var("AUTH_DB_URL").ok()?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("owner_backfill_test pg connection error: {e}");
        }
    })
    .detach();
    Some(client)
}

/// Pull the single `INSERT INTO zeroship.app_members ... ;` statement out of the
/// production migration file, stripping `--` comment lines.
/// This is the *exact* SQL the zeroship-migrate engine executes for the backfill.
fn production_backfill_sql() -> String {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../db/migrations/V0031__app_members_owner_backfill.sql"
    );
    let raw = std::fs::read_to_string(path).expect("read migration file");

    // Keep only the body lines (drop the `--validCheckSum` directive and plain
    // `--` comments). The body is one statement terminated by `;`.
    let body: String = raw
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");

    let start = body
        .find("INSERT INTO zeroship.app_members")
        .expect("changeset must contain the backfill INSERT");
    let after = &body[start..];
    let end = after
        .find(';')
        .expect("backfill INSERT must be terminated with ';'");
    after[..=end].to_string()
}

/// Role of a given `(app_id, user_id)` member row, or `None` if absent.
async fn role_of(client: &Client, app_id: Uuid, user_id: Uuid) -> Option<String> {
    let rows = client
        .query(
            "SELECT role FROM zeroship.app_members WHERE app_id = $1 AND user_id = $2",
            &[&app_id, &user_id],
        )
        .await
        .expect("query role");
    rows.first().map(|r| r.get::<_, String>("role"))
}

/// Number of member rows for an app.
async fn member_count(client: &Client, app_id: Uuid) -> i64 {
    let rows = client
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.app_members WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .expect("count members");
    rows.first().map(|r| r.get::<_, i64>("n")).unwrap_or(0)
}

#[compio::test]
async fn backfill_does_not_promote_delegated_lone_editor_but_does_repair_self_member() {
    let Some(client) = pg().await else {
        eprintln!("skipping owner_backfill_test (no AUTH_DB_URL)");
        return;
    };

    let backfill = production_backfill_sql();

    // Distinct ids so concurrent runs never collide.
    let editor = Uuid::new_v4();
    let creator = Uuid::new_v4();
    let selfmember = Uuid::new_v4();
    let app_delegated = Uuid::new_v4();
    let app_self = Uuid::new_v4();

    // Everything inside one txn; ROLLBACK at the end leaves the live DB clean.
    client.execute("BEGIN", &[]).await.expect("begin");

    let editor_email = format!("{editor}@backfill.test");
    let creator_email = format!("{creator}@backfill.test");
    let self_email = format!("{selfmember}@backfill.test");
    let app_delegated_name = format!("backfill-delegated-{app_delegated}");
    let app_self_name = format!("backfill-self-{app_self}");
    let key_d = format!("k-{app_delegated}");
    let key_s = format!("k-{app_self}");

    let seed = async {
        client
            .execute(
                "INSERT INTO zeroship.users (id, email, name) VALUES \
                 ($1, $4, 'editor'), \
                 ($2, $5, 'creator'), \
                 ($3, $6, 'self')",
                &[
                    &editor,
                    &creator,
                    &selfmember,
                    &editor_email,
                    &creator_email,
                    &self_email,
                ],
            )
            .await?;
        client
            .execute(
                "INSERT INTO zeroship.apps (id, name, api_key) VALUES \
                 ($1, $3, $5), \
                 ($2, $4, $6)",
                &[
                    &app_delegated,
                    &app_self,
                    &app_delegated_name,
                    &app_self_name,
                    &key_d,
                    &key_s,
                ],
            )
            .await?;
        // App 1: owner-less, SOLE member is an editor ADDED BY a distinct
        // creator (delegated). Pre-fix this row was escalated to 'owner'.
        client
            .execute(
                "INSERT INTO zeroship.app_members (app_id, user_id, role, added_by) \
                 VALUES ($1, $2, 'editor', $3)",
                &[&app_delegated, &editor, &creator],
            )
            .await?;
        // App 2: owner-less, SOLE member is SELF-originated (added_by = self),
        // role editor. This is the legitimate repair target.
        client
            .execute(
                "INSERT INTO zeroship.app_members (app_id, user_id, role, added_by) \
                 VALUES ($1, $2, 'editor', $2)",
                &[&app_self, &selfmember],
            )
            .await
    };
    if let Err(e) = seed.await {
        let _ = client.execute("ROLLBACK", &[]).await;
        panic!("seed failed: {e:?}");
    }

    // Drive the REAL migration statement.
    client
        .execute(backfill.as_str(), &[])
        .await
        .expect("run production backfill");

    let delegated_role = role_of(&client, app_delegated, editor).await;
    let self_role = role_of(&client, app_self, selfmember).await;

    // Re-run to prove idempotency (a second migration run is a no-op).
    client
        .execute(backfill.as_str(), &[])
        .await
        .expect("re-run production backfill");
    let delegated_role_2 = role_of(&client, app_delegated, editor).await;
    let self_role_2 = role_of(&client, app_self, selfmember).await;
    let delegated_count = member_count(&client, app_delegated).await;
    let self_count = member_count(&client, app_self).await;

    client.execute("ROLLBACK", &[]).await.expect("rollback");

    // --- over-grant guard (finding 2.0): delegated lone editor is NOT promoted.
    assert_eq!(
        delegated_role.as_deref(),
        Some("editor"),
        "OVER-GRANT: a delegated lone editor (added_by != user_id) of an \
         owner-less app was promoted to '{:?}' by the backfill",
        delegated_role
    );

    // --- legitimate repair preserved (finding 6.1): self-member becomes owner.
    assert_eq!(
        self_role.as_deref(),
        Some("owner"),
        "REGRESSION: an owner-less self-originated sole member should be \
         promoted to owner, got {:?}",
        self_role
    );

    // --- idempotency: a second run changes nothing and creates no extra rows.
    assert_eq!(
        delegated_role_2.as_deref(),
        Some("editor"),
        "idempotency: delegated editor role changed on re-run"
    );
    assert_eq!(
        self_role_2.as_deref(),
        Some("owner"),
        "idempotency: self-member role changed on re-run"
    );
    assert_eq!(delegated_count, 1, "delegated app gained a phantom member row");
    assert_eq!(self_count, 1, "self app gained a phantom member row");
}
