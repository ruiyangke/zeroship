use serde::{Deserialize, Serialize};
use zeroship_core::app_id::AppId;

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
/// **Every id here is an opaque typed id** (`app_...`, `prj_...`, `org_...`),
/// never the slug. A slug is renameable, and a policy or an audit row that
/// referred to one would change meaning under a rename.
///
/// [`Resource::App`] carries an [`AppId`] rather than a `String`, so the
/// rendering is settled by the type rather than re-checked by whoever reads it.
/// That matters because this id is what [`crate::authority::resolve`] KEYS A
/// ROW WITH: a second rendering reaching that join does not error, it matches no
/// app, and the caller is told they hold no seat. A `String` field would keep
/// accepting one forever, and `#[derive(Deserialize)]` would keep accepting one
/// off the wire; `AppId`'s decode is its `parse`, so a wrapper policy naming an
/// app in any other spelling is a decode failure.
///
/// The other two ids are still `String` - their typed-id sweep is a separate
/// change with its own consumers - so for them [`Resource::validate_ids`] is
/// what closes the alphabet.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Resource {
    App { id: AppId },
    Project { id: String },
    Organization { id: String },
    Any,
}

impl Resource {
    #[must_use]
    pub fn cedar_uid(&self) -> String {
        match self {
            Self::App { id } => format!("App::{}", cedar_string(id.as_str())),
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
    /// [`Resource::App`] passes unconditionally, and that is NOT the catch-all
    /// arm returning: an [`AppId`] is reachable only through `mint` or `parse`,
    /// so `app_<base62>` is the only text it can hold and the alphabet is closed
    /// at construction instead of here. It is written as its own arm rather than
    /// folded in with the two `String` ids so that the difference is visible -
    /// and so the next id to gain a type moves an arm rather than deleting a
    /// check.
    ///
    /// # Errors
    ///
    /// Returns the reason string when a `String`-typed id is empty or leaves the
    /// closed alphabet.
    pub fn validate_ids(&self) -> Result<(), &'static str> {
        match self {
            Self::App { .. } | Self::Any => Ok(()),
            Self::Project { id } | Self::Organization { id } => {
                if is_valid_resource_id(id) {
                    Ok(())
                } else {
                    Err("resource id must use only ASCII letters, digits, '_' or '-'")
                }
            }
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
    use super::{is_valid_resource_id, Resource};
    use zeroship_core::app_id::AppId;

    const HOSTILE: &str = "x\"; permit (principal, action, resource);";

    /// Each id-bearing variant refuses a Cedar-breaking id, and the two halves
    /// refuse it in different places. The failure this pins is the one the
    /// catch-all arm used to allow: a variant that carries an id and checks
    /// nothing.
    ///
    /// For the `String` ids the refusal is [`Resource::validate_ids`]. For
    /// `App` there is no such call to make, because the value cannot be built:
    /// the hostile text is refused by [`AppId::parse`], which is the same
    /// refusal one step earlier. Asserting the parse here rather than dropping
    /// the case keeps `App` in this test - a variant silently absent from an
    /// "every variant" list is exactly what the catch-all arm used to be.
    #[test]
    fn every_id_bearing_variant_rejects_a_cedar_string_break() {
        assert!(
            AppId::parse(HOSTILE).is_err(),
            "a Cedar-breaking app id must not be constructible"
        );
        for resource in [
            Resource::Project {
                id: HOSTILE.to_owned(),
            },
            Resource::Organization {
                id: HOSTILE.to_owned(),
            },
        ] {
            assert!(
                resource.validate_ids().is_err(),
                "{resource:?} must refuse a Cedar-breaking id"
            );
        }
        assert!(Resource::Any.validate_ids().is_ok());
    }

    /// The canonical renderings all sit inside the closed alphabet.
    ///
    /// The `App` case is the one worth stating: `validate_ids` returns `Ok` for
    /// it whatever it holds, so the arm alone proves nothing. What is asserted
    /// is the property that makes the arm safe - a MINTED id's printed form
    /// passes the same alphabet the two `String` ids are held to, so typing the
    /// field widened nothing.
    #[test]
    fn typed_ids_pass_the_alphabet() {
        let app = AppId::mint();
        assert!(
            is_valid_resource_id(app.as_str()),
            "{} left the closed resource alphabet",
            app.as_str()
        );
        for resource in [
            Resource::Organization {
                id: "org_0123456789abcdefghijkl".to_owned(),
            },
            Resource::Project {
                id: "prj_0123456789abcdefghijkl".to_owned(),
            },
            Resource::App { id: app },
        ] {
            assert!(resource.validate_ids().is_ok(), "{resource:?}");
        }
    }

    #[test]
    fn cedar_type_matches_the_uid_prefix() {
        for resource in [
            Resource::App { id: AppId::mint() },
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

    /// An app id off the wire goes through `AppId::parse`, so a wrapper policy
    /// naming an app in any other rendering is a DECODE failure rather than a
    /// resource that resolves to nothing. The paired control is the canonical
    /// rendering, which must still decode - otherwise this asserts only that
    /// the field is hard to fill.
    #[test]
    fn a_non_canonical_app_id_does_not_deserialize() {
        let canonical = AppId::mint();
        let ok: Resource = serde_json::from_value(serde_json::json!({
            "type": "app",
            "id": canonical.as_str(),
        }))
        .expect("the canonical rendering must decode");
        assert_eq!(ok, Resource::App { id: canonical });

        for rejected in [
            "",
            "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0",
            "blog",
            "not-a-uuid",
            "prj_0123456789abcdefghijkl",
            HOSTILE,
        ] {
            assert!(
                serde_json::from_value::<Resource>(serde_json::json!({
                    "type": "app",
                    "id": rejected,
                }))
                .is_err(),
                "{rejected:?} must not decode as an app resource"
            );
        }
    }
}
