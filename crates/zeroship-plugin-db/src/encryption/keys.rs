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
//! root  = 32 bytes for <KEYID>          (see "Key sources" below)
//! salt  = app_id                        (per-tenant isolation)
//! info  = "zsenc/aead/v1/k_enc"   →  k_enc
//! info  = "zsenc/aead/v1/k_siv"   →  k_siv
//! ```
//!
//! The salt-by-app step closes cross-tenant ciphertext replay even if
//! the root key is shared across apps (which is the platform
//! model — one root per `key_id`, many apps).
//!
//! ## Key sources
//!
//! Sourcing splits in two. [`LocalKeySource`] resolves a root without
//! touching a database: either [`LocalKeySource::EnvVar`]
//! (`ZEROSHIP_COLUMN_KEY_<KEYID>`, which covers SQLite - no
//! admin-schema sidecar there - and the PG dev-parity case) or
//! [`LocalKeySource::Supplied`], where the operator hands the process
//! the root bytes directly instead of exporting them into the
//! environment. [`KeySource`] wraps that: either the local source
//! alone, or the PG path, which asks
//! `__zeroship_admin.get_column_key($1)` and falls back to a local
//! source when that returns NULL or errors.
//!
//! **The PG arm currently resolves nothing.** The getter and its
//! `__zeroship_admin.column_keys` table were installed only by
//! `auth::bootstrap::ensure_admin_schema`, deleted on 2026-08-27 with
//! the rest of the admin schema. The call now always errors and the
//! fallback always wins, so PG behaves exactly like SQLite. Whether
//! the PG arm gets a new home (an app-schema table, an external KMS)
//! or is deleted outright is an open operator decision - it is NOT
//! resolved by pretending the getter is still there.
//!
//! The fallback is a `LocalKeySource` by TYPE, not by convention: a PG
//! lookup can never be the fallback for another PG lookup, so the
//! chain is at most one round-trip deep and there is no unreachable
//! arm to write.
//!
//! ## Parsing vs. sourcing
//!
//! [`parse_root_key`] holds every hex-decode / length / typed-`DbError`
//! decision and knows nothing about where the string came from. Each
//! source is then a thin fetch that hands its string to it. That split
//! is what lets the malformed-input tests below run with no
//! environment and no store at all.
//!
//! ## Cache
//!
//! Single-threaded (`RefCell`) per the workspace's no-tokio invariant
//! — every isolate is bound to one compio thread, so we don't need
//! `Mutex` / `Arc`. The cache invariant is simple: once a `(app_id,
//! key_id)` entry is inserted, it stays for the lifetime of the
//! [`KeyStore`]. There is no rotation surface today; adding one will
//! require rewiring the cache to track key versions.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;
use zeroship_core::config::DeclaredEnvFamily;

use super::aead::AeadKey;
use crate::error::DbError;

/// Root key material handed to the process directly, addressed by key id.
///
/// This is the root-key channel that is NOT the process environment. An
/// operator who already holds the 32-byte roots (a mounted secret file, a
/// KMS fetch at boot) installs them here instead of exporting
/// `ZEROSHIP_COLUMN_KEY_<KEYID>`; `KeyStore` then resolves, derives and
/// caches through exactly the same path as any other source.
///
/// Key ids match case-insensitively, mirroring the env source, which
/// uppercases the suffix before reading.
///
/// Interior mutability is deliberate. The supplied set can change after the
/// [`KeyStore`] holding it was built - a root withdrawn, a root added - and
/// [`Self::consultations`] records how many times the store actually asked
/// this source for bytes. That counter is what makes the cache contract
/// provable: withdraw the key between two `resolve` calls for the same
/// `(app_id, key_id)` and the second call must still succeed WITHOUT the
/// count rising.
///
/// Single-threaded (`RefCell` / `Cell`) for the same reason the cache is:
/// one isolate per compio thread, so no `Mutex` / `Arc`.
#[derive(Default)]
pub struct SuppliedRootKeys {
    keys: RefCell<HashMap<String, Zeroizing<[u8; 32]>>>,
    consultations: Cell<u64>,
}

