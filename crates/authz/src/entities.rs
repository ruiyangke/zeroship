use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use cedar_policy::{Entities, Entity, EntityUid, RestrictedExpression};
use compio_postgres::Client;
use lru::LruCache;
use uuid::Uuid;

use crate::{Action, AuthzError, Resource};

const ENTITY_CACHE_TTL: Duration = Duration::from_secs(30);
const ENTITY_CACHE_CAPACITY: usize = 1024;

/// Platform role assigned to a principal with no row in
/// `platform_admin_roles`. This is the *default* for every ordinary creator.
///
/// It MUST be a true zero-privilege role: no platform Cedar policy matches it,
/// so an un-roled creator is authorized solely through their `app_members`-bound
/// creator policies (app_owner / app_editor / app_viewer). Using a privileged
/// default such as `"readonly"` here is a fleet-wide cross-tenant read IDOR,
/// because `readonly.cedar` permits reads on an unconstrained resource.
pub const DEFAULT_PLATFORM_ROLE: &str = "none";

static ENTITY_CACHE: LazyLock<Mutex<LruCache<EntityCacheKey, CacheEntry>>> =
    LazyLock::new(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(ENTITY_CACHE_CAPACITY).expect("non-zero cache capacity"),
        ))
    });

fn lock_entity_cache() -> MutexGuard<'static, LruCache<EntityCacheKey, CacheEntry>> {
    ENTITY_CACHE.lock().unwrap_or_else(|poisoned| {
        tracing::error!("authz entity cache mutex poisoned; recovering cache");
        poisoned.into_inner()
    })
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct EntityCacheKey {
    principal_id: Uuid,
    resource_key: String,
}

#[derive(Clone)]
struct CacheEntry {
    inserted_at: Instant,
    entities: Entities,
}

#[derive(Debug)]
pub struct EntityCache;

impl EntityCache {
    pub fn invalidate(principal_id: Uuid) {
        let mut cache = lock_entity_cache();
        let keys = cache
            .iter()
            .filter(|(key, _)| key.principal_id == principal_id)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in keys {
            cache.pop(&key);
        }
    }

    pub fn invalidate_resource(resource: &Resource) {
        let resource_key = resource_cache_key(resource);
        let mut cache = lock_entity_cache();
        let keys = cache
            .iter()
            .filter(|(key, _)| key.resource_key == resource_key)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in keys {
            cache.pop(&key);
        }
    }
}

#[derive(Default)]
struct Memberships {
    owner: Vec<String>,
    editor: Vec<String>,
    viewer: Vec<String>,
}

#[derive(Clone, Copy, Default)]
struct AppFlags {
    suspended: bool,
    audit_locked: bool,
}

/// Assemble Cedar entities for a principal/resource authorization request.
///
/// The store contains the principal `User`, all app membership targets, the
/// requested resource entity, and the P11 placeholder org when needed.
pub async fn assemble_entities(
    pg: &Client,
    principal_id: Uuid,
    _action: Action,
    resource: Resource,
) -> Result<Entities, AuthzError> {
    let key = EntityCacheKey {
        principal_id,
        resource_key: resource_cache_key(&resource),
    };

    if let Some(entities) = cache_get(&key) {
        return Ok(entities);
    }

    let user = load_user(pg, principal_id).await?;
    let memberships = load_memberships(pg, principal_id).await?;
    let mut app_ids = memberships
        .owner
        .iter()
        .chain(memberships.editor.iter())
        .chain(memberships.viewer.iter())
        .cloned()
        .collect::<HashSet<_>>();
    if let Resource::App { id } = &resource {
        app_ids.insert(id.clone());
    }

    let mut entities = Vec::new();
    entities.push(user_entity(principal_id, &user, &memberships)?);

    for app_id in app_ids {
        let flags = load_app_flags(pg, &app_id).await?;
        entities.push(app_entity(&app_id, flags)?);
    }

    if let Resource::Org { id } = &resource {
        entities.push(empty_entity("Org", id)?);
    }

    let entities = Entities::from_entities(entities, None)
        .map_err(|err| AuthzError::CedarEntities(err.to_string()))?;
    cache_put(key, entities.clone());
    Ok(entities)
}

struct UserAttrs {
    email_verified: bool,
    account_locked: bool,
    platform_role: String,
}

async fn load_user(pg: &Client, principal_id: Uuid) -> Result<UserAttrs, AuthzError> {
    // `DEFAULT_PLATFORM_ROLE` ("none") is a true zero-privilege role: no
    // platform Cedar policy matches it, so an un-roled creator is authorized
    // ONLY through their `app_members`-bound creator policies. Defaulting to a
    // privileged role (e.g. "readonly", which permits reads on an unconstrained
    // resource) would grant every creator fleet-wide cross-tenant read.
    let rows = pg
        .query(
            "SELECT \
                u.email_verified_at IS NOT NULL AS email_verified, \
                (u.locked_until IS NOT NULL AND u.locked_until > NOW()) AS account_locked, \
                COALESCE(r.role, $2) AS platform_role \
             FROM zeroship.users u \
             LEFT JOIN zeroship.platform_admin_roles r ON r.user_id = u.id \
             WHERE u.id = $1",
            &[&principal_id, &DEFAULT_PLATFORM_ROLE],
        )
        .await
        .map_err(|err| AuthzError::Db(format!("load user entities: {err}")))?;

    let row = rows
        .first()
        .ok_or_else(|| AuthzError::Validation(format!("principal not found: {principal_id}")))?;

    Ok(UserAttrs {
        email_verified: row.get("email_verified"),
        account_locked: row.get("account_locked"),
        platform_role: row.get("platform_role"),
    })
}

