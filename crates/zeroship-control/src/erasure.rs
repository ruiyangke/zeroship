//! The erasure seam: what the control plane answers when the auth service asks
//! whether a human can be erased.
//!
//! # Why this endpoint exists at all
//!
//! Account erasure is the auth service's lifecycle -- `POST /me/delete`, the
//! grace window, `cron::account_reaper`. Ownership is the control plane's. The
//! question "does deleting this person strand something" can only be answered
//! where the organization tables are readable, and MEASURED they are not
//! readable from auth: `zeroship_auth` holds no privilege at all on
//! `zeroship.organization_members`, `zeroship.organization_accounts` or
//! `zeroship.invoices` (`information_schema.role_table_grants`, on a database
//! with the full corpus applied). The reaper used to ask that question anyway,
//! on the auth connection, and under the real role it does not fail to
//! *retain* -- it fails `42501` and takes the whole erasure with it.
//!
//! So the question crosses the boundary as an HTTP call rather than a
//! cross-domain read, and the answer is computed by the process that owns the
//! tables.
//!
//! # The rule
//!
//! A LIVE organization on which the principal holds the ONLY owner seat is a
//! blocker. Erasing them otherwise leaves an organization nobody can
//! administer: `organization_members.user_id` is `ON DELETE CASCADE`, so the
//! seat goes with the human, and there is no path by which anyone -- including
//! the operator, who has no super-admin plane -- can seat a replacement. It is
//! also the state `projects.organization_id`'s `ON DELETE RESTRICT` would abort
//! against, mid-transaction, naming a constraint to a person who asked to be
//! deleted.
//!
//! The rule is uniform, INCLUDING the principal's own personal organization,
//! and that is a deliberate cost rather than an oversight. A personal
//! organization is a billing subject with a slug, and closing one is a real act
//! with a real precondition (`dissolve` refuses while it still owns projects).
//! Doing it silently, as a side effect of "delete my account", would destroy a
//! creator's apps without ever saying so. The funnel is explicit instead: apps,
//! then projects, then the organization, then the account -- and each refusal
//! names the next step.
//!
//! Deliberately NOT blockers:
//!
//!   * an organization with a second owner (the seat cascades away; the
//!     remaining owner administers it),
//!   * a seat below `owner` (same),
//!   * an ALREADY DISSOLVED organization (it is closed, its ledger is retained
//!     by design, and nobody needs to administer it again).
//!
//! # The second rule: money
//!
//! An organization that still OWES is a blocker too, and it is a different rule
//! rather than an extra condition on the first. The ownership rule is about who
//! can administer a live organization; this one is about a claim that has to be
//! settled by somebody, and the last owner's seat is the last row that names
//! anyone who could.
//!
//! It therefore covers DISSOLVED organizations, which the ownership rule
//! deliberately does not. Closing an organization settles nothing - the
//! reconciler bills the previous month whether or not the subject was closed,
//! so an invoice can be finalized AFTER a dissolve - and without this arm
//! "dissolve, then delete the account" walks away from an unpaid invoice
//! through the one door the ownership rule holds open.
//!
//! What is owed is answered by ONE function,
//! [`crate::billing_read::outstanding_billing`], which every enforcement point
//! shares. This module does not carry billing SQL; it decides WHICH
//! organizations to ask about.

use std::sync::Arc;

use compio_postgres::GenericClient;
use ntex::web::{
    self,
    types::{Path, State},
};
use serde::Serialize;
use zeroship_core::UserId;

use crate::billing_read::{self, BillingRemedy, LocalInvoicing, OutstandingBilling};
use crate::internal::check_service_auth;
use crate::registry::RegistryError;
use crate::AppState;

/// The one action that clears a blocker, chosen from the same row the blocker
/// was raised on so the two can never disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErasureRemedy {
    /// Another member holds a seat: hand them the organization
    /// (`POST /api/organizations/{id}/transfer`).
    Transfer,
    /// Nobody to hand it to, and it still owns projects. `dissolve` refuses
    /// while a project remains, so the projects go first.
    DeleteProjects,
    /// Nobody to hand it to and nothing left in it: close it
    /// (`DELETE /api/organizations/{id}`).
    Dissolve,
}

