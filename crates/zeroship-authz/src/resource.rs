use serde::{Deserialize, Serialize};
use zeroship_id::{AppId, DatabaseId};

use crate::entities::cedar_string;

/// What an authorization request is ABOUT.
///
/// An organization owns projects; a project owns both apps and databases; and
/// `Any` is the synthetic platform-level resource for surfaces that name
/// nothing (create an organization, list the ones you belong to, read your own
/// account).
///
/// [`Resource::Database`] hangs off the PROJECT, not off an app. A database
/// outlives the app that first used it and may be reached by several, so the
/// authority over it is the project seat and there is no app indirection to
/// walk. That is why no band names a database action at `resource is App`.
///
/// **Every id here is an opaque typed id** (`app_...`, `dbs_...`, `prj_...`,
/// `org_...`), never the slug or the display name. A name is renameable, and a
/// policy or an audit row that referred to one would change meaning under a
/// rename.
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
/// [`Resource::Database`] carries a [`DatabaseId`] for the same reason and with
/// the same consequence one step further along: the resolve reaches
/// `zeroship.databases` to find the OWNING PROJECT, so a second rendering would
/// join no row, produce no project, and deny an owner on their own database.
///
/// The other two ids are still `String` - their typed-id sweep is a separate
/// change with its own consumers - so for them [`Resource::validate_ids`] is
/// what closes the alphabet.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Resource {
    App { id: AppId },
    Database { id: DatabaseId },
    Project { id: String },
    Organization { id: String },
    Any,
}

impl Resource {
    #[must_use]
    pub fn cedar_uid(&self) -> String {
        match self {
            Self::App { id } => format!("App::{}", cedar_string(id.as_str())),
            Self::Database { id } => format!("Database::{}", cedar_string(id.as_str())),
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
            Self::Database { .. } => "Database",
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
    /// [`Resource::App`] and [`Resource::Database`] pass unconditionally, and
    /// that is NOT the catch-all arm returning: an [`AppId`] and a
    /// [`DatabaseId`] are each reachable only through `mint` or `parse`, so
    /// `app_<base36>` and `dbs_<base36>` are the only text they can hold and
    /// the alphabet is closed at construction instead of here. They are written
    /// as their own arm rather than folded in with the two `String` ids so that
    /// the difference is visible - and so the next id to gain a type moves an
    /// arm rather than deleting a check.
    ///
    /// # Errors
    ///
    /// Returns the reason string when a `String`-typed id is empty or leaves the
    /// closed alphabet.
    pub fn validate_ids(&self) -> Result<(), &'static str> {
        match self {
            Self::App { .. } | Self::Database { .. } | Self::Any => Ok(()),
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
    use zeroship_id::{AppId, DatabaseId};

    const HOSTILE: &str = "x\"; permit (principal, action, resource);";

    /// Each id-bearing variant refuses a Cedar-breaking id, and the two halves
    /// refuse it in different places.
    ///
    /// For the `String` ids the refusal is [`Resource::validate_ids`]. For
    /// `App` and `Database` there is no such call to make, because the value
    /// cannot be built: the hostile text is refused by [`AppId::parse`] and
    /// [`DatabaseId::parse`], which is the same refusal one step earlier.
    /// Asserting the parses here keeps both typed variants in this "every
    /// variant" list, so a variant cannot be silently absent from it.
    #[test]
    fn every_id_bearing_variant_rejects_a_cedar_string_break() {
        assert!(
            AppId::parse(HOSTILE).is_err(),
            "a Cedar-breaking app id must not be constructible"
        );
        assert!(
            DatabaseId::parse(HOSTILE).is_err(),
            "a Cedar-breaking database id must not be constructible"
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
    /// The `App` and `Database` cases are the ones worth stating:
    /// `validate_ids` returns `Ok` for them whatever they hold, so the arm
    /// alone proves nothing. What is asserted is the property that makes the
    /// arm safe - a MINTED id's printed form passes the same alphabet the two
    /// `String` ids are held to, so typing the field widened nothing.
    #[test]
    fn typed_ids_pass_the_alphabet() {
        let app = AppId::mint();
        let database = DatabaseId::mint();
        for typed in [app.as_str(), database.as_str()] {
            assert!(
                is_valid_resource_id(typed),
                "{typed} left the closed resource alphabet"
            );
        }
        for resource in [
            Resource::Organization {
                id: "org_0000123456789abcdefghijkl".to_owned(),
            },
            Resource::Project {
                id: "prj_0000123456789abcdefghijkl".to_owned(),
            },
            Resource::App { id: app },
            Resource::Database { id: database },
        ] {
            assert!(resource.validate_ids().is_ok(), "{resource:?}");
        }
    }

    #[test]
    fn cedar_type_matches_the_uid_prefix() {
        for resource in [
            Resource::App { id: AppId::mint() },
            Resource::Database {
                id: DatabaseId::mint(),
            },
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
            "prj_0000123456789abcdefghijkl",
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

    /// The same contract for a database, and it is the one that carries the
    /// consequence furthest. A database id off the wire goes through
    /// `DatabaseId::parse`, so a wrapper policy naming a database in any other
    /// rendering is a DECODE failure - not a resource that reaches
    /// `authority::resolve`, joins no `zeroship.databases` row, resolves to no
    /// project and denies its own owner with an audit row that reads like an
    /// honest non-match.
    ///
    /// The rejected set includes an APP id: the two prefixes are the whole
    /// difference between the two typed variants, so a rendering that crossed
    /// them would be a resource of the wrong kind that still decoded. The
    /// paired control is the canonical rendering, which must still decode.
    #[test]
    fn a_non_canonical_database_id_does_not_deserialize() {
        let canonical = DatabaseId::mint();
        let ok: Resource = serde_json::from_value(serde_json::json!({
            "type": "database",
            "id": canonical.as_str(),
        }))
        .expect("the canonical rendering must decode");
        assert_eq!(ok, Resource::Database { id: canonical });

        let app = AppId::mint();
        for rejected in [
            "",
            "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0",
            "main",
            "not-a-uuid",
            "prj_0000123456789abcdefghijkl",
            app.as_str(),
            HOSTILE,
        ] {
            assert!(
                serde_json::from_value::<Resource>(serde_json::json!({
                    "type": "database",
                    "id": rejected,
                }))
                .is_err(),
                "{rejected:?} must not decode as a database resource"
            );
        }
    }
}
