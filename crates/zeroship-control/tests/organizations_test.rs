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
use zeroship_control::billing_read::LocalInvoicing;
use zeroship_control::organizations::{
    self as organizations, AddMemberBody, AddProjectMemberBody, ChangeRoleBody, CreateInviteBody,
    CreateOrganizationBody, CreateProjectBody, OrganizationError, RedeemInviteBody,
    TransferOwnershipBody, UpdateOrganizationBody, UpdateProjectBody,
};
use zeroship_control::Registry;
use zeroship_mailer::RecordingMailer;

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

/// A developer seats nobody, and an admin seats the SAME viewer.
///
/// The strict inequality is not enough here and that is the whole point: a
/// developer outranks a viewer, so `actor.rank > target.rank` holds and the
/// INSERT matched until the `admin` floor was added to it. Until then the only
/// thing refusing this pair was the band Cedar puts on
/// `organization:members:write`, while the sibling `add_project_member` two
/// hundred lines away carried the floor in its own statement - and the module
/// header promised that Cedar is never the only fence.
///
/// The two calls differ in ONE variable, the actor's rank, so the refusal is
/// attributable to the floor and not to the role being granted.
#[compio::test]
async fn a_developer_seats_nobody_where_an_admin_seats_a_viewer() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let mut org = Org::new(&fx, "floororg").await;
    let developer = org.seat(&fx, "developer", "developer").await;
    let admin = org.seat(&fx, "admin", "admin").await;
    let target = seed_user(&fx.pg, "target").await;

    let err = organizations::add_member(
        &fx.registry,
        developer,
        &org.id,
        &AddMemberBody {
            user_id: target,
            role: "viewer".to_string(),
        },
        None,
    )
    .await
    .expect_err("a developer must seat nobody, even a viewer it outranks");
    assert!(
        matches!(err, OrganizationError::Insufficient(_)),
        "expected an authority refusal, got {err:?}"
    );
    assert_eq!(
        org.role_of(&fx, target).await,
        None,
        "the floor is in the INSERT, so a refused seating cannot have written a row"
    );

    // The control: same target, same role, an actor one rank higher.
    organizations::add_member(
        &fx.registry,
        admin,
        &org.id,
        &AddMemberBody {
            user_id: target,
            role: "viewer".to_string(),
        },
        None,
    )
    .await
    .expect("an admin may seat a viewer");
    assert_eq!(org.role_of(&fx, target).await.as_deref(), Some("viewer"));

    org.seeded.push(target);
    org.cleanup(&fx).await;
}

/// The same floor on the invite, because inviting is seating with a delay.
///
/// Without it a developer issues an invitation that redemption will honour, and
/// the escalation lands later and from a different route than the call that
/// authorized it.
#[compio::test]
async fn a_developer_invites_nobody_where_an_admin_invites_a_viewer() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let mut org = Org::new(&fx, "invfloor").await;
    let developer = org.seat(&fx, "developer", "developer").await;
    let admin = org.seat(&fx, "admin", "admin").await;
    let joiner = seed_user(&fx.pg, "joiner").await;
    let joiner_email = email_of(&fx.pg, joiner).await;

    let err = organizations::create_invite(
        &fx.registry,
        developer,
        &org.id,
        &CreateInviteBody {
            email: joiner_email.clone(),
            role: "viewer".to_string(),
        },
        None,
    )
    .await
    .expect_err("a developer must invite nobody, even at viewer");
    assert!(
        matches!(err, OrganizationError::Insufficient(_)),
        "expected an authority refusal, got {err:?}"
    );
    assert_eq!(
        pending_invites(&fx, &org.id, &joiner_email).await,
        0,
        "the floor is in the INSERT, so a refused invitation cannot have written a row"
    );

    // The control: same address, same role, an actor one rank higher.
    let issued = organizations::create_invite(
        &fx.registry,
        admin,
        &org.id,
        &CreateInviteBody {
            email: joiner_email.clone(),
            role: "viewer".to_string(),
        },
        None,
    )
    .await
    .expect("an admin may invite a viewer");
    assert_eq!(issued.invite.role, "viewer");
    assert_eq!(pending_invites(&fx, &org.id, &joiner_email).await, 1);

    let _ = fx
        .pg
        .execute(
            "DELETE FROM zeroship.organization_invites WHERE organization_id = $1",
            &[&org.id],
        )
        .await;
    org.seeded.push(joiner);
    org.cleanup(&fx).await;
}

/// How many unconsumed invitations one organization holds for one address.
async fn pending_invites(fx: &Fx, organization_id: &str, email: &str) -> i64 {
    let rows = fx
        .pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.organization_invites \
              WHERE organization_id = $1 AND email = $2::citext AND consumed_at IS NULL",
            &[&organization_id, &email],
        )
        .await
        .expect("count invites");
    rows[0].get("n")
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

