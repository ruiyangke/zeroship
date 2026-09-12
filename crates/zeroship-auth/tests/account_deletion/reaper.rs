//! Erasure, retained records, and refused reaper work.

use super::{audit_detail, backdate_schedule, clear_control};
use crate::common;
use crate::common::database::Database;
use crate::common::mock_control::{Answer, MockControl};
use uuid::Uuid;
use zeroship_auth::cron::account_reaper::{self, ControlAccess, ReaperReport};
use zeroship_auth::identity::deletion_cancel;
use zeroship_auth::store::users;

// ---------------------------------------------------------------------------
// The reaper
// ---------------------------------------------------------------------------

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reaper_erases_a_due_user_and_cascades() {
    Database::run(async |database| {
        let mut db = database.connect().await;
        let (_mock, control) = clear_control().await;
        let tag = Uuid::new_v4().simple().to_string();
        let email = format!("acctdel-hard-{tag}@zeroship.test");
        let user = users::create(&db, &email, "Hard Delete", Some("phc"))
            .await
            .unwrap();

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

        let report = account_reaper::tick(&mut db, &control)
            .await
            .expect("reaper tick");
        assert_eq!(
            report,
            ReaperReport {
                erased: 1,
                failed: 0
            }
        );

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
    })
    .await;
}

/// A MONEY RECORD OUTLIVES THE HUMAN IT NAMES. GDPR Art. 17(3)(b): the erasure
/// right yields to a legal obligation, and an invoice is one.
///
/// THIS ASSERTION WAS DELETED AND NOTHING REPLACED IT. It used to ride on the
/// anonymize branch, and when the organization work replaced the mechanism that
/// enforced retention - `invoices` stopped naming a user at all and now reaches
/// one only as `invoices -> organization_billing -> organizations` - the guard
/// went out with the code it was attached to. The PROPERTY survived the change;
/// only its witness did. A tree-wide search for Art. 17, `retained`, or
/// `legal-obligation` found nothing, so this was the one legally-motivated
/// property of the subsystem with no test standing behind it.
///
/// What makes it hold now is structural rather than intentional, which is
/// exactly why it needs a witness: the erasure deletes the `users` row, and the
/// only edge from an organization back to a human is
/// `organizations.personal_owner_id`, which is `ON DELETE SET NULL`. The
/// invoice hangs off the ORGANIZATION and never sees the delete. A future change
/// that made that edge `CASCADE` - the tempting spelling, since it reads as
/// "clean up after the person" - would destroy billing history and pass every
/// other test in this file.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn erasing_a_sole_owner_retains_the_organizations_invoice() {
    Database::run(async |database| {
        let mut db = database.connect().await;
        let (_mock, control) = clear_control().await;
        let tag = Uuid::new_v4().simple().to_string();
        let user = users::create(
            &db,
            &format!("acctdel-retain-{tag}@zeroship.test"),
            "Billed Human",
            None,
        )
        .await
        .unwrap();

        // A PERSONAL organization: the shape where the human and the billed party
        // are as close as this model allows, so it is the hardest case for
        // retention rather than the easiest.
        let organization_id = zeroship_core::typed_id::generate("org");
        db.execute(
            "INSERT INTO zeroship.organizations \
             (id, slug, name, billing_email, personal_owner_id, created_by) \
         VALUES ($1, $2, 'Retention Fixture', $3::citext, $4, $4)",
            &[
                &organization_id,
                &format!("retain-{tag}"),
                &format!("acctdel-retain-{tag}@zeroship.test"),
                &user.id,
            ],
        )
        .await
        .unwrap();
        db.execute(
            "INSERT INTO zeroship.organization_billing (organization_id) VALUES ($1)",
            &[&organization_id],
        )
        .await
        .unwrap();
        let invoice_id = zeroship_core::typed_id::generate("inv");
        db.execute(
            "INSERT INTO zeroship.invoices \
             (id, organization_id, period, status, currency, \
              subtotal_cents, credit_cents, tax_cents, total_cents) \
         VALUES ($1, $2, DATE '2026-01-01', 'finalized', 'usd', 1000, 0, 0, 1000)",
            &[&invoice_id, &organization_id],
        )
        .await
        .unwrap();

        users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
            .await
            .unwrap()
            .unwrap();
        backdate_schedule(&db, user.id).await;
        account_reaper::tick(&mut db, &control)
            .await
            .expect("reaper tick");

        let gone = db
            .query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user.id])
            .await
            .unwrap();
        assert!(gone.is_empty(), "the human is erased");

        let invoice = db
            .query(
                "SELECT total_cents FROM zeroship.invoices WHERE id = $1",
                &[&invoice_id],
            )
            .await
            .unwrap();
        assert_eq!(
            invoice.len(),
            1,
            "the invoice is RETAINED (GDPR Art. 17(3)(b)): erasing the human must not \
         destroy the money record that names their organization"
        );

        // The organization survives too, with its pointer cleared rather than the
        // row removed - the personal-to-shared conversion this model already
        // describes, reached here by erasure instead of by choice.
        let organization = db
            .query(
                "SELECT personal_owner_id FROM zeroship.organizations WHERE id = $1",
                &[&organization_id],
            )
            .await
            .unwrap();
        assert_eq!(organization.len(), 1, "the organization outlives its owner");
        let owner: Option<Uuid> = organization[0].get("personal_owner_id");
        assert!(
            owner.is_none(),
            "the pointer to the erased human is cleared"
        );
    })
    .await;
}

