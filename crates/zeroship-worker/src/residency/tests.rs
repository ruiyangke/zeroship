//! What holding an app keeps supplied, and what the last drop withdraws.

#![expect(
    clippy::future_not_send,
    reason = "the worker kernel and its isolates are thread-local, so these cases' \
              futures stay on their compio thread"
)]

use super::*;
use crate::cache::{self, fixture::Kernel, KernelConfig};
use zeroship_core::{database_role::DatabaseCapability, BindingId, DatabaseId};
use zeroship_data_orm::{
    encryption::{
        canonical_aad, decrypt, derive_key, encrypt, AeadKey, KeyStore, ProjectKeySource,
    },
    resolved_bindings::ResolvedBinding,
};

/// The three stores a worker supplies an app's credentials into, and a
/// registry over them.
struct Stores {
    keys: Arc<SuppliedProjectKeys>,
    bindings: Arc<SuppliedAppBindings>,
    envs: SharedEnvs,
    registry: AppResidency,
}

impl Stores {
    fn new() -> Self {
        let keys = Arc::new(SuppliedProjectKeys::new());
        let bindings = Arc::new(SuppliedAppBindings::new());
        let envs = SharedEnvs::default();
        let registry = AppResidency::new(Some(keys.clone()), Some(bindings.clone()), envs.clone());
        Self {
            keys,
            bindings,
            envs,
            registry,
        }
    }

    /// Supply all three for `app`, as the sync path does for a holder.
    fn supply(&self, app: &AppId, root: [u8; 32]) {
        supply(&self.keys, &self.bindings, &self.envs, app, root);
    }

    /// Which of the three this worker holds for `app`: key, bindings, env.
    fn supplied(&self, app: &AppId) -> [bool; 3] {
        supplied(&self.keys, &self.bindings, &self.envs, app)
    }
}

fn supply(
    keys: &SuppliedProjectKeys,
    bindings: &SuppliedAppBindings,
    envs: &SharedEnvs,
    app: &AppId,
    root: [u8; 32],
) {
    keys.supply(
        app.as_str(),
        zeroship_core::ProjectId::mint().as_str(),
        root,
    )
    .expect("a fresh store accepts the app's project key");
    bindings
        .supply(
            app.as_str(),
            ResolvedBinding {
                database: DatabaseId::mint(),
                binding: BindingId::mint(),
                capability: DatabaseCapability::ReadWrite,
            },
        )
        .expect("a fresh store accepts the app's binding");
    crate::sync::put_env_from_json(
        envs,
        app.clone(),
        r#"{"vars":{"API_TOKEN":"held"},"secrets":{},"expose":[]}"#,
        1,
    )
    .expect("the environment parses");
}

fn supplied(
    keys: &SuppliedProjectKeys,
    bindings: &SuppliedAppBindings,
    envs: &SharedEnvs,
    app: &AppId,
) -> [bool; 3] {
    [
        keys.is_bound(app.as_str()).expect("the key store answers"),
        bindings
            .is_bound(app.as_str())
            .expect("the binding store answers"),
        crate::sync::get_env(envs, app).is_some(),
    ]
}

/// One encrypted cell, sealed the way the database service seals a column:
/// under the key the app's project root expands to for one database.
struct Sealed {
    database: DatabaseId,
    aad: Vec<u8>,
    blob: Vec<u8>,
}

impl Sealed {
    fn new(root: [u8; 32], plaintext: &[u8]) -> Self {
        let database = DatabaseId::mint();
        let aad = canonical_aad(&database, "notes", "body", b"row-1");
        let blob = encrypt(
            &derive_key(&AeadKey { k_enc: root }, &database),
            plaintext,
            &aad,
        )
        .expect("the cell seals");
        Self {
            database,
            aad,
            blob,
        }
    }

    /// Read the cell back as a session does: the key is looked up in the
    /// host's store on THIS call, so what decides the read is what the store
    /// holds now.
    async fn read(&self, keys: &Arc<SuppliedProjectKeys>, app: &AppId) -> Result<Vec<u8>, String> {
        let key = KeyStore::new(ProjectKeySource::supplied(keys.clone()))
            .resolve(app.as_str(), &self.database)
            .await
            .map_err(|error| error.to_string())?;
        decrypt(&key, &self.blob, &self.aad).map_err(|error| error.to_string())
    }
}

