//! The organization ownership surface, against a live PostgreSQL.
//!
//! Every case here is about a REFUSAL that the schema cannot express and Cedar
//! must not be the only thing enforcing. The rank comparison rides in the
//! statement that performs the effect, so what these tests bind is that a row
//! did not appear - not that a handler returned a status.
//!
//! # What each group is for
//!
//! - **Narrowing parity.** `crate::organizations::effective_project_rank_sql`
//!   re-expresses `zeroship_authz::effective_project_rank` in SQL, because
//!   decision 4 requires the comparison to be inside the effect statement and a
//!   Rust function cannot be. Two implementations of one rule is a drift risk,
//!   so the whole case table is run through BOTH and compared. The NULL cases
//!   are the point: PostgreSQL's `LEAST` IGNORES nulls, so the obvious SQL
//!   silently widens a member with no project row to their full organization
//!   rank.
//! - **The rank fence.** An actor may act only on a strictly lower rank with at
//!   least equal billing authority. Each refusal is paired with a control that
//!   differs in ONE variable, so a green is a result rather than a coincidence.
//! - **The last owner.** Not a CHECK - the claim is about a set. What the tests
//!   can bind here is the predicate; the LOCK that makes it true under
//!   concurrency is exercised by the concurrent case at the end.
//! - **Invite redemption.** The schema froze the inviter's rank at issue time
//!   and said so. These bind the half only the control plane can enforce: an
//!   invite from a since-demoted admin is refused.

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;
use zeroship_control::organizations::{
    self as organizations, AddMemberBody, AddProjectMemberBody, ChangeRoleBody, CreateInviteBody,
    CreateOrganizationBody, CreateProjectBody, OrganizationError, RedeemInviteBody,
    TransferOwnershipBody, UpdateOrganizationBody,
};
use zeroship_control::Registry;

use crate::common;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A live database and one connection to it. Every case builds its own, so a
/// failure in one leaves nothing behind for the next to trip on.
struct Fx {
    registry: Registry,
    pg: Client,
}

impl Fx {
    /// `None` when no migrated database is configured. Returning rather than
    /// skipping silently is the crate idiom; `common::require_control_db`
    /// already ENDS the run when the variable is set but the schema is absent,
    /// so reaching `None` here means the suite was asked to run without a
    /// database at all.
    async fn new() -> Option<Self> {
        let url = common::require_control_db();
        let (pg, conn) = connect(&url, NoTls).await.expect("control-pg connect");
        compio::runtime::spawn(async move {
            let _ = conn.run().await;
        })
        .detach();
        let registry = Registry::new(&url).await.expect("registry");
        Some(Self { registry, pg })
    }
}

/// One organization with its owner, plus whatever extra members a case seats.
struct Org {
    id: String,
    owner: Uuid,
    seeded: Vec<Uuid>,
}

impl Org {
    /// Mint an organization through the production path, so the fixture and
    /// the product cannot disagree about what a fresh organization looks like.
    async fn new(fx: &Fx, label: &str) -> Self {
        let owner = seed_user(&fx.pg, label).await;
        let record = organizations::create_organization(
            &fx.registry,
            owner,
            &CreateOrganizationBody {
                name: format!("{label} {}", Uuid::new_v4().simple()),
                slug: Some(format!("{label}-{}", Uuid::new_v4().simple())),
                billing_email: None,
            },
            None,
        )
        .await
        .expect("create organization");
        Self {
            id: record.id,
            owner,
            seeded: vec![owner],
        }
    }

    /// Seat a fresh user at `role`, using the OWNER's authority so the seating
    /// itself is never the thing under test.
    async fn seat(&mut self, fx: &Fx, label: &str, role: &str) -> Uuid {
        let user = seed_user(&fx.pg, label).await;
        organizations::add_member(
            &fx.registry,
            self.owner,
            &self.id,
            &AddMemberBody {
                user_id: user,
                role: role.to_string(),
            },
            None,
        )
        .await
        .unwrap_or_else(|err| panic!("seat {label} as {role}: {err:?}"));
        self.seeded.push(user);
        user
    }

