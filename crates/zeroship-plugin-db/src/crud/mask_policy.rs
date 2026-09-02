//! Per-app mask policy storage + `setMaskPolicy`
//! dispatcher + in-process cache.
//!
//! The unmask authorization path (`crate::crud::unmask::check_unmask_authorization`)
//! reads a per-app [`MaskPolicy`] cached on the per-isolate context
//! (`ThreadDbContext::mask_policies`).
//!
//! ## Where the policy comes from
//!
//! **The creator's own source, at boot, and nowhere else on the PG
//! arm.** The app declares `defineMaskPolicy()`; `installSchema` flushes
//! it through the `__platform.setMaskPolicy` native op once per isolate
//! during startup; [`dispatch_set_mask_policy`] validates it and installs
//! it in the per-isolate cache. Redeploy is the only way to change it,
//! which is what "managed in the codebase, immutable at runtime" means.
//! App JS cannot reach the op: it hangs off the `ZS_PLATFORM` V8 private
//! symbol (see `crate::v8_classes::db_platform`).
//!
//! There is no durable policy store on PG. There used to be:
//! `__zeroship_admin.get_mask_policy(app_id)` /
//! `set_mask_policy(app_id, policy)`, a pair of `SECURITY DEFINER`
//! routines over an `__zeroship_admin.mask_policies` table. Installed
//! only by `auth::bootstrap::ensure_admin_schema`, which was `cfg(test,
//! feature = "test-helpers")` - so they never existed in a shipped
//! worker even before that function was deleted on 2026-08-27. They were
//! removed, not rehomed, by operator decision the same day: the worker
//! both read and wrote this state, so it was never privileged (AGENTS.md,
//! "Privilege follows the PROCESS, not the function"), and a policy
//! re-declared from the bundle on every boot has nothing for a durable
//! store to add.
//!
//! **SQLite still carries one** (selected at runtime by a `sqlite://`
//! url): a sidecar JSON file at `<db_dir>/mask_policies.json`, written
//! and re-read through [`persist_sqlite`] / [`load_sqlite`] off-thread
//! via `compio::runtime::spawn_blocking`, with a per-file process-local
//! mutex serialising writers. By the decision above that store is also
//! surplus - the boot-time declaration already seeds the cache, so the
//! sidecar is a write plus a redundant read - but removing it was
//! explicitly out of scope for the change that deleted the PG arm. The
//! two backends therefore DISAGREE on durability today, and only SQLite
//! is out of step.
//!
//! ### The `auto` actor fallback rule
//!
//! Even when an app declares a policy, the `auto` actor kind retains
//! its "everything by default" grant UNLESS the policy explicitly
//! lists `auto` with a restricted classification set. Reason: system
//! writes (migrations, background jobs) need uniform access regardless
//! of app policy. The rule is enforced inside
//! [`MaskPolicy::allows`] — callers don't need to special-case `auto`.
//!
//! ### Validation
//!
//! Both `defineMaskPolicy()` (SDK side) and [`dispatch_set_mask_policy`]
//! (Rust side) validate the classification set against the six built-ins
//! (`public`, `pii`, `spi`, `phi`, `pci`, `internal`). Belt-and-braces:
//! a misbehaving SDK can't poison the storage, and an arbitrary RPC
//! call can't bypass the SDK-side check.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

use zeroship_data_core::error::DbError;

/// The six canonical classification values. Mirrors the SDK's
/// `Classification` type (`sdks/db/src/types.ts`) and `crate::diff::Classification`.
pub const VALID_CLASSIFICATIONS: &[&str] = &["public", "pii", "spi", "phi", "pci", "internal"];

/// Per-app mask policy. Maps actor-role string → set
/// of classifications the role is permitted to unmask.
///
/// Stored on `ThreadDbContext::mask_policies` for the
/// life of the isolate; refreshed write-through when `setMaskPolicy`
/// fires. A `None` cache slot means "no policy declared for this app
/// on this isolate" — [`crate::crud::unmask::check_unmask_authorization`]
/// then falls back to the default-deny rule (only `auto` allowed).
#[derive(Debug, Clone, Default)]
pub struct MaskPolicy {
    /// `role -> set of classifications`. Roles missing from the map
    /// have no privileges (every classification request is denied).
    pub roles: HashMap<String, HashSet<String>>,
}

