//! Account-deletion / GDPR-erase lifecycle — live PG (ISS-12).
//!
//! Skipped unless a test database (`PG_TEST_URL` or the TOML overlay) is available. These drive the
//! REAL store transactions and the REAL reaper tick — no shims — so a green
//! run exercises the same code path the `/me/delete` handler and the
//! `account_reaper` cron call in production.
//!
//! Each test scopes itself with a random tag and cleans up the rows it
//! inserted.
//!
//! `account_reaper::tick` is a FLEET-WIDE due-scan: it anonymizes or
//! hard-deletes EVERY past-grace user and returns aggregate counts, so two
//! tick-driving tests each see the other's due user and the
//! `report.{anonymized, hard_deleted}` assertions break. That was stated here
//! and then guarded with a process-wide `Mutex`, which is right about the
//! mechanism and one boundary short: every run on this migration set is a
//! different PROCESS against the same database. MEASURED 2026-08-20, six copies
//! of this file started together against one database - 4 of 6 red, then 2 of
//! 6, then 0 of 6, every failure on those counts. The lease is a session
//! advisory lock, so it excludes peer runs too; see [`common::lease_sweep`].
//! The counts are FLOORS now rather than figures, because a lease excludes a
//! live peer and not a row a crashed one left past its grace, and the exact
//! claim - which branch this run's own user took - is the row check under each.

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;

use zeroship_auth::cron::account_reaper;
use zeroship_auth::store::users;

use crate::common;

#[allow(clippy::future_not_send)]
async fn pg() -> Option<Client> {
    let dsn = zeroship_core::config::test_database_url_opt()?;
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
        // Best-effort teardown. organization_accounts/payouts FK organization_id, so
        // drop those first for the anonymized creator.
        let _ = db
            .execute("DELETE FROM zeroship.payouts WHERE organization_id = $1", &[id])
            .await;
        let _ = db
            .execute(
                "DELETE FROM zeroship.organization_accounts WHERE organization_id = $1",
                &[id],
            )
            .await;
        let _ = db
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[id])
            .await;
    }
}