    async fn role_of(&self, fx: &Fx, user: Uuid) -> Option<String> {
        let rows = fx
            .pg
            .query(
                "SELECT role FROM zeroship.organization_members \
                  WHERE organization_id = $1 AND user_id = $2",
                &[&self.id, &user],
            )
            .await
            .expect("read role");
        rows.first().map(|row| row.get("role"))
    }

    async fn owner_count(&self, fx: &Fx) -> i64 {
        let rows = fx
            .pg
            .query(
                "SELECT COUNT(*)::bigint AS n FROM zeroship.organization_members \
                  WHERE organization_id = $1 AND role = 'owner'",
                &[&self.id],
            )
            .await
            .expect("count owners");
        rows[0].get("n")
    }

    /// The default project every organization is minted with.
    async fn default_project(&self, fx: &Fx) -> String {
        let rows = fx
            .pg
            .query(
                "SELECT id FROM zeroship.projects WHERE organization_id = $1 ORDER BY id LIMIT 1",
                &[&self.id],
            )
            .await
            .expect("read default project");
        rows.first()
            .map(|row| row.get("id"))
            .expect("a fresh organization carries a default project")
    }

    async fn cleanup(self, fx: &Fx) {
        let pg = &fx.pg;
        let _ = pg
            .execute(
                "DELETE FROM zeroship.projects WHERE organization_id = $1",
                &[&self.id],
            )
            .await;
        let _ = pg
            .execute(
                "DELETE FROM zeroship.organizations WHERE id = $1",
                &[&self.id],
            )
            .await;
        for user in self.seeded {
            let _ = pg
                .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user])
                .await;
        }
    }
}

async fn seed_user(pg: &Client, label: &str) -> Uuid {
    let id = Uuid::new_v4();
    let email = format!("{label}-{}@zeroship.test", id.simple());
    pg.execute(
        "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
         VALUES ($1, $2::citext, $3, NOW())",
        &[&id, &email, &label],
    )
    .await
    .expect("insert user");
    id
}

/// The verified address of a seeded user, which invite redemption matches on.
async fn email_of(pg: &Client, user: Uuid) -> String {
    let rows = pg
        .query(
            "SELECT email::text AS email FROM zeroship.users WHERE id = $1",
            &[&user],
        )
        .await
        .expect("read email");
    rows[0].get("email")
}

// ---------------------------------------------------------------------------
// Narrowing parity
// ---------------------------------------------------------------------------

/// The SQL narrowing and the Rust narrowing must agree on EVERY case.
///
/// This is the test the duplication is acceptable because of. Delete the
/// explicit `WHEN project_rank IS NULL THEN 0` arm from
/// `effective_project_rank_sql` and this goes red on the two rows where a
/// below-admin member holds no project seat - the exact case where
/// `LEAST(rank, NULL)` returns `rank` and hands them authority they were never
/// granted.
#[compio::test]
async fn organization_project_rank_matches_the_authz_narrowing() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let pg = &fx.pg;

    // (organization_rank, project_rank, admin_rank)
    let cases: &[(Option<i32>, Option<i32>, Option<i32>)] = &[
        // Admin and above reach every project at their organization rank.
        (Some(30), None, Some(30)),
        (Some(40), None, Some(30)),
        (Some(40), Some(10), Some(30)),
        // Below admin, a project row is required and the minimum is taken.
        (Some(20), None, Some(30)),
        (Some(10), None, Some(30)),
        (Some(20), Some(10), Some(30)),
        (Some(20), Some(40), Some(30)),
        (Some(10), Some(10), Some(30)),
        // No organization seat at all.
        (None, Some(40), Some(30)),
        (None, None, Some(30)),
        // A ladder with no admin row must NARROW.
        (Some(40), None, None),
        (Some(40), Some(10), None),
    ];

    let sql = format!(
        "SELECT {expr} AS effective",
        expr = organizations::effective_project_rank_sql_for_test("$1::int", "$2::int", "$3::int"),
    );

    for (organization_rank, project_rank, admin_rank) in cases {
        let rows = pg
            .query(&sql, &[organization_rank, project_rank, admin_rank])
            .await
            .unwrap_or_else(|err| panic!("narrowing SQL for {organization_rank:?}: {err}"));
        let from_sql: i32 = rows[0].get("effective");
        let from_rust =
            zeroship_authz::effective_project_rank(*organization_rank, *project_rank, *admin_rank);
        assert_eq!(
            from_sql, from_rust,
            "SQL and Rust must agree for organization={organization_rank:?} \
             project={project_rank:?} admin={admin_rank:?}"
        );
    }

    // A CONTROL for the whole table: it must contain a case where the answer is
    // NOT the organization rank, or an implementation that returned the
    // organization rank unconditionally would pass every row above.
    assert!(
        cases
            .iter()
            .any(|(organization_rank, project_rank, admin_rank)| {
                zeroship_authz::effective_project_rank(
                    *organization_rank,
                    *project_rank,
                    *admin_rank,
                ) != organization_rank.unwrap_or(0)
            }),
        "the case table must include a case the narrowing actually narrows"
    );
    common::drain_pg().await;
}

