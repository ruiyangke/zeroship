//! Session registry: tracks live sandbox sessions, schedules teardown
//! when idle. Backend-agnostic — the registry holds public-facing
//! [`SessionInfo`]; the active runtime (Docker container, k8s Pod, etc.)
//! lives inside the [`crate::backend::Backend`] enum and is keyed by
//! `session_id`.
//!
//! A session is the unit of "one project being edited right now".
//! Multiple editor tabs on the same project reuse the same session
//! (the registry dedups by `project_id`).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::backend::SessionInfo;
use crate::AppState;

/// Internal session record. Holds the public `SessionInfo` plus
/// timing data for the GC.
#[derive(Clone)]
struct Session {
    info: SessionInfo,
    created_at: Instant,
    last_used: Arc<RwLock<Instant>>,
}

impl Session {
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

    fn current_info(&self) -> SessionInfo {
        let last = *self.last_used.read().unwrap();
        let bumped = self.info.created_at_secs
            + last.saturating_duration_since(self.created_at).as_secs();
        SessionInfo {
            last_used_at_secs: bumped,
            ..self.info.clone()
        }
    }
}

/// In-memory session registry. Indexed twice: once by `session_id`
/// (the key the client holds), once by `project_id` (so re-opens
/// from the same project find the existing session).
#[derive(Clone, Default)]
pub struct SessionRegistry {
    by_session: Arc<RwLock<HashMap<Uuid, Session>>>,
    by_project: Arc<RwLock<HashMap<String, Uuid>>>,
}

impl std::fmt::Debug for SessionRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionRegistry").finish_non_exhaustive()
    }
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lookup by id; touches `last_used` on hit.
    pub fn get(&self, id: &Uuid) -> Option<SessionInfo> {
        let guard = self.by_session.read().unwrap();
        let s = guard.get(id)?;
        s.touch();
        Some(s.current_info())
    }

    /// Find an existing session for a project (no touch — used only
    /// by `get_or_create`).
    pub fn find_by_project(&self, project_id: &str) -> Option<Uuid> {
        self.by_project.read().unwrap().get(project_id).copied()
    }

    /// Insert a freshly-spawned session.
    pub fn insert(&self, session_id: Uuid, info: SessionInfo) -> SessionInfo {
        let now = Instant::now();
        let session = Session {
            info: info.clone(),
            created_at: now,
            last_used: Arc::new(RwLock::new(now)),
        };
        let project = info.project_id.clone();
        self.by_session.write().unwrap().insert(session_id, session);
        self.by_project.write().unwrap().insert(project, session_id);
        info
    }

    /// Drop a session from the registry. Caller should already have
    /// stopped the underlying runtime via the backend.
    pub fn remove(&self, id: &Uuid) -> Option<SessionInfo> {
        let mut sessions = self.by_session.write().unwrap();
        let session = sessions.remove(id)?;
        let info = session.current_info();
        let mut by_project = self.by_project.write().unwrap();
        if by_project.get(&session.info.project_id).copied() == Some(*id) {
            by_project.remove(&session.info.project_id);
        }
        Some(info)
    }

    /// Snapshot of all current sessions.
    pub fn list(&self) -> Vec<SessionInfo> {
        self.by_session
            .read()
            .unwrap()
            .values()
            .map(Session::current_info)
            .collect()
    }

    /// IDs exceeding either limit. Read-only — caller stops the
    /// runtime via the backend, then calls `remove`.
    fn expired(&self, idle_threshold: Duration, max_lifetime: Duration) -> Vec<Uuid> {
        let mut out = Vec::new();
        let guard = self.by_session.read().unwrap();
        for (id, s) in guard.iter() {
            if s.idle_for() > idle_threshold || s.lived_for() > max_lifetime {
                out.push(*id);
            }
        }
        out
    }
}

/// Background task that sweeps expired sessions and stops their
/// runtimes. Runs every minute; cheap (in-memory walk + at most
/// one backend.stop per expired session).
pub fn start_idle_gc(state: Arc<AppState>) {
    compio::runtime::spawn(async move {
        let interval = Duration::from_secs(60);
        let idle = Duration::from_secs(state.config.idle_timeout_secs);
        let max_life = Duration::from_secs(state.config.max_lifetime_secs);
        loop {
            compio::time::sleep(interval).await;
            let to_kill = state.sessions.expired(idle, max_life);
            for id in to_kill {
                if let Some(info) = state.sessions.get(&id) {
                    eprintln!(
                        "[sandbox] gc: stopping idle session {} (project={}, backend={})",
                        info.session_id, info.project_id, info.backend,
                    );
                }
                if let Err(e) = state.backend.stop(id).await {
                    eprintln!("[sandbox] gc: backend.stop({id}) failed: {e}");
                }
                state.sessions.remove(&id);
            }
        }
    })
    .detach();
}
