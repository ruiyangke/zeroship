//! Controller restart restore (pg-driven).
//!
//! On boot, the controller queries pg for every sandbox row owned by
//! its `host_id` with `status='running'`, then unseals the matching
//! sealed record by sandbox_id. The sealed record holds secret
//! material only (`signing_key`, `preview_secrets`, `boot_id`); pg
//! holds every other field (`user_id`, `project_id`, `backend`,
//! `vm_index`, `agent_url`, `key_fp`, `created_at`).
//!
//! This module is the boot-time reconciler. There is no periodic
//! reconciler: pg is the single writer for non-secret state;
//! sealed is the single writer for secret state — categories don't
//! overlap, so steady-state has no drift source).
//!
//! ## Outcomes per pg row
//!
//! | Probe result | Action | Pg row | Sealed file |
//! |---|---|---|---|
//! | match | restore in-memory; UPDATE last_used_at | running | kept |
//! | mismatch (different fp) | UPDATE status='recreating' | recreating | deleted |
//! | unreachable | UPDATE status='unreachable' | unreachable | kept |
//! | sealed missing | UPDATE status='lost' | lost | (none) |
//!
//! Plus the orphan sweep:
//! - **Sealed without pg row** → unlink (cancelled-create orphan).
//!
//! ## Current scope
//!
//! Backend rehydration is implemented for nomad-ch only; Docker and
//! K8s bubble up an `Err` from `Backend::restore_from_sealed`. Their
//! sealed records are kept on disk for a future binary that knows
//! how to restore them, and pg rows stay marked `running` (the
//! operator decides whether to delete or wait).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use uuid::Uuid;
use zeroship_sandbox_agent::sig;

use crate::backend::{Backend, SandboxAuth, SandboxInfo};
use crate::db::{Database, SandboxRow, SandboxStatus};
use crate::persist::{seal_filename_for, unseal_one, AeadKey, SealedAuth};
use crate::registry::SandboxRegistry;

/// Per-row outcome the boot path emits for telemetry / tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// Probe matched the agent's reported fingerprint; backend +
    /// registry state restored.
    Restored,
    /// Probe answered with a different fingerprint (or 401-with-
    /// stale-pubkey). Pg row marked `recreating`; sealed file deleted.
    Mismatched,
    /// Agent unreachable within the per-record probe window. Pg row
    /// marked `unreachable`; sealed file left in place.
    Unreachable,
    /// AEAD-unseal failed (corrupt file or wrong key). Pg row marked
    /// `lost`. Sealed file left for the operator's quarantine
    /// runbook.
    Corrupt,
    /// Pg row exists but no sealed file on this host. Sandbox cannot
    /// be reconstructed; `status='lost'` for operator review.
    SealMissing,
    /// Backend doesn't yet know how to rehydrate state for this
    /// row's `backend` field. Pg row left alone; sealed file kept.
    BackendUnsupported,
}

#[derive(Debug, Clone, Default)]
pub struct RestoreSummary {
    pub records_seen: usize,
    pub restored: usize,
    pub mismatched: usize,
    pub unreachable: usize,
    pub corrupt: usize,
    pub seal_missing: usize,
    pub unsupported: usize,
    /// Sealed records orphaned from a partially-cancelled create
    /// (no pg row for this host); unlinked by this pass.
    pub orphans_unlinked: usize,
}