// ---------------------------------------------------------------------------
// The rank fence
// ---------------------------------------------------------------------------

/// An admin may seat a developer and may NOT seat another admin.
///
/// The pair differs in ONE variable - the role being granted - so the refusal
/// is attributable to the rank comparison and not to the actor's seat.
#[compio::test]
async fn an_admin_seats_below_itself_and_never_at_its_own_rank() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let mut org = Org::new(&fx, "rankorg").await;
    let admin = org.seat(&fx, "admin", "admin").await;
    let target = seed_user(&fx.pg, "target").await;

    // The control: one rank below the actor, and it succeeds.
    organizations::add_member(
        &fx.registry,
        admin,
        &org.id,
        &AddMemberBody {
            user_id: target,
            role: "developer".to_string(),
        },
        None,
    )
    .await
    .expect("an admin may seat a developer");
    assert_eq!(org.role_of(&fx, target).await.as_deref(), Some("developer"));

    // The case: same actor, same target, ONE variable changed.
    let refused = seed_user(&fx.pg, "peer").await;
    let err = organizations::add_member(
        &fx.registry,
        admin,
        &org.id,
        &AddMemberBody {
            user_id: refused,
            role: "admin".to_string(),
        },
        None,
    )
    .await
    .expect_err("an admin must not seat another admin");
    assert!(
        matches!(err, OrganizationError::Insufficient(_)),
        "expected a rank refusal, got {err:?}"
    );
    assert_eq!(
        org.role_of(&fx, refused).await,
        None,
        "the refusal must leave NO row: the predicate is in the INSERT, so a refused seating \
         cannot have written one"
    );

    let _ = fx
        .pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&refused])
        .await;
    org.seeded.push(target);
    org.cleanup(&fx).await;
}

/// The BILLING axis refuses independently of the rank axis.
///
/// `admin` is rank 30 / billing_rank 10; `billing` is rank 10 / billing_rank
/// 20. So an admin outranks a bookkeeper on everything except money, and
/// `actor.billing_rank >= target.billing_rank` is the whole reason the seating
/// is refused. Without the billing conjunct this case passes and only this one
/// does - which is what makes the ladder two integers rather than one.
#[compio::test]
async fn the_billing_axis_refuses_where_the_rank_axis_would_allow() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let mut org = Org::new(&fx, "billorg").await;
    let admin = org.seat(&fx, "admin", "admin").await;

    let bookkeeper = seed_user(&fx.pg, "bookkeeper").await;
    let err = organizations::add_member(
        &fx.registry,
        admin,
        &org.id,
        &AddMemberBody {
            user_id: bookkeeper,
            role: "billing".to_string(),
        },
        None,
    )
    .await
    .expect_err("rank 30 outranks rank 10, but billing_rank 10 does not reach 20");
    assert!(matches!(err, OrganizationError::Insufficient(_)), "{err:?}");
    assert_eq!(org.role_of(&fx, bookkeeper).await, None);

    // The control: the OWNER carries billing_rank 20 and seats the same role
    // for the same user. Only the actor changed.
    organizations::add_member(
        &fx.registry,
        org.owner,
        &org.id,
        &AddMemberBody {
            user_id: bookkeeper,
            role: "billing".to_string(),
        },
        None,
    )
    .await
    .expect("an owner carries the billing authority an admin lacks");
    assert_eq!(
        org.role_of(&fx, bookkeeper).await.as_deref(),
        Some("billing")
    );

    org.seeded.push(bookkeeper);
    org.cleanup(&fx).await;
}

