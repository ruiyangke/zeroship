//! Mechanism-independent service identity types.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use serde_json::Value;

/// The administrative boundary that gives a service name its scope.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TrustDomain(Box<str>);

impl TrustDomain {
    /// Construct a trust domain from its canonical name.
    #[must_use]
    pub fn new(value: impl Into<Box<str>>) -> Self {
        Self(value.into())
    }
}

/// A hierarchical service name within a trust domain.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ServiceName(Box<str>);

impl ServiceName {
    /// Construct a hierarchical service name.
    #[must_use]
    pub fn new(value: impl Into<Box<str>>) -> Self {
        Self(value.into())
    }
}

/// An opaque description of the mechanism that verified an identity.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MechanismTag(Box<str>);

impl MechanismTag {
    /// Construct an opaque mechanism tag.
    #[must_use]
    pub fn new(value: impl Into<Box<str>>) -> Self {
        Self(value.into())
    }
}

impl AsRef<str> for MechanismTag {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A service name and its trust domain, compared as one authorization unit.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ServicePrincipal {
    trust_domain: TrustDomain,
    name: ServiceName,
}

impl ServicePrincipal {
    /// Construct a principal from a trust domain and hierarchical name.
    #[must_use]
    pub fn new(trust_domain: TrustDomain, name: ServiceName) -> Self {
        Self { trust_domain, name }
    }
}

/// Verified service identity consumed by authorization code.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceIdentity {
    principal: ServicePrincipal,
    mechanism: MechanismTag,
    attributes: BTreeMap<String, Value>,
}

impl ServiceIdentity {
    /// Construct the mechanism-thin result of successful verification.
    #[must_use]
    pub fn new(
        principal: ServicePrincipal,
        mechanism: MechanismTag,
        attributes: BTreeMap<String, Value>,
    ) -> Self {
        Self {
            principal,
            mechanism,
            attributes,
        }
    }

    /// Compare the identity's name and trust domain in one operation.
    #[must_use]
    pub fn matches_principal(&self, expected: &ServicePrincipal) -> bool {
        &self.principal == expected
    }

    /// Return the opaque verification-mechanism tag.
    #[must_use]
    pub fn mechanism(&self) -> &MechanismTag {
        &self.mechanism
    }

    /// Return mechanism-specific, non-authoritative verification facts.
    #[must_use]
    pub fn attributes(&self) -> &BTreeMap<String, Value> {
        &self.attributes
    }
}

/// Transport observations supplied to the service identity framework.
///
/// This is the mechanism-fat input. It can represent an absent assertion so
/// transports do not have to invent one. Only [`verify_identity`] can convert
/// it into the presence-proven input accepted by verifier implementations.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct PeerCredentials<'a> {
    bearer_assertion: Option<&'a str>,
    tls_peer: Option<TlsPeerInfo<'a>>,
    expected_audience: &'a str,
}

impl fmt::Debug for PeerCredentials<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PeerCredentials")
            .field(
                "bearer_assertion",
                &self.bearer_assertion.map(|_| "[REDACTED]"),
            )
            .field("tls_peer", &self.tls_peer)
            .field("expected_audience", &self.expected_audience)
            .finish()
    }
}

impl<'a> PeerCredentials<'a> {
    /// Record all credentials observed by a transport and the expected audience.
    #[must_use]
    pub fn new(
        bearer_assertion: Option<&'a str>,
        tls_peer: Option<TlsPeerInfo<'a>>,
        expected_audience: &'a str,
    ) -> Self {
        Self {
            bearer_assertion,
            tls_peer,
            expected_audience,
        }
    }
}

/// Peer certificate observations supplied by a TLS transport.
///
/// This is only a transport carrier. It is not an mTLS verifier or adapter.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct TlsPeerInfo<'a> {
    certificate_chain_der: &'a [&'a [u8]],
}

impl fmt::Debug for TlsPeerInfo<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TlsPeerInfo")
            .field("certificate_count", &self.certificate_chain_der.len())
            .finish()
    }
}

