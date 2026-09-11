//! Host-supplied project encryption keys and app-to-project bindings.
//!
//! The host supplies the project key and its authorized app bindings. Column
//! metadata cannot select a key. This module performs no environment or database
//! reads; control-plane provisioning is responsible for delivering key material.

use super::aead::AeadKey;
use crate::error::DbError;
use std::{
    cell::Cell,
    collections::HashMap,
    sync::{Arc, RwLock},
};
use zeroize::Zeroizing;

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

/// Resolve the same project key for every encrypted column of a bound app.
/// Supplied keys are already usable AEAD keys; no per-column or per-app key
/// derivation is performed. App identity is authenticated by the ciphertext AAD.
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
    #[allow(clippy::unused_async)]
    pub async fn resolve(&self, app_id: &str) -> Result<AeadKey, DbError> {
        self.lookups.set(self.lookups.get() + 1);
        self.source.0.lookup(app_id)
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

    #[compio::test]
    async fn an_existing_key_store_observes_host_delivery_from_another_thread() {
        let source = Arc::new(SuppliedProjectKeys::new());
        let store = KeyStore::new(ProjectKeySource::supplied(source.clone()));
        assert!(store.resolve("app").await.is_err());
        std::thread::spawn(move || source.supply("app", "project", [9; 32]).unwrap())
            .join()
            .unwrap();
        assert_eq!(store.resolve("app").await.unwrap().k_enc, [9; 32]);
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

    #[compio::test]
    async fn project_key_is_shared_only_by_bound_apps() {
        let supplied = Arc::new(SuppliedProjectKeys::new());
        supplied.insert_hex("project_a", &"11".repeat(32)).unwrap();
        supplied.insert_hex("project_b", &"22".repeat(32)).unwrap();
        supplied.bind_app("app_a", "project_a").unwrap();
        supplied.bind_app("app_b", "project_a").unwrap();
        supplied.bind_app("app_c", "project_b").unwrap();
        let keys = KeyStore::new(ProjectKeySource::supplied(supplied));
        let a = keys.resolve("app_a").await.unwrap();
        assert_eq!(a.k_enc, keys.resolve("app_b").await.unwrap().k_enc);
        assert_ne!(a.k_enc, keys.resolve("app_c").await.unwrap().k_enc);
        assert!(keys.resolve("unbound").await.is_err());
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
        assert!(keys.resolve("app").await.is_err());
        assert_eq!(keys.lookups_count(), 1);
    }
}
