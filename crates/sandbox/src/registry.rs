//! Sandbox registry: tracks live sandbox runtimes, schedules
//! teardown when idle. Backend-agnostic — the registry holds
//! public-facing [`SandboxInfo`]; the active runtime (Docker
//! container, k8s Pod, etc.) lives inside the
//! [`crate::backend::Backend`] enum and is keyed by `sandbox_id`.
//!
//! A sandbox is the unit of "one (user, project) being edited
//! right now". Multiple editor tabs from the same user on the same
//! project reuse the same sandbox (the registry dedups by
//! `(user_id, project_id)`). Different users on the same project
//! each get their own sandbox — see the storage design.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::backend::{SandboxAuth, SandboxInfo};
use crate::persist::{SealedAuditEntry, SealedPreviewSecrets};
use crate::AppState;

/// Grace period (seconds) during which the *previous* preview-secret
/// version is still honoured after an organic rotation. Explicit
/// `DELETE` is **zero-grace** — see [`SandboxRegistry::rotate_preview_secret`].
/// (preview-URL § II.4 "Per-sandbox secret" — "60 s grace period")
pub const PREVIEW_SECRET_GRACE_SECS: u64 = 60;

/// Per-sandbox HMAC-secret ring used by Phase-3 share tokens.
/// Mirrors [`crate::persist::SealedPreviewSecrets`] — the registry
/// holds the live in-memory copy; persistence layer sees the same
/// shape. Cloning is cheap (32 bytes + Option + u64).
#[derive(Clone, Debug)]
pub struct PreviewSecrets {
    pub sv_current: u32,
    pub current: [u8; 32],
    pub previous: Option<[u8; 32]>,
    /// Wall-clock unix-seconds at which `previous` ages out. Past this
    /// point the previous secret is no longer accepted even if the
    /// in-memory `previous` field is still `Some` (the field is
    /// cleared lazily in [`SandboxRegistry::secret_for_version`]).
    pub grace_until_unix: Option<u64>,
}

impl PreviewSecrets {
    /// Produce the on-disk form for sealing. Mirrors the ring
    /// byte-for-byte — the audit table is a separate field on
    /// `SealedAuth`.
    pub fn to_sealed(&self) -> SealedPreviewSecrets {
        SealedPreviewSecrets {
            sv_current: self.sv_current,
            current: self.current,
            previous: self.previous,
            grace_until_unix: self.grace_until_unix,
        }
    }

    /// Hydrate from a sealed snapshot.
    pub fn from_sealed(s: &SealedPreviewSecrets) -> Self {
        Self {
            sv_current: s.sv_current,
            current: s.current,
            previous: s.previous,
            grace_until_unix: s.grace_until_unix,
        }
    }
}

/// In-memory audit metadata for one minted share token. The token
/// bytes themselves are NOT stored; only what `GET .../share`
/// surfaces. Mirrors [`SealedAuditEntry`].
#[derive(Clone, Debug)]
pub struct PreviewAuditEntry {
    pub token_id: String,
    pub port: u16,
    pub issued_at_unix: u64,
    pub expires_at_unix: u64,
    pub scope: String,
    pub secret_version: u32,
    pub iss: Option<String>,
    pub last_used_at_unix: u64,
    pub use_count: u64,
}

impl PreviewAuditEntry {
    pub fn to_sealed(&self) -> SealedAuditEntry {
        SealedAuditEntry {
            token_id: self.token_id.clone(),
            port: self.port,
            issued_at_unix: self.issued_at_unix,
            expires_at_unix: self.expires_at_unix,
            scope: self.scope.clone(),
            secret_version: self.secret_version,
            iss: self.iss.clone(),
            last_used_at_unix: self.last_used_at_unix,
            use_count: self.use_count,
        }
    }

    pub fn from_sealed(s: &SealedAuditEntry) -> Self {
        Self {
            token_id: s.token_id.clone(),
            port: s.port,
            issued_at_unix: s.issued_at_unix,
            expires_at_unix: s.expires_at_unix,
            scope: s.scope.clone(),
            secret_version: s.secret_version,
            iss: s.iss.clone(),
            last_used_at_unix: s.last_used_at_unix,
            use_count: s.use_count,
        }
    }
}

fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn fresh_secret_bytes() -> [u8; 32] {
    use std::fs::File;
    use std::io::Read;
    let mut buf = [0u8; 32];
    // Falls back to a process-derived seed if `/dev/urandom` is
    // somehow unavailable. The fallback is NOT meant for production —
    // an operator who lost `/dev/urandom` has bigger problems — but
    // it lets unit tests run in environments where /dev is locked
    // down (CI sandboxes occasionally are).
    if let Ok(mut f) = File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    for (i, byte) in buf.iter_mut().enumerate() {
        *byte = ((nanos >> (i * 4)) ^ (pid << (i % 8))) as u8;
    }
    buf
}

/// Internal sandbox record. Holds the public `SandboxInfo` plus
/// timing data for the GC. The `auth` field is the per-sandbox
/// signing-key + agent-URL bundle the preview proxy and any future
/// signed-RPC dispatch reach for; populated at `insert_with_auth`
/// time, `None` for legacy callers that haven't lifted yet (the
/// preview-URL proposal § II.0 says the registry SHOULD always
/// hold auth, but we keep the optional shape so the migration is
/// gradual — the Backend's `session_auth` lookup is still the
/// authoritative source).
#[derive(Clone)]
struct Sandbox {
    info: SandboxInfo,
    created_at: Instant,
    last_used: Arc<RwLock<Instant>>,
    auth: Option<SandboxAuth>,
    /// Phase-3 preview share-token secret ring. `None` until the
    /// first `POST .../share` mint or until a sealed-record restore
    /// re-hydrates one. Lock granularity matches `last_used` — a
    /// per-sandbox `RwLock` so token validate/mint paths don't
    /// contend with each other across sandboxes.
    preview_secrets: Arc<RwLock<Option<PreviewSecrets>>>,
    /// Phase-3 share-token audit metadata. Keyed by `token_id` so
    /// `GET` returns rows in insert order while `record_use` and
    /// `revoke_one` lookups are O(active rows).
    preview_audit: Arc<RwLock<Vec<PreviewAuditEntry>>>,
}

impl Sandbox {
    fn touch(&self) {
        if let Ok(mut t) = self.last_used.write() {
            *t = Instant::now();
        }
    }

    fn idle_for(&self) -> Duration {
        let last = *self.last_used.read().unwrap();
        Instant::now().saturating_duration_since(last)
    }

    fn lived_for(&self) -> Duration {
        Instant::now().saturating_duration_since(self.created_at)
    }

    fn current_info(&self) -> SandboxInfo {
        let last = *self.last_used.read().unwrap();
        let bumped = self.info.created_at_secs
            + last.saturating_duration_since(self.created_at).as_secs();
        SandboxInfo {
            last_used_at_secs: bumped,
            ..self.info.clone()
        }
    }
}

/// In-memory sandbox registry. Indexed twice: once by `sandbox_id`
/// (the key the client holds), once by (`user_id`, `project_id`)
/// (so a user re-opening the same project finds their existing
/// sandbox, while a *different* user on the same project gets
/// their own).
#[derive(Clone, Default)]
pub struct SandboxRegistry {
    by_sandbox: Arc<RwLock<HashMap<Uuid, Sandbox>>>,
    by_user_project: Arc<RwLock<HashMap<(String, String), Uuid>>>,
}

impl std::fmt::Debug for SandboxRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxRegistry").finish_non_exhaustive()
    }
}