impl<'a> TlsPeerInfo<'a> {
    /// Construct a TLS observation from a non-empty certificate chain.
    #[must_use]
    pub fn new(certificate_chain_der: &'a [&'a [u8]]) -> Option<Self> {
        (!certificate_chain_der.is_empty()).then_some(Self {
            certificate_chain_der,
        })
    }

    /// Return the certificate chain exactly as observed by the transport.
    #[must_use]
    pub fn certificate_chain_der(&self) -> &'a [&'a [u8]] {
        self.certificate_chain_der
    }
}

/// Presence-proven credentials passed to a verifier implementation.
///
/// Its state is private and it has no public constructor. The bearer assertion
/// is non-optional, so an implementation can never receive no credential.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct PresentedCredentials<'a> {
    bearer_assertion: &'a str,
    tls_peer: Option<TlsPeerInfo<'a>>,
    expected_audience: &'a str,
}

impl fmt::Debug for PresentedCredentials<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PresentedCredentials")
            .field("bearer_assertion", &"[REDACTED]")
            .field("tls_peer", &self.tls_peer)
            .field("expected_audience", &self.expected_audience)
            .finish()
    }
}

impl PresentedCredentials<'_> {
    /// Return the non-empty bearer assertion observed by the transport.
    #[must_use]
    pub fn bearer_assertion(&self) -> &str {
        self.bearer_assertion
    }

    /// Return the TLS peer observation supplied by the transport, if any.
    #[must_use]
    pub fn tls_peer(&self) -> Option<&TlsPeerInfo<'_>> {
        self.tls_peer.as_ref()
    }

    /// Return the audience the verifier must require.
    #[must_use]
    pub fn expected_audience(&self) -> &str {
        self.expected_audience
    }
}

/// Failure to establish a service identity.
///
/// Every variant means the same thing to the request: it is refused. They
/// differ in what the operator should DO about it, which is why the outage case
/// is not folded into [`AuthError::CredentialRejected`] - a page for a database
/// outage and an alert on a rise in rejected credentials are different
/// responses to different incidents, and a caller that cannot tell them apart
/// gets to choose one of them wrongly.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AuthError {
    /// The transport supplied no non-empty bearer for the supported mechanism.
    #[error("no service credential presented")]
    NoCredentialPresented,
    /// A presented credential did not pass mechanism-specific verification.
    ///
    /// This is the only variant that says anything about the CREDENTIAL, and it
    /// says exactly one thing: no. It never reports which check tripped, so it
    /// is not an oracle for a prober.
    #[error("service credential rejected")]
    CredentialRejected,
    /// A store the mechanism must consult before admitting a credential could
    /// not be reached, so the credential was refused without being judged.
    ///
    /// FAIL CLOSED, identically to [`AuthError::CredentialRejected`]: a store
    /// that cannot answer has not said the credential is fresh, and this
    /// variant exists to change what the caller can OBSERVE and log, never
    /// which requests are admitted.
    ///
    /// It does tell a prober that a backing store is unavailable. That is a
    /// fact about the deployment rather than about their credential - it is
    /// the same signal a 503 carries - and it does not narrow which check a
    /// credential would have failed.
    #[error("service credential store unavailable")]
    StoreUnavailable,
}

/// The future a verifier returns from [`IdentityVerifier::verify`].
///
/// Boxed rather than written as `async fn` in the trait for two reasons. It
/// keeps the trait dyn-compatible, so a deployment can hold its verifier as
/// `&dyn IdentityVerifier` exactly as the `?Sized` bound on [`verify_identity`]
/// always promised; and it names one concrete return type, so every
/// implementation is spelled the same way. There is no `+ Send`: this stack is
/// compio/io_uring, whose futures are thread-per-core and not `Send`.
pub type VerifyFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ServiceIdentity, AuthError>> + 'a>>;

/// Mechanism-specific mapping from presented credentials to a neutral identity.
///
/// Verification is asynchronous because a correct mechanism can need I/O to
/// complete it. The shipped JWT-assertion mechanism must claim the assertion's
/// `jti` in a store shared by every replica of the callee before it may return
/// an identity (OIDC Core section 9 makes single use a MUST, and skipping it is
/// CVE-2020-15222). Making that claim a step the caller performs AFTER
/// `verify` returned would mean a `ServiceIdentity` exists for a replayed
/// assertion, so the await belongs inside the seam.
pub trait IdentityVerifier {
    /// Verify a presence-proven observation and return a mechanism-thin identity.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::CredentialRejected`] when mechanism checks fail.
    fn verify<'a>(&'a self, credentials: &'a PresentedCredentials<'a>) -> VerifyFuture<'a>;
}

