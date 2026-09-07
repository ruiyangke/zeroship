//! Per-app mask policy storage + `setMaskPolicy`
//! dispatcher + in-process cache.
//!
//! The unmask authorization path (`crate::crud::unmask::check_unmask_authorization`)
//! reads a per-app [`MaskPolicy`] from this module's own per-thread cache
//! ([`cache_get`]); the slot left `ThreadDbContext` on 2026-09-02.
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
//! `get_mask_policy(app_id)` / `set_mask_policy(app_id, policy)`, a pair of
//! `SECURITY DEFINER` routines over a `mask_policies` table, all three in the
//! platform-owned system schema deleted on 2026-08-27. Installed
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
//! and re-read through `persist_sqlite` / `load_sqlite` off-thread
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

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use serde_json::Value;

use zeroship_data_core::error::DbError;

use crate::backend::BackendHandle;

/// The six canonical classification values. Mirrors the SDK's
/// `Classification` type (`sdks/db/src/types.ts`) and `crate::diff::Classification`.
pub const VALID_CLASSIFICATIONS: &[&str] = &["public", "pii", "spi", "phi", "pci", "internal"];

/// Per-app mask policy. Maps actor-role string → set
/// of classifications the role is permitted to unmask.
///
/// Held in this module's [`MASK_POLICIES`] thread-local for the
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
/// 2. Install it in this module's per-thread cache via [`cache_put`], so
///    the next unmask on this isolate sees it.
/// 3. On SQLite only, additionally write the sidecar JSON file. PG
///    persists nothing - see the module header for why the durable PG
///    store was deleted rather than rehomed.
///
/// Idempotent: re-running with the same policy overwrites the cache with
/// the same bytes and rewrites the same sidecar.
///
/// Not reachable from app JS. The op lives on the `DbPlatform` handle
/// behind a V8 private symbol, and `installSchema` is its only caller.
///
/// **`backend` is a parameter because this is an INITIALISER, not a reader.**
/// Policy installation runs during boot, before creator code can trigger a
/// data-plane op, so it is normally the call that warms a COLD isolate - and it
/// used to warm the isolate itself, through `exec::ensure_backend_for_shared_sql`,
/// which put this ENGINE path's hands on the ADAPTER's thread context and its
/// lazy `init_pool_async`. The cold-init arm did not go away; it moved up one
/// frame. The only production caller,
/// `crate::v8_classes::dispatch::dispatch_set_mask_policy_field`, now resolves it
/// with `crate::tx_scope::ensure_backend()` - adapter to adapter - and hands the
/// opened handle down. Dropping that arm instead of relocating it would make
/// `installSchema`'s `setMaskPolicy` fail `not_configured` on every fresh
/// isolate, which on the SQLite dev tier is every boot.
pub async fn dispatch_set_mask_policy(
    backend: &BackendHandle,
    app_id: &str,
    policy_v: Value,
) -> Result<(), DbError> {
    let policy = MaskPolicy::from_json(&policy_v)?;

    // Persist first, cache second. **The order is the point:** a cache that
    // outlived a failed write would answer reads with a policy the next restart
    // cannot recover, and a mask policy that silently narrows on restart is the
    // failure this whole module exists to prevent.
    //
    // WHERE it persists, and whether it persists at all, is the backend's
    // business - PostgreSQL stores nothing because `installSchema` reinstalls
    // on every boot. This used to be an `as_postgres()` / `as_sqlite()` pair
    // here, which put both backend names and SQLite's sidecar strategy into the
    // engine.
    backend.persist_mask_policy(app_id, &policy.to_json()).await?;
    cache_put(app_id, Some(policy));
    Ok(())
}

// ----- THE PER-THREAD POLICY CACHE ----------------------------------------

