//! Ownership changes concurrent with an erasure transaction.

use super::{audit_detail, backdate_schedule, clear_control};
use crate::common::database::Database;
use compio_postgres::Client;
use uuid::Uuid;
use zeroship_auth::cron::account_reaper::{self, ReaperReport};
use zeroship_auth::store::users;

// ---------------------------------------------------------------------------
// The ownership rule under concurrency
// ---------------------------------------------------------------------------

/// Observe the reaper waiting for the fixture transaction's row lock.
#[allow(clippy::future_not_send)]
async fn wait_until_blocked(observer: &Client, reaper_pid: i32, blocker_pid: i32) -> bool {
    compio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let blocked: bool = observer
                .query_one(
                    "SELECT $1 = ANY(pg_blocking_pids($2))",
                    &[&blocker_pid, &reaper_pid],
                )
                .await
                .expect("observe the fixture's database lock")
                .get(0);
            if blocked {
                return;
            }
            compio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .is_ok()
}

/// TRIGGER A, and the reason the fence has to be inside the transaction.
///
/// The preflight is an HTTP round trip, so it answers BEFORE the erasure
/// transaction opens. A co-owner who departs in that window is doing something
/// legitimate - two owners were seated when their `leave` ran - and the reaper
/// then deletes the other one. Both statements commit and the organization has
/// no owner, which `zeroship_control::organizations`'s module header names as
/// the state no route can repair.
///
/// The interleaving is forced, not raced: the departure takes the organization
/// row lock and holds it, the reaper's tick parks on that same lock, and only
/// then does the departure commit. Both orders of the same pair are safe once
/// the lock is shared - this is the one where the reaper is second.
///
/// It runs the tick as the REAL `zeroship_auth` role. The fence reads
/// control-owned tables, and a re-check that raises `42501` under the role the
/// service actually connects as would be no fence at all; a suite that only
/// ever connects as `postgres` cannot tell the two apart.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn a_departure_after_the_preflight_cannot_leave_the_organization_ownerless() {
    Database::run(async |database| {
        let mut db = database.connect().await;
        let as_auth = database.connect_as_auth().await;
        let mut departing = database.connect().await;
        let observer = database.connect().await;
        let (mock, control) = clear_control().await;

        let tag = Uuid::new_v4().simple().to_string();
        let organization_id = zeroship_core::typed_id::generate("org");
        let slug = format!("acctdel-{}", &tag[..12]);
        let victim = users::create(
            &db,
            &format!("acctdel-race-victim-{tag}@zeroship.test"),
            "Victim",
            None,
        )
        .await
        .unwrap();
        let co_owner = users::create(
            &db,
            &format!("acctdel-race-peer-{tag}@zeroship.test"),
            "Co Owner",
            None,
        )
        .await
        .unwrap();
        db.execute(
            "INSERT INTO zeroship.organizations \
             (id, slug, name, billing_email, created_by) \
         VALUES ($1, $2::text::citext, $3, $4::text::citext, $5)",
            &[
                &organization_id,
                &slug,
                &"Shared",
                &format!("billing-{tag}@zeroship.test"),
                &victim.id.as_str(),
            ],
        )
        .await
        .expect("seat the organization");
        db.execute(
            "INSERT INTO zeroship.organization_members (organization_id, user_id, role) \
         VALUES ($1, $2, 'owner'), ($1, $3, 'owner')",
            &[&organization_id, &victim.id.as_str(), &co_owner.id.as_str()],
        )
        .await
        .expect("seat two owners");

        users::request_deletion(&mut db, &victim.id, account_reaper::GRACE_DAYS)
            .await
            .unwrap()
            .unwrap();
        backdate_schedule(&db, &victim.id).await;

        // The departure, in the shape `leave_organization` takes it: the
        // organization row lock FIRST, then the delete under its own owners-remain
        // predicate. Uncommitted, so the erasure has to meet it.
        let departure = departing
            .transaction()
            .await
            .expect("begin the co-owner's departure");
        departure
            .query(
                "SELECT id FROM zeroship.organizations WHERE id = $1 FOR UPDATE",
                &[&organization_id],
            )
            .await
            .expect("the departure takes the organization row lock");
        let left = departure
            .execute(
                "DELETE FROM zeroship.organization_members m \
              WHERE m.organization_id = $1 AND m.user_id = $2 \
                AND (m.role <> 'owner' \
                     OR (SELECT count(*) FROM zeroship.organization_members owners \
                          WHERE owners.organization_id = $1 AND owners.role = 'owner') > 1)",
                &[&organization_id, &co_owner.id.as_str()],
            )
            .await
            .expect("the departure runs");
        assert_eq!(
            left, 1,
            "the departure is legitimate: two owners are seated when it runs"
        );

        let reaper_pid: i32 = as_auth
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        let blocker_pid: i32 = departure
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        let control_for_tick = control.clone();
        let erasure = compio::runtime::spawn(async move {
            let mut conn = as_auth;
            account_reaper::tick(&mut conn, &control_for_tick).await
        });

        let parked = wait_until_blocked(&observer, reaper_pid, blocker_pid).await;
        departure.commit().await.expect("the co-owner has left");
        let report = erasure.await.expect("join erasure").expect("tick");

        let owners = db
            .query(
                "SELECT user_id FROM zeroship.organization_members \
              WHERE organization_id = $1 AND role = 'owner'",
                &[&organization_id],
            )
            .await
            .expect("count owners");
        let victim_row = db
            .query(
                "SELECT 1 FROM zeroship.users WHERE id = $1",
                &[&victim.id.as_str()],
            )
            .await
            .expect("read the victim");
        let details = audit_detail(&db, &victim.id, "account_erasure_failed").await;
        let asked = mock.asked();

        assert!(
            asked.contains(&victim.id.as_str().to_owned()),
            "the preflight really answered, and answered clear, before the erasure"
        );
        assert!(
            !owners.is_empty(),
            "the organization was left with NO owner - the state no route repairs"
        );
        assert_eq!(
            victim_row.len(),
            1,
            "a refused erasure leaves the account pending, not half-erased"
        );
        assert_eq!(
            report,
            ReaperReport {
                erased: 0,
                failed: 1
            }
        );
        assert_eq!(details.len(), 1, "one durable record, on this user");
        assert_eq!(
            details[0]["stage"], "ownership",
            "the in-transaction fence is selectable apart from a preflight that \
         answered no: {}",
            details[0]
        );
        // Last, because it rules on the MECHANISM rather than the outcome. The
        // assertions above can all hold on a run where the erasure simply finished
        // first; this one says the ordering was imposed by the lock, so a green is
        // evidence about the fence and not about scheduling.
        assert!(
            parked,
            "the erasure never blocked on the organization row lock, so the fence \
         is not inside the transaction"
        );
    })
    .await;
}