/// Per-probe deadline. Conservative for v1 — a sandbox that doesn't
/// answer a signed `/version` within ~3 s is unlikely to be one we
/// can usefully restore. Adjustable per-test.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Boot-path entry-point. Queries pg for every sandbox row owned by
/// this host; for each row, unseals the matching sealed record by
/// sandbox_id and signed-`/version` probes the agent.
///
/// The orphan sweep (sealed records without a matching pg row for
/// this host) runs at the end of the pass.
pub async fn restore_at_startup(
    database: &Database,
    persist_dir: &Path,
    aead_key: &AeadKey,
    backend: &Backend,
    registry: &SandboxRegistry,
    probe_timeout: Duration,
) -> std::io::Result<RestoreSummary> {
    let sealed_dir = persist_dir.join("sealed-records");
    let host_id = database.host_id();

    // 1. Pg-driven restore: load all 'running' rows for this host.
    let rows = match database.list_running_sandboxes_for_host(host_id).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                error = %e,
                host_id = %host_id,
                "sandbox/restore: pg query for running sandboxes failed; continuing with empty set"
            );
            Vec::new()
        }
    };

    let mut sum = RestoreSummary::default();
    sum.records_seen = rows.len();

    // Track which sealed files the pg-driven pass touched, so the
    // orphan sweep can unlink the ones it didn't.
    let mut consumed = std::collections::HashSet::new();

    for row in rows {
        let outcome = process_pg_row(
            database,
            &sealed_dir,
            aead_key,
            backend,
            registry,
            &row,
            probe_timeout,
            &mut consumed,
        )
        .await;
        match outcome {
            RestoreOutcome::Restored => sum.restored += 1,
            RestoreOutcome::Mismatched => sum.mismatched += 1,
            RestoreOutcome::Unreachable => sum.unreachable += 1,
            RestoreOutcome::Corrupt => sum.corrupt += 1,
            RestoreOutcome::SealMissing => sum.seal_missing += 1,
            RestoreOutcome::BackendUnsupported => sum.unsupported += 1,
        }
    }

    // 2. Orphan sweep: sealed records on disk that have no
    // corresponding pg row for this host. They are leftovers from a
    // partially-cancelled create — the controller crashed between
    // seal and pg-INSERT. Unlink them.
    sum.orphans_unlinked = sweep_orphan_sealed(&sealed_dir, &consumed);

    Ok(sum)
}

