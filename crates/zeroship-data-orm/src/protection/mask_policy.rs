//! App-declared mask policy, installed at startup and fixed for a deployment.
//!
//! `defineMaskPolicy()` supplies the policy through the framework-private
//! `__platform.setMaskPolicy` startup operation. Installation and authorization
//! use memory only; database drivers never store or load policies.
//!
//! Policies are keyed by the app-at-deploy binding so a new deployment cannot
//! change a policy used by an older isolate on the same worker thread.
//! Reinstallation of an identical declaration is allowed when an isolate is
//! recreated. A different declaration for the same binding is rejected.
//!
//! The reserved `auto` actor retains its fallback grant unless the declaration
//! explicitly restricts it. App-supplied reserved actors are sanitized before
//! authorization by `crate::protection::unmask::sanitize_app_actor`.

use std::collections::{HashMap, HashSet};

use zeroship_data_sql::value::Value;

use zeroship_data_orm::error::DbError;

use crate::binding::DbBinding;

/// The six canonical classification values. Mirrors the SDK's
/// `Classification` type (`sdks/db/src/types.ts`) and `crate::catalog::Classification`.
pub const VALID_CLASSIFICATIONS: &[&str] = &["public", "pii", "spi", "phi", "pci", "internal"];

/// Actor roles and the classifications they may explicitly unmask.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaskPolicy {
    /// `role -> set of classifications`. Roles missing from the map
    /// have no privileges (every classification request is denied).
    pub roles: HashMap<String, HashSet<String>>,
}

impl MaskPolicy {
    /// Empty policy — no role has any privilege. The default-deny stub
    /// still applies (only `auto` allowed) because
    /// [`Self::allows`] short-circuits on the `auto` role.
    #[must_use]
    #[cfg(test)]
    pub fn empty() -> Self {
        Self {
            roles: HashMap::new(),
        }
    }

    /// Check whether `role` is permitted to unmask `classification`.
    ///
    /// ### Rules
    ///
    /// 1. **`auto` fallback**: when `role == "auto"` AND the policy
    ///    does NOT explicitly list `auto`, return `true` regardless of
    ///    classification. System writes (migrations, background jobs)
    ///    always have uniform access unless the app explicitly
    ///    restricts them. To restrict the system actor, the policy
    ///    MUST list `auto` (with whatever set the app wants).
    ///
    /// 2. **Explicit role**: when the policy lists `role`, return
    ///    `true` iff `classification` is in the role's set.
    ///
    /// 3. **Unknown role**: deny.
    #[must_use]
    pub fn allows(&self, role: &str, classification: &str) -> bool {
        match self.roles.get(role) {
            Some(set) => set.contains(classification),
            None => {
                // The `auto` fallback rule — see method doc-comment.
                role == "auto"
            }
        }
    }

    /// Parse the app declaration captured from the SDK into a [`MaskPolicy`].
    ///
    /// Expected shape:
    /// ```json
    /// {
    ///   "admin":   ["public", "pii"],
    ///   "support": ["public"]
    /// }
    /// ```
    ///
    /// ### Errors
    ///
    /// - `invalid_mask_policy_shape` — the JSON is not an object, OR a
    ///   role value is not an array of strings.
    /// - `invalid_mask_classification` — a classification is not one of
    ///   the six built-ins.
    pub fn from_json(v: &Value) -> Result<Self, DbError> {
        let obj = v.as_object().ok_or_else(|| DbError::ValidationFailed {
            code: "invalid_mask_policy_shape",
            message: "mask policy: must be an object mapping role strings \
                      to arrays of classifications"
                .to_string(),
            hint: Some("shape: { \"<role>\": [\"<classification>\", ...], ... }".into()),
        })?;
        let mut roles: HashMap<String, HashSet<String>> = HashMap::new();
        for (role, classifications) in obj.iter() {
            let arr = classifications
                .as_array()
                .ok_or_else(|| DbError::ValidationFailed {
                    code: "invalid_mask_policy_shape",
                    message: format!(
                        "mask policy: role '{role}' must map to an array of \
                         classifications"
                    ),
                    hint: None,
                })?;
            let mut set: HashSet<String> = HashSet::with_capacity(arr.len());
            for c in arr {
                let s = c.as_str().ok_or_else(|| DbError::ValidationFailed {
                    code: "invalid_mask_classification",
                    message: format!(
                        "mask policy: role '{role}' includes a non-string \
                         classification value"
                    ),
                    hint: Some(
                        "classifications are strings (one of public, pii, spi, \
                         phi, pci, internal)"
                            .into(),
                    ),
                })?;
                if !VALID_CLASSIFICATIONS.contains(&s) {
                    return Err(DbError::ValidationFailed {
                        code: "invalid_mask_classification",
                        message: format!(
                            "mask policy: role '{role}' includes invalid \
                             classification '{s}'"
                        ),
                        hint: Some(format!(
                            "valid classifications: {}",
                            VALID_CLASSIFICATIONS.join(", ")
                        )),
                    });
                }
                set.insert(s.to_string());
            }
            roles.insert(role.to_string(), set);
        }
        Ok(Self { roles })
    }
}