impl SandboxRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lookup by id; touches `last_used` on hit.
    pub fn get(&self, id: &Uuid) -> Option<SandboxInfo> {
        let guard = self.by_sandbox.read().unwrap();
        let s = guard.get(id)?;
        s.touch();
        Some(s.current_info())
    }

    /// Find an existing sandbox for a (user, project) pair (no
    /// touch — used only by `get_or_create`). A sandbox belongs to
    /// exactly one user; multiple users on the same project each
    /// get their own sandbox.
    pub fn find_by_user_project(&self, user_id: &str, project_id: &str) -> Option<Uuid> {
        self.by_user_project
            .read()
            .unwrap()
            .get(&(user_id.to_string(), project_id.to_string()))
            .copied()
    }

    /// Insert a freshly-spawned sandbox.
    pub fn insert(&self, sandbox_id: Uuid, info: SandboxInfo) -> SandboxInfo {
        self.insert_inner(sandbox_id, info, None)
    }

    /// Insert a sandbox alongside its lifted auth material. Used by
    /// the controller's restart-restore path (preview-URL § II.0):
    /// once a sealed record's `/version` rebind probe succeeds, the
    /// info + auth go in together so the registry holds a complete
    /// view. Phase 1's preview proxy reads `auth` here.
    pub fn insert_with_auth(
        &self,
        sandbox_id: Uuid,
        info: SandboxInfo,
        auth: SandboxAuth,
    ) -> SandboxInfo {
        self.insert_inner(sandbox_id, info, Some(auth))
    }

    fn insert_inner(
        &self,
        sandbox_id: Uuid,
        info: SandboxInfo,
        auth: Option<SandboxAuth>,
    ) -> SandboxInfo {
        let now = Instant::now();
        let sandbox = Sandbox {
            info: info.clone(),
            created_at: now,
            last_used: Arc::new(RwLock::new(now)),
            auth,
            preview_secrets: Arc::new(RwLock::new(None)),
            preview_audit: Arc::new(RwLock::new(Vec::new())),
        };
        let key = (info.user_id.clone(), info.project_id.clone());
        self.by_sandbox.write().unwrap().insert(sandbox_id, sandbox);
        self.by_user_project.write().unwrap().insert(key, sandbox_id);
        info
    }

    /// Restore Phase-3 share-token state from a sealed record. Used
    /// by [`crate::restore::restore_at_startup`] when re-hydrating a
    /// v2 sealed record. No-op for v1 records (`secrets` and
    /// `audit` both empty/None).
    pub fn restore_preview_state(
        &self,
        id: Uuid,
        secrets: Option<PreviewSecrets>,
        audit: Vec<PreviewAuditEntry>,
    ) {
        let guard = self.by_sandbox.read().unwrap();
        if let Some(s) = guard.get(&id) {
            *s.preview_secrets.write().unwrap() = secrets;
            *s.preview_audit.write().unwrap() = audit;
        }
    }

    /// Mint a fresh secret ring for a sandbox if none exists yet.
    /// Returns the freshly-minted (or already-present) secret bytes
    /// and current version. Idempotent: the second call on the same
    /// sandbox returns the same ring as the first.
    ///
    /// Returns `None` for an unknown `id` (the sandbox was reaped
    /// between the controller's authn check and this call — caller
    /// should surface a uniform 404).
    pub fn ensure_preview_secret(&self, id: Uuid) -> Option<PreviewSecrets> {
        let guard = self.by_sandbox.read().unwrap();
        let s = guard.get(&id)?;
        let mut w = s.preview_secrets.write().unwrap();
        if let Some(ring) = w.as_ref() {
            return Some(ring.clone());
        }
        let ring = PreviewSecrets {
            sv_current: 1,
            current: fresh_secret_bytes(),
            previous: None,
            grace_until_unix: None,
        };
        *w = Some(ring.clone());
        Some(ring)
    }

    /// Look up the secret bytes for `sv`. Returns the current secret
    /// if `sv == sv_current`, the previous secret if `sv ==
    /// sv_current - 1` AND we're still inside the grace window;
    /// otherwise `None` (rejected as `revoked`). Lazily clears
    /// `previous` once the grace window is past.
    pub fn secret_for_version(&self, id: Uuid, sv: u32) -> Option<[u8; 32]> {
        let guard = self.by_sandbox.read().unwrap();
        let s = guard.get(&id)?;
        // Promote the read lock to a write only when we need to clear
        // a stale `previous`. The common path (sv == current) reads
        // through the write guard once (cheap) and returns.
        let mut w = s.preview_secrets.write().unwrap();
        let ring = w.as_mut()?;
        if sv == ring.sv_current {
            return Some(ring.current);
        }
        if sv + 1 == ring.sv_current {
            // Previous-version. Honor only inside grace.
            let now = now_unix();
            let in_grace = ring
                .grace_until_unix
                .is_some_and(|until| now < until);
            if in_grace {
                if let Some(prev) = ring.previous {
                    return Some(prev);
                }
            }
            // Past grace OR no previous: clear the field lazily so
            // future calls short-circuit cheaply, then deny.
            ring.previous = None;
            ring.grace_until_unix = None;
            return None;
        }
        None
    }

    /// Bump the secret version. `explicit_delete=true` is the
    /// zero-grace path (DELETE /share): the previous secret is
    /// cleared immediately AND the audit table is wiped (revoking
    /// every still-live token at once).
    /// `explicit_delete=false` is the organic-rotation path: the
    /// previous secret is honoured for `PREVIEW_SECRET_GRACE_SECS`.
    ///
    /// Returns the new ring on success; `None` for unknown id.
    pub fn rotate_preview_secret(
        &self,
        id: Uuid,
        explicit_delete: bool,
    ) -> Option<PreviewSecrets> {
        let guard = self.by_sandbox.read().unwrap();
        let s = guard.get(&id)?;
        let mut w = s.preview_secrets.write().unwrap();
        let new_current = fresh_secret_bytes();
        let new_ring = if let Some(prev_ring) = w.as_ref() {
            let new_sv = prev_ring.sv_current.saturating_add(1);
            if explicit_delete {
                PreviewSecrets {
                    sv_current: new_sv,
                    current: new_current,
                    previous: None,
                    grace_until_unix: None,
                }
            } else {
                PreviewSecrets {
                    sv_current: new_sv,
                    current: new_current,
                    previous: Some(prev_ring.current),
                    grace_until_unix: Some(now_unix() + PREVIEW_SECRET_GRACE_SECS),
                }
            }
        } else {
            // No prior ring — first call to rotate is functionally
            // equivalent to ensure_preview_secret. Audit-table is
            // already empty; nothing to wipe.
            PreviewSecrets {
                sv_current: 1,
                current: new_current,
                previous: None,
                grace_until_unix: None,
            }
        };
        *w = Some(new_ring.clone());
        if explicit_delete {
            // Revoke every still-live audit row. The DELETE returns
            // `revoked: "all"` per § II.4.
            s.preview_audit.write().unwrap().clear();
        }
        Some(new_ring)
    }

    /// Snapshot the secret ring (read-only). Callers who only need to
    /// emit `sv_current` to a freshly-minted token use this; mint
    /// itself uses [`Self::ensure_preview_secret`] to get the ring AND
    /// trigger first-time creation.
    pub fn preview_secret(&self, id: Uuid) -> Option<PreviewSecrets> {
        let guard = self.by_sandbox.read().unwrap();
        let s = guard.get(&id)?;
        let r = s.preview_secrets.read().unwrap();
        r.clone()
    }

    /// Append an audit entry. Caller is responsible for honoring the
    /// per-sandbox-per-day mint cap before calling.
    pub fn append_audit(&self, id: Uuid, entry: PreviewAuditEntry) -> bool {
        let guard = self.by_sandbox.read().unwrap();
        let Some(s) = guard.get(&id) else {
            return false;
        };
        s.preview_audit.write().unwrap().push(entry);
        true
    }

    /// Snapshot the audit rows for a sandbox. Returns an empty `Vec`
    /// for unknown id (callers don't need to differentiate "no
    /// tokens" from "no sandbox" at this layer — the caller's authn
    /// gate already established the sandbox exists).
    pub fn list_audit(&self, id: Uuid) -> Vec<PreviewAuditEntry> {
        let guard = self.by_sandbox.read().unwrap();
        guard
            .get(&id)
            .map(|s| s.preview_audit.read().unwrap().clone())
            .unwrap_or_default()
    }

    /// Record a successful token use against the audit table. Bumps
    /// `last_used_at_unix` and `use_count`. No-op for unknown
    /// `(sandbox, token_id)` — the validator already gated this call
    /// on a successful HMAC verify, so a missing audit row only
    /// happens after a controller-restart with audit-rebuild
    /// pending, in which case we silently skip rather than refuse the
    /// otherwise-valid token.
    pub fn record_audit_use(&self, id: Uuid, token_id: &str) {
        let guard = self.by_sandbox.read().unwrap();
        if let Some(s) = guard.get(&id) {
            let mut w = s.preview_audit.write().unwrap();
            if let Some(row) = w.iter_mut().find(|r| r.token_id == token_id) {
                row.last_used_at_unix = now_unix();
                row.use_count = row.use_count.saturating_add(1);
            }
        }
    }

    /// Snapshot for sealing. Returns `(secrets, audit)` ready to drop
    /// into a fresh `SealedAuth`. Both are cheap clones.
    pub fn preview_state_for_seal(
        &self,
        id: Uuid,
    ) -> (Option<SealedPreviewSecrets>, Vec<SealedAuditEntry>) {
        let guard = self.by_sandbox.read().unwrap();
        let Some(s) = guard.get(&id) else {
            return (None, Vec::new());
        };
        let secrets = s
            .preview_secrets
            .read()
            .unwrap()
            .as_ref()
            .map(PreviewSecrets::to_sealed);
        let audit = s
            .preview_audit
            .read()
            .unwrap()
            .iter()
            .map(PreviewAuditEntry::to_sealed)
            .collect();
        (secrets, audit)
    }

    /// Look up the auth bundle for a sandbox-id. Cheap; clones an
    /// `Arc` (no secret-bytes copy). Returns `None` for sandboxes
    /// inserted via `insert` (no auth attached) — callers should
    /// fall back to `Backend::session_auth` for those.
    pub fn get_auth(&self, id: &Uuid) -> Option<SandboxAuth> {
        let guard = self.by_sandbox.read().unwrap();
        guard.get(id)?.auth.clone()
    }

    /// Drop a sandbox from the registry. Caller should already have
    /// stopped the underlying runtime via the backend.
    pub fn remove(&self, id: &Uuid) -> Option<SandboxInfo> {
        let mut sandboxes = self.by_sandbox.write().unwrap();
        let sandbox = sandboxes.remove(id)?;
        let info = sandbox.current_info();
        let key = (sandbox.info.user_id.clone(), sandbox.info.project_id.clone());
        let mut by_up = self.by_user_project.write().unwrap();
        if by_up.get(&key).copied() == Some(*id) {
            by_up.remove(&key);
        }
        Some(info)
    }

    /// Snapshot of all current sandboxes.
    pub fn list(&self) -> Vec<SandboxInfo> {
        self.by_sandbox
            .read()
            .unwrap()
            .values()
            .map(Sandbox::current_info)
            .collect()
    }

    /// IDs exceeding either limit. Read-only — caller stops the
    /// runtime via the backend, then calls `remove`.
    fn expired(&self, idle_threshold: Duration, max_lifetime: Duration) -> Vec<Uuid> {
        let mut out = Vec::new();
        let guard = self.by_sandbox.read().unwrap();
        for (id, s) in guard.iter() {
            if s.idle_for() > idle_threshold || s.lived_for() > max_lifetime {
                out.push(*id);
            }
        }
        out
    }
}

