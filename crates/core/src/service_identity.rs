//! Mechanism-independent service identity types.

use std::collections::BTreeMap;
use std::fmt;
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
    /// Return the non-empty bearer assertion observed by the transport, if any.
    #[must_use]
    pub fn bearer_assertion(&self) -> Option<&str> {
        Some(self.bearer_assertion)
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
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AuthError {
    /// The transport supplied neither a credential nor a non-empty assertion.
    #[error("no service credential presented")]
    NoCredentialPresented,
    /// A presented credential did not pass mechanism-specific verification.
    #[error("service credential rejected")]
    CredentialRejected,
}

/// Mechanism-specific mapping from presented credentials to a neutral identity.
pub trait IdentityVerifier {
    /// Verify a presence-proven observation and return a mechanism-thin identity.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::CredentialRejected`] when mechanism checks fail.
    fn verify(
        &self,
        credentials: &PresentedCredentials<'_>,
    ) -> Result<ServiceIdentity, AuthError>;
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
    fn verify(
        &self,
        _credentials: &PresentedCredentials<'_>,
    ) -> Result<ServiceIdentity, AuthError> {
        Ok(self.identity.clone())
    }
}

/// Reject absent credentials before dispatching to a verifier implementation.
///
/// # Errors
///
/// Returns [`AuthError::NoCredentialPresented`] when the transport supplied no
/// usable credential. Other errors come from the verifier implementation.
pub fn verify_identity(
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
    verifier.verify(&presented)
}

/// A platform HTTP endpoint protected by service authorization.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ServiceEndpoint(&'static str);

/// Service endpoints named by destination and operation.
pub mod endpoints {
    use super::ServiceEndpoint;

    pub const AUTH_PLATFORM_TOKEN: ServiceEndpoint =
        ServiceEndpoint("auth POST /internal/platform-token");
    pub const MIGRATED_APPLY_MIGRATIONS: ServiceEndpoint =
        ServiceEndpoint("migrated POST /v1/apps/{app}/migrations/apply");
    pub const GATEWAY_BACKCHANNEL_LOGOUT: ServiceEndpoint =
        ServiceEndpoint("gateway POST /oidc/backchannel-logout");
    pub const GATEWAY_WORKFLOW_ADVANCE: ServiceEndpoint =
        ServiceEndpoint("gateway POST workflow advance");
    pub const CONTROL_ROUTES: ServiceEndpoint =
        ServiceEndpoint("control GET /internal/routes");
    pub const CONTROL_WORKFLOW_SIGNAL_INGRESS: ServiceEndpoint =
        ServiceEndpoint("control POST /internal/workflows/signals/ingress");
    pub const CONTROL_VERSIONS: ServiceEndpoint =
        ServiceEndpoint("control GET /internal/versions");
    pub const CONTROL_APP: ServiceEndpoint =
        ServiceEndpoint("control GET /internal/apps/{app}");
    pub const CONTROL_APP_ENV: ServiceEndpoint =
        ServiceEndpoint("control GET /internal/apps/{app}/env");
    pub const CONTROL_BILLING_RECONCILE: ServiceEndpoint =
        ServiceEndpoint("control POST billing reconcile");
    pub const CONTROL_SPEND_RECONCILE: ServiceEndpoint =
        ServiceEndpoint("control POST spend reconcile");
    pub const WORKER_DISPATCH: ServiceEndpoint =
        ServiceEndpoint("worker POST /dispatch/{app}");
    pub const WORKER_WORKFLOW_ADVANCE: ServiceEndpoint =
        ServiceEndpoint("worker POST workflow advance");
    pub const WORKER_APP_LOGS: ServiceEndpoint = ServiceEndpoint("worker app logs");
}

/// One individual service principal and its complete endpoint grants.
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

/// Return the measured service-to-service authorization table.
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
                    endpoints::AUTH_PLATFORM_TOKEN,
                    endpoints::MIGRATED_APPLY_MIGRATIONS,
                    endpoints::GATEWAY_WORKFLOW_ADVANCE,
                    endpoints::WORKER_APP_LOGS,
                ],
            ),
            ServiceAuthorization::new(
                principal("svc/auth"),
                &[endpoints::GATEWAY_BACKCHANNEL_LOGOUT],
            ),
            ServiceAuthorization::new(principal("svc/migrated"), &[]),
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
                    endpoints::CONTROL_APP,
                    endpoints::CONTROL_APP_ENV,
                ],
            ),
        ]
    })
}

/// Return whether an individual verified service may call an endpoint.
#[must_use]
pub fn authorize(identity: &ServiceIdentity, endpoint: ServiceEndpoint) -> bool {
    service_allowlist()
        .iter()
        .find(|row| row.applies_to(identity))
        .is_some_and(|row| row.endpoints.contains(&endpoint))
}