/// Single-row probe-and-register entry point reused by both the
/// boot-path reconciler (`restore_at_startup`) and the takeover
/// follow-up step
/// (`spawn_takeover_task`). Wraps `process_pg_row` with an
/// owned `consumed` set since takeover callers don't run an orphan
/// sweep and don't care which sealed paths the probe touched.
///
/// The function's contract:
///   - On match: registry is populated; pg row's `last_used_at` is
///     refreshed (via the eventual heartbeat, not synchronously).
///   - On fingerprint mismatch / 401: pg row is marked
///     `recreating` and the sealed file is deleted.
///   - On unreachable: pg row is marked `unreachable`, sealed file
///     is kept (the agent may come back).
///   - On corrupt seal / typed-id mismatch / missing seal: pg row
///     is marked `lost`. The sealed file is left in place for
///     operator quarantine review (except the missing case, where
///     there's nothing to leave).
///
/// Current limitation: the new owner's local sealed-records dir might
/// not have the file. Cross-host sealed-record sync is still missing.
/// When the
/// seal is missing, we surface `RestoreOutcome::SealMissing` and
/// the caller bumps `sandbox_ha_takeover_orphan_total`.
pub(crate) async fn probe_and_register_one(
    database: &Database,
    persist_dir: &Path,
    aead_key: &AeadKey,
    backend: &Backend,
    registry: &SandboxRegistry,
    row: &SandboxRow,
    probe_timeout: Duration,
) -> RestoreOutcome {
    let sealed_dir = persist_dir.join("sealed-records");
    let mut consumed = std::collections::HashSet::new();
    process_pg_row(
        database,
        &sealed_dir,
        aead_key,
        backend,
        registry,
        row,
        probe_timeout,
        &mut consumed,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn process_pg_row(
    database: &Database,
    sealed_dir: &Path,
    aead_key: &AeadKey,
    backend: &Backend,
    registry: &SandboxRegistry,
    row: &SandboxRow,
    probe_timeout: Duration,
    consumed: &mut std::collections::HashSet<std::path::PathBuf>,
) -> RestoreOutcome {
    // Parse the sandbox_id (typed-id string) into the embedded UUID
    // so we can compute the sealed filename. An unparseable id
    // means pg disagrees with the typed-id contract; mark lost.
    let sandbox_id_uuid: Uuid = match zeroship_core::typed_id::parse(&row.sandbox_id) {
        Ok((_, uuid)) => uuid,
        Err(e) => {
            // : pre-fix this fell back to
            // `sandbox_id_from_str_lossy` → `Uuid::nil()` and fired
            // an UPDATE that no-op'd against a real row but pretended
            // to have marked it Lost. Today we skip the row entirely
            // and bump `sandbox_corrupt_id_total` so an alert can
            // catch a code/data drift (the only way to land here is
            // if a binary that DOESN'T validate typed-id at insert
            // wrote into the same pg).
            tracing::error!(
                sandbox_id = %row.sandbox_id,
                error = %e,
                "sandbox/restore: pg sandbox_id failed typed-id parse; SKIPPING row + bumping sandbox_corrupt_id_total"
            );
            crate::metrics::inc_sandbox_corrupt_id();
            return RestoreOutcome::Corrupt;
        }
    };
    let sealed_path = sealed_dir.join(seal_filename_for(sandbox_id_uuid));
    consumed.insert(sealed_path.clone());

    // Unseal.
    let sealed: SealedAuth = match unseal_one(&sealed_path, aead_key) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                sandbox_id = %row.sandbox_id,
                path = ?sealed_path,
                "sandbox/restore: pg row has no sealed record on this host; marking lost"
            );
            let _ = database
                .update_sandbox_status(sandbox_id_uuid, SandboxStatus::Lost, row.generation, None)
                .await;
            return RestoreOutcome::SealMissing;
        }
        Err(e) => {
            tracing::warn!(
                sandbox_id = %row.sandbox_id,
                path = ?sealed_path,
                error = %e,
                "sandbox/restore: sealed record corrupt; marking lost"
            );
            let _ = database
                .update_sandbox_status(sandbox_id_uuid, SandboxStatus::Lost, row.generation, None)
                .await;
            return RestoreOutcome::Corrupt;
        }
    };

    // Reconstitute the signing key. The fingerprint check is against
    // the pg-side `key_fp` (round-8: that's where it lives now).
    let signing_key = Arc::new(SigningKey::from_bytes(&sealed.signing_key_bytes));
    let derived_fp = sig::pubkey_fingerprint(&signing_key.verifying_key());
    if derived_fp != row.key_fp {
        tracing::warn!(
            sandbox_id = %row.sandbox_id,
            derived_fp = %derived_fp,
            pg_key_fp = %row.key_fp,
            "sandbox/restore: sealed signing key disagrees with pg key_fp; marking recreating + deleting sealed"
        );
        let _ = std::fs::remove_file(&sealed_path);
        let _ = database
            .update_sandbox_status(sandbox_id_uuid, SandboxStatus::Recreating, row.generation, None)
            .await;
        return RestoreOutcome::Mismatched;
    }

    // Resolve agent_url. Pg has it for docker/k8s; nomad-ch derives
    // it from vm_index, but pg also stores the derived value at
    // create time (round-8: the controller computed it once and put
    // it in the row), so we just trust the pg row.
    let agent_url = match row.agent_url.clone() {
        Some(u) => u,
        None => {
            tracing::warn!(
                sandbox_id = %row.sandbox_id,
                "sandbox/restore: pg row has no agent_url; marking lost"
            );
            let _ = database
                .update_sandbox_status(sandbox_id_uuid, SandboxStatus::Lost, row.generation, None)
                .await;
            return RestoreOutcome::Corrupt;
        }
    };

    // Probe.
    match probe_and_classify(&agent_url, &signing_key, &row.key_fp, probe_timeout).await {
        ProbeOutcome::Match { .. } => {
            // Hand off to the backend to rehydrate per-sandbox
            // state + the registry to record the SandboxInfo.
            match backend
                .restore_from_pg_and_sealed(sandbox_id_uuid, row, &sealed, agent_url.clone())
                .await
            {
                Ok(auth) => {
                    let info = build_restored_info(row);
                    registry.insert_with_auth(sandbox_id_uuid, info, auth);
                    // Hydrate the in-memory generation
                    // from the pg row so any subsequent CAS-guarded
                    // write carries the canonical value (§ 11.2).
                    // Without this, the registry would default to 0
                    // and a stop right after restore would miss the
                    // CAS for any sandbox that's seen a takeover or
                    // status flip.
                    registry.set_generation(&sandbox_id_uuid, row.generation);
                    if let Some(secrets) = sealed.preview_secrets.as_ref() {
                        let pv = crate::registry::PreviewSecrets::from_sealed(secrets);
                        registry.restore_preview_state(sandbox_id_uuid, Some(pv), Vec::new());
                    }
                    // A row that came in as 'unreachable' now passes
                    // the probe — flip
                    // it back to 'running' so subsequent dispatches
                    // see the recovered state. We CAS on the row's
                    // current generation; if a peer has moved past
                    // us in the interim, we silently let the peer
                    // own the transition.
                    if matches!(row.status, SandboxStatus::Unreachable) {
                        match database
                            .update_sandbox_status(
                                sandbox_id_uuid,
                                SandboxStatus::Running,
                                row.generation,
                                None,
                            )
                            .await
                        {
                            Ok(new_gen) => {
                                registry.set_generation(&sandbox_id_uuid, new_gen);
                                tracing::info!(
                                    sandbox_id = %row.sandbox_id,
                                    old_status = "unreachable",
                                    new_status = "running",
                                    new_generation = new_gen,
                                    "sandbox/restore: probe-Ok flipped unreachable → running"
                                );
                            }
                            Err(e) => {
                                tracing::info!(
                                    sandbox_id = %row.sandbox_id,
                                    error = %e,
                                    "sandbox/restore: unreachable→running flip skipped (CAS lost or pg err); registry hydration still applied"
                                );
                            }
                        }
                    }
                    tracing::info!(
                        sandbox_id = %row.sandbox_id,
                        user_id = %row.user_id,
                        project_id = %row.project_id,
                        agent_url = %agent_url,
                        generation = row.generation,
                        prior_status = row.status.as_str(),
                        "sandbox/restore: restored from pg + sealed"
                    );
                    RestoreOutcome::Restored
                }
                Err(e) if e.contains("doesn't yet support") || e.contains("backend mismatch") => {
                    tracing::info!(
                        sandbox_id = %row.sandbox_id,
                        backend = backend.name(),
                        error = %e,
                        "sandbox/restore: backend doesn't support restore; pg row + sealed kept for next-binary boot"
                    );
                    RestoreOutcome::BackendUnsupported
                }
                Err(e) => {
                    tracing::warn!(
                        sandbox_id = %row.sandbox_id,
                        error = %e,
                        "sandbox/restore: backend.restore_from_pg_and_sealed failed; marking lost"
                    );
                    let _ = database
                        .update_sandbox_status(sandbox_id_uuid, SandboxStatus::Lost, row.generation, None)
                        .await;
                    RestoreOutcome::Corrupt
                }
            }
        }
        ProbeOutcome::Mismatched { actual_fp } => {
            tracing::warn!(
                sandbox_id = %row.sandbox_id,
                agent_url = %agent_url,
                expected_fp = %row.key_fp,
                actual_fp = %actual_fp,
                "sandbox/restore: fp_mismatch; marking recreating + deleting sealed"
            );
            let _ = std::fs::remove_file(&sealed_path);
            let _ = database
                .update_sandbox_status(sandbox_id_uuid, SandboxStatus::Recreating, row.generation, None)
                .await;
            RestoreOutcome::Mismatched
        }
        ProbeOutcome::Unauthorized => {
            tracing::warn!(
                sandbox_id = %row.sandbox_id,
                agent_url = %agent_url,
                "sandbox/restore: /version returned 401; marking recreating + deleting sealed"
            );
            let _ = std::fs::remove_file(&sealed_path);
            let _ = database
                .update_sandbox_status(sandbox_id_uuid, SandboxStatus::Recreating, row.generation, None)
                .await;
            RestoreOutcome::Mismatched
        }
        ProbeOutcome::Unreachable(reason) => {
            tracing::warn!(
                sandbox_id = %row.sandbox_id,
                agent_url = %agent_url,
                reason = %reason,
                "sandbox/restore: agent unreachable; marking unreachable; pg row + sealed kept"
            );
            let _ = database
                .update_sandbox_status(sandbox_id_uuid, SandboxStatus::Unreachable, row.generation, None)
                .await;
            RestoreOutcome::Unreachable
        }
    }
}