#[compio::test]
async fn request_marks_deletion_and_schedules() {
    let Some(mut db) = pg().await else {
        zeroship_test_support::skip("skipping account_deletion_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };
    let tag = Uuid::new_v4().simple().to_string();
    let email = format!("acctdel-req-{tag}@zeroship.test");
    let user = users::create(&db, &email, "Req User", Some("phc")).await.unwrap();

    let req = users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .expect("request_deletion")
        .expect("user existed");

    // The request returns the contact details the confirm/undo email needs.
    assert_eq!(req.email, email);

    // Re-read: deletion requested and scheduled, without changing an
    // independent administrative disable.
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
    assert!(disabled.is_none(), "request must not set an administrative disable");
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
    let Some(mut db) = pg().await else {
        return;
    };
    let tag = Uuid::new_v4().simple().to_string();
    let email = format!("acctdel-cancel-{tag}@zeroship.test");
    let user = users::create(&db, &email, "Cancel User", Some("phc")).await.unwrap();

    users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
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
    assert!(disabled.is_none(), "cancel must leave the administrative state unchanged");
    assert!(requested.is_none() && scheduled.is_none(), "cancel clears the schedule");

    cleanup(&db, &[user.id]).await;
}

#[compio::test]
async fn cancellation_preserves_an_independent_administrative_disable() {
    let Some(mut db) = pg().await else {
        return;
    };
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(
        &db,
        &format!("acctdel-disabled-{tag}@zeroship.test"),
        "Disabled User",
        Some("phc"),
    )
    .await
    .unwrap();
    db.execute(
        "UPDATE zeroship.users SET disabled_at = NOW() - INTERVAL '1 day' WHERE id = $1",
        &[&user.id],
    )
    .await
    .unwrap();

    users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    assert!(users::cancel_deletion(&db, user.id).await.unwrap());

    let disabled: bool = db
        .query_one(
            "SELECT disabled_at IS NOT NULL FROM zeroship.users WHERE id = $1",
            &[&user.id],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        disabled,
        "cancelling deletion must not erase an administrative disable"
    );

    cleanup(&db, &[user.id]).await;
}

#[compio::test]
async fn cancellation_does_not_restore_pre_deletion_app_credentials() {
    let Some(mut db) = pg().await else {
        return;
    };
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(
        &db,
        &format!("acctdel-recall-{tag}@zeroship.test"),
        "Recall User",
        Some("phc"),
    )
    .await
    .unwrap();
    let app_id = Uuid::new_v4();
    let client_id = format!("oac_acctdel_{tag}");
    // DERIVED through the production function, not invented. The deletion
    // cascade copies whatever subject it finds stored, so "seed X, assert
    // marker == X" holds for any X - including one no live token carries.
    let pairwise_sub = zeroship_core::auth::derive_pairwise(
        &zeroship_core::crypto::derive_key("account-deletion-test-salt"),
        &user.id.to_string(),
        &format!("https://{client_id}.zeroship.localhost"),
    );

    db.execute(
        "INSERT INTO zeroship.plans \
            (id, name, runtime_limits_json, assignable_by_creator) \
         VALUES ('free', 'Free', '{}'::jsonb, TRUE) \
         ON CONFLICT (id) DO NOTHING",
        &[],
    )
    .await
    .unwrap();
    // An app row needs a project, and a project needs an organization. Account
    // deletion is what this file is about, not authority, so the organization
    // is left member-less.
    let project_id = common::unowned_project(&db).await;
    db.execute(
        "INSERT INTO zeroship.apps (id, name, plan_id, project_id, organization_id) \
         SELECT $1, $2, 'free', p.id, p.organization_id \
           FROM zeroship.projects p WHERE p.id = $3",
        &[&app_id, &format!("acctdel-app-{tag}"), &project_id],
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, redirect_uris, scopes) \
         VALUES ($1, $2, $3, $4)",
        &[
            &client_id,
            &format!("Account deletion {tag}"),
            &vec![format!("https://acctdel-{tag}.test/callback")],
            &vec!["openid".to_string()],
        ],
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO zeroship.app_user_identities \
            (app_client_id, global_user_id, pairwise_sub) \
         VALUES ($1, $2, $3)",
        &[&client_id, &user.id, &pairwise_sub],
    )
    .await
    .unwrap();
    let anchor_id = Uuid::new_v4();
    db.execute(
        "INSERT INTO zeroship.app_session_anchors \
            (id, app_id, client_id, global_user_id, refresh_token_enc, \
             refresh_family_id, abs_expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, NOW() + INTERVAL '30 days')",
        &[
            &anchor_id,
            &app_id,
            &client_id,
            &user.id,
            &b"enc-refresh".to_vec(),
            &format!("rfam_{tag}"),
        ],
    )
    .await
    .unwrap();

    users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    assert!(users::cancel_deletion(&db, user.id).await.unwrap());

    // Read the marker's `sub` back rather than counting rows that match the
    // seed: a count cannot tell "no marker" from "a marker under some other
    // subject", and a marker keyed on anything but the subject the app's tokens
    // carry revokes nothing.
    let marker = db
        .query(
            "SELECT sub FROM zeroship.token_revocations WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .unwrap();
    assert_eq!(
        marker.len(),
        1,
        "deletion must durably revoke access tokens without refresh families"
    );
    let marker_sub: String = marker[0].get("sub");
    assert_eq!(
        marker_sub, pairwise_sub,
        "the deletion marker must be keyed on the per-app pairwise subject the \
         app's access tokens carry"
    );
    let platform_marker = db
        .query(
            "SELECT 1 FROM zeroship.token_revocations \
             WHERE client_id = 'zeroship-cli' AND sub = $1",
            &[&user.id.to_string()],
        )
        .await
        .unwrap();
    assert_eq!(
        platform_marker.len(),
        1,
        "deletion must durably revoke platform tokens after cancellation"
    );
    let anchor_revoked: bool = db
        .query_one(
            "SELECT revoked_at IS NOT NULL FROM zeroship.app_session_anchors \
             WHERE id = $1",
            &[&anchor_id],
        )
        .await
        .unwrap()
        .get(0);
    assert!(anchor_revoked, "deletion must durably revoke recovery anchors");

    cleanup(&db, &[user.id]).await;
    let _ = db.execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id]).await;
    let _ = db
        .execute(
            "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await;
}

#[compio::test]
async fn reaper_hard_deletes_non_billing_user_and_cascades() {
    let Some(mut db) = pg().await else {
        return;
    };
    let _reaper = common::lease_sweep(common::sweep_lock::ACCOUNT_REAPER).await;
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

    users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, user.id).await;

    let report = account_reaper::tick(&mut db).await.expect("reaper tick");
    // A FLOOR, not a figure: the lease keeps a peer run's due user out of this
    // window, but a user a crashed run left past its grace is durable in a
    // database nothing drops and this scan is fleet-wide. Which BRANCH this
    // run's own user took is the row checks below - and they, not a count of
    // one, are what says it was deleted rather than anonymized.
    assert!(report.hard_deleted >= 1, "non-billing user is hard-deleted: {report:?}");

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
    let Some(mut db) = pg().await else {
        return;
    };
    let _reaper = common::lease_sweep(common::sweep_lock::ACCOUNT_REAPER).await;
    let tag = Uuid::new_v4().simple().to_string();
    let email = format!("acctdel-creator-{tag}@zeroship.test");
    let user = users::create(&db, &email, "Creator Person", Some("phc")).await.unwrap();

    // Stripe-Connect financial history: a organization_accounts row + a payout.
    db.execute(
        "INSERT INTO zeroship.organization_accounts (organization_id, stripe_account_id) \
         VALUES ($1, $2)",
        &[&user.id, &format!("acct_{tag}")],
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO zeroship.payouts \
            (organization_id, event_id, event_type, gross_amount, platform_fee, net_amount, currency, occurred_at) \
         VALUES ($1, $2, 'payout.paid', 1000, 150, 850, 'usd', NOW())",
        &[&user.id, &format!("evt_{tag}")],
    )
    .await
    .unwrap();

    users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, user.id).await;

    let report = account_reaper::tick(&mut db).await.expect("reaper tick");
    // A floor; see `reaper_hard_deletes_non_billing_user_and_cascades`. The
    // "not deleted" half is the surviving users row asserted just below.
    assert!(
        report.anonymized >= 1,
        "creator-with-billing is anonymized, not deleted: {report:?}"
    );

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
        .query("SELECT 1 FROM zeroship.payouts WHERE organization_id = $1", &[&user.id])
        .await
        .unwrap();
    assert_eq!(payouts.len(), 1, "payout retained for legal-obligation");
    let acct = db
        .query(
            "SELECT 1 FROM zeroship.organization_accounts WHERE organization_id = $1",
            &[&user.id],
        )
        .await
        .unwrap();
    assert_eq!(acct.len(), 1, "organization_accounts retained");

    cleanup(&db, &[user.id]).await;
}