/// The binding for `db/migrations-ts/20260907000000_user_erasure_edges.ts`.
///
/// Every one of these references BLOCKED a hard delete before that migration -
/// read out of `pg_constraint` as `confdeltype` in (`a`, `r`) - and only
/// `oauth_clients.created_by` was on the reaper's hand-maintained list. Three of
/// them (`identity_links`, `principal_grants`, `app_schema_applies`) were also
/// `NOT NULL`, so the `SET NULL` that list performed was not a spelling they
/// accepted: a creator who had signed in through the CLI or deployed a schema
/// could not be erased at all.
///
/// Seed one of each, then erase. What proves the migration is not that the
/// DELETE succeeds but WHICH way each dependent went: identity edges gone,
/// attribution edges surviving with a NULL.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reaper_erases_a_user_holding_every_previously_blocking_reference() {
    Database::run(async |database| {
        let mut db = database.connect().await;
        let (_mock, control) = clear_control().await;
        let tag = Uuid::new_v4().simple().to_string();
        let victim = users::create(
            &db,
            &format!("acctdel-edges-{tag}@zeroship.test"),
            "Every Edge",
            None,
        )
        .await
        .unwrap();

        // identity edge -> must CASCADE.
        db.execute(
            "INSERT INTO zeroship.identity_links (principal_id, provider, provider_subject) \
         VALUES ($1, 'zeroship', $2)",
            &[&victim.id, &format!("edge-{tag}")],
        )
        .await
        .unwrap();
        // identity edge -> must CASCADE.
        db.execute(
        "INSERT INTO zeroship.principal_grants (principal_id, grant_name) VALUES ($1, 'apps:read')",
        &[&victim.id],
    )
    .await
    .unwrap();
        // identity edge -> must CASCADE. A device grant left behind with a NULL
        // principal and `status = 'approved'` would be a grant authorised by nobody.
        let device_code_hash = format!("dch-{tag}");
        db.execute(
            "INSERT INTO zeroship.device_grants \
            (device_code_hash, user_code, status, principal_id, provider, expires_at) \
         VALUES ($1, $2, 'approved', $3, 'zeroship', NOW() + INTERVAL '10 minutes')",
            &[&device_code_hash, &format!("uc-{tag}"), &victim.id],
        )
        .await
        .unwrap();
        // attribution edge -> must SET NULL, and the row must survive.
        let client_id = format!("acctdel-edges-client-{tag}");
        db.execute(
            "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, redirect_uris, scopes, created_by) \
         VALUES ($1, 'Edge Probe', ARRAY['https://probe.zeroship.test/cb'], \
                 ARRAY['apps:read'], $2)",
            &[&client_id, &victim.id],
        )
        .await
        .unwrap();
        // attribution edge -> must SET NULL. `submitted_by` was NOT NULL and
        // RESTRICT; the schema apply record belongs to the app, not the human.
        db.execute(
            "INSERT INTO zeroship.plans \
            (id, name, runtime_limits_json, assignable_by_creator) \
         VALUES ('free', 'Free', '{}'::jsonb, TRUE) ON CONFLICT (id) DO NOTHING",
            &[],
        )
        .await
        .unwrap();
        let app_id = Uuid::new_v4();
        let project_id = common::unowned_project(&db).await;
        db.execute(
            "INSERT INTO zeroship.apps (id, name, plan_id, project_id, organization_id) \
         SELECT $1, $2, 'free', p.id, p.organization_id \
           FROM zeroship.projects p WHERE p.id = $3",
            &[&app_id, &format!("acctdel-edges-app-{tag}"), &project_id],
        )
        .await
        .unwrap();
        let migration_id = Uuid::new_v4();
        db.execute(
            "INSERT INTO zeroship.app_schema_applies \
            (app_id, migration_id, status, request_body, effective_profile, \
             ceiling_id, ceiling_version, descriptor_sha256, submitted_by) \
         VALUES ($1, $2, 'applied', '{}'::jsonb, '{}'::jsonb, 'managed', 1, $3, $4)",
            &[&app_id, &migration_id, &format!("sha-{tag}"), &victim.id],
        )
        .await
        .unwrap();

        users::request_deletion(&mut db, victim.id, account_reaper::GRACE_DAYS)
            .await
            .unwrap()
            .unwrap();
        backdate_schedule(&db, victim.id).await;

        let report = account_reaper::tick(&mut db, &control)
            .await
            .expect("reaper tick");
        assert_eq!(
            report,
            ReaperReport {
                erased: 1,
                failed: 0
            }
        );
        assert!(
            db.query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&victim.id])
                .await
                .unwrap()
                .is_empty(),
            "no inbound reference blocks the delete"
        );

        for (table, column) in [
            ("identity_links", "principal_id"),
            ("principal_grants", "principal_id"),
            ("device_grants", "principal_id"),
        ] {
            let sql = format!("SELECT 1 FROM zeroship.{table} WHERE {column} = $1");
            assert!(
                db.query(&sql, &[&victim.id]).await.unwrap().is_empty(),
                "{table}.{column} is an identity edge and must CASCADE"
            );
        }
        let created_by: Option<Uuid> = db
            .query_one(
                "SELECT created_by FROM zeroship.oauth_clients WHERE client_id = $1",
                &[&client_id],
            )
            .await
            .expect("the oauth client survives its creator")
            .get("created_by");
        assert!(created_by.is_none(), "oauth_clients.created_by is SET NULL");
        let submitted_by: Option<Uuid> = db
            .query_one(
                "SELECT submitted_by FROM zeroship.app_schema_applies \
             WHERE app_id = $1 AND migration_id = $2",
                &[&app_id, &migration_id],
            )
            .await
            .expect("the schema apply record survives its submitter")
            .get("submitted_by");
        assert!(
            submitted_by.is_none(),
            "app_schema_applies.submitted_by is SET NULL"
        );
    })
    .await;
}

