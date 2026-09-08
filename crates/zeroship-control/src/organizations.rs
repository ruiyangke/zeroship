//! Organizations, projects, membership and invites - the creator-facing
//! ownership surface.
//!
//! An ORGANIZATION owns projects, a project owns apps, and an app reaches its
//! organization through exactly one path (`apps.project_id ->
//! projects.organization_id`). The word is spelled in full everywhere a human
//! reads it; the abbreviation exists only inside the opaque typed-id value.
//!
//! # Authority is two integers, and it is compared where the effect happens
//!
//! `zeroship.organization_roles` is a closed ladder carrying `rank` (authority
//! over apps and the organization) and `billing_rank` (authority over money). An
//! actor may act on a target only when
//!
//! ```text
//! actor.rank > target.rank  AND  actor.billing_rank >= target.billing_rank
//! ```
//!
//! Every mutation here evaluates that comparison **inside the statement that
//! performs the effect**, against the actor's LIVE seat, in the same snapshot
//! and behind the same row lock as the write. Zero rows affected IS the
//! refusal. Cedar runs first and is a real fence, but it is never the only one:
//! a member revoked between the Cedar call and the write finds a statement that
//! matches nothing.
//!
//! # Where the two audit trails go, and why they go in opposite directions
//!
//! - The **decision** row (`zeroship.authz_decisions`, written by
//!   `zeroship_authz::enforce`) MUST NOT share the effect transaction. A refused
//!   mutation rolls its transaction back, and a decision row inside it would
//!   roll back with it - erasing the record of the refusal, which is the one
//!   record a refusal produces. So every handler calls `authz.require(...)`
//!   before opening a transaction, on the shared client.
//! - The **authority-change** row (`zeroship.app_audit`, written by
//!   [`crate::audit::log_in_tx`]) goes the other way: inside the transaction. An
//!   effect that rolled back did not happen, and a trail claiming otherwise is
//!   worse than no trail.
//!
//! # The organization row lock
//!
//! Every membership mutation takes `SELECT ... FOR UPDATE` on the
//! `zeroship.organizations` row FIRST. The lock is the mechanism, not a
//! precaution: "an organization keeps at least one owner" is NOT expressible as
//! a CHECK, because a CHECK sees one row and the claim is about a set. Two
//! concurrent owner removals each read "another owner remains" in their own
//! READ COMMITTED snapshot and both commit, leaving an ownerless organization
//! that no route can repair. Serializing them on the parent row is what makes
//! the count in the DELETE's predicate true at commit as well as at read.
//!
//! # The one place a membership row is written without a rank predicate
//!
//! Seating the FIRST owner of an organization the caller just minted
//! ([`create_organization`], [`ensure_personal_project`]). There is no prior
//! seat to outrank, and the organization is unreachable by anyone else until
//! that row exists. Every other membership write carries the comparison.
//!
//! # What the STRICT inequality costs, stated rather than worked around
//!
//! `actor.rank > target.rank` is strict, so equal ranks cannot act on each
//! other. Three consequences follow and none of them is a bug in this module:
//!
//! - An owner cannot remove or demote a co-owner. The remedy is
//!   [`transfer_ownership`], which is the one operation that reshapes the owner
//!   set.
//! - Nobody can demote or remove THEMSELVES through [`remove_member`], because
//!   an actor never outranks their own rank. Giving up your own seat is
//!   [`leave_organization`], which is a different statement reached by a
//!   different route.
//! - An admin cannot seat another admin, which is what makes
//!   `organization:members:write` safe to grant to admins at all.
//!
//! # The TWO places the strict inequality does not apply, and why each is safe
//!
//! [`transfer_ownership`] relaxes it: the actor gives up rank 40 in the SAME
//! transaction that grants it, so no authority exists after the transfer that
//! did not exist before it. It is gated on `organization:admin`, which no band
//! but the owner band permits.
//!
//! [`leave_organization`] does not relax it - it has no comparison to relax,
//! because it has no target. The route above it
//! (`DELETE /api/organizations/{id}/membership`) carries no user id in its
//! path, its body or its query, so the row it can reach is the authenticated
//! principal's and there is no argument by which it could be another. It
//! carries its own action, `organization:members:leave`, banded at viewer rank
//! rather than at admin, because a viewer holds no `members:write` at any rank
//! and a viewer is the member most likely to want out. The last-owner rule
//! still applies to it, unchanged.
//!
//! # Closing an organization
//!
//! [`dissolve_organization`] sets `dissolved_at` and nothing else is destroyed:
//! the organization is the billing subject, and a row that vanished would take
//! the counterparty out of a money record that has to outlive the relationship.
//! It refuses while any project remains, and names the remedy.
//!
//! The close is enforced in ONE place. [`lock_organization`] refuses a
//! dissolved organization, and every mutation here opens with that call, so a
//! closed organization refuses membership writes, invites, projects, renames,
//! redemption and a second close without any of them carrying its own clause.
//! A `dissolved_at IS NULL` predicate per effect statement would be the same
//! rule written a dozen times, and the thirteenth mutation is the one that
//! forgets it.

