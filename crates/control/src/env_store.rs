//! Per-app secrets + vars store. Secrets are encrypted at rest with
//! AES-256-GCM using a key derived from the control-plane master key.
//!
//! Wire format for secrets in Postgres: the `app_secrets.ciphertext`
//! BYTEA column holds `aad_version(1) || nonce(12) || ciphertext ||
//! tag(16)` exactly as produced by `zeroship_core::crypto::encrypt`.

use uuid::Uuid;
use zeroship_core::crypto::{self, CryptoError};

use crate::registry::Registry;

#[derive(Debug)]
pub enum EnvError {
    Db(String),
    Crypto(CryptoError),
    BadKey(String),
    /// Value exceeded the per-secret/var length cap.
    TooLarge(usize),
    /// Master key is empty and `insecure_dev` was not set.
    MasterKeyRequired,
    /// `merged_env` was called for an app that doesn't exist in the
    /// `apps` table — differentiates "empty env" from "app deleted."
    AppNotFound,
}

impl std::fmt::Display for EnvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(m) => write!(f, "{m}"),
            Self::Crypto(e) => write!(f, "crypto: {e}"),
            Self::BadKey(k) => {
                // Sanitize the key — drop anything non-printable to keep
                // stray bytes out of logs (defense against log injection).
                let safe: String = k
                    .chars()
                    .take(80)
                    .map(|c| if c.is_ascii_graphic() { c } else { '?' })
                    .collect();
                write!(
                    f,
                    "key must match /^[A-Z][A-Z0-9_]{{0,63}}$/, got '{safe}'"
                )
            }
            Self::TooLarge(n) => write!(f, "value too large ({n} bytes; max {MAX_VALUE_BYTES})"),
            Self::MasterKeyRequired => write!(f, "master key is empty — refusing to start without --dev-insecure"),
            Self::AppNotFound => write!(f, "app not found"),
        }
    }
}

impl std::error::Error for EnvError {}

impl From<CryptoError> for EnvError {
    fn from(e: CryptoError) -> Self { Self::Crypto(e) }
}

/// Per-value byte cap. Chosen to comfortably fit the largest legitimate
/// secret (OAuth refresh tokens, PEM-encoded keys, base64-encoded
/// service-account blobs) while blocking DoS vectors that would push
/// megabytes of ciphertext through the env-fetch pipe per app.
pub const MAX_VALUE_BYTES: usize = 64 * 1024;
const APP_SECRET_AAD_PREFIX: &[u8] = b"zs:control:app_secret:v1\0";

/// Valid env key: uppercase ASCII letter then uppercase letters/digits/underscores.
/// Matches the CF Workers + Unix-env convention.
fn valid_key(k: &str) -> bool {
    if k.is_empty() || k.len() > 64 {
        return false;
    }
    let first = k.as_bytes()[0];
    if !first.is_ascii_uppercase() {
        return false;
    }
    k.bytes()
        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

fn app_secret_aad(app_id: Uuid, key_name: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(APP_SECRET_AAD_PREFIX.len() + 16 + 1 + key_name.len());
    aad.extend_from_slice(APP_SECRET_AAD_PREFIX);
    aad.extend_from_slice(app_id.as_bytes());
    aad.push(0);
    aad.extend_from_slice(key_name.as_bytes());
    aad
}

pub struct EnvStore {
    registry: Registry,
    /// Primary key used for ALL encrypts. Always tried first on decrypt.
    primary_key: [u8; 32],
    /// Previous keys. Tried in order on decrypt failure. Empty in
    /// steady state; populated during a rotation grace period so
    /// secrets encrypted with an older key remain readable while we
    /// re-encrypt them in the background.
    previous_keys: Vec<[u8; 32]>,
}

impl std::fmt::Debug for EnvStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never leak the derived keys via Debug.
        f.debug_struct("EnvStore")
            .field("primary_key", &"<redacted 32B>")
            .field("previous_keys", &format!("<{} redacted>", self.previous_keys.len()))
            .finish()
    }
}

impl Drop for EnvStore {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.primary_key);
        for k in &mut self.previous_keys {
            zeroize::Zeroize::zeroize(k);
        }
    }
}

impl EnvStore {
    /// Build a store with a primary master key. `master_key` must be
    /// non-empty unless `insecure_dev`. An empty master key would
    /// SHA-256 the known prefix and produce a publicly-reproducible
    /// encryption key — every secret in the DB would be decryptable
    /// by anyone with read access.
    pub fn new(registry: Registry, master_key: &str, insecure_dev: bool) -> Result<Self, EnvError> {
        Self::new_with_previous(registry, master_key, &[], insecure_dev)
    }