// ---------------------------------------------------------------------------
// Self-departure: the one carve-out in the rank model
// ---------------------------------------------------------------------------

/// A viewer can leave, and the SAME viewer cannot remove anybody else.
///
/// The pair is the whole point of the carve-out. `leave_organization` takes no
/// target, so the first call is a statement about the actor's own row; the
/// second is `remove_member` with the general inequality untouched, and it
/// refuses. If the carve-out had been implemented by relaxing that inequality,
/// the second half of this test would pass and an admin could demote a peer.
#[compio::test]
async fn a_member_may_leave_and_still_may_not_remove_anyone_else() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let mut org = Org::new(&fx, "leaveorg").await;
    let viewer = org.seat(&fx, "viewer", "viewer").await;
    let peer = org.seat(&fx, "peer", "viewer").await;
    let bystander = org.seat(&fx, "bystander", "viewer").await;

    // The case: a viewer, who holds no members:write at any rank and outranks
    // nobody including themselves, gives up their own seat.
    organizations::leave_organization(&fx.registry, viewer, &org.id, None)
        .await
        .expect("a viewer may give up their own seat");
    assert_eq!(
        org.role_of(&fx, viewer).await,
        None,
        "the seat must actually be gone"
    );

    // The CONTROL, differing in one variable - the target. Same actor rank,
    // same organization, a row that is not the actor's own. The general
    // inequality is untouched, so it refuses.
    let err = organizations::remove_member(&fx.registry, peer, &org.id, bystander, None)
        .await
        .expect_err("a viewer must not remove a peer at the same rank");
    assert!(
        matches!(err, OrganizationError::Insufficient(_)),
        "{err:?}"
    );
    assert_eq!(
        org.role_of(&fx, bystander).await.as_deref(),
        Some("viewer"),
        "a refused removal must change nothing"
    );

    // And `remove_member` is not a second way to leave: an actor never
    // outranks their own rank, which is exactly the inequality the carve-out
    // does NOT relax.
    let err = organizations::remove_member(&fx.registry, peer, &org.id, peer, None)
        .await
        .expect_err("remove_member must not be a way to leave");
    assert!(
        matches!(err, OrganizationError::Insufficient(_)),
        "{err:?}"
    );
    assert_eq!(
        org.role_of(&fx, peer).await.as_deref(),
        Some("viewer"),
        "the peer's own seat must survive a call to the wrong route"
    );

    org.cleanup(&fx).await;
}

/// A sole owner is refused, and told to transfer first; a co-owner is not.
///
/// The last-owner rule is not relaxed by the carve-out. The second half is the
/// control that makes the first half a result rather than "leaving never
/// works": after `transfer_ownership` the previous owner is an admin and walks
/// out without argument.
#[compio::test]
async fn the_last_owner_cannot_walk_out_and_is_told_the_remedy() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let mut org = Org::new(&fx, "soleowner").await;
    let heir = org.seat(&fx, "heir", "admin").await;

    let err = organizations::leave_organization(&fx.registry, org.owner, &org.id, None)
        .await
        .expect_err("the only owner must not be able to leave");
    assert!(matches!(err, OrganizationError::LastOwner), "{err:?}");
    assert_eq!(org.owner_count(&fx).await, 1);

    // The remedy the refusal names, followed exactly.
    organizations::transfer_ownership(
        &fx.registry,
        org.owner,
        &org.id,
        &TransferOwnershipBody { user_id: heir },
        None,
    )
    .await
    .expect("transfer ownership to the heir");

    organizations::leave_organization(&fx.registry, org.owner, &org.id, None)
        .await
        .expect("a stepped-down owner may leave");
    assert_eq!(
        org.role_of(&fx, org.owner).await,
        None,
        "the former owner's seat is gone"
    );
    assert_eq!(
        org.owner_count(&fx).await,
        1,
        "the organization still has exactly one owner"
    );

    org.cleanup(&fx).await;
}

/// Leaving an organization you hold no seat in is a 404, not a silent success.
#[compio::test]
async fn leaving_without_a_seat_is_not_found() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let org = Org::new(&fx, "strangerorg").await;
    let stranger = seed_user(&fx.pg, "stranger").await;

    let err = organizations::leave_organization(&fx.registry, stranger, &org.id, None)
        .await
        .expect_err("a stranger holds nothing to give up");
    assert!(matches!(err, OrganizationError::MemberNotFound), "{err:?}");

    let _ = fx
        .pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&stranger])
        .await;
    org.cleanup(&fx).await;
}

// ---------------------------------------------------------------------------
// Project lifecycle
// ---------------------------------------------------------------------------