impl ErasureRemedy {
    /// Derived, never stored. `Transfer` outranks the rest because handing the
    /// organization over is the only remedy that keeps it alive, and a creator
    /// with a colleague on the account almost never wants to close it.
    #[must_use]
    const fn for_counts(other_member_count: i64, project_count: i64) -> Self {
        if other_member_count > 0 {
            Self::Transfer
        } else if project_count > 0 {
            Self::DeleteProjects
        } else {
            Self::Dissolve
        }
    }
}

/// One organization that must be dealt with before this principal can be
/// erased. Every field is a fact the caller can render without a second call.
#[derive(Debug, Clone, Serialize)]
pub struct ErasureBlocker {
    pub organization_id: String,
    pub organization_slug: String,
    pub organization_name: String,
    /// The principal's own personal workspace, as opposed to an organization
    /// they happen to be the last owner of. Changes the copy, not the rule.
    pub personal: bool,
    pub other_member_count: i64,
    pub project_count: i64,
    pub remedy: ErasureRemedy,
}

/// One organization that still owes and whose only owner seat is the one about
/// to be erased. Every field is a fact the caller can render without a second
/// call, including the full typed detail.
#[derive(Debug, Clone, Serialize)]
pub struct BillingBlocker {
    pub organization_id: String,
    pub organization_slug: String,
    pub organization_name: String,
    /// The principal's own personal workspace. Changes the copy, not the rule.
    pub personal: bool,
    /// The organization was ALREADY closed and still owes. Closing it settled
    /// nothing; this is the case the ownership rule does not see.
    pub dissolved: bool,
    /// Cash owed across the unpaid invoices, in [`Self::currency`]'s minor
    /// unit. Zero when only unbilled usage remains - an unbilled period HAS a
    /// price, and `outstanding.unbilled_periods` carries it, but no claim
    /// exists that a payment could settle. See
    /// [`crate::billing_read::OutstandingBilling::owed_cents`].
    pub owed_cents: i64,
    pub currency: String,
    pub unpaid_invoice_count: i64,
    pub unbilled_period_count: i64,
    pub remedy: BillingRemedy,
    /// The full description the summary above is derived from, so a caller
    /// that renders detail does not have to ask a second endpoint.
    pub outstanding: OutstandingBilling,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErasurePreflight {
    pub principal_id: UserId,
    pub blockers: Vec<ErasureBlocker>,
    pub billing_blockers: Vec<BillingBlocker>,
}

impl ErasurePreflight {
    /// Clear means BOTH lists are empty. The two rules are independent - a
    /// human can be nobody's last owner and still be the last seat on a closed
    /// organization that owes - so an `is_clear` reading one of them would
    /// pass exactly the case the other exists for.
    #[must_use]
    pub fn is_clear(&self) -> bool {
        self.blockers.is_empty() && self.billing_blockers.is_empty()
    }
}

/// Every live organization on which `principal` holds the only owner seat.
///
/// ONE statement. The sole-ownership test is a `NOT EXISTS` over a second owner
/// rather than a `count(*) = 1`, because the two differ when the principal
/// somehow holds two rows for one organization -- a count would then read `2`
/// and silently clear a blocker, while the `NOT EXISTS` still refuses.
///
/// Both rules are asked here, and BOTH have to answer. There is no arm in which
/// one of them failing yields a report the other filled in.
///
/// # Errors
///
/// [`RegistryError`] wrapping whichever read failed; the handler is what turns
/// it into a refusal, because an unanswerable preflight must not read as a
/// clear one.
pub async fn preflight(
    db: &(impl GenericClient + Sync),
    principal: &UserId,
    invoicing: LocalInvoicing,
) -> Result<ErasurePreflight, RegistryError> {
    let rows = db
        .query(
            "SELECT o.id, o.slug::text AS slug, o.name, \
                    (o.personal_owner_id = $1) AS personal, \
                    (SELECT count(*) FROM zeroship.organization_members other \
                      WHERE other.organization_id = o.id \
                        AND other.user_id <> $1) AS other_member_count, \
                    (SELECT count(*) FROM zeroship.projects p \
                      WHERE p.organization_id = o.id) AS project_count \
             FROM zeroship.organization_members m \
             JOIN zeroship.organizations o ON o.id = m.organization_id \
             WHERE m.user_id = $1 \
               AND m.role = 'owner' \
               AND o.dissolved_at IS NULL \
               AND NOT EXISTS ( \
                     SELECT 1 FROM zeroship.organization_members rival \
                      WHERE rival.organization_id = o.id \
                        AND rival.role = 'owner' \
                        AND rival.user_id <> $1) \
             ORDER BY o.slug",
            &[&principal.as_str()],
        )
        .await?;
    let blockers = rows
        .iter()
        .map(|row| {
            let other_member_count: i64 = row.get("other_member_count");
            let project_count: i64 = row.get("project_count");
            ErasureBlocker {
                organization_id: row.get("id"),
                organization_slug: row.get("slug"),
                organization_name: row.get("name"),
                // `personal_owner_id` is nullable, so the comparison is NULL
                // for every shared organization -- not false.
                personal: row.get::<_, Option<bool>>("personal").unwrap_or(false),
                other_member_count,
                project_count,
                remedy: ErasureRemedy::for_counts(other_member_count, project_count),
            }
        })
        .collect();
    let billing_blockers = billing_blockers(db, principal, invoicing).await?;
    Ok(ErasurePreflight {
        principal_id: principal.clone(),
        blockers,
        billing_blockers,
    })
}

/// Every organization on which `principal` holds the only owner seat AND which
/// still owes.
///
/// The candidate scan is the sole-ownership shape of [`preflight`] with the
/// `dissolved_at IS NULL` filter REMOVED, for the reason the module header
/// gives: a closed organization is exactly where an unsettled claim hides from
/// the ownership rule.
///
/// One round trip per candidate rather than one join. The candidate set is the
/// organizations ONE person is the last owner of, so it is small by
/// construction, and the alternative is inlining the debt SQL here - a second
/// spelling of the predicate, which is the thing this design is trying not to
/// have. The billing read is asked per organization because that is the shape
/// the dissolve path needs too.
async fn billing_blockers(
    db: &(impl GenericClient + Sync),
    principal: &UserId,
    invoicing: LocalInvoicing,
) -> Result<Vec<BillingBlocker>, RegistryError> {
    let rows = db
        .query(
            "SELECT o.id, o.slug::text AS slug, o.name, \
                    (o.personal_owner_id = $1) AS personal, \
                    (o.dissolved_at IS NOT NULL) AS dissolved \
             FROM zeroship.organization_members m \
             JOIN zeroship.organizations o ON o.id = m.organization_id \
             WHERE m.user_id = $1 \
               AND m.role = 'owner' \
               AND NOT EXISTS ( \
                     SELECT 1 FROM zeroship.organization_members rival \
                      WHERE rival.organization_id = o.id \
                        AND rival.role = 'owner' \
                        AND rival.user_id <> $1) \
             ORDER BY o.slug",
            &[&principal.as_str()],
        )
        .await?;

    let mut blockers = Vec::new();
    for row in &rows {
        let organization_id: String = row.get("id");
        // A driver failure inside the billing read must NOT read as "settled".
        // It is re-raised, and the handler answers 500, because a preflight
        // that could not be computed must never be indistinguishable from one
        // that came back clear.
        let outstanding =
            billing_read::outstanding_billing(db, &organization_id, invoicing).await?;
        let Some(remedy) = outstanding.remedy() else {
            continue;
        };
        blockers.push(BillingBlocker {
            organization_id,
            organization_slug: row.get("slug"),
            organization_name: row.get("name"),
            personal: row.get::<_, Option<bool>>("personal").unwrap_or(false),
            dissolved: row.get("dissolved"),
            owed_cents: outstanding.owed_cents(),
            currency: outstanding.currency().to_string(),
            unpaid_invoice_count: outstanding.unpaid_invoices.len() as i64,
            unbilled_period_count: outstanding.unbilled_periods.len() as i64,
            remedy,
            outstanding,
        });
    }
    Ok(blockers)
}

/// `GET /internal/principals/{principal_id}/erasure-preflight`.
///
/// SERVICE-authenticated, not control-key authenticated: the caller presents its
/// own ed25519 assertion and the allowlist grants this endpoint to `svc/auth`
/// alone. There is no user principal on this call, and the principal in the path
/// is the SUBJECT, never the caller -- so the credential is the only thing
/// deciding who may ask about whom, and it has to name ONE service.
///
/// The shared control key would not have. It is one identity four processes
/// already hold, so putting this route on it means either the auth service holds
/// that key -- and with it the route table, the version feed and both reconcile
/// triggers -- or the erasure preflight is unreachable. Neither is the trade
/// this route is worth, and the assertion mechanism costs nothing extra here:
/// `zeroship dev init` already writes `svc-auth.pem` and publishes its public
/// half in the peer document control verifies against.
///
/// A driver failure answers 500, NEVER an empty blocker list -- a preflight that
/// cannot be computed must not be indistinguishable from one that came back
/// clear.
pub async fn erasure_preflight(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    principal_id: Path<String>,
) -> web::HttpResponse {
    if let Some(resp) = check_service_auth(
        &req,
        &state,
        zeroship_core::service_identity::endpoints::CONTROL_ERASURE_PREFLIGHT,
    )
    .await
    {
        return resp;
    }
    let Ok(principal) = UserId::parse(&principal_id) else {
        return web::HttpResponse::BadRequest()
            .json(&serde_json::json!({"error": "bad principal_id"}));
    };
    let invoicing = LocalInvoicing::of(&state.billing_stack);
    match preflight(state.control_pg.as_ref(), &principal, invoicing).await {
        Ok(report) => web::HttpResponse::Ok().json(&report),
        Err(e) => {
            tracing::error!(
                principal_id = principal.as_str(),
                error = %e,
                "control-internal: erasure preflight failed"
            );
            web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error": "preflight unavailable"}))
        }
    }
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/internal/principals/{principal_id}/erasure-preflight")
            .route(web::get().to(erasure_preflight)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The remedy is derived from the counts, so the three arms are total and
    /// ordered. `Transfer` must win over `DeleteProjects` when both apply: an
    /// organization with a colleague AND projects is handed over, not emptied.
    #[test]
    fn remedy_follows_the_counts_and_prefers_handing_over() {
        assert_eq!(ErasureRemedy::for_counts(1, 3), ErasureRemedy::Transfer);
        assert_eq!(ErasureRemedy::for_counts(1, 0), ErasureRemedy::Transfer);
        assert_eq!(
            ErasureRemedy::for_counts(0, 3),
            ErasureRemedy::DeleteProjects
        );
        assert_eq!(ErasureRemedy::for_counts(0, 0), ErasureRemedy::Dissolve);
    }

    fn report(
        blockers: Vec<ErasureBlocker>,
        billing_blockers: Vec<BillingBlocker>,
    ) -> ErasurePreflight {
        ErasurePreflight {
            principal_id: UserId::mint(),
            blockers,
            billing_blockers,
        }
    }

    fn ownership_blocker() -> ErasureBlocker {
        ErasureBlocker {
            organization_id: "org_0000000000000000000000000".into(),
            organization_slug: "solo".into(),
            organization_name: "Solo".into(),
            personal: true,
            other_member_count: 0,
            project_count: 0,
            remedy: ErasureRemedy::Dissolve,
        }
    }

    fn money_blocker() -> BillingBlocker {
        BillingBlocker {
            organization_id: "org_0000000000000000000000001".into(),
            organization_slug: "closed".into(),
            organization_name: "Closed".into(),
            personal: false,
            dissolved: true,
            owed_cents: 1000,
            currency: "usd".into(),
            unpaid_invoice_count: 1,
            unbilled_period_count: 0,
            remedy: BillingRemedy::SettleInvoices,
            outstanding: OutstandingBilling {
                organization_id: "org_0000000000000000000000001".into(),
                unpaid_invoices: vec![],
                unbilled_periods: vec![],
                billing_identity_on_file: false,
            },
        }
    }

    /// TWO empty lists are the only clear answer, and each one alone refuses.
    /// The billing case is the load-bearing half: its organization is
    /// DISSOLVED, so the ownership rule reports nothing about it, and a
    /// `is_clear` that read only `blockers` would erase the last human who
    /// names an unpaid invoice.
    #[test]
    fn an_empty_blocker_list_is_the_only_clear_answer() {
        assert!(report(vec![], vec![]).is_clear());
        assert!(!report(vec![ownership_blocker()], vec![]).is_clear());
        assert!(!report(vec![], vec![money_blocker()]).is_clear());
        assert!(!report(vec![ownership_blocker()], vec![money_blocker()]).is_clear());
    }
}