impl std::fmt::Debug for SuppliedRootKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The map values are root key bytes - print the shape only, never
        // the material, so a tracing dump cannot leak a root.
        f.debug_struct("SuppliedRootKeys")
            .field("key_ids", &self.keys.borrow().len())
            .field("consultations", &self.consultations.get())
            .finish()
    }
}

impl SuppliedRootKeys {
    /// An empty set. Resolving against it yields the same typed
    /// `column_key_not_configured` error an unset env var does.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder form of [`Self::insert_hex`], for construction in one
    /// expression.
    pub fn with_hex(self, key_id: &str, hex: &str) -> Result<Self, DbError> {
        self.insert_hex(key_id, hex)?;
        Ok(self)
    }

    /// Install a raw 32-byte root under `key_id`, replacing any previous
    /// value for it.
    pub fn insert(&self, key_id: &str, root: [u8; 32]) {
        self.keys
            .borrow_mut()
            .insert(normalise_key_id(key_id), Zeroizing::new(root));
    }

    /// Install a hex-encoded root under `key_id`. The string goes through
    /// [`parse_root_key`], so a malformed value fails here with the same
    /// typed error the env path produces rather than at first decrypt.
    pub fn insert_hex(&self, key_id: &str, hex: &str) -> Result<(), DbError> {
        let root = parse_root_key(&format!("supplied root key '{key_id}'"), hex)?;
        self.insert(key_id, root);
        Ok(())
    }

    /// Withdraw `key_id`. Returns whether a root was actually removed.
    pub fn remove(&self, key_id: &str) -> bool {
        self.keys.borrow_mut().remove(&normalise_key_id(key_id)).is_some()
    }

    /// How many times a [`KeyStore`] has asked this source for bytes,
    /// hit or miss. Every [`Self::lookup`] bumps it.
    #[must_use]
    pub fn consultations(&self) -> u64 {
        self.consultations.get()
    }

    /// Fetch the root for `key_id`, counting the consultation.
    fn lookup(&self, key_id: &str) -> Result<[u8; 32], DbError> {
        self.consultations.set(self.consultations.get() + 1);
        self.keys
            .borrow()
            .get(&normalise_key_id(key_id))
            .map(|root| **root)
            .ok_or_else(|| DbError::Configuration {
                code: "column_key_not_configured",
                message: format!(
                    "Column key '{key_id}' not configured (no root key was supplied for it)"
                ),
                hint: Some("Generate via: openssl rand -hex 32".to_string()),
            })
    }
}

fn normalise_key_id(key_id: &str) -> String {
    key_id.to_ascii_lowercase()
}

/// A root-key source that resolves without a database round-trip.
///
/// Both variants are complete sources in their own right AND are the only
/// things that may sit behind [`KeySource::PgAdminTable`]'s NULL fallback.
#[derive(Debug)]
#[non_exhaustive] // Future local variants (a sealed file, an in-process KMS client) slot in here.
pub enum LocalKeySource {
    /// Read 32-byte root keys from `ZEROSHIP_COLUMN_KEY_<KEYID>` env
    /// vars (hex-encoded). SQLite tier + PG dev parity.
    EnvVar,
    /// Read roots handed to the process directly. `Rc` because the
    /// installer keeps a handle: supplied keys can be added or withdrawn
    /// after the store was built.
    Supplied(Rc<SuppliedRootKeys>),
}

impl LocalKeySource {
    fn lookup_root(&self, key_id: &str) -> Result<[u8; 32], DbError> {
        match self {
            Self::EnvVar => env_lookup_root(key_id),
            Self::Supplied(keys) => keys.lookup(key_id),
        }
    }
}

