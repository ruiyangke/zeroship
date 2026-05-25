//! Per-app column key derivation + cache.
//!
//! ## Architecture
//!
//! A platform deployment has one or more **root keys** addressed by
//! a key id (e.g. `"default"`, `"pii-v2"`). The root key is the bytes
//! the operator generates with `openssl rand -hex 32`; HKDF-SHA256
//! expands it per-(app_id, slot) into the [`AeadKey`] halves:
//!
//! ```text
//! root  = ZEROSHIP_COLUMN_KEY_<KEYID>   (32 bytes, env-sourced)
//! salt  = app_id                        (per-tenant isolation)
//! info  = "zsenc/aead/v1/k_enc"   →  k_enc
//! info  = "zsenc/aead/v1/k_siv"   →  k_siv
//! ```
//!
//! The salt-by-app step closes cross-tenant ciphertext replay even if
//! the root key is shared across apps (which is the P5 platform
//! model — one root per `key_id`, many apps).
//!
//! ## Key sources
//!
//! P5 PR 1 ships one [`KeySource`] variant — env-var lookup. That
//! covers SQLite (where there's no admin-schema sidecar) and the PG
//! dev-parity case. PR 2 adds a `PgAdminTable` variant that reads from
//! `__zeroship_admin.column_keys` via a SECURITY DEFINER getter so
//! the raw bytes never reach app code.
//!
//! ## Cache
//!
//! Single-threaded (`RefCell`) per the workspace's no-tokio invariant
//! — every isolate is bound to one compio thread, so we don't need
//! `Mutex` / `Arc`. The cache invariant is simple: once a `(app_id,
//! key_id)` entry is inserted, it stays for the lifetime of the
//! [`KeyStore`]. Rotation lands in P6b and rewires the cache to
//! track key versions; PR 1 has no rotation surface to worry about.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use hkdf::Hkdf;
use sha2::Sha256;

use super::aead::AeadKey;
use crate::error::DbError;

/// Source of root key material.
///
/// PR 1 shipped only [`Self::EnvVar`]. PR 2 adds
/// [`Self::PgAdminTable`] — the production PG path. The PG path:
///   1. Calls `__zeroship_admin.get_column_key($1)` (SECURITY DEFINER).
///   2. If the getter returns NULL (table empty / key id missing),
///      **falls back to the env-var path** so apps that haven't yet
///      run the column-keys migration still resolve a key.
///
/// `PgAdminTable` is instantiated by `PostgresBackend::new`; the
/// SQLite tier uses `EnvVar`.
#[derive(Debug)]
#[non_exhaustive] // Future variants (Vault, KMS, …) slot in here.
pub enum KeySource {
    /// Read 32-byte root keys from `ZEROSHIP_COLUMN_KEY_<KEYID>` env
    /// vars (hex-encoded). SQLite tier + PG dev parity.
    EnvVar,
    /// **P5 PR 2** — PG production source. Reads
    /// `__zeroship_admin.column_keys` via the SECURITY DEFINER getter
    /// installed by `crate::auth::bootstrap::ensure_admin_schema`.
    /// Falls through to `EnvVar` when the getter returns NULL (covers
    /// the pre-migration / dev-parity case).
    PgAdminTable(std::rc::Rc<compio_postgres::Pool>),
}

/// Per-isolate column-key store. Caches derived `AeadKey` material
/// keyed by `(app_id, key_id)`.
///
/// Construct one per [`crate::backend`] impl; clear on backend drop.
/// PR 2 wires the PG impl through this.
pub struct KeyStore {
    cache: RefCell<HashMap<(String, String), AeadKey>>,
    sourcing: KeySource,
    /// **P5.5 PR 8** — process-local hit/miss counter for the Path-B
    /// "default-read does not load a column key" closeout gate
    /// (§11). Every call to [`KeyStore::resolve`] bumps this; a hit
    /// vs. miss is irrelevant for the gate (the proposal asserts that
    /// `resolve` itself is not called on a default read — neither the
    /// cache nor the env-var path should touch a key when the row is
    /// served through the `<col>_masked AS <col>` alias).
    ///
    /// `Cell<u64>` is sufficient — the `KeyStore` is per-isolate
    /// (single-threaded; see the module-level "Cache" note) and the
    /// counter is purely diagnostic.
    lookups: Cell<u64>,
}