/// Install the app's startup declaration without opening a database.
/// Recreated isolates may reinstall the same policy; runtime changes fail.
pub fn install_mask_policy(binding: &DbBinding, policy_v: Value) -> Result<(), DbError> {
    let policy = MaskPolicy::from_json(&policy_v)?;
    crate::orm_context::current().policies_mut(|policies| {
        if let Some(installed) = policies.get(binding) {
            if installed != &policy {
                return Err(DbError::validation(
                    "mask_policy_immutable",
                    "mask policy is fixed for this deployment; change the app declaration and redeploy",
                ));
            }
        } else {
            policies.insert(binding.clone(), policy);
        }
        Ok(())
    })
}

/// Read the startup policy, fixing the default when none was declared.
/// An authorization attempt cannot be followed by a new runtime declaration.
pub(crate) fn allows(binding: &DbBinding, role: &str, classification: &str) -> bool {
    crate::orm_context::current().policies_mut(|policies| match policies.get(binding) {
        Some(policy) => policy.allows(role, classification),
        None => {
            let policy = MaskPolicy::default();
            let allowed = policy.allows(role, classification);
            policies.insert(binding.clone(), policy);
            allowed
        }
    })
}

#[cfg(test)]
pub fn cache_get(binding: &DbBinding) -> Option<MaskPolicy> {
    crate::orm_context::current().policies(|m| m.get(binding).cloned())
}

/// Test-only fixture seeding and removal. Production installs immutable policy.
#[cfg(test)]
pub fn cache_put(binding: &DbBinding, policy: Option<MaskPolicy>) {
    crate::orm_context::current().policies_mut(|m| match policy {
        Some(p) => {
            m.insert(binding.clone(), p);
        }
        None => {
            m.remove(binding);
        }
    });
}

