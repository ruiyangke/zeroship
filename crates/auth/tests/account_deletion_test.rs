//! Account-deletion / GDPR-erase lifecycle — live PG (ISS-12).
//!
//! Skipped unless `AUTH_DB_URL` (or `PG_TEST_URL`) is set. These drive the
//! REAL store transactions and the REAL reaper tick — no shims — so a green
//! run exercises the same code path the `/me/delete` handler and the
//! `account_reaper` cron call in production.
//!
//! Each test scopes itself with a random tag and cleans up the rows it
//! inserted.

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;

use zeroship_auth::cron::account_reaper;
use zeroship_auth::store::users;

/// `account_reaper::tick` is a FLEET-WIDE due-scan (it anonymizes/hard-deletes
/// EVERY past-grace user, returning aggregate counts). Two reaper-tick tests run
/// concurrently each see the OTHER's due user and the `report.{anonymized,
/// hard_deleted} == 1` assertions break. Serialize the tick-driving tests with a
/// process-wide lock (poison-recovered) so each owns the fleet for its tick.
static REAPER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[allow(clippy::future_not_send)]
async fn pg() -> Option<Client> {
    let dsn = std::env::var("AUTH_DB_URL")
        .ok()
        .or_else(|| std::env::var("PG_TEST_URL").ok())?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("account_deletion test pg connection error: {e}");
        }
    })
    .detach();
    Some(client)
}

/// Force a user's scheduled erasure into the past so the reaper's due-scan
/// selects it without waiting out the 30-day window.
#[allow(clippy::future_not_send)]
async fn backdate_schedule(db: &Client, user_id: Uuid) {
    db.execute(
        "UPDATE zeroship.users \
         SET deletion_scheduled_for = NOW() - INTERVAL '1 minute' \
         WHERE id = $1",
        &[&user_id],
    )
    .await
    .expect("backdate schedule");
}

#[allow(clippy::future_not_send)]
async fn cleanup(db: &Client, ids: &[Uuid]) {
    for id in ids {
        // Best-effort teardown. creator_accounts/payouts FK creator_id, so
        // drop those first for the anonymized creator.
        let _ = db
            .execute("DELETE FROM zeroship.payouts WHERE creator_id = $1", &[id])
            .await;
        let _ = db
            .execute(
                "DELETE FROM zeroship.creator_accounts WHERE creator_id = $1",
                &[id],
            )
            .await;
        let _ = db
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[id])
            .await;
    }
}

#[compio::test]
async fn request_soft_disables_and_schedules() {
    let Some(db) = pg().await else {
        eprintln!("skipping account_deletion_test (no AUTH_DB_URL/PG_TEST_URL)");
        return;
    };
    let tag = Uuid::new_v4().simple().to_string();
    let email = format!("acctdel-req-{tag}@zeroship.test");
    let user = users::create(&db, &email, "Req User", Some("phc")).await.unwrap();

    let req = users::request_deletion(&db, user.id, account_reaper::GRACE_DAYS)
        .await
        .expect("request_deletion")
        .expect("user existed");

    // The request returns the contact details the confirm/undo email needs.
    assert_eq!(req.email, email);

    // Re-read: soft-disabled + scheduled.
    let row = db
        .query_one(
            "SELECT disabled_at, deletion_requested_at, deletion_scheduled_for, anonymized_at \
             FROM zeroship.users WHERE id = $1",
            &[&user.id],
        )
        .await
        .unwrap();
    let disabled: Option<chrono::DateTime<chrono::Utc>> = row.get("disabled_at");
    let requested: Option<chrono::DateTime<chrono::Utc>> = row.get("deletion_requested_at");
    let scheduled: Option<chrono::DateTime<chrono::Utc>> = row.get("deletion_scheduled_for");
    let anonymized: Option<chrono::DateTime<chrono::Utc>> = row.get("anonymized_at");
    assert!(disabled.is_some(), "request must soft-disable the account");
    assert!(requested.is_some(), "deletion_requested_at must be set");
    let scheduled = scheduled.expect("scheduled set");
    assert!(anonymized.is_none(), "not yet anonymized");
    // Scheduled ~30 days out.
    let delta = scheduled - requested.unwrap();
    assert!(
        delta.num_days() >= 29 && delta.num_days() <= 31,
        "scheduled ~30 days after request, got {} days",
        delta.num_days()
    );

    cleanup(&db, &[user.id]).await;
}

