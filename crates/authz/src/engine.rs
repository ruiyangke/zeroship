use std::str::FromStr;

use cedar_policy::{Decision, Entities, PolicySet, Request, Schema};
use sha2::{Digest, Sha256};

use crate::{lower, AuthzError, Policy};

const STATIC_POLICY_SOURCES: &[&str] = &[
    include_str!("../policies/platform/admin.cedar"),
    include_str!("../policies/platform/billing.cedar"),
    include_str!("../policies/platform/readonly.cedar"),
    include_str!("../policies/platform/support.cedar"),
    include_str!("../policies/creator/app-members.cedar"),
];

#[derive(Debug)]
pub struct Authorizer {
    policies: PolicySet,
    schema: Option<Schema>,
}

impl Authorizer {
    /// Builds an authorizer by lowering a wrapper policy into Cedar source.
    ///
    /// # Errors
    ///
    /// Returns [`AuthzError::CedarParse`] when Cedar rejects the generated
    /// policy source.
    pub fn new_from_wrapper(policy: &Policy) -> Result<Self, AuthzError> {
        let source = lower(policy);
        Self::new_from_sources(&[source.as_str()])
    }

    /// Builds an authorizer from Cedar source strings.
    ///
    /// # Errors
    ///
    /// Returns [`AuthzError::CedarParse`] when Cedar rejects any source.
    pub fn new_from_sources(sources: &[&str]) -> Result<Self, AuthzError> {
        let source = sources.join("\n");
        let policies =
            PolicySet::from_str(&source).map_err(|err| AuthzError::CedarParse(err.to_string()))?;

        Ok(Self {
            policies,
            schema: None,
        })
    }

    /// Computes the stable SHA-256 cache key for wrapper JSON.
    #[must_use]
    pub fn policy_hash(wrapper_json: &serde_json::Value) -> String {
        policy_hash(wrapper_json)
    }

    /// Evaluates a Cedar authorization request against this policy set.
    #[must_use]
    pub fn is_authorized(&self, request: &Request, entities: &Entities) -> Decision {
        cedar_policy::Authorizer::new()
            .is_authorized(request, &self.policies, entities)
            .decision()
    }

    /// Returns the optional Cedar schema reserved for U4 validation wiring.
    #[must_use]
    pub const fn schema(&self) -> Option<&Schema> {
        self.schema.as_ref()
    }
}

/// Computes the stable SHA-256 cache key for wrapper JSON.
#[must_use]
pub fn policy_hash(wrapper_json: &serde_json::Value) -> String {
    let canonical = canonical_json(wrapper_json);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    hex::encode(hasher.finalize())
}

/// Load the built-in platform + creator authorization policies.
///
/// These policies are static repo assets parsed once by services at boot.
///
/// # Errors
///
/// Returns [`AuthzError::CedarParse`] if any bundled Cedar source is invalid.
pub fn load_platform_policies() -> Result<PolicySet, AuthzError> {
    PolicySet::from_str(&STATIC_POLICY_SOURCES.join("\n"))
        .map_err(|err| AuthzError::CedarParse(err.to_string()))
}

fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => serde_json::to_string(value)
            .expect("serde_json::Value primitive serialization is infallible"),
        serde_json::Value::Array(values) => {
            let values = values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{values}]")
        }
        serde_json::Value::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));

            let entries = entries
                .into_iter()
                .map(|(key, value)| {
                    let key = serde_json::to_string(key)
                        .expect("JSON object key serialization is infallible");
                    format!("{key}:{}", canonical_json(value))
                })
                .collect::<Vec<_>>()
                .join(",");

            format!("{{{entries}}}")
        }
    }
}