use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use compio_postgres::GenericClient;
use ntex::web::{
    self,
    types::{Json, Path, State},
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroship_authz::{Action as AuthzAction, Resource};
use zeroship_mailer::templates::OrganizationInvite;
use zeroship_mailer::{Address, Email};
use zeroship_core::invite_id::InviteId;
use zeroship_core::organization_id::OrganizationId;
use zeroship_core::project_id::ProjectId;

use crate::audit::{self, Action as AuditAction, AuditEntry};
use crate::authz_guard::AuthzGuard;
use crate::billing_read::{self, BillingRemedy, LocalInvoicing, OutstandingBilling};
use crate::env_handlers::admin_rate_limit;
use crate::http_util;
use crate::registry::{Registry, RegistryError};
use crate::AppState;

/// An organization, project, membership or invite body is a handful of short
/// strings. Far more than that and far less than anything worth streaming.
pub const ORGANIZATION_PAYLOAD_BYTES: usize = 8 * 1024;

/// The closed ladder's role names. They are a vocabulary, not data: the RANKS
/// are read from `zeroship.organization_roles` on every statement, so moving a
/// rank in a migration moves every fence here without touching this file.
pub(crate) const ROLE_OWNER: &str = "owner";
pub(crate) const ROLE_ADMIN: &str = "admin";
pub(crate) const ROLE_DEVELOPER: &str = "developer";
pub(crate) const ROLE_VIEWER: &str = "viewer";

/// The roles whose seating the consent copy reserves to `organization:admin`
/// ("Add or remove organization owners and admins"), rather than to the broader
/// `organization:members:write`.
///
/// This is a CONSENT fence, not the authority fence. The authority fence is the
/// rank comparison in the effect statement, which already refuses an admin
/// seating another admin (`30 > 30` is false). What this adds is that a token
/// carrying only `organization:members:write` cannot mint an admin even when
/// its holder is an owner - which is exactly what the scope's own wording
/// promises the user who granted it.
fn seats_a_privileged_role(role: &str) -> bool {
    role == ROLE_OWNER || role == ROLE_ADMIN
}

const MAX_NAME_CHARS: usize = 120;
const MAX_SLUG_CHARS: usize = 63;
const INVITE_TTL_DAYS: i64 = 7;

// ---------------------------------------------------------------------------
// SQL fragments that must exist exactly once
// ---------------------------------------------------------------------------

/// The actor's LIVE seat at one organization, as a correlated subquery yielding
/// `rank` and `billing_rank`.
///
/// Written once because every membership statement needs it and two spellings
/// would be two answers. The arguments are placeholder TOKENS this crate writes
/// (`"$1"`, `"$4"`), never caller data.
fn actor_seat(organization: &str, actor: &str) -> String {
    format!(
        "zeroship.organization_members actor_member \
         JOIN zeroship.organization_roles actor_role ON actor_role.role = actor_member.role \
              AND actor_member.organization_id = {organization} \
              AND actor_member.user_id = {actor}"
    )
}

/// [`zeroship_authz::effective_project_rank`], in SQL.
///
/// The narrowing has to be re-expressed here because decision 4 requires the
/// comparison to live in the statement that performs the effect, and a Rust
/// function cannot. `organization_project_rank_matches_the_authz_narrowing`
/// pins the two against each other over the whole case table on a live
/// database, including the two cases below that are easy to get wrong:
///
/// - A NULL project rank must yield 0 below admin, NOT the organization rank.
///   `LEAST(20, NULL)` is `20` in PostgreSQL - LEAST and GREATEST IGNORE nulls
///   rather than propagating them - so the obvious
///   `COALESCE(LEAST(org, project), 0)` silently WIDENS a member with no
///   project row to their full organization rank. The explicit
///   `WHEN {project_rank} IS NULL THEN 0` arm is that bug's absence.
/// - A NULL admin rank (a ladder with no `admin` row) must narrow, not widen.
pub(crate) fn effective_project_rank_sql(
    organization_rank: &str,
    project_rank: &str,
    admin_rank: &str,
) -> String {
    format!(
        "CASE \
           WHEN {organization_rank} IS NULL THEN 0 \
           WHEN {admin_rank} IS NOT NULL AND {organization_rank} >= {admin_rank} \
                THEN {organization_rank} \
           WHEN {project_rank} IS NULL THEN 0 \
           ELSE LEAST({organization_rank}, {project_rank}) \
         END"
    )
}

/// The rank of one ladder role, as a scalar subquery. `role` is a placeholder
/// token (`"$6"`), so the role name itself arrives as a bind parameter.
fn ladder_rank(role: &str) -> String {
    format!("(SELECT rank FROM zeroship.organization_roles WHERE role = {role})")
}

/// The narrowing SQL, reachable from the integration test that proves it agrees
/// with [`zeroship_authz::effective_project_rank`].
///
/// It is a separate, explicitly named entry point rather than a `pub` on the
/// real function because a caller outside this crate has no business
/// re-expressing the narrowing; the ONE legitimate outside use is comparing the
/// two implementations, and naming it that way says so.
#[must_use]
pub fn effective_project_rank_sql_for_test(
    organization_rank: &str,
    project_rank: &str,
    admin_rank: &str,
) -> String {
    effective_project_rank_sql(organization_rank, project_rank, admin_rank)
}

/// The same, for a statement that has no placeholder to spare.
///
/// `role` is always one of the [`ROLE_OWNER`] family - a compile-time constant
/// this crate owns, never caller data - which is what makes the inlined literal
/// safe. Passing anything else here would be a SQL injection, so there is no
/// public form of this function.
pub(crate) fn ladder_rank_of(role: &str) -> String {
    debug_assert!(
        [
            ROLE_OWNER,
            ROLE_ADMIN,
            ROLE_DEVELOPER,
            ROLE_VIEWER,
            "billing"
        ]
        .contains(&role),
        "ladder_rank_of inlines its argument; it takes ladder constants only"
    );
    format!("(SELECT rank FROM zeroship.organization_roles WHERE role = '{role}')")
}

/// Which owner is THE owner, when an organization holds several.
///
/// `app_members` permitted a fan-out only through a data-integrity fault, so
/// every consumer collapsed it defensively and the comments called it
/// defensive. An ORGANIZATION legitimately holds several owners, so this is now
/// the ordinary case and the tiebreak is load-bearing: the longest-standing
/// owner, with the id as a total order behind it so two owners seated in the
/// same statement still resolve deterministically.
///
/// The billing subsystems required this rule to be IDENTICAL across the sweep,
/// the notifier and the per-organization slice, and used to guarantee it by three
/// matching copies with comments asking future editors to keep them matching.
/// It is now guaranteed by there being one.
const APP_OWNER_ORDER: &str = "owner_member.added_at, owner_member.user_id";

/// The one join from an app to the human who answers for it, as a LATERAL for
/// a query that already has `zeroship.apps a` in scope.
///
/// `zeroship.app_members` is gone; an app reaches its organization ONLY through
/// `apps.project_id -> projects.organization_id`. A personal organization has
/// exactly one owner, so for the common creator this is the creator.
///
/// Binds the alias `app_owner`, carrying a nullable `user_id`.
#[must_use]
pub fn app_owner_lateral() -> String {
    format!(
        "LEFT JOIN LATERAL ( \
             SELECT owner_member.user_id \
               FROM zeroship.projects owner_project \
               JOIN zeroship.organization_members owner_member \
                    ON owner_member.organization_id = owner_project.organization_id \
              WHERE owner_project.id = a.project_id \
                AND owner_member.role = 'owner' \
              ORDER BY {APP_OWNER_ORDER} \
              LIMIT 1 \
         ) app_owner ON TRUE"
    )
}

/// The same rule as a fleet-wide `(app_id, user_id)` mapping, for the queries
/// that need every app at once rather than one app's owner.
///
/// A parenthesised subquery: join it or select from it under an alias.
#[must_use]
pub fn app_owner_map() -> String {
    format!(
        "( SELECT DISTINCT ON (owner_app.id) owner_app.id AS app_id, owner_member.user_id \
             FROM zeroship.apps owner_app \
             JOIN zeroship.projects owner_project ON owner_project.id = owner_app.project_id \
             JOIN zeroship.organization_members owner_member \
                  ON owner_member.organization_id = owner_project.organization_id \
            WHERE owner_member.role = 'owner' \
            ORDER BY owner_app.id, {APP_OWNER_ORDER} )"
    )
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateOrganizationBody {
    pub name: String,
    /// Optional. Derived from the name when absent.
    #[serde(default)]
    pub slug: Option<String>,
    /// Optional. Seeded from the caller's own address when absent - the billed
    /// party and the notified party separate here for the first time, and
    /// defaulting to the minter is the only defensible seed.
    #[serde(default)]
    pub billing_email: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateOrganizationBody {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub billing_email: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AddMemberBody {
    pub user_id: Uuid,
    pub role: String,
}

#[derive(Debug, Deserialize)]
pub struct ChangeRoleBody {
    pub role: String,
}

#[derive(Debug, Deserialize)]
pub struct TransferOwnershipBody {
    /// The member who becomes owner. They must already hold a seat: transfer
    /// re-roles, it does not admit.
    pub user_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct CreateInviteBody {
    pub email: String,
    pub role: String,
}

#[derive(Debug, Deserialize)]
pub struct RedeemInviteBody {
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateProjectBody {
    pub name: String,
    #[serde(default)]
    pub slug: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateProjectBody {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AddProjectMemberBody {
    pub user_id: Uuid,
    pub role: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrganizationRecord {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub billing_email: String,
    /// Set only on a personal organization. No read path branches on it; it is
    /// reported so a console can label the row and so clearing it (by
    /// transferring ownership) is visible as the personal-to-shared conversion
    /// it is.
    pub personal_owner_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// When the organization was CLOSED, and `None` while it is live.
    ///
    /// A dissolved organization is a historical record: its members, invites
    /// and billing history stay exactly where they are, and every read still
    /// returns it. What it no longer accepts is CHANGE - [`lock_organization`]
    /// refuses, so every mutation in this module refuses, including a second
    /// dissolve and including a departure.
    pub dissolved_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemberRecord {
    pub organization_id: String,
    pub user_id: Uuid,
    pub email: String,
    pub name: String,
    pub role: String,
    pub rank: i32,
    pub billing_rank: i32,
    pub added_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectRecord {
    pub id: String,
    pub organization_id: String,
    pub slug: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectMemberRecord {
    pub project_id: String,
    pub user_id: Uuid,
    pub email: String,
    pub role: String,
    pub added_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InviteRecord {
    pub id: String,
    pub organization_id: String,
    pub email: String,
    pub role: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub consumed_at: Option<DateTime<Utc>>,
}

/// What became of the invitation email.
///
/// It is recorded in `organization_invites.delivery`, which the schema leaves
/// NULL until an attempt resolves - the row is written and COMMITTED before the
/// mail is attempted, so that a redemption can never arrive before the digest it
/// is matched against exists. A send that fails therefore leaves a usable
/// invitation and a row that says the recipient never got it, rather than
/// losing the invitation to a transport error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InviteDelivery {
    /// The mailer accepted it.
    Sent,
    /// The address is in `zeroship.email_suppressions`. Not an error: the
    /// platform declines to mail a known bouncer or complainant, and the
    /// inviter is told so they can deliver the token another way.
    Suppressed,
    /// The transport refused or failed. The invitation stands; the token in
    /// this response is the only copy that exists.
    Failed,
}

impl InviteDelivery {
    /// The `organization_invites_delivery_check` vocabulary. The column is
    /// constrained to exactly these three words, so this is the one spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Suppressed => "suppressed",
            Self::Failed => "failed",
        }
    }

    /// Whether the recipient's mailbox now holds the token.
    #[must_use]
    pub const fn reached_the_recipient(self) -> bool {
        matches!(self, Self::Sent)
    }
}

/// What [`create_invite`] produces: a committed invitation and the one-time
/// token for it, BEFORE anything has tried to deliver it.
///
/// It is a separate type from [`CreatedInvite`] because it is a separate fact.
/// This one says an invitation exists and can be redeemed; the wire type adds
/// what became of the email, which is not known yet and cannot be, since the
/// row has to be committed before the send is attempted.
#[derive(Debug)]
pub struct IssuedInvite {
    pub invite: InviteRecord,
    pub token: String,
}

/// The create-invite response. The token appears HERE and nowhere else, ever:
/// only its digest is stored, so this response is the single moment the secret
/// exists outside the recipient's mailbox. Re-reading the invite returns an
/// [`InviteRecord`] with no token field.
///
/// # Why the token is still returned now that the platform mails it
///
/// Because delivery can fail, and because the inviter may legitimately want to
/// hand the invitation over by another channel. `delivery` says which case this
/// is, so a client can show the token only when the mail did not carry it. The
/// token confers nothing on its holder that the inviter did not already have:
/// redemption additionally requires the redeeming account's VERIFIED address to
/// be the invited address, so possessing it is not a way to seat yourself.
#[derive(Debug, Serialize)]
pub struct CreatedInvite {
    pub invite: InviteRecord,
    pub token: String,
    pub delivery: InviteDelivery,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum OrganizationError {
    OrganizationNotFound,
    ProjectNotFound,
    MemberNotFound,
    InviteNotFound,
    UserNotFound,
    Invalid(String),
    SlugTaken(String),
    AlreadyMember,
    /// The effect statement matched no row because the actor's live rank does
    /// not clear the target's. Carries the comparison that failed so the caller
    /// is told which of the two integers refused them.
    Insufficient(String),
    /// Removing this member would leave the organization with no owner. NOT a
    /// CHECK: the claim is about a set, so it is enforced by the predicate in
    /// the DELETE plus the `FOR UPDATE` lock that makes the count true at
    /// commit. Carries the remedy.
    LastOwner,
    /// The organization was closed. Carries WHEN, because the answer to "why
    /// was this refused" is a date the caller can go and look at.
    ///
    /// Raised by [`lock_organization`], so it precedes every effect statement
    /// in this module rather than being re-derived per mutation.
    Dissolved(DateTime<Utc>),
    /// Dissolve was asked for while the organization still owns projects.
    ///
    /// `projects.organization_id` is `ON DELETE RESTRICT`, so this is not a
    /// policy layered over a permissive schema - it is the same rule stated
    /// where the caller can read it, with the remedy named.
    OrganizationHasProjects(i64),
    /// Delete was asked for while the project still owns apps.
    /// `apps_project_ownership_fkey` is `ON DELETE RESTRICT`; same shape as
    /// [`Self::OrganizationHasProjects`].
    ///
    /// The count is of LIVE apps. A deleted app has left its project, so it is
    /// neither counted here nor held by the constraint.
    ProjectHasApps(i64),
    /// The app named by [`delete_app`] does not exist, or has already been
    /// deleted. ONE variant for both, because a deleted app has left its
    /// project and there is no longer anything that could tell them apart
    /// without re-deriving reach from a row that no longer has any.
    AppNotFound,
    /// Delete was asked for on an app that is still live. Archive is the
    /// reversible step and delete is the terminal one; refusing here is what
    /// keeps the ordering an act rather than an accident.
    AppNotArchived,
    /// Dissolve was asked for while the organization still owes - an unpaid
    /// finalized invoice, or usage in a closed period that was never invoiced.
    ///
    /// It carries the whole typed answer rather than a count, because unlike
    /// the two above there is no single number that describes it: money already
    /// claimed and usage not yet claimed are different debts with different
    /// remedies, and a caller told only "you owe" cannot act.
    OrganizationOwesBilling(OutstandingBilling),
    /// The invite exists but cannot be redeemed: expired, already consumed, the
    /// address does not match, or the inviter's authority has lapsed since
    /// issue. Deliberately ONE variant - distinguishing them for the redeemer
    /// would turn the endpoint into an oracle over other people's invites.
    InviteNotRedeemable,
    Db,
}

impl OrganizationError {
    #[allow(clippy::needless_pass_by_value)]
    pub fn into_response(self) -> web::HttpResponse {
        match self {
            Self::OrganizationNotFound => {
                web::HttpResponse::NotFound().json(&json!({"error": "organization not found"}))
            }
            Self::ProjectNotFound => {
                web::HttpResponse::NotFound().json(&json!({"error": "project not found"}))
            }
            Self::MemberNotFound => {
                web::HttpResponse::NotFound().json(&json!({"error": "member not found"}))
            }
            Self::InviteNotFound => {
                web::HttpResponse::NotFound().json(&json!({"error": "invite not found"}))
            }
            Self::UserNotFound => {
                web::HttpResponse::NotFound().json(&json!({"error": "user not found"}))
            }
            Self::Invalid(detail) => web::HttpResponse::BadRequest()
                .json(&json!({"error": "invalid request", "detail": detail})),
            Self::SlugTaken(slug) => web::HttpResponse::Conflict().json(&json!({
                "error": "slug taken",
                "detail": format!("the slug {slug:?} is already in use"),
                "slug": slug,
            })),
            Self::AlreadyMember => {
                web::HttpResponse::Conflict().json(&json!({"error": "already a member"}))
            }
            Self::Insufficient(detail) => web::HttpResponse::Forbidden()
                .json(&json!({"error": "insufficient authority", "detail": detail})),
            Self::LastOwner => web::HttpResponse::Conflict().json(&json!({
                "error": "last owner",
                "detail": "an organization must keep at least one owner; transfer ownership \
                           first, and then remove this seat",
            })),
            Self::Dissolved(at) => web::HttpResponse::Conflict().json(&json!({
                "error": "organization dissolved",
                "detail": "this organization was closed and accepts no further changes; \
                           its members, invitations and billing history are readable as they \
                           were left",
                "dissolved_at": at,
            })),
            Self::OrganizationHasProjects(remaining) => {
                web::HttpResponse::Conflict().json(&json!({
                    "error": "organization has projects",
                    "detail": "an organization is closed only once it owns no projects; delete \
                               them first with DELETE /api/projects/{project_id}, which itself \
                               needs each project to own no apps",
                    "projects": remaining,
                }))
            }
            Self::ProjectHasApps(remaining) => web::HttpResponse::Conflict().json(&json!({
                "error": "project has apps",
                "detail": "a project is deleted only once it owns no apps; archive each one \
                           with PUT /api/apps/{app_id}/archive and then delete it with \
                           DELETE /api/apps/{app_id}",
                "apps": remaining,
            })),
            Self::AppNotFound => {
                web::HttpResponse::NotFound().json(&json!({"error": "app not found"}))
            }
            Self::AppNotArchived => web::HttpResponse::Conflict().json(&json!({
                "error": "app not archived",
                "detail": "an app is deleted only once it is archived; archive it first with \
                           PUT /api/apps/{app_id}/archive, which is reversible, and then \
                           delete it, which is not",
            })),
            Self::OrganizationOwesBilling(outstanding) => {
                // The remedy is derived from the same rows the refusal reports,
                // so the two can never name different next steps. It is
                // `Some` on every path that builds this variant - the variant
                // is only constructed from an unsettled reading - and the
                // fallback exists so a future settled reading cannot render a
                // refusal with no instruction in it.
                let detail = outstanding.remedy().map_or(
                    "this organization has unsettled billing",
                    BillingRemedy::instruction,
                );
                web::HttpResponse::Conflict().json(&json!({
                    "error": "organization owes",
                    "detail": detail,
                    "owed_cents": outstanding.owed_cents(),
                    "currency": outstanding.currency(),
                    "remedy": outstanding.remedy(),
                    "unpaid_invoices": outstanding.unpaid_invoices,
                    "unbilled_periods": outstanding.unbilled_periods,
                }))
            }
            Self::InviteNotRedeemable => web::HttpResponse::Forbidden().json(&json!({
                "error": "invite not redeemable",
                "detail": "the invitation has expired, has already been used, was issued to a \
                           different address, or the person who sent it no longer holds the \
                           authority to grant that role",
            })),
            Self::Db => {
                web::HttpResponse::InternalServerError().json(&json!({"error": "database error"}))
            }
        }
    }
}

impl From<RegistryError> for OrganizationError {
    fn from(err: RegistryError) -> Self {
        tracing::error!(error = %err, "control: organization store connection failed");
        Self::Db
    }
}

/// The fallback mapper: log the statement that failed and report a database
/// error.
///
/// It deliberately does NOT try to classify a unique violation. Which
/// uniqueness was violated is knowable only at the call site - a slug clash, an
/// outstanding invite and a re-seated member are three different remedies and
/// the constraint NAME is not a contract. Each caller that can produce one maps
/// it itself ([`slug_conflict`], [`invite_conflict`], [`member_conflict`],
/// [`project_member_conflict`]); a caller that reaches this function with one
/// has a case nobody has thought about, and reporting that as a 500 is the
/// honest answer.
fn db_error(err: &compio_postgres::Error, context: &str) -> OrganizationError {
    tracing::error!(error = %err, context, "control: organization statement failed");
    OrganizationError::Db
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// The slug grammar the schema enforces (`^[a-z0-9][a-z0-9-]*$`), applied here
/// so a malformed slug is a 400 naming the rule rather than a 500 carrying a
/// constraint name.
fn validate_slug(slug: &str) -> Result<(), OrganizationError> {
    if slug.is_empty() || slug.chars().count() > MAX_SLUG_CHARS {
        return Err(OrganizationError::Invalid(format!(
            "slug must be 1-{MAX_SLUG_CHARS} characters"
        )));
    }
    let mut chars = slug.chars();
    let first = chars.next().unwrap_or('-');
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(OrganizationError::Invalid(
            "slug must start with a lowercase letter or digit".to_string(),
        ));
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(OrganizationError::Invalid(
            "slug may contain only lowercase letters, digits and hyphens".to_string(),
        ));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), OrganizationError> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_NAME_CHARS {
        return Err(OrganizationError::Invalid(format!(
            "name must be 1-{MAX_NAME_CHARS} characters"
        )));
    }
    Ok(())
}

/// Derive a slug from a display name. Lowercases, replaces every run of
/// non-alphanumerics with one hyphen, and trims hyphens from both ends.
///
/// It can legitimately produce nothing - a name of only punctuation, or of
/// characters with no ASCII form - and the caller must treat that as "the
/// creator has to supply a slug", not as a reason to invent one. Inventing one
/// would put an id-shaped string in a field a human reads.
fn slug_from_name(name: &str) -> Option<String> {
    let mut out = String::new();
    let mut pending_hyphen = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_hyphen && !out.is_empty() {
                out.push('-');
            }
            pending_hyphen = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_hyphen = true;
        }
    }
    let out: String = out.chars().take(MAX_SLUG_CHARS).collect();
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn validate_email(email: &str) -> Result<(), OrganizationError> {
    let trimmed = email.trim();
    if trimmed.len() < 3 || trimmed.len() > 254 || !trimmed.contains('@') || trimmed.contains(' ') {
        return Err(OrganizationError::Invalid(
            "billing_email must be an email address".to_string(),
        ));
    }
    Ok(())
}

/// Invite tokens: 32 CSPRNG bytes, base64url without padding. The value leaves
/// this process once, in the create response; only [`token_digest`] of it is
/// stored.
fn mint_invite_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn token_digest(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

const ORGANIZATION_COLUMNS: &str =
    "o.id, o.slug::text AS slug, o.name, o.billing_email::text AS billing_email, \
     o.personal_owner_id, o.created_at, o.updated_at, o.dissolved_at";

fn row_to_organization(row: &compio_postgres::Row) -> OrganizationRecord {
    OrganizationRecord {
        id: row.get("id"),
        slug: row.get("slug"),
        name: row.get("name"),
        billing_email: row.get("billing_email"),
        personal_owner_id: row.get("personal_owner_id"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        dissolved_at: row.get("dissolved_at"),
    }
}

/// Every organization the caller holds a seat in. This is the `Resource::Any`
/// listing: the self-service band grants `organization:read` platform-wide, so
/// the SCOPE of what comes back has to be the membership join and not the
/// table.
pub async fn list_organizations<C: GenericClient + Sync>(
    pg: &C,
    principal: Uuid,
) -> Result<Vec<OrganizationRecord>, OrganizationError> {
    let sql = format!(
        "SELECT {ORGANIZATION_COLUMNS} \
           FROM zeroship.organizations o \
           JOIN zeroship.organization_members m ON m.organization_id = o.id \
          WHERE m.user_id = $1 \
          ORDER BY o.slug"
    );
    let rows = pg
        .query(&sql, &[&principal])
        .await
        .map_err(|err| db_error(&err, "list organizations"))?;
    Ok(rows.iter().map(row_to_organization).collect())
}

pub async fn get_organization<C: GenericClient + Sync>(
    pg: &C,
    organization_id: &str,
) -> Result<OrganizationRecord, OrganizationError> {
    let sql =
        format!("SELECT {ORGANIZATION_COLUMNS} FROM zeroship.organizations o WHERE o.id = $1");
    let rows = pg
        .query(&sql, &[&organization_id])
        .await
        .map_err(|err| db_error(&err, "get organization"))?;
    rows.first()
        .map(row_to_organization)
        .ok_or(OrganizationError::OrganizationNotFound)
}

pub async fn list_members<C: GenericClient + Sync>(
    pg: &C,
    organization_id: &str,
) -> Result<Vec<MemberRecord>, OrganizationError> {
    let rows = pg
        .query(
            "SELECT m.organization_id, m.user_id, u.email::text AS email, u.name, m.role, \
                    r.rank, r.billing_rank, m.added_at \
               FROM zeroship.organization_members m \
               JOIN zeroship.users u ON u.id = m.user_id \
               JOIN zeroship.organization_roles r ON r.role = m.role \
              WHERE m.organization_id = $1 \
              ORDER BY r.rank DESC, u.email",
            &[&organization_id],
        )
        .await
        .map_err(|err| db_error(&err, "list members"))?;
    Ok(rows
        .iter()
        .map(|row| MemberRecord {
            organization_id: row.get("organization_id"),
            user_id: row.get("user_id"),
            email: row.get("email"),
            name: row.get("name"),
            role: row.get("role"),
            rank: row.get("rank"),
            billing_rank: row.get("billing_rank"),
            added_at: row.get("added_at"),
        })
        .collect())
}

/// The projects of one organization that `principal` can actually reach.
///
/// The filter is the narrowing rule, not a convenience: `organization_read`
/// grants `organization:read` at rank 10, and a viewer holds no authority on a
/// project they have no row for. Returning every project of the organization
/// would hand them the NAMES of projects they cannot open, which is why the
/// Cedar band deliberately omits `project:read` at organization scope and
/// leaves the filtering here.
pub async fn list_projects<C: GenericClient + Sync>(
    pg: &C,
    organization_id: &str,
    principal: Uuid,
) -> Result<Vec<ProjectRecord>, OrganizationError> {
    let effective = effective_project_rank_sql("organization_role.rank", "project_role.rank", "$3");
    let sql = format!(
        "SELECT p.id, p.organization_id, p.slug::text AS slug, p.name, p.created_at \
           FROM zeroship.projects p \
           LEFT JOIN zeroship.organization_members m \
                  ON m.organization_id = p.organization_id AND m.user_id = $2 \
           LEFT JOIN zeroship.organization_roles organization_role \
                  ON organization_role.role = m.role \
           LEFT JOIN zeroship.project_members pm ON pm.project_id = p.id AND pm.user_id = $2 \
           LEFT JOIN zeroship.organization_roles project_role ON project_role.role = pm.role \
          WHERE p.organization_id = $1 \
            AND {effective} >= {viewer} \
          ORDER BY p.slug",
        viewer = ladder_rank("$4")
    );
    let admin_rank = admin_rank(pg).await?;
    let rows = pg
        .query(
            &sql,
            &[&organization_id, &principal, &admin_rank, &ROLE_VIEWER],
        )
        .await
        .map_err(|err| db_error(&err, "list projects"))?;
    Ok(rows.iter().map(row_to_project).collect())
}

fn row_to_project(row: &compio_postgres::Row) -> ProjectRecord {
    ProjectRecord {
        id: row.get("id"),
        organization_id: row.get("organization_id"),
        slug: row.get("slug"),
        name: row.get("name"),
        created_at: row.get("created_at"),
    }
}

/// The organization that owns `app_id`, reached through its project.
///
/// `None` means the app does not exist. There is no second answer: an app has
/// exactly one project and a project has exactly one organization, which is why
/// there is no `apps.organization_id` column for this to disagree with.
///
/// # Errors
///
/// Returns [`OrganizationError::Db`] when the query fails.
pub async fn organization_of_app<C: GenericClient + Sync>(
    pg: &C,
    app_id: Uuid,
) -> Result<Option<String>, OrganizationError> {
    let rows = pg
        .query(
            // `apps.organization_id` rather than the hop through `projects`: the
            // copy is consumed by the composite key into
            // `projects(id, organization_id)`, so the two cannot disagree and the
            // join proved nothing the constraint does not already enforce.
            "SELECT a.organization_id FROM zeroship.apps a WHERE a.id = $1",
            &[&app_id],
        )
        .await
        .map_err(|err| db_error(&err, "resolve organization of app"))?;
    Ok(rows.first().map(|row| row.get("organization_id")))
}

pub async fn get_project<C: GenericClient + Sync>(
    pg: &C,
    project_id: &str,
) -> Result<ProjectRecord, OrganizationError> {
    let rows = pg
        .query(
            "SELECT p.id, p.organization_id, p.slug::text AS slug, p.name, p.created_at \
               FROM zeroship.projects p WHERE p.id = $1",
            &[&project_id],
        )
        .await
        .map_err(|err| db_error(&err, "get project"))?;
    rows.first()
        .map(row_to_project)
        .ok_or(OrganizationError::ProjectNotFound)
}

pub async fn list_project_members<C: GenericClient + Sync>(
    pg: &C,
    project_id: &str,
) -> Result<Vec<ProjectMemberRecord>, OrganizationError> {
    let rows = pg
        .query(
            "SELECT pm.project_id, pm.user_id, u.email::text AS email, pm.role, pm.added_at \
               FROM zeroship.project_members pm \
               JOIN zeroship.users u ON u.id = pm.user_id \
              WHERE pm.project_id = $1 \
              ORDER BY u.email",
            &[&project_id],
        )
        .await
        .map_err(|err| db_error(&err, "list project members"))?;
    Ok(rows
        .iter()
        .map(|row| ProjectMemberRecord {
            project_id: row.get("project_id"),
            user_id: row.get("user_id"),
            email: row.get("email"),
            role: row.get("role"),
            added_at: row.get("added_at"),
        })
        .collect())
}

/// Pending and recently consumed invites. The token digest is never selected -
/// there is nothing a caller could do with it, and a column that is never read
/// cannot be leaked by a serializer that grows a field.
pub async fn list_invites<C: GenericClient + Sync>(
    pg: &C,
    organization_id: &str,
) -> Result<Vec<InviteRecord>, OrganizationError> {
    let rows = pg
        .query(
            "SELECT i.id, i.organization_id, i.email::text AS email, i.role, \
                    i.issued_at, i.expires_at, i.consumed_at \
               FROM zeroship.organization_invites i \
              WHERE i.organization_id = $1 \
              ORDER BY i.issued_at DESC",
            &[&organization_id],
        )
        .await
        .map_err(|err| db_error(&err, "list invites"))?;
    Ok(rows.iter().map(row_to_invite).collect())
}

fn row_to_invite(row: &compio_postgres::Row) -> InviteRecord {
    InviteRecord {
        id: row.get("id"),
        organization_id: row.get("organization_id"),
        email: row.get("email"),
        role: row.get("role"),
        issued_at: row.get("issued_at"),
        expires_at: row.get("expires_at"),
        consumed_at: row.get("consumed_at"),
    }
}

/// The `admin` rank, read from the ladder. The NAME is the stable thing - the
/// same fence `zeroship_authz::authority` reads - so moving `admin` in a
/// migration moves every project-wide threshold here with it.
async fn admin_rank<C: GenericClient + Sync>(conn: &C) -> Result<Option<i32>, OrganizationError> {
    let rows = conn
        .query(
            "SELECT rank FROM zeroship.organization_roles WHERE role = $1",
            &[&ROLE_ADMIN],
        )
        .await
        .map_err(|err| db_error(&err, "read admin rank"))?;
    Ok(rows.first().map(|row| row.get("rank")))
}

// ---------------------------------------------------------------------------
// Mutations
// ---------------------------------------------------------------------------

/// Lock one organization row for the rest of the transaction, and refuse a
/// dissolved one.
///
/// Returns `OrganizationNotFound` when the row does not exist, so a membership
/// mutation against a deleted organization is a 404 rather than a statement
/// that silently affects nothing.
///
/// # The dissolved fence lives HERE and in exactly one place
///
/// Every mutation in this module opens with this call, so making it the fence
/// closes the whole surface at once - membership, invites, projects, renames,
/// redemption and a second dissolve. The alternative, a `dissolved_at IS NULL`
/// clause in each effect statement, is the same rule written a dozen times, and
/// the thirteenth mutation is the one that forgets it.
///
/// It is a REFUSAL and not a filter, and the difference matters for
/// [`dissolve_organization`]: re-dissolving reports the close it already had,
/// with its date, rather than pretending to do it again.
///
/// Cedar is not asked about this. A dissolved organization's members keep their
/// ranks, so `zeroship_authz::authority::resolve` still answers 40 for its
/// owner and the band still permits - which is right, because what ended is the
/// organization, not the seat. The refusal belongs where the effect is.
async fn lock_organization<C: GenericClient + Sync>(
    tx: &C,
    organization_id: &str,
) -> Result<(), OrganizationError> {
    let rows = tx
        .query(
            "SELECT dissolved_at FROM zeroship.organizations WHERE id = $1 FOR UPDATE",
            &[&organization_id],
        )
        .await
        .map_err(|err| db_error(&err, "lock organization"))?;
    let Some(row) = rows.first() else {
        return Err(OrganizationError::OrganizationNotFound);
    };
    match row.get::<_, Option<DateTime<Utc>>>("dissolved_at") {
        Some(at) => Err(OrganizationError::Dissolved(at)),
        None => Ok(()),
    }
}

/// Mint an organization and seat the caller as its first owner.
///
/// This and [`ensure_personal_project`] are the only membership writes with no
/// rank predicate, for the reason the module header gives: the organization did
/// not exist a statement ago, so there is no seat to outrank and nobody else
/// can reach it.
pub async fn create_organization(
    registry: &Registry,
    principal: Uuid,
    body: &CreateOrganizationBody,
    source_ip: Option<&str>,
) -> Result<OrganizationRecord, OrganizationError> {
    validate_name(&body.name)?;
    let slug = match body.slug.as_deref().map(str::trim) {
        Some(slug) if !slug.is_empty() => slug.to_string(),
        _ => slug_from_name(&body.name).ok_or_else(|| {
            OrganizationError::Invalid(
                "could not derive a slug from that name; supply one explicitly".to_string(),
            )
        })?,
    };
    validate_slug(&slug)?;
    if let Some(email) = body.billing_email.as_deref() {
        validate_email(email)?;
    }

    let organization_id = OrganizationId::mint();
    let project_id = ProjectId::mint();

    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin create organization"))?;

    let billing_email = body.billing_email.as_deref().map(str::trim);
    // `AS o` so the RETURNING list is the SAME text every other read uses. A
    // second spelling of the column list is a second thing to keep in step with
    // `row_to_organization`.
    let rows = tx
        .query(
            &format!(
                "INSERT INTO zeroship.organizations AS o \
                     (id, slug, name, billing_email, created_by) \
                 SELECT $1, $2, $3, COALESCE($4, u.email), $5 \
                   FROM zeroship.users u WHERE u.id = $5 \
                 RETURNING {ORGANIZATION_COLUMNS}"
            ),
            &[
                &organization_id.as_str(),
                &slug,
                &body.name.trim(),
                &billing_email,
                &principal,
            ],
        )
        .await
        .map_err(|err| slug_conflict(&err, &slug, "create organization"))?;
    let Some(row) = rows.first() else {
        // The INSERT ... SELECT matched no user row.
        return Err(OrganizationError::UserNotFound);
    };
    let record = row_to_organization(row);

    tx.execute(
        "INSERT INTO zeroship.organization_members \
             (organization_id, user_id, role, added_by, changed_by) \
         VALUES ($1, $2, $3, $2, $2)",
        &[&organization_id.as_str(), &principal, &ROLE_OWNER],
    )
    .await
    .map_err(|err| db_error(&err, "seat first owner"))?;

    // A project from the first moment. An organization with no project cannot
    // hold an app (`apps.project_id` is NOT NULL), so minting one here is what
    // keeps "create an organization, then deploy" a two-step flow instead of a
    // three-step one.
    tx.execute(
        "INSERT INTO zeroship.projects (id, organization_id, slug, name, created_by) \
         VALUES ($1, $2, 'default', 'Default', $3)",
        &[&project_id.as_str(), &organization_id.as_str(), &principal],
    )
    .await
    .map_err(|err| db_error(&err, "create default project"))?;

    audit_authority_change(
        &tx,
        principal,
        AuditAction::OrganizationCreated,
        organization_id.as_str(),
        source_ip,
        &json!({
            "organization_id": organization_id.as_str(),
            "slug": slug,
            "project_id": project_id.as_str(),
            "seated": { "user_id": principal, "role": ROLE_OWNER },
        }),
    )
    .await;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit create organization"))?;
    Ok(record)
}

/// A unique violation naming the slug index is a taken slug; anything else keeps
/// its own classification. Split out because two statements need it and the
/// generic mapper cannot know which column the caller supplied.
fn slug_conflict(err: &compio_postgres::Error, slug: &str, context: &str) -> OrganizationError {
    use compio_postgres::error::SqlState;
    if err.code() == Some(&SqlState::UNIQUE_VIOLATION) {
        tracing::warn!(error = %err, context, slug, "control: slug already taken");
        return OrganizationError::SlugTaken(slug.to_string());
    }
    db_error(err, context)
}

/// Rename an organization, re-slug it, or move the billed address.
///
/// Owner-only, and the rank predicate says so inside the UPDATE rather than
/// only in the Cedar band above it.
pub async fn update_organization(
    registry: &Registry,
    principal: Uuid,
    organization_id: &str,
    body: &UpdateOrganizationBody,
    source_ip: Option<&str>,
) -> Result<OrganizationRecord, OrganizationError> {
    if let Some(name) = body.name.as_deref() {
        validate_name(name)?;
    }
    if let Some(slug) = body.slug.as_deref() {
        validate_slug(slug)?;
    }
    if let Some(email) = body.billing_email.as_deref() {
        validate_email(email)?;
    }
    if body.name.is_none() && body.slug.is_none() && body.billing_email.is_none() {
        return Err(OrganizationError::Invalid(
            "supply at least one of name, slug or billing_email".to_string(),
        ));
    }

    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin update organization"))?;
    lock_organization(&tx, organization_id).await?;

    let sql = format!(
        "UPDATE zeroship.organizations o \
            SET name = COALESCE($2, o.name), \
                slug = COALESCE($3, o.slug), \
                billing_email = COALESCE($4, o.billing_email), \
                updated_at = NOW() \
          WHERE o.id = $1 \
            AND EXISTS (SELECT 1 FROM {seat} \
                         WHERE actor_role.rank >= {owner}) \
        RETURNING {ORGANIZATION_COLUMNS}",
        seat = actor_seat("o.id", "$5"),
        owner = ladder_rank("$6"),
    );
    let name = body.name.as_deref().map(str::trim);
    let slug = body.slug.as_deref().map(str::trim);
    let email = body.billing_email.as_deref().map(str::trim);
    let rows = tx
        .query(
            &sql,
            &[
                &organization_id,
                &name,
                &slug,
                &email,
                &principal,
                &ROLE_OWNER,
            ],
        )
        .await
        .map_err(|err| slug_conflict(&err, slug.unwrap_or(""), "update organization"))?;

    let Some(row) = rows.first() else {
        // The row is locked and exists (`lock_organization` proved it), so the
        // only clause that can have matched nothing is the rank predicate.
        return Err(OrganizationError::Insufficient(
            "changing an organization's name, slug or billing address is reserved to its owners"
                .to_string(),
        ));
    };
    let record = row_to_organization(row);

    audit_authority_change(
        &tx,
        principal,
        AuditAction::OrganizationUpdated,
        organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "name": name,
            "slug": slug,
            "billing_email_changed": email.is_some(),
        }),
    )
    .await;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit update organization"))?;
    Ok(record)
}

/// Seat an existing platform user in an organization.
///
/// The effect statement carries the whole comparison: the actor's live seat is
/// a join inside the INSERT, so a member demoted or removed between the Cedar
/// call and this statement inserts nothing.
///
/// It also carries the `admin` FLOOR, for the same reason
/// [`add_project_member`] does. The strict inequality alone lets a developer
/// seat a viewer, because rank 20 does outrank rank 10 - so without the floor,
/// the band Cedar puts on `organization:members:write` would be the ONLY thing
/// refusing that, and this module's promise is that Cedar is never the only
/// fence.
pub async fn add_member(
    registry: &Registry,
    principal: Uuid,
    organization_id: &str,
    body: &AddMemberBody,
    source_ip: Option<&str>,
) -> Result<MemberRecord, OrganizationError> {
    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin add member"))?;
    lock_organization(&tx, organization_id).await?;

    let sql = format!(
        "INSERT INTO zeroship.organization_members \
             (organization_id, user_id, role, added_by, changed_by) \
         SELECT $1, $2, target_role.role, $4, $4 \
           FROM zeroship.organization_roles target_role, {seat} \
          WHERE target_role.role = $3 \
            AND actor_role.rank >= {admin} \
            AND actor_role.rank > target_role.rank \
            AND actor_role.billing_rank >= target_role.billing_rank \
         RETURNING role, added_at",
        seat = actor_seat("$1", "$4"),
        admin = ladder_rank("$5"),
    );
    let rows = tx
        .query(
            &sql,
            &[
                &organization_id,
                &body.user_id,
                &body.role,
                &principal,
                &ROLE_ADMIN,
            ],
        )
        .await
        .map_err(|err| member_conflict(&err, "add member"))?;

    let Some(row) = rows.first() else {
        return Err(classify_seat_refusal(
            &tx,
            organization_id,
            principal,
            &body.role,
            Some(ROLE_ADMIN),
        )
        .await);
    };
    let added_at: DateTime<Utc> = row.get("added_at");
    let role: String = row.get("role");

    audit_authority_change(
        &tx,
        principal,
        AuditAction::OrganizationMemberAdded,
        organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "user_id": body.user_id,
            "role": role,
        }),
    )
    .await;

    // Read back INSIDE the transaction. The row is visible to this snapshot and
    // to no other, which is exactly right: a read-back after commit would be a
    // second connection reporting a THIRD state of the world.
    let record = read_member(&tx, organization_id, body.user_id, added_at).await?;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit add member"))?;
    Ok(record)
}

/// Change a member's role.
///
/// The actor must outrank BOTH the role being granted and the role being taken
/// away. Only checking the new role would let an admin demote an owner; only
/// checking the old one would let them promote a viewer to owner.
pub async fn change_member_role(
    registry: &Registry,
    principal: Uuid,
    organization_id: &str,
    user_id: Uuid,
    body: &ChangeRoleBody,
    source_ip: Option<&str>,
) -> Result<MemberRecord, OrganizationError> {
    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin change role"))?;
    lock_organization(&tx, organization_id).await?;

    // The last-owner clause is here as well as in the DELETE: demoting the only
    // owner leaves an ownerless organization exactly as removing them does.
    //
    // The alias for the role the member currently HOLDS is `held_role`, not the
    // obvious `current_role`: `CURRENT_ROLE` is a reserved SQL keyword and
    // PostgreSQL refuses it as a bare alias. The failure is a syntax error at
    // execute time, so no test that never reached this statement would have
    // seen it.
    let sql = format!(
        "UPDATE zeroship.organization_members m \
            SET role = target_role.role, changed_at = NOW(), changed_by = $4 \
           FROM zeroship.organization_roles target_role, \
                zeroship.organization_roles held_role, \
                {seat} \
          WHERE m.organization_id = $1 AND m.user_id = $2 \
            AND target_role.role = $3 \
            AND held_role.role = m.role \
            AND actor_role.rank > target_role.rank \
            AND actor_role.billing_rank >= target_role.billing_rank \
            AND actor_role.rank > held_role.rank \
            AND actor_role.billing_rank >= held_role.billing_rank \
            AND (held_role.role <> $5 OR target_role.role = $5 \
                 OR (SELECT COUNT(*) FROM zeroship.organization_members owners \
                      WHERE owners.organization_id = $1 AND owners.role = $5) > 1) \
         RETURNING m.role, m.added_at, held_role.role AS previous_role",
        seat = actor_seat("$1", "$4"),
    );
    let rows = tx
        .query(
            &sql,
            &[
                &organization_id,
                &user_id,
                &body.role,
                &principal,
                &ROLE_OWNER,
            ],
        )
        .await
        .map_err(|err| db_error(&err, "change member role"))?;

    let Some(row) = rows.first() else {
        return Err(classify_member_refusal(
            &tx,
            organization_id,
            principal,
            user_id,
            Some(&body.role),
        )
        .await);
    };
    let added_at: DateTime<Utc> = row.get("added_at");
    let previous_role: String = row.get("previous_role");
    let role: String = row.get("role");

    audit_authority_change(
        &tx,
        principal,
        AuditAction::OrganizationMemberRoleChanged,
        organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "user_id": user_id,
            "from": previous_role,
            "to": role,
        }),
    )
    .await;

    let record = read_member(&tx, organization_id, user_id, added_at).await?;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit change role"))?;
    Ok(record)
}

/// Remove a member.
///
/// Two predicates, both inside the DELETE: the rank comparison, and the
/// last-owner count. The count is only sound because the caller holds the
/// organization row lock - see the module header.
pub async fn remove_member(
    registry: &Registry,
    principal: Uuid,
    organization_id: &str,
    user_id: Uuid,
    source_ip: Option<&str>,
) -> Result<(), OrganizationError> {
    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin remove member"))?;
    lock_organization(&tx, organization_id).await?;

    let sql = format!(
        "DELETE FROM zeroship.organization_members m \
          USING zeroship.organization_roles target_role, {seat} \
          WHERE m.organization_id = $1 AND m.user_id = $2 \
            AND target_role.role = m.role \
            AND actor_role.rank > target_role.rank \
            AND actor_role.billing_rank >= target_role.billing_rank \
            AND (m.role <> $4 \
                 OR (SELECT COUNT(*) FROM zeroship.organization_members owners \
                      WHERE owners.organization_id = $1 AND owners.role = $4) > 1) \
         RETURNING m.role",
        seat = actor_seat("$1", "$3"),
    );
    let rows = tx
        .query(&sql, &[&organization_id, &user_id, &principal, &ROLE_OWNER])
        .await
        .map_err(|err| db_error(&err, "remove member"))?;

    let Some(row) = rows.first() else {
        return Err(classify_member_refusal(&tx, organization_id, principal, user_id, None).await);
    };
    let role: String = row.get("role");

    audit_authority_change(
        &tx,
        principal,
        AuditAction::OrganizationMemberRemoved,
        organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "user_id": user_id,
            "role": role,
        }),
    )
    .await;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit remove member"))?;
    Ok(())
}

/// Give up your OWN seat.
///
/// # The one carve-out in the rank model, and exactly how narrow it is
///
/// Every other membership write compares `actor.rank > target.rank`. An actor
/// never outranks their own rank, so that inequality - which is what stops an
/// admin demoting a peer - also made departure impossible for everyone. The
/// carve-out is this function and nothing else: it takes NO target, because the
/// route above it carries no user id in its path, its body or its query. The
/// statement matches `user_id = $2` where `$2` is the authenticated principal,
/// so "someone else's seat" is not a request this code can be asked to make.
///
/// [`remove_member`] is untouched, still strict, and still the only way to
/// remove anyone else.
///
/// # What still refuses
///
/// The last-owner rule, unchanged and in the same shape: the predicate rides in
/// the DELETE and is sound because the caller holds the organization row lock.
/// A sole owner is refused and told to transfer ownership first - an
/// organization that keeps no owner is one no route can repair, and letting the
/// last one walk out would create exactly that.
pub async fn leave_organization(
    registry: &Registry,
    principal: Uuid,
    organization_id: &str,
    source_ip: Option<&str>,
) -> Result<(), OrganizationError> {
    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin leave organization"))?;
    lock_organization(&tx, organization_id).await?;

    let rows = tx
        .query(
            "DELETE FROM zeroship.organization_members m \
              WHERE m.organization_id = $1 AND m.user_id = $2 \
                AND (m.role <> $3 \
                     OR (SELECT COUNT(*) FROM zeroship.organization_members owners \
                          WHERE owners.organization_id = $1 AND owners.role = $3) > 1) \
             RETURNING m.role",
            &[&organization_id, &principal, &ROLE_OWNER],
        )
        .await
        .map_err(|err| db_error(&err, "leave organization"))?;

    let Some(row) = rows.first() else {
        return Err(classify_departure_refusal(&tx, organization_id, principal).await);
    };
    let role: String = row.get("role");

    audit_authority_change(
        &tx,
        principal,
        AuditAction::OrganizationMemberLeft,
        organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "user_id": principal,
            "role": role,
        }),
    )
    .await;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit leave organization"))?;
    Ok(())
}

/// Close an organization.
///
/// # Soft, and why that is the design rather than a limitation
///
/// `projects.organization_id` is `ON DELETE RESTRICT`, so a hard delete cannot
/// even run while a project remains. That is not the deciding argument, though.
/// The organization is the BILLING SUBJECT: invoices, disputes and status
/// transitions all name it, and a row that vanished would take the counterparty
/// out of a money record that has to outlive the relationship. So the close is a
/// timestamp, everything it owned stays readable, and what ends is the ability
/// to change any of it ([`lock_organization`] is the fence).
///
/// # It refuses while projects remain, and names the remedy
///
/// The refusal is not deferred to the foreign key. `NOT EXISTS (... projects
/// ...)` rides in the UPDATE, under the organization row lock, so a project
/// created concurrently either serializes behind this statement or blocks it -
/// and the caller is told how many remain and which route removes them, rather
/// than receiving a constraint name.
///
/// # A close RELEASES the names the organization was holding
///
/// The slug and the personal-organization slot are both unique among LIVE
/// organizations only - the schema carries `dissolved_at IS NULL` in both
/// indexes - so this statement frees them without touching either column. That
/// matters most for a PERSONAL organization: it is where `zeroship deploy`
/// lands on a fresh account, resolved through `personal_owner_id`, and its slug
/// is derived from the owner's uuid so it can never be re-derived differently.
/// Held globally, closing one would answer the creator's next deploy with a
/// refusal no route could clear.
///
/// The pointer is KEPT rather than cleared. Clearing it is the personal-to-
/// shared conversion [`transfer_ownership`] performs, and a closed organization
/// was not converted to anything - it ended. [`personal_organization_of`]
/// carries the filter instead, which is one place rather than one per writer.
///
/// # It refuses while the organization still owes
///
/// The second refusal, and the one the requirement is about: an unpaid
/// finalized invoice or usage in a closed period that was never invoiced. It is
/// asked FIRST, ahead of the projects rider, because money can appear at any
/// moment while projects only appear when somebody makes one - so an
/// organization that is both empty and indebted must be told about the debt
/// rather than about a rung it has already cleared.
///
/// Unlike the projects test this is a CHECK inside the transaction rather than
/// a rider on the UPDATE, and the difference is deliberate. The organization
/// row lock does not serialize against a concurrent finalize, which locks the
/// organization by advisory key instead, so a rider would not close the race
/// either. What makes the check sufficient is that a dissolve DESTROYS NOTHING:
/// it writes a timestamp, every invoice and every usage row stays exactly where
/// it was, and the erasure of the last human who names them re-asks this same
/// predicate at its own moment ([`crate::erasure::preflight`], which covers
/// dissolved organizations for precisely this reason). The destructive act has
/// the fence; this one has the door.
pub async fn dissolve_organization(
    registry: &Registry,
    principal: Uuid,
    organization_id: &str,
    invoicing: LocalInvoicing,
    source_ip: Option<&str>,
) -> Result<OrganizationRecord, OrganizationError> {
    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin dissolve organization"))?;
    // A second dissolve is refused HERE, with the date of the first, rather
    // than matching no rows and needing a classifier to guess why.
    lock_organization(&tx, organization_id).await?;

    // THE predicate, shared with the erasure seam. A read that fails is a
    // refusal, never a pass: closing an organization on the strength of a debt
    // check that did not run is the outcome this exists to prevent.
    let outstanding = billing_read::outstanding_billing(&tx, organization_id, invoicing).await?;
    if !outstanding.is_settled() {
        return Err(OrganizationError::OrganizationOwesBilling(outstanding));
    }

    let sql = format!(
        "UPDATE zeroship.organizations o \
            SET dissolved_at = NOW(), updated_at = NOW() \
          WHERE o.id = $1 \
            AND o.dissolved_at IS NULL \
            AND EXISTS (SELECT 1 FROM {seat} WHERE actor_role.rank >= {owner}) \
            AND NOT EXISTS (SELECT 1 FROM zeroship.projects p \
                             WHERE p.organization_id = o.id) \
        RETURNING {ORGANIZATION_COLUMNS}",
        seat = actor_seat("o.id", "$2"),
        owner = ladder_rank("$3"),
    );
    let rows = tx
        .query(&sql, &[&organization_id, &principal, &ROLE_OWNER])
        .await
        .map_err(|err| db_error(&err, "dissolve organization"))?;

    let Some(row) = rows.first() else {
        return Err(classify_dissolve_refusal(&tx, organization_id, principal).await);
    };
    let record = row_to_organization(row);

    audit_authority_change(
        &tx,
        principal,
        AuditAction::OrganizationDissolved,
        organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "slug": record.slug,
            "dissolved_at": record.dissolved_at,
        }),
    )
    .await;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit dissolve organization"))?;
    Ok(record)
}