#[compio::test]
async fn cancel_within_grace_restores_account() {
    let Some(db) = pg().await else {
        return;
    };
    let tag = Uuid::new_v4().simple().to_string();
    let email = format!("acctdel-cancel-{tag}@zeroship.test");
    let user = users::create(&db, &email, "Cancel User", Some("phc")).await.unwrap();

    users::request_deletion(&db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    let cancelled = users::cancel_deletion(&db, user.id).await.expect("cancel");
    assert!(cancelled, "an in-flight request within grace must cancel");

    let row = db
        .query_one(
            "SELECT disabled_at, deletion_requested_at, deletion_scheduled_for, anonymized_at \
             FROM zeroship.users WHERE id = $1",
            &[&user.id],
        )
        .await
        .unwrap();
    let disabled: Option<chrono::DateTime<chrono::Utc>> = row.get("disabled_at");
    let requested: Option<chrono::DateTime<chrono::Utc>> = row.get("deletion_requested_at");
    let scheduled: Option<chrono::DateTime<chrono::Utc>> = row.get("deletion_scheduled_for");
    assert!(disabled.is_none(), "cancel must re-enable the account");
    assert!(requested.is_none() && scheduled.is_none(), "cancel clears the schedule");

    cleanup(&db, &[user.id]).await;
}

#[compio::test]
async fn reaper_hard_deletes_non_billing_user_and_cascades() {
    let Some(db) = pg().await else {
        return;
    };
    let _reaper = REAPER_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let tag = Uuid::new_v4().simple().to_string();
    let email = format!("acctdel-hard-{tag}@zeroship.test");
    let user = users::create(&db, &email, "Hard Delete", Some("phc")).await.unwrap();

    // A CASCADE dependent (federated identity) proves the cascade fires.
    db.execute(
        "INSERT INTO zeroship.federated_identities (user_id, provider, subject) \
         VALUES ($1, 'google', $2)",
        &[&user.id, &format!("sub-{tag}")],
    )
    .await
    .unwrap();

    users::request_deletion(&db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, user.id).await;

    let report = account_reaper::tick(&db).await.expect("reaper tick");
    assert_eq!(report.hard_deleted, 1, "non-billing user is hard-deleted");
    assert_eq!(report.anonymized, 0);

    let remaining = db
        .query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .unwrap();
    assert!(remaining.is_empty(), "users row is gone");
    let idents = db
        .query(
            "SELECT 1 FROM zeroship.federated_identities WHERE user_id = $1",
            &[&user.id],
        )
        .await
        .unwrap();
    assert!(idents.is_empty(), "CASCADE dependents are gone");

    cleanup(&db, &[user.id]).await;
}

#[compio::test]
async fn reaper_anonymizes_creator_with_billing_and_retains_financials() {
    let Some(db) = pg().await else {
        return;
    };
    let _reaper = REAPER_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let tag = Uuid::new_v4().simple().to_string();
    let email = format!("acctdel-creator-{tag}@zeroship.test");
    let user = users::create(&db, &email, "Creator Person", Some("phc")).await.unwrap();

    // Stripe-Connect financial history: a creator_accounts row + a payout.
    db.execute(
        "INSERT INTO zeroship.creator_accounts (creator_id, stripe_account_id) \
         VALUES ($1, $2)",
        &[&user.id, &format!("acct_{tag}")],
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO zeroship.payouts \
            (creator_id, event_id, event_type, gross_amount, platform_fee, net_amount, currency, occurred_at) \
         VALUES ($1, $2, 'payout.paid', 1000, 150, 850, 'usd', NOW())",
        &[&user.id, &format!("evt_{tag}")],
    )
    .await
    .unwrap();

    users::request_deletion(&db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, user.id).await;

    let report = account_reaper::tick(&db).await.expect("reaper tick");
    assert_eq!(report.anonymized, 1, "creator-with-billing is anonymized, not deleted");
    assert_eq!(report.hard_deleted, 0);

    // Users row STAYS, PII is gone, anonymized_at stamped.
    let row = db
        .query_one(
            "SELECT email::text AS email, name, avatar_url, password_hash, anonymized_at \
             FROM zeroship.users WHERE id = $1",
            &[&user.id],
        )
        .await
        .expect("users row must remain for the retained creator");
    let new_email: String = row.get("email");
    let name: String = row.get("name");
    let avatar: Option<String> = row.get("avatar_url");
    let pw: Option<String> = row.get("password_hash");
    let anonymized: Option<chrono::DateTime<chrono::Utc>> = row.get("anonymized_at");
    assert_ne!(new_email, email, "email must be replaced with a tombstone");
    assert!(
        !new_email.contains("acctdel-creator"),
        "original local-part must not survive"
    );
    assert!(name.is_empty() || name == "[deleted]", "name cleared, got {name:?}");
    assert!(avatar.is_none(), "avatar cleared");
    assert!(pw.is_none(), "password_hash cleared");
    assert!(anonymized.is_some(), "anonymized_at stamped");

    // Financial rows RETAINED (Art. 17(3)(b)).
    let payouts = db
        .query("SELECT 1 FROM zeroship.payouts WHERE creator_id = $1", &[&user.id])
        .await
        .unwrap();
    assert_eq!(payouts.len(), 1, "payout retained for legal-obligation");
    let acct = db
        .query(
            "SELECT 1 FROM zeroship.creator_accounts WHERE creator_id = $1",
            &[&user.id],
        )
        .await
        .unwrap();
    assert_eq!(acct.len(), 1, "creator_accounts retained");

    cleanup(&db, &[user.id]).await;
}

#[compio::test]
async fn reaper_sets_null_attribution_fk_pointing_at_deleted_user() {
    let Some(db) = pg().await else {
        return;
    };
    let _reaper = REAPER_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let tag = Uuid::new_v4().simple().to_string();
    // `victim` is being erased; `actor` is a SECOND user whose attribution
    // columns point at `victim`. A naive hard DELETE of `victim` would be
    // blocked by these NON-cascade FKs — the reaper must SET NULL first.
    let victim = users::create(&db, &format!("acctdel-victim-{tag}@zeroship.test"), "Victim", None)
        .await
        .unwrap();

    // platform_admin_roles.granted_by → victim (a NON-cascade attribution FK).
    db.execute(
        "INSERT INTO zeroship.platform_admin_roles (user_id, role, granted_by) \
         VALUES ($1, 'support', $2)",
        &[&victim.id, &victim.id],
    )
    .await
    .unwrap();
    // Make the role row's user_id a different, surviving user so CASCADE on
    // user_id doesn't remove the attribution row before we observe SET NULL.
    let bystander =
        users::create(&db, &format!("acctdel-bystander-{tag}@zeroship.test"), "Bystander", None)
            .await
            .unwrap();
    db.execute("DELETE FROM zeroship.platform_admin_roles WHERE user_id = $1", &[&victim.id])
        .await
        .unwrap();
    db.execute(
        "INSERT INTO zeroship.platform_admin_roles (user_id, role, granted_by) \
         VALUES ($1, 'support', $2)",
        &[&bystander.id, &victim.id],
    )
    .await
    .unwrap();

    users::request_deletion(&db, victim.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, victim.id).await;

    let report = account_reaper::tick(&db).await.expect("reaper tick");
    assert_eq!(report.hard_deleted, 1, "victim hard-deleted (no billing)");

    // victim gone; the bystander's attribution FK was SET NULL, not blocking.
    let victim_rows = db
        .query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&victim.id])
        .await
        .unwrap();
    assert!(victim_rows.is_empty(), "victim row deleted despite the inbound attribution FK");
    let granted_by: Option<Uuid> = db
        .query_one(
            "SELECT granted_by FROM zeroship.platform_admin_roles WHERE user_id = $1",
            &[&bystander.id],
        )
        .await
        .unwrap()
        .get("granted_by");
    assert!(granted_by.is_none(), "attribution FK SET NULL");

    db.execute("DELETE FROM zeroship.platform_admin_roles WHERE user_id = $1", &[&bystander.id])
        .await
        .unwrap();
    cleanup(&db, &[victim.id, bystander.id]).await;
}