impl std::fmt::Debug for KeyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The cache contents include key material — print nothing
        // beyond the shape so a tracing dump never leaks bytes.
        f.debug_struct("KeyStore")
            .field("sourcing", &self.sourcing)
            .field("cache_entries", &self.cache.borrow().len())
            .field("lookups", &self.lookups.get())
            .finish()
    }
}

impl KeyStore {
    #[must_use]
    pub fn new(sourcing: KeySource) -> Self {
        Self {
            cache: RefCell::new(HashMap::new()),
            sourcing,
            lookups: Cell::new(0),
        }
    }

    /// **P5.5 PR 8** — total resolve-call count since this `KeyStore`
    /// was constructed. Used by the §11 closeout gate
    /// `default_read_does_not_load_column_key` to assert that a
    /// default masked read serves rows through the `<col>_masked`
    /// alias without consulting the key store at all. A non-zero
    /// reading on a default-read path is a regression: the sibling
    /// column is the masked text — the parent ciphertext (and
    /// therefore the key) must never be touched.
    #[must_use]
    pub fn lookups_count(&self) -> u64 {
        self.lookups.get()
    }

    /// Look up or derive the [`AeadKey`] for `(app_id, key_id)`.
    /// Async signature for parity with the PR 2 `PgAdminTable` variant
    /// (which has to `.await` a SECURITY DEFINER round-trip); PR 1's
    /// `EnvVar` source is sync internally and the body never `.await`s.
    #[allow(clippy::unused_async)] // PR 2's PgAdminTable variant awaits.
    pub async fn resolve(&self, app_id: &str, key_id: &str) -> Result<AeadKey, DbError> {
        // **P5.5 PR 8** — increment BEFORE the cache lookup so a
        // resolve-attempt is counted regardless of cache hit/miss.
        // The §11 closeout gate asserts this stays at zero on
        // default-read paths.
        self.lookups.set(self.lookups.get() + 1);
        // Fast path: cache hit. Borrow the RefCell read-only, clone
        // out the AeadKey (two 32-byte arrays — cheap), drop the
        // borrow before any further work.
        {
            let cache = self.cache.borrow();
            if let Some(k) = cache.get(&(app_id.to_string(), key_id.to_string())) {
                return Ok(k.clone());
            }
        }
        let root = match &self.sourcing {
            KeySource::EnvVar => env_lookup_root(key_id)?,
            KeySource::PgAdminTable(pool) => {
                // PG-prod path: call the SECURITY DEFINER getter. NULL
                // result (table empty, key id missing, table itself
                // missing) → fall back to env-var sourcing so a
                // pre-migration app still resolves a key.
                match pg_admin_lookup_root(pool, key_id).await {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => env_lookup_root(key_id)?,
                    Err(_) => env_lookup_root(key_id)?,
                }
            }
        };
        let key = derive_key(&root, app_id)?;
        self.cache.borrow_mut().insert(
            (app_id.to_string(), key_id.to_string()),
            key.clone(),
        );
        Ok(key)
    }
}

