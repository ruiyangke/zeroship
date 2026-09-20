//! Host-supplied project root keys, and the per-database column key expanded
//! from one.
//!
//! The host supplies the project ROOT key and its authorized app bindings; the
//! key a column is actually encrypted under is [`derive_key`] of that root and
//! the database the row lives in. Column metadata cannot select a key. This
//! module performs no environment or database reads; control-plane provisioning
//! is responsible for delivering key material.
//!
//! **The root stays on the project and the salt is the database.** A project is
//! pinned to one execution zone, so serving its root no further than that zone
//! bounds the blast radius the root itself carries; the derived key is
//! nonetheless per database, which is what lets two apps bound to one database
//! read the same rows and keeps one app's two databases apart.

use super::aead::AeadKey;
use crate::error::DbError;
use hkdf::Hkdf;
use sha2::Sha256;
use std::{
    cell::Cell,
    collections::HashMap,
    sync::{Arc, RwLock},
};
use zeroize::Zeroizing;
use zeroship_core::{database_derivation, DatabaseId};

/// HKDF `info` for the at-rest column key, separating it from any other
/// expansion of the same project root.
const COLUMN_KEY_INFO: &[u8] = b"zeroship:at-rest-column-key:v2";

/// Expand one database's at-rest column key from its project's root key.
///
/// The salt is [`database_derivation::encryption_salt`], so the reconciler's
/// notion of which database a schema is and this expansion cannot disagree
/// about which id is being keyed on.
///
/// Every app the project binds to one database expands the SAME key from it,
/// which is the point: encryption is at-rest protection, not the fence between
/// co-binding-holders.
///
/// # Panics
///
/// If HKDF-SHA256 refuses to expand 32 bytes, which it does only past
/// `255 * 32`. The length is a constant here, so the failure is unreachable and
/// is not turned into a `Result` a caller would have to pretend to handle.
#[must_use]
pub fn derive_key(project_root: &AeadKey, database: &DatabaseId) -> AeadKey {
    let expander = Hkdf::<Sha256>::new(
        Some(database_derivation::encryption_salt(database)),
        &project_root.k_enc,
    );
    let mut k_enc = Zeroizing::new([0u8; 32]);
    expander
        .expand(COLUMN_KEY_INFO, k_enc.as_mut())
        .expect("HKDF-SHA256 expands 32 bytes");
    AeadKey { k_enc: *k_enc }
}

/// Project keys supplied through the trusted Rust host boundary.
#[derive(Default)]
pub struct SuppliedProjectKeys {
    bindings: RwLock<KeyBindings>,
}

#[derive(Default)]
struct KeyBindings {
    projects: HashMap<String, AeadKey>,
    app_projects: HashMap<String, String>,
}

impl std::fmt::Debug for SuppliedProjectKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SuppliedProjectKeys")
            .finish_non_exhaustive()
    }
}

impl SuppliedProjectKeys {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Supply a project's encryption key. Replacing key material requires a
    /// separate rotation protocol and is refused by this interface.
    pub fn insert_hex(&self, project_id: &str, hex: &str) -> Result<(), DbError> {
        let key = AeadKey {
            k_enc: parse_project_key(hex)?,
        };
        let mut bindings = self.bindings.write().map_err(|_| key_store_unavailable())?;
        let projects = &mut bindings.projects;
        if project_id.is_empty() || projects.contains_key(project_id) {
            return Err(DbError::validation(
                "invalid_project_key_binding",
                "project must be nonempty and its key must not already be installed",
            ));
        }
        projects.insert(project_id.to_owned(), key);
        Ok(())
    }

    pub fn with_hex(self, project_id: &str, hex: &str) -> Result<Self, DbError> {
        self.insert_hex(project_id, hex)?;
        Ok(self)
    }

    /// Authorize an app to use an installed project's key. An existing app
    /// cannot be reassigned to another project through this interface.
    pub fn bind_app(&self, app_id: &str, project_id: &str) -> Result<(), DbError> {
        let mut bindings = self.bindings.write().map_err(|_| key_store_unavailable())?;
        if app_id.is_empty() || !bindings.projects.contains_key(project_id) {
            return Err(DbError::validation(
                "invalid_project_key_binding",
                "app must be nonempty and project key must be installed",
            ));
        }
        let bindings = &mut bindings.app_projects;
        if let Some(current) = bindings.get(app_id) {
            if current != project_id {
                return Err(DbError::validation(
                    "invalid_project_key_binding",
                    "app already belongs to another project",
                ));
            }
        }
        bindings.insert(app_id.to_owned(), project_id.to_owned());
        Ok(())
    }