/// A project is renamed and re-slugged in one call, and a developer cannot.
#[compio::test]
async fn renaming_a_project_needs_admin_authority() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let mut org = Org::new(&fx, "renameprj").await;
    let developer = org.seat(&fx, "dev", "developer").await;
    let project = org.default_project(&fx).await;

    let err = organizations::update_project(
        &fx.registry,
        developer,
        &project,
        &UpdateProjectBody {
            name: Some("Stolen".to_string()),
            slug: None,
        },
        None,
    )
    .await
    .expect_err("a developer must not rename a project");
    assert!(
        matches!(err, OrganizationError::Insufficient(_)),
        "{err:?}"
    );

    // The control: the same call by the owner, differing only in the actor.
    let renamed = organizations::update_project(
        &fx.registry,
        org.owner,
        &project,
        &UpdateProjectBody {
            name: Some("Checkout".to_string()),
            slug: Some("checkout".to_string()),
        },
        None,
    )
    .await
    .expect("an owner renames a project");
    assert_eq!(renamed.name, "Checkout");
    assert_eq!(renamed.slug, "checkout");

    // An empty body is refused rather than being a no-op that reports success.
    let err = organizations::update_project(
        &fx.registry,
        org.owner,
        &project,
        &UpdateProjectBody {
            name: None,
            slug: None,
        },
        None,
    )
    .await
    .expect_err("an update naming nothing is a bad request");
    assert!(matches!(err, OrganizationError::Invalid(_)), "{err:?}");

    org.cleanup(&fx).await;
}

/// A project that still owns an app is not deleted, and the refusal counts them.
///
/// The predicate rides in the DELETE, so this binds the refusal rather than the
/// `apps_project_id_fkey` backstop: what a caller reads is a count and a
/// remedy, not a constraint name.
#[compio::test]
async fn a_project_owning_an_app_is_not_deleted() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let org = Org::new(&fx, "prjapps").await;
    let project = org.default_project(&fx).await;
    let app_name = format!("lifecycle{}", Uuid::new_v4().simple());
    common::ensure_builtin_plans(&fx.registry).await;
    // The plan id is READ from the catalog, never spelled: `apps.plan_id` is a
    // foreign key onto `zeroship.plans`, whose ids are typed (`pln_...`), and a
    // literal "free" is a fixture that refuses at the constraint.
    let plan = zeroship_control::plan_catalog::free_plan_id();
    fx.pg
        .execute(
            "INSERT INTO zeroship.apps (name, plan_id, project_id, organization_id) \
             VALUES ($1, $2, $3, $4)",
            &[&app_name, &plan, &project, &org.id],
        )
        .await
        .expect("seed an app in the project");

    let err = organizations::delete_project(&fx.registry, org.owner, &project, None)
        .await
        .expect_err("a project owning an app must not be deleted");
    match err {
        OrganizationError::ProjectHasApps(n) => assert_eq!(n, 1, "the refusal counts the apps"),
        other => panic!("{other:?}"),
    }

    // The CONTROL: remove the one app and the SAME call succeeds. Without it,
    // an implementation that refused every deletion would pass the assertion
    // above.
    fx.pg
        .execute("DELETE FROM zeroship.apps WHERE name = $1", &[&app_name])
        .await
        .expect("drop the app");
    organizations::delete_project(&fx.registry, org.owner, &project, None)
        .await
        .expect("an empty project is deleted");
    assert!(
        organizations::get_project(&fx.pg, &project).await.is_err(),
        "the project row must be gone"
    );

    org.cleanup(&fx).await;
}

/// Deleting a project takes its project seats with it, and refuses a developer.
#[compio::test]
async fn deleting_a_project_needs_admin_and_takes_its_seats() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let mut org = Org::new(&fx, "prjdelete").await;
    let developer = org.seat(&fx, "dev", "developer").await;
    let project = org.default_project(&fx).await;
    organizations::add_project_member(
        &fx.registry,
        org.owner,
        &project,
        &AddProjectMemberBody {
            user_id: developer,
            role: "viewer".to_string(),
        },
        None,
    )
    .await
    .expect("seat the developer on the project");

    let err = organizations::delete_project(&fx.registry, developer, &project, None)
        .await
        .expect_err("a developer must not delete a project");
    assert!(
        matches!(err, OrganizationError::Insufficient(_)),
        "{err:?}"
    );

    organizations::delete_project(&fx.registry, org.owner, &project, None)
        .await
        .expect("an owner deletes an empty project");
    let seats = fx
        .pg
        .query(
            "SELECT 1 FROM zeroship.project_members WHERE project_id = $1",
            &[&project],
        )
        .await
        .expect("read project seats");
    assert!(seats.is_empty(), "the project seats cascade with the project");

    org.cleanup(&fx).await;
}