/// The same erasure, executed by the ROLE the auth service actually runs as.
///
/// Every other test in this file connects as the superuser, and that is how a
/// permission-denied predicate survived in `erase_one_tx` for as long as it did.
/// The cascades this depends on run with the CONSTRAINT OWNER's privileges, not
/// the caller's, which is exactly why declaring the edges works where the
/// reaper's own `UPDATE ... SET col = NULL` on a control-owned table would not.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reaper_erases_as_the_real_auth_role() {
    Database::run(async |database| {
        let db = database.connect().await;
        let (_mock, control) = clear_control().await;
        let mut as_auth = database.connect_as_auth().await;

        let tag = Uuid::new_v4().simple().to_string();
        let user = users::create(
            &db,
            &format!("acctdel-role-{tag}@zeroship.test"),
            "Real Role",
            None,
        )
        .await
        .unwrap();
        // One edge into a table `zeroship_auth` cannot write directly
        // (`identity_links`: no grant at all). If the erasure needed the caller's
        // privileges rather than the constraint owner's, this is where it fails.
        db.execute(
            "INSERT INTO zeroship.identity_links (principal_id, provider, provider_subject) \
         VALUES ($1, 'zeroship', $2)",
            &[&user.id, &format!("role-{tag}")],
        )
        .await
        .unwrap();

        users::request_deletion(&mut as_auth, user.id, account_reaper::GRACE_DAYS)
            .await
            .expect("the real role can open a deletion window")
            .expect("user exists");
        backdate_schedule(&db, user.id).await;

        let report = account_reaper::tick(&mut as_auth, &control)
            .await
            .expect("the real role can run a reaper tick");
        assert_eq!(
            report,
            ReaperReport {
                erased: 1,
                failed: 0
            }
        );
        assert!(
            db.query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user.id])
                .await
                .unwrap()
                .is_empty(),
            "the users row is gone"
        );
        assert!(
            audit_detail(&db, user.id, "account_erasure_failed")
                .await
                .is_empty(),
            "the real role hit no permission or constraint failure"
        );
    })
    .await;
}