    /// Install an authenticated host response atomically. Repeated delivery
    /// of the same key and binding is harmless; changing either is refused.
    pub fn supply(&self, app_id: &str, project_id: &str, key: [u8; 32]) -> Result<(), DbError> {
        let key = AeadKey { k_enc: key };
        let mut bindings = self.bindings.write().map_err(|_| key_store_unavailable())?;
        if app_id.is_empty()
            || project_id.is_empty()
            || bindings
                .app_projects
                .get(app_id)
                .is_some_and(|current| current != project_id)
            || bindings
                .projects
                .get(project_id)
                .is_some_and(|current| current.k_enc != key.k_enc)
        {
            return Err(DbError::validation(
                "invalid_project_key_binding",
                "project key or app binding conflicts with its installed identity",
            ));
        }
        bindings
            .projects
            .entry(project_id.to_owned())
            .or_insert(key);
        bindings
            .app_projects
            .insert(app_id.to_owned(), project_id.to_owned());
        Ok(())
    }

    fn lookup(&self, app_id: &str) -> Result<AeadKey, DbError> {
        let bindings = self.bindings.read().map_err(|_| key_store_unavailable())?;
        let key = bindings
            .app_projects
            .get(app_id)
            .and_then(|project| bindings.projects.get(project).cloned());
        key.ok_or_else(|| DbError::Configuration {
            code: "column_key_not_configured",
            message: format!("No project encryption key was supplied for app '{app_id}'"),
            hint: Some(
                "The host must supply the project's encryption key and app binding".to_owned(),
            ),
        })
    }

    /// Whether this host has already installed an app's immutable binding.
    pub fn is_bound(&self, app_id: &str) -> Result<bool, DbError> {
        Ok(self
            .bindings
            .read()
            .map_err(|_| key_store_unavailable())?
            .app_projects
            .contains_key(app_id))
    }

    /// Retire a deleted app's binding, releasing unused project material.
    pub fn remove_app(&self, app_id: &str) -> Result<(), DbError> {
        let mut bindings = self.bindings.write().map_err(|_| key_store_unavailable())?;
        if let Some(project) = bindings.app_projects.remove(app_id) {
            if !bindings
                .app_projects
                .values()
                .any(|value| value == &project)
            {
                bindings.projects.remove(&project);
            }
        }
        Ok(())
    }
}

/// A local handle to project keys supplied by the host. The default has no keys.
#[derive(Clone, Debug, Default)]
pub struct ProjectKeySource(Arc<SuppliedProjectKeys>);
impl ProjectKeySource {
    #[must_use]
    pub fn unavailable() -> Self {
        Self::default()
    }
    #[must_use]
    pub fn supplied(keys: Arc<SuppliedProjectKeys>) -> Self {
        Self(keys)
    }
}

/// Resolve one database's column key for every encrypted column in it.
///
/// The app names which project's root key the host supplied; the database names
/// what that root is expanded with. No per-column derivation is performed, and
/// the database - not the app - is what the ciphertext AAD authenticates.
#[derive(Debug)]
pub struct KeyStore {
    source: ProjectKeySource,
    lookups: Cell<u64>,
}
impl KeyStore {
    #[must_use]
    pub fn new(source: ProjectKeySource) -> Self {
        Self {
            source,
            lookups: Cell::new(0),
        }
    }
    #[must_use]
    pub fn lookups_count(&self) -> u64 {
        self.lookups.get()
    }

    /// The key `app_id`'s rows in `database` are encrypted under.
    ///
    /// # Errors
    ///
    /// [`DbError::Configuration`] when the host supplied no project root key
    /// for this app.
    #[allow(clippy::unused_async)]
    pub async fn resolve(&self, app_id: &str, database: &DatabaseId) -> Result<AeadKey, DbError> {
        self.lookups.set(self.lookups.get() + 1);
        Ok(derive_key(&self.source.0.lookup(app_id)?, database))
    }
}

fn key_store_unavailable() -> DbError {
    DbError::internal("project key store lock is poisoned")
}