/// A member removed from the organization cannot act, even though Cedar ran
/// against the seat they held a moment earlier.
///
/// This is the whole point of putting the comparison in the effect statement.
/// The test drives the STORE function directly - past Cedar - which is the only
/// way to observe that the statement itself refuses rather than the band.
#[compio::test]
async fn a_revoked_member_is_refused_by_the_statement_not_by_cedar() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let mut org = Org::new(&fx, "revorg").await;
    let admin = org.seat(&fx, "admin", "admin").await;
    let target = seed_user(&fx.pg, "target").await;

    // Control: while seated, the admin can seat a developer.
    organizations::add_member(
        &fx.registry,
        admin,
        &org.id,
        &AddMemberBody {
            user_id: target,
            role: "developer".to_string(),
        },
        None,
    )
    .await
    .expect("a seated admin may seat a developer");
    organizations::remove_member(&fx.registry, org.owner, &org.id, target, None)
        .await
        .expect("owner removes the developer");

    // Now revoke the ACTOR's seat and repeat the identical call.
    organizations::remove_member(&fx.registry, org.owner, &org.id, admin, None)
        .await
        .expect("owner removes the admin");
    let err = organizations::add_member(
        &fx.registry,
        admin,
        &org.id,
        &AddMemberBody {
            user_id: target,
            role: "developer".to_string(),
        },
        None,
    )
    .await
    .expect_err("a revoked actor must seat nobody");
    assert!(
        matches!(err, OrganizationError::Insufficient(_)),
        "expected a rank refusal, got {err:?}"
    );
    assert_eq!(org.role_of(&fx, target).await, None);

    org.seeded.push(target);
    org.cleanup(&fx).await;
}

/// Nothing may leave an organization ownerless, and the refusal names the
/// remedy rather than reporting a constraint.
#[compio::test]
async fn the_last_owner_can_be_neither_removed_nor_demoted() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let mut org = Org::new(&fx, "lastorg").await;

    let removed = organizations::remove_member(&fx.registry, org.owner, &org.id, org.owner, None)
        .await
        .expect_err("the last owner must not be removable");
    assert!(
        matches!(removed, OrganizationError::LastOwner),
        "{removed:?}"
    );

    let demoted = organizations::change_member_role(
        &fx.registry,
        org.owner,
        &org.id,
        org.owner,
        &ChangeRoleBody {
            role: "admin".to_string(),
        },
        None,
    )
    .await
    .expect_err("demoting the last owner is the same hole as removing them");
    assert!(
        matches!(demoted, OrganizationError::LastOwner),
        "{demoted:?}"
    );

    assert_eq!(org.owner_count(&fx).await, 1);
    assert_eq!(org.role_of(&fx, org.owner).await.as_deref(), Some("owner"));

    // The CONTROL, and the way it is set up is itself a finding.
    //
    // NO ROUTE IN THIS MODULE CAN SEAT A SECOND OWNER. Every membership write
    // requires `actor.rank > target.rank`, and nothing outranks rank 40, so
    // `add_member`, `change_member_role` and invite redemption all refuse the
    // owner role outright; `transfer_ownership` moves the seat rather than
    // duplicating it. The `> 1` clause in the DELETE is therefore unreachable
    // through the API today - it guards a state only direct SQL, a restore, or
    // a future route can produce.
    //
    // So this fixture writes the row directly, deliberately reaching past the
    // API to reach the state the API cannot make. Without it the count clause
    // would be asserted only in the direction that is always true, which is the
    // shape of a guard nothing binds.
    let second = seed_user(&fx.pg, "second").await;
    fx.pg
        .execute(
            "INSERT INTO zeroship.organization_members \
                 (organization_id, user_id, role, added_by, changed_by) \
             VALUES ($1, $2, 'owner', $3, $3)",
            &[&org.id, &second, &org.owner],
        )
        .await
        .expect("seat a second owner past the API");
    org.seeded.push(second);
    assert_eq!(org.owner_count(&fx).await, 2);

    // With two owners the count clause is satisfied, so the removal is no
    // longer refused for THAT reason. It is still refused - by the rank fence,
    // because `40 > 40` is false - and the error says so, which is the
    // distinction the two variants exist to make.
    let still_refused =
        organizations::remove_member(&fx.registry, org.owner, &org.id, second, None)
            .await
            .expect_err("an owner does not outrank an owner");
    assert!(
        matches!(still_refused, OrganizationError::Insufficient(_)),
        "with two owners the refusal must be the RANK one, not the last-owner one: \
         {still_refused:?}"
    );

    org.cleanup(&fx).await;
}

