# sandbox-snapshot-restore — Deferred Backlog

Auto-managed by the pilot-cron-worker on `feat/sandbox-snapshot-restore`. Each cron fire reads this file, picks 1-2 actionable items, lands a fix per logical commit, and removes the entry in the same commit. Findings whose blocker still stands stay listed with an updated "last considered" line.

Last seeded: 2026-05-22 (post bug-#13 cluster smoke; cluster torn down).
Last updated: 2026-05-23 (post bug-#14 diagnostic cycle — bug #15 found, both #14 demoted).
Branch HEAD at seed: `fce3e208`.
Branch HEAD at last update: `8ad3cf3f`.
Worktree: `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.

---

## CRITICAL (open blockers on Phase B cluster validation)

### [B15] `teardown_source_for_snapshot` wipes `host_dir` → workspace.img gone at wake → wrapper exit 1
- **Source**: cluster smoke 2026-05-23 02:32Z (bug-#14 diagnostic round; see `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-23-r1.md` for the verbatim evidence).
- **Symptom**: every wake fails with `restore_backend: nomad alloc terminal status=failed: Failed tasks`. Wrapper stderr (captured via live-patched tee) shows `[wrapper] FATAL: workspace image missing: /var/zeroship/ch/<sandbox-id>/workspace.img`. Confirmed by directory listing: every host_dir whose snapshot succeeded ALSO had `workspace.img` removed; the one whose snapshot timed out (teardown not reached) still has the image.
- **Root cause**: `crates/sandbox/src/admin_handlers.rs::snapshot_sandbox` post-success path calls `backend.teardown_source_for_snapshot(sandbox_id)`. That delegates to `b.stop(sandbox_id)` (`crates/sandbox/src/backend/mod.rs:412`). `stop()` step 5 (`crates/sandbox/src/backend/nomad_ch.rs:1063-1077`) runs `remove_dir_all(host_dir)`, which is the same dir holding the per-sandbox `workspace.img` created at `create_ext4_image_if_missing(&workspace_img, …)` (`nomad_ch.rs:660-663`). Wrapper's pre-CH defensive gate (`crates/sandbox/scripts/nomad-vm-wrapper.sh:222-225`) trips.
- **Why missed earlier**: pre-virtio-blk pivot, host_dir held only a virtiofsd socket (re-attached on restore via fresh daemon). The pivot moved workspace storage into per-sandbox raw ext4 images under host_dir, but `teardown_source_for_snapshot` kept its "just call stop()" delegation. Unit tests use tempfile-stubs that don't exercise the host_dir-wipe interaction.
- **Relevant code**:
  - `crates/sandbox/src/admin_handlers.rs:1196-1206` (snapshot success → teardown call)
  - `crates/sandbox/src/backend/mod.rs:407-419` (`teardown_source_for_snapshot` → stop)
  - `crates/sandbox/src/backend/nomad_ch.rs:1063-1077` (host_dir `remove_dir_all`)
  - `crates/sandbox/src/backend/nomad_ch.rs:660-663` (workspace.img creation in host_dir)
  - `crates/sandbox/scripts/nomad-vm-wrapper.sh:222-225` (the FATAL gate)
- **Action plan (Option A, recommended)**: introduce a snapshot-aware variant of stop() — call it `stop_preserving_state(&self, sandbox_id) -> Result<(), String>` — that runs steps 1-4 of the existing `stop` (Nomad job purge + host-fence + vm_index release + in-memory map removal) but **skips step 5's `remove_dir_all(host_dir)`**. Wire `teardown_source_for_snapshot` to it. Make the orphan-prune sweep aware that `snapshotted` rows' host_dirs are *expected* to persist; only sweep host_dirs whose pg row is `stopped`/terminal-not-restorable. Symmetric with how `home.img` is intentionally preserved across snapshot lifetimes.
- **Action plan (rejected, Option B)**: re-materialize `workspace.img` in the wrapper or restore_handler. **Silently destroys persisted workspace data on every wake** — defeats the whole point of `/workspace` as durable per-sandbox storage. Don't do this.
- **Tests to add**:
  - `crates/sandbox/src/backend/nomad_ch.rs` unit test: assert `stop_preserving_state` does NOT call `remove_dir_all` on host_dir (mock filesystem).
  - `crates/sandbox/tests/` integration test: cold-boot → write file to /workspace via agent (or just write a sentinel file at workspace.img mount path via a fake) → snapshot → assert host_dir + workspace.img still on disk → restore → assert workspace.img unchanged.
- **Once landed**: re-run the 2026-05-23 1+1 smoke; re-evaluate whether B14a/B14b are still failing in their own right (likely both go away when B15 closes, since the wrapper never got past the workspace.img gate to demonstrate either).

### [B14a] (DEMOTED) snap-stage `memory-ranges` absent at wake time → wrapper exits 1
- **Status**: refuted by 2026-05-23 cycle. Controller `restore: post-store.get staged files` tracing confirms all three files (config.json=2804, memory-ranges=1073741824, state.json=~102K) are staged successfully. The wake fails AFTER staging because of bug-#15, not because the stage is empty.
- **Disposition**: leave listed as a watch-item; re-test once #15 closes. If the wrapper's restore-branch instrumentation never fires in the next smoke, this is fully closed.

### [B14b] (DEMOTED) tap `NO-CARRIER` after CH `--restore` → No route to host
- **Status**: not reproduced in 2026-05-23 cycle. The wrapper never gets past the workspace.img gate, so we never observe CH `--restore` proceeding to net-device resume. The previously-observed NO-CARRIER could be a real second-order bug or could have been a one-off; can't tell from current evidence.
- **Disposition**: same — re-test once #15 closes. The speculative tap-up retry loop (lines 378-384 of nomad-vm-wrapper.sh) is harmless; keep it.

---

## IMPORTANT (Phase B follow-ups; blocked on #14 closing)

### [B-SLO] 5-worker × 20-cycle SLO empirical validation
- **Blocked-by**: B14a + B14b
- **Action**: once smoke wake ≥ 3/4, scale to 3+5 + run cluster stress; capture create / snapshot / wake / post-exec / stop p50/p95/p99/max; compare wake p50 to 4243ms cold-boot baseline (§ 10.2 SLO targets: p50 ≤ 1.0s; p95 ≤ 1.5s; p99 ≤ 2.0s; p99.9 ≤ 6.0s).

---

## IMPORTANT (main-repo sandbox TODO P1 — pick up once Phase B is closed)

### [T1] Read-only `sandbox_admin_ro` role for admin read endpoints
- **Source**: `crates/sandbox/TODO.md` P1 (Round-4 IMPORTANT #7 deferred)
- **Scope**: new migration (5th role + read-only grants); `Database::pool_admin_ro()` helper; per-handler routing decision (`open_app_pool` → `open_admin_ro_pool` for the list/detail/export read paths).
- **Last considered**: 2026-05-22 — clean follow-up; no blocker

### [T2] Rate-limit on heavyweight admin endpoints
- **Source**: `crates/sandbox/TODO.md` P1 (Round-4 IMPORTANT #8)
- **Scope**: counting bucket on `GET /admin/users/{u}/export` (REPEATABLE READ + multi-table aggregate) + `DELETE /admin/users/{u}` (multi-statement cascade). Default 1 req/s sustained, burst 5; bypass for `force_takeover` admin scope.
- **Last considered**: 2026-05-22 — clean follow-up; no blocker

### [T3] `alloc_running_timeout_secs` 60→120 default
- **Source**: May-5 cluster stress (31/60 creates timed out before alloc-running under c=60 single-worker)
- **Scope**: one-line default change in `crates/sandbox/src/config.rs` (and matching tests). Mirrors the host_fence_timeout 30→120 shape from commit `cad098e`.

### [T4] Controller-side stop semaphore
- **Source**: May-5 cluster stress observations
- **Scope**: cap concurrent host_fence polls per worker via `Mutex<HashMap<worker_id, RateBucket>>` on AppState. Default 16 concurrent stops; surfaces metric.

### [T5] Restore handler: signed `/version` fingerprint check
- **Source**: Phase A subagent report (deferred from initial impl)
- **Scope**: v1 polls only unsigned `/livez`; the signed `/version` fingerprint requires plumbing the per-sandbox signing key out of the sealed record. Mitigates a hypothetical stale-tenant race on v2 cluster-wide restore. Defer until cross-worker restore is on the roadmap.

### [T6] `spawn_idle_eviction_sweep` auto-spawn
- **Source**: Phase A subagent report
- **Scope**: function lives in `crates/sandbox/src/sweep.rs` but isn't auto-spawned in `AppState::from_config`. Wire it behind `SANDBOX_IDLE_SNAPSHOT_SECS > 0`. Transient-takeover sweep IS already auto-spawned.

---

## MINOR (defer to broader cleanup)

### [Q1] AEAD DEK derivation `time_of_put_unix_secs` vs pg `snapshot_taken_at`
- **Source**: Phase A subagent report
- **Note**: DEK depends on `snapshot_taken_at` per proposal § 4.3; controller can't pre-stage exact value since pg writes it server-side. v1 stamps `time_of_put_unix_secs` into the AEAD header itself; get-path re-derives. Consider whether this divergence from the proposal text matters.

### [Q2] GCS upload retry-with-backoff on `x-goog-hash` mismatch
- **Source**: Phase A subagent report
- **Note**: proposal § 4 step 3 mandates "retried up to 3× before alerting"; current `GcsSnapshotStore::put` is single-shot. Add retry loop on upload completion mismatch.

### [Q3] `TieredSnapshotStore` L2-back-fill on L1 miss
- **Source**: Phase A subagent report
- **Note**: L1-miss → L2-hit doesn't populate L1 as side effect; just delegates. Documented as TODO in the GCS PR; fix lands when the GCS adapter graduates from stub.

### [Q4] Per-creator workspace.img size override
- **Source**: virtio-blk pivot (2026-05-22)
- **Note**: `SANDBOX_WORKSPACE_IMAGE_SIZE_GB` is controller-wide (default 20). Premium tier might want larger workspaces. Add per-creator override + lifecycle policy hooks.

### [Q5] `crates/sandbox/tests/sandbox_pg_e2e.rs` fixture refresh post-virtio-blk
- **Source**: virtio-blk pivot subagent report
- **Note**: two `#[ignore]`d pg-gated tests still carry an `fs[].socket` entry in their fixture `config.json`. The rewrite invariant ("doesn't touch fs[]") still holds, but the fixture's virtiofs shape is legacy. Refresh to virtio-blk-shaped fixtures.

### [Q6] Doc comments on `NomadCHConfig::keys_dir/workspace_dir/user_home_dir_root`
- **Source**: virtio-blk pivot subagent report
- **Note**: post-pivot, these are image-root paths not dir-mount roots. Doc still says "directory persists across sandbox lifetimes" — functionally accurate but lexically stale. Doc-only pass.

---

## SCOPE GUARDS (read by every cron cycle)

**Allowed**:
- `crates/sandbox/**` (primary target)
- `crates/sandbox-agent/**` (in-VM agent — touch only when a finding genuinely demands it)
- `docs/reviews/sandbox-snapshot-*` (review artifacts, this file)
- `crates/sandbox/TODO.md` (backlog cross-linking)
- `crates/sandbox/migrations/**` (only for new migrations approved by the pilot)

**Forbidden** (drift here is a `git restore` + re-dispatch, not a "let me just fix this too"):
- `crates/gateway/**`, `crates/control/**`, `crates/worker/**`, `crates/runtime/**`, `crates/runtime-macros/**`, etc. (any non-sandbox crate)
- `docs/proposals/sandbox-snapshot-restore.md` (per workflow rule — proposal commits with implementing PR only)
- Any `git push` to remote (user rule for this cron)
- Any change to the `main` branch directly
- Deleting GCS buckets, firewall rules, VPC, or shared infra
- Force-push, --no-verify, --no-gpg-sign

**Real-env testing**:
- Each cluster cycle MUST end with `bash crates/sandbox/scripts/teardown-gcp-cluster.sh` regardless of outcome
- $30 hard cap per cycle (despite the 100K GCP credits — bound burn rate, not budget)
- Honor the "13-bug observation chain" lesson: log every cluster bug verbatim before clearing it
