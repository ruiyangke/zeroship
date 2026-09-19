use std::fmt::Write as _;
use std::str::FromStr;

use cedar_policy::{PolicySet, Schema, ValidationMode, Validator};
use sha2::{Digest, Sha256};

use crate::AuthzError;

/// The whole shipped policy set: the self-service baseline, plus one file per
/// authority band of the organization role ladder.
///
/// There is no platform STAFF policy in it: `admin` / `support` / `billing` /
/// `readonly` each permitted an unconstrained `resource`, so every one of them
/// was a cross-tenant grant, and `admin` was a literal universal allow. A
/// hosting vendor's staff permission model belongs to the vendor's own portal,
/// against its own copy of the data.
///
/// **THIS LIST IS THE ONLY THING THAT LOADS A POLICY.** `build.rs` walks
/// `deploy/policies/`, and parses AND schema-validates every `.cedar` file it
/// finds, but it LOADS none of them - so a policy file that is committed,
/// reviewed, valid against the schema and absent from this array authorizes
/// exactly nothing. A crate test reconciles the two: every `.cedar` file on
/// disk must appear here.
const PLATFORM_POLICY_SOURCES: &[&str] = &[
    include_str!("../../../deploy/policies/platform/self_service.cedar"),
    include_str!("../../../deploy/policies/creator/organization_read.cedar"),
    include_str!("../../../deploy/policies/creator/organization_depart.cedar"),
    include_str!("../../../deploy/policies/creator/organization_develop.cedar"),
    include_str!("../../../deploy/policies/creator/organization_administer.cedar"),
    include_str!("../../../deploy/policies/creator/organization_own.cedar"),
    include_str!("../../../deploy/policies/creator/organization_billing.cedar"),
];

/// The Cedar schema describing the whole authorization vocabulary: the entity
/// types, the closed action list, and the request context keys.
///
/// It is embedded rather than read at runtime for the same reason the policies
/// are: a service that could disagree with the tree about what it is enforcing
/// is a service whose audit rows mean nothing.
const PLATFORM_SCHEMA_SOURCE: &str = include_str!("../../../deploy/policies/zeroship.cedarschema");

/// The shipped bands, and the schema they were validated against.
///
/// The two travel together because they must be the SAME schema at both ends.
/// Validation proves the policies only reference declared actions, entity types
/// and context keys; `eval::build_request` then binds each request to
/// that same declaration, so a request outside the vocabulary is an error
/// instead of a deny nobody can distinguish from an honest non-match. A second,
/// separately-parsed copy on the request side would let the two drift.
#[derive(Clone, Debug)]
pub struct PlatformPolicies {
    policies: PolicySet,
    schema: Schema,
}

impl PlatformPolicies {
    /// The validated policy set.
    #[must_use]
    pub const fn policies(&self) -> &PolicySet {
        &self.policies
    }

    /// The schema the policy set was validated against, and the one every
    /// request must be built against.
    #[must_use]
    pub const fn schema(&self) -> &Schema {
        &self.schema
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

/// Loads the static platform and creator Cedar policies embedded in this crate
/// and validates them against [`PLATFORM_SCHEMA_SOURCE`] under
/// [`ValidationMode::Strict`].
///
/// **THIS IS A REFUSAL, NOT A WARNING.** Every service that authorizes anything
/// calls this on startup and cannot serve without what it returns, so a policy
/// set that does not validate takes the service down instead of authorizing
/// requests against a vocabulary nobody checked. The failure it exists for is
/// silent by construction: `Action::"apps:raed"` parses, loads, denies every
/// request the band was written to permit, and records an audit row identical
/// to an honest non-match.
///
/// Warnings are refused alongside errors. The one that matters is "policy is
/// impossible" - a band that can never fire reads to an auditor as authority
/// that exists, and it denies exactly like a band that was never written.
///
/// The same validation runs in `build.rs` over every `.cedar` file on disk, so
/// a defect that would reach this refusal has already failed the build on the
/// machine that wrote it. That is the mitigation for the boot-time failure mode
/// this function introduces, and it is why the refusal here can be absolute.
///
/// # Errors
///
/// Returns [`AuthzError::CedarParse`] when Cedar rejects the embedded policy
/// sources, and [`AuthzError::CedarValidation`] when the schema fails to parse
/// or the policy set fails to validate against it.
pub fn load_platform_policies() -> Result<PlatformPolicies, AuthzError> {
    let source = PLATFORM_POLICY_SOURCES.join("\n");
    let policies =
        PolicySet::from_str(&source).map_err(|err| AuthzError::CedarParse(err.to_string()))?;
    let schema = Schema::from_str(PLATFORM_SCHEMA_SOURCE).map_err(|err| {
        AuthzError::CedarValidation(format!("deploy/policies/zeroship.cedarschema: {err}"))
    })?;

    let result = Validator::new(schema.clone()).validate(&policies, ValidationMode::Strict);
    if !result.validation_passed_without_warnings() {
        let mut report = String::from("the shipped policy set does not validate:");
        for error in result.validation_errors() {
            write!(report, "\n  error: {error}").expect("writing to String is infallible");
        }
        for warning in result.validation_warnings() {
            write!(report, "\n  warning: {warning}").expect("writing to String is infallible");
        }
        return Err(AuthzError::CedarValidation(report));
    }

    Ok(PlatformPolicies { policies, schema })
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
            entries.sort_by_key(|(left, _)| *left);

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

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use cedar_policy::{PolicySet, Schema, ValidationMode, Validator};

    use super::{load_platform_policies, PLATFORM_SCHEMA_SOURCE};

    /// The shipped set validates STRICTLY, with no warnings. On its own this
    /// assertion is worthless - a validator wired to nothing passes too - so it
    /// is paired with `strict_validation_refuses_a_typod_action_id` below,
    /// which shows the same instrument going red.
    #[test]
    fn the_shipped_set_validates_strictly_with_no_warnings() {
        load_platform_policies().expect("the shipped policy set validates strictly");
    }

    /// An unknown action parses as Cedar but fails schema validation. The
    /// accepted control differs only in the action identifier.
    #[test]
    fn strict_validation_refuses_a_typod_action_id() {
        let schema = Schema::from_str(PLATFORM_SCHEMA_SOURCE).expect("schema parses");
        let validator = Validator::new(schema);
        let policy = |action: &str| {
            PolicySet::from_str(&format!(
                r#"permit (principal is User, action == Action::"{action}", resource is App);"#
            ))
            .expect("both action identifiers are syntactically valid Cedar")
        };
        let accepted = validator.validate(&policy("apps:read"), ValidationMode::Strict);
        assert!(accepted.validation_passed_without_warnings());

        let result = validator.validate(&policy("apps:raed"), ValidationMode::Strict);

        assert!(
            !result.validation_passed(),
            "strict validation accepted an action id no schema declares"
        );
        assert!(
            result
                .validation_errors()
                .any(|error| error.to_string().contains("apps:raed")),
            "the diagnostic must name the offending id"
        );
    }
}