/// Ownership transfer is the one operation that reshapes the owner set: it
/// grants rank 40 while giving up rank 40 in the same transaction.
#[compio::test]
async fn transfer_moves_ownership_and_steps_the_previous_owner_down() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let mut org = Org::new(&fx, "xferorg").await;
    let successor = org.seat(&fx, "successor", "developer").await;

    organizations::transfer_ownership(
        &fx.registry,
        org.owner,
        &org.id,
        &TransferOwnershipBody { user_id: successor },
        None,
    )
    .await
    .expect("transfer");

    assert_eq!(org.role_of(&fx, successor).await.as_deref(), Some("owner"));
    assert_eq!(org.role_of(&fx, org.owner).await.as_deref(), Some("admin"));
    assert_eq!(
        org.owner_count(&fx).await,
        1,
        "the organization must keep exactly one owner across the transfer"
    );

    // The previous owner is now an admin, so the SAME call is refused - which is
    // the control proving the transfer moved authority rather than copying it.
    let err = organizations::transfer_ownership(
        &fx.registry,
        org.owner,
        &org.id,
        &TransferOwnershipBody { user_id: successor },
        None,
    )
    .await
    .expect_err("a stepped-down owner may not transfer again");
    assert!(matches!(err, OrganizationError::Insufficient(_)), "{err:?}");

    org.cleanup(&fx).await;
}

/// Transferring a PERSONAL organization clears `personal_owner_id`. That
/// pointer means "this organization was minted for exactly this person", and
/// once somebody else owns it the statement is false.
#[compio::test]
async fn transferring_a_personal_organization_converts_it_to_shared() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let creator = seed_user(&fx.pg, "solo").await;
    let project = organizations::ensure_personal_project(&fx.registry, creator)
        .await
        .expect("personal project");

    let rows = fx
        .pg
        .query(
            "SELECT o.id, o.personal_owner_id FROM zeroship.organizations o \
               JOIN zeroship.projects p ON p.organization_id = o.id WHERE p.id = $1",
            &[&project.as_str()],
        )
        .await
        .expect("read organization");
    let organization_id: String = rows[0].get("id");
    assert_eq!(
        rows[0].get::<_, Option<Uuid>>("personal_owner_id"),
        Some(creator),
        "the personal pointer names the creator it was minted for"
    );

    // A second call must find the SAME organization and project rather than
    // minting a second personal one.
    let again = organizations::ensure_personal_project(&fx.registry, creator)
        .await
        .expect("idempotent");
    assert_eq!(again, project, "ensure_personal_project must be idempotent");

    let successor = seed_user(&fx.pg, "successor").await;
    organizations::add_member(
        &fx.registry,
        creator,
        &organization_id,
        &AddMemberBody {
            user_id: successor,
            role: "developer".to_string(),
        },
        None,
    )
    .await
    .expect("seat successor");
    organizations::transfer_ownership(
        &fx.registry,
        creator,
        &organization_id,
        &TransferOwnershipBody { user_id: successor },
        None,
    )
    .await
    .expect("transfer");

    let after = fx
        .pg
        .query(
            "SELECT personal_owner_id FROM zeroship.organizations WHERE id = $1",
            &[&organization_id],
        )
        .await
        .expect("re-read organization");
    assert_eq!(
        after[0].get::<_, Option<Uuid>>("personal_owner_id"),
        None,
        "a transferred personal organization is an ordinary shared one"
    );

    let pg = &fx.pg;
    let _ = pg
        .execute(
            "DELETE FROM zeroship.projects WHERE organization_id = $1",
            &[&organization_id],
        )
        .await;
    let _ = pg
        .execute(
            "DELETE FROM zeroship.organizations WHERE id = $1",
            &[&organization_id],
        )
        .await;
    for user in [creator, successor] {
        let _ = pg
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user])
            .await;
    }
}

// ---------------------------------------------------------------------------
// Invites
// ---------------------------------------------------------------------------