#[compio::test]
async fn reaper_skips_cancelled_request() {
    let Some(db) = pg().await else {
        return;
    };
    let _reaper = REAPER_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let tag = Uuid::new_v4().simple().to_string();
    let email = format!("acctdel-skip-{tag}@zeroship.test");
    let user = users::create(&db, &email, "Skip User", Some("phc")).await.unwrap();

    users::request_deletion(&db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, user.id).await;
    // Cancel clears the schedule — even though the (now-cleared) date was in
    // the past, the reaper must not touch a cancelled request.
    users::cancel_deletion(&db, user.id).await.unwrap();

    let report = account_reaper::tick(&db).await.expect("reaper tick");
    assert_eq!(report.hard_deleted, 0, "cancelled request is skipped");
    assert_eq!(report.anonymized, 0);

    let remaining = db
        .query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .unwrap();
    assert_eq!(remaining.len(), 1, "user survives a cancelled request");

    cleanup(&db, &[user.id]).await;
}

// ---------------------------------------------------------------------------
// Redesign regression (change 8): `user_has_financial_history` is widened to
// `creator_accounts OR invoices`. A creator with an INVOICE (the durable
// infra-billing artifact) but NO Connect account must ANONYMIZE-retain (so the
// invoice's creator_id FK target survives), NOT hard-delete. (RED before the
// widening: only creator_accounts counted, so an invoiced-but-not-Connected
// creator would be hard-deleted and the CASCADE would reap the invoice.)
// ---------------------------------------------------------------------------

