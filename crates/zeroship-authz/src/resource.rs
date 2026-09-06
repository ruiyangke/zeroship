use serde::{Deserialize, Serialize};

use crate::entities::cedar_string;

/// What an authorization request is ABOUT.
///
/// There are four variants and they sit on one chain: an organization owns
/// projects, a project owns apps, and `Any` is the synthetic platform-level
/// resource for surfaces that name nothing (create an organization, list the
/// ones you belong to, read your own account).
///
/// A previous `Org` variant was deleted because it existed for exactly one
/// caller - an operator gate probing a synthetic `Org::"zeroship_platform"` that
/// no migration ever inserted, satisfiable only by a universal-allow policy that
/// is also gone. The condition that file recorded for reviving it was
/// "introducing real organizations, with rows and membership, rather than a
/// sentinel id". [`Resource::Organization`] meets it: every id names a row in
/// `zeroship.organizations`, and authority over it is resolved from
/// `zeroship.organization_members` per request.
///
/// **Organization and project ids are the opaque typed ids** (`org_...`,
/// `prj_...`), never the slug. A slug is renameable, and a policy or an audit
/// row that referred to one would change meaning under a rename. `App` still
/// carries the app's uuid in string form, because `zeroship.apps` still keys on
/// uuid.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Resource {
    App { id: String },
    Project { id: String },
    Organization { id: String },
    Any,
}

impl Resource {
    #[must_use]
    pub fn cedar_uid(&self) -> String {
        match self {
            Self::App { id } => format!("App::{}", cedar_string(id)),
            Self::Project { id } => format!("Project::{}", cedar_string(id)),
            Self::Organization { id } => format!("Organization::{}", cedar_string(id)),
            Self::Any => "*".to_owned(),
        }
    }

    /// The Cedar entity TYPE name. The policy bands discriminate on this in
    /// their SCOPE (`resource is App`), never in their body, so an
    /// organization-scoped action cannot be satisfied by an App-typed probe.
    #[must_use]
    pub const fn cedar_type(&self) -> &'static str {
        match self {
            Self::App { .. } => "App",
            Self::Project { .. } => "Project",
            Self::Organization { .. } => "Organization",
            Self::Any => "Resource",
        }
    }

    /// Validate resource IDs before they reach Cedar source lowering.
    ///
    /// A resource id is a platform identifier, not an arbitrary Cedar string.
    /// Keeping the alphabet closed prevents source-level ambiguity in wrapper
    /// policies and fails bad HTTP path/input values before authorization.
    ///
    /// **This match is EXHAUSTIVE on purpose.** It used to end in `_ => Ok(())`,
    /// which meant a new variant compiled silently and validated nothing -
    /// while every other `Resource` match in the crate would have forced an arm.
    /// Writing `Self::Any` out makes the NEXT variant a compile error here,
    /// which is the whole point.
    ///
    /// # Errors
    ///
    /// Returns the reason string when the id is empty or leaves the closed
    /// alphabet.
    pub fn validate_ids(&self) -> Result<(), &'static str> {
        match self {
            Self::App { id } | Self::Project { id } | Self::Organization { id } => {
                if is_valid_resource_id(id) {
                    Ok(())
                } else {
                    Err("resource id must use only ASCII letters, digits, '_' or '-'")
                }
            }
            Self::Any => Ok(()),
        }
    }
}

#[must_use]
pub fn is_valid_resource_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::Resource;

    /// Each variant validates its own id. The failure this pins is the one the
    /// catch-all arm used to allow: a variant that carries an id and checks
    /// nothing.
    #[test]
    fn every_id_bearing_variant_rejects_a_cedar_string_break() {
        let hostile = "x\"; permit (principal, action, resource);".to_owned();
        for resource in [
            Resource::App {
                id: hostile.clone(),
            },
            Resource::Project {
                id: hostile.clone(),
            },
            Resource::Organization { id: hostile },
        ] {
            assert!(
                resource.validate_ids().is_err(),
                "{resource:?} must refuse a Cedar-breaking id"
            );
        }
        assert!(Resource::Any.validate_ids().is_ok());
    }

    #[test]
    fn typed_ids_pass_the_alphabet() {
        for resource in [
            Resource::Organization {
                id: "org_0123456789abcdefghijkl".to_owned(),
            },
            Resource::Project {
                id: "prj_0123456789abcdefghijkl".to_owned(),
            },
            Resource::App {
                id: "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0".to_owned(),
            },
        ] {
            assert!(resource.validate_ids().is_ok(), "{resource:?}");
        }
    }

    #[test]
    fn cedar_type_matches_the_uid_prefix() {
        for resource in [
            Resource::App { id: "a".to_owned() },
            Resource::Project { id: "p".to_owned() },
            Resource::Organization { id: "o".to_owned() },
        ] {
            let uid = resource.cedar_uid();
            assert!(
                uid.starts_with(&format!("{}::", resource.cedar_type())),
                "{uid} does not lead with {}",
                resource.cedar_type()
            );
        }
        assert_eq!(Resource::Any.cedar_type(), "Resource");
    }
}