/// The half the schema explicitly could NOT enforce: an invite issued by an
/// admin who has since been demoted names a role its issuer can no longer
/// grant, and redemption must re-derive their LIVE rank and refuse.
///
/// The migration's own comment says this: "The CHECK closes escalation at ISSUE
/// time structurally; redemption must re-derive the inviter's live rank." This
/// is that sentence, made a test.
#[compio::test]
async fn a_demoted_inviter_cannot_seat_by_a_pending_invite() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let mut org = Org::new(&fx, "invorg").await;
    let admin = org.seat(&fx, "admin", "admin").await;
    let joiner = seed_user(&fx.pg, "joiner").await;
    let joiner_email = email_of(&fx.pg, joiner).await;

    let created = organizations::create_invite(
        &fx.registry,
        admin,
        &org.id,
        &CreateInviteBody {
            email: joiner_email.clone(),
            role: "developer".to_string(),
        },
        None,
    )
    .await
    .expect("an admin may invite a developer");

    // Demote the inviter. The invite row still carries the FROZEN rank pair.
    organizations::change_member_role(
        &fx.registry,
        org.owner,
        &org.id,
        admin,
        &ChangeRoleBody {
            role: "viewer".to_string(),
        },
        None,
    )
    .await
    .expect("owner demotes the admin");

    let err = organizations::redeem_invite(&fx.registry, joiner, &created.token, None)
        .await
        .expect_err("the inviter no longer holds the authority they issued under");
    assert!(
        matches!(err, OrganizationError::InviteNotRedeemable),
        "{err:?}"
    );
    assert_eq!(
        org.role_of(&fx, joiner).await,
        None,
        "a refused redemption must seat nobody"
    );

    // The CONTROL: an invite from someone whose authority is intact redeems.
    // Only the inviter's live rank differs.
    let second = organizations::create_invite(
        &fx.registry,
        org.owner,
        &org.id,
        &CreateInviteBody {
            email: joiner_email,
            role: "developer".to_string(),
        },
        None,
    )
    .await
    .expect_err("an unconsumed invite to that address already exists");
    assert!(
        matches!(second, OrganizationError::Invalid(_)),
        "{second:?}"
    );

    organizations::revoke_invite(&fx.registry, org.owner, &org.id, &created.invite.id, None)
        .await
        .expect("owner revokes the stale invite");
    let fresh = organizations::create_invite(
        &fx.registry,
        org.owner,
        &org.id,
        &CreateInviteBody {
            email: email_of(&fx.pg, joiner).await,
            role: "developer".to_string(),
        },
        None,
    )
    .await
    .expect("owner issues a fresh invite");
    organizations::redeem_invite(&fx.registry, joiner, &fresh.token, None)
        .await
        .expect("an invite from a live authority redeems");
    assert_eq!(org.role_of(&fx, joiner).await.as_deref(), Some("developer"));

    org.seeded.push(joiner);
    org.cleanup(&fx).await;
}

/// The token is a secret, so a wrong one is indistinguishable from a used one -
/// and a redemption by the wrong ACCOUNT is refused even with the right token.
#[compio::test]
async fn an_invite_seats_only_the_address_it_names() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let mut org = Org::new(&fx, "addrorg").await;
    let invited = seed_user(&fx.pg, "invited").await;
    let bystander = seed_user(&fx.pg, "bystander").await;

    let created = organizations::create_invite(
        &fx.registry,
        org.owner,
        &org.id,
        &CreateInviteBody {
            email: email_of(&fx.pg, invited).await,
            role: "viewer".to_string(),
        },
        None,
    )
    .await
    .expect("invite");

    // A forwarded mail is not a transfer of the invitation.
    let err = organizations::redeem_invite(&fx.registry, bystander, &created.token, None)
        .await
        .expect_err("the token alone must not seat whoever holds it");
    assert!(
        matches!(err, OrganizationError::InviteNotRedeemable),
        "{err:?}"
    );
    assert_eq!(org.role_of(&fx, bystander).await, None);

    // A token that names no invite reports the SAME thing, so the endpoint is
    // not an oracle over other people's pending invitations.
    let unknown = organizations::redeem_invite(&fx.registry, invited, "not-a-real-token", None)
        .await
        .expect_err("an unknown token is refused");
    assert!(
        matches!(unknown, OrganizationError::InviteNotRedeemable),
        "{unknown:?}"
    );

    // The control: the named address redeems the same token.
    organizations::redeem_invite(&fx.registry, invited, &created.token, None)
        .await
        .expect("the invited address redeems");
    assert_eq!(org.role_of(&fx, invited).await.as_deref(), Some("viewer"));

    // And the invite is spent: a second redemption of the same token fails.
    let replay = organizations::redeem_invite(&fx.registry, invited, &created.token, None)
        .await
        .expect_err("an invite is single-use");
    assert!(
        matches!(replay, OrganizationError::InviteNotRedeemable),
        "{replay:?}"
    );

    let _ = fx
        .pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&bystander])
        .await;
    org.seeded.push(invited);
    org.cleanup(&fx).await;
}