/// Background task that sweeps expired sandboxes and stops their
/// runtimes. Runs every minute; cheap (in-memory walk + at most
/// one backend.stop per expired sandbox).
///
/// **Panic recovery.** A previous version held a single
/// `compio::runtime::spawn(...).detach()` — any panic in the loop
/// (RwLock poisoning, malformed UUID, weird backend error) killed
/// the task forever, with no log saying GC died. Sandboxes
/// accumulated indefinitely. We now wrap each iteration in
/// `catch_unwind`; a single bad sweep is logged and we keep
/// running.
#[cfg(test)]
mod tests {
    use super::*;

    fn make_info(id: Uuid, user: &str) -> SandboxInfo {
        SandboxInfo {
            sandbox_id: id.to_string(),
            user_id: user.into(),
            project_id: "p".into(),
            backend: "nomad-ch".into(),
            backend_hint: "test".into(),
            created_at_secs: 0,
            last_used_at_secs: 0,
        }
    }

    #[test]
    fn ensure_preview_secret_is_idempotent() {
        let r = SandboxRegistry::new();
        let id = Uuid::now_v7();
        r.insert(id, make_info(id, "alice"));
        let a = r.ensure_preview_secret(id).unwrap();
        let b = r.ensure_preview_secret(id).unwrap();
        assert_eq!(a.sv_current, 1);
        assert_eq!(a.sv_current, b.sv_current);
        assert_eq!(a.current, b.current, "idempotent — same bytes both calls");
        assert!(a.previous.is_none());
    }

