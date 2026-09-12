//! The erasure seam, against the real organization tables.
//!
//! `crate::erasure::preflight` is what the auth service asks before it opens a
//! deletion window and again before the reaper erases. This file rules on its
//! OWNERSHIP rule - is this human the only owner of a live organization - and
//! every blocker it returns has to carry enough for the person to act on it.
//!
//! Its second rule, whether an organization still OWES, has its own file
//! (`crates/zeroship-control/tests/deletion_owes_test.rs`) because it shares one
//! predicate with the dissolve path and the two are worth ruling on together.
//! The reaper's half of that rule is in
//! `crates/zeroship-auth/tests/account_deletion_test.rs`, where the reaper is.
//!
//! The auth suite exercises the WIRE (`mock_control`); this exercises the SQL,
//! because that is where the tables are and `zeroship_auth` cannot read them.

#![allow(clippy::future_not_send)]

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;

use zeroship_control::billing_read::LocalInvoicing;
use zeroship_control::erasure::{preflight, ErasureRemedy};
use zeroship_control::organizations::{
    self, AddMemberBody, CreateOrganizationBody, TransferOwnershipBody,
};
use zeroship_control::Registry;
use zeroship_core::UserId;

use crate::common;

struct Fx {
    registry: Registry,
    pg: Client,
}

impl Fx {
    /// A fixture, or no run at all.
    ///
    /// IT RETURNS `Self` AND NOT `Option<Self>` ON PURPOSE. There is no
    /// database state this can decline for: `common::require_control_db` ENDS
    /// the process when the DSN is absent or the schema is not there, so the
    /// only value an `Option` could carry is `Some`. It carried one anyway
    /// until 2026-09-08, and every caller spelled `let Some(fx) = ... else {
    /// return }` - the exact shape that used to mean "pass silently", left
    /// standing as a template for the next test to copy. Returning the value
    /// makes that shape unwritable rather than merely unreachable.
    async fn new() -> Self {
        let url = common::require_control_db();
        let (pg, conn) = connect(&url, NoTls).await.expect("control-pg connect");
        compio::runtime::spawn(async move {
            let _ = conn.run().await;
        })
        .detach();
        let registry = Registry::new(&url).await.expect("registry");
        Self { registry, pg }
    }

    async fn seed_user(&self, label: &str) -> UserId {
        let id = UserId::mint();
        let email = format!("{label}-{}@zeroship.test", id.as_str());
        self.pg
            .execute(
                "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
                 VALUES ($1, $2::citext, $3, NOW())",
                &[&id.as_str(), &email, &label],
            )
            .await
            .expect("insert user");
        id
    }

    /// Mint an organization through the production path so the fixture and the
    /// product cannot disagree about what a fresh one looks like. A fresh
    /// organization carries a default project, which is why the no-colleague
    /// remedy below is `DeleteProjects` rather than `Dissolve`.
    async fn organization(&self, owner: &UserId, label: &str) -> String {
        organizations::create_organization(
            &self.registry,
            owner,
            &CreateOrganizationBody {
                name: format!("{label} {}", Uuid::new_v4().simple()),
                slug: Some(format!("{label}-{}", Uuid::new_v4().simple())),
                billing_email: None,
            },
            None,
        )
        .await
        .expect("create organization")
        .id
    }

    async fn drop_projects(&self, organization: &str) {
        self.pg
            .execute(
                "DELETE FROM zeroship.projects WHERE organization_id = $1",
                &[&organization],
            )
            .await
            .expect("drop projects");
    }

    async fn cleanup(&self, organizations: &[&str], users: &[&UserId]) {
        for organization in organizations {
            let _ = self
                .pg
                .execute(
                    "DELETE FROM zeroship.projects WHERE organization_id = $1",
                    &[organization],
                )
                .await;
            let _ = self
                .pg
                .execute(
                    "DELETE FROM zeroship.organizations WHERE id = $1",
                    &[organization],
                )
                .await;
        }
        for user in users {
            let _ = self
                .pg
                .execute(
                    "DELETE FROM zeroship.users WHERE id = $1",
                    &[&user.as_str()],
                )
                .await;
        }
    }
}

/// The sole owner of a live organization is a blocker, and the blocker names
/// the organization and the first thing that has to happen.
#[compio::test]
async fn a_sole_owner_is_blocked_and_told_what_to_do() {
    let fx = Fx::new().await;
    let owner = fx.seed_user("erasure-sole").await;
    let organization = fx.organization(&owner, "erasure-sole").await;

    let report = preflight(&fx.pg, &owner, LocalInvoicing::Yes)
        .await
        .expect("preflight");
    assert_eq!(report.blockers.len(), 1, "{report:?}");
    let blocker = &report.blockers[0];
    assert_eq!(blocker.organization_id, organization);
    assert_eq!(blocker.other_member_count, 0);
    assert!(
        blocker.project_count > 0,
        "a fresh organization has a project"
    );
    assert_eq!(
        blocker.remedy,
        ErasureRemedy::DeleteProjects,
        "nobody to hand it to and it still owns projects"
    );

    // Empty it and the remedy becomes the one the creator can actually run.
    fx.drop_projects(&organization).await;
    let report = preflight(&fx.pg, &owner, LocalInvoicing::Yes)
        .await
        .expect("preflight");
    assert_eq!(report.blockers[0].remedy, ErasureRemedy::Dissolve);

    fx.cleanup(&[&organization], &[&owner]).await;
}

