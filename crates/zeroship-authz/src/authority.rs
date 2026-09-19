//! The caller's authority at ONE resource, re-derived on every request.
//!
//! There is one query per authorization, it is a JOIN rather than a lookup, and
//! its result is never stored: a membership row removed by a committed
//! transaction is invisible to the very next request in every process, with no
//! invalidation signal to build, publish or miss.
//!
//! # The two integers
//!
//! `zeroship.organization_roles` is a closed ladder carrying two independent
//! ranks. `rank` orders authority over apps and the organization; `billing_rank`
//! orders authority over money. They are deliberately not the same number:
//! `viewer` and `billing` share a rank, and `billing` outranks `admin` on money
//! while `admin` outranks it on everything else.
//!
//! # The narrowing
//!
//! A project GRANTS and CEILINGS; it never widens.
//!
//! - An organization member at admin rank or above holds authority over EVERY
//!   project in the organization, at their organization rank.
//! - A member below admin holds authority only where a `project_members` row
//!   exists, and their effective rank there is
//!   `min(organization rank, project rank)`.
//! - A database carries no rank of its own. Its authority IS its project's, so
//!   there is no per-database role to grant, and an app's binding to a database
//!   confers nothing on the app's own members: a binding is data access for
//!   running code, not authority over the database.
//! - `billing_rank` is organization-level entirely. There is no per-project
//!   invoice, so there is no per-project money authority, and `project_members`
//!   carries no billing dimension.
//!
//! The minimum is taken HERE, in [`effective_project_rank`], and nowhere else.
//! The schema cannot take it - a CHECK cannot subquery, and freezing the
//! organization rank into the project row would make a demotion FAIL whenever
//! the member held a higher project role, which is the opposite of narrowing.
//! Keeping it in one pure function is what lets it be exercised without a
//! database while the SQL beside it stays an ordinary join.
//!
//! # One rendering per id
//!
//! Every id this module binds is `text`, and every one of them is parsed before
//! it gets here: the app id by [`AppId`] and the database id by [`DatabaseId`]
//! on the [`Resource`] itself, the project and organization ids by
//! `Resource::validate_ids`. Nothing in this file chooses between two spellings
//! of the same id, and there is nothing left to refuse - which is why the app
//! and database arms of [`resolve`] are single calls rather than a parse
//! followed by one.
//!
//! That matters more here than it reads. A mis-rendered id does not fail this
//! query: `zeroship.apps` and `zeroship.databases` are each reached by a LEFT
//! JOIN, so an id in a rendering the column does not hold contributes NO ROW,
//! every rank comes back NULL, and the caller is told they hold no seat on a
//! resource they own. The types are what make that unreachable.
//!
//! **This binds `zeroship.apps.id` and `zeroship.databases.id` as `text`, and
//! that is a REQUIREMENT ON THOSE COLUMNS, not a description of them.** Neither
//! [`AppId`] nor [`DatabaseId`] exposes a route to a uuid - there is no
//! `uuid()` to call - so text against text is the only comparison these joins
//! can make, and a platform database whose `apps.id` or `databases.id` is not
//! `text` fails outright with a type error rather than resolving anything.
//! Loud, and on the first query.

use compio_postgres::Client;
use zeroship_id::{AppId, DatabaseId, UserId};

use crate::{AuthzError, Resource};

/// What the caller may do at the requested resource, as of this request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Authority {
    pub email_verified: bool,
    pub account_locked: bool,
    /// Authority over apps and the organization at the REQUESTED resource,
    /// already narrowed for a project- or app-scoped request. `0` means no live
    /// authority, which every band denies at its comparison.
    pub effective_rank: i32,
    /// Authority over money. Organization-level always; a project-scoped
    /// request carries the organization's value unchanged.
    pub billing_rank: i32,
}

impl Authority {
    /// The authority of a principal that reached no membership at all. Used for
    /// [`Resource::Any`], where no organization is named and therefore no
    /// organization authority can be satisfied.
    const fn unranked(email_verified: bool, account_locked: bool) -> Self {
        Self {
            email_verified,
            account_locked,
            effective_rank: 0,
            billing_rank: 0,
        }
    }
}

/// The organization role whose rank is the "covers every project" threshold.
/// The NAME is the stable thing; its rank is data, read from the ladder on
/// every resolve, so moving `admin` up or down in a migration moves this fence
/// with it and needs no code change.
const PROJECT_WIDE_ROLE: &str = "admin";