fn parse_project_key(hex: &str) -> Result<[u8; 32], DbError> {
    let invalid = || DbError::Configuration {
        code: "column_key_not_configured",
        message: "Project encryption key must be an AES-256 key encoded as hexadecimal".to_owned(),
        hint: None,
    };
    if hex.len() != 64 {
        return Err(invalid());
    }
    let mut bytes = Zeroizing::new([0u8; 32]);
    for (out, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        let high = (pair[0] as char).to_digit(16).ok_or_else(invalid)?;
        let low = (pair[1] as char).to_digit(16).ok_or_else(invalid)?;
        *out = ((high << 4) | low) as u8;
    }
    Ok(*bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The expansion is a function of BOTH inputs.** A root that never
    /// reached HKDF, or a salt that did not, would leave one of the two axes
    /// unkeyed - and neither shows up as an error, only as a read that
    /// succeeds or fails in the wrong place.
    #[test]
    fn the_column_key_varies_with_the_root_and_with_the_database() {
        let root = AeadKey { k_enc: [1; 32] };
        let other_root = AeadKey { k_enc: [2; 32] };
        let here = DatabaseId::mint();
        let there = DatabaseId::mint();
        assert_ne!(here, there, "the control: two mints are two databases");

        let derived = derive_key(&root, &here);
        assert_eq!(
            derived.k_enc,
            derive_key(&root, &here).k_enc,
            "the expansion must be deterministic or no row ever decrypts twice"
        );
        assert_ne!(
            derived.k_enc,
            derive_key(&root, &there).k_enc,
            "the database must reach the expansion"
        );
        assert_ne!(
            derived.k_enc,
            derive_key(&other_root, &here).k_enc,
            "the project root must reach the expansion"
        );
        assert_ne!(
            derived.k_enc, root.k_enc,
            "the root itself must never be handed to AES-GCM"
        );
    }

    #[compio::test]
    async fn an_existing_key_store_observes_host_delivery_from_another_thread() {
        let source = Arc::new(SuppliedProjectKeys::new());
        let store = KeyStore::new(ProjectKeySource::supplied(source.clone()));
        let database = DatabaseId::mint();
        assert!(store.resolve("app", &database).await.is_err());
        std::thread::spawn(move || source.supply("app", "project", [9; 32]).unwrap())
            .join()
            .unwrap();
        assert_eq!(
            store.resolve("app", &database).await.unwrap().k_enc,
            derive_key(&AeadKey { k_enc: [9; 32] }, &database).k_enc
        );
    }

    #[test]
    fn host_delivery_is_shared_idempotent_and_does_not_partially_rebind() {
        let keys = Arc::new(SuppliedProjectKeys::new());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let keys = keys.clone();
                scope.spawn(move || keys.supply("app", "project", [7; 32]).unwrap());
            }
        });
        keys.supply("sibling", "project", [7; 32]).unwrap();
        assert!(keys.supply("other", "project", [8; 32]).is_err());
        assert!(!keys.is_bound("other").unwrap());
        assert!(keys.supply("app", "other", [8; 32]).is_err());
        assert_eq!(keys.lookup("app").unwrap().k_enc, [7; 32]);
        keys.remove_app("app").unwrap();
        assert!(keys.lookup("app").is_err());
        assert_eq!(keys.lookup("sibling").unwrap().k_enc, [7; 32]);
        keys.remove_app("sibling").unwrap();
        assert!(keys.bindings.read().unwrap().projects.is_empty());
    }

    /// Two apps of one project reading ONE database expand one column key, and
    /// an app of another project reading the same database expands another.
    ///
    /// The database is held constant across all three, so the only variable is
    /// which root the app's project supplied - which is what makes the first
    /// equality evidence about co-binding-holders rather than about the salt.
    #[compio::test]
    async fn co_binding_holders_of_one_project_expand_one_column_key() {
        let supplied = Arc::new(SuppliedProjectKeys::new());
        supplied.insert_hex("project_a", &"11".repeat(32)).unwrap();
        supplied.insert_hex("project_b", &"22".repeat(32)).unwrap();
        supplied.bind_app("app_a", "project_a").unwrap();
        supplied.bind_app("app_b", "project_a").unwrap();
        supplied.bind_app("app_c", "project_b").unwrap();
        let keys = KeyStore::new(ProjectKeySource::supplied(supplied));
        let shared = DatabaseId::mint();
        let elsewhere = DatabaseId::mint();

        let a = keys.resolve("app_a", &shared).await.unwrap();
        assert_eq!(
            a.k_enc,
            keys.resolve("app_b", &shared).await.unwrap().k_enc,
            "two apps bound to one database must read each other's rows"
        );
        assert_ne!(
            a.k_enc,
            keys.resolve("app_c", &shared).await.unwrap().k_enc,
            "another project's root must not expand this project's key"
        );
        assert_ne!(
            a.k_enc,
            keys.resolve("app_a", &elsewhere).await.unwrap().k_enc,
            "one app's two databases must not share a key"
        );
        assert!(keys.resolve("unbound", &shared).await.is_err());
    }

    #[test]
    fn keys_cannot_be_replaced_or_apps_rebound() {
        let supplied = SuppliedProjectKeys::new();
        supplied.insert_hex("a", &"11".repeat(32)).unwrap();
        supplied.insert_hex("b", &"22".repeat(32)).unwrap();
        assert!(supplied.insert_hex("a", &"33".repeat(32)).is_err());
        supplied.bind_app("app", "a").unwrap();
        assert!(supplied.bind_app("app", "b").is_err());
        assert!(supplied.bind_app("unknown", "missing").is_err());
        assert!(supplied.bind_app("", "a").is_err());
    }

    #[test]
    fn invalid_project_key_material_is_refused() {
        for key in ["", "ab", &"g".repeat(64), &"é".repeat(32)] {
            assert!(parse_project_key(key).is_err());
        }
        assert_eq!(parse_project_key(&"aB".repeat(32)).unwrap(), [0xab; 32]);
    }

    #[compio::test]
    async fn lookup_counter_includes_failed_resolves() {
        let keys = KeyStore::new(ProjectKeySource::unavailable());
        assert_eq!(keys.lookups_count(), 0);
        assert!(keys.resolve("app", &DatabaseId::mint()).await.is_err());
        assert_eq!(keys.lookups_count(), 1);
    }
}