impl MaskPolicy {
    /// Empty policy — no role has any privilege. The default-deny stub
    /// still applies (only `auto` allowed) because
    /// [`Self::allows`] short-circuits on the `auto` role.
    #[must_use]
    #[cfg(test)]
    pub fn empty() -> Self {
        Self {
            roles: HashMap::new(),
        }
    }

    /// Check whether `role` is permitted to unmask `classification`.
    ///
    /// ### Rules
    ///
    /// 1. **`auto` fallback**: when `role == "auto"` AND the policy
    ///    does NOT explicitly list `auto`, return `true` regardless of
    ///    classification. System writes (migrations, background jobs)
    ///    always have uniform access unless the app explicitly
    ///    restricts them. To restrict the system actor, the policy
    ///    MUST list `auto` (with whatever set the app wants).
    ///
    /// 2. **Explicit role**: when the policy lists `role`, return
    ///    `true` iff `classification` is in the role's set.
    ///
    /// 3. **Unknown role**: deny.
    #[must_use]
    pub fn allows(&self, role: &str, classification: &str) -> bool {
        match self.roles.get(role) {
            Some(set) => set.contains(classification),
            None => {
                // The `auto` fallback rule — see method doc-comment.
                role == "auto"
            }
        }
    }

    /// Parse a JSON value (the wire shape the SDK + the SECURITY
    /// DEFINER getter both produce) into a [`MaskPolicy`].
    ///
    /// Expected shape:
    /// ```json
    /// {
    ///   "admin":   ["public", "pii"],
    ///   "support": ["public"]
    /// }
    /// ```
    ///
    /// ### Errors
    ///
    /// - `invalid_mask_policy_shape` — the JSON is not an object, OR a
    ///   role value is not an array of strings.
    /// - `invalid_mask_classification` — a classification is not one of
    ///   the six built-ins.
    pub fn from_json(v: &Value) -> Result<Self, DbError> {
        let obj = v.as_object().ok_or_else(|| DbError::ValidationFailed {
            code: "invalid_mask_policy_shape",
            message: "mask policy: must be an object mapping role strings \
                      to arrays of classifications"
                .to_string(),
            hint: Some("shape: { \"<role>\": [\"<classification>\", ...], ... }".into()),
        })?;
        let mut roles: HashMap<String, HashSet<String>> = HashMap::new();
        for (role, classifications) in obj.iter() {
            let arr = classifications
                .as_array()
                .ok_or_else(|| DbError::ValidationFailed {
                    code: "invalid_mask_policy_shape",
                    message: format!(
                        "mask policy: role '{role}' must map to an array of \
                         classifications"
                    ),
                    hint: None,
                })?;
            let mut set: HashSet<String> = HashSet::with_capacity(arr.len());
            for c in arr {
                let s = c.as_str().ok_or_else(|| DbError::ValidationFailed {
                    code: "invalid_mask_classification",
                    message: format!(
                        "mask policy: role '{role}' includes a non-string \
                         classification value"
                    ),
                    hint: Some(
                        "classifications are strings (one of public, pii, spi, \
                         phi, pci, internal)"
                            .into(),
                    ),
                })?;
                if !VALID_CLASSIFICATIONS.contains(&s) {
                    return Err(DbError::ValidationFailed {
                        code: "invalid_mask_classification",
                        message: format!(
                            "mask policy: role '{role}' includes invalid \
                             classification '{s}'"
                        ),
                        hint: Some(format!(
                            "valid classifications: {}",
                            VALID_CLASSIFICATIONS.join(", ")
                        )),
                    });
                }
                set.insert(s.to_string());
            }
            roles.insert(role.to_string(), set);
        }
        Ok(Self { roles })
    }