/// TRIGGER C, and the reason the lock is taken over SEATS rather than owner
/// seats.
///
/// Triggers A and B both need this human to already hold an owner seat, so a
/// fence that locked exactly the organizations they own closed both. Ownership
/// is also reachable by PROMOTION: `transfer_ownership` raises a sitting member
/// with `UPDATE organization_members SET role`, and a referencing-side RI
/// trigger fires only when the key columns change - so that UPDATE takes no
/// lock on `zeroship.users`, and holding the victim's row `FOR UPDATE` does not
/// serialize it.
///
/// The victim here is a DEVELOPER when the preflight answers, which is why it
/// answers clear. A fence locking only owner seats locks nothing at all for
/// this organization, the promotion commits underneath the erasure, and the
/// cascade takes the freshly-granted owner seat with it.
///
/// The interleaving is forced the same way as trigger A: the transfer takes the
/// organization row lock and holds it, the tick parks on that lock, and only
/// then does the transfer commit. So a green says the re-check saw a promotion
/// that landed after the lock was requested - which is the whole claim.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn a_promotion_after_the_preflight_cannot_leave_the_organization_ownerless() {
    Database::run(async |database| {
        let mut db = database.connect().await;
        let as_auth = database.connect_as_auth().await;
        let mut transferring = database.connect().await;
        let observer = database.connect().await;
        let (mock, control) = clear_control().await;

        let tag = Uuid::new_v4().simple().to_string();
        let organization_id = zeroship_core::typed_id::generate("org");
        let slug = format!("acctdel-{}", &tag[..12]);
        let victim = users::create(
            &db,
            &format!("acctdel-promo-victim-{tag}@zeroship.test"),
            "Victim",
            None,
        )
        .await
        .unwrap();
        let sitting_owner = users::create(
            &db,
            &format!("acctdel-promo-owner-{tag}@zeroship.test"),
            "Sitting Owner",
            None,
        )
        .await
        .unwrap();
        db.execute(
            "INSERT INTO zeroship.organizations \
             (id, slug, name, billing_email, created_by) \
         VALUES ($1, $2::text::citext, $3, $4::text::citext, $5)",
            &[
                &organization_id,
                &slug,
                &"Handover",
                &format!("billing-{tag}@zeroship.test"),
                &sitting_owner.id.as_str(),
            ],
        )
        .await
        .expect("seat the organization");
        // The victim is a DEVELOPER. This is what makes the preflight answer clear
        // and what a role-filtered lock would decline to lock.
        db.execute(
            "INSERT INTO zeroship.organization_members (organization_id, user_id, role) \
         VALUES ($1, $2, 'developer'), ($1, $3, 'owner')",
            &[
                &organization_id,
                &victim.id.as_str(),
                &sitting_owner.id.as_str(),
            ],
        )
        .await
        .expect("seat one owner and one developer");

        users::request_deletion(&mut db, &victim.id, account_reaper::GRACE_DAYS)
            .await
            .unwrap()
            .unwrap();
        backdate_schedule(&db, &victim.id).await;

        // The handover, in the shape `transfer_ownership` takes it: the
        // organization row lock FIRST, then promote the incoming owner and step the
        // outgoing one down. Uncommitted, so the erasure has to meet it.
        let transfer = transferring
            .transaction()
            .await
            .expect("begin the ownership transfer");
        transfer
            .query(
                "SELECT id FROM zeroship.organizations WHERE id = $1 FOR UPDATE",
                &[&organization_id],
            )
            .await
            .expect("the transfer takes the organization row lock");
        let promoted = transfer
            .execute(
                "UPDATE zeroship.organization_members \
                SET role = 'owner', changed_at = NOW(), changed_by = $3 \
              WHERE organization_id = $1 AND user_id = $2",
                &[
                    &organization_id,
                    &victim.id.as_str(),
                    &sitting_owner.id.as_str(),
                ],
            )
            .await
            .expect("promote the incoming owner");
        assert_eq!(promoted, 1, "the promotion is legitimate when it runs");
        transfer
            .execute(
                "UPDATE zeroship.organization_members \
                SET role = 'admin', changed_at = NOW(), changed_by = $3 \
              WHERE organization_id = $1 AND user_id = $2",
                &[
                    &organization_id,
                    &sitting_owner.id.as_str(),
                    &sitting_owner.id.as_str(),
                ],
            )
            .await
            .expect("step the outgoing owner down");

        let reaper_pid: i32 = as_auth
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        let blocker_pid: i32 = transfer
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        let control_for_tick = control.clone();
        let erasure = compio::runtime::spawn(async move {
            let mut conn = as_auth;
            account_reaper::tick(&mut conn, &control_for_tick).await
        });

        let parked = wait_until_blocked(&observer, reaper_pid, blocker_pid).await;
        transfer.commit().await.expect("the handover completed");
        let report = erasure.await.expect("join erasure").expect("tick");

        let owners = db
            .query(
                "SELECT user_id FROM zeroship.organization_members \
              WHERE organization_id = $1 AND role = 'owner'",
                &[&organization_id],
            )
            .await
            .expect("count owners");
        let victim_row = db
            .query(
                "SELECT 1 FROM zeroship.users WHERE id = $1",
                &[&victim.id.as_str()],
            )
            .await
            .expect("read the victim");
        let details = audit_detail(&db, &victim.id, "account_erasure_failed").await;
        let asked = mock.asked();

        assert!(
            asked.contains(&victim.id.as_str().to_owned()),
            "the preflight really answered, and answered clear, before the erasure"
        );
        assert!(
            !owners.is_empty(),
            "the organization was left with NO owner - a promotion the fence never \
         locked against"
        );
        assert_eq!(
            victim_row.len(),
            1,
            "a refused erasure leaves the account pending, not half-erased"
        );
        assert_eq!(
            report,
            ReaperReport {
                erased: 0,
                failed: 1
            }
        );
        assert_eq!(details.len(), 1, "one durable record, on this user");
        assert_eq!(
            details[0]["stage"], "ownership",
            "the in-transaction fence is selectable apart from a preflight that \
         answered no: {}",
            details[0]
        );
        assert!(
            parked,
            "the erasure never blocked on the organization row lock, so the lock \
         did not cover a seat the victim held"
        );
    })
    .await;
}
