//! Mechanism-independent service identity types.

use std::collections::BTreeMap;

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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCredentials<'a> {
    bearer_assertion: Option<&'a str>,
    expected_audience: &'a str,
}

impl<'a> PeerCredentials<'a> {
    /// Record the assertion observed by a transport and the expected audience.
    #[must_use]
    pub fn new(bearer_assertion: Option<&'a str>, expected_audience: &'a str) -> Self {
        Self {
            bearer_assertion,
            expected_audience,
        }
    }
}

/// Presence-proven credentials passed to a verifier implementation.
///
/// Its fields are private and it has no public constructor, so an
/// implementation can never receive an absent or empty assertion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentedCredentials<'a> {
    bearer_assertion: &'a str,
    expected_audience: &'a str,
}

impl PresentedCredentials<'_> {
    /// Return the non-empty bearer assertion observed by the transport.
    #[must_use]
    pub fn bearer_assertion(&self) -> &str {
        self.bearer_assertion
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
/// Returns [`AuthError::NoCredentialPresented`] for a missing or empty bearer
/// assertion. Other errors come from the selected verifier implementation.
pub fn verify_identity(
    verifier: &(impl IdentityVerifier + ?Sized),
    observed: &PeerCredentials<'_>,
) -> Result<ServiceIdentity, AuthError> {
    let bearer_assertion = observed
        .bearer_assertion
        .filter(|assertion| !assertion.is_empty())
        .ok_or(AuthError::NoCredentialPresented)?;
    let presented = PresentedCredentials {
        bearer_assertion,
        expected_audience: observed.expected_audience,
    };
    verifier.verify(&presented)
}