    #[test]
    fn ensure_preview_secret_unknown_id_returns_none() {
        let r = SandboxRegistry::new();
        let id = Uuid::now_v7();
        assert!(r.ensure_preview_secret(id).is_none());
    }

    #[test]
    fn rotate_organic_keeps_previous_in_grace() {
        let r = SandboxRegistry::new();
        let id = Uuid::now_v7();
        r.insert(id, make_info(id, "alice"));
        let v1 = r.ensure_preview_secret(id).unwrap();
        let v2 = r.rotate_preview_secret(id, false).unwrap();
        assert_eq!(v2.sv_current, v1.sv_current + 1);
        assert_eq!(v2.previous, Some(v1.current), "previous secret retained");
        assert!(v2.grace_until_unix.is_some(), "organic rotation sets grace");
        // Both versions resolve.
        assert_eq!(r.secret_for_version(id, v2.sv_current), Some(v2.current));
        assert_eq!(r.secret_for_version(id, v1.sv_current), Some(v1.current));
    }

    #[test]
    fn rotate_explicit_delete_is_zero_grace() {
        let r = SandboxRegistry::new();
        let id = Uuid::now_v7();
        r.insert(id, make_info(id, "alice"));
        let v1 = r.ensure_preview_secret(id).unwrap();
        // Pre-seed an audit row so we can assert it's wiped.
        r.append_audit(
            id,
            PreviewAuditEntry {
                token_id: "shr_x".into(),
                port: 5173,
                issued_at_unix: 0,
                expires_at_unix: 9_999_999_999,
                scope: "ro".into(),
                secret_version: 1,
                iss: None,
                last_used_at_unix: 0,
                use_count: 0,
            },
        );
        assert_eq!(r.list_audit(id).len(), 1);

        let v2 = r.rotate_preview_secret(id, true).unwrap();
        assert_eq!(v2.sv_current, v1.sv_current + 1);
        assert_eq!(v2.previous, None, "explicit DELETE clears previous");
        assert_eq!(v2.grace_until_unix, None);
        // Old secret immediately rejected — no grace.
        assert_eq!(r.secret_for_version(id, v1.sv_current), None);
        // Audit table wiped.
        assert!(r.list_audit(id).is_empty(), "explicit DELETE clears audit");
    }