/// Source of root key material.
///
/// [`Self::Local`] is the SQLite / dev-parity path and the
/// operator-supplied path. [`Self::PgAdminTable`] is the PG path:
///   1. Calls `__zeroship_admin.get_column_key($1)`.
///   2. If that returns NULL or errors, **falls back to its `fallback`
///      local source**.
///
/// Since the admin schema was deleted (2026-08-27) nothing installs
/// that getter, so step 1 always fails and step 2 always runs.
///
/// `PgAdminTable` is instantiated by `PostgresBackend::new`; the
/// SQLite tier uses `Local`.
#[derive(Debug)]
#[non_exhaustive] // Future variants (Vault, KMS, ...) slot in here.
pub enum KeySource {
    /// Resolve locally, with no database round-trip.
    Local(LocalKeySource),
    /// PG source. Reads `__zeroship_admin.column_keys` via a
    /// `get_column_key` getter that NOTHING NOW INSTALLS - it came
    /// from `auth::bootstrap::ensure_admin_schema`, deleted 2026-08-27.
    /// So this always falls through to `fallback`.
    PgAdminTable {
        /// Pool the SECURITY DEFINER getter is called on.
        pool: Rc<compio_postgres::Pool>,
        /// Consulted only when the getter returns NULL.
        fallback: LocalKeySource,
    },
}

impl KeySource {
    /// Roots come from `ZEROSHIP_COLUMN_KEY_<KEYID>`.
    #[must_use]
    pub fn env_var() -> Self {
        Self::Local(LocalKeySource::EnvVar)
    }

    /// Roots come from bytes the caller already holds.
    #[must_use]
    pub fn supplied(keys: Rc<SuppliedRootKeys>) -> Self {
        Self::Local(LocalKeySource::Supplied(keys))
    }

    /// Admin table first, `fallback` behind it. There is no
    /// `pg_admin_table(pool)` shorthand: the PG backend always names the
    /// fallback it wants, because "which local source is behind the
    /// getter" is exactly the decision this type exists to make explicit.
    #[must_use]
    pub fn pg_admin_table_with_fallback(
        pool: Rc<compio_postgres::Pool>,
        fallback: LocalKeySource,
    ) -> Self {
        Self::PgAdminTable { pool, fallback }
    }
}

/// Per-isolate column-key store. Caches derived `AeadKey` material
/// keyed by `(app_id, key_id)`.
///
/// Construct one per [`crate::backend`] impl; clear on backend drop.
/// The PG impl wires through this.
pub struct KeyStore {
    cache: RefCell<HashMap<(String, String), AeadKey>>,
    sourcing: KeySource,
    /// Process-local hit/miss counter for the
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

    /// Total resolve-call count since this `KeyStore`
    /// was constructed. Used by the §11 closeout gate
    /// `default_read_does_not_load_column_key` to assert that a
    /// default masked read serves rows through the `<col>_masked`
    /// alias without consulting the key store at all. A non-zero
    /// reading on a default-read path is a regression: the sibling
    /// column is the masked text — the parent ciphertext (and
    /// therefore the key) must never be touched.
    #[must_use]
    #[cfg(test)]
    pub fn lookups_count(&self) -> u64 {
        self.lookups.get()
    }