    /// Serialise the policy back to the canonical wire JSON shape.
    /// Sorted by role and classification so the wire bytes are
    /// deterministic — operators reading the storage column see a
    /// stable form regardless of HashMap iteration order.
    pub fn to_json(&self) -> Value {
        let mut role_names: Vec<&String> = self.roles.keys().collect();
        role_names.sort();
        let mut obj = serde_json::Map::with_capacity(role_names.len());
        for role in role_names {
            let set = &self.roles[role];
            let mut cls: Vec<&String> = set.iter().collect();
            cls.sort();
            let arr: Vec<Value> = cls.into_iter().map(|c| Value::String(c.clone())).collect();
            obj.insert(role.clone(), Value::Array(arr));
        }
        Value::Object(obj)
    }
}

// ---------------------------------------------------------------------------
// `setMaskPolicy` dispatcher — write through storage + refresh cache.
// ---------------------------------------------------------------------------

/// Boot-time installer for `__platform.setMaskPolicy`.
///
/// 1. Validate the policy JSON via [`MaskPolicy::from_json`] (both shape
///    + classification taxonomy).
/// 2. Install it in the per-isolate cache on
///    `ThreadDbContext::mask_policies`, so the next unmask on this
///    isolate sees it.
/// 3. On SQLite only, additionally write the sidecar JSON file. PG
///    persists nothing - see the module header for why the durable PG
///    store was deleted rather than rehomed.
///
/// Idempotent: re-running with the same policy overwrites the cache with
/// the same bytes and rewrites the same sidecar.
///
/// Not reachable from app JS. The op lives on the `DbPlatform` handle
/// behind a V8 private symbol, and `installSchema` is its only caller.
pub async fn dispatch_set_mask_policy(app_id: &str, policy_v: Value) -> Result<(), DbError> {
    let policy = MaskPolicy::from_json(&policy_v)?;

    // Policy installation runs during boot, before creator code can trigger a
    // data-plane operation. Initialise the configured backend here so the
    // policy path does not depend on an unrelated earlier DB call.
    let backend = crate::exec::ensure_backend_for_shared_sql().await?;

    // ---- PG arm: cache only, no storage round-trip ----
    if backend.as_postgres().is_some() {
        crate::context::with_mut(|c| c.set_mask_policy_for_app(app_id, Some(policy)));
        return Ok(());
    }

    // ---- SQLite arm ----
    if let Some(sq) = backend.as_sqlite() {
        persist_sqlite(sq, app_id, &policy).await?;
        crate::context::with_mut(|c| c.set_mask_policy_for_app(app_id, Some(policy)));
        return Ok(());
    }
    Err(DbError::Configuration {
        code: "backend_unsupported",
        message: "db: no backend arm available for setMaskPolicy".to_string(),
        hint: None,
    })
}

// ---------------------------------------------------------------------------
// SQLite persistence
// ---------------------------------------------------------------------------

/// SQLite sidecar file path. Lives next to the per-app
/// SQLite files at `<db_dir>/mask_policies.json`. Single global file
/// keyed by `app_id` — mirrors the in-process structure most closely
/// and avoids per-app I/O multipliers (a 50-app worker would otherwise
/// open 50 files at startup).
fn sqlite_policy_path(sq: &crate::backend::sqlite::SqliteBackend) -> std::path::PathBuf {
    sq.db_dir().join("mask_policies.json")
}

type PolicyFileLock = &'static Mutex<()>;

fn sqlite_policy_file_lock(path: &Path) -> Result<std::sync::MutexGuard<'static, ()>, DbError> {
    static POLICY_FILE_LOCKS: OnceLock<Mutex<HashMap<PathBuf, PolicyFileLock>>> = OnceLock::new();
    let locks = POLICY_FILE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let lock: PolicyFileLock = {
        let mut locks = locks
            .lock()
            .map_err(|_| DbError::internal("mask_policies.json: global lock registry poisoned"))?;
        *locks
            .entry(path.to_path_buf())
            .or_insert_with(|| Box::leak(Box::new(Mutex::new(()))))
    };
    lock.lock()
        .map_err(|_| DbError::internal("mask_policies.json: per-file lock poisoned"))
}