/// A blocker that appeared DURING the grace window must stop the delete, and
/// the refusal must be durable and name the user. Logging and continuing is
/// what this replaces.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reaper_refuses_and_records_when_the_preflight_names_a_blocker() {
    Database::run(async |database| {
        let mut db = database.connect().await;
        let mock = MockControl::start(Answer::Clear).await;
        let control = ControlAccess {
            control_url: mock.base.clone(),
            keyring: mock.keyring(),
        };
        let tag = Uuid::new_v4().simple().to_string();
        let user = users::create(
            &db,
            &format!("acctdel-blocked-{tag}@zeroship.test"),
            "Blocked",
            None,
        )
        .await
        .unwrap();
        users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
            .await
            .unwrap()
            .unwrap();
        backdate_schedule(&db, user.id).await;

        // The window opened clear; by the time the reaper runs, this human is the
        // last owner of something.
        mock.set(Answer::SoleOwnerOf {
            slug: format!("late-{tag}"),
        });
        let report = account_reaper::tick(&mut db, &control).await.expect("tick");
        assert_eq!(
            report,
            ReaperReport {
                erased: 0,
                failed: 1
            }
        );

        assert!(
            !db.query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user.id])
                .await
                .unwrap()
                .is_empty(),
            "a re-blocked user is left pending, not half-erased"
        );
        let details = audit_detail(&db, user.id, "account_erasure_failed").await;
        assert_eq!(details.len(), 1, "one durable record, on this user");
        assert_eq!(details[0]["stage"], "preflight");
        assert!(
            details[0]["reason"]
                .as_str()
                .expect("reason")
                .contains(&format!("late-{tag}")),
            "the record names the organization: {}",
            details[0]
        );
        assert!(
            mock.asked().contains(&user.id.to_string()),
            "the reaper really asked about this principal"
        );
    })
    .await;
}