/// Hand ownership to another member and step down to admin.
///
/// One transaction, so the organization is never ownerless and never has an
/// owner who did not consent to holding it. On a PERSONAL organization this
/// also clears `personal_owner_id`, which IS the personal-to-shared conversion:
/// the pointer names the single user the organization was minted for, and once
/// somebody else owns it that is no longer true.
pub async fn transfer_ownership(
    registry: &Registry,
    principal: Uuid,
    organization_id: &str,
    body: &TransferOwnershipBody,
    source_ip: Option<&str>,
) -> Result<(), OrganizationError> {
    if body.user_id == principal {
        return Err(OrganizationError::Invalid(
            "ownership transfer needs a different member as its target".to_string(),
        ));
    }

    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin transfer"))?;
    lock_organization(&tx, organization_id).await?;

    // Promote. The predicate is that the ACTOR is an owner and the TARGET
    // already holds a seat - transfer re-roles, it does not admit, so a
    // transfer cannot be used to bypass the invite path.
    let promote = format!(
        "UPDATE zeroship.organization_members m \
            SET role = $4, changed_at = NOW(), changed_by = $3 \
          WHERE m.organization_id = $1 AND m.user_id = $2 \
            AND EXISTS (SELECT 1 FROM {seat} WHERE actor_role.role = $4) \
         RETURNING m.role",
        seat = actor_seat("$1", "$3"),
    );
    let promoted = tx
        .query(
            &promote,
            &[&organization_id, &body.user_id, &principal, &ROLE_OWNER],
        )
        .await
        .map_err(|err| db_error(&err, "promote new owner"))?;
    if promoted.is_empty() {
        return Err(classify_transfer_refusal(&tx, organization_id, principal, body.user_id).await);
    }

    // Step down. The organization now has at least two owners, so this cannot
    // be the last-owner removal - which is why the demotion follows the
    // promotion and not the other way round.
    tx.execute(
        "UPDATE zeroship.organization_members \
            SET role = $3, changed_at = NOW(), changed_by = $2 \
          WHERE organization_id = $1 AND user_id = $2",
        &[&organization_id, &principal, &ROLE_ADMIN],
    )
    .await
    .map_err(|err| db_error(&err, "step down"))?;

    let converted = tx
        .execute(
            "UPDATE zeroship.organizations \
                SET personal_owner_id = NULL, updated_at = NOW() \
              WHERE id = $1 AND personal_owner_id = $2",
            &[&organization_id, &principal],
        )
        .await
        .map_err(|err| db_error(&err, "clear personal owner"))?;

    audit_authority_change(
        &tx,
        principal,
        AuditAction::OrganizationOwnershipTransferred,
        organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "to": body.user_id,
            "from": principal,
            "became_shared": converted > 0,
        }),
    )
    .await;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit transfer"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Invites