    #[test]
    fn secret_for_version_unknown_sv_returns_none() {
        let r = SandboxRegistry::new();
        let id = Uuid::now_v7();
        r.insert(id, make_info(id, "alice"));
        r.ensure_preview_secret(id);
        // sv far in the future — refuse.
        assert_eq!(r.secret_for_version(id, 999), None);
        // sv == 0: also rejected (token claims ‹ current).
        assert_eq!(r.secret_for_version(id, 0), None);
    }

    #[test]
    fn append_audit_unknown_id_is_false() {
        let r = SandboxRegistry::new();
        let id = Uuid::now_v7();
        assert!(!r.append_audit(
            id,
            PreviewAuditEntry {
                token_id: "shr_x".into(),
                port: 5173,
                issued_at_unix: 0,
                expires_at_unix: 9_999_999_999,
                scope: "ro".into(),
                secret_version: 1,
                iss: None,
                last_used_at_unix: 0,
                use_count: 0,
            },
        ));
    }

    #[test]
    fn record_audit_use_bumps_count_and_last_used() {
        let r = SandboxRegistry::new();
        let id = Uuid::now_v7();
        r.insert(id, make_info(id, "alice"));
        r.append_audit(
            id,
            PreviewAuditEntry {
                token_id: "shr_x".into(),
                port: 5173,
                issued_at_unix: 0,
                expires_at_unix: 9_999_999_999,
                scope: "ro".into(),
                secret_version: 1,
                iss: None,
                last_used_at_unix: 0,
                use_count: 0,
            },
        );
        r.record_audit_use(id, "shr_x");
        r.record_audit_use(id, "shr_x");
        let rows = r.list_audit(id);
        assert_eq!(rows[0].use_count, 2);
        assert!(rows[0].last_used_at_unix > 0);
    }

