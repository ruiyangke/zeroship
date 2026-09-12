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
//! # The credential is this service's OWN identity, never a shared root
//!
//! The call carries an ed25519 assertion minted under `svc/auth`, audienced to
//! the control plane, and control grants `CONTROL_ERASURE_PREFLIGHT` to that
//! principal alone. It is deliberately NOT the shared control key: that key is
//! one identity the gateway, the worker, the migration service and control
//! itself already hold, so presenting it would have made this process
//! indistinguishable from them at control's door - and would have carried the
//! route table, the version feed and both reconcile triggers with it, none of
//! which the auth service has any business reaching.
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
use zeroship_core::service_peers::{service_issuer, ServiceKeyring, CONTROL_SERVICE_NAME};

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
    AttachPaymentMethod,
    ReconcileClosedPeriod,
    #[serde(other)]
    Unknown,
}

impl BillingRemedy {
    /// One sentence a person can act on, in the words of the page rather than
    /// the words of the API. Two arms name the one thing a creator can do -
    /// attach a payment method - because a refusal that names no next step is a
    /// dead end.
    ///
    /// `ReconcileClosedPeriod` is the arm that does NOT, and saying so is the
    /// point. Control raises it when the payment method is already on file and
    /// a closed period was simply never invoiced; the automatic sweep bills
    /// only the immediately previous month, so nothing the person does here
    /// moves it. Telling them to add a card they already have would read as
    /// progress and produce none.
    #[must_use]
    pub const fn instruction(self) -> &'static str {
        match self {
            Self::SettleInvoices => {
                "add a payment method to this organization; the outstanding invoice is \
                 then collected automatically"
            }
            Self::AttachPaymentMethod => {
                "this organization has usage that was never invoiced because no payment \
                 method is on file; add one so the outstanding usage can be billed and paid"
            }
            Self::ReconcileClosedPeriod => {
                "this organization has usage from a past month that was never invoiced, and \
                 the payment method on file is not what is missing; contact support to have \
                 that month billed"
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
    /// This process could not produce a credential naming itself to the control
    /// plane. Fail-closed: the call cannot be authenticated, so it is not
    /// attempted and not guessed at.
    ///
    /// An ABSENT key is not this arm and cannot reach it - `ServiceKeyring::load`
    /// refuses the boot, so a process that is running holds one. What is left is
    /// a signing failure, which is a fault rather than a configuration.
    #[error("this service could not assert its identity to the control plane: {0}")]
    NoCredential(String),
    #[error("control plane unreachable: {0}")]
    Transport(String),
    #[error("control plane answered {status}")]
    Status { status: u16 },
    #[error("control plane answered an unreadable body: {0}")]
    Body(String),
}

/// Mint this service's credential for ONE preflight call.
///
/// A fresh assertion per call, deliberately. Caching one would defeat the
/// single-use property control's replay store enforces: the second presentation
/// is exactly what that store refuses.
fn control_authorization(keyring: &ServiceKeyring) -> Result<String, PreflightError> {
    let control = service_issuer(CONTROL_SERVICE_NAME)
        .map_err(|e| PreflightError::NoCredential(format!("control service issuer: {e}")))?;
    keyring
        .mint_for(&control)
        .map(|assertion| format!("Bearer {assertion}"))
        .map_err(|e| PreflightError::NoCredential(e.to_string()))
}

/// Ask the control plane whether `principal` can be erased.
///
/// # Errors
///
/// [`PreflightError`] for every arm in which the answer is unknown, including a
/// credential this process could not mint. There is no success value that means
/// "could not check".
#[allow(clippy::future_not_send)]
pub async fn erasure_preflight(
    control_url: &str,
    keyring: &ServiceKeyring,
    principal: &zeroship_core::UserId,
) -> Result<ErasurePreflight, PreflightError> {
    let authorization = control_authorization(keyring)?;
    let url = format!(
        "{}/internal/principals/{}/erasure-preflight",
        control_url.trim_end_matches('/'),
        principal.as_str()
    );
    let client = cyper::Client::new();
    let request = client
        .request(Method::GET, &url)
        .map_err(|e| PreflightError::Transport(format!("build request: {e}")))?
        .header("authorization", authorization)
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

    /// The credential names THIS service and reaches exactly one endpoint.
    ///
    /// Both halves matter and only the second distinguishes an assertion from
    /// the shared control key. A key would have opened every `/internal/*` route
    /// that checks it; this opens `CONTROL_ERASURE_PREFLIGHT` and is refused at
    /// the route table, the version feed and both reconcile triggers - verified
    /// here through the same `verify_service_call` control runs, not asserted.
    #[test]
    fn the_minted_credential_names_this_service_and_opens_only_the_preflight() {
        use std::sync::Arc;

        use zeroship_core::service_assertion::{
            InMemoryReplayStore, ServiceAssertionVerifier, ServiceSigningKey, ServiceTrustBundle,
        };
        use zeroship_core::service_identity::{endpoints, verify_service_call};
        use zeroship_core::service_peers::AUTH_SERVICE_NAME;

        let auth = service_issuer(AUTH_SERVICE_NAME).expect("auth issuer");
        let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
        let key = ServiceSigningKey::generate();
        let mut trusted = ServiceTrustBundle::new();
        trusted
            .trust_signing_key(&auth, key.key_id(), &key)
            .expect("trust the auth key");
        let keyring = ServiceKeyring::from_parts(auth, key, ServiceTrustBundle::new())
            .expect("build the auth keyring");
        let verifier = ServiceAssertionVerifier::new(trusted, Arc::new(InMemoryReplayStore::new()));

        let runtime = compio::runtime::Runtime::new().expect("runtime");
        for (endpoint, granted) in [
            (endpoints::CONTROL_ERASURE_PREFLIGHT, true),
            (endpoints::CONTROL_ROUTES, false),
            (endpoints::CONTROL_VERSIONS, false),
            (endpoints::CONTROL_BILLING_RECONCILE, false),
            (endpoints::CONTROL_SPEND_RECONCILE, false),
            (endpoints::CONTROL_APP_ENV, false),
        ] {
            // A FRESH mint per endpoint, because the store is single-use and a
            // reused assertion would be refused for replay rather than for the
            // grant, which is the wrong reason and would hide a widened row.
            let header = control_authorization(&keyring).expect("mint");
            let verified = runtime.block_on(verify_service_call(
                &verifier,
                Some(&header),
                control.as_str(),
                endpoint,
            ));
            assert_eq!(
                verified.is_ok(),
                granted,
                "wrong verdict for {endpoint:?}: {verified:?}"
            );
        }
    }

    /// The audience is the CALLEE's issuer, so the same assertion presented to
    /// any other service is refused. Without this the credential would be a
    /// bearer any peer could replay onward.
    #[test]
    fn the_credential_is_audienced_to_the_control_plane_alone() {
        use std::sync::Arc;

        use zeroship_core::service_assertion::{
            InMemoryReplayStore, ServiceAssertionVerifier, ServiceSigningKey, ServiceTrustBundle,
        };
        use zeroship_core::service_identity::{endpoints, verify_service_call};
        use zeroship_core::service_peers::{AUTH_SERVICE_NAME, GATEWAY_SERVICE_NAME};

        let auth = service_issuer(AUTH_SERVICE_NAME).expect("auth issuer");
        let gateway = service_issuer(GATEWAY_SERVICE_NAME).expect("gateway issuer");
        let key = ServiceSigningKey::generate();
        let mut trusted = ServiceTrustBundle::new();
        trusted
            .trust_signing_key(&auth, key.key_id(), &key)
            .expect("trust the auth key");
        let keyring = ServiceKeyring::from_parts(auth, key, ServiceTrustBundle::new())
            .expect("build the auth keyring");
        let verifier = ServiceAssertionVerifier::new(trusted, Arc::new(InMemoryReplayStore::new()));

        let header = control_authorization(&keyring).expect("mint");
        let verified =
            compio::runtime::Runtime::new()
                .expect("runtime")
                .block_on(verify_service_call(
                    &verifier,
                    Some(&header),
                    gateway.as_str(),
                    endpoints::GATEWAY_BACKCHANNEL_LOGOUT,
                ));
        assert!(
            verified.is_err(),
            "a control-audienced assertion must not open a gateway endpoint"
        );
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
            BillingRemedy::AttachPaymentMethod,
            BillingRemedy::ReconcileClosedPeriod,
            BillingRemedy::Unknown,
        ] {
            assert!(!remedy.instruction().is_empty());
        }
        let unknown: BillingRemedy =
            serde_json::from_str("\"a_remedy_this_build_predates\"").expect("tolerated");
        assert_eq!(unknown, BillingRemedy::Unknown);
    }
}