/// A project seat NARROWS in one statement, never by delete-then-add.
#[compio::test]
async fn a_project_seat_is_narrowed_atomically() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let mut org = Org::new(&fx, "prjrole").await;
    let developer = org.seat(&fx, "dev", "developer").await;
    let project = org.default_project(&fx).await;
    organizations::add_project_member(
        &fx.registry,
        org.owner,
        &project,
        &AddProjectMemberBody {
            user_id: developer,
            role: "developer".to_string(),
        },
        None,
    )
    .await
    .expect("seat the developer");

    let moved = organizations::change_project_member_role(
        &fx.registry,
        org.owner,
        &project,
        developer,
        &ChangeRoleBody {
            role: "viewer".to_string(),
        },
        None,
    )
    .await
    .expect("an owner narrows a project seat");
    assert_eq!(moved.role, "viewer");

    // The row was MOVED, not replaced: `added_at` survives, which is what a
    // delete-then-add cannot preserve.
    let rows = fx
        .pg
        .query(
            "SELECT role, added_at FROM zeroship.project_members \
              WHERE project_id = $1 AND user_id = $2",
            &[&project, &developer],
        )
        .await
        .expect("read the seat");
    assert_eq!(rows.len(), 1, "exactly one seat, never zero and never two");
    assert_eq!(rows[0].get::<_, String>("role"), "viewer");
    assert_eq!(
        rows[0].get::<_, chrono::DateTime<chrono::Utc>>("added_at"),
        moved.added_at,
        "the seat kept its original added_at, so it was moved rather than reseated"
    );

    // The rank fence still applies: the developer cannot re-widen their own
    // project seat, because they do not clear the admin threshold.
    let err = organizations::change_project_member_role(
        &fx.registry,
        developer,
        &project,
        developer,
        &ChangeRoleBody {
            role: "owner".to_string(),
        },
        None,
    )
    .await
    .expect_err("a developer must not re-role a project seat");
    assert!(
        matches!(err, OrganizationError::Insufficient(_)),
        "{err:?}"
    );

    // And a member with no project row is a 404 rather than a silent grant.
    let stranger = org.seat(&fx, "stranger", "viewer").await;
    let err = organizations::change_project_member_role(
        &fx.registry,
        org.owner,
        &project,
        stranger,
        &ChangeRoleBody {
            role: "viewer".to_string(),
        },
        None,
    )
    .await
    .expect_err("there is no seat to move");
    assert!(matches!(err, OrganizationError::MemberNotFound), "{err:?}");

    org.cleanup(&fx).await;
}

// ---------------------------------------------------------------------------
// Dissolution
// ---------------------------------------------------------------------------

/// An organization is closed only once it owns no projects, and the refusal
/// names how many remain.
#[compio::test]
async fn an_organization_with_projects_is_not_dissolved() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let org = Org::new(&fx, "dissolveorg").await;
    let project = org.default_project(&fx).await;

    let err = organizations::dissolve_organization(&fx.registry, org.owner, &org.id, LocalInvoicing::Yes, None)
        .await
        .expect_err("a project remains");
    match err {
        OrganizationError::OrganizationHasProjects(n) => {
            assert_eq!(n, 1, "the refusal counts the projects");
        }
        other => panic!("{other:?}"),
    }

    // The CONTROL: the remedy named in the refusal, then the same call.
    organizations::delete_project(&fx.registry, org.owner, &project, None)
        .await
        .expect("delete the default project");
    let closed = organizations::dissolve_organization(&fx.registry, org.owner, &org.id, LocalInvoicing::Yes, None)
        .await
        .expect("an empty organization is closed");
    assert!(closed.dissolved_at.is_some());

    org.cleanup(&fx).await;
}

