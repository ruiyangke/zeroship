//! The auth service's one credentialed call into the control plane.
//!
//! `control_url` has been configured on this process since before any code read
//! it; this is the first caller. It exists because account erasure is the auth
//! service's lifecycle and ownership is the control plane's, and MEASURED the
//! two cannot be merged by a query: `zeroship_auth` holds no privilege on
//! `zeroship.organization_members`, `zeroship.organizations`,
//! `zeroship.projects`, `zeroship.organization_accounts` or `zeroship.invoices`
//! (`information_schema.role_table_grants`, full corpus applied). A reader that
//! reached across anyway would not degrade -- it would raise `42501` and take
//! the erasure with it, which is exactly what the deleted
//! `user_has_financial_history` did.
//!
//! # Fail-closed, on purpose
//!
//! Every failure arm here is [`PreflightError`], and every caller treats it as a
//! REFUSAL rather than a pass. An erasure request whose precondition could not
//! be checked must not be honoured: the cost of refusing is that the human
//! retries, and the cost of proceeding is an organization nobody can administer
//! and a `users` DELETE that aborts against `projects_organization_id_fkey`.

use std::time::Duration;

use http::Method;
use serde::Deserialize;

/// Wall-clock ceiling on the preflight round trip. `POST /me/delete` is a
/// browser form submit, so this is bounded by what a person will sit through,
/// not by what the control plane might eventually manage.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(5);

/// The action that clears one blocker. Mirrors control's `ErasureRemedy`; the
/// wire is the contract and `Unknown` is what keeps an added variant from
/// turning a refusal into a deserialization error (which the caller would have
/// to classify all over again).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErasureRemedy {
    Transfer,
    DeleteProjects,
    Dissolve,
    #[serde(other)]
    Unknown,
}

impl ErasureRemedy {
    /// One sentence a person can act on. The auth service renders this; the
    /// control plane sends facts, not prose.
    #[must_use]
    pub const fn instruction(self) -> &'static str {
        match self {
            Self::Transfer => "transfer ownership to another member of this organization",
            Self::DeleteProjects => {
                "delete this organization's projects, then dissolve the organization"
            }
            Self::Dissolve => "dissolve this organization",
            Self::Unknown => "resolve this organization's ownership through the control plane",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErasureBlocker {
    pub organization_id: String,
    pub organization_slug: String,
    pub organization_name: String,
    pub personal: bool,
    pub other_member_count: i64,
    pub project_count: i64,
    pub remedy: ErasureRemedy,
}

/// The action that clears one MONEY blocker. Mirrors control's
/// `billing_read::BillingRemedy`; `Unknown` plays the same role as the one on
/// [`ErasureRemedy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingRemedy {
    SettleInvoices,
    BillOutstandingUsage,
    #[serde(other)]
    Unknown,
}

impl BillingRemedy {
    /// One sentence a person can act on. Every arm names the route that is
    /// wired end to end - attaching a default payment method - because a
    /// refusal that names no next step is a dead end, and it is the only
    /// remedy a creator can reach today.
    #[must_use]
    pub const fn instruction(self) -> &'static str {
        match self {
            Self::SettleInvoices => {
                "add a payment method to this organization; the outstanding invoice is \
                 then collected automatically"
            }
            Self::BillOutstandingUsage => {
                "this organization has usage that was never invoiced, which happens when \
                 no payment method is on file; add one so the outstanding usage can be \
                 billed and paid"
            }
            Self::Unknown => "settle this organization's outstanding billing",
        }
    }
}