async fn load_memberships(pg: &Client, principal_id: Uuid) -> Result<Memberships, AuthzError> {
    let rows = pg
        .query(
            "SELECT app_id, role FROM zeroship.app_members WHERE user_id = $1",
            &[&principal_id],
        )
        .await
        .map_err(|err| AuthzError::Db(format!("load app memberships: {err}")))?;

    let mut memberships = Memberships::default();
    for row in rows {
        // `app_members.app_id` is a `uuid` column (0004_control.sql) — read it
        // as Uuid, then stringify. Reading it directly as String panics
        // (WrongType) the moment any membership row exists; this went undetected
        // because the only covering test is env-gated and never run in CI.
        let app_id: Uuid = row.get("app_id");
        let app_id = app_id.to_string();
        let role: String = row.get("role");
        match role.as_str() {
            "owner" => memberships.owner.push(app_id),
            "editor" => memberships.editor.push(app_id),
            "viewer" => memberships.viewer.push(app_id),
            other => {
                return Err(AuthzError::Validation(format!(
                    "unknown app member role: {other}"
                )))
            }
        }
    }
    Ok(memberships)
}

async fn load_app_flags(pg: &Client, app_id: &str) -> Result<AppFlags, AuthzError> {
    let rows = pg
        .query(
            "SELECT suspended, audit_locked FROM apps WHERE id::text = $1",
            &[&app_id],
        )
        .await
        .map_err(|err| AuthzError::Db(format!("load app entity: {err}")))?;

    Ok(rows.first().map_or_else(AppFlags::default, |row| AppFlags {
        suspended: row.get("suspended"),
        audit_locked: row.get("audit_locked"),
    }))
}

fn user_entity(
    principal_id: Uuid,
    user: &UserAttrs,
    memberships: &Memberships,
) -> Result<Entity, AuthzError> {
    let attrs = HashMap::from([
        (
            "platform_role".to_owned(),
            restricted_string(&user.platform_role)?,
        ),
        (
            "email_verified".to_owned(),
            restricted_bool(user.email_verified)?,
        ),
        (
            "account_locked".to_owned(),
            restricted_bool(user.account_locked)?,
        ),
        (
            "app_owner_of".to_owned(),
            restricted_app_set(&memberships.owner)?,
        ),
        (
            "app_editor_of".to_owned(),
            restricted_app_set(&memberships.editor)?,
        ),
        (
            "app_viewer_of".to_owned(),
            restricted_app_set(&memberships.viewer)?,
        ),
    ]);
    Entity::new(uid("User", &principal_id.to_string())?, attrs, HashSet::new())
        .map_err(|err| AuthzError::CedarEntities(err.to_string()))
}

fn app_entity(app_id: &str, flags: AppFlags) -> Result<Entity, AuthzError> {
    let attrs = HashMap::from([
        ("suspended".to_owned(), restricted_bool(flags.suspended)?),
        (
            "audit_locked".to_owned(),
            restricted_bool(flags.audit_locked)?,
        ),
    ]);
    Entity::new(uid("App", app_id)?, attrs, HashSet::new())
        .map_err(|err| AuthzError::CedarEntities(err.to_string()))
}

fn empty_entity(entity_type: &str, id: &str) -> Result<Entity, AuthzError> {
    Ok(Entity::new_no_attrs(uid(entity_type, id)?, HashSet::new()))
}

fn restricted_app_set(app_ids: &[String]) -> Result<RestrictedExpression, AuthzError> {
    let apps = app_ids
        .iter()
        .map(|id| format!("App::{}", cedar_string(id)))
        .collect::<Vec<_>>()
        .join(", ");
    restricted(&format!("[{apps}]"))
}

fn restricted_bool(value: bool) -> Result<RestrictedExpression, AuthzError> {
    restricted(if value { "true" } else { "false" })
}

fn restricted_string(value: &str) -> Result<RestrictedExpression, AuthzError> {
    restricted(&cedar_string(value))
}

fn restricted(source: &str) -> Result<RestrictedExpression, AuthzError> {
    RestrictedExpression::from_str(source)
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

fn resource_cache_key(resource: &Resource) -> String {
    match resource {
        Resource::App { id } => format!("app:{id}"),
        Resource::Org { id } => format!("org:{id}"),
        Resource::Any => "any:*".to_owned(),
    }
}

fn cache_get(key: &EntityCacheKey) -> Option<Entities> {
    let mut cache = lock_entity_cache();
    let entry = cache.get(key)?;
    if entry.inserted_at.elapsed() <= ENTITY_CACHE_TTL {
        Some(entry.entities.clone())
    } else {
        cache.pop(key);
        None
    }
}

fn cache_put(key: EntityCacheKey, entities: Entities) {
    let mut cache = lock_entity_cache();
    cache.put(
        key,
        CacheEntry {
            inserted_at: Instant::now(),
            entities,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_cache_invalidation_recovers_after_poison() {
        let poisoned = std::panic::catch_unwind(|| {
            let _guard = ENTITY_CACHE.lock().unwrap();
            panic!("poison entity cache");
        });
        assert!(poisoned.is_err());

        EntityCache::invalidate(Uuid::new_v4());
        EntityCache::invalidate_resource(&Resource::Any);
    }
}