/// The last holder's drop withdraws the app's project key, its database
/// bindings and its environment, and nothing before it does.
///
/// Two controls: a drop that leaves another holder withdraws nothing, and an
/// app held by nothing that was dropped keeps all three.
#[test]
fn the_last_holder_dropping_withdraws_the_key_the_bindings_and_the_environment() {
    let stores = Stores::new();
    let app = AppId::mint();
    let neighbour = AppId::mint();
    let first = stores.registry.reside(app.clone());
    let second = stores.registry.reside(app.clone());
    let _neighbour = stores.registry.reside(neighbour.clone());
    stores.supply(&app, [1; 32]);
    stores.supply(&neighbour, [2; 32]);
    assert_eq!(
        stores.supplied(&app),
        [true; 3],
        "the premise: all three supplied"
    );

    drop(first);
    assert_eq!(
        stores.supplied(&app),
        [true; 3],
        "another holder still holds the app, so nothing is withdrawn"
    );

    drop(second);
    assert_eq!(
        stores.supplied(&app),
        [false; 3],
        "the last holder's drop withdraws the key, the bindings and the environment"
    );
    assert_eq!(stores.registry.holders(&app), 0);
    assert_eq!(
        stores.supplied(&neighbour),
        [true; 3],
        "withdrawal is the dropped app's alone"
    );
}

/// A refresh joins only an app something already holds.
///
/// An app nothing holds has had its material withdrawn, and a refresh that held
/// it anyway would supply credentials no holder ever withdraws. The control is
/// a held app, which the refresh does join, so its own drop is not the last.
#[test]
fn a_refresh_holds_only_an_app_something_already_holds() {
    let stores = Stores::new();
    let app = AppId::mint();
    assert!(
        stores.registry.reside_held(&app).is_none(),
        "nothing holds the app, so a refresh does not hold it"
    );
    assert_eq!(stores.registry.holders(&app), 0);

    let holder = stores.registry.reside(app.clone());
    stores.supply(&app, [3; 32]);
    let refresh = stores
        .registry
        .reside_held(&app)
        .expect("a held app is refreshed under a hold of its own");
    assert_eq!(stores.registry.holders(&app), 2);
    drop(holder);
    assert_eq!(
        stores.supplied(&app),
        [true; 3],
        "the refresh's own hold keeps what it is refreshing"
    );
    drop(refresh);
    assert_eq!(stores.supplied(&app), [false; 3]);
}

/// A request in flight keeps reading its app's encrypted data after the cache
/// drops the isolate it runs on, and the same read refuses once the request
/// ends.
///
/// The cache entry is one holder and the request another: a reconcile that
/// evicts a withdrawn deploy or a deleted app drops the entry while the request
/// is still running, and the database service resolves the key on every call.
#[compio::test]
async fn a_request_holding_its_app_keeps_reading_encrypted_data_after_the_cache_drops_it() {
    zeroship_runtime::init::init_v8();
    let service = cache::fixture::database_service("postgresql://fixture:fixture@localhost/unused");
    let envs = SharedEnvs::default();
    let registry = AppResidency::new(
        Some(service.project_keys().clone()),
        Some(service.app_bindings().clone()),
        envs.clone(),
    );
    let _kernel = Kernel::install(
        10,
        KernelConfig {
            workflows: None,
            db_service: Some(service.clone()),
            kv_store: None,
            storage_backend: None,
            meter: Arc::new(zeroship_metering::Meter::new()),
            residency: Some(registry.clone()),
        },
    );
    let app = AppId::mint();
    let root = [4; 32];
    let sealed = Sealed::new(root, b"held secret");

    let hold = cache::hold(&app);
    supply(
        service.project_keys(),
        service.app_bindings(),
        &envs,
        &app,
        root,
    );
    cache::load_app(
        hold,
        cache::test_modules(br#"export default { fetch() { return new Response("ok"); } }"#),
        crate::cache::TEST_LIMITS,
        zeroship_core::types::AppNetPolicy::default(),
        None,
        None,
        &zeroship_bundle::Manifest::default(),
        &zeroship_runtime::EnvSnapshot::empty(),
    )
    .await
    .expect("the app loads");
    let request = cache::get_runtime(&app).expect("the app is cached");

    cache::evict_app(&app);
    assert!(
        cache::get_runtime(&app).is_none(),
        "the premise: the cache dropped its entry"
    );
    assert_eq!(
        sealed.read(service.project_keys(), &app).await.as_deref(),
        Ok(&b"held secret"[..]),
        "the request still holds the app, so its key is still supplied"
    );

    drop(request);
    let refused = sealed
        .read(service.project_keys(), &app)
        .await
        .expect_err("with the last holder gone the key is withdrawn");
    assert!(refused.contains("No project encryption key"), "{refused}");
    assert_eq!(
        supplied(service.project_keys(), service.app_bindings(), &envs, &app),
        [false; 3]
    );
}