// : `sandbox_id_from_str_lossy` was removed —
// see the corresponding `process_pg_row` arm above. It used to swallow
// malformed ids and fire a no-op UPDATE; now we skip the row + emit
// `sandbox_corrupt_id_total`.

fn build_restored_info(row: &SandboxRow) -> SandboxInfo {
    SandboxInfo {
        sandbox_id: row.sandbox_id.clone(),
        user_id: row.user_id.clone(),
        project_id: row.project_id.clone(),
        backend: row.backend.clone(),
        backend_hint: format!(
            "restored vm_index={:?} fp={}",
            row.vm_index, row.key_fp
        ),
        created_at_secs: row.created_at_secs,
        last_used_at_secs: row.last_used_at_secs,
    }
}

/// Walk the sealed-records dir and delete every `*.sealed` file
/// whose path is not in `consumed` — those files were not matched
/// by any pg row owned by this host, so they're orphans from a
/// partially-cancelled create. Returns count unlinked.
fn sweep_orphan_sealed(
    sealed_dir: &Path,
    consumed: &std::collections::HashSet<std::path::PathBuf>,
) -> usize {
    let read = match std::fs::read_dir(sealed_dir) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let mut n = 0;
    for entry in read.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|e| e == "sealed") {
            continue;
        }
        if consumed.contains(&path) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                tracing::info!(
                    path = ?path,
                    "sandbox/restore: orphan sealed record (no pg row for this host); unlinked"
                );
                n += 1;
            }
            Err(e) => {
                tracing::warn!(
                    path = ?path,
                    error = %e,
                    "sandbox/restore: failed to unlink orphan sealed record"
                );
            }
        }
    }
    n
}