/// One organization whose LAST owner seat is this human's and which still owes.
///
/// The counts and the total are carried on the wire so the refusal page can
/// name what is owed without a second call. Control also sends the full
/// per-invoice detail; this struct deliberately does not deserialize it,
/// because nothing the auth service renders needs an invoice id.
#[derive(Debug, Clone, Deserialize)]
pub struct BillingBlocker {
    pub organization_id: String,
    pub organization_slug: String,
    pub organization_name: String,
    pub personal: bool,
    /// The organization is already closed and still owes. Closing it settled
    /// nothing, which is why this blocker exists at all.
    pub dissolved: bool,
    pub owed_cents: i64,
    pub currency: String,
    pub unpaid_invoice_count: i64,
    pub unbilled_period_count: i64,
    pub remedy: BillingRemedy,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErasurePreflight {
    pub blockers: Vec<ErasureBlocker>,
    /// Organizations that still owe. A SEPARATE list rather than a variant of
    /// [`ErasureBlocker`], because the two rules cover different organizations:
    /// the ownership rule looks only at LIVE ones, and an unsettled claim on a
    /// CLOSED one is exactly what it does not see.
    pub billing_blockers: Vec<BillingBlocker>,
}

impl ErasurePreflight {
    /// Clear means BOTH lists are empty. A reading of one of them would pass
    /// the case the other exists for.
    #[must_use]
    pub fn is_clear(&self) -> bool {
        self.blockers.is_empty() && self.billing_blockers.is_empty()
    }
}

/// Why the preflight produced no answer. Never a clear one.
#[derive(Debug, thiserror::Error)]
pub enum PreflightError {
    /// No `control_key` on this process. Fail-closed: without it the call
    /// cannot be authenticated, so it is not attempted and not guessed at.
    #[error("control key is not configured; account erasure cannot be verified")]
    NoCredential,
    #[error("control plane unreachable: {0}")]
    Transport(String),
    #[error("control plane answered {status}")]
    Status { status: u16 },
    #[error("control plane answered an unreadable body: {0}")]
    Body(String),
}

/// Ask the control plane whether `principal` can be erased.
///
/// # Errors
///
/// [`PreflightError`] for every arm in which the answer is unknown, including a
/// missing credential. There is no success value that means "could not check".
#[allow(clippy::future_not_send)]
pub async fn erasure_preflight(
    control_url: &str,
    control_key: Option<&str>,
    principal: uuid::Uuid,
) -> Result<ErasurePreflight, PreflightError> {
    let Some(key) = control_key.filter(|k| !k.is_empty()) else {
        return Err(PreflightError::NoCredential);
    };
    let url = format!(
        "{}/internal/principals/{}/erasure-preflight",
        control_url.trim_end_matches('/'),
        principal
    );
    let client = cyper::Client::new();
    let request = client
        .request(Method::GET, &url)
        .map_err(|e| PreflightError::Transport(format!("build request: {e}")))?
        .header("authorization", format!("Bearer {key}"))
        .map_err(|e| PreflightError::Transport(format!("authorization header: {e}")))?
        .header("accept", "application/json")
        .map_err(|e| PreflightError::Transport(format!("accept header: {e}")))?;

    let response = compio::time::timeout(PREFLIGHT_TIMEOUT, request.send())
        .await
        .map_err(|_| PreflightError::Transport("timed out".into()))?
        .map_err(|e| PreflightError::Transport(e.to_string()))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(PreflightError::Status { status });
    }
    let body = response
        .text()
        .await
        .map_err(|e| PreflightError::Body(e.to_string()))?;
    serde_json::from_str(&body).map_err(|e| PreflightError::Body(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_or_empty_credential_refuses_without_a_round_trip() {
        // Both spellings of "not configured" take the same arm. An empty
        // string is what an unset `-file` secret reads as, and treating it as
        // a usable key would send `Bearer ` and read control's 401 as a
        // transport blip.
        for key in [None, Some(""), Some("")] {
            let err = compio::runtime::Runtime::new()
                .expect("runtime")
                .block_on(erasure_preflight("http://127.0.0.1:1", key, uuid::Uuid::nil()))
                .expect_err("must refuse");
            assert!(
                matches!(err, PreflightError::NoCredential),
                "expected NoCredential, got {err:?}"
            );
        }
    }

    /// The wire carries facts; the instruction is this crate's. A remedy the
    /// control plane adds later must still render something a person can act
    /// on rather than failing the whole refusal page.
    #[test]
    fn every_remedy_renders_an_instruction() {
        for remedy in [
            ErasureRemedy::Transfer,
            ErasureRemedy::DeleteProjects,
            ErasureRemedy::Dissolve,
            ErasureRemedy::Unknown,
        ] {
            assert!(!remedy.instruction().is_empty());
        }
        let unknown: ErasureRemedy =
            serde_json::from_str("\"a_remedy_this_build_predates\"").expect("tolerated");
        assert_eq!(unknown, ErasureRemedy::Unknown);
    }

    #[test]
    fn a_clear_preflight_is_exactly_two_empty_blocker_lists() {
        let clear: ErasurePreflight =
            serde_json::from_str(r#"{"principal_id":"x","blockers":[],"billing_blockers":[]}"#)
                .expect("parse");
        assert!(clear.is_clear());
        let blocked: ErasurePreflight = serde_json::from_str(
            r#"{"blockers":[{"organization_id":"org_1","organization_slug":"solo",
                 "organization_name":"Solo","personal":true,"other_member_count":0,
                 "project_count":2,"remedy":"delete_projects"}],"billing_blockers":[]}"#,
        )
        .expect("parse");
        assert!(!blocked.is_clear());
        assert_eq!(blocked.blockers[0].remedy, ErasureRemedy::DeleteProjects);
    }

    /// A money blocker refuses on its own, with NO ownership blocker beside it.
    /// That pairing is the case the ownership rule cannot produce: the
    /// organization is already dissolved, so it is not a live one anybody has
    /// to administer, and the only thing left about it is the debt.
    #[test]
    fn a_billing_blocker_alone_refuses_and_carries_what_is_owed() {
        let owing: ErasurePreflight = serde_json::from_str(
            r#"{"blockers":[],"billing_blockers":[{"organization_id":"org_1",
                 "organization_slug":"closed","organization_name":"Closed",
                 "personal":false,"dissolved":true,"owed_cents":1250,"currency":"usd",
                 "unpaid_invoice_count":1,"unbilled_period_count":0,
                 "remedy":"settle_invoices","outstanding":{"organization_id":"org_1",
                 "unpaid_invoices":[],"unbilled_periods":[]}}]}"#,
        )
        .expect("parse");
        assert!(!owing.is_clear());
        assert!(owing.blockers.is_empty(), "no ownership blocker beside it");
        let blocker = &owing.billing_blockers[0];
        assert_eq!(blocker.owed_cents, 1250);
        assert_eq!(blocker.currency, "usd");
        assert!(blocker.dissolved);
        assert_eq!(blocker.remedy, BillingRemedy::SettleInvoices);
    }

    /// Same tolerance as [`ErasureRemedy`]: a remedy this build predates must
    /// still render a refusal rather than turning the whole answer into a
    /// parse error, which the caller would have to classify all over again.
    #[test]
    fn every_billing_remedy_renders_an_instruction() {
        for remedy in [
            BillingRemedy::SettleInvoices,
            BillingRemedy::BillOutstandingUsage,
            BillingRemedy::Unknown,
        ] {
            assert!(!remedy.instruction().is_empty());
        }
        let unknown: BillingRemedy =
            serde_json::from_str("\"a_remedy_this_build_predates\"").expect("tolerated");
        assert_eq!(unknown, BillingRemedy::Unknown);
    }
}