    #[test]
    fn restore_preview_state_overwrites_in_place() {
        let r = SandboxRegistry::new();
        let id = Uuid::now_v7();
        r.insert(id, make_info(id, "alice"));
        // Seed something we expect overwrite to clear.
        r.ensure_preview_secret(id);
        r.append_audit(
            id,
            PreviewAuditEntry {
                token_id: "old".into(),
                port: 1,
                issued_at_unix: 0,
                expires_at_unix: 0,
                scope: "ro".into(),
                secret_version: 99,
                iss: None,
                last_used_at_unix: 0,
                use_count: 0,
            },
        );
        let restored_secrets = PreviewSecrets {
            sv_current: 7,
            current: [0xAB; 32],
            previous: None,
            grace_until_unix: None,
        };
        let restored_audit = vec![PreviewAuditEntry {
            token_id: "new".into(),
            port: 5173,
            issued_at_unix: 100,
            expires_at_unix: 200,
            scope: "rw".into(),
            secret_version: 7,
            iss: Some("usr_alice".into()),
            last_used_at_unix: 0,
            use_count: 0,
        }];
        r.restore_preview_state(id, Some(restored_secrets), restored_audit);
        let s = r.preview_secret(id).unwrap();
        assert_eq!(s.sv_current, 7);
        assert_eq!(s.current, [0xAB; 32]);
        let rows = r.list_audit(id);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].token_id, "new");
    }
}

pub fn start_idle_gc(state: Arc<AppState>) {
    compio::runtime::spawn(async move {
        let interval = Duration::from_secs(60);
        let idle = Duration::from_secs(state.config.idle_timeout_secs);
        let max_life = Duration::from_secs(state.config.max_lifetime_secs);
        loop {
            compio::time::sleep(interval).await;
            // Sync portion (lock walk) wrapped for panic safety.
            // The async `backend.stop` happens after, with its own
            // best-effort error log.
            let to_kill = match std::panic::catch_unwind(
                std::panic::AssertUnwindSafe(|| state.sandboxes.expired(idle, max_life)),
            ) {
                Ok(ids) => ids,
                Err(p) => {
                    eprintln!(
                        "[sandbox] gc: panic during expired-walk; continuing: {p:?}"
                    );
                    continue;
                }
            };
            for id in to_kill {
                if let Some(info) = state.sandboxes.get(&id) {
                    eprintln!(
                        "[sandbox] gc: stopping idle sandbox {} (user={}, project={}, backend={})",
                        info.sandbox_id, info.user_id, info.project_id, info.backend,
                    );
                }
                // Don't unwrap-or-panic — propagate the failure as a
                // log line and move on. The registry remove below
                // is also wrapped in case a poisoned lock would
                // otherwise kill the loop.
                if let Err(e) = state.backend.stop(id).await {
                    eprintln!("[sandbox] gc: backend.stop({id}) failed: {e}");
                }
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    state.sandboxes.remove(&id);
                }));
            }
        }
    })
    .detach();
}
