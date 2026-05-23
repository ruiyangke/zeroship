# sandbox-snapshot-restore — Deferred Backlog

Auto-managed by the pilot-cron-worker on `feat/sandbox-snapshot-restore`. Each cron fire reads this file, picks 1-2 actionable items, lands a fix per logical commit, and removes the entry in the same commit. Findings whose blocker still stands stay listed with an updated "last considered" line.

Last seeded: 2026-05-22 (post bug-#13 cluster smoke; cluster torn down).
Branch HEAD at seed: `fce3e208`.
Worktree: `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.

---

## CRITICAL (open blockers on Phase B cluster validation)

### [B14a] snap-stage `memory-ranges` absent at wake time → wrapper exits 1
- **Source**: cluster smoke 2026-05-22 23:02Z (post-bug-#13 rebake)
- **Symptom**: alloc `Started → Terminated msg="Exit Code: 1"` within ~2-80 ms. Wrapper's `set -Eeuo pipefail` kills it before CH starts. Either the `[ ! -f memory-ranges ]` precondition fires (snap-stage dir reported empty post-event) OR the `sed -i config.json` path rewrite fails.
- **Suspected layer**: controller-vs-wrapper path skew — `RestoreBackend::restore_alloc_dir` returns one path; `SnapshotStore::get` populates another; or a stage→reset cycle wipes the dir between submit and wrapper exec.
- **Relevant code**:
  - `crates/sandbox/src/restore_handler.rs::restore_sandbox` step 4 (`store.get(&sandbox_id_typed, &alloc_dir, &snap.sha256)`)
  - `crates/sandbox/src/restore_handler.rs::RealRestoreBackend::restore_alloc_dir`
  - `crates/sandbox/scripts/nomad-vm-wrapper.sh` restore branch (`ZSBX_RESTORE_FROM` validation around line 294)
- **Action plan**: (1) add `tracing::info!(staged_dir=?alloc_dir, …)` after the `store.get` call in `restore_sandbox`; (2) have the wrapper log `ls -la $ZSBX_RESTORE_FROM` to stderr before the precondition check; (3) cluster smoke with diff between paths captured. Likely fix: store.get destination path needs to match wrapper-read path exactly.

### [B14b] tap `NO-CARRIER` after CH `--restore` → No route to host (agent unreachable)
- **Source**: cluster smoke 2026-05-22 23:02Z (the one snapshot that did succeed and the post-smoke ad-hoc retries)
- **Symptom**: CH restore succeeds (no Mode A), guest boots, but the tap shows state DOWN / NO-CARRIER for the whole restore lifecycle; livez timeout fires at 30s; alloc terminated by controller SIGINT (exit 130).
- **Suspected layer**: CH `--restore` may detach + re-attach the tap as part of resume; wrapper's `ip link set up` (line 142, pre-CH-spawn) gets undone by CH's tap-reattach; or CH's tap-attach silently fails when restoring from snapshot's net device state.
- **Relevant code**:
  - `crates/sandbox/scripts/nomad-vm-wrapper.sh` tap-up block (lines 142-150) and the restore branch (line 290+)
  - `crates/sandbox/src/restore_handler.rs::rewrite_config_json` (net.tap/mac rewrite already verified by bug-#8 test)
- **Action plan**: (1) capture `ip -br link show $TAP` at three points — before CH spawn, 1s after CH spawn, 10s after spawn — log to wrapper stderr; (2) tail CH's resume log for net-device messages (`grep -i 'net\|tap\|virtio_net' ch.log`); (3) likely fix: add a post-CH-spawn `ip link set $TAP up` polling loop in the wrapper (the cgroup may force a re-up). Alternatively: switch CH net mode away from tap-by-name to file-descriptor passing, but that's wrapper-invasive.

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