/// Read a 32-byte root key from `ZEROSHIP_COLUMN_KEY_<KEYID>`.
/// Returns a typed `Configuration { code: "column_key_not_configured" }`
/// if the env var is missing or malformed — operators see the hint
/// (`openssl rand -hex 32`) in the SDK error surface.
fn env_lookup_root(key_id: &str) -> Result<[u8; 32], DbError> {
    let env_name = format!("ZEROSHIP_COLUMN_KEY_{}", key_id.to_uppercase());
    let hex = std::env::var(&env_name).map_err(|_| DbError::Configuration {
        code: "column_key_not_configured",
        message: format!("Column key '{key_id}' not configured (set {env_name})"),
        hint: Some("Generate via: openssl rand -hex 32".to_string()),
    })?;
    let bytes = hex_decode(&hex).map_err(|e| DbError::Configuration {
        code: "column_key_not_configured",
        message: format!("{env_name}: hex decode failed: {e}"),
        hint: Some("Value must be 64 hex characters (32 bytes). Generate via: openssl rand -hex 32".to_string()),
    })?;
    if bytes.len() != 32 {
        return Err(DbError::Configuration {
            code: "column_key_not_configured",
            message: format!(
                "{env_name} must decode to 32 bytes, got {}",
                bytes.len()
            ),
            hint: Some("Value must be 64 hex characters (32 bytes). Generate via: openssl rand -hex 32".to_string()),
        });
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// HKDF-SHA256 expansion of `root` into `(k_enc, k_siv)`, salted by
/// `app_id` so two apps under the same root produce independent
/// AEAD keys.
fn derive_key(root: &[u8; 32], app_id: &str) -> Result<AeadKey, DbError> {
    let hkdf = Hkdf::<Sha256>::new(Some(app_id.as_bytes()), root);
    let mut k_enc = [0u8; 32];
    let mut k_siv = [0u8; 32];
    hkdf.expand(b"zsenc/aead/v1/k_enc", &mut k_enc)
        .map_err(|_| DbError::internal("HKDF expand k_enc"))?;
    hkdf.expand(b"zsenc/aead/v1/k_siv", &mut k_siv)
        .map_err(|_| DbError::internal("HKDF expand k_siv"))?;
    Ok(AeadKey { k_enc, k_siv })
}

/// Tiny local hex decoder — keeps the encryption module independent
/// of `crate::auth::util` and avoids adding a `hex` crate dependency
/// just for one call site.
fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err(format!("odd-length hex string ({} chars)", s.len()));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = hex_nibble(bytes[i])?;
        let lo = hex_nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!("invalid hex digit {:?}", c as char)),
    }
}

/// **P5 PR 2** — PG production root-key fetcher.
///
/// Calls `__zeroship_admin.get_column_key($1)` (the SECURITY DEFINER
/// getter installed by `crate::auth::bootstrap::ensure_admin_schema`).
/// Returns `Ok(Some(bytes))` on a successful lookup, `Ok(None)` when
/// the getter returns NULL (table empty / row missing), and `Err(...)`
/// when the call itself failed (table doesn't exist, permission denied,
/// connection error). Callers fall through to env-var sourcing on
/// `Ok(None)` OR `Err(_)` so apps that haven't run the column-keys
/// migration still resolve a key.
///
/// The function returns the raw 32-byte root; HKDF expansion happens
/// in `derive_key`.
async fn pg_admin_lookup_root(
    pool: &compio_postgres::Pool,
    key_id: &str,
) -> Result<Option<[u8; 32]>, DbError> {
    // The getter returns `bytea`. compio-postgres' text protocol
    // surfaces bytea as a `\x`-prefixed hex string (the PG default for
    // `bytea_output = hex`). We parse it back to bytes; an empty
    // result set OR a NULL value → `Ok(None)`.
    let rows = pool
        .query_text_params(
            "SELECT __zeroship_admin.get_column_key($1)",
            &[&key_id],
        )
        .await
        .map_err(|e| DbError::Configuration {
            code: "column_key_not_configured",
            message: format!(
                "PG getter call failed for key_id '{key_id}': {e}"
            ),
            hint: Some(
                "Run __zeroship_admin bootstrap migration or set ZEROSHIP_COLUMN_KEY_<KEYID>"
                    .to_string(),
            ),
        })?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    // `try_get` returns `Err` for NULL (a `WasNull` typed error) and
    // for type mismatches; either way we treat the row as "no key
    // present" and let the caller fall through to env-var sourcing.
    // `Row::get` would panic on NULL.
    let s: String = match row.try_get::<_, String>(0) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    if s.is_empty() || s == "NULL" {
        return Ok(None);
    }
    // PG `bytea_output = hex` format: `\xHHHH...`.
    let hex_part = s.strip_prefix("\\x").unwrap_or(&s);
    let Ok(decoded) = hex_decode(hex_part) else {
        return Ok(None);
    };
    if decoded.len() != 32 {
        return Ok(None);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&decoded);
    Ok(Some(out))
}