/// Transfer is the remedy, so it has to actually clear the blocker: after it,
/// the person who asked to be deleted is no longer anybody's last owner and the
/// successor is.
#[compio::test]
async fn transferring_ownership_moves_the_blocker_to_the_successor() {
    let fx = Fx::new().await;
    let owner = fx.seed_user("erasure-xfer").await;
    let organization = fx.organization(&owner, "erasure-xfer").await;
    let successor = fx.seed_user("erasure-heir").await;
    organizations::add_member(
        &fx.registry,
        &owner,
        &organization,
        &AddMemberBody {
            user_id: successor.clone(),
            role: "admin".to_string(),
        },
        None,
    )
    .await
    .expect("seat the successor");

    organizations::transfer_ownership(
        &fx.registry,
        &owner,
        &organization,
        &TransferOwnershipBody {
            user_id: successor.clone(),
        },
        None,
    )
    .await
    .expect("transfer");

    assert!(
        preflight(&fx.pg, &owner, LocalInvoicing::Yes)
            .await
            .expect("preflight")
            .is_clear(),
        "the remedy the blocker names must clear the blocker"
    );
    let heir = preflight(&fx.pg, &successor, LocalInvoicing::Yes)
        .await
        .expect("preflight");
    assert_eq!(
        heir.blockers.len(),
        1,
        "the successor inherits it: {heir:?}"
    );

    fx.cleanup(&[&organization], &[&owner, &successor]).await;
}

/// The sole-ownership test is `NOT EXISTS(a second owner)`, and this is what
/// binds that clause.
///
/// MEASURED: no product path mints a second owner today. `add_member` and
/// `change_member_role` both demand a STRICTLY higher rank than the role being
/// granted, so an owner cannot seat an owner; `organization_invites_no_escalation`
/// refuses the same thing on the invite path; and `transfer_ownership` steps the
/// previous owner down as it promotes. The SCHEMA permits two - there is no
/// unique index on `(organization_id) WHERE role = 'owner'`, only a partial
/// btree - so the row is seeded directly here rather than through an API that
/// refuses it. Without this case the clause would be an unbound guard.
#[compio::test]
async fn a_second_owner_row_clears_the_blocker_for_both() {
    let fx = Fx::new().await;
    let owner = fx.seed_user("erasure-co").await;
    let organization = fx.organization(&owner, "erasure-co").await;
    let peer = fx.seed_user("erasure-peer").await;
    fx.pg
        .execute(
            "INSERT INTO zeroship.organization_members \
                (organization_id, user_id, role, added_by, changed_by) \
             SELECT $1, $2, 'owner', NULL, NULL",
            &[&organization, &peer.as_str()],
        )
        .await
        .expect("seed a second owner row");

    assert!(
        preflight(&fx.pg, &owner, LocalInvoicing::Yes)
            .await
            .expect("preflight")
            .is_clear(),
        "two owners means neither is the last one"
    );
    assert!(preflight(&fx.pg, &peer, LocalInvoicing::Yes)
        .await
        .expect("preflight")
        .is_clear());

    fx.cleanup(&[&organization], &[&owner, &peer]).await;
}

/// A member below `owner` is never a blocker - and this is the control for the
/// case above: an organization with exactly one owner still blocks THAT owner
/// while clearing everyone else on it.
#[compio::test]
async fn a_non_owner_seat_is_never_a_blocker_and_the_owner_still_is() {
    let fx = Fx::new().await;
    let owner = fx.seed_user("erasure-owner").await;
    let organization = fx.organization(&owner, "erasure-member").await;
    let member = fx.seed_user("erasure-dev").await;
    organizations::add_member(
        &fx.registry,
        &owner,
        &organization,
        &AddMemberBody {
            user_id: member.clone(),
            role: "developer".to_string(),
        },
        None,
    )
    .await
    .expect("seat a developer");

    assert!(
        preflight(&fx.pg, &member, LocalInvoicing::Yes)
            .await
            .expect("preflight")
            .is_clear(),
        "a developer's departure strands nothing"
    );
    let report = preflight(&fx.pg, &owner, LocalInvoicing::Yes)
        .await
        .expect("preflight");
    assert_eq!(report.blockers.len(), 1);
    assert_eq!(
        report.blockers[0].remedy,
        ErasureRemedy::Transfer,
        "there is somebody to hand it to, so handing it over outranks closing it"
    );
    assert_eq!(report.blockers[0].other_member_count, 1);

    fx.cleanup(&[&organization], &[&owner, &member]).await;
}

/// A DISSOLVED organization is not a blocker. It is closed, its ledger is
/// retained by design, and nobody needs to administer it again - so demanding
/// a successor for it would be a refusal with no remedy.
#[compio::test]
async fn a_dissolved_organization_is_not_a_blocker() {
    let fx = Fx::new().await;
    let owner = fx.seed_user("erasure-closed").await;
    let organization = fx.organization(&owner, "erasure-closed").await;
    fx.drop_projects(&organization).await;
    fx.pg
        .execute(
            "UPDATE zeroship.organizations SET dissolved_at = NOW() WHERE id = $1",
            &[&organization],
        )
        .await
        .expect("close the organization");

    assert!(
        preflight(&fx.pg, &owner, LocalInvoicing::Yes)
            .await
            .expect("preflight")
            .is_clear(),
        "a closed organization needs no successor"
    );

    fx.cleanup(&[&organization], &[&owner]).await;
}

/// A principal with no seat anywhere is clear. Without this the suite could
/// pass on a preflight that blocked everybody.
#[compio::test]
async fn a_principal_with_no_seat_is_clear() {
    let fx = Fx::new().await;
    let nobody = fx.seed_user("erasure-nobody").await;
    assert!(preflight(&fx.pg, &nobody, LocalInvoicing::Yes)
        .await
        .expect("preflight")
        .is_clear());
    fx.cleanup(&[], &[&nobody]).await;
}