/// The narrowing rule, as a pure function.
///
/// `admin_rank` is `None` when the ladder carries no `admin` row at all. That
/// is treated as "nobody clears the project-wide threshold", which is the
/// NARROWER reading: an organization member then reaches only the projects they
/// hold an explicit row on. A missing ladder row must not widen anyone.
#[must_use]
pub fn effective_project_rank(
    organization_rank: Option<i32>,
    project_rank: Option<i32>,
    admin_rank: Option<i32>,
) -> i32 {
    let Some(organization_rank) = organization_rank else {
        // Not a member of the owning organization. A project_members row cannot
        // exist without one (the composite foreign key makes it unspellable),
        // so this is the whole answer.
        return 0;
    };
    if admin_rank.is_some_and(|admin_rank| organization_rank >= admin_rank) {
        return organization_rank;
    }
    project_rank.map_or(0, |project_rank| organization_rank.min(project_rank))
}

/// Resolve the caller's authority at `resource`.
///
/// # Errors
///
/// Returns [`AuthzError::Validation`] when the principal has no `zeroship.users`
/// row, and [`AuthzError::Db`] when the query itself fails. It never degrades a
/// failure into rank zero: a database error must not read as "an ordinary
/// member with no seat".
pub async fn resolve(
    pg: &Client,
    principal_id: &UserId,
    resource: &Resource,
) -> Result<Authority, AuthzError> {
    match resource {
        Resource::Any => resolve_unranked(pg, principal_id).await,
        Resource::Organization { id } => resolve_organization(pg, principal_id, id).await,
        Resource::Project { id } => {
            resolve_narrowed(pg, principal_id, Some(id.as_str()), None, None).await
        }
        Resource::App { id } => resolve_narrowed(pg, principal_id, None, Some(id), None).await,
        Resource::Database { id } => {
            resolve_narrowed(pg, principal_id, None, None, Some(id)).await
        }
    }
}

const USER_ATTRS: &str = "u.email_verified_at IS NOT NULL AS email_verified, \
     (u.locked_until IS NOT NULL AND u.locked_until > NOW()) AS account_locked";

async fn resolve_unranked(pg: &Client, principal_id: &UserId) -> Result<Authority, AuthzError> {
    let sql = format!("SELECT {USER_ATTRS} FROM zeroship.users u WHERE u.id = $1");
    let rows = pg
        .query(&sql, &[&principal_id.as_str()])
        .await
        .map_err(|err| AuthzError::Db(format!("resolve authority (unranked): {err}")))?;
    let row = principal_row(rows.first(), principal_id)?;
    Ok(Authority::unranked(
        row.get("email_verified"),
        row.get("account_locked"),
    ))
}

async fn resolve_organization(
    pg: &Client,
    principal_id: &UserId,
    organization_id: &str,
) -> Result<Authority, AuthzError> {
    // Organization scope takes NO minimum: this is the seat itself, not a
    // project view of it.
    let sql = format!(
        "SELECT {USER_ATTRS}, \
                organization_role.rank         AS organization_rank, \
                organization_role.billing_rank AS organization_billing_rank \
           FROM zeroship.users u \
           LEFT JOIN zeroship.organization_members m \
                  ON m.organization_id = $2 AND m.user_id = u.id \
           LEFT JOIN zeroship.organization_roles organization_role \
                  ON organization_role.role = m.role \
          WHERE u.id = $1"
    );
    let rows = pg
        .query(&sql, &[&principal_id.as_str(), &organization_id])
        .await
        .map_err(|err| AuthzError::Db(format!("resolve organization authority: {err}")))?;
    let row = principal_row(rows.first(), principal_id)?;
    let organization_rank: Option<i32> = row.get("organization_rank");
    let billing_rank: Option<i32> = row.get("organization_billing_rank");
    Ok(Authority {
        email_verified: row.get("email_verified"),
        account_locked: row.get("account_locked"),
        effective_rank: organization_rank.unwrap_or(0),
        billing_rank: billing_rank.unwrap_or(0),
    })
}