/// Non-cryptographic verifier used to exercise the framework seam.
///
/// This verifier only proves that transport observations reach an
/// implementation after the framework's presence check. It must not be used
/// as an authentication mechanism.
#[derive(Clone, Debug)]
pub struct StubIdentityVerifier {
    identity: ServiceIdentity,
}

impl StubIdentityVerifier {
    /// Construct a stub that maps every presented credential to one identity.
    #[must_use]
    pub fn new(identity: ServiceIdentity) -> Self {
        Self { identity }
    }
}

impl IdentityVerifier for StubIdentityVerifier {
    fn verify<'a>(&'a self, _credentials: &'a PresentedCredentials<'a>) -> VerifyFuture<'a> {
        Box::pin(async move { Ok(self.identity.clone()) })
    }
}

/// Reject absent credentials before dispatching to a verifier implementation.
///
/// # Errors
///
/// Returns [`AuthError::NoCredentialPresented`] when the transport supplied no
/// non-empty bearer assertion. TLS observations are not accepted as credentials
/// while the only supported mechanism is JWT. Other errors come from the
/// verifier implementation.
pub async fn verify_identity(
    verifier: &(impl IdentityVerifier + ?Sized),
    observed: &PeerCredentials<'_>,
) -> Result<ServiceIdentity, AuthError> {
    let bearer_assertion = observed
        .bearer_assertion
        .filter(|value| !value.is_empty())
        .ok_or(AuthError::NoCredentialPresented)?;
    let presented = PresentedCredentials {
        bearer_assertion,
        tls_peer: observed.tls_peer,
        expected_audience: observed.expected_audience,
    };
    verifier.verify(&presented).await
}

/// Verify a peer's transport credential and its grant on ONE endpoint.
///
/// The whole inbound guard for an internal edge, in one call, so every edge
/// runs the same three checks in the same order and none of them can be
/// half-written at a call site:
///
/// 1. a credential was presented at all ([`verify_identity`]);
/// 2. it verifies under the peer bundle for the issuer it claims, against
///    `expected_audience` - which must be the issuer identifier the CALLEE is
///    ADDRESSED by, never an endpoint URL. That is usually the callee's own
///    issuer and is not always it: a process may mint under a finer name than
///    its callers hold, which is why
///    [`crate::service_peers::ServiceKeyring`] carries the two separately;
/// 3. the verified principal holds the machine grant for `endpoint`.
///
/// Step 3 is what makes a valid credential insufficient. Without it any service
/// that can mint an assertion at all reaches every guarded edge on every peer,
/// which is the shared-bearer property this mechanism replaces.
///
/// # Errors
///
/// Returns [`AuthError::NoCredentialPresented`] when the header carries no
/// bearer, [`AuthError::CredentialRejected`] when verification fails or the
/// grant is absent, and [`AuthError::StoreUnavailable`] when a store the
/// mechanism must consult could not answer.
pub async fn verify_service_call(
    verifier: &(impl IdentityVerifier + ?Sized),
    authorization: Option<&str>,
    expected_audience: &str,
    endpoint: ServiceEndpoint,
) -> Result<ServiceIdentity, AuthError> {
    let bearer = authorization.and_then(crate::auth::extract_bearer);
    let observed = PeerCredentials::new(bearer, None, expected_audience);
    let identity = verify_identity(verifier, &observed).await?;
    if !authorize(&identity, endpoint) {
        tracing::debug!(
            destination = endpoint.destination(),
            path = endpoint.path_template(),
            "service call rejected: verified principal holds no grant for this endpoint"
        );
        return Err(AuthError::CredentialRejected);
    }
    Ok(identity)
}

/// A platform HTTP endpoint protected by service authorization.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ServiceEndpoint {
    destination: &'static str,
    method: &'static str,
    path_template: &'static str,
}