/// The MONEY rule, at the reaper.
///
/// This is the third enforcement point of the rule whose SQL is bound in
/// `crates/zeroship-control/tests/deletion_owes_test.rs`. It is a separate
/// point rather than a repeat of the ownership one: the blocker arrives with an
/// EMPTY `blockers` list, because the organization is dissolved and the
/// ownership rule deliberately says nothing about closed organizations. A
/// reaper that read only `blockers` would erase this human and walk away from
/// the invoice.
///
/// The refusal must also be selectable as a MONEY refusal - `stage = billing` -
/// so "whose erasure is money holding up" is one query rather than a grep of
/// reason strings. The paired control is the clear answer that erases the same
/// user under the same fixture: one variable, the answer.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reaper_refuses_and_records_billing_when_the_organization_still_owes() {
    Database::run(async |database| {
        let mut db = database.connect().await;
        let mock = MockControl::start(Answer::Clear).await;
        let control = ControlAccess {
            control_url: mock.base.clone(),
            keyring: mock.keyring(),
        };
        let tag = Uuid::new_v4().simple().to_string();
        let debtor = users::create(
            &db,
            &format!("acctdel-owes-{tag}@zeroship.test"),
            "Owes",
            None,
        )
        .await
        .unwrap();
        users::request_deletion(&mut db, debtor.id, account_reaper::GRACE_DAYS)
            .await
            .unwrap()
            .unwrap();
        backdate_schedule(&db, debtor.id).await;

        // The window opened clear. The billing sweep then finalized last month's
        // invoice on an organization this human had already closed - which needs
        // nobody to act, and is why the request-time check cannot stand in here.
        mock.set(Answer::OwesBilling {
            slug: format!("owing-{tag}"),
            owed_cents: 4_200,
        });
        let report = account_reaper::tick(&mut db, &control).await.expect("tick");
        assert_eq!(
            report,
            ReaperReport {
                erased: 0,
                failed: 1
            }
        );

        assert!(
            !db.query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&debtor.id])
                .await
                .unwrap()
                .is_empty(),
            "a human who owes is left pending, not half-erased"
        );
        let details = audit_detail(&db, debtor.id, "account_erasure_failed").await;
        assert_eq!(details.len(), 1, "one durable record, on this user");
        assert_eq!(
            details[0]["stage"], "billing",
            "a money refusal must be selectable as one: {}",
            details[0]
        );
        let reason = details[0]["reason"].as_str().expect("reason");
        assert!(
            reason.contains(&format!("owing-{tag}")) && reason.contains("4200"),
            "the record names the organization and what it owes: {reason}"
        );
        assert!(
            mock.asked().contains(&debtor.id.to_string()),
            "the reaper really asked about this principal"
        );

        // Recheck the same pending user after the debt is settled.
        mock.set(Answer::Clear);
        let report = account_reaper::tick(&mut db, &control).await.expect("tick");
        assert_eq!(
            report,
            ReaperReport {
                erased: 1,
                failed: 0
            }
        );
        assert!(
            db.query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&debtor.id])
                .await
                .unwrap()
                .is_empty(),
            "the same user is erased after settlement"
        );
    })
    .await;
}

/// An unanswerable preflight is a refusal too. The failure mode being ruled out
/// is the one where "control is down" and "control said yes" are the same
/// outcome.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reaper_refuses_when_the_preflight_cannot_be_answered() {
    Database::run(async |database| {
        let mut db = database.connect().await;
        let mock = MockControl::start(Answer::Unavailable).await;
        let control = ControlAccess {
            control_url: mock.base.clone(),
            keyring: mock.keyring(),
        };
        let tag = Uuid::new_v4().simple().to_string();
        let user = users::create(
            &db,
            &format!("acctdel-unavail-{tag}@zeroship.test"),
            "Unavailable",
            None,
        )
        .await
        .unwrap();
        users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
            .await
            .unwrap()
            .unwrap();
        backdate_schedule(&db, user.id).await;

        let report = account_reaper::tick(&mut db, &control).await.expect("tick");
        assert_eq!(
            report,
            ReaperReport {
                erased: 0,
                failed: 1
            }
        );
        assert!(
            !db.query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user.id])
                .await
                .unwrap()
                .is_empty(),
            "nothing is erased on an unanswerable preflight"
        );
        let details = audit_detail(&db, user.id, "account_erasure_failed").await;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0]["stage"], "preflight");
    })
    .await;
}

/// A credential the control plane does not trust is the deployment-fault arm.
/// It must refuse rather than proceed, and it must NOT be distinguishable from
/// a clear answer only by a log line.
///
/// The keyring here is a well-formed `svc/auth` identity under a key this mock
/// never trusted - the one variable that differs from the clear-answer fixture.
/// A missing credential cannot be expressed any more: `ServiceKeyring::load`
/// refuses the boot, so the arm that survives is a REJECTED one, and it is the
/// stronger arm because the round trip really happens.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reaper_refuses_when_the_control_plane_rejects_its_credential() {
    Database::run(async |database| {
        let mut db = database.connect().await;
        let mock = MockControl::start(Answer::Clear).await;
        let control = ControlAccess {
            control_url: mock.base.clone(),
            keyring: common::mock_control::untrusted_auth_keyring(),
        };
        let tag = Uuid::new_v4().simple().to_string();
        let user = users::create(
            &db,
            &format!("acctdel-nokey-{tag}@zeroship.test"),
            "No Key",
            None,
        )
        .await
        .unwrap();
        users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
            .await
            .unwrap()
            .unwrap();
        backdate_schedule(&db, user.id).await;

        let report = account_reaper::tick(&mut db, &control).await.expect("tick");
        assert_eq!(
            report,
            ReaperReport {
                erased: 0,
                failed: 1
            }
        );
        assert!(
            !db.query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user.id])
                .await
                .unwrap()
                .is_empty(),
            "an unverifiable erasure does not happen"
        );
        assert!(
            mock.asked().is_empty(),
            "a refused credential must not reach the answer: the 401 comes before \
         the handler records the principal"
        );
    })
    .await;
}