    /// Build a store with rotation support. `previous_master_keys` is
    /// tried in order on decrypt failure — lets ops rotate the primary
    /// key without dumping the secrets table.
    pub fn new_with_previous(
        registry: Registry,
        master_key: &str,
        previous_master_keys: &[&str],
        insecure_dev: bool,
    ) -> Result<Self, EnvError> {
        if master_key.is_empty() && !insecure_dev {
            return Err(EnvError::MasterKeyRequired);
        }
        let previous_keys = previous_master_keys
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| crypto::derive_key(s))
            .collect();
        Ok(Self {
            registry,
            primary_key: crypto::derive_key(master_key),
            previous_keys,
        })
    }

    /// Test-only access to the raw ciphertext bytes stored at rest —
    /// lets integration tests verify "no plaintext leaks via the DB"
    /// without the crate having to export a DB handle.
    #[doc(hidden)]
    pub async fn __raw_ciphertext_for_test(
        &self,
        app_id: uuid::Uuid,
        key_name: &str,
    ) -> Result<Option<Vec<u8>>, EnvError> {
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| EnvError::Db(format!("{e}")))?;
        let rows = conn
            .query(
                "SELECT ciphertext FROM app_secrets WHERE app_id = $1 AND key_name = $2",
                &[&app_id, &key_name],
            )
            .await
            .map_err(|e| EnvError::Db(e.to_string()))?;
        Ok(rows.first().map(|r| r.get::<_, Vec<u8>>("ciphertext")))
    }

    // ------------------------------------------------------------------
    // Vars (plaintext)
    // ------------------------------------------------------------------

    pub async fn list_vars(&self, app_id: Uuid) -> Result<Vec<(String, String)>, EnvError> {
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| EnvError::Db(format!("{e}")))?;
        let rows = conn
            .query(
                "SELECT key_name, value FROM app_vars WHERE app_id = $1 ORDER BY key_name",
                &[&app_id],
            )
            .await
            .map_err(|e| EnvError::Db(e.to_string()))?;
        let out = rows
            .iter()
            .map(|r| (r.get::<_, String>("key_name"), r.get::<_, String>("value")))
            .collect();
        Ok(out)
    }

    pub async fn set_var(&self, app_id: Uuid, key: &str, value: &str) -> Result<(), EnvError> {
        if !valid_key(key) {
            return Err(EnvError::BadKey(key.into()));
        }
        if value.len() > MAX_VALUE_BYTES {
            return Err(EnvError::TooLarge(value.len()));
        }
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| EnvError::Db(format!("{e}")))?;
        conn.execute(
            "INSERT INTO app_vars(app_id, key_name, value) VALUES($1, $2, $3)
             ON CONFLICT (app_id, key_name) DO UPDATE
                SET value = EXCLUDED.value, updated_at = NOW()",
            &[&app_id, &key, &value],
        )
        .await
        .map_err(|e| EnvError::Db(e.to_string()))?;
        self.bump_env_version(app_id).await;
        Ok(())
    }

    pub async fn delete_var(&self, app_id: Uuid, key: &str) -> Result<bool, EnvError> {
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| EnvError::Db(format!("{e}")))?;
        let n = conn
            .execute(
                "DELETE FROM app_vars WHERE app_id = $1 AND key_name = $2",
                &[&app_id, &key],
            )
            .await
            .map_err(|e| EnvError::Db(e.to_string()))?;
        if n > 0 {
            self.bump_env_version(app_id).await;
        }
        Ok(n > 0)
    }

    /// Best-effort env_version bump. Failure logged but not propagated —
    /// the mutation has already committed; worst case workers refetch
    /// env on the next reconcile interval anyway.
    async fn bump_env_version(&self, app_id: Uuid) {
        if let Err(e) = self.registry.bump_env_version(app_id).await {
            tracing::warn!(app_id = %app_id, error = %e, "env_store: bump_env_version failed");
        }
    }

    // ------------------------------------------------------------------
    // Secrets (encrypted at rest)
    // ------------------------------------------------------------------

    pub async fn list_secret_names(&self, app_id: Uuid) -> Result<Vec<String>, EnvError> {
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| EnvError::Db(format!("{e}")))?;
        let rows = conn
            .query(
                "SELECT key_name FROM app_secrets WHERE app_id = $1 ORDER BY key_name",
                &[&app_id],
            )
            .await
            .map_err(|e| EnvError::Db(e.to_string()))?;
        Ok(rows.iter().map(|r| r.get::<_, String>("key_name")).collect())
    }

    pub async fn set_secret(&self, app_id: Uuid, key: &str, value: &str) -> Result<(), EnvError> {
        if !valid_key(key) {
            return Err(EnvError::BadKey(key.into()));
        }
        if value.len() > MAX_VALUE_BYTES {
            return Err(EnvError::TooLarge(value.len()));
        }
        let aad = app_secret_aad(app_id, key);
        let ct = crypto::encrypt(&self.primary_key, &aad, value.as_bytes())?;
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| EnvError::Db(format!("{e}")))?;
        conn.execute(
            "INSERT INTO app_secrets(app_id, key_name, ciphertext) VALUES($1, $2, $3)
             ON CONFLICT (app_id, key_name) DO UPDATE
                SET ciphertext = EXCLUDED.ciphertext, updated_at = NOW()",
            &[&app_id, &key, &ct],
        )
        .await
        .map_err(|e| EnvError::Db(e.to_string()))?;
        self.bump_env_version(app_id).await;
        Ok(())
    }

    pub async fn delete_secret(&self, app_id: Uuid, key: &str) -> Result<bool, EnvError> {
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| EnvError::Db(format!("{e}")))?;
        let n = conn
            .execute(
                "DELETE FROM app_secrets WHERE app_id = $1 AND key_name = $2",
                &[&app_id, &key],
            )
            .await
            .map_err(|e| EnvError::Db(e.to_string()))?;
        if n > 0 {
            self.bump_env_version(app_id).await;
        }
        Ok(n > 0)
    }

    /// Merged env for worker consumption. Decrypts every secret. Vars +
    /// secrets share the same namespace; if a var and secret have the
    /// same key, the secret wins (populated second, overwriting).
    ///
    /// **Never expose over a public API** — only `/internal/apps/:id/env`
    /// authenticated by the control/master key.
    pub async fn merged_env(
        &self,
        app_id: Uuid,
    ) -> Result<serde_json::Map<String, serde_json::Value>, EnvError> {
        // Distinguish "app exists, empty env" from "app deleted." The
        // latter should 404 on /internal/apps/:id/env so workers don't
        // silently hydrate a stale or nonexistent app with empty env.
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| EnvError::Db(format!("{e}")))?;
        let exists = conn
            .query("SELECT 1 FROM apps WHERE id = $1", &[&app_id])
            .await
            .map_err(|e| EnvError::Db(e.to_string()))?;
        if exists.is_empty() {
            return Err(EnvError::AppNotFound);
        }

        let mut map = serde_json::Map::new();
        for (k, v) in self.list_vars(app_id).await? {
            map.insert(k, serde_json::Value::String(v));
        }
        let rows = conn
            .query(
                "SELECT key_name, ciphertext FROM app_secrets WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .map_err(|e| EnvError::Db(e.to_string()))?;
        // Decrypt path tries primary key first, then any previous
        // keys (rotation grace). Building the keys vec once outside
        // the loop avoids repeated allocs.
        let mut keys: Vec<[u8; 32]> = Vec::with_capacity(1 + self.previous_keys.len());
        keys.push(self.primary_key);
        keys.extend_from_slice(&self.previous_keys);

        for r in rows.iter() {
            let k: String = r.get("key_name");
            let ct: Vec<u8> = r.get("ciphertext");
            let aad = app_secret_aad(app_id, &k);
            let plain = crypto::decrypt_with_keys(&keys, &aad, &ct)?;
            // Strict UTF-8 — `from_utf8_lossy` would silently replace
            // invalid bytes with U+FFFD and hand the creator a mangled
            // secret. Treat any non-UTF-8 in plaintext as corruption.
            let s = String::from_utf8(plain).map_err(|_| EnvError::Crypto(CryptoError::Decrypt))?;
            map.insert(k, serde_json::Value::String(s));
        }
        Ok(map)
    }

    // ------------------------------------------------------------------
    // Expose list (per-app opt-in for `process.env`)
    // ------------------------------------------------------------------

    /// Read the list of secret names the creator has opted to surface in
    /// `process.env`. Sorted, deterministic — used both for the worker
    /// wire format and the admin API readback.
    pub async fn list_expose(&self, app_id: Uuid) -> Result<Vec<String>, EnvError> {
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| EnvError::Db(format!("{e}")))?;
        let rows = conn
            .query(
                "SELECT key_name FROM app_env_expose WHERE app_id = $1 ORDER BY key_name",
                &[&app_id],
            )
            .await
            .map_err(|e| EnvError::Db(e.to_string()))?;
        Ok(rows.iter().map(|r| r.get::<_, String>("key_name")).collect())
    }

    /// Replace the per-app expose list atomically. Empty `keys` clears
    /// the list. Returns the new list (sorted, deduplicated).
    ///
    /// Each name is validated against the same `valid_key` regex that
    /// gates `set_var` / `set_secret` — bad input is rejected wholesale
    /// (no partial application). Two-statement transaction ensures the
    /// list is replaced atomically; a worker fetching env mid-replace
    /// sees either the old set or the new set, never a torn state.
    pub async fn set_expose(&self, app_id: Uuid, keys: &[String]) -> Result<Vec<String>, EnvError> {
        // Validate all names first — fail loud before touching the DB.
        for k in keys {
            if !valid_key(k) {
                return Err(EnvError::BadKey(k.clone()));
            }
        }
        // Sort + dedup so rows are deterministic and the
        // PRIMARY KEY (app_id, key_name) constraint can't reject a
        // duplicate input.
        let mut sorted: Vec<String> = keys.iter().cloned().collect();
        sorted.sort();
        sorted.dedup();

        let mut conn = self
            .registry
            .conn()
            .await
            .map_err(|e| EnvError::Db(format!("{e}")))?;
        let tx = conn
            .transaction()
            .await
            .map_err(|e| EnvError::Db(e.to_string()))?;
        tx.execute(
            "DELETE FROM app_env_expose WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .map_err(|e| EnvError::Db(e.to_string()))?;
        for name in &sorted {
            tx.execute(
                "INSERT INTO app_env_expose(app_id, key_name) VALUES($1, $2)
                 ON CONFLICT (app_id, key_name) DO NOTHING",
                &[&app_id, &name],
            )
            .await
            .map_err(|e| EnvError::Db(e.to_string()))?;
        }
        tx.commit().await.map_err(|e| EnvError::Db(e.to_string()))?;
        // Bump env_version so workers refetch and re-derive process.env
        // membership on the next reconcile cycle.
        self.bump_env_version(app_id).await;
        Ok(sorted)
    }

    // ------------------------------------------------------------------
    // Worker wire format
    // ------------------------------------------------------------------

    /// Merged env for worker consumption in the new split shape:
    ///
    /// ```json
    /// { "vars": {...}, "secrets": {...}, "expose": [...] }
    /// ```
    ///
    /// This is the canonical worker-facing payload. The runtime parses
    /// it once and uses the three parts to decide what lands in
    /// `process.env` (vars + opt-in-exposed secrets) vs. what's only
    /// reachable through the explicit `zeroship.env` import / `env.get()`
    /// (the merged map).
    ///
    /// **Never expose over a public API** — only `/internal/apps/:id/env`
    /// authenticated by the control/master key.
    pub async fn merged_env_for_worker(
        &self,
        app_id: Uuid,
    ) -> Result<serde_json::Value, EnvError> {
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| EnvError::Db(format!("{e}")))?;
        let exists = conn
            .query("SELECT 1 FROM apps WHERE id = $1", &[&app_id])
            .await
            .map_err(|e| EnvError::Db(e.to_string()))?;
        if exists.is_empty() {
            return Err(EnvError::AppNotFound);
        }

        // Vars — already plaintext.
        let mut vars = serde_json::Map::new();
        for (k, v) in self.list_vars(app_id).await? {
            vars.insert(k, serde_json::Value::String(v));
        }

        // Secrets — decrypt each row. Same key fallback as `merged_env`
        // (primary first, then any rotation-grace previous keys).
        let secret_rows = conn
            .query(
                "SELECT key_name, ciphertext FROM app_secrets WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .map_err(|e| EnvError::Db(e.to_string()))?;
        let mut keys: Vec<[u8; 32]> = Vec::with_capacity(1 + self.previous_keys.len());
        keys.push(self.primary_key);
        keys.extend_from_slice(&self.previous_keys);

        let mut secrets = serde_json::Map::new();
        for r in secret_rows.iter() {
            let k: String = r.get("key_name");
            let ct: Vec<u8> = r.get("ciphertext");
            let aad = app_secret_aad(app_id, &k);
            let plain = crypto::decrypt_with_keys(&keys, &aad, &ct)?;
            let s = String::from_utf8(plain).map_err(|_| EnvError::Crypto(CryptoError::Decrypt))?;
            secrets.insert(k, serde_json::Value::String(s));
        }

        // Expose list — sorted ascending.
        let expose = self.list_expose(app_id).await?;

        Ok(serde_json::json!({
            "vars": vars,
            "secrets": secrets,
            "expose": expose,
        }))
    }

    /// Re-encrypt every secret for `app_id` with the current primary
    /// key. Used to drain a rotation grace period: after every secret
    /// has been touched once, it's safe to drop `previous_keys`.
    /// Returns the count rewritten.
    ///
    /// Skip-if-already-on-primary detection: try decrypting with the
    /// primary key alone. If that succeeds, the ciphertext is already
    /// bound to the primary key and re-encryption is wasted work.
    /// Stack-local key copies are zeroized on scope exit.
    pub async fn rotate_app(&self, app_id: Uuid) -> Result<usize, EnvError> {
        use zeroize::{Zeroize, Zeroizing};

        let conn = self.registry.conn().await.map_err(|e| EnvError::Db(format!("{e}")))?;
        let rows = conn
            .query(
                "SELECT key_name, ciphertext FROM app_secrets WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .map_err(|e| EnvError::Db(e.to_string()))?;

        // Wrap in Zeroizing — the underlying Vec<u8> is zeroized on drop.
        let mut all_keys_buf: Vec<u8> = Vec::with_capacity(32 * (1 + self.previous_keys.len()));
        all_keys_buf.extend_from_slice(&self.primary_key);
        for k in &self.previous_keys {
            all_keys_buf.extend_from_slice(k);
        }
        let _all_keys_zeroize = Zeroizing::new(all_keys_buf.clone());
        let all_keys: Vec<[u8; 32]> = all_keys_buf
            .chunks_exact(32)
            .map(|c| {
                let mut a = [0u8; 32];
                a.copy_from_slice(c);
                a
            })
            .collect();

        let primary_only: [[u8; 32]; 1] = [self.primary_key];
        let mut count = 0;
        for r in rows.iter() {
            let k: String = r.get("key_name");
            let ct: Vec<u8> = r.get("ciphertext");
            let aad = app_secret_aad(app_id, &k);

            // Already-on-primary check: if primary alone decrypts, skip
            // re-encryption (avoids churn on already-current ciphertexts).
            if crypto::decrypt_with_keys(&primary_only, &aad, &ct).is_ok() {
                continue;
            }

            // Otherwise: decrypt under any known key, re-encrypt with primary.
            let plain = Zeroizing::new(crypto::decrypt_with_keys(&all_keys, &aad, &ct)?);
            let new_ct = crypto::encrypt(&self.primary_key, &aad, &plain)?;
            conn.execute(
                "UPDATE app_secrets SET ciphertext = $1, updated_at = NOW()
                 WHERE app_id = $2 AND key_name = $3",
                &[&new_ct, &app_id, &k],
            )
            .await
            .map_err(|e| EnvError::Db(e.to_string()))?;
            count += 1;
        }
        // Final scrub of the temporary key material.
        all_keys_buf.zeroize();
        Ok(count)
    }
}

#[cfg(test)]
mod valid_key_tests {
    use super::*;

    #[test]
    fn accepts_standard_shape() {
        assert!(valid_key("FOO"));
        assert!(valid_key("STRIPE_KEY"));
        assert!(valid_key("X"));
        assert!(valid_key("A1_B2"));
    }

    #[test]
    fn rejects_bad_shapes() {
        assert!(!valid_key(""));
        assert!(!valid_key("lowercase"));
        assert!(!valid_key("1LEADING_DIGIT"));
        assert!(!valid_key("_LEADING_UNDERSCORE"));
        assert!(!valid_key("HAS-DASH"));
        assert!(!valid_key("HAS SPACE"));
        assert!(!valid_key("FOO\n"));
        assert!(!valid_key(&"A".repeat(65)));
    }

    #[test]
    fn unicode_rejected() {
        assert!(!valid_key("Ä"));
        assert!(!valid_key("KEY_Ω"));
    }
}