impl ServiceEndpoint {
    const fn new(
        destination: &'static str,
        method: &'static str,
        path_template: &'static str,
    ) -> Self {
        Self {
            destination,
            method,
            path_template,
        }
    }

    /// Return the destination service that owns this endpoint.
    #[must_use]
    pub const fn destination(self) -> &'static str {
        self.destination
    }

    /// Return the exact HTTP method required by this endpoint.
    #[must_use]
    pub const fn method(self) -> &'static str {
        self.method
    }

    /// Return the exact route template registered by the destination.
    #[must_use]
    pub const fn path_template(self) -> &'static str {
        self.path_template
    }
}

/// Service endpoints named by destination and operation.
pub mod endpoints {
    use super::ServiceEndpoint;

    pub const CDC_SUBSCRIBE: ServiceEndpoint =
        ServiceEndpoint::new("cdc", "GET", "/internal/v1/cdc/subscribe");

    pub const GATEWAY_BACKCHANNEL_LOGOUT: ServiceEndpoint =
        ServiceEndpoint::new("gateway", "POST", "/oidc/backchannel-logout");
    pub const GATEWAY_WORKFLOW_ADVANCE: ServiceEndpoint = ServiceEndpoint::new(
        "gateway",
        "POST",
        "/__zeroship/internal/workflow-advance",
    );
    pub const CONTROL_ROUTES: ServiceEndpoint =
        ServiceEndpoint::new("control", "GET", "/internal/routes");
    pub const CONTROL_WORKFLOW_SIGNAL_INGRESS: ServiceEndpoint = ServiceEndpoint::new(
        "control",
        "POST",
        "/internal/workflows/signals/ingress",
    );
    pub const CONTROL_VERSIONS: ServiceEndpoint =
        ServiceEndpoint::new("control", "GET", "/internal/versions");
    pub const CONTROL_APP: ServiceEndpoint =
        ServiceEndpoint::new("control", "GET", "/internal/apps/{app_id}");
    pub const CONTROL_APP_ENV: ServiceEndpoint =
        ServiceEndpoint::new("control", "GET", "/internal/apps/{app_id}/env");
    pub const CONTROL_APP_DATA_KEY: ServiceEndpoint =
        ServiceEndpoint::new("control", "GET", "/internal/apps/{app_id}/data-key");
    pub const CONTROL_DEPLOYMENT_HOLD_ACQUIRE: ServiceEndpoint =
        ServiceEndpoint::new("control", "POST", "/v1/deployment-holds/acquire");
    pub const CONTROL_DEPLOYMENT_HOLD_RELEASE: ServiceEndpoint =
        ServiceEndpoint::new("control", "POST", "/v1/deployment-holds/release");
    pub const CONTROL_BILLING_RECONCILE: ServiceEndpoint =
        ServiceEndpoint::new("control", "POST", "/internal/billing/reconcile");
    pub const CONTROL_SPEND_RECONCILE: ServiceEndpoint =
        ServiceEndpoint::new("control", "POST", "/internal/spend/reconcile");
    pub const CONTROL_WORKER_ENROL: ServiceEndpoint =
        ServiceEndpoint::new("control", "POST", "/internal/workers/enrol");
    pub const CONTROL_ERASURE_PREFLIGHT: ServiceEndpoint = ServiceEndpoint::new(
        "control",
        "GET",
        "/internal/principals/{principal_id}/erasure-preflight",
    );
    pub const WORKFLOW_WORKERS: ServiceEndpoint = ServiceEndpoint::new(
        "workflow",
        "POST",
        "/v1/workers/list",
    );
    pub const WORKFLOW_ASSIGN: ServiceEndpoint = ServiceEndpoint::new(
        "workflow",
        "POST",
        "/v1/assignments/assign",
    );
    pub const WORKFLOW_VERIFY_ASSIGNMENT: ServiceEndpoint =
        ServiceEndpoint::new("workflow", "POST", "/v1/assignments/verify");
    pub const WORKFLOW_RECOVERY: ServiceEndpoint = ServiceEndpoint::new(
        "workflow",
        "POST",
        "/v1/assignments/recovery",
    );
    pub const WORKFLOW_MANAGE: ServiceEndpoint = ServiceEndpoint::new(
        "workflow",
        "POST",
        "/v1/management/enqueue",
    );
    pub const WORKFLOW_MANAGEMENT_STATUS: ServiceEndpoint = ServiceEndpoint::new(
        "workflow",
        "POST",
        "/v1/management/status",
    );
    pub const WORKFLOW_REGISTER: ServiceEndpoint = ServiceEndpoint::new(
        "workflow",
        "POST",
        "/v1/workers/register",
    );
    pub const WORKFLOW_ASSIGNMENTS: ServiceEndpoint = ServiceEndpoint::new(
        "workflow",
        "POST",
        "/v1/assignments/list",
    );
    pub const WORKFLOW_RENEW: ServiceEndpoint = ServiceEndpoint::new(
        "workflow",
        "POST",
        "/v1/assignments/renew",
    );
    pub const WORKFLOW_RELEASE: ServiceEndpoint = ServiceEndpoint::new(
        "workflow",
        "POST",
        "/v1/assignments/release",
    );
    pub const WORKFLOW_WAKE: ServiceEndpoint = ServiceEndpoint::new(
        "workflow",
        "POST",
        "/v1/wake-hints/publish",
    );
    pub const WORKFLOW_MANAGEMENT_POLL: ServiceEndpoint = ServiceEndpoint::new(
        "workflow",
        "POST",
        "/v1/management/poll",
    );
    pub const WORKFLOW_MANAGEMENT_ACK: ServiceEndpoint = ServiceEndpoint::new(
        "workflow",
        "POST",
        "/v1/management/acknowledge",
    );
    pub const WORKFLOW_JOB_SUBMIT: ServiceEndpoint =
        ServiceEndpoint::new("workflow", "POST", "/v1/jobs/submit");
    pub const WORKFLOW_JOB_CLAIM: ServiceEndpoint =
        ServiceEndpoint::new("workflow", "POST", "/v1/jobs/claim");
    pub const WORKFLOW_JOB_HEARTBEAT: ServiceEndpoint =
        ServiceEndpoint::new("workflow", "POST", "/v1/jobs/heartbeat");
    pub const WORKFLOW_JOB_SETTLE: ServiceEndpoint =
        ServiceEndpoint::new("workflow", "POST", "/v1/jobs/settle");
    pub const WORKER_DISPATCH: ServiceEndpoint =
        ServiceEndpoint::new("worker", "POST", "/dispatch/{app_id}");
    pub const WORKER_WORKFLOW_ADVANCE: ServiceEndpoint = ServiceEndpoint::new(
        "worker",
        "POST",
        "/workflow-advance-unsigned/{app_id}",
    );
    pub const WORKER_APP_LOGS: ServiceEndpoint =
        ServiceEndpoint::new("worker", "GET", "/logs/{app_id}");
}