#[derive(Debug)]
enum ProbeOutcome {
    Match { actual_fp: String },
    Mismatched { actual_fp: String },
    Unauthorized,
    Unreachable(String),
}

/// Single-shot signed `/version` probe.
async fn probe_version_signed(
    agent_url: &str,
    signing_key: &Arc<SigningKey>,
    timeout: Duration,
) -> ProbeOutcome {
    let url = format!("{agent_url}/version");
    let path = "/version".to_string();
    let signing_key = Arc::clone(signing_key);
    let result: Result<AgentResp, String> = compio::runtime::spawn_blocking(move || {
        let ts = unix_now();
        let nonce = match random_hex(16) {
            Ok(n) => n,
            Err(e) => return Err(format!("rng: {e}")),
        };
        let signature = sig::sign(&signing_key, "GET", &path, &[], ts, &nonce);
        match ureq::get(&url)
            .timeout(timeout)
            .set("x-sbx-timestamp", &ts.to_string())
            .set("x-sbx-nonce", &nonce)
            .set("x-sbx-signature", &signature)
            .call()
        {
            Ok(r) => Ok(AgentResp {
                status: r.status(),
                body: r.into_string().unwrap_or_default(),
            }),
            Err(ureq::Error::Status(code, r)) => Ok(AgentResp {
                status: code,
                body: r.into_string().unwrap_or_default(),
            }),
            Err(e) => Err(e.to_string()),
        }
    })
    .await
    .unwrap_or_else(|p| Err(format!("blocking task panic: {p:?}")));

    match result {
        Ok(resp) if resp.status == 200 => {
            let fp = serde_json::from_str::<serde_json::Value>(&resp.body)
                .ok()
                .and_then(|v| {
                    v.get("pubkey_fingerprint")
                        .and_then(|s| s.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_default();
            ProbeOutcome::Match { actual_fp: fp }
        }
        Ok(resp) if resp.status == 401 => ProbeOutcome::Unauthorized,
        Ok(resp) => ProbeOutcome::Unreachable(format!(
            "/version → status {} body={:?}",
            resp.status,
            resp.body.chars().take(120).collect::<String>()
        )),
        Err(e) => ProbeOutcome::Unreachable(e),
    }
}

async fn probe_and_classify(
    agent_url: &str,
    signing_key: &Arc<SigningKey>,
    expected_fp: &str,
    timeout: Duration,
) -> ProbeOutcome {
    match probe_version_signed(agent_url, signing_key, timeout).await {
        ProbeOutcome::Match { actual_fp } if actual_fp == expected_fp => {
            ProbeOutcome::Match { actual_fp }
        }
        ProbeOutcome::Match { actual_fp } => ProbeOutcome::Mismatched { actual_fp },
        other => other,
    }
}

#[derive(Debug)]
struct AgentResp {
    status: u16,
    body: String,
}

fn unix_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_secs()
}

fn random_hex(bytes: usize) -> Result<String, String> {
    use std::io::Read as _;
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .map_err(|e| format!("open /dev/urandom: {e}"))?
        .read_exact(&mut buf)
        .map_err(|e| format!("read /dev/urandom: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Used by tests to construct a `SandboxAuth` from secret material
/// + pg-supplied agent_url + key_fp, mirroring the path the production
/// boot loop walks.
#[doc(hidden)]
pub fn _test_build_auth_from_sealed(
    sealed: &SealedAuth,
    agent_url: String,
    pubkey_fp: String,
) -> Result<SandboxAuth, String> {
    sealed.into_sandbox_auth_with(agent_url, pubkey_fp)
}

// ─── tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_restored_info_round_trips_pg_fields() {
        let row = SandboxRow {
            sandbox_id: "sbx_abcdefghij1234567890".into(),
            user_id: "usr_aaaaaaaaaaaaaaaaaaaa".into(),
            project_id: "prj_bbbbbbbbbbbbbbbbbbbb".into(),
            backend: "nomad-ch".into(),
            vm_index: Some(7),
            agent_url: Some("http://10.99.107.2:7777".into()),
            host_id: "hst_cccccccccccccccccccc".into(),
            generation: 0,
            status: SandboxStatus::Running,
            key_fp: "0123456789abcdef0123456789abcdef".into(),
            created_at_secs: 1_700_000_000,
            started_at_secs: Some(1_700_000_001),
            stopped_at_secs: None,
            last_used_at_secs: 1_700_000_500,
        };
        let info = build_restored_info(&row);
        assert_eq!(info.user_id, "usr_aaaaaaaaaaaaaaaaaaaa");
        assert_eq!(info.project_id, "prj_bbbbbbbbbbbbbbbbbbbb");
        assert_eq!(info.backend, "nomad-ch");
        assert_eq!(info.created_at_secs, 1_700_000_000);
    }

    // : removed `sandbox_id_lossy_parses_typed_id_suffix`
    // because the helper itself is gone. The new behaviour (skip + bump
    // `sandbox_corrupt_id_total`) is exercised end-to-end in the pg-gated
    // integration tests.

    #[test]
    fn sweep_orphan_sealed_unlinks_only_unconsumed() {
        let dir = std::env::temp_dir().join(format!(
            "zsbx-orphan-{}",
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let id_consumed = uuid::Uuid::now_v7();
        let id_orphan = uuid::Uuid::now_v7();
        let consumed_path = dir.join(seal_filename_for(id_consumed));
        let orphan_path = dir.join(seal_filename_for(id_orphan));
        std::fs::write(&consumed_path, b"x").unwrap();
        std::fs::write(&orphan_path, b"y").unwrap();
        // A file that isn't a .sealed file must not be touched.
        let stray = dir.join("README.txt");
        std::fs::write(&stray, b"keep").unwrap();

        let mut consumed = std::collections::HashSet::new();
        consumed.insert(consumed_path.clone());

        let n = sweep_orphan_sealed(&dir, &consumed);
        assert_eq!(n, 1);
        assert!(consumed_path.exists(), "consumed file must stay");
        assert!(!orphan_path.exists(), "orphan file must be unlinked");
        assert!(stray.exists(), "non-.sealed files must be ignored");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