#[compio::test]
async fn reaper_sets_null_attribution_fk_pointing_at_deleted_user() {
    let Some(mut db) = pg().await else {
        return;
    };
    let _reaper = common::lease_sweep(common::sweep_lock::ACCOUNT_REAPER).await;
    let tag = Uuid::new_v4().simple().to_string();
    // `victim` is being erased; `actor` is a SECOND user whose attribution
    // columns point at `victim`. A naive hard DELETE of `victim` would be
    // blocked by these NON-cascade FKs — the reaper must SET NULL first.
    let victim = users::create(&db, &format!("acctdel-victim-{tag}@zeroship.test"), "Victim", None)
        .await
        .unwrap();

    // `oauth_clients.created_by` -> victim (a NON-cascade attribution FK).
    // This used to use `platform_admin_roles.granted_by`; that table is gone
    // with the platform staff roles, and the two FKs it left behind are the
    // ones the reaper still has to null.
    let bystander =
        users::create(&db, &format!("acctdel-bystander-{tag}@zeroship.test"), "Bystander", None)
            .await
            .unwrap();
    let client_id = format!("acctdel-client-{tag}");
    db.execute(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, redirect_uris, scopes, skip_consent, created_by, \
             refresh_allowed, token_endpoint_auth_method) \
         VALUES ($1, 'Attribution Probe', ARRAY['https://probe.zeroship.test/cb'], \
                 ARRAY['apps:read'], FALSE, $2, FALSE, 'none')",
        &[&client_id, &victim.id],
    )
    .await
    .unwrap();

    users::request_deletion(&mut db, victim.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, victim.id).await;

    let report = account_reaper::tick(&mut db).await.expect("reaper tick");
    assert!(report.hard_deleted >= 1, "victim hard-deleted (no billing): {report:?}");

    // victim gone; the bystander's attribution FK was SET NULL, not blocking.
    let victim_rows = db
        .query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&victim.id])
        .await
        .unwrap();
    assert!(victim_rows.is_empty(), "victim row deleted despite the inbound attribution FK");
    let created_by: Option<Uuid> = db
        .query_one(
            "SELECT created_by FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .unwrap()
        .get("created_by");
    assert!(created_by.is_none(), "attribution FK SET NULL");

    db.execute("DELETE FROM zeroship.oauth_clients WHERE client_id = $1", &[&client_id])
        .await
        .unwrap();
    cleanup(&db, &[victim.id, bystander.id]).await;
}