#[cfg(test)]
#[allow(unsafe_code)] // Tests mutate process-global env via
                     // `std::env::{set_var, remove_var}` (unsafe in
                     // 2024-edition stdlib). Each test uses a
                     // uniquely-named env var so calls don't race with
                     // each other or with other crates. Mirrors the
                     // pattern in `crates/sandbox/src/db.rs::tests`.
mod tests {
    use super::*;

    /// Helper: set an env var, run a closure, unset. Each caller uses
    /// a uniquely-named env var so concurrent tests don't race on the
    /// process-global env table.
    fn with_env<F: FnOnce()>(name: &str, value: &str, f: F) {
        // SAFETY: each test uses a uniquely-named env var; no other
        // crate touches the `ZEROSHIP_COLUMN_KEY_*` namespace.
        unsafe {
            std::env::set_var(name, value);
        }
        f();
        unsafe {
            std::env::remove_var(name);
        }
    }

    /// `derive_key` is deterministic: same `(root, app_id)` always
    /// produces the same `(k_enc, k_siv)`.
    #[test]
    fn derive_key_is_deterministic() {
        let root = [0x42u8; 32];
        let a = derive_key(&root, "app_1").expect("derive");
        let b = derive_key(&root, "app_1").expect("derive");
        assert_eq!(a.k_enc, b.k_enc);
        assert_eq!(a.k_siv, b.k_siv);
    }

    /// Per-app isolation: same root + different `app_id` yields
    /// independent keys. This is the cross-tenant ciphertext-replay
    /// defence at the key layer.
    #[test]
    fn derive_key_per_app_isolation() {
        let root = [0x42u8; 32];
        let a = derive_key(&root, "app_1").expect("derive 1");
        let b = derive_key(&root, "app_2").expect("derive 2");
        assert_ne!(a.k_enc, b.k_enc);
        assert_ne!(a.k_siv, b.k_siv);
    }

    /// Different roots produce different derived keys (sanity).
    #[test]
    fn derive_key_per_root_distinct() {
        let root_a = [0x01u8; 32];
        let root_b = [0x02u8; 32];
        let a = derive_key(&root_a, "app").expect("derive a");
        let b = derive_key(&root_b, "app").expect("derive b");
        assert_ne!(a.k_enc, b.k_enc);
        assert_ne!(a.k_siv, b.k_siv);
    }

    /// `k_enc` and `k_siv` are distinct within one derivation —
    /// they share a root but the `info` strings differ so HKDF
    /// expands different bytes. A regression here would mean the
    /// two halves alias and a `k_siv` leak compromises `k_enc`.
    #[test]
    fn derive_key_halves_are_distinct() {
        let root = [0x42u8; 32];
        let key = derive_key(&root, "app").expect("derive");
        assert_ne!(key.k_enc, key.k_siv);
    }

    /// Missing env var → typed `Configuration` error with the
    /// `column_key_not_configured` code.
    #[test]
    fn missing_env_var_yields_typed_error() {
        // Use a key_id no test sets, so the env var is guaranteed
        // missing.
        let err = env_lookup_root("missing_test_key_xyz")
            .expect_err("missing env var must error");
        match err {
            DbError::Configuration { code, hint, .. } => {
                assert_eq!(code, "column_key_not_configured");
                assert!(hint.is_some(), "hint must include openssl-rand suggestion");
            }
            other => panic!("expected Configuration, got {other:?}"),
        }
    }

    /// Malformed hex (odd length) → typed Configuration error.
    #[test]
    fn malformed_hex_yields_typed_error() {
        with_env(
            "ZEROSHIP_COLUMN_KEY_BAD_HEX_TEST",
            "abc", // odd length
            || {
                let err = env_lookup_root("bad_hex_test")
                    .expect_err("odd-length hex must error");
                match err {
                    DbError::Configuration { code, .. } => {
                        assert_eq!(code, "column_key_not_configured");
                    }
                    other => panic!("expected Configuration, got {other:?}"),
                }
            },
        );
    }