/// A dissolved organization is still READABLE and accepts no further change.
///
/// Every mutation goes through `lock_organization`, so this drives one of each
/// KIND - a rename, a membership write, a project create, a departure and a
/// second dissolve - rather than trusting that they all share the fence.
#[compio::test]
async fn a_dissolved_organization_reads_and_refuses_every_change() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let mut org = Org::new(&fx, "closedorg").await;
    let member = org.seat(&fx, "member", "developer").await;
    let project = org.default_project(&fx).await;
    organizations::delete_project(&fx.registry, org.owner, &project, None)
        .await
        .expect("empty the organization");
    let closed = organizations::dissolve_organization(&fx.registry, org.owner, &org.id, LocalInvoicing::Yes, None)
        .await
        .expect("close it");
    let at = closed.dissolved_at.expect("a close carries its date");

    // Readable, and the timestamp is what a reader sees.
    let read = organizations::get_organization(&fx.pg, &org.id)
        .await
        .expect("a closed organization is still readable");
    assert_eq!(read.dissolved_at, Some(at));
    let listed = organizations::list_organizations(&fx.pg, org.owner)
        .await
        .expect("list");
    assert!(
        listed.iter().any(|o| o.id == org.id && o.dissolved_at.is_some()),
        "a closed organization stays in its members' listing, marked closed"
    );
    assert_eq!(
        organizations::list_members(&fx.pg, &org.id)
            .await
            .expect("members")
            .len(),
        2,
        "the members survive the close"
    );

    let dissolved = |err: &OrganizationError| matches!(err, OrganizationError::Dissolved(_));

    let err = organizations::update_organization(
        &fx.registry,
        org.owner,
        &org.id,
        &UpdateOrganizationBody {
            name: Some("Reopened".to_string()),
            slug: None,
            billing_email: None,
        },
        None,
    )
    .await
    .expect_err("a closed organization cannot be renamed");
    assert!(dissolved(&err), "{err:?}");

    let newcomer = seed_user(&fx.pg, "newcomer").await;
    let err = organizations::add_member(
        &fx.registry,
        org.owner,
        &org.id,
        &AddMemberBody {
            user_id: newcomer,
            role: "viewer".to_string(),
        },
        None,
    )
    .await
    .expect_err("a closed organization seats nobody");
    assert!(dissolved(&err), "{err:?}");

    let err = organizations::create_project(
        &fx.registry,
        org.owner,
        &org.id,
        &CreateProjectBody {
            name: "Revival".to_string(),
            slug: None,
        },
        None,
    )
    .await
    .expect_err("a closed organization holds no new projects");
    assert!(dissolved(&err), "{err:?}");

    let err = organizations::leave_organization(&fx.registry, member, &org.id, None)
        .await
        .expect_err("a closed record does not change, including by departure");
    assert!(dissolved(&err), "{err:?}");

    let err = organizations::dissolve_organization(&fx.registry, org.owner, &org.id, LocalInvoicing::Yes, None)
        .await
        .expect_err("closing twice reports the first close");
    match err {
        OrganizationError::Dissolved(reported) => assert_eq!(reported, at),
        other => panic!("{other:?}"),
    }

    let _ = fx
        .pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&newcomer])
        .await;
    org.cleanup(&fx).await;
}

/// Only an owner closes an organization.
#[compio::test]
async fn closing_an_organization_is_reserved_to_its_owners() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let mut org = Org::new(&fx, "adminclose").await;
    let admin = org.seat(&fx, "admin", "admin").await;
    let project = org.default_project(&fx).await;
    organizations::delete_project(&fx.registry, org.owner, &project, None)
        .await
        .expect("empty it first, so the refusal below is about rank");

    let err = organizations::dissolve_organization(&fx.registry, admin, &org.id, LocalInvoicing::Yes, None)
        .await
        .expect_err("an admin must not close an organization");
    assert!(
        matches!(err, OrganizationError::Insufficient(_)),
        "{err:?}"
    );
    assert!(
        organizations::get_organization(&fx.pg, &org.id)
            .await
            .expect("still there")
            .dissolved_at
            .is_none(),
        "a refused close must leave the organization open"
    );

    // The control: the owner, same organization, same state.
    organizations::dissolve_organization(&fx.registry, org.owner, &org.id, LocalInvoicing::Yes, None)
        .await
        .expect("the owner closes it");

    org.cleanup(&fx).await;
}

/// Closing a PERSONAL organization frees the pointer, so the creator's next
/// first deploy mints a fresh one instead of landing on a closed record.
///
/// This is the case that would brick an account: `ensure_personal_project`
/// resolves through `personal_owner_id`, and a closed row still holding it
/// would answer every deploy with a refusal the creator could not clear -- the
/// partial unique index would stop them minting a replacement.
#[compio::test]
async fn closing_a_personal_organization_frees_the_creator_to_start_again() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let owner = seed_user(&fx.pg, "solo").await;

    let first = organizations::ensure_personal_project(&fx.registry, owner)
        .await
        .expect("first deploy mints a personal organization");
    let first_organization: String = fx
        .pg
        .query(
            "SELECT organization_id FROM zeroship.projects WHERE id = $1",
            &[&first.as_str()],
        )
        .await
        .expect("read the project's organization")[0]
        .get("organization_id");
    assert_eq!(
        organizations::get_organization(&fx.pg, &first_organization)
            .await
            .expect("read it")
            .personal_owner_id,
        Some(owner),
        "the pointer names the creator while it is live"
    );

    organizations::delete_project(&fx.registry, owner, first.as_str(), None)
        .await
        .expect("empty the personal organization first");
    organizations::dissolve_organization(&fx.registry, owner, &first_organization, LocalInvoicing::Yes, None)
        .await
        .expect("the creator closes their personal organization");
    // The pointer is KEPT: the record stays truthful about what it was. What
    // frees the slot is `dissolved_at IS NULL` in the unique index and in the
    // read, not a column edit.
    assert_eq!(
        organizations::get_organization(&fx.pg, &first_organization)
            .await
            .expect("still readable")
            .personal_owner_id,
        Some(owner),
        "a closed personal organization still records whose it was"
    );

    // The whole point: the next deploy works, and lands somewhere new.
    let second = organizations::ensure_personal_project(&fx.registry, owner)
        .await
        .expect("a creator who closed one workspace can still deploy");
    assert_ne!(
        second.as_str(),
        first.as_str(),
        "the second deploy must not resolve back into the closed organization"
    );

    let _ = fx
        .pg
        .execute(
            "DELETE FROM zeroship.projects WHERE organization_id IN \
               (SELECT id FROM zeroship.organizations WHERE created_by = $1 \
                   OR personal_owner_id = $1)",
            &[&owner],
        )
        .await;
    let _ = fx
        .pg
        .execute(
            "DELETE FROM zeroship.organizations WHERE created_by = $1 OR personal_owner_id = $1",
            &[&owner],
        )
        .await;
    let _ = fx
        .pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&owner])
        .await;
}

