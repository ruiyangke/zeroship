use std::str::FromStr;

use cedar_policy::{Decision, Entities, PolicySet, Request, Schema};
use sha2::{Digest, Sha256};

use crate::{lower, AuthzError, Policy};

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
/// `deploy/policies/` and parse-checks every `.cedar` file it finds, but it
/// LOADS none of them - so a policy file that is committed, reviewed and absent
/// from this array is syntactically valid and authorizes exactly nothing. A
/// crate test reconciles the two: every `.cedar` file on disk must appear here.
const PLATFORM_POLICY_SOURCES: &[&str] = &[
    include_str!("../../../deploy/policies/platform/self_service.cedar"),
    include_str!("../../../deploy/policies/creator/organization_read.cedar"),
    include_str!("../../../deploy/policies/creator/organization_develop.cedar"),
    include_str!("../../../deploy/policies/creator/organization_administer.cedar"),
    include_str!("../../../deploy/policies/creator/organization_own.cedar"),
    include_str!("../../../deploy/policies/creator/organization_billing.cedar"),
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

/// Loads the static platform and creator Cedar policies embedded in this crate.
///
/// # Errors
///
/// Returns [`AuthzError::CedarParse`] when Cedar rejects the embedded policy
/// sources.
pub fn load_platform_policies() -> Result<PolicySet, AuthzError> {
    let source = PLATFORM_POLICY_SOURCES.join("\n");
    PolicySet::from_str(&source).map_err(|err| AuthzError::CedarParse(err.to_string()))
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
    use super::{load_platform_policies, policy_files_on_disk, PLATFORM_POLICY_SOURCES};

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
        let policies = load_platform_policies().expect("bundled policies parse");
        let statements: usize = PLATFORM_POLICY_SOURCES
            .iter()
            .map(|source| source.matches("permit (").count())
            .sum();
        assert!(statements >= 2, "ruled on {statements} permit statements");
        assert_eq!(policies.policies().count(), statements);
    }
}