// ---------------------------------------------------------------------------

/// Issue an invite.
///
/// The row freezes the role triple AND the inviter's rank pair, pinned to the
/// ladder by a composite foreign key. That is what lets the schema's
/// `organization_invites_no_escalation` CHECK be a single-row test. The SELECT
/// below carries the same comparison against the actor's LIVE seat, so the
/// CHECK never has to be the thing that catches an escalation - it is the
/// backstop, not the fence.
///
/// It carries the `admin` floor too, for the reason spelled out on
/// [`add_member`]: inviting is seating with a delay, so the two statements must
/// refuse the same actors.
pub async fn create_invite(
    registry: &Registry,
    principal: Uuid,
    organization_id: &str,
    body: &CreateInviteBody,
    source_ip: Option<&str>,
) -> Result<IssuedInvite, OrganizationError> {
    validate_email(&body.email)?;
    let invite_id = InviteId::mint();
    let token = mint_invite_token();
    let digest = token_digest(&token);
    let ttl = format!("{INVITE_TTL_DAYS} days");

    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin create invite"))?;
    lock_organization(&tx, organization_id).await?;

    // `$7::text::interval`, not `$7::interval`, and the difference is not
    // cosmetic: a bare `::interval` makes PostgreSQL infer the PARAMETER's own
    // type as `interval`, so the driver is asked to serialize a Rust `String`
    // against an interval OID and refuses before the statement runs. Binding as
    // text and letting PostgreSQL cast is the same trap `audit::log_with_detail`
    // documents for `inet`.
    let sql = format!(
        "INSERT INTO zeroship.organization_invites \
             (id, token_hash, organization_id, email, role, role_rank, role_billing_rank, \
              invited_by, invited_by_rank, invited_by_billing_rank, purpose, expires_at) \
         SELECT $1, $2, $3, $4, target_role.role, target_role.rank, target_role.billing_rank, \
                $6, actor_role.rank, actor_role.billing_rank, 'organization_invite', \
                NOW() + $7::text::interval \
           FROM zeroship.organization_roles target_role, {seat} \
          WHERE target_role.role = $5 \
            AND actor_role.rank >= {admin} \
            AND actor_role.rank > target_role.rank \
            AND actor_role.billing_rank >= target_role.billing_rank \
         RETURNING id, organization_id, email::text AS email, role, issued_at, expires_at, \
                   consumed_at",
        seat = actor_seat("$3", "$6"),
        admin = ladder_rank("$8"),
    );
    let rows = tx
        .query(
            &sql,
            &[
                &invite_id.as_str(),
                &digest,
                &organization_id,
                &body.email.trim(),
                &body.role,
                &principal,
                &ttl,
                &ROLE_ADMIN,
            ],
        )
        .await
        .map_err(|err| invite_conflict(&err, "create invite"))?;

    let Some(row) = rows.first() else {
        return Err(classify_seat_refusal(
            &tx,
            organization_id,
            principal,
            &body.role,
            Some(ROLE_ADMIN),
        )
        .await);
    };
    let invite = row_to_invite(row);

    audit_authority_change(
        &tx,
        principal,
        AuditAction::OrganizationInviteCreated,
        organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "invite_id": invite.id,
            "email": invite.email,
            "role": invite.role,
        }),
    )
    .await;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit create invite"))?;
    Ok(IssuedInvite { invite, token })
}

/// Issue an invitation and then try to mail it.
///
/// The two steps are composed HERE rather than in the handler so that the
/// ordering is a property of a function instead of a convention every caller
/// has to remember: [`create_invite`] commits, and only then does
/// [`deliver_invite`] attempt a send whose outcome it records. A handler that
/// got that backwards would be a token in flight before its digest was durable.
///
/// A delivery failure is NOT an error. The invitation exists, the token is in
/// the response, and `delivery` says the recipient did not get it - which is
/// the state of the world, and is actionable in a way a 500 is not.
pub async fn create_and_deliver_invite(
    registry: &Registry,
    pg: &compio_postgres::Client,
    mailer: &dyn zeroship_mailer::Mailer,
    principal: Uuid,
    organization_id: &str,
    body: &CreateInviteBody,
    source_ip: Option<&str>,
) -> Result<CreatedInvite, OrganizationError> {
    let issued = create_invite(registry, principal, organization_id, body, source_ip).await?;
    let delivery = deliver_invite(pg, mailer, &issued.invite, &issued.token).await;
    Ok(CreatedInvite {
        invite: issued.invite,
        token: issued.token,
        delivery,
    })
}

/// Who the invitation appears to come from.
///
/// A const, like [`crate::notify`]'s billing sender, because the control plane
/// has no operator-facing mail-from setting to read - the auth service is the
/// one that carries `auth.mail_from_email`. If a deployment ever needs to move
/// this, it moves both.
const INVITE_FROM_EMAIL: &str = "invites@zeroship.ai";
const INVITE_FROM_NAME: &str = "zeroship";

/// Mail the invitation, and record what became of the attempt.
///
/// # The ordering is the whole design
///
/// [`create_invite`] has already COMMITTED the row by the time this runs. That
/// is deliberate and it is why `organization_invites.delivery` is nullable:
/// were the mail sent inside the creating transaction, a recipient quick enough
/// to redeem could present a token whose digest had not been committed yet, and
/// a transport that hung would hold the row lock while it did. So the invitation
/// exists first, the attempt happens second, and the OUTCOME is recorded third.
///
/// A failed send therefore leaves a usable invitation plus a row that says the
/// recipient never received it - which is the difference between a delivery
/// problem and a lost invitation.
///
/// # Suppression is honoured by the trait, not by a check here
///
/// Every `Mailer` implementation calls `zeroship_mailer::check_suppression`
/// before transport and returns `MailerError::Suppressed`; that is the trait's
/// documented contract and re-checking it here would be a second answer to a
/// question the transport already answers. What this function adds is that the
/// suppression is RECORDED rather than swallowed, so the inviter learns the
/// address is unreachable instead of waiting for a reply that cannot come.
///
/// # It never returns an error
///
/// There is no failure of this function that should fail the request: the
/// invitation is already created and the token is already in the response. A
/// database error while RECORDING the outcome leaves `delivery` NULL, which is
/// exactly what it means - nobody knows - and is logged.
pub async fn deliver_invite(
    pg: &compio_postgres::Client,
    mailer: &dyn zeroship_mailer::Mailer,
    invite: &InviteRecord,
    token: &str,
) -> InviteDelivery {
    let (organization, inviter) = invite_mail_context(pg, invite).await;
    let expires_in = format!("{INVITE_TTL_DAYS} days");
    let rendered = match (OrganizationInvite {
        organization: &organization,
        inviter: &inviter,
        role: &invite.role,
        token,
        expires_in: &expires_in,
    })
    .render()
    {
        Ok(rendered) => rendered,
        Err(err) => {
            // A template that will not render is a build-time defect reaching
            // production. Record the failure rather than sending a blank body.
            tracing::error!(error = %err, invite_id = invite.id, "control: invite template failed");
            record_invite_delivery(pg, &invite.id, InviteDelivery::Failed).await;
            return InviteDelivery::Failed;
        }
    };

    let message = Email {
        to: Address {
            email: invite.email.clone(),
            name: None,
        },
        header_to: None,
        from: Address {
            email: INVITE_FROM_EMAIL.to_owned(),
            name: Some(INVITE_FROM_NAME.to_owned()),
        },
        reply_to: None,
        envelope_from: None,
        subject: rendered.subject,
        text: rendered.text,
        html: Some(rendered.html),
        headers: Vec::new(),
        tags: vec!["organization_invite".to_owned()],
        // No provider-side dedup key. Nothing re-drives this send: a token is
        // minted once, and a lost invitation is revoked and re-issued, which is
        // a DIFFERENT invitation and must not be deduplicated against the first.
        idempotency_key: None,
    };

    let outcome = match mailer.send(pg, message).await {
        Ok(_) => InviteDelivery::Sent,
        Err(zeroship_mailer::MailerError::Suppressed(_)) => {
            tracing::info!(
                invite_id = invite.id,
                "control: invitation not mailed; the address is suppressed"
            );
            InviteDelivery::Suppressed
        }
        Err(err) => {
            // The invite id, not the address: this line is written on a path a
            // caller can trigger at will, and the id is enough to find the row.
            // What the TRANSPORT puts in its own error text is the transport's
            // business and may well include the recipient - this is a choice
            // about the fields, not a guarantee about the whole line.
            tracing::warn!(error = %err, invite_id = invite.id, "control: invitation send failed");
            InviteDelivery::Failed
        }
    };
    record_invite_delivery(pg, &invite.id, outcome).await;
    outcome
}

/// The organization's display name and the inviter's, for the mail body.
///
/// Both fall back rather than failing: an invitation whose inviter row has been
/// erased (`invited_by` is `ON DELETE SET NULL`) still has to be deliverable,
/// and the organization name is decoration around a token that is the actual
/// payload.
async fn invite_mail_context(
    pg: &compio_postgres::Client,
    invite: &InviteRecord,
) -> (String, String) {
    // One statement rather than two round trips. The invite is the driving
    // row, so both joins hang off it: the organization is guaranteed by the
    // invite's foreign key, and the inviter is not (`invited_by` is `ON DELETE
    // SET NULL`), which is why only that one is a LEFT JOIN.
    let rows = pg
        .query(
            "SELECT o.name AS organization, u.name AS inviter \
               FROM zeroship.organization_invites i \
               JOIN zeroship.organizations o ON o.id = i.organization_id \
               LEFT JOIN zeroship.users u ON u.id = i.invited_by \
              WHERE i.id = $1",
            &[&invite.id],
        )
        .await;
    match rows {
        Ok(rows) => rows.first().map_or_else(
            || (invite.organization_id.clone(), "Someone".to_string()),
            |row| {
                (
                    row.get::<_, String>("organization"),
                    row.get::<_, Option<String>>("inviter")
                        .filter(|name| !name.trim().is_empty())
                        .unwrap_or_else(|| "Someone".to_string()),
                )
            },
        ),
        Err(err) => {
            tracing::warn!(error = %err, "control: could not read invite mail context");
            (invite.organization_id.clone(), "Someone".to_string())
        }
    }
}

/// Write the outcome into `organization_invites.delivery`.
///
/// One autocommit statement, no transaction and no organization lock. It writes
/// a column nothing derives authority from, on a row whose identity is already
/// fixed - so there is nothing for a lock to serialize with, and taking one
/// would put a mail transport's latency inside a lock every membership mutation
/// waits on.
async fn record_invite_delivery(
    pg: &compio_postgres::Client,
    invite_id: &str,
    outcome: InviteDelivery,
) {
    if let Err(err) = pg
        .execute(
            "UPDATE zeroship.organization_invites SET delivery = $2 WHERE id = $1",
            &[&invite_id, &outcome.as_str()],
        )
        .await
    {
        tracing::warn!(
            error = %err,
            invite_id,
            delivery = outcome.as_str(),
            "control: could not record invitation delivery"
        );
    }
}

fn invite_conflict(err: &compio_postgres::Error, context: &str) -> OrganizationError {
    use compio_postgres::error::SqlState;
    if err.code() == Some(&SqlState::UNIQUE_VIOLATION) {
        tracing::warn!(error = %err, context, "control: invite already outstanding");
        return OrganizationError::Invalid(
            "an unconsumed invitation to that address already exists; revoke it first".to_string(),
        );
    }
    db_error(err, context)
}

fn member_conflict(err: &compio_postgres::Error, context: &str) -> OrganizationError {
    use compio_postgres::error::SqlState;
    if err.code() == Some(&SqlState::UNIQUE_VIOLATION) {
        return OrganizationError::AlreadyMember;
    }
    db_error(err, context)
}

/// Revoke a pending invite. Deleting rather than tombstoning: the partial unique
/// index that limits one live invite per address keys on `consumed_at IS NULL`,
/// so a revoked row that stayed would keep the slot occupied forever.
pub async fn revoke_invite(
    registry: &Registry,
    principal: Uuid,
    organization_id: &str,
    invite_id: &str,
    source_ip: Option<&str>,
) -> Result<(), OrganizationError> {
    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin revoke invite"))?;
    lock_organization(&tx, organization_id).await?;

    // The actor must still outrank the role the invite grants: an admin demoted
    // to developer may no longer touch an invite that seats a developer.
    let sql = format!(
        "DELETE FROM zeroship.organization_invites i \
          USING {seat} \
          WHERE i.id = $2 AND i.organization_id = $1 AND i.consumed_at IS NULL \
            AND actor_role.rank > i.role_rank \
            AND actor_role.billing_rank >= i.role_billing_rank \
         RETURNING i.email::text AS email, i.role",
        seat = actor_seat("$1", "$3"),
    );
    let rows = tx
        .query(&sql, &[&organization_id, &invite_id, &principal])
        .await
        .map_err(|err| db_error(&err, "revoke invite"))?;

    let Some(row) = rows.first() else {
        let exists = tx
            .query(
                "SELECT 1 FROM zeroship.organization_invites \
                  WHERE id = $1 AND organization_id = $2 AND consumed_at IS NULL",
                &[&invite_id, &organization_id],
            )
            .await
            .map_err(|err| db_error(&err, "classify revoke refusal"))?;
        return Err(if exists.is_empty() {
            OrganizationError::InviteNotFound
        } else {
            OrganizationError::Insufficient(
                "revoking an invitation needs authority over the role it grants".to_string(),
            )
        });
    };
    let email: String = row.get("email");
    let role: String = row.get("role");

    audit_authority_change(
        &tx,
        principal,
        AuditAction::OrganizationInviteRevoked,
        organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "invite_id": invite_id,
            "email": email,
            "role": role,
        }),
    )
    .await;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit revoke invite"))?;
    Ok(())
}

