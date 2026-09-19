use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use cedar_policy::{Entities, Entity, EntityUid, RestrictedExpression};
use zeroship_id::UserId;

use crate::authority::Authority;
use crate::{AuthzError, Resource};

/// Assemble the Cedar entity store for one authorization request.
///
/// **The store no longer carries membership, and there is no cache in front of
/// it.** It holds exactly two entities: the principal `User`, whose attributes
/// come from `zeroship.users` alone, and the request's own resource, which
/// exists so `resource is App` / `is Database` / `is Project` /
/// `is Organization` has something to bind to.
///
/// The authority that decides the request rides in the request CONTEXT
/// ([`crate::eval`]), not here. Two properties follow, and both are the reason:
///
/// - **A missing membership denies at a comparison, auditably.** `build_context`
///   always supplies every key it declares, so a principal with no seat carries
///   rank zero and each band's `context.effective_rank >= N` evaluates to false.
///   Carrying the rank as an entity ATTRIBUTE would instead make Cedar SKIP any
///   policy dereferencing it, with an evaluation error that
///   `matched_policy_ids` never records - a deny indistinguishable from a clean
///   no-match.
/// - **The store stops growing with organization size.** The old shape
///   enumerated every app the principal could reach, per request, to build
///   `app_owner_of` / `app_editor_of` / `app_viewer_of` sets.
///
/// There is no parent hierarchy and no nested organizations, so
/// `Entities::from_entities`' duplicate-uid and transitive-closure failures -
/// each of which surfaces as a 500 on every request for the affected principal
/// rather than a Deny - are unreachable by construction rather than guarded.
///
/// # Errors
///
/// Returns [`AuthzError::CedarEntities`] when Cedar rejects a uid or the store.
pub fn assemble_entities(
    principal_id: &UserId,
    authority: &Authority,
    resource: &Resource,
) -> Result<Entities, AuthzError> {
    let entities = vec![
        user_entity(principal_id, authority)?,
        Entity::new_no_attrs(resource_entity_uid(resource)?, HashSet::new()),
    ];
    Entities::from_entities(entities, None)
        .map_err(|err| AuthzError::CedarEntities(err.to_string()))
}

fn user_entity(principal_id: &UserId, authority: &Authority) -> Result<Entity, AuthzError> {
    let attrs = HashMap::from([
        (
            "email_verified".to_owned(),
            restricted_bool(authority.email_verified)?,
        ),
        (
            "account_locked".to_owned(),
            restricted_bool(authority.account_locked)?,
        ),
    ]);
    Entity::new(uid("User", principal_id.as_str())?, attrs, HashSet::new())
        .map_err(|err| AuthzError::CedarEntities(err.to_string()))
}

/// The Cedar uid of a request's resource. ONE definition, used to build both
/// the entity and the request, so the store and the request can never name
/// different entities for the same resource.
///
/// # Errors
///
/// Returns [`AuthzError::CedarEntities`] when Cedar rejects the uid.
pub(crate) fn resource_entity_uid(resource: &Resource) -> Result<EntityUid, AuthzError> {
    match resource {
        Resource::App { id } => uid(resource.cedar_type(), id.as_str()),
        Resource::Database { id } => uid(resource.cedar_type(), id.as_str()),
        Resource::Project { id } | Resource::Organization { id } => uid(resource.cedar_type(), id),
        Resource::Any => uid(resource.cedar_type(), "*"),
    }
}

fn restricted_bool(value: bool) -> Result<RestrictedExpression, AuthzError> {
    RestrictedExpression::from_str(if value { "true" } else { "false" })
        .map_err(|err| AuthzError::CedarEntities(err.to_string()))
}

pub(crate) fn uid(entity_type: &str, id: &str) -> Result<EntityUid, AuthzError> {
    EntityUid::from_str(&format!("{entity_type}::{}", cedar_string(id)))
        .map_err(|err| AuthzError::CedarEntities(err.to_string()))
}

pub(crate) fn cedar_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }
    escaped.push('"');
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal() -> UserId {
        UserId::mint()
    }

    fn authority() -> Authority {
        Authority {
            email_verified: true,
            account_locked: false,
            effective_rank: 40,
            billing_rank: 20,
        }
    }

    /// The store is exactly two entities regardless of how much authority the
    /// principal holds. This is the property the membership sets used to break:
    /// it grew with the number of apps a principal could reach.
    #[test]
    fn the_store_is_bounded_at_two_entities() {
        for resource in [
            Resource::Any,
            Resource::App {
                id: zeroship_id::AppId::mint(),
            },
            Resource::Database {
                id: zeroship_id::DatabaseId::mint(),
            },
            Resource::Project {
                id: "prj_0000123456789abcdefghijkl".to_owned(),
            },
            Resource::Organization {
                id: "org_0000123456789abcdefghijkl".to_owned(),
            },
        ] {
            let entities = assemble_entities(&principal(), &authority(), &resource)
                .expect("entities should assemble");
            assert_eq!(entities.iter().count(), 2, "{resource:?}");
        }
    }

    /// Rank must NOT appear on the principal. If it ever does, a policy could
    /// be written against the attribute, and a principal missing it would be
    /// skipped with an unrecorded evaluation error instead of denied at a
    /// comparison.
    #[test]
    fn the_user_entity_carries_no_rank_attribute() {
        let principal = principal();
        let entities = assemble_entities(&principal, &authority(), &Resource::Any)
            .expect("entities should assemble");
        let user = entities
            .get(&uid("User", principal.as_str()).expect("uid"))
            .expect("user entity present");
        let json = user.to_json_value().expect("entity json").to_string();
        assert!(json.contains("email_verified"), "{json}");
        assert!(
            !json.contains("rank"),
            "rank must not ride on the principal: {json}"
        );
        assert!(!json.contains("app_owner_of"), "{json}");
    }
}