// ---------------------------------------------------------------------------
// Projects: the narrowing, end to end
// ---------------------------------------------------------------------------

/// A developer holds authority ONLY where a `project_members` row exists, and
/// the row grants at most their organization rank.
#[compio::test]
async fn a_project_seat_grants_and_ceilings_but_never_widens() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let mut org = Org::new(&fx, "prjorg").await;
    let developer = org.seat(&fx, "developer", "developer").await;
    let default_project = org.default_project(&fx).await;

    // Before any project row: the developer reaches nothing in that project.
    let authority = zeroship_authz::authority::resolve(
        &fx.pg,
        developer,
        &zeroship_authz::Resource::Project {
            id: default_project.clone(),
        },
    )
    .await
    .expect("resolve");
    assert_eq!(
        authority.effective_rank, 0,
        "a developer with no project row reaches nothing"
    );

    // Seating them at ADMIN on the project must NOT widen them past developer.
    //
    // Admin rather than owner because the rank fence applies here too: an
    // owner may grant only STRICTLY below their own rank, so `owner` is not a
    // project role anyone can be seated at. Admin is the highest a project seat
    // can carry, and it is already above the developer being ceilinged - which
    // is all this case needs.
    organizations::add_project_member(
        &fx.registry,
        org.owner,
        &default_project,
        &AddProjectMemberBody {
            user_id: developer,
            role: "admin".to_string(),
        },
        None,
    )
    .await
    .expect("owner seats a project member");
    let authority = zeroship_authz::authority::resolve(
        &fx.pg,
        developer,
        &zeroship_authz::Resource::Project {
            id: default_project.clone(),
        },
    )
    .await
    .expect("resolve");
    assert_eq!(
        authority.effective_rank, 20,
        "min(organization developer, project admin) is developer: a project grants and \
         ceilings, never widens"
    );

    // The control on the other side of the minimum: an ADMIN reaches the same
    // project at their organization rank with no row at all.
    let admin = org.seat(&fx, "admin", "admin").await;
    let authority = zeroship_authz::authority::resolve(
        &fx.pg,
        admin,
        &zeroship_authz::Resource::Project {
            id: default_project.clone(),
        },
    )
    .await
    .expect("resolve");
    assert_eq!(authority.effective_rank, 30);

    // A developer may not seat project members: the threshold is admin, and it
    // is in the INSERT.
    let other = org.seat(&fx, "other", "developer").await;
    let err = organizations::add_project_member(
        &fx.registry,
        developer,
        &default_project,
        &AddProjectMemberBody {
            user_id: other,
            role: "viewer".to_string(),
        },
        None,
    )
    .await
    .expect_err("a developer must not grant project reach");
    assert!(matches!(err, OrganizationError::Insufficient(_)), "{err:?}");

    org.cleanup(&fx).await;
}

/// A project membership naming a user with no organization seat is unspellable
/// - the composite foreign key refuses it - and the control plane reports that
/// as the caller's mistake rather than as a database failure.
#[compio::test]
async fn a_project_seat_requires_an_organization_seat() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let org = Org::new(&fx, "fkorg").await;
    let outsider = seed_user(&fx.pg, "outsider").await;
    let default_project = org.default_project(&fx).await;

    let err = organizations::add_project_member(
        &fx.registry,
        org.owner,
        &default_project,
        &AddProjectMemberBody {
            user_id: outsider,
            role: "viewer".to_string(),
        },
        None,
    )
    .await
    .expect_err("a project seat cannot precede an organization seat");
    assert!(
        matches!(err, OrganizationError::Invalid(_)),
        "the refusal must point at the missing organization seat, got {err:?}"
    );

    let _ = fx
        .pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&outsider])
        .await;
    org.cleanup(&fx).await;
}