/// The project-, app- and database-scoped resolve. ONE query serves all three,
/// because an app and a database each reach an organization only through their
/// project: exactly one of `project_id` / `app_id` / `database_id` is supplied,
/// and the other arms' joins contribute nothing.
///
/// Duplicating this as three nearly identical statements is what would let the
/// narrowing drift between the paths, so it is written once.
///
/// **A database is project-owned, so there is no app in its chain.** It is
/// reached by `zeroship.databases.project_id` directly, not by walking a
/// binding: a binding is an app's access to a database, and an app holding one
/// confers no authority over the database on the app's own members.
///
/// Every bound id is `text` and every one arrives already parsed - the app id
/// as an [`AppId`], the database id as a [`DatabaseId`], the project id as a
/// validated [`Resource::Project`] id. There is no cast to get wrong and no
/// second rendering to pick between, which is what the deleted
/// `app_uuid_or_refuse` existed to arbitrate.
async fn resolve_narrowed(
    pg: &Client,
    principal_id: &UserId,
    project_id: Option<&str>,
    app_id: Option<&AppId>,
    database_id: Option<&DatabaseId>,
) -> Result<Authority, AuthzError> {
    let app_id = app_id.map(AppId::as_str);
    let database_id = database_id.map(DatabaseId::as_str);
    let sql = format!(
        "SELECT {USER_ATTRS}, \
                organization_role.rank         AS organization_rank, \
                organization_role.billing_rank AS organization_billing_rank, \
                project_role.rank              AS project_rank, \
                (SELECT rank FROM zeroship.organization_roles WHERE role = $4) AS admin_rank \
           FROM zeroship.users u \
           LEFT JOIN zeroship.apps a ON a.id = $3::text \
           LEFT JOIN zeroship.databases d ON d.id = $5::text \
           LEFT JOIN zeroship.projects p \
                  ON p.id = COALESCE($2::text, a.project_id, d.project_id) \
           LEFT JOIN zeroship.organization_members m \
                  ON m.organization_id = p.organization_id AND m.user_id = u.id \
           LEFT JOIN zeroship.organization_roles organization_role \
                  ON organization_role.role = m.role \
           LEFT JOIN zeroship.project_members pm \
                  ON pm.project_id = p.id AND pm.user_id = u.id \
           LEFT JOIN zeroship.organization_roles project_role \
                  ON project_role.role = pm.role \
          WHERE u.id = $1"
    );
    let rows = pg
        .query(
            &sql,
            &[
                &principal_id.as_str(),
                &project_id,
                &app_id,
                &PROJECT_WIDE_ROLE,
                &database_id,
            ],
        )
        .await
        .map_err(|err| AuthzError::Db(format!("resolve project authority: {err}")))?;
    let row = principal_row(rows.first(), principal_id)?;
    let organization_rank: Option<i32> = row.get("organization_rank");
    let project_rank: Option<i32> = row.get("project_rank");
    let admin_rank: Option<i32> = row.get("admin_rank");
    let billing_rank: Option<i32> = row.get("organization_billing_rank");
    Ok(Authority {
        email_verified: row.get("email_verified"),
        account_locked: row.get("account_locked"),
        effective_rank: effective_project_rank(organization_rank, project_rank, admin_rank),
        // Money authority never narrows: there is no per-project invoice.
        billing_rank: billing_rank.unwrap_or(0),
    })
}

fn principal_row<T>(row: Option<T>, principal_id: &UserId) -> Result<T, AuthzError> {
    row.ok_or_else(|| {
        AuthzError::Validation(format!("principal not found: {}", principal_id.as_str()))
    })
}

/// Every organization the principal holds a live membership row in.
///
/// Used only by the consent-delegation probe. It is a membership enumeration,
/// so it is bounded by how many organizations one human belongs to.
///
/// # Errors
///
/// Returns [`AuthzError::Db`] when the query fails, and
/// [`AuthzError::Validation`] when a stored id leaves the closed resource
/// alphabet.
pub async fn organization_resources(
    pg: &Client,
    principal_id: &UserId,
) -> Result<Vec<Resource>, AuthzError> {
    let rows = pg
        .query(
            "SELECT organization_id FROM zeroship.organization_members WHERE user_id = $1 \
             ORDER BY organization_id",
            &[&principal_id.as_str()],
        )
        .await
        .map_err(|err| AuthzError::Db(format!("load organization memberships: {err}")))?;

    rows.into_iter()
        .map(|row| {
            let resource = Resource::Organization {
                id: row.get::<_, String>("organization_id"),
            };
            resource
                .validate_ids()
                .map_err(|message| AuthzError::Validation(message.to_owned()))?;
            Ok(resource)
        })
        .collect()
}