/// SQLite atomic write. Strategy:
///
/// 1. Read existing `<dir>/mask_policies.json` (treat ENOENT as empty
///    `{}`).
/// 2. Merge: insert / overwrite the app's entry with `policy.to_json()`.
/// 3. Serialise the merged map.
/// 4. Write to `<dir>/mask_policies.json.tmp`.
/// 5. Atomic rename to `<dir>/mask_policies.json`. POSIX rename is
///    atomic on the same filesystem; on crash mid-write the original
///    file survives untouched.
///
/// The blocking file I/O runs on compio's blocking pool, NOT on the
/// compio event loop thread. A per-file process-local mutex
/// serialises concurrent writers so two `setMaskPolicy` calls cannot
/// both read the same old JSON, then race their rewrites.
async fn persist_sqlite(
    sq: &crate::backend::sqlite::SqliteBackend,
    app_id: &str,
    policy: &MaskPolicy,
) -> Result<(), DbError> {
    let path = sqlite_policy_path(sq);
    let app_id = app_id.to_string();
    let policy = policy.clone();
    compio::runtime::spawn_blocking(move || persist_sqlite_blocking(path, app_id, policy))
        .await
        .map_err(|_| {
            DbError::internal("mask_policies.json: persist spawn_blocking task panicked")
        })?
}

fn persist_sqlite_blocking(
    path: PathBuf,
    app_id: String,
    policy: MaskPolicy,
) -> Result<(), DbError> {
    use std::fs;
    let _file_guard = sqlite_policy_file_lock(&path)?;
    let tmp = path.with_extension("json.tmp");

    // 1. Read existing.
    let existing: Value = match fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).map_err(|e| {
            DbError::internal(format!(
                "mask_policies.json: parse existing file at {}: {e}",
                path.display()
            ))
        })?,
        Ok(_) => Value::Object(serde_json::Map::new()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(serde_json::Map::new()),
        Err(e) => {
            return Err(DbError::internal(format!(
                "mask_policies.json: read {}: {e}",
                path.display()
            )));
        }
    };
    let mut obj = match existing {
        Value::Object(o) => o,
        _ => serde_json::Map::new(),
    };

    // 2. Merge.
    obj.insert(app_id, policy.to_json());
    let merged = Value::Object(obj);
    let serialised = serde_json::to_string_pretty(&merged)
        .map_err(|e| DbError::internal(format!("mask_policies.json: serialise: {e}")))?;

    // 3 + 4. Write tmp.
    fs::write(&tmp, &serialised).map_err(|e| {
        DbError::internal(format!(
            "mask_policies.json: write tmp {}: {e}",
            tmp.display()
        ))
    })?;

    // 5. Atomic rename.
    fs::rename(&tmp, &path).map_err(|e| {
        DbError::internal(format!(
            "mask_policies.json: rename {} -> {}: {e}",
            tmp.display(),
            path.display()
        ))
    })?;
    Ok(())
}

/// Load the policy for a single app from the sidecar
/// file. Returns `None` when the file is absent or has no entry for
/// the app.
pub async fn load_sqlite(
    sq: &crate::backend::sqlite::SqliteBackend,
    app_id: &str,
) -> Result<Option<MaskPolicy>, DbError> {
    let path = sqlite_policy_path(sq);
    let app_id = app_id.to_string();
    compio::runtime::spawn_blocking(move || load_sqlite_blocking(path, app_id))
        .await
        .map_err(|_| DbError::internal("mask_policies.json: load spawn_blocking task panicked"))?
}