    /// Hex with the wrong number of bytes (after decode) → typed
    /// Configuration error.
    #[test]
    fn wrong_length_hex_yields_typed_error() {
        with_env(
            "ZEROSHIP_COLUMN_KEY_WRONG_LEN_TEST",
            // 16 hex chars = 8 bytes, not 32.
            "0123456789abcdef",
            || {
                let err = env_lookup_root("wrong_len_test")
                    .expect_err("8-byte hex must error");
                match err {
                    DbError::Configuration { code, message, .. } => {
                        assert_eq!(code, "column_key_not_configured");
                        assert!(
                            message.contains("32 bytes"),
                            "message must name the expected length, got: {message}"
                        );
                    }
                    other => panic!("expected Configuration, got {other:?}"),
                }
            },
        );
    }

    /// Hex with invalid characters → typed Configuration error.
    #[test]
    fn invalid_hex_chars_yield_typed_error() {
        with_env(
            "ZEROSHIP_COLUMN_KEY_BAD_CHARS_TEST",
            "zzzz000000000000000000000000000000000000000000000000000000000000",
            || {
                let err = env_lookup_root("bad_chars_test")
                    .expect_err("non-hex chars must error");
                match err {
                    DbError::Configuration { code, .. } => {
                        assert_eq!(code, "column_key_not_configured");
                    }
                    other => panic!("expected Configuration, got {other:?}"),
                }
            },
        );
    }

    /// KeyStore caches: a second `resolve` for the same `(app_id,
    /// key_id)` must hit the cache. We test indirectly: insert a
    /// well-formed env var, resolve once, then UNSET the env var
    /// and resolve again — the second call still succeeds (proves
    /// the cache short-circuited the env lookup).
    #[test]
    fn key_store_caches_derived_key() {
        let env_name = "ZEROSHIP_COLUMN_KEY_CACHE_TEST";
        let hex = "0".repeat(64); // 64 hex chars = 32 bytes
        unsafe {
            std::env::set_var(env_name, &hex);
        }
        let store = KeyStore::new(KeySource::EnvVar);
        let rt = compio::runtime::Runtime::new().expect("compio runtime");
        let app_id = "app_cache_test";
        let key_id = "cache_test";

        let first = rt.block_on(async { store.resolve(app_id, key_id).await });
        assert!(first.is_ok(), "first resolve must succeed: {first:?}");

        // Clear the env — second resolve must still succeed via
        // the cache.
        unsafe {
            std::env::remove_var(env_name);
        }
        let second = rt.block_on(async { store.resolve(app_id, key_id).await });
        assert!(
            second.is_ok(),
            "second resolve must hit cache (env now empty): {second:?}"
        );

        // The returned keys must match byte-for-byte.
        let k1 = first.unwrap();
        let k2 = second.unwrap();
        assert_eq!(k1.k_enc, k2.k_enc);
        assert_eq!(k1.k_siv, k2.k_siv);
    }

    /// KeyStore caches per `(app_id, key_id)`, not just `key_id`:
    /// two apps under the same key id must produce different
    /// derived keys.
    #[test]
    fn key_store_per_app_isolation() {
        let env_name = "ZEROSHIP_COLUMN_KEY_MULTI_APP_TEST";
        let hex = "1".repeat(64);
        unsafe {
            std::env::set_var(env_name, &hex);
        }
        let store = KeyStore::new(KeySource::EnvVar);
        let rt = compio::runtime::Runtime::new().expect("compio runtime");
        let key_id = "multi_app_test";
        let k1 = rt.block_on(async { store.resolve("app_a", key_id).await }).unwrap();
        let k2 = rt.block_on(async { store.resolve("app_b", key_id).await }).unwrap();
        assert_ne!(k1.k_enc, k2.k_enc);
        unsafe {
            std::env::remove_var(env_name);
        }
    }