/// An organization update is owner-only, and the predicate is in the UPDATE.
#[compio::test]
async fn renaming_an_organization_is_reserved_to_its_owners() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let mut org = Org::new(&fx, "updorg").await;
    let admin = org.seat(&fx, "admin", "admin").await;

    let err = organizations::update_organization(
        &fx.registry,
        admin,
        &org.id,
        &UpdateOrganizationBody {
            name: Some("Renamed By Admin".to_string()),
            slug: None,
            billing_email: None,
        },
        None,
    )
    .await
    .expect_err("an admin may not rename the organization");
    assert!(matches!(err, OrganizationError::Insufficient(_)), "{err:?}");

    // The control: same call, owner instead of admin.
    let record = organizations::update_organization(
        &fx.registry,
        org.owner,
        &org.id,
        &UpdateOrganizationBody {
            name: Some("Renamed By Owner".to_string()),
            slug: None,
            billing_email: None,
        },
        None,
    )
    .await
    .expect("an owner may rename");
    assert_eq!(record.name, "Renamed By Owner");

    org.cleanup(&fx).await;
}

/// Creating a project needs admin authority, and a fresh organization already
/// carries one so the zero-config path never needs it.
#[compio::test]
async fn creating_a_project_needs_admin_authority() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let mut org = Org::new(&fx, "mkorg").await;
    let developer = org.seat(&fx, "developer", "developer").await;

    let err = organizations::create_project(
        &fx.registry,
        developer,
        &org.id,
        &CreateProjectBody {
            name: "Developer Project".to_string(),
            slug: None,
        },
        None,
    )
    .await
    .expect_err("a developer may not mint a project");
    assert!(matches!(err, OrganizationError::Insufficient(_)), "{err:?}");

    let admin = org.seat(&fx, "admin", "admin").await;
    let record = organizations::create_project(
        &fx.registry,
        admin,
        &org.id,
        &CreateProjectBody {
            name: "Admin Project".to_string(),
            slug: None,
        },
        None,
    )
    .await
    .expect("an admin may mint a project");
    assert_eq!(record.slug, "admin-project");
    assert_eq!(record.organization_id, org.id);

    org.cleanup(&fx).await;
}

/// The redemption body and the invite listing must not carry the token. The
/// create response is the ONE place it exists.
#[compio::test]
async fn a_listed_invite_never_carries_its_token() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let org = Org::new(&fx, "listorg").await;
    let joiner = seed_user(&fx.pg, "joiner").await;

    let created = organizations::create_invite(
        &fx.registry,
        org.owner,
        &org.id,
        &CreateInviteBody {
            email: email_of(&fx.pg, joiner).await,
            role: "viewer".to_string(),
        },
        None,
    )
    .await
    .expect("invite");

    let listed = organizations::list_invites(&fx.pg, &org.id)
        .await
        .expect("list");
    let serialized = serde_json::to_string(&listed).expect("serialize");
    assert!(
        !serialized.contains(&created.token),
        "an invite listing must not carry the secret"
    );
    assert!(
        serialized.contains(&created.invite.id),
        "the listing must still name the invite, or the assertion above passes vacuously"
    );

    // And the stored value is a digest, not the token.
    let rows = fx
        .pg
        .query(
            "SELECT token_hash FROM zeroship.organization_invites WHERE id = $1",
            &[&created.invite.id],
        )
        .await
        .expect("read digest");
    let digest: Vec<u8> = rows[0].get("token_hash");
    assert_ne!(digest, created.token.as_bytes().to_vec());

    let _ = fx
        .pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&joiner])
        .await;
    org.cleanup(&fx).await;
}

/// The redeem body type is exercised so the wire shape cannot drift away from
/// the store function without a compile error.
#[test]
fn the_redeem_body_names_the_token_field() {
    let parsed: RedeemInviteBody =
        serde_json::from_str(r#"{"token":"abc"}"#).expect("redeem body parses");
    assert_eq!(parsed.token, "abc");
}
