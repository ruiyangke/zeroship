# sandbox-snapshot-restore — Deferred Backlog

Auto-managed by the pilot-cron-worker on `feat/sandbox-snapshot-restore`. Each cron fire reads this file, picks 1-2 actionable items, lands a fix per logical commit, and removes the entry in the same commit. Findings whose blocker still stands stay listed with an updated "last considered" line.

Last seeded: 2026-05-22 (post bug-#13 cluster smoke; cluster torn down).
Last updated: 2026-05-24 (cluster smoke aborted at controller-start — bug #16 found; T3/T6 closed; 4 new reviewer rounds added).
Branch HEAD at seed: `fce3e208`.
Branch HEAD at last update: `4a7e8e03` (T6 wired idle eviction sweep; T3 bumped alloc_running_timeout 60→120; B15 fix at `eaf5ea83`).
Worktree: `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.

---

## CRITICAL (open blockers on Phase B cluster validation)

### [B16] NixOS-built controller binary unrunnable on GCE Ubuntu workers
- **Source**: cluster smoke 2026-05-24 03:15 UTC (see `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-24-r1.md`)
- **Symptom**: `zsbx-ctl.service` exits 203/EXEC immediately; `/usr/local/bin/zeroship-sandbox: cannot execute: required file not found` (kernel's misleading text for missing PT_INTERP).
- **Root cause**: `readelf -p .interp target/release/zeroship-sandbox` → `/nix/store/jms7zxzm7w1whczwny5m3gkgdjghmi2r-glibc-2.42-51/lib/ld-linux-x86-64.so.2`. The local NixOS-built binary's dynamic linker path is absent on GCE Ubuntu. Even with patchelf, glibc-2.42 ABI > Ubuntu 22.04's glibc-2.35 — version errors at runtime.
- **Action plan (ranked)**:
  - (A) Docker-based cross-build with `rust:bookworm-slim` image → Debian glibc-2.36 + x86_64 ELF interp at `/lib64/ld-linux-x86-64.so.2`. Ubuntu 22.04+ runs Debian-compiled binaries. Requires only `docker run -v $PWD:/work -w /work rust:bookworm-slim cargo build --release -p zeroship-sandbox --bin zeroship-sandbox`.
  - (B) Add a `pkgs.pkgsStatic` or `pkgs.pkgsMusl` entry to `flake.nix` for a musl-static build — clean but bigger flake change.
  - (C) Build inside a remote GCE Ubuntu instance and pull the binary back — last resort.
- **Pre-flight gate**: add `readelf -l target/release/zeroship-sandbox | grep INTERP` to `provision-gcp-cluster.sh` that REFUSES upload if the interp path contains `/nix/`. Caught this would have saved 13min + $0.38 of provisioning.
- **B14a/B14b status**: still demoted, still pending cluster smoke that actually reaches the wake path. Will re-evaluate after #16 closes.

### [B14a] (DEMOTED) snap-stage `memory-ranges` absent at wake time → wrapper exits 1
- **Status**: refuted by 2026-05-23 cycle. Controller `restore: post-store.get staged files` tracing confirms all three files (config.json=2804, memory-ranges=1073741824, state.json=~102K) are staged successfully. The wake fails AFTER staging because of bug-#15, not because the stage is empty.
- **Disposition**: leave listed as a watch-item; re-test once #16 closes. If the wrapper's restore-branch instrumentation never fires in the next smoke, this is fully closed.

### [B14b] (DEMOTED) tap `NO-CARRIER` after CH `--restore` → No route to host
- **Status**: not reproduced in 2026-05-23 cycle. The wrapper never gets past the workspace.img gate, so we never observe CH `--restore` proceeding to net-device resume. The previously-observed NO-CARRIER could be a real second-order bug or could have been a one-off; can't tell from current evidence.
- **Disposition**: same — re-test once #16 closes. The speculative tap-up retry loop (lines 378-384 of nomad-vm-wrapper.sh) is harmless; keep it.

### [A1] AEAD never wraps prod snapshot store (CRITICAL, security-r1)
- **Source**: 2026-05-24 security review (also flagged by arch-r1)
- **File**: `crates/sandbox/src/lib.rs:316-345`
- **Symptom**: production builds bare `LocalDiskSnapshotStore` or `TieredSnapshotStore<LocalDisk, Gcs>`; `AeadSnapshotStore` never composed. `snapshot_handler.rs:358` still stamps `snapshot_aead_dek_id="v1"` into pg, so operators see "encrypted" in the audit trail while guest RAM hits GCS in plaintext.
- **Action**: wrap the inner store in `AeadSnapshotStore` if `SANDBOX_SNAPSHOT_AEAD_ENABLED=1` (or unconditionally for prod). Add a startup log line stating the effective AEAD posture. Add an integration test that asserts get/put roundtrip through AEAD layer.

### [A2] `GcsSnapshotStore::verify()` discards `expected_sha256` (CRITICAL, security-r1)
- **Source**: 2026-05-24 security review
- **File**: `crates/sandbox/src/snapshot_store_gcs.rs:660-682`
- **Symptom**: `let _ = expected_sha256;` — the integrity gate is a no-op. An attacker with bucket-write can substitute snapshots and the get-path will accept them.
- **Action**: implement the SHA-256 compare; surface mismatch as `SnapshotIntegrityError`; add a tampered-blob unit test.

### [A3] 1 GB sync I/O on compio worker (CRITICAL, perf-r1 + concurrency-r1)
- **Source**: 2026-05-24 performance + concurrency reviews
- **Files**:
  - `crates/sandbox/src/snapshot_handler.rs:316-373` — sync 1 GB SHA + AEAD + rename on compio worker (violates trait's own contract in `snapshot_store.rs:95`)
  - `crates/sandbox/src/restore_handler.rs:286-412,1088,1121` — `store.get` does two full 1 GB passes (verify + AEAD decrypt) + `std::thread::sleep` in `wait_for_alloc_running_blocking` / `wait_for_livez_blocking`
  - `crates/sandbox/src/snapshot_handler.rs:335-338` — `ChRemoteClient::pause/snapshot` sync from async handler
- **Symptom**: stalls a single-threaded compio worker for the full SHA-256 + AEAD + GCS roundtrip. Up to 90s blocking on ureq + Command + std::fs.
- **Action**: wrap all `SnapshotStore::put`/`get` calls + `ChRemoteClient` calls in `compio::runtime::spawn_blocking` (pattern already used in `persist.rs:641,656,676`). Replace `std::thread::sleep` with `compio::time::sleep`.

### [A4] HTTP error envelope §10.0 violated globally (CRITICAL, api-surface-r1)
- **Source**: 2026-05-24 api-surface review
- **Files**: `crates/sandbox/src/admin_handlers.rs:179-192` + `handlers.rs:25-44` + `preview.rs:836-838` + `preview_share_handlers.rs:112-114` (only `admin_handlers.rs:1041-1044` matches spec)
- **Spec**: `{"error":"<kind>","message":"<human>",...}` per proposal § 10.0
- **Symptom**: most sites emit `{"error":"<human prose>"}` (no `message`); preview sites emit `{"error":<code>,"code":<code>}` (duplicate, no message).
- **Action**: introduce typed `ErrorEnvelope` helper in `crates/sandbox/src/error_envelope.rs`; replace all `err()` call sites; lock with a unit test per error site.

### [A5] `AppState.admin_token` is `pub` (CRITICAL, api-surface-r1)
- **Source**: 2026-05-24 api-surface review
- **File**: `crates/sandbox/src/lib.rs:98`
- **Symptom**: pub field — `admin_handlers.rs:128-136` documents the footgun (empty `Zeroizing<String>` defeats bearer compare). Field needs `pub(crate)` + constructor.
- **Action**: `pub(crate)`-restrict + add `AppState::with_admin_token(...)` constructor that rejects empty tokens at compile-time-of-call.

### [C1] Lease-takeover sweep is dead code (CRITICAL, concurrency-r1 + arch-r1)
- **Source**: 2026-05-24 concurrency review (corroborates 2026-05-23 architecture-r1)
- **Files**: `crates/sandbox/src/db.rs:2305` (`update_lessee` — zero callers); `crates/sandbox/src/db.rs:1712-1724` (`update_sandbox_status` never sets `lessee_updated_at`); `crates/sandbox/src/db.rs:2361` (sweep query filter `WHERE lessee_updated_at IS NOT NULL` excludes every real transient row)
- **Symptom**: §6.1 crash recovery never fires. Under a controller crash mid-Snapshotting/Restoring, the sandbox row is stuck in transient state forever.
- **Action**: either (a) wire `update_lessee` into every state transition that crosses transient boundaries OR (b) remove the dead code + redesign §6.1 around `updated_at` timestamps with a separate `transient_since` column.

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

### [T4] Controller-side stop semaphore
- **Source**: May-5 cluster stress observations
- **Scope**: cap concurrent host_fence polls per worker via `Mutex<HashMap<worker_id, RateBucket>>` on AppState. Default 16 concurrent stops; surfaces metric.

### [T5] Restore handler: signed `/version` fingerprint check
- **Source**: Phase A subagent report (deferred from initial impl)
- **Scope**: v1 polls only unsigned `/livez`; the signed `/version` fingerprint requires plumbing the per-sandbox signing key out of the sealed record. Mitigates a hypothetical stale-tenant race on v2 cluster-wide restore. Defer until cross-worker restore is on the roadmap.

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