thread_local! {
    /// This worker thread's per-app mask-policy cache.
    ///
    /// It lives HERE rather than as a field on the adapter's `ThreadDbContext`
    /// because `docs/proposals/2026-09-02-thread-context-ownership.md` assigns
    /// `mask_policies` to data-engine, and `MaskPolicy` is this module's own
    /// type. Parking an engine type's cache on the adapter made every read of
    /// it - all of them in `crud/` - an engine-reaching-up edge.
    ///
    /// Seeded on first unmask attempt from durable storage, refreshed
    /// write-through by `setMaskPolicy`. A missing entry is NOT "no policy": it
    /// means this thread has not loaded one yet, and the caller must consult
    /// durable storage before falling through to the default-deny rule. That
    /// distinction is why [`cache_has`] exists separately from [`cache_get`].
    ///
    /// Entries are never proactively evicted, so one lives for the thread's
    /// lifetime unless replaced.
    static MASK_POLICIES: RefCell<HashMap<String, MaskPolicy>> =
        RefCell::new(HashMap::new());
}

/// The cached policy for `app_id`, if this thread has loaded one.
pub fn cache_get(app_id: &str) -> Option<MaskPolicy> {
    MASK_POLICIES.with_borrow(|m| m.get(app_id).cloned())
}

/// Write-through install. `Some` upserts; `None` clears the entry.
pub fn cache_put(app_id: &str, policy: Option<MaskPolicy>) {
    MASK_POLICIES.with_borrow_mut(|m| match policy {
        Some(p) => {
            m.insert(app_id.to_string(), p);
        }
        None => {
            m.remove(app_id);
        }
    });
}

/// Whether an entry is cached for `app_id`.
///
/// Cheaper than [`cache_get`] when the caller only needs to gate the durable
/// load, and distinct from `cache_get(..).is_some()` in intent: see the
/// thread-local's own note on why absent does not mean "no policy".
pub fn cache_has(app_id: &str) -> bool {
    MASK_POLICIES.with_borrow(|m| m.contains_key(app_id))
}

/// Empty this thread's cache.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_tests() {
    MASK_POLICIES.with_borrow_mut(HashMap::clear);
}

// ---------------------------------------------------------------------------


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
    // entry. The cache is keyed by app_id in [`MASK_POLICIES`]; this test pins that
    // invariant directly through the public surface so a future
    // refactor that accidentally widens the key (e.g. to a shared
    // singleton) trips the gate.
    // -----------------------------------------------------------------

    #[test]
    fn mask_policy_per_app_isolated() {
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

        // Drives the thread-local cache directly. It used to build a private
        // `ThreadDbContext` instead, to "avoid touching the thread-local so
        // test ordering is irrelevant" - but ordering was never the hazard it
        // implied: libtest gives every `#[test]` its own OS thread even under
        // `--test-threads=1` (measured 2026-09-01), so no other test can
        // observe or disturb these entries.
        cache_put("app_a", Some(policy_a.clone()));
        cache_put("app_b", Some(policy_b.clone()));

        // App A sees policy A only — admin grants are visible; the
        // app-B `support` role is not in the cache for app A.
        let a = cache_get("app_a").expect("app_a cached");
        assert!(a.allows("admin", "pii"));
        assert!(a.allows("admin", "spi"));
        assert!(
            !a.allows("support", "public"),
            "app_b's role must not leak into app_a"
        );

        // App B sees policy B only — support grants are visible; the
        // app-A `admin` role is not in the cache for app B.
        let b = cache_get("app_b").expect("app_b cached");
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
        assert!(cache_get("app_c").is_none());

        // Clearing app A leaves app B intact (fence against a clear-
        // implementation that walks the whole map).
        cache_put("app_a", None);
        assert!(cache_get("app_a").is_none());
        assert!(cache_get("app_b").is_some());
    }

    // `set_mask_policy_installs_through_an_adapter_opened_cold_backend` MOVED to
    // `zeroship-plugin-db`'s `tx_scope.rs` with the engine cut. What it witnesses
    // is the ADAPTER half of the boot sequence - a cold isolate, a configured
    // SQLite url, `tx_scope::ensure_backend()` warming it - and only its last
    // line is the engine's. It could not stay: it named `crate::context`,
    // `crate::tx_scope` and `crate::set_db_url_for_tests`, none of which this
    // crate may see.
}