/// A `23503` from the DELETE has to reach a durable per-user record naming the
/// constraint. Proved by introducing a blocking reference the migration does
/// not know about - which is exactly the future defect the loud arm exists for.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn a_new_blocking_reference_is_recorded_with_its_constraint() {
    Database::run(async |database| {
        let mut db = database.connect().await;
        let (_mock, control) = clear_control().await;
        let tag = Uuid::new_v4().simple().to_string();
        let probe = format!("erasure_probe_{tag}");
        db.execute(
            &format!(
                "CREATE TABLE zeroship.{probe} ( \
                 user_id uuid NOT NULL REFERENCES zeroship.users(id) ON DELETE RESTRICT)"
            ),
            &[],
        )
        .await
        .expect("create probe table");

        let user = users::create(
            &db,
            &format!("acctdel-probe-{tag}@zeroship.test"),
            "Probe",
            None,
        )
        .await
        .unwrap();
        db.execute(
            &format!("INSERT INTO zeroship.{probe} (user_id) VALUES ($1)"),
            &[&user.id],
        )
        .await
        .unwrap();
        users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
            .await
            .unwrap()
            .unwrap();
        backdate_schedule(&db, user.id).await;

        let report = account_reaper::tick(&mut db, &control).await.expect("tick");
        let details = audit_detail(&db, user.id, "account_erasure_failed").await;

        assert_eq!(
            report,
            ReaperReport {
                erased: 0,
                failed: 1
            }
        );
        assert_eq!(details.len(), 1, "one record, on the user it happened to");
        assert_eq!(
            details[0]["stage"], "constraint",
            "a 23503 is classified as a constraint refusal, not a generic db error"
        );
        let constraint = details[0]["constraint"]
            .as_str()
            .expect("constraint recorded");
        assert!(
            constraint.contains(&probe),
            "the record names the reference that blocked: {constraint}"
        );
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reaper_skips_cancelled_request() {
    Database::run(async |database| {
        let mut db = database.connect().await;
        let (_mock, control) = clear_control().await;
        let tag = Uuid::new_v4().simple().to_string();
        let email = format!("acctdel-skip-{tag}@zeroship.test");
        let user = users::create(&db, &email, "Skip User", Some("phc"))
            .await
            .unwrap();

        let req = users::request_deletion(&mut db, user.id, account_reaper::GRACE_DAYS)
            .await
            .unwrap()
            .unwrap();
        backdate_schedule(&db, user.id).await;
        // Cancel clears the schedule - even though the (now-cleared) date was in
        // the past, the reaper must not touch a cancelled request.
        deletion_cancel::redeem(&db, &req.cancel_token)
            .await
            .unwrap();

        // No count assertion here, and that is the point rather than an omission:
        // "the reaper touched nobody" is a claim over the whole database, and this
        // run owns exactly one user in it. The row check below makes the same claim
        // about the only row that is this run's to speak for.
        account_reaper::tick(&mut db, &control)
            .await
            .expect("reaper tick");

        let remaining = db
            .query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user.id])
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1, "user survives a cancelled request");
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reaper_ignores_a_schedule_without_a_deletion_request() {
    Database::run(async |database| {
        let mut db = database.connect().await;
        let (_mock, control) = clear_control().await;
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

        account_reaper::tick(&mut db, &control).await.unwrap();

        let still_exists = db
            .query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user.id])
            .await
            .unwrap();
        assert_eq!(
            still_exists.len(),
            1,
            "a schedule alone is a deny state, not erasure authorization"
        );
    })
    .await;
}