/// Redeem an invite.
///
/// FOUR conditions ride in the claiming UPDATE, and the third is the one the
/// schema explicitly could not carry:
///
/// 1. unconsumed, and 2. unexpired;
/// 3. the INVITER'S LIVE authority still clears the frozen role. The schema's
///    CHECK froze the inviter's rank at ISSUE time; an admin demoted the next
///    day still has a pending invite naming a role they can no longer grant,
///    and only a redemption-time re-derivation can refuse it;
/// 4. the redeeming account's VERIFIED address is the invited address. Without
///    it the token alone would seat whoever presents it, and a forwarded mail
///    would be a transfer of the invitation.
///
/// A refusal reports one undifferentiated [`OrganizationError::InviteNotRedeemable`]:
/// telling the presenter WHICH condition failed turns the endpoint into an
/// oracle over other people's pending invitations.
pub async fn redeem_invite(
    registry: &Registry,
    principal: Uuid,
    token: &str,
    source_ip: Option<&str>,
) -> Result<OrganizationRecord, OrganizationError> {
    let digest = token_digest(token);

    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin redeem"))?;

    // Which organization to lock is itself a lookup by digest. A miss is
    // reported as not-redeemable rather than not-found: "this token names no
    // invite" and "this token names one you may not use" must be the same
    // answer.
    let located = tx
        .query(
            "SELECT organization_id FROM zeroship.organization_invites WHERE token_hash = $1",
            &[&digest],
        )
        .await
        .map_err(|err| db_error(&err, "locate invite"))?;
    let Some(row) = located.first() else {
        return Err(OrganizationError::InviteNotRedeemable);
    };
    let organization_id: String = row.get("organization_id");
    lock_organization(&tx, &organization_id).await?;

    let claimed = tx
        .query(
            "UPDATE zeroship.organization_invites i \
                SET consumed_at = NOW(), consumed_by = $2 \
              WHERE i.token_hash = $1 \
                AND i.consumed_at IS NULL \
                AND i.expires_at > NOW() \
                AND EXISTS (SELECT 1 FROM zeroship.organization_members inviter \
                              JOIN zeroship.organization_roles inviter_role \
                                   ON inviter_role.role = inviter.role \
                             WHERE inviter.organization_id = i.organization_id \
                               AND inviter.user_id = i.invited_by \
                               AND inviter_role.rank > i.role_rank \
                               AND inviter_role.billing_rank >= i.role_billing_rank) \
                AND EXISTS (SELECT 1 FROM zeroship.users u \
                             WHERE u.id = $2 AND u.email = i.email \
                               AND u.email_verified_at IS NOT NULL) \
             RETURNING i.id, i.organization_id, i.role, i.invited_by",
            &[&digest, &principal],
        )
        .await
        .map_err(|err| db_error(&err, "claim invite"))?;
    let Some(row) = claimed.first() else {
        return Err(OrganizationError::InviteNotRedeemable);
    };
    let invite_id: String = row.get("id");
    let role: String = row.get("role");
    let invited_by: Option<Uuid> = row.get("invited_by");

    // The seat itself. No rank predicate: the claim above IS the authority, and
    // it re-derived the inviter's live rank against the frozen role rather than
    // trusting the frozen copy. A conflict means the person was seated by some
    // other path while the invite was outstanding; the invite is spent either
    // way, which is why this is `DO NOTHING` and not an error.
    tx.execute(
        "INSERT INTO zeroship.organization_members \
             (organization_id, user_id, role, added_by, changed_by) \
         VALUES ($1, $2, $3, $4, $4) \
         ON CONFLICT (organization_id, user_id) DO NOTHING",
        &[&organization_id, &principal, &role, &invited_by],
    )
    .await
    .map_err(|err| db_error(&err, "seat redeemed member"))?;

    audit_authority_change(
        &tx,
        principal,
        AuditAction::OrganizationInviteRedeemed,
        &organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "invite_id": invite_id,
            "user_id": principal,
            "role": role,
        }),
    )
    .await;

    let record = get_organization(&tx, &organization_id).await?;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit redeem"))?;
    Ok(record)
}

// ---------------------------------------------------------------------------
// Projects
// ---------------------------------------------------------------------------

/// Create a project inside an organization. Admin and above, and the predicate
/// says so in the INSERT.
///
/// The threshold is `admin` because that is where the narrowing stops applying:
/// below it a member reaches only projects they hold a row on, so letting them
/// mint one would let them mint their own authority.
pub async fn create_project(
    registry: &Registry,
    principal: Uuid,
    organization_id: &str,
    body: &CreateProjectBody,
    source_ip: Option<&str>,
) -> Result<ProjectRecord, OrganizationError> {
    validate_name(&body.name)?;
    let slug = match body.slug.as_deref().map(str::trim) {
        Some(slug) if !slug.is_empty() => slug.to_string(),
        _ => slug_from_name(&body.name).ok_or_else(|| {
            OrganizationError::Invalid(
                "could not derive a slug from that name; supply one explicitly".to_string(),
            )
        })?,
    };
    validate_slug(&slug)?;
    let project_id = ProjectId::mint();

    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin create project"))?;
    lock_organization(&tx, organization_id).await?;

    let sql = format!(
        "INSERT INTO zeroship.projects (id, organization_id, slug, name, created_by) \
         SELECT $1, $2, $3, $4, $5 \
           FROM {seat} \
          WHERE actor_role.rank >= {admin} \
         RETURNING id, organization_id, slug::text AS slug, name, created_at",
        seat = actor_seat("$2", "$5"),
        admin = ladder_rank("$6"),
    );
    let rows = tx
        .query(
            &sql,
            &[
                &project_id.as_str(),
                &organization_id,
                &slug,
                &body.name.trim(),
                &principal,
                &ROLE_ADMIN,
            ],
        )
        .await
        .map_err(|err| slug_conflict(&err, &slug, "create project"))?;
    let Some(row) = rows.first() else {
        return Err(OrganizationError::Insufficient(
            "creating a project needs admin authority in the organization".to_string(),
        ));
    };
    let record = row_to_project(row);

    audit_authority_change(
        &tx,
        principal,
        AuditAction::ProjectCreated,
        organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "project_id": record.id,
            "slug": record.slug,
        }),
    )
    .await;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit create project"))?;
    Ok(record)
}

/// Rename or re-slug a project. Admin and above, said in the UPDATE.
///
/// The threshold matches [`create_project`] and the `project:write` band, and
/// it is the same argument: below admin a member reaches only the projects they
/// hold a row on, so letting them re-slug one would let them reshape the thing
/// that grants them reach.
pub async fn update_project(
    registry: &Registry,
    principal: Uuid,
    project_id: &str,
    body: &UpdateProjectBody,
    source_ip: Option<&str>,
) -> Result<ProjectRecord, OrganizationError> {
    if let Some(name) = body.name.as_deref() {
        validate_name(name)?;
    }
    if let Some(slug) = body.slug.as_deref() {
        validate_slug(slug)?;
    }
    if body.name.is_none() && body.slug.is_none() {
        return Err(OrganizationError::Invalid(
            "supply at least one of name or slug".to_string(),
        ));
    }

    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin update project"))?;
    let organization_id = lock_project_organization(&tx, project_id).await?;

    let sql = format!(
        "UPDATE zeroship.projects p \
            SET name = COALESCE($3, p.name), \
                slug = COALESCE($4, p.slug), \
                updated_at = NOW() \
          WHERE p.id = $1 \
            AND EXISTS (SELECT 1 FROM {seat} WHERE actor_role.rank >= {admin}) \
        RETURNING p.id, p.organization_id, p.slug::text AS slug, p.name, p.created_at",
        seat = actor_seat("$2", "$5"),
        admin = ladder_rank("$6"),
    );
    let name = body.name.as_deref().map(str::trim);
    let slug = body.slug.as_deref().map(str::trim);
    let rows = tx
        .query(
            &sql,
            &[
                &project_id,
                &organization_id,
                &name,
                &slug,
                &principal,
                &ROLE_ADMIN,
            ],
        )
        .await
        .map_err(|err| slug_conflict(&err, slug.unwrap_or(""), "update project"))?;
    let Some(row) = rows.first() else {
        // The project exists (`lock_project_organization` proved it) and is not
        // dissolved, so the only clause left is the rank predicate.
        return Err(OrganizationError::Insufficient(
            "renaming a project needs admin authority in the organization".to_string(),
        ));
    };
    let record = row_to_project(row);

    audit_authority_change(
        &tx,
        principal,
        AuditAction::ProjectUpdated,
        &organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "project_id": project_id,
            "name": name,
            "slug": slug,
        }),
    )
    .await;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit update project"))?;
    Ok(record)
}

/// Delete a project.
///
/// Hard, unlike [`dissolve_organization`], and the asymmetry is the point: a
/// project is not a billing subject and names no money record, so there is
/// nothing that has to outlive it. Its `project_members` rows go with it by
/// cascade, which is right - a seat on a project that does not exist is not a
/// record worth keeping.
///
/// # Apps refuse it, and the predicate says so rather than the constraint
///
/// `apps.project_id` is `ON DELETE RESTRICT`. Leaning on that would surface a
/// constraint name; the `NOT EXISTS` clause here is the same rule under the
/// organization row lock, and the classifier turns zero rows into a count and a
/// remedy.
pub async fn delete_project(
    registry: &Registry,
    principal: Uuid,
    project_id: &str,
    source_ip: Option<&str>,
) -> Result<(), OrganizationError> {
    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin delete project"))?;
    let organization_id = lock_project_organization(&tx, project_id).await?;

    let sql = format!(
        "DELETE FROM zeroship.projects p \
          WHERE p.id = $1 \
            AND EXISTS (SELECT 1 FROM {seat} WHERE actor_role.rank >= {admin}) \
            AND NOT EXISTS (SELECT 1 FROM zeroship.apps a WHERE a.project_id = p.id) \
         RETURNING p.slug::text AS slug",
        seat = actor_seat("$2", "$3"),
        admin = ladder_rank("$4"),
    );
    let rows = tx
        .query(
            &sql,
            &[&project_id, &organization_id, &principal, &ROLE_ADMIN],
        )
        .await
        .map_err(|err| db_error(&err, "delete project"))?;
    let Some(row) = rows.first() else {
        return Err(classify_project_deletion_refusal(&tx, &organization_id, principal, project_id).await);
    };
    let slug: String = row.get("slug");

    audit_authority_change(
        &tx,
        principal,
        AuditAction::ProjectDeleted,
        &organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "project_id": project_id,
            "slug": slug,
        }),
    )
    .await;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit delete project"))?;
    Ok(())
}

/// Delete an app: the terminal end of the app lifecycle, and the step the
/// account-closure funnel used to name without providing.
///
/// # Why it lives here and not beside `archive_app`
///
/// This is the bottom of the same funnel [`delete_project`] and
/// [`dissolve_organization`] sit in, and it borrows both of their rules: the
/// organization row is locked first, a dissolved organization refuses, and the
/// `admin` floor rides in the effect statement rather than in a Rust `if`. The
/// authority to end an app is the authority to end the project it lives in;
/// splitting the two across modules is how they would drift apart.
///
/// # It is a MARKER, and the two children of `zeroship.apps` say why
///
/// `invoice_lines.app_id` is `ON DELETE RESTRICT`, so a row delete is refused
/// outright once a finalized line prices the app. `usage_aggregates.app_id` is
/// `ON DELETE CASCADE`, so a row delete instead destroys the input the
/// unbilled-usage predicate reads - the same predicate `dissolve` and the
/// erasure preflight refuse on. A hard delete is therefore impossible or
/// destructive depending only on whether the reconciler has run, and
/// `db/migrations-ts/20260831000000_archive_apps.ts` revoked the DELETE
/// privilege for that reason. The row is retained and marked; nothing cascades.
///
/// # What ends, and what is kept
///
/// **Ends.** The project edge (`project_id` is set NULL, which is what lets the
/// project be deleted afterwards - the ownership key is `ON DELETE RESTRICT`
/// and a retained edge would pin the project forever). The current artifact
/// pointer, so no route or worker can serve the app again. The whole creator
/// environment - vars, secrets, and the `process.env` expose list - because
/// that is the app's live capability rather than a record of anything.
///
/// **Kept.** Every billing record: `usage_aggregates`, `app_usage_history`,
/// `invoice_lines`, `plan_change_events`, `spend_state_history`. The reconciler
/// and [`crate::billing_read::outstanding_billing`] both reach an app through
/// `apps.organization_id`, never through its project, so a detached app is
/// still billed and still owes. The audit trail, which is append-only and keyed
/// to no parent. And the app's NAME, which is its routable hostname: it is
/// retired rather than released, because old links, cookies and OAuth redirect
/// URIs still point at it and recycling it would hand the next registrant an
/// audience it never earned.
///
/// **Not this service's to end.** The deploy blobs are content-addressed and
/// shared by hash across apps and deploys, so reclaiming them is a sweep over
/// the whole store rather than a statement here. The app's database schema and
/// role are privileged teardown and belong to migrate-server, for the reason
/// [`Registry::archive_app`] already gives: control holds no provisioning DSN.
///
/// # A deleted app is UNREACHABLE afterwards, by construction
///
/// `zeroship_authz` resolves an app's organization only through its project, so
/// once the edge is cut every app-scoped Cedar check on it resolves to rank 0
/// and denies. A repeated delete is answered `403`, not `404`, which is the
/// same answer a stranger's app id gets. Nothing here relies on that: the
/// statement below carries its own `deleted_at IS NULL` guard, so a second
/// delete could not move the marker even if it were reached.
pub async fn delete_app(
    registry: &Registry,
    principal: Uuid,
    app_id: Uuid,
    source_ip: Option<&str>,
) -> Result<(), OrganizationError> {
    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin delete app"))?;
    let AppOwnership {
        organization_id,
        project_id,
    } = lock_app_organization(&tx, app_id).await?;
    // The same advisory lock `archive_app`, `unarchive_app` and the worker's
    // final workflow claim take. Once this returns, no claim and no restore can
    // have crossed the marker.
    tx.query_one(
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
        &[&zeroship_core::app_derivation::lifecycle_lock_seed(
            &zeroship_core::app_id::AppId::from_uuid(&app_id),
        )],
    )
    .await
    .map_err(|err| db_error(&err, "lock app lifecycle"))?;

    let sql = format!(
        "UPDATE zeroship.apps a \
            SET deleted_at = NOW(), \
                project_id = NULL, \
                deploy_hash = NULL, \
                manifest_json = NULL, \
                env_version = a.env_version + 1, \
                updated_at = NOW() \
          WHERE a.id = $1 \
            AND a.deleted_at IS NULL \
            AND a.archived_at IS NOT NULL \
            AND EXISTS (SELECT 1 FROM {seat} WHERE actor_role.rank >= {admin}) \
         RETURNING a.name, a.project_id AS still_attached",
        seat = actor_seat("$2", "$3"),
        admin = ladder_rank("$4"),
    );
    let rows = tx
        .query(&sql, &[&app_id, &organization_id, &principal, &ROLE_ADMIN])
        .await
        .map_err(|err| db_error(&err, "delete app"))?;
    let Some(row) = rows.first() else {
        return Err(classify_app_deletion_refusal(&tx, &organization_id, principal, app_id).await);
    };
    let name: String = row.get("name");
    debug_assert!(
        row.get::<_, Option<String>>("still_attached").is_none(),
        "a deleted app must have left its project"
    );

    // The environment goes with the app. These three tables are the creator's
    // live capability - what the running app could read - and none of them is
    // evidence of anything. Retaining a deleted app's secret material because
    // nothing can reach it any more is an argument that stops being true the
    // day something can.
    for table in ["app_vars", "app_secrets", "app_env_expose"] {
        tx.execute(
            &format!("DELETE FROM zeroship.{table} WHERE app_id = $1"),
            &[&app_id],
        )
        .await
        .map_err(|err| db_error(&err, "purge deleted app environment"))?;
    }

    audit::log_in_tx(
        &tx,
        AuditEntry {
            app_id: Some(app_id),
            organization_id: Some(&organization_id),
            actor_user_id: Some(principal),
            action: AuditAction::AppDeleted,
            resource: Some(&project_id),
            source_ip,
        },
        &json!({
            "organization_id": organization_id,
            "project_id": project_id,
            "app_id": app_id,
            "name": name,
        }),
    )
    .await;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit delete app"))?;
    Ok(())
}

/// Who owns one app, read under the organization row lock.
struct AppOwnership {
    organization_id: String,
    /// The project the app belongs to WHILE it is live. Read before the delete
    /// cuts the edge, because afterwards nothing can reconstruct it and the
    /// audit row is the only place it survives.
    project_id: String,
}

/// Lock the ORGANIZATION that owns `app_id`, and return the ownership pair.
///
/// The peer of [`lock_project_organization`], reached one edge further out: an
/// app names a project and a project names an organization. An app whose
/// project edge is already cut - which is what deletion does - is reported
/// [`OrganizationError::AppNotFound`] by this join rather than by a separate
/// state read, because after the cut there is no longer any organization the
/// caller can be shown to have authority over.
async fn lock_app_organization<C: GenericClient + Sync>(
    tx: &C,
    app_id: Uuid,
) -> Result<AppOwnership, OrganizationError> {
    let rows = tx
        .query(
            "SELECT o.id, o.dissolved_at, p.id AS project_id \
               FROM zeroship.organizations o \
               JOIN zeroship.projects p ON p.organization_id = o.id \
               JOIN zeroship.apps a ON a.project_id = p.id \
              WHERE a.id = $1 FOR UPDATE OF o",
            &[&app_id],
        )
        .await
        .map_err(|err| db_error(&err, "lock app organization"))?;
    let Some(row) = rows.first() else {
        return Err(OrganizationError::AppNotFound);
    };
    match row.get::<_, Option<DateTime<Utc>>>("dissolved_at") {
        Some(at) => Err(OrganizationError::Dissolved(at)),
        None => Ok(AppOwnership {
            organization_id: row.get("id"),
            project_id: row.get("project_id"),
        }),
    }
}