/// The projects worth probing for "can this principal do X somewhere".
///
/// Exact and bounded, and the two arms are exactly the two halves of the
/// narrowing rule:
///
/// - Below admin, a nonzero effective rank requires an explicit
///   `project_members` row, so those rows ARE the answer.
/// - At admin and above the effective rank is the organization rank on EVERY
///   project in the organization, so any one project is representative and the
///   arm takes the first by id. An organization with no projects contributes
///   nothing, which the LATERAL naturally expresses.
///
/// The probe set is therefore bounded by the principal's own membership counts
/// rather than by organization size. Enumerating every project of every
/// organization would be the same answer at unbounded cost.
///
/// # Errors
///
/// Returns [`AuthzError::Db`] when the query fails, and
/// [`AuthzError::Validation`] when a stored id leaves the closed resource
/// alphabet.
pub async fn project_probe_resources(
    pg: &Client,
    principal_id: &UserId,
) -> Result<Vec<Resource>, AuthzError> {
    let rows = pg
        .query(
            "SELECT DISTINCT probe.project_id FROM ( \
                 SELECT pm.project_id \
                   FROM zeroship.project_members pm \
                  WHERE pm.user_id = $1 \
                 UNION ALL \
                 SELECT representative.id \
                   FROM zeroship.organization_members m \
                   JOIN zeroship.organization_roles r ON r.role = m.role \
                   JOIN LATERAL ( \
                       SELECT p.id FROM zeroship.projects p \
                        WHERE p.organization_id = m.organization_id \
                        ORDER BY p.id LIMIT 1 \
                   ) representative ON true \
                  WHERE m.user_id = $1 \
                    AND r.rank >= (SELECT rank FROM zeroship.organization_roles WHERE role = $2) \
             ) probe ORDER BY probe.project_id",
            &[&principal_id.as_str(), &PROJECT_WIDE_ROLE],
        )
        .await
        .map_err(|err| AuthzError::Db(format!("load project probe resources: {err}")))?;

    rows.into_iter()
        .map(|row| {
            let resource = Resource::Project {
                id: row.get::<_, String>("project_id"),
            };
            resource
                .validate_ids()
                .map_err(|message| AuthzError::Validation(message.to_owned()))?;
            Ok(resource)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::effective_project_rank;

    const VIEWER: i32 = 10;
    const DEVELOPER: i32 = 20;
    const ADMIN: i32 = 30;
    const OWNER: i32 = 40;

    /// The three cases of decision 5, each with the control that makes it a
    /// result rather than a coincidence.
    #[test]
    fn admin_and_above_reach_every_project_at_their_organization_rank() {
        // No project row at all: still full organization rank.
        assert_eq!(
            effective_project_rank(Some(ADMIN), None, Some(ADMIN)),
            ADMIN
        );
        assert_eq!(
            effective_project_rank(Some(OWNER), None, Some(ADMIN)),
            OWNER
        );
        // CONTROL, one variable changed: one rank below admin, same absent
        // project row, and the answer collapses to zero.
        assert_eq!(
            effective_project_rank(Some(DEVELOPER), None, Some(ADMIN)),
            0
        );
    }

    #[test]
    fn below_admin_requires_a_project_row_and_takes_the_minimum() {
        // The project ceilings the organization rank.
        assert_eq!(
            effective_project_rank(Some(DEVELOPER), Some(VIEWER), Some(ADMIN)),
            VIEWER
        );
        // The organization rank ceilings the project rank: a project cannot
        // widen. A developer seated as project owner is still a developer.
        assert_eq!(
            effective_project_rank(Some(DEVELOPER), Some(OWNER), Some(ADMIN)),
            DEVELOPER
        );
        // Equal ranks are their own minimum.
        assert_eq!(
            effective_project_rank(Some(VIEWER), Some(VIEWER), Some(ADMIN)),
            VIEWER
        );
    }

    #[test]
    fn a_non_member_of_the_organization_has_no_authority() {
        assert_eq!(effective_project_rank(None, Some(OWNER), Some(ADMIN)), 0);
        assert_eq!(effective_project_rank(None, None, Some(ADMIN)), 0);
    }

    /// A ladder with no `admin` row must NARROW, never widen. Pinning this
    /// separately because the tempting implementation - treat a missing
    /// threshold as zero - would hand every member project-wide authority.
    #[test]
    fn a_missing_admin_row_narrows_rather_than_widens() {
        assert_eq!(effective_project_rank(Some(OWNER), None, None), 0);
        assert_eq!(
            effective_project_rank(Some(OWNER), Some(VIEWER), None),
            VIEWER
        );
    }
}

#[cfg(test)]
mod database_tests;