// ---------------------------------------------------------------------------
// Invitation delivery
// ---------------------------------------------------------------------------

/// The invitation is mailed, the outcome is recorded, and the mail carries the
/// token that only this one message and the create response ever hold.
#[compio::test]
async fn an_invitation_is_mailed_and_the_outcome_recorded() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let org = Org::new(&fx, "mailorg").await;
    let joiner = seed_user(&fx.pg, "joiner").await;
    let joiner_email = email_of(&fx.pg, joiner).await;
    let mailer = RecordingMailer::new();

    let created = organizations::create_and_deliver_invite(
        &fx.registry,
        &fx.pg,
        &mailer,
        org.owner,
        &org.id,
        &CreateInviteBody {
            email: joiner_email.clone(),
            role: "developer".to_string(),
        },
        None,
    )
    .await
    .expect("invite");

    assert_eq!(created.delivery, organizations::InviteDelivery::Sent);
    assert_eq!(delivery_of(&fx, &created.invite.id).await.as_deref(), Some("sent"));

    let sent = mailer.sent_to(&joiner_email);
    assert_eq!(sent.len(), 1, "exactly one message to the invited address");
    let message = &sent[0];
    assert!(
        message.text.contains(&created.token),
        "the recipient's copy must carry the token; it exists nowhere else"
    );
    assert!(
        message.html.as_deref().is_some_and(|html| html.contains(&created.token)),
        "and so must the HTML part, or a HTML-only client gets an unusable mail"
    );
    // The mail names the ORGANIZATION and the INVITER, both resolved by
    // `invite_mail_context`. Asserting only that the subject says "zeroship"
    // would still pass with the organization id in place of its name, which is
    // exactly what that function's fallback produces when its query goes wrong.
    let record = organizations::get_organization(&fx.pg, &org.id)
        .await
        .expect("read the organization");
    assert!(
        message.subject.contains(&record.name),
        "the subject must name the organization, got {:?}",
        message.subject
    );
    assert!(
        !message.subject.contains(&org.id),
        "the id is the fallback, not the name: {:?}",
        message.subject
    );
    assert!(
        message.text.contains(&invite_role_line(&created.invite.role)),
        "the body must name the role being offered: {:?}",
        message.text
    );
    // The digest is stored; the plaintext is not. This is the assertion a
    // refactor that "helpfully" persisted the token would fail.
    let rows = fx
        .pg
        .query(
            "SELECT token_hash FROM zeroship.organization_invites WHERE id = $1",
            &[&created.invite.id],
        )
        .await
        .expect("read digest");
    assert_ne!(
        rows[0].get::<_, Vec<u8>>("token_hash"),
        created.token.as_bytes().to_vec()
    );

    let _ = fx
        .pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&joiner])
        .await;
    org.cleanup(&fx).await;
}