/// Seat an organization member on one project.
///
/// The two composite foreign keys on `project_members` make a row naming
/// another organization's project, or a user who is not a member of this
/// organization, UNSPELLABLE - so neither is checked here. What IS checked is
/// the actor's authority, and the threshold is `admin` for the same reason
/// [`create_project`] uses it: below admin, project membership is the thing that
/// grants reach, so a member below admin granting it would be granting
/// themselves.
pub async fn add_project_member(
    registry: &Registry,
    principal: Uuid,
    project_id: &str,
    body: &AddProjectMemberBody,
    source_ip: Option<&str>,
) -> Result<ProjectMemberRecord, OrganizationError> {
    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin add project member"))?;
    let organization_id = lock_project_organization(&tx, project_id).await?;

    let sql = format!(
        "INSERT INTO zeroship.project_members \
             (project_id, organization_id, user_id, role, added_by, changed_by) \
         SELECT p.id, p.organization_id, $2, target_role.role, $4, $4 \
           FROM zeroship.projects p, zeroship.organization_roles target_role, {seat} \
          WHERE p.id = $1 \
            AND target_role.role = $3 \
            AND actor_role.rank >= {admin} \
            AND actor_role.rank > target_role.rank \
            AND actor_role.billing_rank >= target_role.billing_rank \
         RETURNING project_id, user_id, role, added_at",
        seat = actor_seat("$5", "$4"),
        admin = ladder_rank("$6"),
    );
    let rows = tx
        .query(
            &sql,
            &[
                &project_id,
                &body.user_id,
                &body.role,
                &principal,
                &organization_id,
                &ROLE_ADMIN,
            ],
        )
        .await
        .map_err(|err| project_member_conflict(&err, "add project member"))?;
    let Some(row) = rows.first() else {
        return Err(classify_project_seat_refusal(
            &tx,
            &organization_id,
            principal,
            body.user_id,
            &body.role,
        )
        .await);
    };
    let added_at: DateTime<Utc> = row.get("added_at");
    let role: String = row.get("role");

    audit_authority_change(
        &tx,
        principal,
        AuditAction::ProjectMemberAdded,
        &organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "project_id": project_id,
            "user_id": body.user_id,
            "role": role,
        }),
    )
    .await;

    let record = ProjectMemberRecord {
        project_id: project_id.to_string(),
        user_id: body.user_id,
        email: member_email(&tx, body.user_id).await?,
        role,
        added_at,
    };

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit add project member"))?;
    Ok(record)
}

fn project_member_conflict(err: &compio_postgres::Error, context: &str) -> OrganizationError {
    use compio_postgres::error::SqlState;
    match err.code() {
        Some(code) if code == &SqlState::UNIQUE_VIOLATION => OrganizationError::AlreadyMember,
        // The composite foreign key onto `organization_members` is what refuses
        // a user with no seat in the owning organization. It is a structural
        // refusal, not a rank one, and reporting it as a 500 would send the
        // caller looking in the wrong place.
        Some(code) if code == &SqlState::FOREIGN_KEY_VIOLATION => OrganizationError::Invalid(
            "that user holds no seat in this project's organization; add them to the \
             organization first"
                .to_string(),
        ),
        _ => db_error(err, context),
    }
}

/// Move a project seat to another role, in one statement.
///
/// # Why this is not delete-then-add
///
/// Two calls have a window between them in which the member holds NO project
/// row, and a member below admin with no project row reaches nothing - so
/// narrowing someone from project owner to project viewer used to blank their
/// access first and restore part of it after. Worse in the other direction: a
/// crash between the two leaves the seat gone rather than narrowed, and the
/// caller who asked for an adjustment has performed a removal.
///
/// # It compares BOTH roles, exactly as [`change_member_role`] does
///
/// The actor must outrank the role being granted AND the role being taken away.
/// Checking only the new role would let an admin narrow an owner's project seat;
/// checking only the old one would let them widen a viewer's.
pub async fn change_project_member_role(
    registry: &Registry,
    principal: Uuid,
    project_id: &str,
    user_id: Uuid,
    body: &ChangeRoleBody,
    source_ip: Option<&str>,
) -> Result<ProjectMemberRecord, OrganizationError> {
    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin change project role"))?;
    let organization_id = lock_project_organization(&tx, project_id).await?;

    // `held_role`, not `current_role`: `CURRENT_ROLE` is a reserved SQL keyword
    // and PostgreSQL refuses it as a bare alias. Same trap `change_member_role`
    // documents, and it fails at execute time rather than at compile time.
    let sql = format!(
        "UPDATE zeroship.project_members pm \
            SET role = target_role.role, changed_at = NOW(), changed_by = $4 \
           FROM zeroship.organization_roles target_role, \
                zeroship.organization_roles held_role, \
                {seat} \
          WHERE pm.project_id = $1 AND pm.user_id = $2 \
            AND target_role.role = $3 \
            AND held_role.role = pm.role \
            AND actor_role.rank >= {admin} \
            AND actor_role.rank > target_role.rank \
            AND actor_role.billing_rank >= target_role.billing_rank \
            AND actor_role.rank > held_role.rank \
            AND actor_role.billing_rank >= held_role.billing_rank \
         RETURNING pm.role, pm.added_at, held_role.role AS previous_role",
        seat = actor_seat("$5", "$4"),
        admin = ladder_rank("$6"),
    );
    let rows = tx
        .query(
            &sql,
            &[
                &project_id,
                &user_id,
                &body.role,
                &principal,
                &organization_id,
                &ROLE_ADMIN,
            ],
        )
        .await
        .map_err(|err| db_error(&err, "change project member role"))?;
    let Some(row) = rows.first() else {
        let seated = tx
            .query(
                "SELECT 1 FROM zeroship.project_members WHERE project_id = $1 AND user_id = $2",
                &[&project_id, &user_id],
            )
            .await
            .map_err(|err| db_error(&err, "classify project role change"))?;
        return Err(if seated.is_empty() {
            OrganizationError::MemberNotFound
        } else {
            classify_seat_refusal(
                &tx,
                &organization_id,
                principal,
                &body.role,
                Some(ROLE_ADMIN),
            )
            .await
        });
    };
    let added_at: DateTime<Utc> = row.get("added_at");
    let previous_role: String = row.get("previous_role");
    let role: String = row.get("role");

    audit_authority_change(
        &tx,
        principal,
        AuditAction::ProjectMemberRoleChanged,
        &organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "project_id": project_id,
            "user_id": user_id,
            "from": previous_role,
            "to": role,
        }),
    )
    .await;

    let record = ProjectMemberRecord {
        project_id: project_id.to_string(),
        user_id,
        email: member_email(&tx, user_id).await?,
        role,
        added_at,
    };

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit change project role"))?;
    Ok(record)
}

pub async fn remove_project_member(
    registry: &Registry,
    principal: Uuid,
    project_id: &str,
    user_id: Uuid,
    source_ip: Option<&str>,
) -> Result<(), OrganizationError> {
    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin remove project member"))?;
    let organization_id = lock_project_organization(&tx, project_id).await?;

    let sql = format!(
        "DELETE FROM zeroship.project_members pm \
          USING zeroship.organization_roles target_role, {seat} \
          WHERE pm.project_id = $1 AND pm.user_id = $2 \
            AND target_role.role = pm.role \
            AND actor_role.rank >= {admin} \
            AND actor_role.rank > target_role.rank \
            AND actor_role.billing_rank >= target_role.billing_rank \
         RETURNING pm.role",
        seat = actor_seat("$4", "$3"),
        admin = ladder_rank("$5"),
    );
    let rows = tx
        .query(
            &sql,
            &[
                &project_id,
                &user_id,
                &principal,
                &organization_id,
                &ROLE_ADMIN,
            ],
        )
        .await
        .map_err(|err| db_error(&err, "remove project member"))?;
    let Some(row) = rows.first() else {
        let exists = tx
            .query(
                "SELECT 1 FROM zeroship.project_members WHERE project_id = $1 AND user_id = $2",
                &[&project_id, &user_id],
            )
            .await
            .map_err(|err| db_error(&err, "classify project removal"))?;
        return Err(if exists.is_empty() {
            OrganizationError::MemberNotFound
        } else {
            OrganizationError::Insufficient(
                "removing a project member needs admin authority in the organization".to_string(),
            )
        });
    };
    let role: String = row.get("role");

    audit_authority_change(
        &tx,
        principal,
        AuditAction::ProjectMemberRemoved,
        &organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "project_id": project_id,
            "user_id": user_id,
            "role": role,
        }),
    )
    .await;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit remove project member"))?;
    Ok(())
}

/// Lock the ORGANIZATION that owns `project_id`, and return its id.
///
/// Project membership mutations serialize on the organization row rather than
/// on the project, because that is the row every other membership path already
/// locks: two locks over one authority model would deadlock the first time a
/// caller re-roled someone at both heights.
/// It carries the same dissolved refusal [`lock_organization`] does, for the
/// same reason: a project of a closed organization is part of a closed record.
/// In practice a dissolved organization owns no projects at all - dissolve
/// refuses while any remains - so this arm is the one that stays true if that
/// ever stops being so.
async fn lock_project_organization<C: GenericClient + Sync>(
    tx: &C,
    project_id: &str,
) -> Result<String, OrganizationError> {
    let rows = tx
        .query(
            "SELECT o.id, o.dissolved_at FROM zeroship.organizations o \
               JOIN zeroship.projects p ON p.organization_id = o.id \
              WHERE p.id = $1 FOR UPDATE OF o",
            &[&project_id],
        )
        .await
        .map_err(|err| db_error(&err, "lock project organization"))?;
    let Some(row) = rows.first() else {
        return Err(OrganizationError::ProjectNotFound);
    };
    match row.get::<_, Option<DateTime<Utc>>>("dissolved_at") {
        Some(at) => Err(OrganizationError::Dissolved(at)),
        None => Ok(row.get::<_, String>("id")),
    }
}

// ---------------------------------------------------------------------------
// The personal organization
// ---------------------------------------------------------------------------

/// The project a creator's first deploy lands in, minting the personal
/// organization and its default project if they do not exist yet.
///
/// A creator's first deploy must stay ONE step. The alternative - make them
/// create an organization, then a project, then an app - is three round trips
/// before anything runs, and every one of them is a decision they have no
/// information to make yet.
///
/// A personal organization is an ORDINARY row: it has members, projects, apps
/// and a billing subject exactly like any other, and no read path branches on
/// `personal_owner_id`. The pointer exists so a second call finds the first
/// call's organization, and so transferring ownership can clear it.
///
/// # Idempotence under a race
///
/// Three statements, each with its own snapshot, rather than one CTE. A CTE's
/// `ON CONFLICT DO NOTHING` and its sibling SELECT share one snapshot, so a
/// concurrent minter that commits between them yields BOTH an empty insert and
/// an empty read. Re-reading in a separate statement is what closes that.
///
/// Takes a [`Registry`] rather than the `AppState` every other mutation here
/// takes, because `dev-provision` needs the same one-step landing place and has
/// no `AppState` to give. Sharing the function is the point: a second
/// "provision a project for this owner" path would be a second personal
/// organization policy.
pub async fn ensure_personal_project(
    registry: &Registry,
    principal: Uuid,
) -> Result<ProjectId, OrganizationError> {
    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin ensure personal project"))?;

    let organization_id = ensure_personal_organization(&tx, principal).await?;
    lock_organization(&tx, &organization_id).await?;

    // The owner seat. `ON CONFLICT DO NOTHING` and no rank predicate, and the
    // absence of one is safe for a reason worth stating: reaching here means
    // `personal_owner_id` names this principal, and an owner seat in an
    // organization whose pointer names you cannot have been taken away -
    // an admin cannot remove an owner (`30 > 40` is false) and the last owner
    // cannot be removed at all.
    tx.execute(
        "INSERT INTO zeroship.organization_members \
             (organization_id, user_id, role, added_by, changed_by) \
         VALUES ($1, $2, $3, $2, $2) \
         ON CONFLICT (organization_id, user_id) DO NOTHING",
        &[&organization_id, &principal, &ROLE_OWNER],
    )
    .await
    .map_err(|err| db_error(&err, "seat personal owner"))?;

    let project_id = ensure_default_project(&tx, &organization_id, principal).await?;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit ensure personal project"))?;
    Ok(project_id)
}

/// The slug of a personal organization.
///
/// Derived from the owner's id rather than their name, so it is globally unique
/// by construction and needs no retry loop. It is ugly on purpose and it is not
/// permanent: `organization:write` renames it, and a creator who never looks at
/// it never meets it.
fn personal_slug(owner: Uuid) -> String {
    format!("personal-{}", owner.simple())
}

async fn ensure_personal_organization<C: GenericClient + Sync>(
    tx: &C,
    principal: Uuid,
) -> Result<String, OrganizationError> {
    if let Some(existing) = personal_organization_of(tx, principal).await? {
        return Ok(existing);
    }
    let organization_id = OrganizationId::mint();
    let rows = tx
        .query(
            // The inference clause names BOTH conjuncts of
            // `organizations_live_personal_owner_key`. PostgreSQL matches a
            // partial unique index only when the statement's own predicate
            // implies the index's, and a mismatch is not a silent widening - it
            // is "there is no unique or exclusion constraint matching the ON
            // CONFLICT specification", on the first deploy of a fresh account.
            "INSERT INTO zeroship.organizations \
                 (id, slug, name, billing_email, personal_owner_id, created_by) \
             SELECT $1, $2, u.name, u.email, u.id, u.id \
               FROM zeroship.users u WHERE u.id = $3 \
             ON CONFLICT (personal_owner_id) \
                 WHERE personal_owner_id IS NOT NULL AND dissolved_at IS NULL DO NOTHING \
             RETURNING id",
            &[
                &organization_id.as_str(),
                &personal_slug(principal),
                &principal,
            ],
        )
        .await
        .map_err(|err| db_error(&err, "mint personal organization"))?;
    if let Some(row) = rows.first() {
        return Ok(row.get("id"));
    }
    // Either a concurrent call minted it, or the principal has no user row.
    // The re-read distinguishes them.
    personal_organization_of(tx, principal)
        .await?
        .ok_or(OrganizationError::UserNotFound)
}

/// The creator's LIVE personal organization, if they have one.
///
/// `dissolved_at IS NULL` is the whole of what makes closing one recoverable: a
/// creator who closed their personal workspace gets a fresh one on their next
/// deploy rather than resolving back into a row that refuses everything. The
/// filter matches the predicate on `organizations_live_personal_owner_key`, so
/// the read and the uniqueness rule say the same thing.
async fn personal_organization_of<C: GenericClient + Sync>(
    tx: &C,
    principal: Uuid,
) -> Result<Option<String>, OrganizationError> {
    let rows = tx
        .query(
            "SELECT id FROM zeroship.organizations \
              WHERE personal_owner_id = $1 AND dissolved_at IS NULL",
            &[&principal],
        )
        .await
        .map_err(|err| db_error(&err, "read personal organization"))?;
    Ok(rows.first().map(|row| row.get("id")))
}

async fn ensure_default_project<C: GenericClient + Sync>(
    tx: &C,
    organization_id: &str,
    principal: Uuid,
) -> Result<ProjectId, OrganizationError> {
    let existing = tx
        .query(
            "SELECT id FROM zeroship.projects \
              WHERE organization_id = $1 ORDER BY created_at, id LIMIT 1",
            &[&organization_id],
        )
        .await
        .map_err(|err| db_error(&err, "read default project"))?;
    if let Some(row) = existing.first() {
        return parse_project_id(&row.get::<_, String>("id"));
    }
    let project_id = ProjectId::mint();
    let rows = tx
        .query(
            "INSERT INTO zeroship.projects (id, organization_id, slug, name, created_by) \
             VALUES ($1, $2, 'default', 'Default', $3) \
             ON CONFLICT (organization_id, slug) DO NOTHING \
             RETURNING id",
            &[&project_id.as_str(), &organization_id, &principal],
        )
        .await
        .map_err(|err| db_error(&err, "mint default project"))?;
    if let Some(row) = rows.first() {
        return parse_project_id(&row.get::<_, String>("id"));
    }
    let raced = tx
        .query(
            "SELECT id FROM zeroship.projects WHERE organization_id = $1 AND slug = 'default'",
            &[&organization_id],
        )
        .await
        .map_err(|err| db_error(&err, "re-read default project"))?;
    raced
        .first()
        .ok_or(OrganizationError::ProjectNotFound)
        .and_then(|row| parse_project_id(&row.get::<_, String>("id")))
}

/// A stored project id that does not parse is a corrupt row, not a caller
/// error. It is reported as a database failure rather than silently handed on
/// as a bare string, because the shape CHECK on the column makes it
/// unreachable and an unreachable arm that degrades quietly is how a corrupt
/// row becomes a routing decision.
fn parse_project_id(raw: &str) -> Result<ProjectId, OrganizationError> {
    ProjectId::parse(raw).map_err(|err| {
        tracing::error!(error = %err, raw, "control: stored project id is malformed");
        OrganizationError::Db
    })
}

// ---------------------------------------------------------------------------
// Refusal classification
// ---------------------------------------------------------------------------
//
// Every function below runs AFTER an effect statement matched nothing. The
// decision was the statement's; these only turn "zero rows" into the sentence a
// human can act on. They are deliberately separate from the effect: folding the
// diagnosis into the predicate would make the predicate describe the error
// instead of the rule.

/// `floor` is the ladder role the calling STATEMENT requires of the actor, or
/// `None` for a statement that carries no floor. It is a parameter rather than a
/// constant because the statements that reach here disagree: the seating and
/// inviting writes require `admin`, while `change_member_role` and
/// `remove_member` require only the strict inequality. A classifier that assumed
/// either one would name the wrong refusal for the other half - and "needs a
/// strictly higher rank" told to a developer who HAS a strictly higher rank is
/// the kind of true-sounding message that sends a reader looking at the ladder
/// instead of the floor.
async fn classify_seat_refusal<C: GenericClient + Sync>(
    tx: &C,
    organization_id: &str,
    principal: Uuid,
    role: &str,
    floor: Option<&str>,
) -> OrganizationError {
    let sql = format!(
        "SELECT (SELECT COUNT(*) FROM zeroship.organization_roles WHERE role = $3)::bigint \
                AS role_known, \
                (SELECT COUNT(*) FROM {seat})::bigint AS actor_seated, \
                (SELECT COUNT(*) FROM {seat} WHERE actor_role.rank >= {floor})::bigint \
                AS actor_over_floor",
        seat = actor_seat("$1", "$2"),
        // No floor is spelled as rank >= 0, so `actor_over_floor` collapses onto
        // `actor_seated` and the floor arm below cannot fire.
        floor = floor.map_or_else(|| "0".to_string(), ladder_rank_of),
    );
    let rows = match tx.query(&sql, &[&organization_id, &principal, &role]).await {
        Ok(rows) => rows,
        Err(err) => return db_error(&err, "classify seat refusal"),
    };
    let Some(row) = rows.first() else {
        return OrganizationError::Db;
    };
    let role_known: i64 = row.get("role_known");
    let actor_seated: i64 = row.get("actor_seated");
    let actor_over_floor: i64 = row.get("actor_over_floor");
    if role_known == 0 {
        return OrganizationError::Invalid(format!(
            "unknown role {role:?}; the ladder is viewer, developer, billing, admin, owner"
        ));
    }
    if actor_seated == 0 {
        return OrganizationError::Insufficient(
            "you hold no seat in this organization".to_string(),
        );
    }
    if let Some(floor) = floor.filter(|_| actor_over_floor == 0) {
        return OrganizationError::Insufficient(format!(
            "granting membership needs the {floor:?} rank or higher, whatever the role granted"
        ));
    }
    OrganizationError::Insufficient(format!(
        "granting {role:?} needs a strictly higher rank and at least equal billing authority"
    ))
}

