//! Integration tests for `EnvStore` against a real Postgres.
//!
//! Set `CONTROL_TEST_DB` to a Postgres URL (the docker-compose in this
//! repo boots one on 127.0.0.1:5440 for local dev). Tests are silently
//! skipped otherwise so this file doesn't gate CI without a DB.
//!
//! Each test uses a unique app row so parallel runs don't collide.

use uuid::Uuid;
use zeroship_control::audit::{self, Action, AuditEntry};
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
async fn env_version_bumps_on_every_mutation() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = EnvStore::new(registry.clone(), "k", false).expect("store");
    let app = create_test_app(&registry).await;

    // Freshly-created app starts at env_version=0.
    let v0 = registry.get_versions().await.unwrap();
    assert_eq!(v0.get(&app).unwrap().env_version, 0);

    // Each mutation bumps by 1.
    store.set_var(app, "FOO", "1").await.unwrap();
    let v1 = registry.get_versions().await.unwrap();
    assert_eq!(v1.get(&app).unwrap().env_version, 1);

    store.set_secret(app, "STRIPE_KEY", "sk_live_1").await.unwrap();
    let v2 = registry.get_versions().await.unwrap();
    assert_eq!(v2.get(&app).unwrap().env_version, 2);

    store.set_var(app, "FOO", "2").await.unwrap(); // overwrite still bumps
    let v3 = registry.get_versions().await.unwrap();
    assert_eq!(v3.get(&app).unwrap().env_version, 3);

    store.delete_var(app, "FOO").await.unwrap();
    let v4 = registry.get_versions().await.unwrap();
    assert_eq!(v4.get(&app).unwrap().env_version, 4);

    // Delete of a non-existent key does NOT bump.
    let removed = store.delete_var(app, "GHOST").await.unwrap();
    assert!(!removed);
    let v5 = registry.get_versions().await.unwrap();
    assert_eq!(v5.get(&app).unwrap().env_version, 4);

    registry.delete_app(&app).await.ok();
}

#[compio::test]
async fn rotation_decrypts_old_secrets_and_rewrites_to_new_key() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let app = create_test_app(&registry).await;

    // Initial write with key v1.
    let store_v1 = EnvStore::new(registry.clone(), "key-v1", false).expect("store");
    store_v1.set_secret(app, "STRIPE_KEY", "sk_live_old").await.unwrap();

    // Rotate to key v2 and keep key-v1 as a legacy decrypt-only key.
    // Existing ciphertexts still decrypt; new writes use v2.
    let store_v2 = EnvStore::new_with_previous(
        registry.clone(),
        "key-v2",
        &["key-v1"],
        false,
    ).expect("store");

    let merged = store_v2.merged_env(app).await.unwrap();
    assert_eq!(merged.get("STRIPE_KEY").and_then(|v| v.as_str()), Some("sk_live_old"));

    // New write goes through with v2.
    store_v2.set_secret(app, "OPENAI_KEY", "sk-new").await.unwrap();
    let merged = store_v2.merged_env(app).await.unwrap();
    assert_eq!(merged.get("OPENAI_KEY").and_then(|v| v.as_str()), Some("sk-new"));

    // Rewrite all secrets onto v2 (drains the rotation grace period).
    let count = store_v2.rotate_app(app).await.unwrap();
    // STRIPE_KEY needed re-encryption (was on v1); OPENAI_KEY already
    // on v2 — skipped.
    assert_eq!(count, 1);

    // Drop the legacy key. Previously-rotated secrets still decrypt
    // via the new primary alone.
    let store_v3 = EnvStore::new(registry.clone(), "key-v2", false).expect("store");
    let merged = store_v3.merged_env(app).await.unwrap();
    assert_eq!(merged.get("STRIPE_KEY").and_then(|v| v.as_str()), Some("sk_live_old"));
    assert_eq!(merged.get("OPENAI_KEY").and_then(|v| v.as_str()), Some("sk-new"));

    registry.delete_app(&app).await.ok();
}

