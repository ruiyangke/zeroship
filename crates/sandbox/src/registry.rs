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

use crate::backend::SandboxInfo;
use crate::AppState;

/// Internal sandbox record. Holds the public `SandboxInfo` plus
/// timing data for the GC.
#[derive(Clone)]
struct Sandbox {
    info: SandboxInfo,
    created_at: Instant,
    last_used: Arc<RwLock<Instant>>,
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
        let now = Instant::now();
        let sandbox = Sandbox {
            info: info.clone(),
            created_at: now,
            last_used: Arc::new(RwLock::new(now)),
        };
        let key = (info.user_id.clone(), info.project_id.clone());
        self.by_sandbox.write().unwrap().insert(sandbox_id, sandbox);
        self.by_user_project.write().unwrap().insert(key, sandbox_id);
        info
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
pub fn start_idle_gc(state: Arc<AppState>) {
    compio::runtime::spawn(async move {
        let interval = Duration::from_secs(60);
        let idle = Duration::from_secs(state.config.idle_timeout_secs);
        let max_life = Duration::from_secs(state.config.max_lifetime_secs);
        loop {
            compio::time::sleep(interval).await;
            let to_kill = state.sandboxes.expired(idle, max_life);
            for id in to_kill {
                if let Some(info) = state.sandboxes.get(&id) {
                    eprintln!(
                        "[sandbox] gc: stopping idle sandbox {} (user={}, project={}, backend={})",
                        info.sandbox_id, info.user_id, info.project_id, info.backend,
                    );
                }
                if let Err(e) = state.backend.stop(id).await {
                    eprintln!("[sandbox] gc: backend.stop({id}) failed: {e}");
                }
                state.sandboxes.remove(&id);
            }
        }
    })
    .detach();
}