/// A send that FAILS leaves a usable invitation and records `failed`.
///
/// The whole reason the row is committed before the attempt: a transport error
/// must cost the platform an email, not an invitation. The control is that the
/// same token still redeems.
#[compio::test]
async fn a_failed_send_records_failure_and_keeps_the_invitation_usable() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let org = Org::new(&fx, "failmail").await;
    let joiner = seed_user(&fx.pg, "joiner").await;
    let joiner_email = email_of(&fx.pg, joiner).await;
    let mailer = RecordingMailer::new();
    mailer.fail_transport("smtp: connection refused");

    let created = organizations::create_and_deliver_invite(
        &fx.registry,
        &fx.pg,
        &mailer,
        org.owner,
        &org.id,
        &CreateInviteBody {
            email: joiner_email.clone(),
            role: "developer".to_string(),
        },
        None,
    )
    .await
    .expect("a transport failure is not a failed request");

    assert_eq!(created.delivery, organizations::InviteDelivery::Failed);
    assert!(!created.delivery.reached_the_recipient());
    assert_eq!(
        delivery_of(&fx, &created.invite.id).await.as_deref(),
        Some("failed"),
        "the row must say the recipient never got it"
    );
    assert!(
        mailer.sent_to(&joiner_email).is_empty(),
        "nothing was delivered"
    );

    // The invitation survived the failure and is still the real thing.
    organizations::redeem_invite(&fx.registry, joiner, &created.token, None)
        .await
        .expect("the token from a failed send still redeems");
    assert_eq!(
        org.role_of(&fx, joiner).await.as_deref(),
        Some("developer")
    );

    let _ = fx
        .pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&joiner])
        .await;
    org.cleanup(&fx).await;
}

/// A suppressed address is not mailed, and that is recorded as its own outcome.
///
/// The suppression is enforced by the `Mailer` contract, against the real
/// `zeroship.email_suppressions` table, so this test inserts a row rather than
/// configuring a fake. The paired control - the same mailer, an address that is
/// not suppressed - is what makes the refusal attributable to the suppression.
#[compio::test]
async fn a_suppressed_address_is_not_mailed_and_says_so() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let org = Org::new(&fx, "suppressed").await;
    let blocked = seed_user(&fx.pg, "blocked").await;
    let blocked_email = email_of(&fx.pg, blocked).await;
    let allowed = seed_user(&fx.pg, "allowed").await;
    let allowed_email = email_of(&fx.pg, allowed).await;
    fx.pg
        .execute(
            "INSERT INTO zeroship.email_suppressions (email, reason) \
             VALUES ($1::citext, 'hard_bounce') ON CONFLICT (email) DO NOTHING",
            &[&blocked_email],
        )
        .await
        .expect("suppress the address");
    let mailer = RecordingMailer::new();

    let refused = organizations::create_and_deliver_invite(
        &fx.registry,
        &fx.pg,
        &mailer,
        org.owner,
        &org.id,
        &CreateInviteBody {
            email: blocked_email.clone(),
            role: "viewer".to_string(),
        },
        None,
    )
    .await
    .expect("the invitation is still created");
    assert_eq!(refused.delivery, organizations::InviteDelivery::Suppressed);
    assert_eq!(
        delivery_of(&fx, &refused.invite.id).await.as_deref(),
        Some("suppressed")
    );
    assert!(mailer.sent_to(&blocked_email).is_empty());

    // The CONTROL: same organization, same mailer, one variable changed.
    let delivered = organizations::create_and_deliver_invite(
        &fx.registry,
        &fx.pg,
        &mailer,
        org.owner,
        &org.id,
        &CreateInviteBody {
            email: allowed_email.clone(),
            role: "viewer".to_string(),
        },
        None,
    )
    .await
    .expect("invite");
    assert_eq!(delivered.delivery, organizations::InviteDelivery::Sent);
    assert_eq!(mailer.sent_to(&allowed_email).len(), 1);

    let _ = fx
        .pg
        .execute(
            "DELETE FROM zeroship.email_suppressions WHERE email = $1::citext",
            &[&blocked_email],
        )
        .await;
    for user in [blocked, allowed] {
        let _ = fx
            .pg
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user])
            .await;
    }
    org.cleanup(&fx).await;
}

/// How the template names the offered role. Written once so the assertion and
/// the template cannot disagree by whitespace.
fn invite_role_line(role: &str) -> String {
    format!("as {role}.")
}

/// `organization_invites.delivery`, or `None` while nothing has resolved.
async fn delivery_of(fx: &Fx, invite_id: &str) -> Option<String> {
    let rows = fx
        .pg
        .query(
            "SELECT delivery FROM zeroship.organization_invites WHERE id = $1",
            &[&invite_id],
        )
        .await
        .expect("read delivery");
    rows.first().and_then(|row| row.get("delivery"))
}