/// One individual principal and its machine-identity endpoint grants.
///
/// A row is one necessary authorization dimension. Delegated creator grants,
/// app-scoped capabilities, and registered third-party targets remain separate
/// mandatory checks at the endpoint that owns them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceAuthorization {
    principal: ServicePrincipal,
    endpoints: &'static [ServiceEndpoint],
}

impl ServiceAuthorization {
    fn new(principal: ServicePrincipal, endpoints: &'static [ServiceEndpoint]) -> Self {
        Self {
            principal,
            endpoints,
        }
    }

    /// Return whether this row applies to the complete verified principal.
    #[must_use]
    pub fn applies_to(&self, identity: &ServiceIdentity) -> bool {
        identity.matches_principal(&self.principal)
    }

    /// Return all endpoint grants in this row.
    #[must_use]
    pub fn endpoints(&self) -> &'static [ServiceEndpoint] {
        self.endpoints
    }
}

/// Return the measured service-to-service machine-identity table.
#[must_use]
pub fn service_allowlist() -> &'static [ServiceAuthorization] {
    static ALLOWLIST: OnceLock<[ServiceAuthorization; 5]> = OnceLock::new();

    ALLOWLIST.get_or_init(|| {
        let principal = |name| {
            ServicePrincipal::new(TrustDomain::new("zeroship.ai"), ServiceName::new(name))
        };
        [
            ServiceAuthorization::new(
                principal("svc/control"),
                &[
                    endpoints::GATEWAY_WORKFLOW_ADVANCE,
                    endpoints::WORKFLOW_WORKERS,
                    endpoints::WORKFLOW_ASSIGN,
                    endpoints::WORKFLOW_VERIFY_ASSIGNMENT,
                    endpoints::WORKFLOW_RECOVERY,
                    endpoints::WORKFLOW_MANAGE,
                    endpoints::WORKFLOW_MANAGEMENT_STATUS,
                    endpoints::WORKER_APP_LOGS,
                ],
            ),
            ServiceAuthorization::new(
                principal("svc/auth"),
                // Registered third-party BCL targets are checked dynamically.
                //
                // The erasure preflight is the auth service's ONE call into the
                // control plane, and it is here rather than on the shared
                // control key on purpose: that key is one identity four other
                // processes already hold, so handing it to the process that
                // renders the login form would have given the most exposed
                // surface on the platform the route table, the version feed and
                // both reconcile triggers as well.
                &[
                    endpoints::GATEWAY_BACKCHANNEL_LOGOUT,
                    endpoints::CONTROL_ERASURE_PREFLIGHT,
                ],
            ),
            ServiceAuthorization::new(principal("svc/migrate-server"), &[]),
            ServiceAuthorization::new(
                principal("svc/gateway"),
                &[
                    endpoints::CONTROL_ROUTES,
                    endpoints::CONTROL_WORKFLOW_SIGNAL_INGRESS,
                    endpoints::WORKER_DISPATCH,
                    endpoints::WORKER_WORKFLOW_ADVANCE,
                ],
            ),
            ServiceAuthorization::new(
                principal("svc/worker"),
                &[
                    endpoints::CONTROL_VERSIONS,
                    endpoints::CONTROL_DEPLOYMENT_HOLD_ACQUIRE,
                    endpoints::CONTROL_DEPLOYMENT_HOLD_RELEASE,
                    endpoints::WORKFLOW_REGISTER,
                    endpoints::WORKFLOW_ASSIGNMENTS,
                    endpoints::WORKFLOW_RENEW,
                    endpoints::WORKFLOW_RELEASE,
                    endpoints::WORKFLOW_WAKE,
                    endpoints::WORKFLOW_MANAGEMENT_POLL,
                    endpoints::WORKFLOW_MANAGEMENT_ACK,
                    endpoints::WORKFLOW_JOB_SUBMIT,
                    endpoints::WORKFLOW_JOB_CLAIM,
                    endpoints::WORKFLOW_JOB_HEARTBEAT,
                    endpoints::WORKFLOW_JOB_SETTLE,
                    endpoints::CDC_SUBSCRIBE,
                    // Host app reads are role-scoped: an authenticated worker
                    // may request any app's version, environment, and project
                    // data key. Instance enrolment provides attribution and
                    // revocation; it does not establish app assignment.
                    endpoints::CONTROL_APP,
                    endpoints::CONTROL_APP_ENV,
                    endpoints::CONTROL_APP_DATA_KEY,
                    // Enrolment authenticates with THIS SHARED ROLE KEY, so a
                    // holder of it can enrol many instances. That is a
                    // DISTINGUISHER, not a boundary: what it buys is
                    // attribution, per-instance revocation and a countable
                    // event, and it is what makes the narrowing above writable
                    // at all. Do not read this grant as a fence against a
                    // role-key holder.
                    endpoints::CONTROL_WORKER_ENROL,
                ],
            ),
        ]
    })
}

/// Return whether a verified service has the machine grant for an endpoint.
///
/// A true result does not replace any delegated-user or resource-scope check
/// required by that endpoint.
#[must_use]
pub fn authorize(identity: &ServiceIdentity, endpoint: ServiceEndpoint) -> bool {
    service_allowlist()
        .iter()
        .find(|row| row.applies_to(identity))
        .is_some_and(|row| row.endpoints.contains(&endpoint))
}