    /// Local hex_decode round-trips.
    #[test]
    fn local_hex_decode_round_trip() {
        let bytes = [0xde, 0xad, 0xbe, 0xef, 0x00, 0xff];
        let s = "deadbeef00ff";
        assert_eq!(hex_decode(s).unwrap(), bytes);
        assert_eq!(hex_decode("DEADBEEF00FF").unwrap(), bytes);
    }

    /// Local hex_decode rejects odd-length input.
    #[test]
    fn local_hex_decode_rejects_odd_length() {
        assert!(hex_decode("abc").is_err());
    }

    /// Local hex_decode rejects non-hex characters.
    #[test]
    fn local_hex_decode_rejects_garbage() {
        assert!(hex_decode("xy").is_err());
    }

    // -----------------------------------------------------------------
    // P5.5 PR 8 — §11 closeout: `default_read_does_not_load_column_key`
    // (Path B fence). The proposal asserts that a default masked read
    // serves rows through the `<col>_masked AS <col>` alias so the
    // ciphertext column never leaves Postgres and the column key is
    // never consulted. The `KeyStore::lookups_count()` counter is the
    // diagnostic surface for that invariant: it records every call to
    // `resolve()` regardless of cache hit/miss.
    //
    // The unit tests below pin the counter contract. The end-to-end
    // SELECT-shape gate (`default_read_does_not_touch_ciphertext_column`)
    // lives in `query.rs` and asserts the SELECT clause directly
    // (sibling alias substitution, ciphertext column absent).
    // -----------------------------------------------------------------

    /// Freshly-constructed `KeyStore` reports zero lookups.
    #[test]
    fn key_store_lookups_count_starts_at_zero() {
        let store = KeyStore::new(KeySource::EnvVar);
        assert_eq!(store.lookups_count(), 0);
    }

    /// Every `resolve()` call bumps the counter — hit or miss.
    /// Pins the §11 "default read does not load a column key" gate:
    /// production code paths that should not consult the key store
    /// can assert `store.lookups_count() == 0` AFTER the SELECT.
    #[test]
    fn key_store_lookups_count_bumps_on_every_resolve() {
        let env_name = "ZEROSHIP_COLUMN_KEY_LOOKUPS_COUNTER_TEST";
        let hex = "2".repeat(64);
        unsafe {
            std::env::set_var(env_name, &hex);
        }
        let store = KeyStore::new(KeySource::EnvVar);
        let rt = compio::runtime::Runtime::new().expect("compio runtime");

        assert_eq!(store.lookups_count(), 0, "fresh store starts at zero");

        // First call — cache miss, env lookup, derive.
        rt.block_on(async {
            store
                .resolve("app_counter", "lookups_counter_test")
                .await
                .unwrap();
        });
        assert_eq!(
            store.lookups_count(),
            1,
            "first resolve must bump counter to 1"
        );

        // Second call — cache hit; the counter STILL bumps so the
        // diagnostic captures key-store traffic, not just key
        // derivations.
        rt.block_on(async {
            store
                .resolve("app_counter", "lookups_counter_test")
                .await
                .unwrap();
        });
        assert_eq!(
            store.lookups_count(),
            2,
            "cache hit must still bump counter"
        );

        unsafe {
            std::env::remove_var(env_name);
        }
    }

    /// Failed `resolve()` calls (env var missing) still bump the
    /// counter — the gate cares about whether the production code
    /// path TRIED to load a key, not whether the load succeeded.
    #[test]
    fn key_store_lookups_count_bumps_on_failed_resolve() {
        let store = KeyStore::new(KeySource::EnvVar);
        let rt = compio::runtime::Runtime::new().expect("compio runtime");

        // Use a unique env var name that is guaranteed not to exist.
        let err = rt
            .block_on(async { store.resolve("app_x", "absent_for_counter_test").await });
        assert!(err.is_err(), "absent env var must error");
        assert_eq!(
            store.lookups_count(),
            1,
            "failed resolve still bumps counter (gate semantics)"
        );
    }
}
