//! Integration tests for `EnvStore` against a real Postgres.
//!
//! Set `CONTROL_TEST_DB` to a Postgres URL (the docker-compose in this
//! repo boots one on 127.0.0.1:5440 for local dev). Tests are silently
//! skipped otherwise so this file doesn't gate CI without a DB.
//!
//! Each test uses a unique app row so parallel runs don't collide.

use uuid::Uuid;
use zeroship_control::{EnvStore, Registry};

fn db_url() -> Option<String> { std::env::var("CONTROL_TEST_DB").ok() }

async fn create_test_app(registry: &Registry) -> Uuid {
    // Generate a unique name to survive parallel test runs.
    let name = format!("test-{}", &Uuid::new_v4().simple().to_string()[..12]);
    let rec = registry.create_app(&name, "free").await.expect("create_app");
    rec.id
}

#[compio::test]
async fn var_crud_roundtrip() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let registry = Registry::new(&url).await.expect("registry");
    let store = EnvStore::new(registry.clone(), "dev-master-key", false).expect("store");
    let app = create_test_app(&registry).await;

    // Empty initially.
    assert!(store.list_vars(app).await.unwrap().is_empty());

    // Create one var.
    store.set_var(app, "FOO", "bar").await.unwrap();
    let vars = store.list_vars(app).await.unwrap();
    assert_eq!(vars, vec![("FOO".to_string(), "bar".to_string())]);

    // Update overwrites.
    store.set_var(app, "FOO", "baz").await.unwrap();
    let vars = store.list_vars(app).await.unwrap();
    assert_eq!(vars, vec![("FOO".to_string(), "baz".to_string())]);

    // Delete returns true.
    assert!(store.delete_var(app, "FOO").await.unwrap());
    assert!(store.list_vars(app).await.unwrap().is_empty());
    // Second delete returns false.
    assert!(!store.delete_var(app, "FOO").await.unwrap());

    // Cleanup.
    registry.delete_app(&app).await.ok();
}

#[compio::test]
async fn secret_roundtrip_encrypted() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = EnvStore::new(registry.clone(), "dev-master-key", false).expect("store");
    let app = create_test_app(&registry).await;

    store.set_secret(app, "STRIPE_KEY", "sk_live_sensitive").await.unwrap();

    // List surface is write-only for values — only names come back.
    let names = store.list_secret_names(app).await.unwrap();
    assert_eq!(names, vec!["STRIPE_KEY"]);

    // Merged env decrypts the secret correctly.
    let merged = store.merged_env(app).await.unwrap();
    assert_eq!(
        merged.get("STRIPE_KEY").and_then(|v| v.as_str()),
        Some("sk_live_sensitive")
    );

    // Raw ciphertext at rest must NOT contain the plaintext.
    let ct = store.__raw_ciphertext_for_test(app, "STRIPE_KEY").await.unwrap().expect("row");
    assert!(!ct.windows(b"sensitive".len()).any(|w| w == b"sensitive"),
        "ciphertext contained plaintext bytes");

    // Rotation: new value replaces old.
    store.set_secret(app, "STRIPE_KEY", "sk_live_rotated").await.unwrap();
    let merged2 = store.merged_env(app).await.unwrap();
    assert_eq!(
        merged2.get("STRIPE_KEY").and_then(|v| v.as_str()),
        Some("sk_live_rotated")
    );

    // Cleanup.
    registry.delete_app(&app).await.ok();
}

#[compio::test]
async fn merged_env_secret_overrides_var() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = EnvStore::new(registry.clone(), "dev-master-key", false).expect("store");
    let app = create_test_app(&registry).await;

    // Same key in both tables — secret wins because it's applied last in merged_env.
    store.set_var(app, "API_KEY", "var-value").await.unwrap();
    store.set_secret(app, "API_KEY", "secret-value").await.unwrap();

    let merged = store.merged_env(app).await.unwrap();
    assert_eq!(merged.get("API_KEY").and_then(|v| v.as_str()), Some("secret-value"));

    registry.delete_app(&app).await.ok();
}

#[compio::test]
async fn invalid_key_rejected_client_side() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = EnvStore::new(registry.clone(), "dev-master-key", false).expect("store");
    let app = create_test_app(&registry).await;

    for bad in ["lowercase", "1LEADING_DIGIT", "HAS-DASH", "_LEADING_US", "HAS SPACE", ""] {
        let err = store.set_var(app, bad, "x").await.unwrap_err();
        assert!(matches!(err, zeroship_control::env_store::EnvError::BadKey(_)),
            "expected BadKey for '{bad}', got {err:?}");
        let err = store.set_secret(app, bad, "x").await.unwrap_err();
        assert!(matches!(err, zeroship_control::env_store::EnvError::BadKey(_)),
            "expected BadKey for '{bad}', got {err:?}");
    }

    registry.delete_app(&app).await.ok();
}

