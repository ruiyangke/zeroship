//! Per-app secrets + vars store. Secrets are encrypted at rest with
//! AES-256-GCM using a key derived from the control-plane master key.
//!
//! Wire format for secrets in Postgres: the `app_secrets.ciphertext`
//! BYTEA column holds `nonce(12) || ciphertext || tag(16)` exactly as
//! produced by `zeroship_core::crypto::encrypt`.

use uuid::Uuid;
use zeroship_core::crypto::{self, CryptoError};

use crate::registry::Registry;

#[derive(Debug)]
pub enum EnvError {
    Db(String),
    Crypto(CryptoError),
    BadKey(String),
}

impl std::fmt::Display for EnvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(m) => write!(f, "{m}"),
            Self::Crypto(e) => write!(f, "crypto: {e}"),
            Self::BadKey(k) => write!(
                f,
                "key must match /^[A-Z][A-Z0-9_]{{0,63}}$/, got '{k}'"
            ),
        }
    }
}

impl std::error::Error for EnvError {}

impl From<CryptoError> for EnvError {
    fn from(e: CryptoError) -> Self { Self::Crypto(e) }
}

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

#[allow(missing_debug_implementations)]
pub struct EnvStore {
    registry: Registry,
    key: [u8; 32],
}

impl EnvStore {
    pub fn new(registry: Registry, master_key: &str) -> Self {
        Self {
            registry,
            key: crypto::derive_key(master_key),
        }
    }

    pub fn registry(&self) -> &Registry { &self.registry }

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
        Ok(n > 0)
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
        let ct = crypto::encrypt(&self.key, value.as_bytes())?;
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
        let mut map = serde_json::Map::new();
        for (k, v) in self.list_vars(app_id).await? {
            map.insert(k, serde_json::Value::String(v));
        }
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| EnvError::Db(format!("{e}")))?;
        let rows = conn
            .query(
                "SELECT key_name, ciphertext FROM app_secrets WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .map_err(|e| EnvError::Db(e.to_string()))?;
        for r in rows.iter() {
            let k: String = r.get("key_name");
            let ct: Vec<u8> = r.get("ciphertext");
            let plain = crypto::decrypt(&self.key, &ct)?;
            let s = String::from_utf8_lossy(&plain).into_owned();
            map.insert(k, serde_json::Value::String(s));
        }
        Ok(map)
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