#[compio::test]
async fn reaper_skips_cancelled_request() {
    let Some(mut db) = pg().await else {
        return;
    };
    let _reaper = common::lease_sweep(common::sweep_lock::ACCOUNT_REAPER).await;
    let tag = Uuid::new_v4().simple().to_string();
    let email = format!("acctdel-skip-{tag}@zeroship.test");
    let user = users::create(&db, &email, "Skip User", Some("phc")).await.unwrap();

    users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, user.id).await;
    // Cancel clears the schedule — even though the (now-cleared) date was in
    // the past, the reaper must not touch a cancelled request.
    users::cancel_deletion(&db, user.id).await.unwrap();

    // No count assertion here, and that is the point rather than an omission:
    // "the reaper touched nobody" is a claim over the whole database, and this
    // run owns exactly one user in it. The row check below makes the same claim
    // about the only row that is this run's to speak for.
    account_reaper::tick(&mut db).await.expect("reaper tick");

    let remaining = db
        .query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .unwrap();
    assert_eq!(remaining.len(), 1, "user survives a cancelled request");

    cleanup(&db, &[user.id]).await;
}

#[compio::test]
async fn reaper_ignores_a_schedule_without_a_deletion_request() {
    let Some(mut db) = pg().await else {
        return;
    };
    let _reaper = common::lease_sweep(common::sweep_lock::ACCOUNT_REAPER).await;
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(
        &db,
        &format!("acctdel-schedule-only-{tag}@zeroship.test"),
        "Schedule Only",
        Some("phc"),
    )
    .await
    .unwrap();
    db.execute(
        "UPDATE zeroship.users \
         SET deletion_scheduled_for = NOW() - INTERVAL '1 minute' \
         WHERE id = $1",
        &[&user.id],
    )
    .await
    .unwrap();

    account_reaper::tick(&mut db).await.unwrap();

    let still_exists = db
        .query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .unwrap();
    assert_eq!(
        still_exists.len(),
        1,
        "a schedule alone is a deny state, not erasure authorization"
    );
    cleanup(&db, &[user.id]).await;
}

// ---------------------------------------------------------------------------
// Redesign regression (change 8): `user_has_financial_history` is widened to
// `organization_accounts OR invoices`. A creator with an INVOICE (the durable
// infra-billing artifact) but NO Connect account must ANONYMIZE-retain (so the
// invoice's organization_id FK target survives), NOT hard-delete. (RED before the
// widening: only organization_accounts counted, so an invoiced-but-not-Connected
// creator would be hard-deleted and the CASCADE would reap the invoice.)
// ---------------------------------------------------------------------------

#[compio::test]
async fn reaper_anonymizes_invoiced_creator_with_no_connect_account() {
    let Some(mut db) = pg().await else {
        return;
    };
    let _reaper = common::lease_sweep(common::sweep_lock::ACCOUNT_REAPER).await;
    let tag = Uuid::new_v4().simple().to_string();
    let email = format!("acctdel-inv-{tag}@zeroship.test");
    let user = users::create(&db, &email, "Invoiced Creator", Some("phc")).await.unwrap();

    // Infra-billing financial history: a organization_billing identity + a finalized
    // invoice. NO organization_accounts (this creator never used Connect).
    db.execute(
        "INSERT INTO zeroship.organization_billing (organization_id) VALUES ($1)",
        &[&user.id],
    )
    .await
    .unwrap();
    let inv_id = format!("inv_reaper_{tag}");
    db.execute(
        "INSERT INTO zeroship.invoices (id, organization_id, period, status, subtotal_cents, total_cents, finalized_at) \
         VALUES ($1, $2, date_trunc('month', NOW())::date, 'finalized', 500, 500, NOW())",
        &[&inv_id, &user.id],
    )
    .await
    .unwrap();

    users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
        .await
        .unwrap()
        .unwrap();
    backdate_schedule(&db, user.id).await;

    let report = account_reaper::tick(&mut db).await.expect("reaper tick");
    // A floor; see `reaper_hard_deletes_non_billing_user_and_cascades`. The
    // "not hard-deleted" half is the surviving users row asserted just below.
    assert!(
        report.anonymized >= 1,
        "an invoiced creator (no Connect account) is ANONYMIZED, not hard-deleted: {report:?}",
    );

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
    let _ = db.execute("DELETE FROM zeroship.organization_billing WHERE organization_id = $1", &[&user.id]).await;
    cleanup(&db, &[user.id]).await;
}