#[compio::test]
async fn per_app_isolation() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = EnvStore::new(registry.clone(), "dev-master-key", false).expect("store");
    let app_a = create_test_app(&registry).await;
    let app_b = create_test_app(&registry).await;

    store.set_var(app_a, "SHARED_NAME", "a-value").await.unwrap();
    store.set_secret(app_b, "SHARED_NAME", "b-value").await.unwrap();

    let env_a = store.merged_env(app_a).await.unwrap();
    let env_b = store.merged_env(app_b).await.unwrap();

    assert_eq!(env_a.get("SHARED_NAME").and_then(|v| v.as_str()), Some("a-value"));
    assert_eq!(env_b.get("SHARED_NAME").and_then(|v| v.as_str()), Some("b-value"));
    // Each app sees exactly one entry.
    assert_eq!(env_a.len(), 1);
    assert_eq!(env_b.len(), 1);

    registry.delete_app(&app_a).await.ok();
    registry.delete_app(&app_b).await.ok();
}

#[compio::test]
async fn wrong_master_key_fails_decrypt() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let writer = EnvStore::new(registry.clone(), "correct-key", false).expect("store");
    let reader = EnvStore::new(registry.clone(), "wrong-key", false).expect("store");
    let app = create_test_app(&registry).await;

    writer.set_secret(app, "TOKEN", "hidden").await.unwrap();

    // Reader with wrong master key can see the name...
    let names = reader.list_secret_names(app).await.unwrap();
    assert_eq!(names, vec!["TOKEN"]);

    // ...but merged_env fails on decrypt rather than silently returning
    // garbage or the plaintext.
    let err = reader.merged_env(app).await.unwrap_err();
    assert!(matches!(err, zeroship_control::env_store::EnvError::Crypto(_)));

    registry.delete_app(&app).await.ok();
}

#[compio::test]
async fn delete_cascades_from_app() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = EnvStore::new(registry.clone(), "k", false).expect("store");
    let app = create_test_app(&registry).await;

    store.set_var(app, "V1", "x").await.unwrap();
    store.set_secret(app, "S1", "y").await.unwrap();

    // Drop the app — FK ON DELETE CASCADE removes rows.
    registry.delete_app(&app).await.unwrap();
    assert!(store.list_vars(app).await.unwrap().is_empty());
    assert!(store.list_secret_names(app).await.unwrap().is_empty());
}

#[compio::test]
async fn merged_env_404s_on_missing_app() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = EnvStore::new(registry.clone(), "k", false).expect("store");

    let ghost = Uuid::new_v4();
    let err = store.merged_env(ghost).await.unwrap_err();
    assert!(
        matches!(err, zeroship_control::env_store::EnvError::AppNotFound),
        "expected AppNotFound for missing app, got {err:?}",
    );
}

#[compio::test]
async fn empty_master_key_rejected_without_dev_flag() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let err = EnvStore::new(registry, "", false).unwrap_err();
    assert!(matches!(err, zeroship_control::env_store::EnvError::MasterKeyRequired));
}

#[compio::test]
async fn empty_master_key_allowed_in_dev_mode() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let _store = EnvStore::new(registry, "", true).expect("dev-mode should accept empty key");
}

#[compio::test]
async fn set_value_over_cap_rejected() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = EnvStore::new(registry.clone(), "k", false).expect("store");
    let app = create_test_app(&registry).await;

    let too_big = "A".repeat(zeroship_control::env_store::MAX_VALUE_BYTES + 1);
    let err = store.set_var(app, "FOO", &too_big).await.unwrap_err();
    assert!(matches!(err, zeroship_control::env_store::EnvError::TooLarge(_)));
    let err = store.set_secret(app, "FOO", &too_big).await.unwrap_err();
    assert!(matches!(err, zeroship_control::env_store::EnvError::TooLarge(_)));

    registry.delete_app(&app).await.ok();
}

#[compio::test]
async fn long_value_roundtrip() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = EnvStore::new(registry.clone(), "k", false).expect("store");
    let app = create_test_app(&registry).await;

    // 16 KiB — simulating a JSON Stripe-connect OAuth blob or similar.
    let big: String = (0..16 * 1024).map(|i| char::from(b'A' + (i % 26) as u8)).collect();
    store.set_secret(app, "BIG_TOKEN", &big).await.unwrap();
    let merged = store.merged_env(app).await.unwrap();
    assert_eq!(merged.get("BIG_TOKEN").and_then(|v| v.as_str()), Some(big.as_str()));

    registry.delete_app(&app).await.ok();
}
