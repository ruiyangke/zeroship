//! Nomad + Cloud Hypervisor backend — scaffolding stub.
//!
//! This file exists so the `Backend` enum can carry a `NomadCh`
//! variant from day one of the feature branch. The full
//! create/stop/exec/file lifecycle is filled in by subsequent
//! commits; this stub returns "not implemented" for any method
//! that needs the wrapper script + Nomad HTTP plumbing.
//!
//! See `crates/sandbox/src/backend/k8s.rs` for the closest analog —
//! the eventual implementation mirrors that file's structure
//! (Ed25519 keypair lifecycle, `CreateGuard` cleanup, per-user
//! `ReleaseCreating` gate, `wait_for_agent_livez`, `http_signed_async`
//! over `compio::runtime::spawn_blocking`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use uuid::Uuid;

use super::{ExecOutput, SandboxInfo, TreeEntry};
use crate::config::SandboxConfig;

#[derive(Debug)]
pub struct NomadCHBackend {
    #[allow(dead_code)]
    cfg: SandboxConfig,
    healthy: Arc<AtomicBool>,
}

impl NomadCHBackend {
    pub fn new(cfg: SandboxConfig) -> Result<Self, String> {
        Ok(Self {
            cfg,
            healthy: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub async fn probe(&self) -> Result<(), String> {
        // Stub: actual `/v1/status/leader` reachability check
        // lands in the next commit.
        Err("nomad-ch backend: probe not yet implemented".into())
    }

    pub async fn cleanup_orphans_at_startup(&self) -> Result<usize, String> {
        Ok(0)
    }

    pub async fn create(
        &self,
        _sandbox_id: Uuid,
        _user_id: &str,
        _project_id: &str,
    ) -> Result<SandboxInfo, String> {
        Err("nomad-ch backend: create not yet implemented".into())
    }

    pub async fn stop(&self, _sandbox_id: Uuid) -> Result<(), String> {
        Err("nomad-ch backend: stop not yet implemented".into())
    }

    pub async fn exec(
        &self,
        _sandbox_id: Uuid,
        _cmd: &str,
        _cwd: Option<&str>,
        _timeout_ms: Option<u64>,
    ) -> Result<ExecOutput, String> {
        Err("nomad-ch backend: exec not yet implemented".into())
    }

    pub async fn read_file(&self, _sandbox_id: Uuid, _path: &str) -> Result<Vec<u8>, String> {
        Err("nomad-ch backend: read_file not yet implemented".into())
    }

    pub async fn write_file(
        &self,
        _sandbox_id: Uuid,
        _path: &str,
        _body: &[u8],
    ) -> Result<(), String> {
        Err("nomad-ch backend: write_file not yet implemented".into())
    }

    pub async fn delete_file(&self, _sandbox_id: Uuid, _path: &str) -> Result<bool, String> {
        Err("nomad-ch backend: delete_file not yet implemented".into())
    }

    pub async fn file_tree(&self, _sandbox_id: Uuid) -> Result<Vec<TreeEntry>, String> {
        Err("nomad-ch backend: file_tree not yet implemented".into())
    }
}