#[cfg(test)]
pub fn reset_for_tests() {
    crate::orm_context::current().policies_mut(HashMap::clear);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use zeroship_data_sql::value;

    #[test]
    fn installed_policy_is_immutable() {
        let binding = DbBinding::cold_start("app_fixed_policy");
        install_mask_policy(&binding, value!({ "support": ["public"] })).unwrap();
        install_mask_policy(&binding, value!({ "support": ["public"] })).unwrap();
        let error = install_mask_policy(&binding, value!({ "support": ["pii"] })).unwrap_err();
        assert!(matches!(
            error,
            DbError::ValidationFailed {
                code: "mask_policy_immutable",
                ..
            }
        ));
        assert!(allows(&binding, "support", "public"));
        assert!(!allows(&binding, "support", "pii"));
    }

    #[test]
    fn redeploy_does_not_change_an_older_isolates_policy() {
        let schema = zeroship_data_sql::SchemaName::new("app_policy_deploys").unwrap();
        let old = DbBinding::new("app_policy_deploys", "old", schema.clone());
        let new = DbBinding::new("app_policy_deploys", "new", schema);
        install_mask_policy(&old, value!({ "support": ["public"] })).unwrap();
        install_mask_policy(&new, value!({ "support": ["pii"] })).unwrap();
        assert!(!allows(&old, "support", "pii"));
        assert!(allows(&new, "support", "pii"));
        assert!(allows(&old, "support", "public"));
        assert!(!allows(&new, "support", "public"));
    }

    #[test]
    fn default_policy_cannot_be_replaced_after_authorization() {
        let binding = DbBinding::cold_start("app_default_policy");
        assert!(!allows(&binding, "support", "pii"));
        assert!(install_mask_policy(&binding, value!({ "support": ["pii"] })).is_err());
        assert!(!allows(&binding, "support", "pii"));
    }

    #[test]
    fn invalid_declaration_does_not_install_a_partial_policy() {
        let binding = DbBinding::cold_start("app_invalid_policy");
        assert!(install_mask_policy(&binding, value!({ "support": ["unknown"] })).is_err());
        assert!(cache_get(&binding).is_none());
        install_mask_policy(&binding, value!({ "support": ["pii"] })).unwrap();
        assert!(allows(&binding, "support", "pii"));
    }

    #[test]
    fn allows_grants_listed_classification() {
        let mut roles: HashMap<String, HashSet<String>> = HashMap::new();
        let mut admin_set = HashSet::new();
        admin_set.insert("pii".to_string());
        admin_set.insert("spi".to_string());
        roles.insert("admin".to_string(), admin_set);
        let p = MaskPolicy { roles };
        assert!(p.allows("admin", "pii"));
        assert!(p.allows("admin", "spi"));
    }

    #[test]
    fn allows_denies_unlisted_classification() {
        let mut roles: HashMap<String, HashSet<String>> = HashMap::new();
        let mut user_set = HashSet::new();
        user_set.insert("public".to_string());
        roles.insert("user".to_string(), user_set);
        let p = MaskPolicy { roles };
        assert!(p.allows("user", "public"));
        assert!(!p.allows("user", "pii"));
        assert!(!p.allows("user", "spi"));
    }

    #[test]
    fn allows_denies_unknown_role() {
        let p = MaskPolicy::empty();
        assert!(!p.allows("operator", "public"));
        assert!(!p.allows("ai-builder", "pii"));
        // Empty string role too.
        assert!(!p.allows("", "pii"));
    }

    #[test]
    fn auto_fallback_when_not_in_policy() {
        // Empty policy — auto still has uniform access.
        let p = MaskPolicy::empty();
        assert!(p.allows("auto", "pii"));
        assert!(p.allows("auto", "spi"));
        assert!(p.allows("auto", "internal"));
    }

    #[test]
    fn auto_fallback_when_other_roles_in_policy() {
        // Policy lists a non-auto role; auto is still uniform.
        let mut roles: HashMap<String, HashSet<String>> = HashMap::new();
        roles.insert("user".to_string(), HashSet::from(["public".to_string()]));
        let p = MaskPolicy { roles };
        assert!(p.allows("auto", "pii"));
        assert!(p.allows("auto", "phi"));
    }

    #[test]
    fn auto_explicit_restriction_overrides_fallback() {
        // Policy lists `auto` with a restricted set — fallback DOES NOT
        // apply. The app is explicitly saying "even the system actor
        // is restricted".
        let mut roles: HashMap<String, HashSet<String>> = HashMap::new();
        roles.insert("auto".to_string(), HashSet::from(["public".to_string()]));
        let p = MaskPolicy { roles };
        assert!(p.allows("auto", "public"));
        assert!(!p.allows("auto", "pii"));
        assert!(!p.allows("auto", "spi"));
    }

    #[test]
    fn auto_explicit_empty_set_denies_everything() {
        // Policy lists `auto` with an EMPTY set — the system actor is
        // fully restricted. The fallback's "auto always allowed" does
        // not apply because the role IS listed.
        let mut roles: HashMap<String, HashSet<String>> = HashMap::new();
        roles.insert("auto".to_string(), HashSet::new());
        let p = MaskPolicy { roles };
        assert!(!p.allows("auto", "public"));
        assert!(!p.allows("auto", "pii"));
    }

    #[test]
    fn from_json_parses_well_formed_policy() {
        let v = value!({
            "admin":   ["public", "pii", "spi"],
            "support": ["public", "pii"],
            "user":    ["public"],
        });
        let p = MaskPolicy::from_json(&v).expect("parse");
        assert!(p.allows("admin", "pii"));
        assert!(p.allows("admin", "spi"));
        assert!(!p.allows("admin", "phi"));
        assert!(p.allows("support", "pii"));
        assert!(!p.allows("support", "spi"));
        assert!(p.allows("user", "public"));
        assert!(!p.allows("user", "pii"));
    }

    #[test]
    fn from_json_rejects_non_object() {
        let v = value!("not an object");
        let err = MaskPolicy::from_json(&v).unwrap_err();
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "invalid_mask_policy_shape");
            }
            other => panic!("expected invalid_mask_policy_shape, got {other:?}"),
        }
    }

    #[test]
    fn from_json_rejects_non_array_value() {
        let v = value!({ "admin": "pii" });
        let err = MaskPolicy::from_json(&v).unwrap_err();
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "invalid_mask_policy_shape");
            }
            other => panic!("expected invalid_mask_policy_shape, got {other:?}"),
        }
    }

    #[test]
    fn from_json_rejects_unknown_classification() {
        let v = value!({ "admin": ["public", "badclass"] });
        let err = MaskPolicy::from_json(&v).unwrap_err();
        match err {
            DbError::ValidationFailed { code, message, .. } => {
                assert_eq!(code, "invalid_mask_classification");
                assert!(
                    message.contains("badclass"),
                    "diagnostic must include the bad value: {message}"
                );
            }
            other => panic!("expected invalid_mask_classification, got {other:?}"),
        }
    }

    #[test]
    fn from_json_rejects_non_string_classification() {
        let v = value!({ "admin": [42] });
        let err = MaskPolicy::from_json(&v).unwrap_err();
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "invalid_mask_classification");
            }
            other => panic!("expected invalid_mask_classification, got {other:?}"),
        }
    }

    #[test]
    fn from_json_accepts_all_six_classifications() {
        let v = value!({
            "admin": ["public", "pii", "spi", "phi", "pci", "internal"],
        });
        let p = MaskPolicy::from_json(&v).expect("parse");
        for c in VALID_CLASSIFICATIONS {
            assert!(p.allows("admin", c), "admin must allow {c}");
        }
    }

    #[test]
    fn from_json_empty_array_yields_empty_set() {
        let v = value!({ "auto": [] });
        let p = MaskPolicy::from_json(&v).expect("parse");
        // Even though auto is listed with an empty set, the explicit
        // listing means the fallback rule does NOT apply.
        assert!(!p.allows("auto", "pii"));
    }

    #[test]
    fn valid_classifications_constant_matches_taxonomy() {
        // Pinning the canonical six against the SDK list. A drift
        // (e.g. a 7th value added on one side without the other)
        // breaks compile here.
        let expected = ["public", "pii", "spi", "phi", "pci", "internal"];
        assert_eq!(VALID_CLASSIFICATIONS.len(), expected.len());
        for c in expected {
            assert!(
                VALID_CLASSIFICATIONS.contains(&c),
                "missing classification: {c}"
            );
        }
    }

    // -----------------------------------------------------------------
    // §11 closeout: `mask_policy_per_app_isolated`
    //
    // The proposal asserts that a `defineMaskPolicy()` write under
    // app A's isolate-context entry must NOT be visible from app B's
    // entry. The cache is keyed by binding in [`MASK_POLICIES`]; this test pins that
    // invariant directly through the public surface so a future
    // refactor that accidentally widens the key (e.g. to a shared
    // singleton) trips the gate.
    // -----------------------------------------------------------------

    #[test]
    fn mask_policy_per_app_isolated() {
        // Build two distinct policies — one permissive for `admin`,
        // one restrictive for `support` — and seed them under
        // different app ids.
        let mut admin_set: HashSet<String> = HashSet::new();
        admin_set.insert("pii".to_string());
        admin_set.insert("spi".to_string());
        admin_set.insert("phi".to_string());
        let mut roles_a: HashMap<String, HashSet<String>> = HashMap::new();
        roles_a.insert("admin".to_string(), admin_set);
        let policy_a = MaskPolicy { roles: roles_a };

        let mut support_set: HashSet<String> = HashSet::new();
        support_set.insert("public".to_string());
        let mut roles_b: HashMap<String, HashSet<String>> = HashMap::new();
        roles_b.insert("support".to_string(), support_set);
        let policy_b = MaskPolicy { roles: roles_b };

        // Drives the thread-local cache directly. It used to build a private
        // `ThreadDbContext` instead, to "avoid touching the thread-local so
        // test ordering is irrelevant" - but ordering was never the hazard it
        // implied: libtest gives every `#[test]` its own OS thread even under
        // `--test-threads=1` (measured 2026-09-01), so no other test can
        // observe or disturb these entries.
        cache_put(&DbBinding::cold_start("app_a"), Some(policy_a.clone()));
        cache_put(&DbBinding::cold_start("app_b"), Some(policy_b.clone()));

        // App A sees policy A only — admin grants are visible; the
        // app-B `support` role is not in the cache for app A.
        let a = cache_get(&DbBinding::cold_start("app_a")).expect("app_a cached");
        assert!(a.allows("admin", "pii"));
        assert!(a.allows("admin", "spi"));
        assert!(
            !a.allows("support", "public"),
            "app_b's role must not leak into app_a"
        );

        // App B sees policy B only — support grants are visible; the
        // app-A `admin` role is not in the cache for app B.
        let b = cache_get(&DbBinding::cold_start("app_b")).expect("app_b cached");
        assert!(b.allows("support", "public"));
        assert!(
            !b.allows("admin", "pii"),
            "app_a's role must not leak into app_b"
        );
        assert!(
            !b.allows("admin", "spi"),
            "app_a's role must not leak into app_b"
        );

        // App C — never seeded — sees nothing.
        assert!(cache_get(&DbBinding::cold_start("app_c")).is_none());

        // Clearing app A leaves app B intact (fence against a clear-
        // implementation that walks the whole map).
        cache_put(&DbBinding::cold_start("app_a"), None);
        assert!(cache_get(&DbBinding::cold_start("app_a")).is_none());
        assert!(cache_get(&DbBinding::cold_start("app_b")).is_some());
    }
}