async fn classify_member_refusal<C: GenericClient + Sync>(
    tx: &C,
    organization_id: &str,
    principal: Uuid,
    user_id: Uuid,
    new_role: Option<&str>,
) -> OrganizationError {
    let rows = tx
        .query(
            "SELECT m.role, \
                    (SELECT COUNT(*) FROM zeroship.organization_members owners \
                      WHERE owners.organization_id = $1 AND owners.role = $3)::bigint AS owners \
               FROM zeroship.organization_members m \
              WHERE m.organization_id = $1 AND m.user_id = $2",
            &[&organization_id, &user_id, &ROLE_OWNER],
        )
        .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(err) => return db_error(&err, "classify member refusal"),
    };
    let Some(row) = rows.first() else {
        return OrganizationError::MemberNotFound;
    };
    let current_role: String = row.get("role");
    let owners: i64 = row.get("owners");
    // The last-owner clause is checked before the rank one because it is the
    // refusal a legitimate owner meets, and it names a remedy the rank refusal
    // does not have.
    let demoting_away_from_owner = new_role.is_none_or(|role| role != ROLE_OWNER);
    if current_role == ROLE_OWNER && owners <= 1 && demoting_away_from_owner {
        return OrganizationError::LastOwner;
    }
    if let Some(role) = new_role {
        // `change_member_role`'s UPDATE carries no floor - the strict inequality
        // against BOTH the held and the target role is its whole fence - so this
        // classifier must not claim one.
        return classify_seat_refusal(tx, organization_id, principal, role, None).await;
    }
    OrganizationError::Insufficient(format!(
        "removing a member holding {current_role:?} needs a strictly higher rank and at least \
         equal billing authority. To give up your OWN seat, call \
         DELETE /api/organizations/{{organization_id}}/membership, which needs no rank at all"
    ))
}

/// Why the departure DELETE matched nothing.
///
/// It is NOT [`classify_member_refusal`], and the difference is the point:
/// that function's fallback sentence explains a RANK comparison, and the
/// departure statement has no rank comparison to fail. Only two clauses can
/// match nothing here - no seat, or the last owner's seat - so those are the
/// only two answers this can give. Reusing the other classifier would have
/// meant a departure occasionally reporting an authority failure that did not
/// happen.
async fn classify_departure_refusal<C: GenericClient + Sync>(
    tx: &C,
    organization_id: &str,
    principal: Uuid,
) -> OrganizationError {
    let rows = tx
        .query(
            "SELECT m.role FROM zeroship.organization_members m \
              WHERE m.organization_id = $1 AND m.user_id = $2",
            &[&organization_id, &principal],
        )
        .await;
    match rows {
        Ok(rows) if rows.is_empty() => OrganizationError::MemberNotFound,
        // A seat exists and the DELETE still matched nothing, so the only
        // surviving clause is the last-owner count.
        Ok(_) => OrganizationError::LastOwner,
        Err(err) => db_error(&err, "classify departure refusal"),
    }
}

/// Why the dissolve UPDATE matched nothing.
///
/// The dissolved case cannot reach here - [`lock_organization`] refuses it
/// before the statement runs - so the two live answers are "you are not an
/// owner" and "projects remain". Projects are reported FIRST and with a count,
/// because that is the refusal a legitimate owner meets and it is the one with
/// a next step.
async fn classify_dissolve_refusal<C: GenericClient + Sync>(
    tx: &C,
    organization_id: &str,
    principal: Uuid,
) -> OrganizationError {
    let sql = format!(
        "SELECT (SELECT COUNT(*) FROM zeroship.projects p \
                  WHERE p.organization_id = $1)::bigint AS projects, \
                (SELECT COUNT(*) FROM {seat} \
                  WHERE actor_role.role = $3)::bigint AS actor_owns",
        seat = actor_seat("$1", "$2"),
    );
    let rows = match tx
        .query(&sql, &[&organization_id, &principal, &ROLE_OWNER])
        .await
    {
        Ok(rows) => rows,
        Err(err) => return db_error(&err, "classify dissolve refusal"),
    };
    let Some(row) = rows.first() else {
        return OrganizationError::Db;
    };
    let projects: i64 = row.get("projects");
    let actor_owns: i64 = row.get("actor_owns");
    if projects > 0 {
        return OrganizationError::OrganizationHasProjects(projects);
    }
    if actor_owns == 0 {
        return OrganizationError::Insufficient(
            "closing an organization is reserved to its owners".to_string(),
        );
    }
    OrganizationError::Db
}

/// Why the project DELETE matched nothing: apps remain, or the actor is below
/// admin. Apps first, for the same reason projects come first above.
async fn classify_project_deletion_refusal<C: GenericClient + Sync>(
    tx: &C,
    organization_id: &str,
    principal: Uuid,
    project_id: &str,
) -> OrganizationError {
    let sql = format!(
        "SELECT (SELECT COUNT(*) FROM zeroship.apps a \
                  WHERE a.project_id = $3)::bigint AS apps, \
                (SELECT COUNT(*) FROM {seat} \
                  WHERE actor_role.rank >= {admin})::bigint AS actor_admin",
        seat = actor_seat("$1", "$2"),
        admin = ladder_rank("$4"),
    );
    let rows = match tx
        .query(
            &sql,
            &[&organization_id, &principal, &project_id, &ROLE_ADMIN],
        )
        .await
    {
        Ok(rows) => rows,
        Err(err) => return db_error(&err, "classify project deletion refusal"),
    };
    let Some(row) = rows.first() else {
        return OrganizationError::Db;
    };
    let apps: i64 = row.get("apps");
    let actor_admin: i64 = row.get("actor_admin");
    if apps > 0 {
        return OrganizationError::ProjectHasApps(apps);
    }
    if actor_admin == 0 {
        return OrganizationError::Insufficient(
            "deleting a project needs admin authority in the organization".to_string(),
        );
    }
    OrganizationError::Db
}

/// Why the app UPDATE matched nothing: the app was already deleted, it is
/// still live, or the actor is below admin.
///
/// State before authority, for the reason the two classifiers above give: the
/// caller is told the step that comes next, and "archive it" is a step they can
/// take while "you are not an admin" is not.
async fn classify_app_deletion_refusal<C: GenericClient + Sync>(
    tx: &C,
    organization_id: &str,
    principal: Uuid,
    app_id: Uuid,
) -> OrganizationError {
    let sql = format!(
        "SELECT (SELECT COUNT(*) FROM zeroship.apps a \
                  WHERE a.id = $3 AND a.archived_at IS NULL)::bigint AS live, \
                (SELECT COUNT(*) FROM zeroship.apps a \
                  WHERE a.id = $3 AND a.deleted_at IS NOT NULL)::bigint AS already_deleted, \
                (SELECT COUNT(*) FROM {seat} \
                  WHERE actor_role.rank >= {admin})::bigint AS actor_admin",
        seat = actor_seat("$1", "$2"),
        admin = ladder_rank("$4"),
    );
    let rows = match tx
        .query(&sql, &[&organization_id, &principal, &app_id, &ROLE_ADMIN])
        .await
    {
        Ok(rows) => rows,
        Err(err) => return db_error(&err, "classify app deletion refusal"),
    };
    let Some(row) = rows.first() else {
        return OrganizationError::Db;
    };
    if row.get::<_, i64>("already_deleted") > 0 {
        return OrganizationError::AppNotFound;
    }
    if row.get::<_, i64>("live") > 0 {
        return OrganizationError::AppNotArchived;
    }
    if row.get::<_, i64>("actor_admin") == 0 {
        return OrganizationError::Insufficient(
            "deleting an app needs admin authority in the organization".to_string(),
        );
    }
    OrganizationError::Db
}

async fn classify_transfer_refusal<C: GenericClient + Sync>(
    tx: &C,
    organization_id: &str,
    principal: Uuid,
    target: Uuid,
) -> OrganizationError {
    let rows = tx
        .query(
            "SELECT \
               (SELECT COUNT(*) FROM zeroship.organization_members \
                 WHERE organization_id = $1 AND user_id = $2 AND role = $4)::bigint AS actor_owns, \
               (SELECT COUNT(*) FROM zeroship.organization_members \
                 WHERE organization_id = $1 AND user_id = $3)::bigint AS target_seated",
            &[&organization_id, &principal, &target, &ROLE_OWNER],
        )
        .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(err) => return db_error(&err, "classify transfer refusal"),
    };
    let Some(row) = rows.first() else {
        return OrganizationError::Db;
    };
    let actor_owns: i64 = row.get("actor_owns");
    let target_seated: i64 = row.get("target_seated");
    if actor_owns == 0 {
        return OrganizationError::Insufficient("only an owner can transfer ownership".to_string());
    }
    if target_seated == 0 {
        return OrganizationError::MemberNotFound;
    }
    OrganizationError::Db
}

async fn classify_project_seat_refusal<C: GenericClient + Sync>(
    tx: &C,
    organization_id: &str,
    principal: Uuid,
    target: Uuid,
    role: &str,
) -> OrganizationError {
    let rows = tx
        .query(
            "SELECT COUNT(*)::bigint AS seated FROM zeroship.organization_members \
              WHERE organization_id = $1 AND user_id = $2",
            &[&organization_id, &target],
        )
        .await;
    match rows {
        Ok(rows)
            if rows
                .first()
                .is_some_and(|row| row.get::<_, i64>("seated") == 0) =>
        {
            OrganizationError::Invalid(
                "that user holds no seat in this project's organization; add them to the \
                 organization first"
                    .to_string(),
            )
        }
        // The one caller is `add_project_member`, whose INSERT carries the
        // `admin` floor.
        Ok(_) => classify_seat_refusal(tx, organization_id, principal, role, Some(ROLE_ADMIN)).await,
        Err(err) => db_error(&err, "classify project seat refusal"),
    }
}

// ---------------------------------------------------------------------------
// Read-back helpers
// ---------------------------------------------------------------------------

async fn read_member<C: GenericClient + Sync>(
    pg: &C,
    organization_id: &str,
    user_id: Uuid,
    added_at: DateTime<Utc>,
) -> Result<MemberRecord, OrganizationError> {
    let rows = pg
        .query(
            "SELECT m.organization_id, m.user_id, u.email::text AS email, u.name, m.role, \
                    r.rank, r.billing_rank \
               FROM zeroship.organization_members m \
               JOIN zeroship.users u ON u.id = m.user_id \
               JOIN zeroship.organization_roles r ON r.role = m.role \
              WHERE m.organization_id = $1 AND m.user_id = $2",
            &[&organization_id, &user_id],
        )
        .await
        .map_err(|err| db_error(&err, "read member"))?;
    let row = rows.first().ok_or(OrganizationError::MemberNotFound)?;
    Ok(MemberRecord {
        organization_id: row.get("organization_id"),
        user_id: row.get("user_id"),
        email: row.get("email"),
        name: row.get("name"),
        role: row.get("role"),
        rank: row.get("rank"),
        billing_rank: row.get("billing_rank"),
        added_at,
    })
}

async fn member_email<C: GenericClient + Sync>(
    pg: &C,
    user_id: Uuid,
) -> Result<String, OrganizationError> {
    let rows = pg
        .query(
            "SELECT email::text AS email FROM zeroship.users WHERE id = $1",
            &[&user_id],
        )
        .await
        .map_err(|err| db_error(&err, "read member email"))?;
    rows.first()
        .map(|row| row.get("email"))
        .ok_or(OrganizationError::UserNotFound)
}

