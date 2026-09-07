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
/// The three `app_members`-bound files (`app_owner` / `app_editor` /
/// `app_viewer`) are deleted with the table that backed them.
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

/// Every `.cedar` file under `deploy/policies/`, found by walking the tree the
/// same way `build.rs` does.
#[cfg(test)]
fn policy_files_on_disk() -> Vec<std::path::PathBuf> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read policies directory") {
            let path = entry.expect("read policies directory entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "cedar") {
                out.push(path);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/policies");
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::str::FromStr;

    use cedar_policy::{PolicySet, Schema, ValidationMode, Validator};

    use super::{
        load_platform_policies, policy_files_on_disk, PLATFORM_POLICY_SOURCES,
        PLATFORM_SCHEMA_SOURCE,
    };
    use crate::Action;

    /// `build.rs` parse-checks the policy directory and LOADS nothing, so a
    /// committed `.cedar` file missing from `PLATFORM_POLICY_SOURCES` is valid
    /// Cedar that authorizes nothing - and the failure is invisible: the build
    /// is green, the review passes, and every request that policy was written
    /// for is denied with no matched policy to name.
    ///
    /// This arm rules on every `.cedar` file on disk. The floor is 2: one
    /// platform baseline and at least one creator band. A run that found fewer
    /// found the wrong directory.
    #[test]
    fn every_policy_file_on_disk_is_wired_into_the_loaded_set() {
        let files = policy_files_on_disk();
        assert!(
            files.len() >= 2,
            "ruled on {} policy files, expected at least 2 - the walk found the wrong directory",
            files.len()
        );
        for path in &files {
            let source = std::fs::read_to_string(path).expect("read .cedar file");
            assert!(
                PLATFORM_POLICY_SOURCES.contains(&source.as_str()),
                "{} is committed but absent from PLATFORM_POLICY_SOURCES, so it authorizes nothing",
                path.display()
            );
        }
        assert_eq!(
            files.len(),
            PLATFORM_POLICY_SOURCES.len(),
            "PLATFORM_POLICY_SOURCES names an entry with no file on disk"
        );
    }

    /// The loaded set must parse as one Cedar policy set, and every statement
    /// in it must survive as a distinct policy. A file whose statements
    /// collapsed would silently drop a band.
    #[test]
    fn the_loaded_set_parses_into_one_policy_per_statement() {
        let loaded = load_platform_policies().expect("bundled policies parse");
        let statements: usize = PLATFORM_POLICY_SOURCES
            .iter()
            .map(|source| source.matches("permit (").count())
            .sum();
        assert!(statements >= 2, "ruled on {statements} permit statements");
        assert_eq!(loaded.policies().policies().count(), statements);
    }

    /// The shipped set validates STRICTLY, with no warnings. On its own this
    /// assertion is worthless - a validator wired to nothing passes too - so it
    /// is paired with `strict_validation_refuses_a_typod_action_id` below,
    /// which shows the same instrument going red.
    #[test]
    fn the_shipped_set_validates_strictly_with_no_warnings() {
        load_platform_policies().expect("the shipped policy set validates strictly");
    }

    /// The mutation proof for the assertion above, and the failure the whole
    /// schema exists for: `PolicySet::from_str` ACCEPTS a typo'd action id
    /// without complaint, so before the schema this defect produced no
    /// diagnostic anywhere and denied every request the band was written to
    /// permit.
    ///
    /// Both halves are asserted in one test on purpose - the parse succeeding
    /// is what makes the validation failure meaningful.
    #[test]
    fn strict_validation_refuses_a_typod_action_id() {
        let source = PLATFORM_POLICY_SOURCES
            .join("\n")
            .replace(r#"Action::"apps:read""#, r#"Action::"apps:raed""#);
        assert!(
            source.contains(r#"Action::"apps:raed""#),
            "the mutation did not apply, so nothing below is a measurement"
        );

        let policies = PolicySet::from_str(&source)
            .expect("Cedar's parser accepts an unknown action id - that is the defect");
        let schema = Schema::from_str(PLATFORM_SCHEMA_SOURCE).expect("schema parses");
        let result = Validator::new(schema).validate(&policies, ValidationMode::Strict);

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

    /// The schema's action list and [`Action::all`] must be the same set, in
    /// both directions.
    ///
    /// **STRICT VALIDATION CANNOT SEE THIS GAP.** It rules only on the ids the
    /// POLICIES quote, and `migrations:approve` is quoted by no band by design
    /// while `ControlPlaneAuthenticator::authorize` issues it at a real
    /// `Resource::App` on every migration go-live. Drop it from the schema and
    /// validation stays green while that route starts returning 500 - because
    /// `Request::new` refuses an action the schema does not declare, and the
    /// refusal fires before `audit_decision`, so there is no durable record
    /// either.
    #[test]
    fn the_schema_declares_exactly_the_action_vocabulary() {
        let declared: HashSet<&str> = PLATFORM_SCHEMA_SOURCE
            .lines()
            .filter_map(|line| line.trim().strip_prefix("action \""))
            .filter_map(|rest| rest.split('"').next())
            .collect();
        assert!(
            declared.len() >= 2,
            "ruled on {} declared action(s) - the extraction found nothing",
            declared.len()
        );

        let vocabulary: HashSet<&str> = Action::all().iter().map(Action::cedar_id).collect();
        assert_eq!(
            declared, vocabulary,
            "the schema's action list and Action::all() have diverged"
        );
    }
}
