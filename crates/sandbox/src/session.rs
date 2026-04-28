//! Session registry: tracks live sandbox containers, schedules
//! teardown when idle.
//!
//! A session is the unit of "one project being edited right now":
//! exactly one Docker container, exactly one bind-mounted workspace
//! under `{workspace_root}/{project_id}/`. Multiple editor tabs on
//! the same project reuse the same session (the registry dedups by
//! `project_id`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use uuid::Uuid;

use crate::AppState;

#[derive(Clone, Debug, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub project_id: String,
    pub container_id: String,
    pub container_name: String,
    pub container_ip: String,
    pub workspace_path: String,
    pub created_at_secs: u64,
    pub last_used_at_secs: u64,
}

/// Internal session record. `last_used` is updated on every API call
/// so the GC knows whether the session is active.
#[derive(Clone)]
struct Session {
    session_id: Uuid,
    project_id: String,
    container_id: String,
    container_name: String,
    container_ip: String,
    workspace_path: PathBuf,
    created_at: Instant,
    created_at_unix: u64,
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

    fn to_info(&self) -> SessionInfo {
        let last = *self.last_used.read().unwrap();
        let last_unix = self.created_at_unix
            + last.saturating_duration_since(self.created_at).as_secs();
        SessionInfo {
            session_id: self.session_id.to_string(),
            project_id: self.project_id.clone(),
            container_id: self.container_id.clone(),
            container_name: self.container_name.clone(),
            container_ip: self.container_ip.clone(),
            workspace_path: self.workspace_path.to_string_lossy().into_owned(),
            created_at_secs: self.created_at_unix,
            last_used_at_secs: last_unix,
        }
    }
}

/// In-memory session registry. Indexed twice: once by `session_id`
/// (the key the editor app holds), once by `project_id` (so re-opens
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
        Some(s.to_info())
    }

    /// Find an existing session for a project (no touch — used only
    /// by `get_or_create`).
    pub fn find_by_project(&self, project_id: &str) -> Option<Uuid> {
        self.by_project.read().unwrap().get(project_id).copied()
    }

    /// Insert a freshly-spawned session.
    pub fn insert(
        &self,
        session_id: Uuid,
        project_id: String,
        container_id: String,
        container_name: String,
        container_ip: String,
        workspace_path: PathBuf,
    ) -> SessionInfo {
        let now = Instant::now();
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let session = Session {
            session_id,
            project_id: project_id.clone(),
            container_id,
            container_name,
            container_ip,
            workspace_path,
            created_at: now,
            created_at_unix: now_unix,
            last_used: Arc::new(RwLock::new(now)),
        };

        let info = session.to_info();
        self.by_session.write().unwrap().insert(session_id, session);
        self.by_project.write().unwrap().insert(project_id, session_id);
        info
    }

    /// Drop a session from the registry (does NOT stop the container —
    /// caller must do that first).
    pub fn remove(&self, id: &Uuid) -> Option<SessionInfo> {
        let mut sessions = self.by_session.write().unwrap();
        let session = sessions.remove(id)?;
        let info = session.to_info();
        let mut by_project = self.by_project.write().unwrap();
        if by_project.get(&session.project_id).copied() == Some(*id) {
            by_project.remove(&session.project_id);
        }
        Some(info)
    }

    /// Snapshot of all current sessions (for the GC and the list endpoint).
    pub fn list(&self) -> Vec<SessionInfo> {
        self.by_session
            .read()
            .unwrap()
            .values()
            .map(Session::to_info)
            .collect()
    }

    /// IDs of sessions exceeding either limit. Read-only — caller
    /// stops the container then calls `remove`.
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
/// containers. Runs every minute; cheap (just an in-memory walk).
pub fn start_idle_gc(state: Arc<AppState>) {
    compio::runtime::spawn(async move {
        let interval = Duration::from_secs(60);
        let idle = Duration::from_secs(state.config.idle_timeout_secs);
        let max_life = Duration::from_secs(state.config.max_lifetime_secs);
        loop {
            compio::time::sleep(interval).await;
            let to_kill = state.sessions.expired(idle, max_life);
            for id in to_kill {
                let info = match state.sessions.get(&id) {
                    Some(i) => i,
                    None => continue,
                };
                eprintln!(
                    "[sandbox] gc: stopping idle session {} (project={})",
                    info.session_id, info.project_id,
                );
                if let Err(e) = crate::docker::stop_container(&info.container_name).await {
                    eprintln!("[sandbox] gc: stop {} failed: {e}", info.container_name);
                }
                state.sessions.remove(&id);
            }
        }
    })
    .detach();
}