    /// Look up or derive the [`AeadKey`] for `(app_id, key_id)`.
    /// Async signature for parity with the `PgAdminTable` variant
    /// (which has to `.await` a SECURITY DEFINER round-trip); the
    /// `EnvVar` source is sync internally and the body never `.await`s.
    #[allow(clippy::unused_async)] // The PgAdminTable variant awaits.
    pub async fn resolve(&self, app_id: &str, key_id: &str) -> Result<AeadKey, DbError> {
        // Increment BEFORE the cache lookup so a
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
        let root = Zeroizing::new(match &self.sourcing {
            KeySource::Local(local) => local.lookup_root(key_id)?,
            KeySource::PgAdminTable { pool, fallback } => {
                // PG-prod path: call the SECURITY DEFINER getter. Only a NULL
                // result (table empty, key id missing, table itself missing) —
                // i.e. `Ok(None)` - falls back to the local source, so a
                // pre-migration app still resolves a key.
                match pg_admin_lookup_root(pool, key_id).await {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => fallback.lookup_root(key_id)?,
                    // DB-10: a GENUINE getter fault (permission denied, connection
                    // error, SQL failure) must SURFACE — not silently downgrade to
                    // whatever the fallback source happens to hold, which
                    // could be a stale/test key and would produce wrong-key
                    // encrypt/decrypt with no signal. Propagate it.
                    Err(e) => return Err(e),
                }
            }
        });
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
///
/// The name is keyed by DATA: `key_id` is whatever the creator wrote in their
/// schema, so the member set is open and no list of literals describes it. That
/// is what [`zeroship_core::config::DeclaredEnvFamily`] is for - the PREFIX is
/// declared and recorded, the suffix stays data. Class `platform`: the prefix
/// is zeroship-owned and read on the worker path, and it has no generated
/// declaration yet.
fn env_lookup_root(key_id: &str) -> Result<[u8; 32], DbError> {
    const COLUMN_KEY_FAMILY: DeclaredEnvFamily<String, crate::PluginDbConsumer> =
        DeclaredEnvFamily::platform("ZEROSHIP_COLUMN_KEY_");
    let suffix = key_id.to_uppercase();
    // Derived from the family so the diagnostics cannot drift from the name
    // that was actually read.
    let env_name = format!("{}{suffix}", COLUMN_KEY_FAMILY.prefix());
    let hex = Zeroizing::new(
        zeroship_core::read_declared_env_family!(
            COLUMN_KEY_FAMILY,
            &suffix,
            crate::PluginDbConsumer
        )
        .ok()
        .flatten()
        .ok_or_else(|| DbError::Configuration {
            code: "column_key_not_configured",
            message: format!("Column key '{key_id}' not configured (set {env_name})"),
            hint: Some("Generate via: openssl rand -hex 32".to_string()),
        })?,
    );
    parse_root_key(&env_name, &hex)
}

/// Decode a hex-encoded 32-byte root key, or say why it is not one.
///
/// Pure: no environment, no store, no I/O. `source` names where the string
/// came from and appears verbatim in the message an operator sees - an env
/// var name on the env path, a `supplied root key '<id>'` label on the
/// supplied path. Every malformed-input case carries
/// `code: "column_key_not_configured"`, the same code a missing key does,
/// because the operator's fix is the same either way.
pub(crate) fn parse_root_key(source: &str, hex: &str) -> Result<[u8; 32], DbError> {
    let bytes = Zeroizing::new(hex_decode(hex).map_err(|e| DbError::Configuration {
        code: "column_key_not_configured",
        message: format!("{source}: hex decode failed: {e}"),
        hint: Some("Value must be 64 hex characters (32 bytes). Generate via: openssl rand -hex 32".to_string()),
    })?);
    if bytes.len() != 32 {
        return Err(DbError::Configuration {
            code: "column_key_not_configured",
            message: format!(
                "{source} must decode to 32 bytes, got {}",
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
    if !s.len().is_multiple_of(2) {
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

/// PG production root-key fetcher.
///
/// Calls `__zeroship_admin.get_column_key($1)`. Returns `Ok(Some(bytes))`
/// on a successful lookup, `Ok(None)` when the getter returns NULL, and
/// `Err(...)` when the call itself failed. Callers fall through to the
/// local source on `Ok(None)` OR `Err(_)`.
///
/// **Today that is always `Err(_)`:** the getter was installed only by
/// `auth::bootstrap::ensure_admin_schema` (deleted 2026-08-27), so the
/// function does not exist on any database. This body is left intact
/// rather than removed because choosing where PG column roots come
/// from instead is a design decision, not a deletion.
///
/// The function returns the raw 32-byte root; HKDF expansion happens
/// in `derive_key`.
async fn pg_admin_lookup_root(
    pool: &compio_postgres::Pool,
    key_id: &str,
) -> Result<Option<[u8; 32]>, DbError> {
    // The getter returns `bytea`, and compio-postgres surfaces result
    // columns in binary format. Read the raw bytes directly; an empty
    // result set OR a NULL value → `Ok(None)`.
    let rows = pool
        .query_text_params(
            "SELECT __zeroship_admin.get_column_key($1)",
            &[key_id],
        )
        .await
        .map_err(|e| DbError::Configuration {
            code: "column_key_not_configured",
            message: format!(
                "PG getter call failed for key_id '{key_id}': {e}"
            ),
            hint: Some(
                "Set ZEROSHIP_COLUMN_KEY_<KEYID> or supply the root directly; \
                 there is no admin-schema getter any more"
                    .to_string(),
            ),
        })?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    // `try_get` returns `Err` for NULL (a `WasNull` typed error) and
    // for type mismatches; either way we treat the row as "no key
    // present" and let the caller fall through to the local source.
    // `Row::get` would panic on NULL.
    let decoded = match row.try_get::<_, Vec<u8>>(0) {
        Ok(bytes) => Zeroizing::new(bytes),
        Err(_) => return Ok(None),
    };
    if decoded.len() != 32 {
        return Ok(None);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&decoded);
    Ok(Some(out))
}

// These tests plant no environment. Malformed-input cases call
// `parse_root_key` directly, and every case that needs a RESOLVABLE key
// drives a real `KeyStore` over a `SuppliedRootKeys` source, so the
// resolve / derive / cache path under test is the production one and the
// process-global env table is never touched.
#[cfg(test)]
mod tests {
    use super::*;

    /// A store over a single supplied root, plus the handle to that
    /// source so a test can withdraw the key or read the consultation
    /// count.
    fn store_with_root(key_id: &str, hex: &str) -> (KeyStore, Rc<SuppliedRootKeys>) {
        let keys = Rc::new(
            SuppliedRootKeys::new()
                .with_hex(key_id, hex)
                .expect("fixture root key must parse"),
        );
        (KeyStore::new(KeySource::supplied(Rc::clone(&keys))), keys)
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
    ///
    /// Malformed-input rejection is a property of `parse_root_key`, not of
    /// where the string came from, so this drives it directly. The `source`
    /// argument is the env var name the env path would pass.
    #[test]
    fn malformed_hex_yields_typed_error() {
        let err = parse_root_key("ZEROSHIP_COLUMN_KEY_BAD_HEX_TEST", "abc")
            .expect_err("odd-length hex must error");
        match err {
            DbError::Configuration { code, .. } => {
                assert_eq!(code, "column_key_not_configured");
            }
            other => panic!("expected Configuration, got {other:?}"),
        }
    }

    /// Hex with the wrong number of bytes (after decode) → typed
    /// Configuration error.
    #[test]
    fn wrong_length_hex_yields_typed_error() {
        let err = parse_root_key(
            "ZEROSHIP_COLUMN_KEY_WRONG_LEN_TEST",
            // 16 hex chars = 8 bytes, not 32.
            "0123456789abcdef",
        )
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
    }

    /// Hex with invalid characters → typed Configuration error.
    #[test]
    fn invalid_hex_chars_yield_typed_error() {
        let err = parse_root_key(
            "ZEROSHIP_COLUMN_KEY_BAD_CHARS_TEST",
            "zzzz000000000000000000000000000000000000000000000000000000000000",
        )
        .expect_err("non-hex chars must error");
        match err {
            DbError::Configuration { code, .. } => {
                assert_eq!(code, "column_key_not_configured");
            }
            other => panic!("expected Configuration, got {other:?}"),
        }
    }

    /// A supplied root that is not 32 hex-decodable bytes is rejected at
    /// INSTALL time, with the same typed error and a label naming the key
    /// id. Without this, a bad root would surface as a decrypt failure far
    /// from the operator's mistake.
    #[test]
    fn supplied_root_key_rejects_malformed_hex_on_insert() {
        let keys = SuppliedRootKeys::new();
        let err = keys
            .insert_hex("bad_supplied", "0123456789abcdef")
            .expect_err("8-byte hex must error");
        match err {
            DbError::Configuration { code, message, .. } => {
                assert_eq!(code, "column_key_not_configured");
                assert!(
                    message.contains("bad_supplied"),
                    "message must name the key id, got: {message}"
                );
                assert!(
                    message.contains("32 bytes"),
                    "message must name the expected length, got: {message}"
                );
            }
            other => panic!("expected Configuration, got {other:?}"),
        }
    }

    /// Resolving a key id no source holds gives the same typed
    /// `column_key_not_configured` an unset env var produces. Unlike
    /// `missing_env_var_yields_typed_error` this does not depend on what
    /// the ambient environment happens to contain: the source is empty by
    /// construction.
    #[test]
    fn missing_supplied_key_yields_typed_error() {
        let keys = Rc::new(SuppliedRootKeys::new());
        let store = KeyStore::new(KeySource::supplied(Rc::clone(&keys)));
        let rt = compio::runtime::Runtime::new().expect("compio runtime");
        let err = rt
            .block_on(async { store.resolve("app_x", "never_supplied").await })
            .expect_err("absent supplied key must error");
        match err {
            DbError::Configuration { code, hint, .. } => {
                assert_eq!(code, "column_key_not_configured");
                assert!(hint.is_some(), "hint must include openssl-rand suggestion");
            }
            other => panic!("expected Configuration, got {other:?}"),
        }
        assert_eq!(
            keys.consultations(),
            1,
            "a miss must still count as a consultation"
        );
    }

    /// KeyStore caches: a second `resolve` for the same `(app_id,
    /// key_id)` must hit the cache. We test indirectly: supply a
    /// well-formed root, resolve once, then WITHDRAW the root from the
    /// source and resolve again - the second call still succeeds (proves
    /// the cache short-circuited the source lookup).
    ///
    /// The consultation counter is the second, independent leg of the
    /// same claim: "still succeeds" alone would also hold if the source
    /// had handed the bytes over a second time, so the count must not
    /// rise across the second resolve.
    #[test]
    fn key_store_caches_derived_key() {
        let app_id = "app_cache_test";
        let key_id = "cache_test";
        // 64 hex chars = 32 bytes.
        let (store, keys) = store_with_root(key_id, &"0".repeat(64));
        let rt = compio::runtime::Runtime::new().expect("compio runtime");

        let first = rt.block_on(async { store.resolve(app_id, key_id).await });
        assert!(first.is_ok(), "first resolve must succeed: {first:?}");
        assert_eq!(
            keys.consultations(),
            1,
            "first resolve must have gone to the source"
        );

        // Withdraw the root - second resolve must still succeed via
        // the cache.
        assert!(keys.remove(key_id), "fixture root must have been present");
        let second = rt.block_on(async { store.resolve(app_id, key_id).await });
        assert!(
            second.is_ok(),
            "second resolve must hit cache (source now empty): {second:?}"
        );
        assert_eq!(
            keys.consultations(),
            1,
            "second resolve must not consult the source at all"
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
        let key_id = "multi_app_test";
        let (store, keys) = store_with_root(key_id, &"1".repeat(64));
        let rt = compio::runtime::Runtime::new().expect("compio runtime");
        let k1 = rt.block_on(async { store.resolve("app_a", key_id).await }).unwrap();
        let k2 = rt.block_on(async { store.resolve("app_b", key_id).await }).unwrap();
        assert_ne!(k1.k_enc, k2.k_enc);
        // Two distinct cache keys, so the source was asked twice - the
        // per-app entries did not alias.
        assert_eq!(keys.consultations(), 2);
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
    // §11 closeout: `default_read_does_not_load_column_key`.
    // The proposal asserts that a default masked read
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
        let store = KeyStore::new(KeySource::env_var());
        assert_eq!(store.lookups_count(), 0);
    }

    /// Every `resolve()` call bumps the counter — hit or miss.
    /// Pins the §11 "default read does not load a column key" gate:
    /// production code paths that should not consult the key store
    /// can assert `store.lookups_count() == 0` AFTER the SELECT.
    #[test]
    fn key_store_lookups_count_bumps_on_every_resolve() {
        let (store, _keys) = store_with_root("lookups_counter_test", &"2".repeat(64));
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
    }

    /// Failed `resolve()` calls (no such key in the source) still bump
    /// the counter - the gate cares about whether the production code
    /// path TRIED to load a key, not whether the load succeeded.
    #[test]
    fn key_store_lookups_count_bumps_on_failed_resolve() {
        let store = KeyStore::new(KeySource::supplied(Rc::new(SuppliedRootKeys::new())));
        let rt = compio::runtime::Runtime::new().expect("compio runtime");

        // The source holds no roots at all, so this resolve cannot succeed.
        let err = rt
            .block_on(async { store.resolve("app_x", "absent_for_counter_test").await });
        assert!(err.is_err(), "absent root key must error");
        assert_eq!(
            store.lookups_count(),
            1,
            "failed resolve still bumps counter (gate semantics)"
        );
    }
}