/// The delivery vocabulary is exactly the one the schema's CHECK admits.
///
/// A fourth variant, or a renamed one, would be refused by
/// `organization_invites_delivery_check` at UPDATE time - which is a warning
/// logged and swallowed, not a failed request, so nothing else would notice.
#[compio::test]
async fn the_delivery_vocabulary_is_the_one_the_check_admits() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let org = Org::new(&fx, "deliveryvocab").await;
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

    for outcome in [
        organizations::InviteDelivery::Sent,
        organizations::InviteDelivery::Suppressed,
        organizations::InviteDelivery::Failed,
    ] {
        fx.pg
            .execute(
                "UPDATE zeroship.organization_invites SET delivery = $2 WHERE id = $1",
                &[&created.invite.id, &outcome.as_str()],
            )
            .await
            .unwrap_or_else(|err| panic!("{outcome:?} must satisfy the CHECK: {err}"));
    }
    // The control: a word outside the vocabulary is refused, so the loop above
    // is a result rather than a CHECK that admits anything.
    let refused = fx
        .pg
        .execute(
            "UPDATE zeroship.organization_invites SET delivery = 'queued' WHERE id = $1",
            &[&created.invite.id],
        )
        .await;
    assert!(refused.is_err(), "the CHECK must refuse an unknown outcome");

    let _ = fx
        .pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&joiner])
        .await;
    org.cleanup(&fx).await;
}

/// A closed organization releases its slug, and a live one still holds it.
///
/// The pair is what makes this a statement about `dissolved_at` rather than
/// about uniqueness in general: the SAME slug is refused while the first
/// organization is open and accepted once it is closed.
#[compio::test]
async fn a_closed_organization_releases_its_slug() {
    let Some(fx) = Fx::new().await else {
        return;
    };
    let owner = seed_user(&fx.pg, "slugowner").await;
    let slug = format!("acme-{}", Uuid::new_v4().simple());
    let body = || CreateOrganizationBody {
        name: "Acme".to_string(),
        slug: Some(slug.clone()),
        billing_email: None,
    };

    let first = organizations::create_organization(&fx.registry, owner, &body(), None)
        .await
        .expect("mint the first organization");

    // While it is live the name is taken.
    let err = organizations::create_organization(&fx.registry, owner, &body(), None)
        .await
        .expect_err("a live organization holds its slug");
    assert!(matches!(err, OrganizationError::SlugTaken(_)), "{err:?}");

    let project = fx
        .pg
        .query(
            "SELECT id FROM zeroship.projects WHERE organization_id = $1",
            &[&first.id],
        )
        .await
        .expect("read the default project")[0]
        .get::<_, String>("id");
    organizations::delete_project(&fx.registry, owner, &project, None)
        .await
        .expect("empty it");
    organizations::dissolve_organization(&fx.registry, owner, &first.id, LocalInvoicing::Yes, None)
        .await
        .expect("close it");

    // The CASE: one variable changed - the first organization is now closed.
    let second = organizations::create_organization(&fx.registry, owner, &body(), None)
        .await
        .expect("a closed organization does not hold a live namespace");
    assert_ne!(second.id, first.id);
    assert_eq!(second.slug, slug);
    // And the closed row kept the name it was known by.
    assert_eq!(
        organizations::get_organization(&fx.pg, &first.id)
            .await
            .expect("still readable")
            .slug,
        slug,
        "releasing the slot must not rewrite the closed record"
    );

    for organization in [&second.id, &first.id] {
        let _ = fx
            .pg
            .execute(
                "DELETE FROM zeroship.projects WHERE organization_id = $1",
                &[organization],
            )
            .await;
        let _ = fx
            .pg
            .execute(
                "DELETE FROM zeroship.organizations WHERE id = $1",
                &[organization],
            )
            .await;
    }
    let _ = fx
        .pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&owner])
        .await;
}

// ---------------------------------------------------------------------------
// The fixture seat is not allowed to be quiet
// ---------------------------------------------------------------------------

/// `common::seat_app_organization_member` must REFUSE an app it cannot reach an
/// organization from, rather than reporting success for a seat it did not make.
///
/// The statement is `INSERT ... SELECT` over the join from the app to its
/// project's organization. Over an empty result set that is a SUCCESSFUL
/// statement affecting no rows, so an app id that was never created - or one
/// whose project row is missing - used to seat nobody and say nothing. The run
/// then failed far away, as a 403 from whichever route wanted an owner, with
/// nothing pointing back at the fixture. The shell peer of this helper reads a
/// token back out of the database for the same reason; this one reads the
/// affected-row count.
///
/// The app id is a fresh UUID, so the failure is the one being bound rather
/// than a foreign key on some other column: no row is examined at all.
///
/// No `drain_pg` teardown, and it cannot have one: the body is expected to
/// PANIC, so nothing after the call runs. Its one connection outlives the test
/// the way every other case in this file's does.
#[compio::test]
#[should_panic(expected = "affected 0 row(s)")]
async fn seating_an_app_that_does_not_exist_refuses_instead_of_seating_nobody() {
    // `expect`, not a `let else` that fabricates the expected panic: this must
    // fail on a missing database rather than report the refusal it never saw.
    let fx = Fx::new().await.expect("a migrated database");
    common::seat_app_organization_member(&fx.pg, &Uuid::new_v4(), &Uuid::new_v4(), "owner").await;
}