fn load_sqlite_blocking(path: PathBuf, app_id: String) -> Result<Option<MaskPolicy>, DbError> {
    use std::fs;
    let _file_guard = sqlite_policy_file_lock(&path)?;
    let text = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(DbError::internal(format!(
                "mask_policies.json: read {}: {e}",
                path.display()
            )));
        }
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| {
        DbError::internal(format!("mask_policies.json: parse {}: {e}", path.display()))
    })?;
    let entry = v.get(app_id.as_str());
    match entry {
        Some(p) => Ok(Some(MaskPolicy::from_json(p)?)),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// V8 dispatch glue
// ---------------------------------------------------------------------------

use zeroship_runtime::state::ResolveValue;

/// V8-facing dispatch helper for `zeroship.db.setMaskPolicy`. Returns
/// the unresolved Promise; the dispatcher body runs as a spawned op
/// and resolves with `{}` on success or rejects with the typed
/// `OpError`.
///
/// Called from `v8_classes::db::Db::set_mask_policy` (the `#[v8_method]`
/// wrapping this entry point).
pub(crate) fn dispatch_set_mask_policy_field<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    policy_v: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = crate::v8_bridge::runtime_state(scope);
    let (resolver, request_id, promise) = crate::v8_bridge::setup_js_promise(scope, &state);
    let app = app_id.to_string();

    // The engine half already existed as a separate `async fn`; what was here
    // was a hand-rolled copy of `settle`'s two arms. Its error arm and
    // `settle`'s are the same `reject_op` call.
    state.borrow_mut().spawned_ops.push(Box::pin(super::settle(
        resolver,
        request_id,
        async move { dispatch_set_mask_policy(&app, policy_v).await },
        |()| ResolveValue::Json("{}".to_string()),
    )));

    promise
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use serde_json::json;

    fn run<F: std::future::Future>(f: F) -> F::Output {
        compio::runtime::Runtime::new()
            .expect("compio runtime build")
            .block_on(f)
    }

    #[test]
    fn allows_grants_listed_classification() {
        let mut roles: HashMap<String, HashSet<String>> = HashMap::new();
        let mut admin_set = HashSet::new();
        admin_set.insert("pii".to_string());
        admin_set.insert("spi".to_string());
        roles.insert("admin".to_string(), admin_set);
        let p = MaskPolicy { roles };
        assert!(p.allows("admin", "pii"));
        assert!(p.allows("admin", "spi"));
    }

    #[test]
    fn allows_denies_unlisted_classification() {
        let mut roles: HashMap<String, HashSet<String>> = HashMap::new();
        let mut user_set = HashSet::new();
        user_set.insert("public".to_string());
        roles.insert("user".to_string(), user_set);
        let p = MaskPolicy { roles };
        assert!(p.allows("user", "public"));
        assert!(!p.allows("user", "pii"));
        assert!(!p.allows("user", "spi"));
    }

    #[test]
    fn allows_denies_unknown_role() {
        let p = MaskPolicy::empty();
        assert!(!p.allows("operator", "public"));
        assert!(!p.allows("ai-builder", "pii"));
        // Empty string role too.
        assert!(!p.allows("", "pii"));
    }

    #[test]
    fn auto_fallback_when_not_in_policy() {
        // Empty policy — auto still has uniform access.
        let p = MaskPolicy::empty();
        assert!(p.allows("auto", "pii"));
        assert!(p.allows("auto", "spi"));
        assert!(p.allows("auto", "internal"));
    }

    #[test]
    fn auto_fallback_when_other_roles_in_policy() {
        // Policy lists a non-auto role; auto is still uniform.
        let mut roles: HashMap<String, HashSet<String>> = HashMap::new();
        roles.insert("user".to_string(), HashSet::from(["public".to_string()]));
        let p = MaskPolicy { roles };
        assert!(p.allows("auto", "pii"));
        assert!(p.allows("auto", "phi"));
    }

    #[test]
    fn auto_explicit_restriction_overrides_fallback() {
        // Policy lists `auto` with a restricted set — fallback DOES NOT
        // apply. The app is explicitly saying "even the system actor
        // is restricted".
        let mut roles: HashMap<String, HashSet<String>> = HashMap::new();
        roles.insert("auto".to_string(), HashSet::from(["public".to_string()]));
        let p = MaskPolicy { roles };
        assert!(p.allows("auto", "public"));
        assert!(!p.allows("auto", "pii"));
        assert!(!p.allows("auto", "spi"));
    }

    #[test]
    fn auto_explicit_empty_set_denies_everything() {
        // Policy lists `auto` with an EMPTY set — the system actor is
        // fully restricted. The fallback's "auto always allowed" does
        // not apply because the role IS listed.
        let mut roles: HashMap<String, HashSet<String>> = HashMap::new();
        roles.insert("auto".to_string(), HashSet::new());
        let p = MaskPolicy { roles };
        assert!(!p.allows("auto", "public"));
        assert!(!p.allows("auto", "pii"));
    }

    #[test]
    fn from_json_parses_well_formed_policy() {
        let v = json!({
            "admin":   ["public", "pii", "spi"],
            "support": ["public", "pii"],
            "user":    ["public"],
        });
        let p = MaskPolicy::from_json(&v).expect("parse");
        assert!(p.allows("admin", "pii"));
        assert!(p.allows("admin", "spi"));
        assert!(!p.allows("admin", "phi"));
        assert!(p.allows("support", "pii"));
        assert!(!p.allows("support", "spi"));
        assert!(p.allows("user", "public"));
        assert!(!p.allows("user", "pii"));
    }

    #[test]
    fn from_json_rejects_non_object() {
        let v = json!("not an object");
        let err = MaskPolicy::from_json(&v).unwrap_err();
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "invalid_mask_policy_shape");
            }
            other => panic!("expected invalid_mask_policy_shape, got {other:?}"),
        }
    }

    #[test]
    fn from_json_rejects_non_array_value() {
        let v = json!({ "admin": "pii" });
        let err = MaskPolicy::from_json(&v).unwrap_err();
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "invalid_mask_policy_shape");
            }
            other => panic!("expected invalid_mask_policy_shape, got {other:?}"),
        }
    }

    #[test]
    fn from_json_rejects_unknown_classification() {
        let v = json!({ "admin": ["public", "badclass"] });
        let err = MaskPolicy::from_json(&v).unwrap_err();
        match err {
            DbError::ValidationFailed { code, message, .. } => {
                assert_eq!(code, "invalid_mask_classification");
                assert!(
                    message.contains("badclass"),
                    "diagnostic must include the bad value: {message}"
                );
            }
            other => panic!("expected invalid_mask_classification, got {other:?}"),
        }
    }

    #[test]
    fn from_json_rejects_non_string_classification() {
        let v = json!({ "admin": [42] });
        let err = MaskPolicy::from_json(&v).unwrap_err();
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "invalid_mask_classification");
            }
            other => panic!("expected invalid_mask_classification, got {other:?}"),
        }
    }

    #[test]
    fn from_json_accepts_all_six_classifications() {
        let v = json!({
            "admin": ["public", "pii", "spi", "phi", "pci", "internal"],
        });
        let p = MaskPolicy::from_json(&v).expect("parse");
        for c in VALID_CLASSIFICATIONS {
            assert!(p.allows("admin", c), "admin must allow {c}");
        }
    }

    #[test]
    fn from_json_empty_array_yields_empty_set() {
        let v = json!({ "auto": [] });
        let p = MaskPolicy::from_json(&v).expect("parse");
        // Even though auto is listed with an empty set, the explicit
        // listing means the fallback rule does NOT apply.
        assert!(!p.allows("auto", "pii"));
    }

    #[test]
    fn to_json_round_trips() {
        let original = json!({
            "admin":   ["pii", "public"],
            "support": ["public"],
        });
        let p = MaskPolicy::from_json(&original).expect("parse");
        let serialised = p.to_json();
        // Round-trip through parse again so we don't depend on
        // key ordering (to_json sorts; from_json doesn't care).
        let p2 = MaskPolicy::from_json(&serialised).expect("re-parse");
        assert!(p2.allows("admin", "pii"));
        assert!(p2.allows("admin", "public"));
        assert!(p2.allows("support", "public"));
        assert!(!p2.allows("support", "pii"));
    }

    #[test]
    fn to_json_is_deterministic() {
        let mut roles: HashMap<String, HashSet<String>> = HashMap::new();
        roles.insert(
            "admin".to_string(),
            HashSet::from(["pii".to_string(), "spi".to_string(), "public".to_string()]),
        );
        roles.insert("user".to_string(), HashSet::from(["public".to_string()]));
        let p = MaskPolicy { roles };
        let s1 = p.to_json().to_string();
        let s2 = p.to_json().to_string();
        assert_eq!(s1, s2, "to_json must produce identical bytes per call");
        // Also: roles are sorted alphabetically.
        let idx_admin = s1.find("admin").expect("admin present");
        let idx_user = s1.find("user").expect("user present");
        assert!(idx_admin < idx_user, "roles must serialise sorted: {s1}");
    }

    #[test]
    fn valid_classifications_constant_matches_taxonomy() {
        // Pinning the canonical six against the SDK list. A drift
        // (e.g. a 7th value added on one side without the other)
        // breaks compile here.
        let expected = ["public", "pii", "spi", "phi", "pci", "internal"];
        assert_eq!(VALID_CLASSIFICATIONS.len(), expected.len());
        for c in expected {
            assert!(
                VALID_CLASSIFICATIONS.contains(&c),
                "missing classification: {c}"
            );
        }
    }

    // -----------------------------------------------------------------
    // §11 closeout: `mask_policy_per_app_isolated`
    //
    // The proposal asserts that a `defineMaskPolicy()` write under
    // app A's isolate-context entry must NOT be visible from app B's
    // entry. The cache is keyed by app_id on
    // `ThreadDbContext.mask_policies`; this test pins that
    // invariant directly through the public surface so a future
    // refactor that accidentally widens the key (e.g. to a shared
    // singleton) trips the gate.
    // -----------------------------------------------------------------

    #[test]
    fn mask_policy_per_app_isolated() {
        use crate::context::ThreadDbContext;

        // Build two distinct policies — one permissive for `admin`,
        // one restrictive for `support` — and seed them under
        // different app ids.
        let mut admin_set: HashSet<String> = HashSet::new();
        admin_set.insert("pii".to_string());
        admin_set.insert("spi".to_string());
        admin_set.insert("phi".to_string());
        let mut roles_a: HashMap<String, HashSet<String>> = HashMap::new();
        roles_a.insert("admin".to_string(), admin_set);
        let policy_a = MaskPolicy { roles: roles_a };

        let mut support_set: HashSet<String> = HashSet::new();
        support_set.insert("public".to_string());
        let mut roles_b: HashMap<String, HashSet<String>> = HashMap::new();
        roles_b.insert("support".to_string(), support_set);
        let policy_b = MaskPolicy { roles: roles_b };

        // Construct a fresh context (mirrors the pattern in the
        // `context` module's tests — avoids touching the thread-local
        // THREAD_DB_CTX so test ordering is irrelevant).
        let mut ctx = ThreadDbContext::new();
        ctx.set_mask_policy_for_app("app_a", Some(policy_a.clone()));
        ctx.set_mask_policy_for_app("app_b", Some(policy_b.clone()));

        // App A sees policy A only — admin grants are visible; the
        // app-B `support` role is not in the cache for app A.
        let a = ctx.mask_policy_for("app_a").expect("app_a cached");
        assert!(a.allows("admin", "pii"));
        assert!(a.allows("admin", "spi"));
        assert!(
            !a.allows("support", "public"),
            "app_b's role must not leak into app_a"
        );

        // App B sees policy B only — support grants are visible; the
        // app-A `admin` role is not in the cache for app B.
        let b = ctx.mask_policy_for("app_b").expect("app_b cached");
        assert!(b.allows("support", "public"));
        assert!(
            !b.allows("admin", "pii"),
            "app_a's role must not leak into app_b"
        );
        assert!(
            !b.allows("admin", "spi"),
            "app_a's role must not leak into app_b"
        );

        // App C — never seeded — sees nothing.
        assert!(ctx.mask_policy_for("app_c").is_none());

        // Clearing app A leaves app B intact (fence against a clear-
        // implementation that walks the whole map).
        ctx.set_mask_policy_for_app("app_a", None);
        assert!(ctx.mask_policy_for("app_a").is_none());
        assert!(ctx.mask_policy_for("app_b").is_some());
    }

    #[test]
    fn sqlite_mask_policy_async_round_trip() {
        run(async {
            let dir = tempfile::tempdir().expect("create tempdir");
            let backend = crate::backend_selection::new_sqlite_backend(PathBuf::from(dir.path()))
                .expect("open sqlite backend");
            let policy = MaskPolicy::from_json(&json!({
                "admin": ["public", "pii"],
                "support": ["public"]
            }))
            .expect("build policy");

            persist_sqlite(&backend, "app_async", &policy)
                .await
                .expect("persist sqlite policy");
            let loaded = load_sqlite(&backend, "app_async")
                .await
                .expect("load sqlite policy")
                .expect("policy entry");

            assert!(loaded.allows("admin", "pii"));
            assert!(loaded.allows("support", "public"));
            assert!(!loaded.allows("support", "pii"));
        });
    }

    #[test]
    fn set_mask_policy_initializes_a_cold_sqlite_backend() {
        crate::reset_context_for_tests();
        let dir = tempfile::tempdir().expect("create tempdir");
        let url = format!("sqlite:{}", dir.path().join("cold.sqlite").display());
        crate::set_db_url_for_tests(&url);
        assert!(crate::context::with(|context| context.backend()).is_none());

        run(async {
            dispatch_set_mask_policy("app_cold_policy", json!({ "support": ["spi"] }))
                .await
                .expect("cold policy install must initialize the backend");
        });

        assert!(
            crate::context::with(|context| context.backend())
                .is_some_and(|backend| backend.as_sqlite().is_some()),
            "policy install must leave the configured SQLite backend ready"
        );
        let policy = crate::context::with(|context| context.mask_policy_for("app_cold_policy"))
            .expect("policy must be cached after cold initialization");
        assert!(policy.allows("support", "spi"));
        crate::reset_context_for_tests();
    }

    #[test]
    fn sqlite_mask_policy_sidecar_serializes_concurrent_writers() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("mask_policies.json");
        let policy_a = MaskPolicy::from_json(&json!({
            "admin": ["pii", "spi"]
        }))
        .expect("policy A");
        let policy_b = MaskPolicy::from_json(&json!({
            "support": ["public"]
        }))
        .expect("policy B");

        std::thread::scope(|scope| {
            let path_a = path.clone();
            let path_b = path.clone();
            let write_a =
                scope.spawn(move || persist_sqlite_blocking(path_a, "app_a".to_string(), policy_a));
            let write_b =
                scope.spawn(move || persist_sqlite_blocking(path_b, "app_b".to_string(), policy_b));

            write_a
                .join()
                .expect("writer A thread")
                .expect("writer A persist");
            write_b
                .join()
                .expect("writer B thread")
                .expect("writer B persist");
        });

        let raw = std::fs::read_to_string(&path).expect("read sidecar");
        let json: Value = serde_json::from_str(&raw).expect("parse sidecar json");
        assert!(
            json.get("app_a").is_some() && json.get("app_b").is_some(),
            "serialized sidecar must retain both app entries: {raw}"
        );

        let loaded_a = load_sqlite_blocking(path.clone(), "app_a".to_string())
            .expect("load app_a")
            .expect("app_a entry");
        let loaded_b = load_sqlite_blocking(path, "app_b".to_string())
            .expect("load app_b")
            .expect("app_b entry");
        assert!(loaded_a.allows("admin", "pii"));
        assert!(loaded_b.allows("support", "public"));
    }
}
