//! Deletion requests and undo credentials.

use crate::common;
use crate::common::database::Database;
use uuid::Uuid;
use zeroship_auth::cron::account_reaper::{self};
use zeroship_auth::identity::deletion_cancel;
use zeroship_auth::store::users;

// ---------------------------------------------------------------------------
// The request, and the undo credential it mints
// ---------------------------------------------------------------------------

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn request_marks_deletion_schedules_and_mints_one_undo_token() {
    Database::run(async |database| {
        let mut db = database.connect().await;
        let tag = Uuid::new_v4().simple().to_string();
        let email = format!("acctdel-req-{tag}@zeroship.test");
        let user = users::create(&db, &email, "Req User", Some("phc"))
            .await
            .unwrap();

        let req = users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
            .await
            .expect("request_deletion")
            .expect("user existed");

        // The request returns the contact details the confirm/undo email needs.
        assert_eq!(req.email, email);
        assert!(
            !req.cancel_token.is_empty(),
            "the undo token is minted here"
        );

        // Re-read: deletion requested and scheduled, without changing an
        // independent administrative disable.
        let row = db
            .query_one(
                "SELECT disabled_at, deletion_requested_at, deletion_scheduled_for \
             FROM zeroship.users WHERE id = $1",
                &[&user.id],
            )
            .await
            .unwrap();
        let disabled: Option<chrono::DateTime<chrono::Utc>> = row.get("disabled_at");
        let requested: Option<chrono::DateTime<chrono::Utc>> = row.get("deletion_requested_at");
        let scheduled: Option<chrono::DateTime<chrono::Utc>> = row.get("deletion_scheduled_for");
        assert!(
            disabled.is_none(),
            "request must not set an administrative disable"
        );
        assert!(requested.is_some(), "deletion_requested_at must be set");
        let scheduled = scheduled.expect("scheduled set");
        assert_eq!(
            scheduled - requested.unwrap(),
            chrono::Duration::days(account_reaper::GRACE_DAYS),
            "the schedule must honor the requested grace interval"
        );

        // Exactly ONE live undo token, and it expires WITH the window rather than
        // on a TTL of its own. A token outliving the schedule would let someone
        // "cancel" an account the reaper already erased; one expiring early would
        // shorten the window the email promises.
        let tokens = db
            .query(
                "SELECT expires_at FROM zeroship.magic_links \
             WHERE user_id = $1 AND purpose = 'deletion_cancel' AND consumed_at IS NULL",
                &[&user.id],
            )
            .await
            .unwrap();
        assert_eq!(tokens.len(), 1, "one live undo token per pending request");
        assert_eq!(
            tokens[0].get::<_, chrono::DateTime<chrono::Utc>>("expires_at"),
            scheduled,
            "the undo window IS the grace window"
        );
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn the_emailed_token_cancels_within_grace_and_only_once() {
    Database::run(async |database| {
        let mut db = database.connect().await;
        let tag = Uuid::new_v4().simple().to_string();
        let email = format!("acctdel-cancel-{tag}@zeroship.test");
        let user = users::create(&db, &email, "Cancel User", Some("phc"))
            .await
            .unwrap();

        let req = users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
            .await
            .unwrap()
            .unwrap();
        let cancelled = deletion_cancel::redeem(&db, &req.cancel_token)
            .await
            .expect("redeem")
            .expect("an in-flight request within grace must cancel");
        assert_eq!(cancelled.user_id, user.id);

        let row = db
            .query_one(
                "SELECT disabled_at, deletion_requested_at, deletion_scheduled_for \
             FROM zeroship.users WHERE id = $1",
                &[&user.id],
            )
            .await
            .unwrap();
        let disabled: Option<chrono::DateTime<chrono::Utc>> = row.get("disabled_at");
        let requested: Option<chrono::DateTime<chrono::Utc>> = row.get("deletion_requested_at");
        let scheduled: Option<chrono::DateTime<chrono::Utc>> = row.get("deletion_scheduled_for");
        assert!(
            disabled.is_none(),
            "cancel must leave the administrative state unchanged"
        );
        assert!(
            requested.is_none() && scheduled.is_none(),
            "cancel clears the schedule"
        );

        // Single use. The same token presented again is not a second cancel, and
        // (the control that makes this claim mean something) it is refused even
        // though a fresh deletion request is now in flight.
        users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
            .await
            .unwrap()
            .unwrap();
        assert!(
            deletion_cancel::redeem(&db, &req.cancel_token)
                .await
                .expect("redeem")
                .is_none(),
            "a spent token must not cancel a later request"
        );
        let still_pending: bool = db
            .query_one(
                "SELECT deletion_requested_at IS NOT NULL FROM zeroship.users WHERE id = $1",
                &[&user.id],
            )
            .await
            .unwrap()
            .get(0);
        assert!(
            still_pending,
            "the second request is untouched by the spent token"
        );
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reissuing_a_request_supersedes_the_previous_undo_token() {
    Database::run(async |database| {
        let mut db = database.connect().await;
        let tag = Uuid::new_v4().simple().to_string();
        let user = users::create(
            &db,
            &format!("acctdel-reissue-{tag}@zeroship.test"),
            "Reissue User",
            Some("phc"),
        )
        .await
        .unwrap();

        let first = users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
            .await
            .unwrap()
            .unwrap();
        let second = users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(first.cancel_token, second.cancel_token);
        // The link in the OLDER message stops working the moment a newer one is
        // issued; otherwise two live undo credentials exist for one window.
        assert!(
            deletion_cancel::redeem(&db, &first.cancel_token)
                .await
                .expect("redeem")
                .is_none(),
            "the superseded token must not cancel"
        );
        assert!(
            deletion_cancel::redeem(&db, &second.cancel_token)
                .await
                .expect("redeem")
                .is_some(),
            "the current token must cancel"
        );
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn a_token_past_the_grace_window_is_refused() {
    Database::run(async |database| {
        let mut db = database.connect().await;
        let tag = Uuid::new_v4().simple().to_string();
        let user = users::create(
            &db,
            &format!("acctdel-expired-{tag}@zeroship.test"),
            "Expired Token",
            Some("phc"),
        )
        .await
        .unwrap();
        let req = users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
            .await
            .unwrap()
            .unwrap();
        db.execute(
            "UPDATE zeroship.magic_links SET expires_at = NOW() - INTERVAL '1 minute' \
         WHERE user_id = $1 AND purpose = 'deletion_cancel'",
            &[&user.id],
        )
        .await
        .unwrap();

        assert!(
            deletion_cancel::redeem(&db, &req.cancel_token)
                .await
                .expect("redeem")
                .is_none(),
            "an expired undo token must not cancel"
        );
        let still_pending: bool = db
            .query_one(
                "SELECT deletion_requested_at IS NOT NULL FROM zeroship.users WHERE id = $1",
                &[&user.id],
            )
            .await
            .unwrap()
            .get(0);
        assert!(still_pending);
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn cancellation_preserves_an_independent_administrative_disable() {
    Database::run(async |database| {
        let mut db = database.connect().await;
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

        let req = users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
            .await
            .unwrap()
            .unwrap();
        assert!(deletion_cancel::redeem(&db, &req.cancel_token)
            .await
            .unwrap()
            .is_some());

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
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn cancellation_does_not_restore_pre_deletion_app_credentials() {
    Database::run(async |database| {
        let mut db = database.connect().await;
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

        let req = users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
            .await
            .unwrap()
            .unwrap();
        assert!(deletion_cancel::redeem(&db, &req.cancel_token)
            .await
            .unwrap()
            .is_some());

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
        assert!(
            anchor_revoked,
            "deletion must durably revoke recovery anchors"
        );
    })
    .await;
}