#[compio::test]
async fn reaper_anonymizes_invoiced_creator_with_no_connect_account() {
    let Some(db) = pg().await else {
        return;
    };
    let _reaper = REAPER_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let tag = Uuid::new_v4().simple().to_string();
    let email = format!("acctdel-inv-{tag}@zeroship.test");
    let user = users::create(&db, &email, "Invoiced Creator", Some("phc")).await.unwrap();

    // Infra-billing financial history: a creator_billing identity + a finalized
    // invoice. NO creator_accounts (this creator never used Connect).
    db.execute(
        "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1)",
        &[&user.id],
    )
    .await
    .unwrap();
    let inv_id = format!("inv_reaper_{tag}");
    db.execute(
        "INSERT INTO zeroship.invoices (id, creator_id, period, status, subtotal_cents, total_cents, finalized_at) \
         VALUES ($1, $2, date_trunc('month', NOW())::date, 'finalized', 500, 500, NOW())",
        &[&inv_id, &user.id],
    )
    .await
    .unwrap();

    users::request_deletion(&db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, user.id).await;

    let report = account_reaper::tick(&db).await.expect("reaper tick");
    assert_eq!(
        report.anonymized, 1,
        "an invoiced creator (no Connect account) is ANONYMIZED, not hard-deleted",
    );
    assert_eq!(report.hard_deleted, 0);

    // The users row + the invoice (FK target alive) both survive.
    let u = db
        .query("SELECT anonymized_at FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .unwrap();
    assert_eq!(u.len(), 1, "users row retained (anonymize-in-place)");
    assert!(
        u[0].get::<_, Option<chrono::DateTime<chrono::Utc>>>("anonymized_at").is_some(),
        "anonymized_at stamped",
    );
    let inv = db
        .query("SELECT 1 FROM zeroship.invoices WHERE id = $1", &[&inv_id])
        .await
        .unwrap();
    assert_eq!(inv.len(), 1, "the invoice is RETAINED (Art. 17(3)(b))");

    // Teardown: drop the billing rows then the user.
    let _ = db.execute("DELETE FROM zeroship.invoices WHERE id = $1", &[&inv_id]).await;
    let _ = db.execute("DELETE FROM zeroship.creator_billing WHERE creator_id = $1", &[&user.id]).await;
    cleanup(&db, &[user.id]).await;
}