/// Write one authority-change row, INSIDE the caller's transaction.
///
/// Best-effort in the same sense [`crate::audit::log`] is - a failed insert
/// warns rather than failing the mutation - but its placement is the opposite:
/// this row lives or dies with the effect it describes, because an effect that
/// rolled back did not happen.
async fn audit_authority_change<C: GenericClient + Sync>(
    tx: &C,
    actor: Uuid,
    action: AuditAction,
    organization_id: &str,
    source_ip: Option<&str>,
    detail: &serde_json::Value,
) {
    audit::log_in_tx(
        tx,
        AuditEntry {
            app_id: None,
            organization_id: None,
            actor_user_id: Some(actor),
            action,
            resource: Some(organization_id),
            source_ip,
        },
        detail,
    )
    .await;
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------
//
// EVERY handler calls `authz.require(...)` BEFORE opening a transaction. That
// ordering is the module header's decision-row rule in code: `enforce` writes
// the `authz_decisions` row on the shared client, so a refusal's record commits
// independently of the effect it refused.

fn bad_id(kind: &str) -> web::HttpResponse {
    web::HttpResponse::BadRequest().json(&json!({"error": format!("bad {kind}")}))
}

/// Reject a malformed typed id before it reaches Cedar or the database.
///
/// `Resource::validate_ids` would also refuse it, but with a message about the
/// resource alphabet rather than about the id the caller typed - and the parse
/// is what proves the string is one of OUR ids rather than merely alphabet-safe.
fn parse_organization_id(raw: &str) -> Result<String, web::HttpResponse> {
    OrganizationId::parse(raw)
        .map(|id| id.as_str().to_string())
        .map_err(|_| bad_id("organization_id"))
}

fn parse_project_path(raw: &str) -> Result<String, web::HttpResponse> {
    ProjectId::parse(raw)
        .map(|id| id.as_str().to_string())
        .map_err(|_| bad_id("project_id"))
}

fn parse_invite_path(raw: &str) -> Result<String, web::HttpResponse> {
    InviteId::parse(raw)
        .map(|id| id.as_str().to_string())
        .map_err(|_| bad_id("invite_id"))
}

pub async fn create(
    req: web::HttpRequest,
    authz: AuthzGuard,
    body: Json<CreateOrganizationBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    if let Err(resp) = authz
        .require(AuthzAction::OrganizationCreate, Resource::Any, &state)
        .await
    {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match create_organization(&state.registry, authz.principal_id, &body, ip.as_deref()).await {
        Ok(record) => web::HttpResponse::Created().json(&record),
        Err(e) => e.into_response(),
    }
}

pub async fn list(
    req: web::HttpRequest,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    if let Err(resp) = authz
        .require(AuthzAction::OrganizationRead, Resource::Any, &state)
        .await
    {
        return resp;
    }
    match list_organizations(state.control_pg.as_ref(), authz.principal_id).await {
        Ok(records) => web::HttpResponse::Ok().json(&json!({ "organizations": records })),
        Err(e) => e.into_response(),
    }
}

pub async fn show(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let id = match parse_organization_id(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::OrganizationRead,
            Resource::Organization { id: id.clone() },
            &state,
        )
        .await
    {
        return resp;
    }
    match get_organization(state.control_pg.as_ref(), &id).await {
        Ok(record) => web::HttpResponse::Ok().json(&record),
        Err(e) => e.into_response(),
    }
}

pub async fn update(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: Json<UpdateOrganizationBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let id = match parse_organization_id(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::OrganizationWrite,
            Resource::Organization { id: id.clone() },
            &state,
        )
        .await
    {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match update_organization(
        &state.registry,
        authz.principal_id,
        &id,
        &body,
        ip.as_deref(),
    )
    .await
    {
        Ok(record) => web::HttpResponse::Ok().json(&record),
        Err(e) => e.into_response(),
    }
}

pub async fn members(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let id = match parse_organization_id(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::OrganizationMembersRead,
            Resource::Organization { id: id.clone() },
            &state,
        )
        .await
    {
        return resp;
    }
    match list_members(state.control_pg.as_ref(), &id).await {
        Ok(records) => web::HttpResponse::Ok().json(&json!({ "members": records })),
        Err(e) => e.into_response(),
    }
}

/// Seating a member.
///
/// Two Cedar calls when the requested role is `admin` or `owner`: the broad
/// `organization:members:write` AND the reserved `organization:admin`. See
/// [`seats_a_privileged_role`] - it is the consent copy's promise, not the
/// authority fence.
async fn require_seat_authority(
    authz: &AuthzGuard,
    state: &AppState,
    organization_id: &str,
    role: &str,
) -> Result<(), web::HttpResponse> {
    authz
        .require(
            AuthzAction::OrganizationMembersWrite,
            Resource::Organization {
                id: organization_id.to_string(),
            },
            state,
        )
        .await?;
    if seats_a_privileged_role(role) {
        authz
            .require(
                AuthzAction::OrganizationAdmin,
                Resource::Organization {
                    id: organization_id.to_string(),
                },
                state,
            )
            .await?;
    }
    Ok(())
}

pub async fn add_member_handler(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: Json<AddMemberBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let id = match parse_organization_id(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = require_seat_authority(&authz, &state, &id, &body.role).await {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match add_member(
        &state.registry,
        authz.principal_id,
        &id,
        &body,
        ip.as_deref(),
    )
    .await
    {
        Ok(record) => web::HttpResponse::Created().json(&record),
        Err(e) => e.into_response(),
    }
}

pub async fn change_role_handler(
    req: web::HttpRequest,
    path: Path<(String, String)>,
    authz: AuthzGuard,
    body: Json<ChangeRoleBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let (raw_organization, raw_user) = path.into_inner();
    let id = match parse_organization_id(&raw_organization) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let Ok(user_id) = raw_user.parse::<Uuid>() else {
        return bad_id("user_id");
    };
    if let Err(resp) = require_seat_authority(&authz, &state, &id, &body.role).await {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match change_member_role(
        &state.registry,
        authz.principal_id,
        &id,
        user_id,
        &body,
        ip.as_deref(),
    )
    .await
    {
        Ok(record) => web::HttpResponse::Ok().json(&record),
        Err(e) => e.into_response(),
    }
}

pub async fn remove_member_handler(
    req: web::HttpRequest,
    path: Path<(String, String)>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let (raw_organization, raw_user) = path.into_inner();
    let id = match parse_organization_id(&raw_organization) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let Ok(user_id) = raw_user.parse::<Uuid>() else {
        return bad_id("user_id");
    };
    // Removal cannot be pre-narrowed by target role - the caller does not name
    // one - so the broad members:write scope gates it and the rank predicate in
    // the DELETE decides. An admin cannot remove an owner because `30 > 40` is
    // false, not because a scope said so.
    if let Err(resp) = authz
        .require(
            AuthzAction::OrganizationMembersWrite,
            Resource::Organization { id: id.clone() },
            &state,
        )
        .await
    {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match remove_member(
        &state.registry,
        authz.principal_id,
        &id,
        user_id,
        ip.as_deref(),
    )
    .await
    {
        Ok(()) => web::HttpResponse::NoContent().finish(),
        Err(e) => e.into_response(),
    }
}

/// Give up your own seat.
///
/// The route carries NO user id - not in the path, not in the body - so the
/// only seat it can reach is the bearer's. That is what makes the rank
/// carve-out safe: it is not a check that could be got round, it is an argument
/// that does not exist.
///
/// Gated on `organization:members:leave`, which is banded at viewer rank and
/// above. `organization:members:write` would have been the wrong gate (a viewer
/// never holds it, and a viewer is the member most likely to want out) and
/// `organization:read` would have been worse (a read-only consent could then
/// delete its holder's seat).
pub async fn leave_handler(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let id = match parse_organization_id(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::OrganizationMembersLeave,
            Resource::Organization { id: id.clone() },
            &state,
        )
        .await
    {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match leave_organization(&state.registry, authz.principal_id, &id, ip.as_deref()).await {
        Ok(()) => web::HttpResponse::NoContent().finish(),
        Err(e) => e.into_response(),
    }
}

/// Close an organization.
///
/// Gated on `organization:admin`, the action no band but the owner's permits -
/// the same gate as [`transfer_handler`], because reshaping who owns an
/// organization and ending it are the two things only an owner may do.
///
/// It answers `200` with the closed record rather than `204`, because
/// `dissolved_at` is the whole result and a caller that had to re-read for it
/// would be reading a row that now refuses everything else.
pub async fn dissolve_handler(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let id = match parse_organization_id(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::OrganizationAdmin,
            Resource::Organization { id: id.clone() },
            &state,
        )
        .await
    {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match dissolve_organization(
        &state.registry,
        authz.principal_id,
        &id,
        LocalInvoicing::of(&state.billing_stack),
        ip.as_deref(),
    )
    .await
    {
        Ok(record) => web::HttpResponse::Ok().json(&record),
        Err(e) => e.into_response(),
    }
}

pub async fn transfer_handler(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: Json<TransferOwnershipBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let id = match parse_organization_id(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::OrganizationAdmin,
            Resource::Organization { id: id.clone() },
            &state,
        )
        .await
    {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match transfer_ownership(
        &state.registry,
        authz.principal_id,
        &id,
        &body,
        ip.as_deref(),
    )
    .await
    {
        Ok(()) => web::HttpResponse::NoContent().finish(),
        Err(e) => e.into_response(),
    }
}

pub async fn invites(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let id = match parse_organization_id(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::OrganizationMembersRead,
            Resource::Organization { id: id.clone() },
            &state,
        )
        .await
    {
        return resp;
    }
    match list_invites(state.control_pg.as_ref(), &id).await {
        Ok(records) => web::HttpResponse::Ok().json(&json!({ "invites": records })),
        Err(e) => e.into_response(),
    }
}

pub async fn create_invite_handler(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: Json<CreateInviteBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let id = match parse_organization_id(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = require_seat_authority(&authz, &state, &id, &body.role).await {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match create_and_deliver_invite(
        &state.registry,
        state.control_pg.as_ref(),
        state.mailer.as_ref(),
        authz.principal_id,
        &id,
        &body,
        ip.as_deref(),
    )
    .await
    {
        Ok(created) => web::HttpResponse::Created().json(&created),
        Err(e) => e.into_response(),
    }
}

pub async fn revoke_invite_handler(
    req: web::HttpRequest,
    path: Path<(String, String)>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let (raw_organization, raw_invite) = path.into_inner();
    let id = match parse_organization_id(&raw_organization) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let invite_id = match parse_invite_path(&raw_invite) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::OrganizationMembersWrite,
            Resource::Organization { id: id.clone() },
            &state,
        )
        .await
    {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match revoke_invite(
        &state.registry,
        authz.principal_id,
        &id,
        &invite_id,
        ip.as_deref(),
    )
    .await
    {
        Ok(()) => web::HttpResponse::NoContent().finish(),
        Err(e) => e.into_response(),
    }
}

/// Redeem an invitation.
///
/// The gate is `organization:read` at `Resource::Any`, and the choice needs
/// stating because it looks weak. The TOKEN is the capability here: the
/// redeemer holds no seat, so there is no rank for a band to compare and no
/// organization-scoped action they could satisfy. What Cedar still decides is
/// whether the BEARER's own token admits organization business at all - a token
/// narrowed to `apps:read` cannot join its holder to an organization - and that
/// is the whole of what it can honestly decide here.
pub async fn redeem_handler(
    req: web::HttpRequest,
    authz: AuthzGuard,
    body: Json<RedeemInviteBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    if let Err(resp) = authz
        .require(AuthzAction::OrganizationRead, Resource::Any, &state)
        .await
    {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match redeem_invite(
        &state.registry,
        authz.principal_id,
        &body.token,
        ip.as_deref(),
    )
    .await
    {
        Ok(record) => web::HttpResponse::Ok().json(&record),
        Err(e) => e.into_response(),
    }
}

pub async fn projects(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let id = match parse_organization_id(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::OrganizationRead,
            Resource::Organization { id: id.clone() },
            &state,
        )
        .await
    {
        return resp;
    }
    match list_projects(state.control_pg.as_ref(), &id, authz.principal_id).await {
        Ok(records) => web::HttpResponse::Ok().json(&json!({ "projects": records })),
        Err(e) => e.into_response(),
    }
}

pub async fn create_project_handler(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: Json<CreateProjectBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let id = match parse_organization_id(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::ProjectCreate,
            Resource::Organization { id: id.clone() },
            &state,
        )
        .await
    {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match create_project(
        &state.registry,
        authz.principal_id,
        &id,
        &body,
        ip.as_deref(),
    )
    .await
    {
        Ok(record) => web::HttpResponse::Created().json(&record),
        Err(e) => e.into_response(),
    }
}

pub async fn show_project(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let id = match parse_project_path(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::ProjectRead,
            Resource::Project { id: id.clone() },
            &state,
        )
        .await
    {
        return resp;
    }
    match get_project(state.control_pg.as_ref(), &id).await {
        Ok(record) => web::HttpResponse::Ok().json(&record),
        Err(e) => e.into_response(),
    }
}

pub async fn update_project_handler(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: Json<UpdateProjectBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let id = match parse_project_path(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::ProjectWrite,
            Resource::Project { id: id.clone() },
            &state,
        )
        .await
    {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match update_project(
        &state.registry,
        authz.principal_id,
        &id,
        &body,
        ip.as_deref(),
    )
    .await
    {
        Ok(record) => web::HttpResponse::Ok().json(&record),
        Err(e) => e.into_response(),
    }
}

/// Delete a project.
///
/// The same `project:write` gate as the rename, and deliberately so: the
/// consent copy for that scope is "Rename and delete projects", so a separate
/// action would either need its own consent line or would be authority a human
/// approved under a different sentence.
pub async fn delete_project_handler(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let id = match parse_project_path(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::ProjectWrite,
            Resource::Project { id: id.clone() },
            &state,
        )
        .await
    {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match delete_project(&state.registry, authz.principal_id, &id, ip.as_deref()).await {
        Ok(()) => web::HttpResponse::NoContent().finish(),
        Err(e) => e.into_response(),
    }
}

pub async fn project_members(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let id = match parse_project_path(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::ProjectMembersRead,
            Resource::Project { id: id.clone() },
            &state,
        )
        .await
    {
        return resp;
    }
    match list_project_members(state.control_pg.as_ref(), &id).await {
        Ok(records) => web::HttpResponse::Ok().json(&json!({ "members": records })),
        Err(e) => e.into_response(),
    }
}

pub async fn add_project_member_handler(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: Json<AddProjectMemberBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let id = match parse_project_path(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::ProjectMembersWrite,
            Resource::Project { id: id.clone() },
            &state,
        )
        .await
    {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match add_project_member(
        &state.registry,
        authz.principal_id,
        &id,
        &body,
        ip.as_deref(),
    )
    .await
    {
        Ok(record) => web::HttpResponse::Created().json(&record),
        Err(e) => e.into_response(),
    }
}

pub async fn change_project_role_handler(
    req: web::HttpRequest,
    path: Path<(String, String)>,
    authz: AuthzGuard,
    body: Json<ChangeRoleBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let (raw_project, raw_user) = path.into_inner();
    let id = match parse_project_path(&raw_project) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let Ok(user_id) = raw_user.parse::<Uuid>() else {
        return bad_id("user_id");
    };
    // No `require_seat_authority` here, and the asymmetry with the
    // ORGANIZATION role change is deliberate. That one reserves seating an
    // owner or an admin to `organization:admin`, because those roles carry
    // authority over the organization. A project role carries none: the
    // effective rank is min(organization rank, project rank), so seating a
    // developer as project owner leaves them at 20. There is nothing here for
    // the consent fence to protect.
    if let Err(resp) = authz
        .require(
            AuthzAction::ProjectMembersWrite,
            Resource::Project { id: id.clone() },
            &state,
        )
        .await
    {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match change_project_member_role(
        &state.registry,
        authz.principal_id,
        &id,
        user_id,
        &body,
        ip.as_deref(),
    )
    .await
    {
        Ok(record) => web::HttpResponse::Ok().json(&record),
        Err(e) => e.into_response(),
    }
}

pub async fn remove_project_member_handler(
    req: web::HttpRequest,
    path: Path<(String, String)>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let (raw_project, raw_user) = path.into_inner();
    let id = match parse_project_path(&raw_project) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let Ok(user_id) = raw_user.parse::<Uuid>() else {
        return bad_id("user_id");
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::ProjectMembersWrite,
            Resource::Project { id: id.clone() },
            &state,
        )
        .await
    {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match remove_project_member(
        &state.registry,
        authz.principal_id,
        &id,
        user_id,
        ip.as_deref(),
    )
    .await
    {
        Ok(()) => web::HttpResponse::NoContent().finish(),
        Err(e) => e.into_response(),
    }
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    let payload = || web::types::PayloadConfig::new(ORGANIZATION_PAYLOAD_BYTES);
    cfg.service(
        web::resource("/api/organizations")
            .state(payload())
            .route(web::get().to(list))
            .route(web::post().to(create)),
    )
    .service(
        web::resource("/api/organizations/{organization_id}")
            .state(payload())
            .route(web::get().to(show))
            .route(web::patch().to(update))
            .route(web::delete().to(dissolve_handler)),
    )
    .service(
        web::resource("/api/organizations/{organization_id}/members")
            .state(payload())
            .route(web::get().to(members))
            .route(web::post().to(add_member_handler)),
    )
    // Registered BEFORE `/members/{user_id}` so the literal wins the match.
    // `membership` is not a uuid, so the parameterised route would answer it
    // with `bad user_id` rather than 404 - a refusal naming the wrong thing.
    .service(
        web::resource("/api/organizations/{organization_id}/membership")
            .route(web::delete().to(leave_handler)),
    )
    .service(
        web::resource("/api/organizations/{organization_id}/members/{user_id}")
            .state(payload())
            .route(web::patch().to(change_role_handler))
            .route(web::delete().to(remove_member_handler)),
    )
    .service(
        web::resource("/api/organizations/{organization_id}/transfer")
            .state(payload())
            .route(web::post().to(transfer_handler)),
    )
    .service(
        web::resource("/api/organizations/{organization_id}/invites")
            .state(payload())
            .route(web::get().to(invites))
            .route(web::post().to(create_invite_handler)),
    )
    .service(
        web::resource("/api/organizations/{organization_id}/invites/{invite_id}")
            .route(web::delete().to(revoke_invite_handler)),
    )
    .service(
        web::resource("/api/organization-invites/redeem")
            .state(payload())
            .route(web::post().to(redeem_handler)),
    )
    .service(
        web::resource("/api/organizations/{organization_id}/projects")
            .state(payload())
            .route(web::get().to(projects))
            .route(web::post().to(create_project_handler)),
    )
    .service(
        web::resource("/api/projects/{project_id}")
            .state(payload())
            .route(web::get().to(show_project))
            .route(web::patch().to(update_project_handler))
            .route(web::delete().to(delete_project_handler)),
    )
    .service(
        web::resource("/api/projects/{project_id}/members")
            .state(payload())
            .route(web::get().to(project_members))
            .route(web::post().to(add_project_member_handler)),
    )
    .service(
        web::resource("/api/projects/{project_id}/members/{user_id}")
            .state(payload())
            .route(web::patch().to(change_project_role_handler))
            .route(web::delete().to(remove_project_member_handler)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slug_is_derived_from_a_name_and_refuses_to_invent_one() {
        assert_eq!(slug_from_name("Acme Rockets"), Some("acme-rockets".into()));
        assert_eq!(
            slug_from_name("  Acme   Rockets  "),
            Some("acme-rockets".into())
        );
        assert_eq!(slug_from_name("Acme, Inc."), Some("acme-inc".into()));
        assert_eq!(slug_from_name("2026 Ideas"), Some("2026-ideas".into()));
        // A name with no ASCII alphanumerics yields NOTHING rather than a
        // fabricated slug. The control below is what makes that a result: an
        // implementation that returned None for everything would pass the line
        // above on its own.
        assert_eq!(slug_from_name("!!!"), None);
        assert_eq!(slug_from_name(""), None);
        // Every derived slug must satisfy the grammar the schema enforces,
        // including the leading-character rule that a trim could violate.
        for name in ["-Acme-", "...Acme...", "9lives", "a"] {
            let slug = slug_from_name(name).expect("derivable");
            validate_slug(&slug).unwrap_or_else(|_| panic!("{name:?} derived invalid {slug:?}"));
        }
    }

    #[test]
    fn the_slug_validator_matches_the_schema_grammar() {
        for good in ["acme", "acme-rockets", "9lives", "a", "0"] {
            assert!(validate_slug(good).is_ok(), "{good:?} must be accepted");
        }
        // Each of these differs from an accepted value in exactly one way.
        for bad in ["-acme", "Acme", "acme_rockets", "acme rockets", "", "acmé"] {
            assert!(validate_slug(bad).is_err(), "{bad:?} must be refused");
        }
    }

    /// The privileged-role fence is a CLOSED set over the ladder's vocabulary.
    /// It must name admin and owner and nothing else - listing `billing` here
    /// would let its billing_rank 20 be granted only by an owner, which is not
    /// what the ladder says, and omitting `admin` would let a
    /// members:write-only token mint one.
    #[test]
    fn only_admin_and_owner_are_consent_reserved() {
        assert!(seats_a_privileged_role(ROLE_OWNER));
        assert!(seats_a_privileged_role(ROLE_ADMIN));
        assert!(!seats_a_privileged_role(ROLE_DEVELOPER));
        assert!(!seats_a_privileged_role(ROLE_VIEWER));
        assert!(!seats_a_privileged_role("billing"));
    }

    /// The narrowing SQL must not be able to reach the LEAST-ignores-null trap.
    /// This is a shape assertion on the generated text, and it is deliberately
    /// crude: the BEHAVIOURAL proof is
    /// `organization_project_rank_matches_the_authz_narrowing` in
    /// `tests/organizations_test.rs`, which runs both implementations against a
    /// live PostgreSQL. What this catches is an edit that removes the explicit
    /// null arm while the live test is not being run.
    #[test]
    fn the_narrowing_sql_handles_a_missing_project_row_explicitly() {
        let sql = effective_project_rank_sql("org.rank", "proj.rank", "$3");
        assert!(
            sql.contains("WHEN proj.rank IS NULL THEN 0"),
            "a member with no project row must resolve to 0, not to LEAST(org, NULL): {sql}"
        );
        assert!(
            sql.contains("WHEN org.rank IS NULL THEN 0"),
            "a non-member must resolve to 0: {sql}"
        );
        assert!(
            !sql.contains("COALESCE(LEAST"),
            "COALESCE(LEAST(a, NULL), 0) is `a` in PostgreSQL and silently widens: {sql}"
        );
    }

    /// An invite token must be high-entropy, url-safe and NOT the digest that
    /// gets stored. The last clause is the one worth pinning: a refactor that
    /// stored the token itself would still pass an entropy assertion.
    #[test]
    fn an_invite_token_is_url_safe_and_is_not_what_gets_stored() {
        let token = mint_invite_token();
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "token must be base64url without padding: {token}"
        );
        assert!(token.len() >= 43, "32 CSPRNG bytes must survive encoding");
        let digest = token_digest(&token);
        assert_eq!(digest.len(), 32, "the stored value is a SHA-256 digest");
        assert_ne!(
            digest,
            token.as_bytes().to_vec(),
            "the stored value must not be the token"
        );
        // Two mints must differ. Without this an implementation returning a
        // constant would satisfy every assertion above.
        assert_ne!(mint_invite_token(), mint_invite_token());
    }

    /// The personal slug must be globally unique by construction and legal
    /// under the schema grammar, because nothing retries it.
    #[test]
    fn a_personal_slug_is_derived_from_the_owner_and_is_always_legal() {
        let one = Uuid::new_v4();
        let two = Uuid::new_v4();
        assert_ne!(personal_slug(one), personal_slug(two));
        validate_slug(&personal_slug(one)).expect("the personal slug must satisfy the grammar");
        // The uuid must be rendered WITHOUT hyphens-as-separators problems: a
        // hyphenated uuid is still legal, but the simple form is what the
        // function promises and a change to `to_string()` would lengthen every
        // slug silently.
        assert!(
            personal_slug(one).starts_with("personal-"),
            "a personal slug is recognisable as one"
        );
        assert!(
            !personal_slug(one)["personal-".len()..].contains('-'),
            "the owner id is rendered in simple form"
        );
    }

    #[test]
    fn the_app_owner_join_reaches_the_organization_through_the_project() {
        // `zeroship.app_members` is deleted. The one path from an app to a
        // human runs through its project; a join naming the old table, or
        // naming `apps.organization_id` (which does not exist), is the failure
        // this pins.
        for sql in [app_owner_lateral(), app_owner_map()] {
            assert!(!sql.contains("app_members"), "{sql}");
            assert!(!sql.contains("a.organization_id"), "{sql}");
            assert!(sql.contains("owner_project.organization_id"), "{sql}");
            assert!(sql.contains("owner_member.role = 'owner'"), "{sql}");
        }
        assert!(app_owner_lateral().contains("a.project_id"));
        assert!(app_owner_map().contains("owner_app.project_id"));
    }

    /// The two shapes must pick the SAME owner. They differ in how they
    /// collapse the fan-out - `LIMIT 1` against `DISTINCT ON` - and that is
    /// exactly where two copies would drift, so the tiebreak itself is one
    /// constant and both must be built from it.
    ///
    /// Three consumers used to carry three hand-matched copies of this ORDER
    /// BY, each with a comment asking the next editor to keep them matching.
    /// This is that comment, made checkable.
    #[test]
    fn both_owner_shapes_apply_the_same_tiebreak() {
        assert!(
            app_owner_lateral().contains(&format!("ORDER BY {APP_OWNER_ORDER}")),
            "the lateral must order by the shared rule alone"
        );
        assert!(
            app_owner_map().contains(&format!("ORDER BY owner_app.id, {APP_OWNER_ORDER}")),
            "the map's DISTINCT ON key must lead, then the shared rule"
        );
        assert!(
            app_owner_lateral().contains("LIMIT 1"),
            "several owners are permitted, so the lateral must collapse to one row"
        );
        assert!(
            app_owner_map().contains("DISTINCT ON (owner_app.id)"),
            "several owners are permitted, so the map must collapse to one row per app"
        );
    }
}