#[compio::test]
async fn audit_log_roundtrip() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let app = create_test_app(&registry).await;

    audit::log(&registry, AuditEntry {
        app_id: Some(app),
        creator_id: None,
        actor: "admin",
        action: Action::SetSecret,
        resource: Some("STRIPE_KEY"),
        source_ip: Some("203.0.113.7"),
    }).await;
    audit::log(&registry, AuditEntry {
        app_id: Some(app),
        creator_id: None,
        actor: "admin",
        action: Action::DeleteSecret,
        resource: Some("STRIPE_KEY"),
        source_ip: None,
    }).await;

    let rows = audit::recent_for_app(&registry, app, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    // Newest first.
    assert_eq!(rows[0].action, "delete_secret");
    assert_eq!(rows[0].resource.as_deref(), Some("STRIPE_KEY"));
    assert_eq!(rows[0].source_ip, None);
    assert_eq!(rows[1].action, "set_secret");
    assert_eq!(rows[1].source_ip.as_deref(), Some("203.0.113.7"));
    assert_eq!(rows[0].actor, "admin");

    registry.delete_app(&app).await.ok();
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
async fn merged_env_for_worker_emits_split_shape() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = EnvStore::new(registry.clone(), "dev-master-key", false).expect("store");
    let app = create_test_app(&registry).await;

    // Mix of vars + secrets + an opt-in expose entry.
    store.set_var(app, "NODE_ENV", "production").await.unwrap();
    store.set_var(app, "API_URL", "https://api.example.com").await.unwrap();
    store.set_secret(app, "OPENAI_API_KEY", "sk-secret-1").await.unwrap();
    store.set_secret(app, "STRIPE_KEY", "sk_live_2").await.unwrap();
    let new_expose = store.set_expose(app, &["OPENAI_API_KEY".to_string()]).await.unwrap();
    assert_eq!(new_expose, vec!["OPENAI_API_KEY".to_string()]);

    let payload = store.merged_env_for_worker(app).await.unwrap();
    let obj = payload.as_object().expect("top-level object");

    // Three keys exactly: vars, secrets, expose.
    let mut top_keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    top_keys.sort();
    assert_eq!(top_keys, vec!["expose", "secrets", "vars"]);

    let vars = obj.get("vars").and_then(|v| v.as_object()).expect("vars object");
    assert_eq!(vars.get("NODE_ENV").and_then(|v| v.as_str()), Some("production"));
    assert_eq!(vars.get("API_URL").and_then(|v| v.as_str()), Some("https://api.example.com"));

    let secrets = obj.get("secrets").and_then(|v| v.as_object()).expect("secrets object");
    assert_eq!(secrets.get("OPENAI_API_KEY").and_then(|v| v.as_str()), Some("sk-secret-1"));
    assert_eq!(secrets.get("STRIPE_KEY").and_then(|v| v.as_str()), Some("sk_live_2"));

    let expose = obj.get("expose").and_then(|v| v.as_array()).expect("expose array");
    let names: Vec<&str> = expose.iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(names, vec!["OPENAI_API_KEY"]);

    // Replacing the list overwrites — not appends.
    store.set_expose(app, &["STRIPE_KEY".to_string()]).await.unwrap();
    let payload = store.merged_env_for_worker(app).await.unwrap();
    let names: Vec<&str> = payload["expose"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(names, vec!["STRIPE_KEY"]);

    // Empty list clears.
    store.set_expose(app, &[]).await.unwrap();
    let payload = store.merged_env_for_worker(app).await.unwrap();
    assert_eq!(payload["expose"].as_array().unwrap().len(), 0);

    // Bad key rejected.
    let err = store.set_expose(app, &["lowercase".to_string()]).await.unwrap_err();
    assert!(matches!(err, zeroship_control::env_store::EnvError::BadKey(_)));

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
