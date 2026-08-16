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
