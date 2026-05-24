# sandbox-snapshot-restore — Deferred Backlog

Auto-managed by the pilot-cron-worker on `feat/sandbox-snapshot-restore`. Each cron fire reads this file, picks 1-2 actionable items, lands a fix per logical commit, and removes the entry in the same commit. Findings whose blocker still stands stay listed with an updated "last considered" line.

Last seeded: 2026-05-22 (post bug-#13 cluster smoke; cluster torn down).
Last updated: 2026-05-25 r1 (post critical-fix-sweep cluster smoke at v18 + rootfs v5: Phase 1 c=4 HARD FAIL 0/16 creates, NEW bug #24 — controller `Tasks[].Env` block in `nomad_ch.rs:2224-2264` missing `ZSBX_SANDBOX_ID` so the wrapper's R8-DEPLOY1 guard fires at line 153 on every cold boot, killing alloc in ~50ms with empty ch.stderr. Phase 2 c=20 NOT REACHED. Cluster fully torn down. B19 + B-SLO REMAIN UNVERIFIED at cluster on this branch HEAD. Detail: `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-r1.md`).
Prior update: 2026-05-23 (cycle r7+B22-fixer: bug #22 CLOSED — root cause was CH `--restore` preserving `CLOCK_REALTIME` from snapshot-time; fix is signed `/_clock_resync` handshake on agent + controller-side call in `do_restore_inner` between `wait_for_livez` and `register_restored`. v17 controller + v4 rootfs pushed; cluster c=4 confirms POST-WAKE EXEC 7/7 = 100% (was 0/9). B19 also FULLY CLOSED. B-SLO escalation blocked on NEW bug #23 — provision script fails on SERVER_COUNT>1).
Branch HEAD at seed: `fce3e208`.
Branch HEAD at last update: B20 fixer cycle on `3e8bfad5` parent (B20 + Appendix D commit forthcoming). Prior commits: `4fd92bef` (S4); `0aa93a0f` (A3-partial); `15b4f9a8` (B19 in-code); `4e6c70c1` (R4-T1); `28f60d73` (R3-Q3); `2928d5ae` (A4 closed); `b4ddb98b` (B18). Lib tests at parent HEAD: **275 passed**.
Worktree: `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.

---

## CRITICAL (open blockers on Phase B cluster validation)

### [#24] (OPEN 2026-05-25 r1) Controller's Nomad task `Env` block missing `ZSBX_SANDBOX_ID` — every cold-boot alloc dies at wrapper line 153 (CRITICAL)
- **Symptom**: Phase 1 c=4 cluster smoke at v18 + rootfs v5 hard-failed 0/16 creates. Controller logs report `nomad alloc terminal status=failed: Failed tasks` for every cycle; Nomad allocs exit code 1 at ~50ms with empty `ch.stderr.0` (bash `:?` parameter expansion writes to a fifo Nomad logmon hasn't yet attached → bytes dropped).
- **Root cause**: The wrapper at `crates/sandbox/scripts/nomad-vm-wrapper.sh:153` validates `: "${ZSBX_SANDBOX_ID:?missing ZSBX_SANDBOX_ID (R8-DEPLOY1)}"` on every cold boot (skipped only when `ZSBX_RESTORE_FROM` is set). The controller's jobspec env block in `crates/sandbox/src/backend/nomad_ch.rs:2224-2264` enumerates `ZSBX_VM_INDEX/ZSBX_ARTIFACT_DIR/ZSBX_RUNTIME/ZSBX_WORKSPACE_IMG/ZSBX_USER_HOME_IMG/ZSBX_PUBKEY_HEX/ZSBX_VM_MEMORY_MB/ZSBX_VM_CPUS_BOOT/ZSBX_SUBNET_BASE_OCTET` — but never injects `ZSBX_SANDBOX_ID`. The R8-DEPLOY1 fix landed on the wrapper + the doc-comment block, but the actual `task["Env"]["ZSBX_SANDBOX_ID"] = ...` insert never made it into the controller. `grep -rnE "ZSBX_SANDBOX_ID|SANDBOX_AGENT_SANDBOX_ID|R8-DEPLOY1" crates/sandbox/src/` returns zero matches.
- **Reproduction**: `sudo -E -u nobody env ZSBX_VM_INDEX=99 ZSBX_ARTIFACT_DIR=... ZSBX_RUNTIME=... ZSBX_WORKSPACE_IMG=... ZSBX_USER_HOME_IMG=... ZSBX_PUBKEY_HEX=<64hex> ZSBX_VM_MEMORY_MB=512 ZSBX_VM_CPUS_BOOT=1 ZSBX_SUBNET_BASE_OCTET=99 /etc/zeroship/nomad-vm-wrapper.sh` → `line 153: ZSBX_SANDBOX_ID: missing ZSBX_SANDBOX_ID (R8-DEPLOY1)`. Adding `ZSBX_SANDBOX_ID=...` to the env progresses past validation to a different (expected) gate.
- **Proposed fix** (NOT applied — captured per task constraints): one line in `crates/sandbox/src/backend/nomad_ch.rs:2263` env block: `"ZSBX_SANDBOX_ID": id.to_string(),` (or equivalent — the sandbox id is in scope at the jobspec construction site). Add a sibling assertion to the test at `nomad_ch.rs:3667-3680`.
- **Cluster evidence**: `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-r1.md` § "Root cause analysis — new bug #24".
- **Blocks**: B19 cluster re-verification, B-SLO measurement, full Phase B closure.

### [B16] (RESOLVED 2026-05-24 r2) NixOS-built controller binary unrunnable on GCE Ubuntu workers
- **Status**: **CLOSED**. Verified fix via Docker cross-build (Option A from the action plan) in `rust:slim-bookworm` (note: brief said `rust:bookworm-slim`; correct Docker Hub tag is `slim-bookworm`). v13 binary uploaded to `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v13` with portable interp `/lib64/ld-linux-x86-64.so.2`. Controller starts and serves `/livez` on GCE Ubuntu. See cluster review Appendix A for full transcript.
- **Original symptom**: `zsbx-ctl.service` exits 203/EXEC immediately; `/usr/local/bin/zeroship-sandbox: cannot execute: required file not found` (kernel's misleading text for missing PT_INTERP).
- **Original root cause**: `readelf -p .interp target/release/zeroship-sandbox` → `/nix/store/jms7zxzm7w1whczwny5m3gkgdjghmi2r-glibc-2.42-51/lib/ld-linux-x86-64.so.2`. The local NixOS-built binary's dynamic linker path is absent on GCE Ubuntu.
- **Pre-flight gate (still recommended)**: add `readelf -l target/release/zeroship-sandbox | grep INTERP` to `provision-gcp-cluster.sh` that REFUSES upload if the interp path contains `/nix/`. Cheap insurance for future cycles.
- **B14a/B14b status**: **CLOSED** (see below). Wake path now exercised end-to-end after B17 fix.

### [B17] (CLOSED 2026-05-24 r4) Restored VM agent unreachable post-CH-restore — `No route to host`
- **Status**: **CLOSED**. Root cause: CH `--restore` brings the VM back in a paused state; without `ch-remote resume` the vCPUs never run and the guest's virtio-net never replies to ARP, surfacing as EHOSTUNREACH. Fix: poll the CH API socket post-spawn with `ch-remote ping`, then issue `ch-remote resume`. Cluster smoke c=1 PASS (wake 200 OK in 9.5s); c=4 PASS for all 5 cycles that reached snapshot (5/5 wake). The 11/16 c=4 create failures are a separate bug (#18 below), not a wake regression.
- **Hypothesis history (refuted)**: tap/bridge attachment — there is no bridge in this architecture; the model is /30-per-tap with host as gateway. The wrapper's pre-branch tap-up sequence is identical for cold-boot and restore; the missing step was vCPU resume, not network plumbing.
- **Evidence**: wrapper stderr captured `tap … <NO-CARRIER>` before resume, `tap … <LOWER_UP>` after resume — exactly matches "vCPUs were paused, the virtio-net device was attached but no traffic was flowing".
- **Files**: `crates/sandbox/scripts/nomad-vm-wrapper.sh` (lines 366-419, restore branch).

### [B14a] (CLOSED 2026-05-24 r4) snap-stage `memory-ranges` absent at wake time
- **Status**: **CLOSED**. Cluster smoke shows wrapper restore-branch executes the full file-staging path (`ls -la $ZSBX_RESTORE_FROM` reports memory-ranges=1073741824, config.json=3813, state.json=102535) and proceeds to CH `--restore` successfully. No `FATAL: ZSBX_RESTORE_FROM=… missing` events observed across c=4.

### [B14b] (CLOSED 2026-05-24 r4) tap `NO-CARRIER` after CH `--restore`
- **Status**: **CLOSED**. The NO-CARRIER was a real second-order observation of the B17 root cause: CH `--restore` attaches the tap but the VM is paused, so no LOWER_UP. Post-resume the tap transitions to `<BROADCAST,MULTICAST,UP,LOWER_UP>` as expected. The speculative tap-up retry loop at lines 414-418 stays as belt-and-braces.

### [B18] (CLOSED 2026-05-23 B18-fixer cycle) Stale controller pubkey on VM slot reuse — create-side 401
- **Status**: **CLOSED**. Root cause was NOT in-VM stickiness; the init.sh path was already correct. Actual bug: two separate `vm_index` allocators (NomadCHBackend's `vm_index_allocator` vs RealRestoreBackend's private `VmIndexReservations`) — the wake path reserved slots into a private map invisible to the create-side allocator, so a subsequent create handed the same tap/IP to a fresh sandbox that collided with the live restored VM on that slot. The 401 surfaced because `/version` was answered by the **old** (restored) agent verifying a different signing-pubkey. Fix: share the `Arc<Mutex<VmIndexAllocator>>` between both backends. Two regression tests added. Cluster c=4 verification: 11/16 stale-pubkey 401s pre-fix → **0/16 post-fix**.
- **Cluster evidence**: `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-24-r2.md` Appendix B.

### [B19] (CLOSED 2026-05-23 r7 B22-fixer) Wake path doesn't register restored VM in backend state map (CRITICAL)
- **Status**: **FULLY CLOSED**. With bug #22 fixed in this cycle, the wake path runs end-to-end. Cluster c=4 confirms wake 7/7 → exec_post 7/7 (was 0/9 in Appendix E pre-#22) → stop 7/7 (slot released). The B19 trait-dispatch fires on every successful wake; the post-wake state-map insert + signing-key install both work as designed. The "sandbox not found in nomad-ch backend" surface is gone.
- **Cluster evidence**: `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-24-r2.md` Appendix F (B22 fix + B19 full closure).
- **Files changed (cumulative across B19 + B21 + B22)**:
  - `crates/sandbox/src/backend/{nomad_ch.rs,mod.rs}` — `NomadCh(Arc<…>)` wrap + `register_restored` on both NomadCHBackend and Backend.
  - `crates/sandbox/src/restore_handler.rs` — trait + impl + `with_nomad_handle` + `do_restore_inner` post-livez chain (unseal + resync + register_restored).
  - `crates/sandbox/src/persist.rs` — `Persistence::unseal(sandbox_id)`.
  - `crates/sandbox/src/{admin_handlers.rs,lib.rs}` — wiring at `from_config` + `wake_sandbox`.
  - `crates/sandbox-agent/src/{sig.rs,handlers.rs,main.rs,metrics.rs,version.rs}` — `/_clock_resync` + skew-bypass verifier surface (bug #22).

### [B20] (CLOSED 2026-05-23 r5 B20-fixer) Cold-boot /livez never 200 — root cause: `gcp-worker-startup.sh` pulled pre-virtio-blk rootfs
- **Status**: **CLOSED**. Root cause: `crates/sandbox/scripts/gcp-worker-startup.sh:143` hard-coded `gs_pull rootfs-slim.img.fp32` (2026-05-06 pre-virtio-blk artifact), but the wrapper's cold-boot `--disk` block passes virtio-blk paths and the new init.sh expects `/dev/vdb`/`/dev/vdc`. The fp32 rootfs's in-VM init.sh can't mount the virtio-blk disks (or has the bug-#12/#13-era broken pubkey decoder), so `sandbox-agent` never binds `:7777` and the tap stays `<NO-CARRIER>`. B18-fixer's c=4 v14 PASS depended on a local stash (`stash@{0}` swaps the line to `rootfs-slim.img.virtio-blk-v3`) that never landed; B19-fixer's fresh worktree reverted to the committed line, producing the 0/16 failure shape. Fix: bulk-bump to `virtio-blk-v3` + comment updates. Cluster c=4 post-fix: 11/16 cold-boot creates PASS (was 0/16); 9/9 wakes PASS; remaining 5 create failures are downstream of bug #21's slot leak. No controller / wrapper / rootfs rebuild needed.
- **Cluster evidence**: `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-24-r2.md` Appendix D.

### [B21] (CLOSED 2026-05-23 r6 B21-fixer) Controller systemd unit missing `SANDBOX_PERSIST_AUTH=1` — B19 wake-side `register_restored` silently no-ops
- **Status**: **CLOSED**. Fix in `crates/sandbox/scripts/gcp-worker-startup.sh`: provisions a 32-byte mode-0o400 AEAD key at `/etc/zeroship/sandbox-aead-key` (idempotent), then exports the triplet `SANDBOX_PERSIST_AUTH=1` + `SANDBOX_AEAD_KEY_PATH=...` + `SANDBOX_PERSIST_DIR=/var/lib/zeroship/sandbox` in the controller systemd unit. Paired with R5-S1's boot-time fail-CLOSED assertion in `AppState::from_config` (now refuses to start when `snapshot_enabled=true && persist=None`). v16 controller binary uploaded to `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v16`. Cluster c=4 smoke confirms `register_restored skipped — persist=None` log line is gone on all 9 wakes; the wake path executes the unseal + register chain end-to-end. Full B19 cluster closure (exec_post 200) is now gated on NEW bug #22 (post-wake agent `/exec` 401), not on persist=None. Lib tests 275 → 280 (+5 for the new assertion truth-table).
- **Cluster evidence**: `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-24-r2.md` Appendix E.

### [#22] (CLOSED 2026-05-23 r7 B22-fixer) Post-wake agent `/exec` returns 401 unauthorized on every wake
- **Status**: **CLOSED**. ROOT CAUSE: CH `--restore` preserves the guest's `CLOCK_REALTIME` from snapshot time. The agent's `unix_now()` lags the controller's by the snapshot→wake gap, blowing through the strict 5-second skew window in `Verifier::verify_kind`. NOT a stale signing key — the sealed bytes match the agent's pubkey perfectly; the failure is purely the wall-clock gate.
- **Fix shape** (16 new tests; agent + controller):
  - **Agent** `crates/sandbox-agent/src/sig.rs`: new `Verifier::verify_kind_skew_bypass(...)` — identical to `verify_kind` but skips the skew check. Refactor extracts a private `verify_kind_inner(..., skip_skew_check: bool)` so the strict path stays default. +6 tests.
  - **Agent** `crates/sandbox-agent/src/handlers.rs`: new `POST /_clock_resync` handler. Verifies signature with skew-bypass, parses body `{"ts":<unix_secs>}`, calls `libc::settimeofday(2)` to set `CLOCK_REALTIME` to the signed ts. +7 tests.
  - **Agent** `crates/sandbox-agent/src/{main.rs,metrics.rs,version.rs}`: register route, add `sbx_agent_clock_resyncs_total` counter, advertise `clock.resync-v1` capability.
  - **Controller** `crates/sandbox/src/restore_handler.rs`: new `clock_resync_post_restore(...)` free fn. `do_restore_inner` calls it between `wait_for_livez` Ok and `register_restored`. `RestoreBackend` trait gains `derive_agent_url(vm_index)`. +3 tests.
- **Security**: skew-bypass is signature-bound; an in-VM attacker cannot forge. Nonce LRU prevents replay. Only the `/_clock_resync` path uses the bypass; every other endpoint stays on strict 5-second skew.
- **Deploy**: v17 controller + v4 rootfs both pushed to GCS; `gcp-worker-startup.sh` bumped `virtio-blk-v3` → `virtio-blk-v4`.
- **Cluster evidence**: c=4 smoke POST-WAKE EXEC went 0/9 → 7/7 (100%). Wake p50 9235 ms (vs 9729 ms pre-fix — resync overhead is negligible). See `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-24-r2.md` Appendix F.
- **Lib tests**: sandbox-agent 207 → 214; sandbox 280 → 283.

### [#23] (CLOSED at `0be352a2`) `provision-gcp-cluster.sh` SERVER_COUNT>1 fixed via --metadata-from-file
- **Source**: B22-fixer cycle attempted to escalate to 3+5 cluster for B-SLO c=20 stress; provision failed.
- **Symptom**: `ERROR: (gcloud.compute.instances.create) argument --metadata: Bad syntax for dict arg: [10.178.0.11]`. The server-IPs list is passed to the next-server's metadata without proper escaping/joining; gcloud parses the bracket-formatted Python repr as a dict key.
- **Action shape**: fix the script's metadata-flag concatenation. Likely `--metadata server-ips=10.178.0.10,10.178.0.11,10.178.0.12` should use `--metadata-from-file` or a properly-escaped CSV; investigate `provision-gcp-cluster.sh` around the server-creation loop.
- **Captured, not fixed** per brief constraint (NEW bug → capture verbatim).
- **Blocked**: B-SLO empirical validation at c=20 scale (deferred until #23 is fixed).

### [A1] (CLOSED at `18e2034b`) AeadSnapshotStore now wraps prod store when SANDBOX_SNAPSHOT_ROOT_KEK_PATH provided. Follow-up: A1-FOLLOWUP (boot assertion vs warn when key missing in tiered+GCS mode — per arch-r9 fail-CLOSED gap)
- **Source**: 2026-05-24 security review (also flagged by arch-r1)
- **File**: `crates/sandbox/src/lib.rs:316-345`
- **Symptom**: production builds bare `LocalDiskSnapshotStore` or `TieredSnapshotStore<LocalDisk, Gcs>`; `AeadSnapshotStore` never composed. `snapshot_handler.rs:358` still stamps `snapshot_aead_dek_id="v1"` into pg, so operators see "encrypted" in the audit trail while guest RAM hits GCS in plaintext.
- **Action**: wrap the inner store in `AeadSnapshotStore` if `SANDBOX_SNAPSHOT_AEAD_ENABLED=1` (or unconditionally for prod). Add a startup log line stating the effective AEAD posture. Add an integration test that asserts get/put roundtrip through AEAD layer.

### [A3] 1 GB sync I/O on compio worker (CRITICAL, perf-r1 + concurrency-r1)
- **Source**: 2026-05-24 performance + concurrency reviews
- **Files**:
  - `crates/sandbox/src/snapshot_handler.rs:316-373` — sync 1 GB SHA + AEAD + rename on compio worker (violates trait's own contract in `snapshot_store.rs:95`)
  - `crates/sandbox/src/restore_handler.rs:286-412,1088,1121` — `store.get` does two full 1 GB passes (verify + AEAD decrypt) + `std::thread::sleep` in `wait_for_alloc_running_blocking` / `wait_for_livez_blocking`
  - `crates/sandbox/src/snapshot_handler.rs:335-338` — `ChRemoteClient::pause/snapshot` sync from async handler
- **Symptom**: stalls a single-threaded compio worker for the full SHA-256 + AEAD + GCS roundtrip. Up to 90s blocking on ureq + Command + std::fs.
- **Action**: wrap all `SnapshotStore::put`/`get` calls + `ChRemoteClient` calls in `compio::runtime::spawn_blocking` (pattern already used in `persist.rs:641,656,676`). Replace `std::thread::sleep` with `compio::time::sleep`.

### [A4] (CLOSED 2026-05-23 cycle r4) HTTP error envelope §10.0 — fully migrated
- **Status**: **CLOSED**. A4 fixer reconvened late and landed 4 more commits after the pilot's r4 artifacts commit: `64db0d30` admin_handlers, `c0296c76` preview, `5330acd9` preview-share, `2928d5ae` wire-shape tests. Adherence: **8/26 (31%) → 26/26 (100%)**. All wire error responses now funnel through `crates/sandbox/src/error_envelope.rs::ErrorEnvelope` / `error_response()`. 21 new tests pin the shape at HEAD `2928d5ae`. Test count: 238 → 266 (+28).

### [A7] (CLOSED 2026-05-23) `SandboxConfig.token` is `pub` — last credential field exposure
- **Source**: 2026-05-24 A6b-fixer report (`2380605e`)
- **File**: `crates/sandbox/src/config.rs:19` — `pub token: ApiToken` on `SandboxConfig`
- **Symptom**: with A6b closing the 5 `AppState` fields, the last creator-side credential field that still allows external clobber lives on the embedded `SandboxConfig`. `ApiToken` is the bearer for creator-facing endpoints; swap = bypass-auth.
- **Action**: same shape as A5/A6/A6b — `pub(crate)`-restrict the field + add `with_token(...)` builder on `SandboxConfig`. Add a setter test mirroring `admin_token_setter_tests`. Migrate any out-of-crate write-sites (likely test-only).

### [C3] `do_restore_inner` is cancel-unsafe — wedge on future-drop (CRITICAL, concurrency-r2)
- **Source**: 2026-05-24 concurrency-r2
- **File**: `crates/sandbox/src/restore_handler.rs:286-412`
- **Symptom**: if the surrounding handler future drops between `submit_restore_job` success and final `update_sandbox_status(Running)` await, the sandbox row wedges in `Restoring` state with `vm_index` leaked. The only recovery path is the transient-takeover sweep — **which is dead code per C1** ([C1] in this file). Two open critical issues compound: cancel-unsafe restore + no recovery sweep = permanent wedge.
- **Action**: wrap the post-`submit_restore_job` section in a `pin_project` / scope guard that, on drop without success, marks the row `RestoringAborted` (or rolls back to `Snapshotted`) before yielding. Compio's cancellation semantics + a defer/scope-guard pattern from `crates/sandbox/src/admin_handlers.rs::with_lease` (if it exists; otherwise introduce).

### [A2b] (CLOSED at `b925ad0d`) `verify_metadata_only` fast-path lands; trait default delegates to deep `verify` for back-compat; sweep callers documented to use metadata-only
- **Source**: 2026-05-24 performance-r2 (a side-effect of the A2 fix at `f32507ce`)
- **File**: `crates/sandbox/src/snapshot_store_gcs.rs:586-611,920-930`
- **Symptom**: A2 fix made `verify` re-stream the whole artifact and recompute SHA. Fine while L1 is warm. But `TieredSnapshotStore::verify` falls through to L2 the moment L1 is evicted, exposing operators to a sustained 1 GB GCS egress + a SHA-bound core whenever a periodic verify sweep lands on cold rows.
- **Action**: add a `verify_metadata_only(&self, expected_sha256: &Hash)` fast-path that compares against the `x-goog-meta-sha256` header we set on put. The full re-stream stays as the "deep verify" mode. Periodic sweeps use fast-path; integrity audits (manual) use deep.

### [W1] (CLOSED at `f0ebf783`) sed → python3 heredoc with anchored prefix + known-key restriction; metacharacters become JSON string bytes, not regex input
- **Source**: 2026-05-24 security-r2 (also flagged in r1)
- **File**: `crates/sandbox/scripts/nomad-vm-wrapper.sh:359`
- **Symptom**: `sed -i "s|...|...|" config.json` where the substitution pattern includes user-influenceable fields. Unanchored, no escaping of `&`/`/`/`\`. With A1 still open (snapshot plaintext on GCS), an attacker with bucket-write could substitute a config.json whose sed-target field contains sed metacharacters, achieving code execution as raw_exec root. (Currently dormant because A1 is locally-mitigated by the worker-local L1 — but A1 will close to AEAD prod-wrap, which doesn't fix the wrapper sink.)
- **Action**: replace `sed` with a Python or `jq`-based rewrite that validates each field is a JSON string (not metacharacter-bearing). If `jq` isn't in the rootfs (likely), use a small inline Python invocation (`/usr/bin/python3 -c '...'`). Or: switch to a Rust pre-stage step in the controller (the controller already does some path rewriting in `restore_handler::rewrite_config_json`).

### [C1] (CLOSED at `de3523c4`) `update_sandbox_status_with_host` now sets `lessee_updated_at = now()` on transient entry, NULL on exit; 2 pg-gated regression tests added
- **Source**: 2026-05-24 concurrency review (corroborates 2026-05-23 architecture-r1)
- **Files**: `crates/sandbox/src/db.rs:2305` (`update_lessee` — zero callers); `crates/sandbox/src/db.rs:1712-1724` (`update_sandbox_status` never sets `lessee_updated_at`); `crates/sandbox/src/db.rs:2361` (sweep query filter `WHERE lessee_updated_at IS NOT NULL` excludes every real transient row)
- **Symptom**: §6.1 crash recovery never fires. Under a controller crash mid-Snapshotting/Restoring, the sandbox row is stuck in transient state forever.
- **Action**: either (a) wire `update_lessee` into every state transition that crosses transient boundaries OR (b) remove the dead code + redesign §6.1 around `updated_at` timestamps with a separate `transient_since` column.

---

## IMPORTANT (round-r3 reviewers — structural decay + T6/T7 regressions)

### [R3-A1] Backend enum masquerades as trait — 4 methods return Err for 2/3 variants (IMPORTANT, arch-r3)
- **File**: `crates/sandbox/src/backend/mod.rs:166-497`
- **Symptom**: `Backend` enum has 19 methods; four (`lookup_source_vm_ops`, `teardown_source_for_snapshot`, `restore_from_sealed`, `restore_from_pg_and_sealed`) return `Err("backend X doesn't support …")` for two of three variants. Compile-time-checkable design hole.
- **Action**: split `SnapshotCapableBackend` trait. Only `NomadCHBackend` implements it. Use `&dyn SnapshotCapableBackend` at call sites that need the surface.

### [R3-A2] `restore_handler` is silently a second NomadCHBackend implementation (IMPORTANT, arch-r3)
- **File**: `crates/sandbox/src/restore_handler.rs` (full)
- **Symptom**: restore lifecycle logic lives outside `nomad_ch.rs` despite being nomad-ch-specific. Two seams to keep in sync.
- **Action**: move the body of `restore_handler::do_restore_inner` into `NomadCHBackend::do_restore_inner` (or a sibling private fn in `nomad_ch.rs`). The HTTP handler in `restore_handler.rs` becomes a thin shim.

### [R3-A3] Wrapper bash should move to Rust sidecar (IMPORTANT, arch-r3; closes W1 structurally)
- **File**: `crates/sandbox/scripts/nomad-vm-wrapper.sh` (421 LOC bash)
- **Symptom**: bash does JSON surgery (`sed -i`), cmdline building, and now CH-API polling. Each new feature adds bash. Eventually rewrites in Rust.
- **Action**: extract a small Rust binary `zsbx-vm-wrapper` (or fold into `zeroship-sandbox` as a subcommand) that handles the per-VM ops. Bash shrinks to `exec zsbx-vm-wrapper "$@"`. Bonus: closes W1 (sed code-exec sink) structurally.

### [R3-A4] `StopDisposition` enum would prevent C2-class bugs (IMPORTANT, arch-r3)
- **Files**: `crates/sandbox/src/backend/nomad_ch.rs` (`stop_inner(_, remove_host_dir: bool)`)
- **Symptom**: bool flags accumulate (B15 added `remove_host_dir`, C2 reused it; future may add `cleanup_metrics`, `cancel_alloc`, etc.). Bool-soup signature.
- **Action**: introduce `enum StopDisposition { TearDown, PreserveForSnapshotWake }` (or similar). Methods take the enum; the boolean knobs become exhaustive match arms.

### [R3-Q2] (CLOSED at `ac6a6bf2`) `with_persistence` returns `Self`; only A6's was infallible
- **File**: `crates/sandbox/src/lib.rs:272` (`with_persistence -> Result<Self, String>`)
- **Symptom**: `Result<Self, String>` that cannot fail forces in-crate callers to `.expect()` on an infallible operation. Signature smell.
- **Action**: change to `pub fn with_persistence(self, p: Arc<Persistence>) -> Self`. Future invariants can switch to `Result` when they actually need it. Same shape applies to A6b's 5 new builders — review them.

### [R3-Q3] (CLOSED 2026-05-23 at `28f60d73`) Stale `alloc_running_timeout_secs: 60` in test fixtures
- **Files**: 11 test fixtures across `crates/sandbox/src/config.rs:744`, `backend/docker.rs:832`, `backend/nomad_ch.rs:3449`, 4 e2e tests; plus A6 newly copy-pasted `60` at `lib.rs:1267,1399`
- **Symptom**: T3 bumped production default 60→120 but synthetic test fixtures stayed at 60. Not a bug today (fixtures don't exercise the default-loading path) but a noise source for grep-based refactors.
- **Action**: bulk-update fixture literals 60→120. One-line per fixture. No test logic change.

### [R4-A1] AppState builder pattern is becoming a typed-state builder by accretion (CRITICAL, arch-r4)
- **Files**: `crates/sandbox/src/lib.rs:50-407` — 8 `with_*` builders across 3 different return-type shapes (`Self`, `Result<Self, String>`, mixed) + a 14-field `new_fixture()` shadow constructor on both `AppState` AND `SandboxConfig`
- **Symptom**: A5/A6/A6b/A7 trail evidences a typed-state builder being built by accretion rather than design. `new_fixture()` now spans 2 production types as `pub` API — test scaffolding has leaked into production surface.
- **Action**: refactor to a single `AppStateBuilder` typed-state pattern. `new_fixture()` becomes `#[cfg(test)]` or feature-gated behind `pub(crate)`. R3-Q2 (infallible Result) auto-closes by consistent shape.

### [R4-A2] State HashMap eviction + vm_index_allocator release straddle 60-120s of async fence work (CRITICAL, arch-r4)
- **Files**: `crates/sandbox/src/backend/nomad_ch.rs:944-952` (state HashMap removal) + `:1087-1091` (vm_index_allocator release)
- **Symptom**: two separate locked structures span 60-120s of async fence work with no unified RAII guard. This is the architectural coupling underneath B18 — even though B18's surface fix (shared allocator) closed the symptom, the underlying race-window pattern remains. B19 is a symptom of the same design hole (forgotten state-map registration).
- **Action**: introduce a `LeasedVmSlot` RAII guard that holds the state-map entry + vm_index reservation atomically; release on Drop or explicit commit. Subsumes the B18 fix and prevents B19-class bugs.

### [R4-Q1] (CLOSED at `28f60d73`, subsumed by R3-Q3) A7 stale 60 literal
- **File**: `crates/sandbox/src/config.rs:652` (A7's new `new_fixture()` shadow constructor)
- **Symptom**: same commit whose doc cites R3-Q2 by name introduced 2 more `60` literals (5 → 7 in-src). R3-Q3 already tracks the 11 fixture sites; this expands the count without addressing the root pattern.
- **Action**: bulk-update 60 → 120 in fixtures (R3-Q3 catch-all). Subsume R4-Q1.

### [R4-T1] (CLOSED 2026-05-23 at `4e6c70c1`) `shellcheck --severity=error` regression gate landed
- **Source**: 2026-05-24 test-coverage-r4 ran shellcheck on `crates/sandbox/scripts/nomad-vm-wrapper.sh` and got 5 INFO-level only, 0 errors/warnings.
- **Action**: add `shellcheck --severity=error crates/sandbox/scripts/*.sh` as a workspace gate. Costs minutes; would have caught B12/B13/B17-like issues pre-cluster. Sister of R3-T3 (full coverage) — this is the cheap-first-step.

### [R4-T2] A3 local wake-latency canary is feasible (IMPORTANT, test-cov-r4)
- **Source**: 2026-05-24 test-coverage-r4
- **Note**: two concurrent `do_restore_inner` calls + a `SlowGetSnapshotStore` (returns 1 GB in 100ms simulated) would expose A3's 9.5s wake p50 without a real cluster. `restore_handler.rs:286-412` already takes `&dyn` traits.
- **Action**: add a `wake_latency_canary` integration test that asserts wake completes within N×100ms when N concurrent waker tasks run. Catches A3 regression before cluster.

### [R3-T2] A5/A6/A6b builders cover happy/empty only — no fuzz/edge (MINOR, test-coverage-r3)
- **Files**: `lib.rs::admin_token_setter_tests` + `persist_setter_tests` + new `field_setter_tests`
- **Symptom**: no all-whitespace, NUL-byte, oversized-input, or fuzz coverage. Edge cases that production input validators usually need to catch.
- **Action**: add `quickcheck` or `proptest` to `dev-dependencies`; lock builders against pathological inputs (e.g., 1 MB token, NUL-embedded token).

### [R3-T3] Wrapper still zero in-repo coverage despite B17 (CRITICAL, test-coverage-r3)
- **File**: `crates/sandbox/scripts/nomad-vm-wrapper.sh` (421 LOC bash; B17 closed by editing lines 366-419, no test)
- **Symptom**: no `shellcheck`, no `bats`, no `bash -n` gate in CI. B17 (and prior B12/B13) were caught at cluster smoke — i.e., $0.50+ per discovery instead of pre-commit.
- **Action**: add `shellcheck crates/sandbox/scripts/*.sh` to the workspace CI. Optionally add `bats` smoke tests that exercise the wrapper's path-handling without spawning CH.

---

## IMPORTANT (T6 regressions — round-r2 reviewers flagged)

### [T8] `ControllerIdleSnapshotter` zero non-pg coverage (test-coverage-r2)
- **Source**: 2026-05-24 test-coverage round-r2
- **File**: `crates/sandbox/src/sweep.rs:292-383` (90 LOC of new prod code)
- **Symptom**: pg-gated tests exercise only `RecordingIdleSnapshotter`. The real prod implementation that bridges to `snapshot_handler::snapshot_sandbox` has no unit test.
- **Action**: add a non-pg unit test using a `MockSnapshotSandbox` trait/fake. Mirror `RecordingIdleSnapshotter`'s pattern but for production code paths.

### [T9] `ControllerIdleSnapshotter` duplicates `admin_handlers` orchestration (architecture-r2)
- **Source**: 2026-05-24 architecture round-r2
- **Files**: `crates/sandbox/src/sweep.rs:283-407` vs `crates/sandbox/src/admin_handlers.rs:1109-1206`
- **Symptom**: near-byte-for-byte parallel orchestrator — same `lookup_source_vm_ops` preflight, same `snap_stage_dir` resolution, same post-snapshot best-effort `teardown_source_for_snapshot`, same `StateMismatch` race tolerance. Private `ResolvedSourceVmOps` struct is byte-identical between the two files.
- **Action**: extract shared orchestrator function `pub(crate) fn perform_snapshot(...)` in a new `crates/sandbox/src/snapshot_orchestrator.rs`; both callers invoke it. Move `ResolvedSourceVmOps` to a `pub(crate)` location.

### [T10] `ControllerIdleSnapshotter` holds `Arc<AppState>` with 4-field reach (architecture-r2)
- **Source**: 2026-05-24 architecture round-r2
- **Files**: `crates/sandbox/src/sweep.rs:283-294` + `crates/sandbox/src/lib.rs:449-454`
- **Symptom**: the bridge reaches into `state.backend`, `state.config`, `state.database`, `state.snapshot_store` from sweep — turning what was a pure DB→CAS sweep module into an AppState god-Arc consumer. Layering escape hatch.
- **Action**: pass the required fields explicitly via a small constructor-bound struct (`SnapshotterDeps { backend, config, database, snapshot_store }`). Sweep then doesn't need `AppState`. Easier to unit-test.

---

## IMPORTANT (Phase B follow-ups; blocked on #14 closing)

### [B-SLO] 5-worker × 20-cycle SLO empirical validation
- **Blocked-by**: **bug #23** (provision script can't bring up SERVER_COUNT>1 cluster). Bug #22 CLOSED in r7. Single-worker c=4 baseline captured: wake p50 9235 ms (target ≤1000ms → MISS 9.2×); snapshot p50 50573 ms (target ≤2000ms → MISS 25×); both attributable to OPEN A3 (sync I/O on compio worker).
- **Action**: close #23 (1-2 LOC fix in `provision-gcp-cluster.sh` metadata escaping). Then scale to 3+5 + c=20 stress. The SLO misses won't move materially with bug #22's resync overhead (sub-100ms LAN round-trip) — they're A3-gated. R5-P1b's BufReader + spawn_blocking refactor on `store.get` is the SLO mover.

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

---

## NEW r4/r5 ROUND FINDINGS (added by pilot cycle 2026-05-23)

### [R4-S1] (CLOSED at `93348b91`) `Backend::vm_index_allocator()` pub→pub(crate)
- **File**: `crates/sandbox/src/backend/mod.rs:440-447`
- **Symptom**: B18 added `vm_index_allocator()` accessor exposing the worker slot-pool `Arc<Mutex<VmIndexAllocator>>` externally. Zero out-of-crate callers; downstream code could `.lock()` it and deadlock create/restore.
- **Action**: `pub(crate)`-restrict the accessor on Backend. Verify no out-of-crate uses first.

### [R4-S2] `ErrorEnvelope::with_extra()` silently discards non-object Values (IMPORTANT, api-surface-r4)
- **File**: `crates/sandbox/src/error_envelope.rs:93-104`
- **Symptom**: `with_extra()` takes `serde_json::Value` but silently no-ops on non-object input. Should take `serde_json::Map<String, Value>` so the type forbids the misuse at compile-time.
- **Action**: change signature; migrate the 4-5 in-crate call sites.

### [R5-A1] B19 worsens enum-as-trait (5th `Err("backend X doesn't support…")` method) (CRITICAL, arch-r5)
- **Files**: `crates/sandbox/src/backend/mod.rs:175-180,449-505`
- **Symptom**: B19's `NomadCh(Arc<NomadCHBackend>)` asymmetric wrap + new `nomad_ch_handle()` escape hatch + `register_restored` as the 5th method that returns Err for 2/3 Backend variants. Confirms r3-A1 trend: structural debt accumulates with every "small additive fix".
- **Action**: split `SnapshotCapableBackend` trait. Only NomadCH impls it. Use `&dyn SnapshotCapableBackend` at call sites needing the surface. Closes r3-A1 + r5-A1 together.

### [R5-A2] B19 added 3rd state-map insert path; R4-A2 RAII gap still open (CRITICAL, arch-r5)
- **Files**: `crates/sandbox/src/backend/nomad_ch.rs:977-990,1650-1681`
- **Symptom**: wake path now has FOUR ways to enter state map (create / restart-restore / register_restored / restore_from_pg_and_sealed). state-map removal still happens up-front in `stop_inner` while vm_index release straddles 60-120s of fence work. R4-A2's `LeasedVmSlot` RAII guard would subsume all four.
- **Action**: introduce `LeasedVmSlot` RAII guard. Implement once, use everywhere. Closes R4-A2 + R5-A2 + the structural smell underneath B18/B19.

### [R5-Q1] `RestoreBackend::register_restored` default impl silently returns Ok(()) (CRITICAL, code-quality-r5)
- **File**: `crates/sandbox/src/restore_handler.rs:162-170` (trait) + `:1023-1030` (impl warning)
- **Symptom**: trait method has a default impl returning `Ok(())` — a silent no-op. The `RealRestoreBackend::register_restored` impl's own doc-comment warns that silent no-ops re-introduce the pre-B19 slot-leak/sandbox-not-found symptoms. The default contradicts the safety property the impl is trying to enforce.
- **Action**: remove the default impl; make `register_restored` required. Or have the default `Err(...)` so a missing impl fails loudly.

### [R5-T1] B19 trait-dispatch wire-up at `restore_handler.rs:450-463` is uncovered (CRITICAL, test-cov-r5)
- **Source**: 2026-05-24 test-coverage-r5
- **File**: `crates/sandbox/src/restore_handler.rs:450-463` — the post-`wait_for_livez` Ok path that calls `backend.register_restored(...)`
- **Symptom**: direct backend-level tests for `register_restored` exist (in nomad_ch.rs:4562+); but every in-repo `restore_sandbox` caller passes `persist=None`, which takes the warn-skip arm before reaching the trait-dispatch. So the wire-up is byte-coverage-zero.
- **Action**: add an integration test that constructs a `RealRestoreBackend::with_nomad_handle(...)`, populates `persist` with a sealed record, calls `restore_sandbox`, and asserts both: (a) `register_restored` was invoked on the backend, (b) the post-stop assert (state-map empty + vm_index released) holds.

### [R5-T2] R3-T3 wrapper coverage PARTIAL-CLOSED by R4-T1 (informational, test-cov-r5)
- **Source**: 2026-05-24 test-coverage-r5
- **Status**: R4-T1's shellcheck integration test covers syntax-half of R3-T3. Behavioral wrapper-logic coverage (start vs restore branch divergence, path-rewriting, etc.) is still open. Future fixer could add `bats` smoke tests that mock CH spawn.

### [R5-S4] (CLOSED at `4fd92bef`) A4 admin sites leaked raw driver-error strings — sanitized via `err_safe()` helper

### [R5-API1] (CLOSED at `93348b91`) `Backend::register_restored` pub→pub(crate) + dead-code anchored
- **File**: `crates/sandbox/src/backend/mod.rs:487-505`
- **Symptom**: B19 added `register_restored` to the public Backend enum surface. It takes raw `[u8; 32]` signing-key bytes by value — bypasses the create-path key-minting + sealed-record contract. Zero out-of-crate callers; pub leak.
- **Action**: `pub(crate)`-restrict the method on Backend. Verify no out-of-crate uses first. Pair with R4-S1 (same pattern for `vm_index_allocator()`).

### [R5-API2] (CLOSED at `93348b91`) `Backend::nomad_ch_handle()` pub→pub(crate)
- **File**: `crates/sandbox/src/backend/mod.rs:467-474`
- **Symptom**: B19 added escape hatch to lift the Arc-wrapped backend internals out of the enum. Voids the "enum dispatch is the only contract" module promise (l. 34-40).
- **Action**: `pub(crate)`-restrict. The shared-allocator + register-restored use sites are all in-crate.

### [R5-C1] C3 widened a THIRD time by B19's `unseal+register_restored` (CRITICAL, concurrency-r5)
- **File**: `crates/sandbox/src/restore_handler.rs:450-463` (between `wait_for_livez` Ok and `update_sandbox_status(Running)`)
- **Symptom**: cancel-unsafe restore window now includes 2 more awaits (Persistence::unseal + Backend::register_restored). Drop creates a new wedge state: live VM + missing state-map + `Restoring` pg row + leaked vm_index.
- **Action**: same shape as C3 — scope-guard around the restore-Ok section. Covers all C3 widenings (r2 original + r4 `clear_snapshot_metadata` + r5 unseal/register).

### [R5-P1] (PARTIAL at `77ea717f` — BufReader landed; spawn_blocking deferred to R5-P1b) Next A3 slice: BufReader on SHA + spawn_blocking on store.get (IMPORTANT, perf-r5)

### [R5-P1b] (CLOSED at `cdd2e677`) `&dyn → Arc<dyn>` flip + `spawn_blocking` on store.get
- **Source**: R5-P1 fixer at `77ea717f` documented the blocker.
- **Files**: `crates/sandbox/src/restore_handler.rs:184,329` (`store: &dyn SnapshotStore` → needs `Arc<dyn SnapshotStore>`); `crates/sandbox/src/admin_handlers.rs:1355` (in-scope call site); `crates/sandbox/tests/sandbox_pg_e2e.rs:2504,2545,2570,2769,2882` (out-of-scope call sites that pass `&store`).
- **Symptom**: `compio::runtime::spawn_blocking` requires `FnOnce + Send + 'static`. With `&dyn`, the borrow can't be moved into the closure. The trait shape (`fn get(&self, …)`) is fine — only the call-site borrow needs to flip to `Arc::clone` first.
- **Pattern reference**: `persist.rs::unseal` at `crates/sandbox/src/persist.rs:677-687` — clones `Arc<key>` before `spawn_blocking(move || …)`.
- **Action**: change `restore_sandbox` + `do_restore_inner` to take `Arc<dyn SnapshotStore>`; migrate the 5 call sites; wrap the `store.get` body in `spawn_blocking`. Lib test count delta: 0 (refactor); estimated wake p50 reduction: 1.5-2.5s c=1, 3-5s c=4.
- **Files**: `crates/sandbox/src/snapshot_store.rs:153-185` (SHA loop) + `crates/sandbox/src/restore_handler.rs:365` (store.get await site)
- **Symptom**: post-A3-partial (hard_link), the SHA loop reads at 64 KiB unbuffered (16× syscall amplification) and the whole store.get blocks the ntex worker.
- **Action**: 2-line change: `BufReader::with_capacity(1 << 20, f)` at the SHA loop site + wrap `store.get` in `compio::runtime::spawn_blocking`. Estimated reduction: 1.5-2.5s on c=1 wake; 3-5s on c=4 wake.

### [R5-S1] (CLOSED 2026-05-23 r6 B21-fixer) B19 fail-OPEN: `register_restored` silently no-ops when persist=None
- **Status**: **CLOSED**. Boot-time fail-CLOSED guard added to `AppState::from_config` via `assert_persist_required_when_snapshot_enabled(snapshot_enabled, persist_present, test_override)`. Controller now refuses to start with a clear remediation message when `SANDBOX_SNAPSHOT_ENABLED=true && persist.is_none()`. Test escape hatch `SANDBOX_PERSIST_NONE_OK=1` lets `StubRestoreBackend`-driven test fixtures bypass the assertion. 5 unit tests pin every cell of the truth table. Cluster c=4 confirms the assertion does NOT fire in the legal `snap=on, persist=on` config (controller boots active, livez 200).
- **Cluster evidence**: `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-24-r2.md` Appendix E (boot section).

### [R5-S5] (CLOSED at `e7ecbbd6`) chmod 0o444 on alloc-side hard links; CH verified read-only on memory-ranges (upstream `memory_manager.rs::fill_saved_regions` uses O_RDONLY + `read(2)`, not MAP_SHARED)
- **File**: `crates/sandbox/src/snapshot_store.rs:259-273`
- **Symptom**: hard_link aliases canonical L1 memory-ranges + state.json to writable alloc dir. CH `MAP_SHARED` writeback or alloc-dir chmod by raw_exec silently widens canonical L1 in place. config.json is safe (sed -i renames break the link); memory-ranges is the worst case.
- **Action**: `chmod 0444` on the alloc-side hard links right after creation; or use reflink/CoW when available (`copy_file_range`); or accept that L1 is mutable and document the threat model in the L1 store's doc comment.

### [R5-S3] F2 unsigned `wait_for_livez_blocking` now closable in 3 lines (IMPORTANT, security-r5)
- **File**: `crates/sandbox/src/restore_handler.rs:1322-1341` (unsigned probe) vs `crates/sandbox/src/backend/nomad_ch.rs:2767+` (signed `wait_for_agent_livez`)
- **Note**: post-B19, `signing_key_bytes` is in scope at `restore_handler.rs:451`. Use it to call `wait_for_agent_livez` instead of the unsigned variant.
- **Status**: 39 production callsites sanitized; 5 new tests pin no-leak invariant; raw errors now log via tracing::error! for operator debug, never on wire.

---

## NEW r6 ROUND FINDINGS (added by pilot cycle 2026-05-23/24)

### [R6-#22-RC] Bug #22 root cause CONVERGED: guest wall-clock skew on CH --restore (CRITICAL, security-r6 + concurrency-r6)
- **Source**: 2026-05-24 security-r6 + concurrency-r6 independently converged on same root cause.
- **Files**: `crates/sandbox-agent/src/sig.rs:312-316,565-570` (5s skew check + raw `SystemTime::now()`); `crates/sandbox/scripts/nomad-vm-wrapper.sh:388-419` (restore branch resumes vCPU + tap but never step-syncs guest clock).
- **Root cause**: CH `--restore` resumes kvm-clock from the snapshot-frozen TSC; wrapper restore branch issues `ch-remote resume` but no `clock_settime` / VSOCK time bridge / chronyd makestep. Agent `unix_now()` lags real wall time by `snapshot_age + wake_latency` (≥9.7s observed). `sig.rs:313-316` rejects with `SkewTooLarge` → 401.
- **Refuted alternatives**: agent never rotates signing keypair (sandbox-agent/src/auth.rs:1-148 holds only controller pubkey); cmdline pubkey ≡ sealed signing_key_bytes by construction (nomad_ch.rs:626-637,867-873).
- **Diagnostic**: `journalctl -u sandbox-agent | grep auth_fail` reads the `AuthFail` discriminator. `skew-too-large` confirms hypothesis.
- **Action**: in wrapper restore branch post-`ch-remote resume`, issue clock sync via (a) VSOCK time bridge from host; (b) `chronyd makestep`; (c) `hwclock --hctosys` if RTC available; or (d) controller writes fresh ts to /workspace pre-resume and init.sh reads it. (c) is simplest.
- **Sister concern**: nonce LRU survives snapshot (sig.rs:228 Mutex<LruCache> in guest RAM); will be the next failure once skew closes. Wake-time LRU clear is a one-line fix in init.sh post-resume.

### [R6-P1] (CLOSED at `4c090992`) detach teardown_source_for_snapshot into compio::runtime::spawn
### [R6-A1] (CLOSED at `2ead8692`) rename SANDBOX_PERSIST_NONE_OK → ZEROSHIP_SANDBOX_TEST_DISABLE_PERSIST_ASSERTION
### [R6-C1] (CLOSED at `e598c2dd`) reap ch-remote-resume background subshell via PID capture + cleanup trap

---

## NEW r7 ROUND FINDINGS (added by pilot cycle 2026-05-24)

### [R7-S1] (CLOSED at `e95baa89`) clock_resync body now binds sandbox_id + 32-byte challenge + LRU replay defense; rootfs v5 + controller v18 + wrapper SANDBOX_AGENT_SANDBOX_ID env injection needed for cluster smoke
- **Source**: 2026-05-24 security-r7
- **Files**: `crates/sandbox-agent/src/sig.rs:348-355` (nonce LRU not pre-loaded for resync) + `crates/sandbox-agent/src/handlers.rs::clock_resync` (canonical body `{"ts": <unix_secs>}` lacks sandbox_id)
- **Symptom**: `/_clock_resync` is authenticated by same per-sandbox signing key as other RPCs (in-VM forgery impossible) AND the skew-bypass verifier runs nonce LRU + signature gates. BUT: canonical body has no sandbox_id and no per-restore controller challenge. The nonce LRU is the only replay defense, and because resync arrives POST-restore, no resync nonce is ever in any snapshot's LRU. A network-adjacent attacker who captured a cycle-N resync can race the controller's cycle-N+1 POST to set CLOCK_REALTIME to stale T_old → sustained 401 DoS on all strict-skew RPCs.
- **Action**: bind `sandbox_id` + a per-restore controller-issued challenge into the canonical body. Verifier checks the challenge matches the controller's pre-shared nonce for this restore.

### [R7-S2] (CLOSED at `f572a135`) `derive_agent_url` default removed — required trait method now compile-time-enforced
- **File**: `crates/sandbox/src/restore_handler.rs:182-184`
- **Symptom**: B22 added a trait method `derive_agent_url` with a Default impl that returns `http://127.0.0.1:0`. A bogus URL that resolves but answers nothing — silent fail-OPEN.
- **Action**: change default to `panic!` or `Err`. Either way the implementor MUST provide a real URL.

### [R7-API1] (CLOSED at `0a271d2f`) `Verifier::verify_kind_skew_bypass` pub→pub(crate); `verify_signed_skew_bypass` already module-private
- **File**: `crates/sandbox-agent/src/sig.rs:356`
- **Symptom**: B22's skew-bypass verifier is public on a public module. Same anti-pattern R4-S1/R5-API1/R5-API2 just closed at `93348b91`, but this regressed across the crate boundary in sandbox-agent. Docstring asserts "only /_clock_resync uses it" but type system doesn't enforce.
- **Action**: `pub(crate)`-restrict on sandbox-agent crate. Verify zero out-of-crate callers first.

### [R7-API2] (CLOSED — comment fix; clock.resync-v1 documented as mandatory; +1 regression test) capability list reframed as per-entry-semantic
- **Files**: `crates/sandbox-agent/src/version.rs:1-90` (module + const doc + per-entry comment) + new test `mandatory_clock_resync_v1_present`
- **Resolution**: chose option (b) — fix the misleading "feature-detectable for graceful fallback" framing. The deployment story is "agent v17+ always has clock.resync-v1; older agents fail elsewhere (R8-A4 envelope, R8-DEPLOY1 sandbox_id binding) before this call path is reached." Wiring feature-detection (option a) would require an extra HTTP call to `/_version` + capability cache + skip-with-log branch in restore_handler.rs — YAGNI plumbing for a downgrade scenario that can't happen. Updated module + const docs to call out that per-entry semantic is NOT uniform: some caps (`proxy.ws-v1`) are genuinely feature-detected, others are diagnostic / mandatory. Per-entry comment on `clock.resync-v1` now explicitly says **Mandatory (not feature-detected)** with the rationale and a pointer to wire feature-detection if the situation ever changes. Added `mandatory_clock_resync_v1_present` regression test (240 passing, +1 from 239 baseline) that catches silent removal of the cap without simultaneous controller-side feature-detect wiring.

### [R7-C1] R6-P1 `spawn(...).detach()` is unbounded — no cancellation/completion tracking (IMPORTANT, concurrency-r7)
- **File**: `crates/sandbox/src/admin_handlers.rs:1310-1324`
- **Symptom**: detached teardown task has no JoinHandle / cancellation signal. Under controller shutdown, the spawn is severed mid-await; the orphan-prune sweep at next-boot reclaims state. Not a functional bug — but no operational visibility.
- **Action**: track outstanding detached teardowns via a per-AppState counter + log on shutdown if N > 0. Optional: replace detach with a structured task supervisor.

### [R7-C2] C3 cancel-window widened a 4th time by B22's clock_resync (CRITICAL, concurrency-r7)
- **File**: `crates/sandbox/src/restore_handler.rs:485-506`
- **Symptom**: post-`wait_for_livez` Ok now includes (unseal + clock_resync up to 10s ureq + register_restored + 2 pg awaits) — all unguarded. Drop creates new wedge state.
- **Action**: same as C3 — scope guard around the restore-Ok section. Covers all 4 C3 widenings.

### [R7-P1] (CLOSED at `79428d53`) spawn_blocking on ch.pause/snapshot + store.put — A3 slice 4 complete; estimated snapshot p50 50s→10-15s at c=4 (pending cluster verification)
- **File**: `crates/sandbox/src/snapshot_handler.rs:316-373,566-621`
- **Symptom**: r6 root-causing was wrong — R6-P1 detach correctly removed the teardown wait but snapshot p50 stayed at 50s. Real cause: `ChRemoteClient::pause/snapshot` + `LocalDiskSnapshotStore::put` run synchronously on the async caller. Trait doc at `snapshot_store.rs:99` mandates `spawn_blocking`; no caller wraps. At c=4 on n2-standard-4, four sync 2GB-dumps contend on local SSD → ~40-50 MB/s per stream → matches observed 50s wall.
- **Action**: wrap `ch.pause`, `ch.snapshot`, AND `LocalDiskSnapshotStore::put` in `compio::runtime::spawn_blocking`. Same shape as R5-P1b's spawn_blocking wrap on `store.get`. Estimated snapshot p50 reduction: 50s → 10-15s at c=4 (limited by SSD bandwidth at concurrency=4 × 2GB = 8GB).

### [R7-P2] (CLOSED at 6f314025) `TieredSnapshotStore::put` detaches L2 GCS via `compio::runtime::spawn` (not `spawn_blocking`) (IMPORTANT, performance-r7)
- **File**: `crates/sandbox/src/snapshot_store_gcs.rs:832`
- **Symptom**: 1GB GCS PUT runs on a regular compio task, parking a runtime worker. While next snapshot's `ch.snapshot` writes the same SSD, the L2 PUT competes for both compio worker + SSD bandwidth.
- **Action**: change `spawn(...)` → `spawn_blocking(...)`. Frees the compio worker; SSD still contends, but compio scheduler can serve `/livez` etc.

---

## NEW r8 ROUND FINDINGS (added by pilot cycle 2026-05-24)

### [R8-DEPLOY1] (CLOSED at `f0ebf783`) kernel cmdline `SANDBOX_AGENT_SANDBOX_ID=${ZSBX_SANDBOX_ID}` injection; typed_id-shape validator; both cold-boot + restore branches handled (restore inherits from snapshot RAM)
- **Source**: 2026-05-24 security-r8
- **Files**: `crates/sandbox-agent/src/main.rs:97-102` (binds requirement) vs `crates/sandbox/scripts/nomad-vm-wrapper.sh` (does not write env or `/run/keys/sandbox-id`)
- **Symptom**: R7-S1 added a fail-closed assertion that the agent must learn its sandbox_id at boot, but the wrapper that spawns the VM doesn't pass it. Boot exits 1; every cluster wake breaks.
- **Action**: in the wrapper restore + cold-boot branches, after the tap-up sequence, add `--env "SANDBOX_AGENT_SANDBOX_ID=$ZSBX_SANDBOX_ID"` to the CH `--cmdline` (or write to `/run/keys/sandbox-id` via cloud-init). Upload updated wrapper to GCS. Rebake rootfs v5. Rebuild controller v18.

### [R8-A4] (CLOSED at `fc3e9972` + `ae5cc977`) sandbox-agent `error_envelope.rs` lands; 34 sites migrated; proxy.rs A4 field-order INVERSION fixed (was `{"error":<prose>,"code":<kind>}`); 13 new wire-shape tests pin contract
- **Source**: 2026-05-24 api-surface-r8 (quantified)
- **Files**: `crates/sandbox-agent/src/handlers.rs` (13× err, 11× unauthorized, 5× draining, 3× clock_resync from R7-S1), `crates/sandbox-agent/src/proxy.rs:98,115,194,201` (err + err_with_code, latter INVERTS A4 field order), `crates/sandbox-agent/src/main.rs:1×`
- **Symptom**: A4 (§10.0 ErrorEnvelope) closed in sandbox crate at `2928d5ae` but never extended to sandbox-agent. All ~34 sites emit non-§10.0 shapes. R7-S1's `/_clock_resync` inherits the broken shape.
- **Action**: extract `ErrorEnvelope` from `crates/sandbox/src/error_envelope.rs` into either `zeroship-core` (cross-crate) OR duplicate into `crates/sandbox-agent/src/error_envelope.rs`. Migrate all 34 sites. Add wire-shape tests (cap N per file).

### [R8-A3-5] (CLOSED at `64cbb447`) spawn_blocking wraps on submit_restore_job + wait_for_livez; restore_sandbox + do_restore_inner signature flip to `Arc<dyn RestoreBackend>`; 5 test sites migrated. Expected wake p50 reduction 5-8s/wake pending cluster smoke verification.
- **Source**: 2026-05-24 performance-r8
- **File**: `crates/sandbox/src/restore_handler.rs:460-467` (callers) + `:1378,1411` (the std::thread::sleep parking sites)
- **Symptom**: `submit_restore_job` + `wait_for_livez` are sync internally (`std::thread::sleep` parking ntex worker 5-8s/wake). Single largest remaining wake-path target.
- **Action**: wrap both in `compio::runtime::spawn_blocking`. Requires `Arc<dyn RestoreBackend: Send + Sync>` (or method-to-free-fn flip). Pattern: `cdd2e677` (R5-P1b). Estimated wake p50 reduction: **9235ms → 2500-4000ms**.

### [R8-T1] (NO-OP — already declared at trait birth in `70ba24db`) `ChRemoteClient: Send + Sync` bound was always present; reviewer read stale source
- **File**: `crates/sandbox/src/snapshot_handler.rs:92-105`
- **Symptom**: R7-P1's spawn_blocking correctness rests on incidental impl auto-derivation. A future `Rc<_>`-bearing impl would silently break the spawn_blocking call site without trait-level compile error.
- **Action**: add `: Send + Sync` to the trait declaration. One-line change.

### [R8-CONC1] R7-P1 widened C3 a 5th time (CRITICAL, concurrency-r8)
- **File**: `crates/sandbox/src/snapshot_handler.rs:355-407`
- **Symptom**: snapshot path now has 4 awaits; drop after `ch.snapshot` Ok but before `store.put` wedges a paused VM + staged 2 GB artifact + `Snapshotting` pg row permanently (C1 sweep is dead).
- **Action**: subsumed by R4-A2's `LeasedVmSlot` RAII guard (4 cycles open). The structural fix would close both snapshot-path AND restore-path C3 widenings.

### [R8-CONC2] (CLOSED at `f1bed99a`) `RESYNC_CHALLENGE_CAPACITY` 4→32 (sized for 12 vm_index × 2-3 retries ≈ 36 worst-case)
- **File**: `crates/sandbox-agent/src/handlers.rs:76`
- **Symptom**: LRU=4 not exploitable today (controller mints one challenge per restore), but any future retry-on-transient that pushes 3+ resyncs in seconds could evict the legit challenge.
- **Action**: bump to 16-32. One-line const. Doc the chosen capacity.

---

## NEW r9 ROUND FINDINGS (added by pilot cycle 2026-05-25, post critical-fix sweep audit)

### [C1-FOLLOWUP] (CLOSED at `0e71e5c4`) sweep query excludes self.host_id() and new claim_orphan_transient_for_recovery fences on the row's observed (host_id, generation) instead of self.host_id(); ownership atomically transferred to self.host_id() on hit; ABA-safe via `lessee_updated_at < now() - threshold` predicate; 1 updated + 3 new pg-gated regression tests
- **Source**: 2026-05-25 concurrency-r9 audit of C1 (closed at `de3523c4`)
- **Files**: `crates/sandbox/src/sweep.rs:167-169` + `crates/sandbox/src/db.rs:1690-1697,1773`
- **Symptom**: C1 correctly sets `lessee_updated_at` on transient entry + clears on exit. BUT the recovery CAS uses `self.host_id()` (current controller's host_id) which by definition never equals the CRASHED controller's host_id stored on the row. So §6.1 sweep query finds rows but the CAS-UPDATE rejects every one of them.
- **Fix shape (landed)**: (1) `db.rs::transient_state_lease_expired_sandboxes` now also filters `host_id <> self.host_id()` at the SELECT level; (2) new `db.rs::claim_orphan_transient_for_recovery` does the recovery CAS with fences on the row's observed `(host_id, generation)` AND `lessee_updated_at < now() - threshold` (ABA-safe), then on hit transfers ownership to `self.host_id()`, bumps generation, flips to recovery target, clears lessee; (3) `sweep.rs::run_transient_takeover_once` calls the new fn instead of `update_sandbox_status`.
- **A1 follow-up flag**: split out as its own [A1-FOLLOWUP] entry below — DO NOT lose track.

### [A1-FOLLOWUP] Boot warns vs panics when `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` missing in tiered+GCS mode (CRITICAL, arch-r9 fail-CLOSED gap)
- **Source**: 2026-05-25 (split from C1-FOLLOWUP's tracking note; originally arch-r9)
- **File**: `crates/sandbox/src/lib.rs` (AppState boot path that composes the snapshot store stack — search for the wrap point closed in A1 commit `18e2034b`)
- **Symptom**: A1 added the `AeadSnapshotStore` wrap when the KEK env var is set. But when `SANDBOX_SNAPSHOT_BACKEND=tiered` (L1 local + L2 GCS) and the operator FORGETS to set the KEK path, boot logs a warning and continues with bare plaintext-to-GCS — exactly the audit-trail-vs-reality gap A1 was meant to close. Per arch-r9's fail-CLOSED principle, a missing KEK in any mode that writes to remote object storage MUST panic at boot.
- **Action**: in the boot composition site (lib.rs around the A1 wrap), if the backend stack includes a non-local L2 (GCS or any other remote) and `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` is unset, return a hard configuration error from `AppState::from_config` (boot panic, not log line). Local-only L1 is the only mode allowed to run without a KEK. Add a `SANDBOX_SNAPSHOT_ALLOW_UNENCRYPTED_REMOTE=1` escape hatch for non-production envs that explicitly opt in.

### [R9-C1] R8-A3-5 widened C3 a 6th time — cancel-unsafety window now 7 awaits (CRITICAL, concurrency-r9)
- **Source**: 2026-05-25 concurrency-r9
- **File**: `crates/sandbox/src/restore_handler.rs:489-517`
- **Symptom**: pre-R8-A3-5, `submit_restore_job` and `wait_for_livez` were sync inline non-yielding calls. R8-A3-5 wrapped both in spawn_blocking, adding TWO new await points to `do_restore_inner`. Cancel-unsafety window: 5 → 7 awaits. Combined with C1-FOLLOWUP (sweep can't recover), a controller-restart-mid-restore = permanent wedge.
- **Action**: subsumed by R4-A2's `LeasedVmSlot` RAII guard. The structural fix would close all 6 C3 widenings + the C1-FOLLOWUP recovery gap simultaneously. Now incident-class urgency.

### [R9-P1] AEAD-active wake discards R5-P1's hard_link zero-copy win (CRITICAL, performance-r9)
- **Source**: 2026-05-25 performance-r9
- **File**: `crates/sandbox/src/snapshot_aead.rs:619-664`
- **Symptom**: post-A3+A1, wake p50 estimated 2.5-4.5s when AEAD inactive (down from 9.2s — huge win). But AEAD-active wake = 5.5-7.5s because `AeadSnapshotStore::get` re-writes a full 1GB plaintext copy into `alloc_dir/memory-ranges` after the inner store hard-linked ciphertext into `stage/`. At c=4, four concurrent 1GB writes contend on the SSD queue.
- **Action**: in-place decrypt: stream ciphertext from `stage/<sha>` and write plaintext directly to `alloc_dir/memory-ranges` in a single pass (no intermediate copy). Estimated wake p50 reduction: 0.5-1.5s/wake when AEAD on. Restores the hard_link win.

### [R9-P2] A2b verify_metadata_only has ZERO production callers — dead code (IMPORTANT, performance-r9)
- **Source**: 2026-05-25 performance-r9
- **File**: `crates/sandbox/src/snapshot_store_gcs.rs:693-718` (verify_metadata_only landed) + `crates/sandbox/src/sweep.rs` (no caller)
- **Symptom**: A2b's fast-path is plumbed but nothing in `sweep.rs` actually calls `verify_metadata_only`. The whole point was that periodic sweeps use the fast-path; sweeps just don't call verify at all today.
- **Action**: wire `verify_metadata_only` into the sweep's L2 integrity check (or document why the sweep doesn't need it; A2b may have anticipated a future sweep that hasn't materialized).

### [R9-P3] AEAD snapshot path is 3 separate I/O passes on memory-ranges — fusable to 1 (IMPORTANT, performance-r9)
- **Source**: 2026-05-25 performance-r9
- **Files**: `crates/sandbox/src/snapshot_aead.rs:363-451` + `crates/sandbox/src/snapshot_store.rs:184-223`
- **Symptom**: snapshot path with AEAD on does (1) encrypt-read, (2) encrypt-write, (3) SHA-read — three full 1GB passes on the same file. Fusable into one streamed pass via a chained reader.
- **Action**: implement `EncryptingHashWriter<W>` that wraps the destination with both AEAD encrypt + SHA-256 in a single pass. Estimated snapshot p50 reduction: 0.6-1s/snapshot.

---

## NEW r9/r10 ROUND FINDINGS (added by pilot cycle 2026-05-25 r2; post-C1-FOLLOWUP, post-R8-API1 full close)

### [R9-S1] AEAD by-design exempts `config.json`; W1 Python rewrite passes through attacker-substituted disks[].path / serial.file (CRITICAL, security-r9)
- **Source**: 2026-05-25 security-r9
- **Files**: `crates/sandbox/src/snapshot_aead.rs` (config.json deliberately unwrapped) + `crates/sandbox/scripts/nomad-vm-wrapper.sh` (Python rewrite anchored at `^/opt/nomad/data/alloc/.../local(/|$)`; non-matching paths pass through verbatim)
- **Symptom**: GCS-bucket-write attacker can substitute `disks[].path` or `serial.file` with arbitrary host paths (e.g. `/etc/shadow`, `/dev/sda`). Deep verify SHA-256 catches it but only if a sweep deep-verifies the artifact; AEAD doc-comment is misleading ("only memory-ranges wrapped" implies the rest are integrity-protected).
- **Action**: extend AEAD wrap to config.json + state.json (struct-aware reserialize after decrypt — keep the wrapper's anchored-rewrite trust boundary). Alternatively: at wake time, validate every path in config.json against a whitelist before passing to CH.

### [R9-S2] AEAD DEK derivation uses 1-sec timestamp granularity — same-second re-snapshot = ChaCha20-Poly1305 nonce reuse (IMPORTANT, security-r9)
- **Source**: 2026-05-25 security-r9
- **File**: `crates/sandbox/src/snapshot_aead.rs` (DEK derivation function; uses `time_of_put_unix_secs`)
- **Symptom**: same-sandbox same-second re-snapshot derives identical DEK; chunk nonce starts from same prefix → nonce reuse if plaintext differs across the two snapshots. Confidentiality + integrity broken for the colliding pair.
- **Action**: extend DEK derivation domain to include a per-snapshot random salt (32 bytes); store salt in the AEAD header. Or use a monotonic counter from pg.

### [R9-S3] `snapshot_aead_dek_id="v1"` hard-coded regardless of AEAD active — pg metadata diverges from artifact truth (IMPORTANT, security-r9)
- **Source**: 2026-05-25 security-r9
- **File**: `crates/sandbox/src/snapshot_handler.rs:417`
- **Symptom**: when AEAD inactive (no KEK), pg still stamps `snapshot_aead_dek_id="v1"`. Operator audit trail says "encrypted" but artifact is plaintext.
- **Action**: stamp the actual posture (`"none"` when no KEK; `"v1"` only when `AeadSnapshotStore` wrapped).

### [R9-S4] (CLOSED at cca1e74d) KEK loader uid check landed
- **Source**: 2026-05-25 security-r9
- **File**: KEK loader (locate via `grep -rn "AEAD_KEY_PATH" crates/sandbox/src/`)
- **Symptom**: a non-root attacker who can pre-create a chmod-400 file at the KEK path before systemd starts can supply a known-key to the controller, breaking confidentiality of all future snapshots.
- **Action**: add `metadata().uid() == 0` check; refuse to load otherwise. Cheap insurance.

### [R9-S4b] (CLOSED at e4e5db60) persist.rs AeadKey uid check landed (sibling of R9-S4)
- **Source**: 2026-05-25 r2 sweep — found by R9-S4 fixer as a sibling instance that was OUT-OF-SCOPE for the R9-S4 commit (`cca1e74d`).
- **File**: `crates/sandbox/src/persist.rs::AeadKey::from_path` (~line 326-354)
- **Symptom**: same vulnerability shape as R9-S4 but on `SANDBOX_AEAD_KEY_PATH` (the sealed-records persistence AEAD key, distinct from the snapshot-store root KEK). Mode 0o400 checked but uid not — non-root attacker who pre-creates the file at the path before systemd starts can supply a known key for sealed-record encryption.
- **Action**: same one-line `metadata().uid() == 0` fix as R9-S4. Mirror the test pattern from `snapshot_aead.rs` (`from_path_rejects_non_root_owned_file`).

### [R9-S4c] (CLOSED at 2c10f63a) db.rs enforce_password_file_mode uid check landed (sibling of R9-S4 / R9-S4b)
- **Source**: Discovered by R9-S4b fixer 2026-05-25 r4. Comment at `crates/sandbox/src/db.rs:810` literally said "Mirrors `persist::AeadKey::from_path`" — inherited R9-S4 / R9-S4b's defect.
- **File**: `crates/sandbox/src/db.rs:812-831` (`enforce_password_file_mode`)
- **Symptom**: mode-only-no-uid on `SANDBOX_DATABASE_PASSWORD_PATH` (pg-superuser-password file). Non-root attacker pre-creating a 0o400 file at this path before systemd injects an attacker-known pg password → controller connects to pg with that password. Bigger blast radius if attacker can also influence DNS / pg endpoint.
- **Action**: same one-line `metadata().uid() == 0` fix. Added 2 tests mirroring R9-S4/R9-S4b naming; existing `_rejects_loose_permissions` test gained a uid-aware guard on its 0o400-pass arm.
- **Fourth sibling identified (NOT closed here)**: `crates/sandbox/src/lib.rs::load_admin_token` (~line 924-942) loads `SANDBOX_ADMIN_TOKEN_PATH` with mode 0o400 checked but uid not. Same vulnerability shape, same one-line fix. Tracked as a follow-up so this commit stays focused on the pg-password loader. See new R9-S4d entry below.

### [R9-S4d] (CLOSED at b4c3ef27) `lib.rs::load_admin_token` uid check landed (fourth and final sibling of R9-S4 / R9-S4b / R9-S4c)
- **Source**: Discovered by R9-S4c fixer 2026-05-25 r5. Comment at `crates/sandbox/src/lib.rs:908` says "Mirrors `Persistence::AeadKey::from_path`" — inherited the same mode-only-no-uid defect that R9-S4 / R9-S4b / R9-S4c each patched in their respective loaders.
- **File**: `crates/sandbox/src/lib.rs:924-957` (`load_admin_token`); the mode check is at line 937-942, missing the owner-uid assertion.
- **Symptom**: mode-only-no-uid on `SANDBOX_ADMIN_TOKEN_PATH` (admin-API bearer-token file). A non-root attacker who pre-creates a 0o400 file at this path before systemd starts injects an attacker-known admin token. The controller then accepts that token on every admin endpoint — full admin-API takeover on first boot. **Largest blast radius of the four R9-S4 siblings**: R9-S4 leaks future snapshot confidentiality; R9-S4b leaks sealed-record confidentiality; R9-S4c is a controller→pg redirect (requires DNS lift); R9-S4d is immediate admin-API takeover with no other lift.
- **Action**: same one-line `metadata().uid() == 0` fix. `load_admin_token` returns `Result<_, String>` (matching R9-S4 / R9-S4b — NOT R9-S4c's `DatabaseError::Validation`); error message format mirrors the existing function shape (`"SANDBOX_ADMIN_TOKEN_PATH={path:?}: owner uid {uid} != 0 (chown root:root the file)"`). Added 2 dedicated tests (`load_admin_token_rejects_non_root_owned_file`, `load_admin_token_accepts_root_owned_file_when_running_as_root`) and uid-aware guards on the three existing positive arms (`loader_reads_token_when_mode_0o400`, `loader_refuses_empty_file`, `loader_trims_trailing_newline`) since the mode-0o400 happy path now requires uid 0. Sandbox lib 317 → 319.
- **Fifth sibling sweep**: ran `grep -rnE "permissions\(\)\.mode\(\)|0o400 " crates/sandbox/src/` — only other `permissions().mode()` site is `config.rs:486` (wrapper-script-executability check, NOT a secret loader; orthogonal to the R9-S4 family). No fifth sibling exists.
- **Follow-up (NOT addressed)**: R11-Q2 helper extraction (consolidate the four near-identical strict-root-owned-secret loaders into `pub(crate) fn read_root_owned_secret_file(...)`) remains a separate refactor, tracked at its own R11-Q2 entry.

### [R9-S5] Restore-branch wrapper handles ZSBX_SANDBOX_ID asymmetrically vs cold-boot (informational, hides pre-R7-S1-snapshot wedge mode) (IMPORTANT, security-r9)
- **Source**: 2026-05-25 security-r9
- **Files**: `crates/sandbox/scripts/nomad-vm-wrapper.sh` (restore branch ~366-419 vs cold-boot branch ~140-170) + `crates/sandbox/src/backend/nomad_ch.rs::build_restore_nomad_job_json`
- **Symptom**: restore-branch logs `ZSBX_SANDBOX_ID` informationally but has no validator and no cmdline injection (because the agent inherits its sandbox_id from the snapshot RAM image — for a pre-R7-S1 snapshot, that's undefined). Wake-from-pre-R7-S1 snapshot wedges silently.
- **Action**: either inject `SANDBOX_AGENT_SANDBOX_ID` on the restore branch too (cheap insurance; backward-compat with restored agents that ignore the env) OR gate restore on snapshot version metadata (refuse pre-R7-S1 snapshots in pg).

### [R9-S6] (MINOR carry) Restore-internal `/_clock_resync` error path still embeds 256 chars of agent body in journald
- **Source**: 2026-05-25 security-r9 (r8 carry)
- **File**: `crates/sandbox/src/restore_handler.rs::clock_resync_post_restore` error path
- **Symptom**: wire is sanitized but journald log line embeds up to 256 chars of arbitrary agent-controlled response body. Information leak to a non-root reader of journald.
- **Action**: truncate to 64 chars + force-ascii-printable in the log line.

### [R10-C1] (CLOSED at `be246395`) `teardown_restore` now removes the state-map entry before vm_index release via `NomadCHBackend::unregister_restored`; 4 regression tests pin the symmetric inverse (in-module unit + cross-module integration, plus early-rollback idempotency). Structural cure via R4-A2 LeasedVmSlot RAII remains follow-up.

### [R10-C2] (CLOSED at `be246395`) Rollback call at `restore_handler.rs:274` now wrapped in `compio::runtime::spawn_blocking` (option b2: call site wrap, keep callee sync — `RestoreBackend` trait stays sync so the in-crate `StubRestoreBackend` test scaffolding doesn't propagate `async fn`). Structural include_str! regression test pins the wrap shape. Sibling `snapshot_handler.rs:424 vm_ops.teardown_source` has the same sync-on-async shape, NOT fixed in this commit (separate scope).

### [R10-I1] Drop ordering on final CAS Err leaves state-map entry + (with R10-C1) ghost (IMPORTANT, concurrency-r10)
- **Source**: 2026-05-25 concurrency-r10
- **File**: `crates/sandbox/src/restore_handler.rs:~595-620` (final CAS Err arm)
- **Action**: subsumed by R4-A2 LeasedVmSlot RAII; the R10-C1 interim 1-liner partially closes this too.

### [R10-P1] AEAD+GCS snapshot path reads memory-ranges 5 times sequentially per snapshot — fusable to 2 (CRITICAL, performance-r10)
- **Source**: 2026-05-25 performance-r10
- **Files**: `crates/sandbox/src/snapshot_aead.rs:363-449` + `crates/sandbox/src/snapshot_store.rs:184-223` + `crates/sandbox/src/snapshot_store_gcs.rs:539-622`
- **Symptom**: 5 passes: (1) encrypt-in-place input read, (2) L1 SHA, (3) L2 canonical SHA, (4) L2 per-file SHA, (5) L2 upload stream. Fusable to 2.
- **Action**: implement streaming `EncryptingHashWriter` (subsumes R9-P3) + plumb `SnapshotStore::put` to accept a precomputed sha256 (R10-P4). Estimated savings ~1.5-2.5 s/snapshot at single-stream SSD; unknown at c=N.

### [R10-P3] `cipher.encrypt`/`decrypt` allocate fresh 1-MiB Vec per chunk — 2048 heap allocs per AEAD round-trip (IMPORTANT, performance-r10)
- **Source**: 2026-05-25 performance-r10
- **File**: `crates/sandbox/src/snapshot_aead.rs` (encrypt/decrypt chunk loop)
- **Action**: switch to `encrypt_in_place_detached` / `decrypt_in_place_detached`. Estimated saving: bounded by alloc-amortization; likely ~10-50 ms per 1 GB round-trip.

### [R10-P4] GCS L2 put recomputes canonical SHA that L1 already knows (IMPORTANT, performance-r10)
- **Source**: 2026-05-25 performance-r10
- **File**: `crates/sandbox/src/snapshot_store.rs` (put signature) + `crates/sandbox/src/snapshot_store_gcs.rs::put`
- **Action**: extend `SnapshotStore::put` signature with `precomputed_sha256: Option<Hash>`. L1 → L2 call passes the L1-computed hash; L2 verifies the upload completion against it instead of recomputing.

### [R10-P5] (MINOR) Missing `BufWriter` on AEAD output paths
- **Source**: 2026-05-25 performance-r10
- **Action**: wrap the destination with `BufWriter::with_capacity(1 << 20, …)`.

### [R10-P6] ~32 fresh ureq connections per wake at 250 ms / 150 ms poll cadences — cached `ureq::Agent` saves ~30-100 ms (IMPORTANT, performance-r10)
- **Source**: 2026-05-25 performance-r10
- **File**: `crates/sandbox/src/backend/nomad_ch.rs::wait_for_alloc_running` + `wait_for_livez` polling
- **Action**: cache a `ureq::Agent` per controller (or per nomad_url) with keep-alive + connection pool. One-shot fix; saves connection-establishment cost on every poll.

### [R10-P7] (MINOR) `clock_resync_random_hex` could use `hex::encode` (refines r9 #7)

### [R9-T1] (CRITICAL) AEAD + persist + clock_resync + register_restored block has zero end-to-end integration coverage
- **Source**: 2026-05-25 test-coverage-r9
- **File**: `crates/sandbox/src/restore_handler.rs:557-593` (the post-livez Ok arm with persist=Some)
- **Symptom**: every existing `restore_sandbox` integration test passes `persist=None` (StubRestoreBackend), which takes the warn-skip branch. The B19 + B22 + R7-S1 + R8-A3-5 trait-dispatch wire-up is byte-coverage-zero.
- **Action**: add an integration test that constructs a `RealRestoreBackend::with_nomad_handle(...)`, populates `persist` with a sealed record + fake snapshot artifact, calls `restore_sandbox`, and asserts: (a) `unseal` was called, (b) `clock_resync_post_restore` was called with the right url/sandbox_id, (c) `register_restored` was called on the backend, (d) post-stop assert (state-map empty + vm_index released) holds. Pattern from existing nomad_ch::tests with a mock RestoreBackend.

### [R9-T2] (CLOSED at `0e71e5c4`) sweep takeover used same-host fixture; now uses `inject_extra_host` to simulate crashed peer

### [R9-T3] 6 spawn_blocking-panic-recovery branches untested (IMPORTANT, test-coverage-r9)
- **Source**: 2026-05-25 test-coverage-r9
- **Action**: a unit test per spawn_blocking site that pre-poisons via `panic!()` inside the closure; assert the parent surfaces a clean Backend error envelope, not a panic.

### [R9-T4] (CLOSED at `419c154b`) AEAD header-validation guard arms untested
- **Source**: 2026-05-25 test-coverage-r9
- **File**: `crates/sandbox/src/snapshot_aead.rs` (header parse, `decrypt_to`)
- **Resolution**: 7 negative tests added (4 from the original list + 3 additional arms surfaced while reading the parser). Each pins the specific error-message substring since every guard collapses to `SnapshotError::InvalidArtifact(String)` — a bare `is_err()` would conflate magic/version/cipher/nonce/chunk-bounds failures.
  - `aead_decrypt_rejects_bad_magic` — flips byte 0 → "AEAD magic mismatch"
  - `aead_decrypt_rejects_bad_version` — flips byte 8 → "AEAD version unsupported"
  - `aead_decrypt_rejects_bad_cipher_tag` — flips byte 9 → "AEAD cipher tag unsupported"
  - `aead_decrypt_rejects_bad_nonce_prefix` — flips byte 20 → "AEAD nonce-prefix mismatch" (defense-in-depth pre-check arm, not the chunk-decrypt arm)
  - `aead_decrypt_rejects_truncated_header` — additional arm: truncates blob to 16B → "AEAD header read"
  - `aead_decrypt_rejects_chunk_length_exceeds_cap` — additional arm: chunk_len > CHUNK_CIPHERTEXT_MAX → "exceeds cap"
  - `aead_decrypt_rejects_chunk_length_below_tag_size` — additional arm: chunk_len < AEAD_TAG_LEN → "below tag size"
- **Verification**: `cargo test -p zeroship-sandbox --lib` 289 → 296 (no regression).

### [R9-T5] `build_restore_nomad_job_json` has zero test callers — restore-side env block contracts untested (IMPORTANT, test-coverage-r9)
- **Source**: 2026-05-25 test-coverage-r9
- **File**: `crates/sandbox/src/backend/nomad_ch.rs::build_restore_nomad_job_json`
- **Action**: mirror the cold-boot path's `nomad_job_spec_includes_sandbox_id_env` regression test (B24 added). Pin restore-side env block shape too.

### [R9-T6] `derive_agent_url` duplicated across two backends; no test pins they match (IMPORTANT, test-coverage-r9)
- **Source**: 2026-05-25 test-coverage-r9
- **Files**: `crates/sandbox/src/restore_handler.rs::RealRestoreBackend::derive_agent_url` + `crates/sandbox/src/backend/nomad_ch.rs::NomadCHBackend::derive_agent_url`
- **Action**: add a test that constructs both backends with same inputs + asserts the URLs are byte-equal. Closes the silent-drift risk.

### [R9-T7] (CLOSED at `73a263a2`) `init_sandbox_id_from_env` has zero direct tests
- **Source**: 2026-05-25 test-coverage-r9
- **File**: `crates/sandbox-agent/src/handlers.rs`
- **Resolution**: extracted the parsing slice into `read_sandbox_id_from_sources(fallback_path)` — pure helper, no OnceLock touch, behaviour-identical when called with the production fallback constant. 8 new lib tests pin: env-var happy-path (32-hex), empty-env rejected, arbitrary-string accepted (no shape guard — see R9-T7-FOLLOWUP below), hyphenated UUID accepted (no .simple() normalisation), file-fallback used when env absent (trailing newline trimmed), no-env + missing-file returns Err naming both sources, file-fallback empty-after-trim rejected, and an e2e `init_sandbox_id_from_env` call that asserts OnceLock wiring. Tests serialised via a module-local `Mutex` (`SANDBOX_ID_ENV_LOCK`) with an `EnvGuard` RAII so a panicking test restores the prior env. `cargo test -p zeroship-sandbox-agent --lib`: 231 → 239 (no regression, no flake across two runs).

### [R9-T7-FOLLOWUP] (NEW, IMPORTANT) `init_sandbox_id_from_env` accepts any non-empty string — no typed_id / UUID shape guard
- **Source**: R9-T7 fixer investigation 2026-05-25
- **File**: `crates/sandbox-agent/src/handlers.rs::read_sandbox_id_from_sources`
- **Surprise**: the R9-T7 brief assumed B24 had landed a typed_id-shape guard (32-hex or hyphenated UUID). Reading the actual code shows there is NO such guard — only `id.is_empty()` after `.trim()`. A wrapper that misconfigures `SANDBOX_AGENT_SANDBOX_ID="not-a-uuid"` will boot the agent with that exact string and the `/_clock_resync` handler will compare it byte-equal against the controller-signed body's `sandbox_id` field. The hardening surface is: a typo-d wrapper deployment silently weakens the cluster-wake replay defence to "any agreed-upon string".
- **Pinned by**: `r9t7_read_env_var_arbitrary_string_accepted_no_shape_guard` — the test name flags the gap so a future tightening PR breaks the pin and must update both the guard and the test.
- **Action (deferred — out of scope for R9-T7 fixer)**: add UUID-shape validation via `uuid::Uuid::parse_str` at boot, return Err with a clear "not a UUID" message. Coordinate with the controller side, which already canonicalises hyphenated form for `TEST_SANDBOX_ID`. Update both the production code and the R9-T7 pin in the same PR.

### [R9-T8] (MINOR) Sweep recovery end-to-end pinned only for Snapshotting, not Restoring/RestoringCold

### [R9-T9] (MINOR) `/_clock_resync` 4 KiB body cap has no pin test or 413 A4-envelope shape test

### [R9-T10] (MINOR carry) r8's slow-agent / agent-500 / malformed-body gaps on clock_resync_post_restore

---

## NEW r10 ROUND FINDINGS (added by pilot cycle 2026-05-25 r3 — arch/code-quality/api-surface)

### [R10-A1] `claim_orphan_transient_for_recovery` pushed db.rs to 3003 LOC; mate reader is outside db.rs (IMPORTANT, architecture-r10)
- **Source**: 2026-05-25 architecture-r10
- **Files**: `crates/sandbox/src/db.rs:2500` (the C1-FOLLOWUP recovery CAS) + `crates/sandbox/src/restore_handler.rs:332` (`read_snapshot_row` — an inline pg reader outside db.rs)
- **Symptom**: recovery semantics now span db.rs (3003 LOC) AND restore_handler.rs. The C1-FOLLOWUP added a 100-LOC recovery fn to an already-overgrown module; the matching read path lives elsewhere, hiding the recovery contract.
- **Action**: extract `crates/sandbox/src/db_recovery.rs` housing both the recovery CAS + the read_snapshot_row reader + the sweep's takeover loop's pg surface. Test: lib unaffected; pg-gated tests follow the new module path.

### [R10-A2] `RestoreBackend` trait grew to 7 methods; 3 are 1-line delegations (CRITICAL, architecture-r10)
- **Source**: 2026-05-25 architecture-r10
- **File**: `crates/sandbox/src/restore_handler.rs::RestoreBackend` trait
- **Symptom**: trait has 7 methods after B19 + B22 + R7-S2 + R8-A3-5; `RealRestoreBackend` holds 2 `Arc<NomadCHBackend>`-typed fields; 3 of 7 methods are 1-line delegations to the inner Arc. The trait is increasingly a facade over NomadCHBackend; pretends to be polymorphic but isn't.
- **Action**: merge with the proposed `SnapshotCapableBackend` trait (from R3-A1/R5-A1) so the wake path uses ONE trait surface. The 3 delegations collapse. Subsumes R5-A1 / R3-A1 / R5-A2 if the merge is taken together.

### [R10-A3] Backend enum still has 5 Err-returning methods after 3 rounds (CRITICAL, architecture-r10 — confirms R3-A1/R5-A1)
- **Source**: 2026-05-25 architecture-r10 audit (inertia confirmation)
- **File**: `crates/sandbox/src/backend/mod.rs` (enum surface)
- **Symptom**: 5 methods still return `Err("backend X doesn't support …")` for 2/3 variants. R3-A1 (r3), R5-A1 (r5), now confirmed at r10. The "small additive fix" pattern has not slowed; structural fix is overdue.
- **Action**: split `SnapshotCapableBackend` trait. Only NomadCH impls it. Use `&dyn SnapshotCapableBackend` at call sites needing the surface. Combine with R10-A2's RestoreBackend merge as ONE PR.

### [R10-A4] nomad_ch.rs at 4923 LOC with a single 459-LOC `create()` (IMPORTANT, architecture-r10)
- **Source**: 2026-05-25 architecture-r10
- **File**: `crates/sandbox/src/backend/nomad_ch.rs`
- **Symptom**: one of the two heaviest modules in the crate. Single `create()` fn is 459 LOC.
- **Action**: 8-child module split sketched in r10 report — `nomad_ch/{create,stop,restore,jobspec,allocator,state,ch_remote,tests}.rs`. PR can be staged file-by-file (each child split is one commit).

### [R10-A5] (CLOSED) error_envelope.rs duplication between sandbox + sandbox-agent is structurally justified
- **Source**: 2026-05-25 architecture-r10
- **Finding**: divergence is real — sandbox has `extra`/`no_store` helpers, agent doesn't. Wire shape identical, code paths different. Keep duplicated. Closes the r8-A4 "extract to zeroship-core?" carry.

### [R10-A7] T9 + T10 confirmed: ControllerIdleSnapshotter dup of admin_handlers (IMPORTANT, architecture-r10)
- **Source**: 2026-05-25 architecture-r10
- **Files**: `crates/sandbox/src/sweep.rs:283-407` + `crates/sandbox/src/admin_handlers.rs:1109-1206`
- **Status**: r10 confirms T9 + T10 (open since r2). Two orchestrators with literally-duplicated `ResolvedSourceVmOps` adapters.
- **Action**: extract `crates/sandbox/src/snapshot_orchestrator.rs` per T9. Closes T9 + T10 + R10-A7 together.

### [R10-API1] `_test_build_auth_from_sealed` is pub but has zero callers (MINOR, api-surface-r10)
- **Source**: 2026-05-25 api-surface-r10
- **File**: `crates/sandbox/src/restore.rs:613`
- **Symptom**: orphan pub fn. Possibly stale test helper from earlier scaffolding.
- **Action**: delete or move to `#[cfg(test)]`-gated test helper module.

### [R10-API2] `ExecBody` + `not_found` in sandbox-agent are over-pub'd (MINOR, api-surface-r10)
- **Source**: 2026-05-25 api-surface-r10
- **Files**: `crates/sandbox-agent/src/handlers.rs` (locate via grep `pub struct ExecBody\|pub fn not_found`)
- **Symptom**: zero cross-crate consumers; same anti-pattern R8-API1 closed.
- **Action**: pub→pub(crate). Mechanical.

### [R10-API3] (CLOSED at `f50c95da`, partial) persist.rs had 5 module-level `pub fn`s with only in-crate callers (MINOR, api-surface-r10)
- **Source**: 2026-05-25 api-surface-r10 (4-round api-surface carry through r11/r12)
- **File**: `crates/sandbox/src/persist.rs` — `seal`, `unseal_one`, `unseal_dir`, `seal_filename_for`, `seal_filename_for_str`
- **Symptom**: bypassed the `Persistence` handle's discipline. Pub on a stable boundary not justified externally.
- **Fix (f50c95da)**: pub→pub(crate) on the 3 with zero external callers (`seal`, `unseal_dir`, `seal_filename_for_str`). Verified via `grep -rn 'persist::{seal\b,unseal_dir,seal_filename_for_str}'` across `crates/sandbox/src/`, `crates/sandbox/tests/`, `crates/sandbox-agent/src/` — zero matches outside `persist.rs` itself.
- **Out of scope**: `unseal_one` + `seal_filename_for` stay `pub` — externally consumed by `crates/sandbox/tests/sandbox_preview_share_e2e.rs` (`unseal_one` at :1034,1065,1079,1114; `seal_filename_for` at :1058,1112). Documented in the R11 partial-invalidation note below.
- **Verification post-fix**: `cargo check -p zeroship-sandbox` clean (1 dead-code warning on `seal_filename_for_str` — only referenced under `#[cfg(test)]`, expected since pub(crate) lets the compiler see the prod-build callgraph). `cargo test -p zeroship-sandbox --lib` → 327 passed / 0 failed / 1 ignored (unchanged from HEAD). E2e test still compiles.

### [R10-API4] `readyz` returns `{"status":"draining"}` rather than §10.0 envelope (MINOR, api-surface-r10)
- **Source**: 2026-05-25 api-surface-r10
- **Symptom**: liveness/readiness probes drift from §10.0 ErrorEnvelope. Arguably justified as a probe shape — but document the carve-out explicitly.
- **Action**: either bring readyz in line with §10.0 OR add a comment + invariant test pinning the probe-shape decision.

### [R10-Q1] (CLOSED at `228569d3`) handlers.rs:670/821/837 raw `{e}` leak — was a 7-round carry, sanitized via shared `err_safe()`
- **Source**: 2026-05-25 code-quality-r10 (also tracked as a 7-round api-surface carry across earlier review rounds)
- **Files**: `crates/sandbox/src/handlers.rs:670,821,837` (3 substitutions) + `crates/sandbox/src/admin_handlers.rs:228` (bumped `err_safe` from `fn` → `pub(crate)` so handlers.rs can share the single sanitizer — option (a) from the brief: no new sibling module, no duplication)
- **Fix**: substituted `err(500, "...", format!("...: {e}"))` → `err_safe(500, "...", "<public_msg>", e)`. Raw driver-error strings (ch-remote socket paths, agent IPs, mount paths, uids) now go to journald via `tracing::error!` only; wire body carries fixed public prose (`"backend stop failed"`, `"backend exec failed"`, `"backend file-tree failed"`).
- **Tests added**: 3 wire-shape tests in `handlers::tests` (`r10_q1_backend_{stop,exec,file_tree}_sanitizes_raw_driver_error`) feed sentinel-bearing raw errors and assert (a) wire `message` is the fixed public string and (b) sentinel substrings are absent. Sandbox lib: 305 → 308.
- **Verification post-fix**: `grep -nE 'err\(500.*\{e\}' crates/sandbox/src/*.rs` is empty — no other raw-leak sites of this shape remain in the crate.

### [R10-Q5] (CLOSED at `0cc7af52`) proxy.rs `_ref_imports` dead-by-design fn deleted (3-round carry: r9 #6 → r10 → r11 → r12)
- **Source**: 2026-05-25 code-quality-r10 (carry from r9 #6, surfaced again in r11 and r12)
- **File**: `crates/sandbox-agent/src/proxy.rs:547-561` (pre-fix)
- **Symptom**: 15-line `#[allow(dead_code)] fn _ref_imports() { … }` block whose entire purpose was "keep `sig` / `HeaderName` / `Uri` / `CanonicalKind` imports legal for a future rev." A comment in code form. Zero callers across `crates/sandbox-agent/src/` and `crates/sandbox/src/`.
- **Fix**: deleted the function + its 3-line comment block + `#[allow(dead_code)]` attr. Audited which imports actually became unused: `use ntex::http::header::HeaderName;` (only used by the dead fn) and the `, Uri` half of `use ntex::http::{StatusCode, Uri};` were dropped at module scope. `use crate::sig::{self, CanonicalKind};` moved from module scope into `#[cfg(test)] mod tests { … }` since the only remaining callers (`sig::sign_kind` at the v1.1 sign helper, `CanonicalKind::V1_1` at the same site, `crate::sig::sign` in the v1-on-proxy reject test) all live inside the test module and reach the parent's `use` solely via `use super::*`.
- **Verification**: `cargo check -p zeroship-sandbox-agent --tests` clean (0 warnings, 0 errors). `cargo test -p zeroship-sandbox-agent --lib` → 242 passed / 0 failed / 0 ignored — no test count change.
- **Net diff**: -19 / +2 (net -17 LOC; brief estimated 14 LOC delete — close, the extra few lines come from the two module-scope import lines we had to clean up plus the 1 line added inside the tests module for `use crate::sig::{self, CanonicalKind};`).

### [R10-Q3] registry.rs has 35 bare `RwLock::{read,write}().unwrap()` sites with no poison-recover (MAJOR, code-quality-r10)
- **Source**: 2026-05-25 code-quality-r10
- **File**: `crates/sandbox/src/registry.rs` (35 sites); also k8s.rs (10) and docker.rs (6)
- **Symptom**: 42 sites elsewhere in the crate use `unwrap_or_else(|p| p.into_inner())` for poison-recover. `registry.rs` is the hot-path `SandboxRegistry` shared by every HTTP request — any panic anywhere in the crate that crosses an RwLock leaves these sites panicking instead of recovering.
- **Action**: bulk-replace `.unwrap()` → `.unwrap_or_else(|p| p.into_inner())` on RwLock {read,write} in registry.rs (35) + k8s.rs (10) + docker.rs (6). Wrap in a `lock_recover!` macro to keep the call sites short.

---

## NEW r10/r11 ROUND FINDINGS (added by pilot cycle 2026-05-25 r5 — security r10, perf r11, code-quality r11, api-surface r11, test-coverage r10, concurrency r11)

### [R11-P1] (CRITICAL, performance-r11) Every `Database` method opens a fresh pg Pool per call
- **Source**: 2026-05-25 performance-r11
- **Files**: `crates/sandbox/src/db.rs:492-514` (`open_pool()` called from every method) + `crates/compio-postgres/src/pool.rs:264-300` (fresh TCP+STARTUP+auth handshake per connect)
- **Symptom**: TODO comment at db.rs:494-507 documents the pattern but no prior perf round flagged it. Wake-path pays 5× per restore (`get_sandbox_row`, `read_snapshot_row`, `update_sandbox_status` ×2, `clear_snapshot_metadata`); transient-takeover sweep pays 1+N per tick. Largest low-effort lever after R9-P1. Estimated savings: ~10-75 ms median per wake.
- **Action**: cache the pool. Either a `OnceLock<Pool>` per Database struct OR an `Arc<RwLock<Option<Pool>>>` with lazy init. The pool already supports max_connections; we just need to stop re-creating it.

### [R11-P2] (IMPORTANT, performance-r11) `download_to_disk` no BufWriter — 131072 write(2) calls per 1 GB — CLOSED at 3d5c527f
- **Source**: 2026-05-25 performance-r11
- **File**: `crates/sandbox/src/snapshot_store_gcs.rs:438-445`
- **Symptom**: `std::io::copy(reader, file)` defaults to 8 KiB buffer. 1 GB download = ~131072 write(2) syscalls. Wake-path counterpart to R10-P5 at 16× higher syscall granularity.
- **Action**: wrap dest in `BufWriter::with_capacity(1 << 20, …)`. 1-line fix.
- **Resolution (3d5c527f)**: `download_to_disk` now wraps the destination File in `std::io::BufWriter::with_capacity(1 << 20, f)` around `io::copy`; `into_inner()` flushes the BufWriter before `sync_all()` so all bytes are observed on disk. Tests: sandbox lib 310 (unchanged — perf-only).

### [R11-P3] (IMPORTANT, performance-r11) Two SHA helpers lack the BufReader R5-P1 added — CLOSED at 3d5c527f
- **Source**: 2026-05-25 performance-r11
- **Files**: `crates/sandbox/src/snapshot_store_gcs.rs:966-997` (`canonical_artifact_sha256`) + `:506-521` (`sha256_file`)
- **Symptom**: R5-P1 added a 1 MiB `BufReader` on `snapshot_store.rs`'s SHA loop, but the GCS adapter has its own 2 SHA helpers that never got the same treatment. 16× syscall amplification on the 5-pass disk walk (R10-P1).
- **Action**: 2-line fix in each helper. Wrap the `File` in `BufReader::with_capacity(1 << 20, …)` before the SHA loop.
- **Resolution (3d5c527f)**: both `sha256_file` and `canonical_artifact_sha256` now wrap the input File in `std::io::BufReader::with_capacity(1 << 20, f)` before the SHA chunk loop, mirroring R5-P1's pattern at 77ea717f byte-for-byte (same canonical hash domain). Tests: sandbox lib 310 (unchanged — perf-only).

### [R11-P4] (MINOR, performance-r11) `sweep.rs::attempted` Vec clones SandboxRow unnecessarily
- **Source**: 2026-05-25 performance-r11
- **File**: `crates/sandbox/src/sweep.rs:524-526`
- **Action**: change `attempted` to `Vec<TypedId>` (or just an integer counter) since consumers only want `len()` and the id.

### [R11-Q2] (IMPORTANT, code-quality-r11) R9-S4/4b/4c/4d copy-pasted uid+mode+read logic — extract helper
- **Source**: 2026-05-25 code-quality-r11
- **Files**: `crates/sandbox/src/snapshot_aead.rs::RootKek::from_path` (R9-S4 cca1e74d) + `crates/sandbox/src/persist.rs::AeadKey::from_path` (R9-S4b e4e5db60) + `crates/sandbox/src/db.rs::enforce_password_file_mode` (R9-S4c 2c10f63a) + `crates/sandbox/src/lib.rs::load_admin_token` (R9-S4d pending)
- **Symptom**: 4 sites with byte-for-byte identical pattern (stat → check mode 0o400 → check uid == 0 → length-check → read). Any future fifth secret-loader will inherit the same pattern.
- **Action**: extract `pub(crate) fn read_root_owned_secret_file(path: &Path, expected_len: usize) -> Result<Vec<u8>, String>` (or similar). Move the 4 sites onto it after R9-S4d closes.

### [R11-Q3] (MINOR, code-quality-r11) sandbox-agent JSON-parse paths leak serde_json::Error via raw format
- **Source**: 2026-05-25 code-quality-r11
- **Files**: `crates/sandbox-agent/src/handlers.rs:592,777`
- **Symptom**: `format!("invalid JSON body: {e}")` exposes structural details of the body parser. 400-class so low signal, but the shape `err_safe` was built for.
- **Action**: route through a sanitizing helper analogous to the controller-side `err_safe`. Could share the helper from R10-A5's discussion (or keep duplicated per R10-A5's resolution).

### [R11-Q4] (MINOR, code-quality-r11) R9-S4b new test fns lack doc-comments — mixed convention
- **Source**: 2026-05-25 code-quality-r11
- **File**: `crates/sandbox/src/persist.rs:1072,1109`
- **Action**: add `///` lines.

### [R11-Q5] (MINOR, code-quality-r11, 7-round carry of R5-Q1) `register_restored` default `Ok(())` is a silent fail-OPEN
- **Source**: 2026-05-23 code-quality-r5; carried through r11
- **File**: `crates/sandbox/src/restore_handler.rs:162-170` (trait default)
- **Symptom**: 7th-round carry; be246395's `unregister_restored` makes the silent fail-OPEN more consequential — if a future `RestoreBackend` impl forgets to override, the recovery state-map invariant breaks silently.
- **Action**: remove the default impl; make `register_restored` required. Or have the default `panic!()` so a missing impl fails loudly.

### [R11-API1] (MINOR, api-surface-r11) Orphan `#[doc(hidden)] pub fn` test accessors in metrics.rs
- **Source**: 2026-05-25 api-surface-r11
- **File**: `crates/sandbox-agent/src/metrics.rs:223,229` (`takeover_corrupt_value`, `sandbox_corrupt_id_value`)
- **Symptom**: zero callers anywhere. Same flavor as R10-API1's `_test_build_auth_from_sealed`.
- **Action**: delete or move to `#[cfg(test)]`. Cluster with R10-API1 in a single sweep.

### [R10-API3 PARTIALLY INVALIDATED — only 3 of 5 `persist::*` pub fns are safely demotable] (CLOSED at `f50c95da`)
- **Source**: 2026-05-25 api-surface-r11 audit re-verified r10's claim
- **Files**: `crates/sandbox/src/persist.rs`
- **Resolution**: `unseal_one` + `seal_filename_for` ARE externally consumed by `tests/sandbox_preview_share_e2e.rs:151,1064,1114` — must stay `pub`. Only `seal`, `unseal_dir`, `seal_filename_for_str` are safely demotable.
- **Action**: when picking up R10-API3, only demote those 3.
- **Fix (f50c95da)**: 3 fns demoted pub→pub(crate); 2 stayed pub per the r11 carve-out. See the R10-API3 closure entry above for full verification record.

### [R10-S1] (IMPORTANT, security-r10) R9-S4 KEK uid check has symlink-attack residual
- **Source**: 2026-05-25 security-r10
- **File**: `crates/sandbox/src/snapshot_aead.rs::RootKek::from_path` (R9-S4 closure at cca1e74d)
- **Symptom**: R9-S4 used follow-symlinks `std::fs::metadata` + `std::fs::read`. Non-root attacker with symlink-create access + operator-typo'd KEK path can silently redirect to attacker-controlled file (which the attacker can chown root).
- **Action**: `symlink_metadata` refusal OR `O_NOFOLLOW` + `fstat` on the same fd. Apply to all 4 sites (R9-S4/4b/4c/4d) when refactoring via R11-Q2.

### [R10-S2] (IMPORTANT, security-r10) R10-C2 spawn_blocking discards JoinError — panic silently swallowed
- **Source**: 2026-05-25 security-r10
- **File**: `crates/sandbox/src/restore_handler.rs:294-298` (R10-C2 closure at be246395)
- **Symptom**: the `let _ = compio::runtime::spawn_blocking(move || …).await;` form drops the JoinError. A panic inside teardown_restore → unregister_restored or release_vm_index is silenced, leaving ghost state.
- **Action**: match the Err arm; tracing::error! the join error. 4-line fix.

### [R10-S3] (MINOR, security-r10) teardown_restore releases vm_index even when Nomad DELETE fails
- **Source**: 2026-05-25 security-r10
- **Action**: re-order: only release vm_index after the DELETE returns Ok. Or: persist the vm_index as pending-release and let a sweep retry the DELETE.

### [R10-S4] (MINOR, security-r10, duplicates R10-Q3) registry.rs RwLock unwrap pattern inconsistency

### [R10-S5] (MINOR, security-r10, duplicates R9-S3) AEAD posture leak via pg snapshot_aead_dek_id

### [R10-S6] (MINOR, security-r10, duplicates R9-T7-FOLLOWUP) read_sandbox_id_from_sources no UUID-shape guard

### [R10-T1] (CRITICAL, test-coverage-r10) R10-C1+C2 tests structural-only — bypass restore_sandbox
- **Source**: 2026-05-25 test-coverage-r10
- **File**: `restore_handler.rs:2235,2308` (R10-C1+C2 integration tests at be246395)
- **Symptom**: both new integration tests call `backend.teardown_restore(...)` DIRECTLY; never drive the rollback closure at `:263-313` where the fix actually lives. R10-C2 "test" is a pure `include_str!` text-grep with no behavioural assertion. The brief explicitly asked for >100ms-teardown behavioural test — doesn't exist.
- **Action**: write a true end-to-end test that triggers do_restore_inner → CasLost → teardown_restore. Assert: state map empty post-teardown, vm_index released, no ghost via subsequent register_restored at same vm_index.

### [R10-T2] (IMPORTANT, test-coverage-r10, re-files R9-T1) Persist(Some(_)) chain still uncovered
- **Source**: 2026-05-25 test-coverage-r10 (4-round carry of R9-T1)
- **File**: `restore_handler.rs:581-617`
- **Symptom**: r10 cycle shipped +12 tests in real_backend_tests; none drives the production `unseal → clock_resync → register_restored` ladder. Every pg `restore_sandbox` call still passes `persist=None`.

### [R10-T3] (IMPORTANT, test-coverage-r10) R9-T7 read_sandbox_id lacks cross-source test
- **Source**: 2026-05-25 test-coverage-r10
- **File**: `crates/sandbox-agent/src/handlers.rs::read_sandbox_id_from_sources`
- **Symptom**: edge case "env=empty + file=valid" (where empty env wins via Ok("") at handlers.rs:139-150) is unpinned.
- **Action**: add a test that sets env="" + file=valid, asserts empty wins (or fix the production semantic).

### [R10-T4] (IMPORTANT, test-coverage-r10) R9-S4's from_env production entry not covered under the uid invariant
- **Source**: 2026-05-25 test-coverage-r10
- **Action**: add a test for `from_env` (not just `from_path`) — the env-driven entry should inherit the same uid check.

### [R10-T5] (MINOR, test-coverage-r10) R10-P2 Tiered::put L2 spawn_blocking has no behavioural test
- **Action**: add a test asserting L1 completes before L2 joins + a panic-isolation test (panic in L2 doesn't poison L1 result).

### [R11-C1] (CRITICAL, concurrency-r11) teardown_restore unregister conditional on nomad_handle.is_some() — silent-fail-OPEN footgun
- **Source**: 2026-05-25 concurrency-r11
- **File**: `crates/sandbox/src/restore_handler.rs:1189-1199` (R10-C1 fix at be246395)
- **Symptom**: if wiring regresses (or a test/stub path leaks into prod), state-map cleanup is silently skipped — same R10-C1 ghost the round-10 fix was supposed to close. The R10-C1 fix itself ships the footgun.
- **Action**: invert the condition — error out if nomad_handle is None during teardown_restore in a production wake path. Or: make the field non-optional in RealRestoreBackend.

### [R11-C2] (CRITICAL, concurrency-r11) R10-C2 spawn_blocking introduced new 2-await drop window
- **Source**: 2026-05-25 concurrency-r11
- **File**: `crates/sandbox/src/restore_handler.rs:294-298` (R10-C2)
- **Symptom**: future-drop between the spawn_blocking join + the pg row update runs the blocking-pool teardown to completion BUT skips the pg `update_sandbox_status` — row stays `Restoring` for ≤120s until sweep takes over. Compounds with [R11-C1].
- **Action**: subsumed by R4-A2 LeasedVmSlot RAII. Now incident-class — 6+ cycles open.

### [R11-C2 retraction] r10's `snapshot_handler.rs:424 vm_ops.teardown_source` sync-on-async claim — RETRACTED
- **Source**: 2026-05-25 concurrency-r11 verification
- **Resolution**: both production `ResolvedSourceVmOps::teardown_source` impls return `Ok(())` unconditionally — it's a no-op stub, not a sync-on-async call. The real teardown runs from the admin/sweep async layer.

### [R7-API2] (CLOSED at c8000537) — see /commit log

---

## NEW r11 ROUND FINDINGS (added by pilot cycle 2026-05-25 r6 — arch r11, test-cov r11, security r11)

### [R11-A1] (CRITICAL, architecture-r11, dup of R11-Q2) 4-site root-owned-secret-file loader duplication
- **Source**: 2026-05-25 architecture-r11
- **Files**: `crates/sandbox/src/snapshot_aead.rs:185` (R9-S4 closed), `persist.rs:333` (R9-S4b closed), `db.rs:824` (R9-S4c closed), `lib.rs:924` (R9-S4d closed at `b4c3ef27`)
- **Symptom**: R9-S4 trio widened duplication 3→4 across the cycle. All 4 sites carry identical mode+uid+length+read+map_err logic. 6 near-identical tests across 3 files; R9-S4d added 2 more.
- **Action**: extract to new `crates/sandbox/src/secret_io.rs` with `pub(crate) fn read_root_owned_secret_file(env_name, path, expected_len) -> Result<Vec<u8>, String>`. All 4 callers migrate. Consolidates the 8+ tests into one place. Closes R11-Q2 + R11-A1 in single PR.

### [R11-A2] (IMPORTANT, architecture-r11) T-7 jobspec parallel-mode will fragment across 6 zones in nomad_ch.rs post-T-8
- **Source**: 2026-05-25 architecture-r11 (read uncommitted T-7 working-tree state)
- **Files**: `crates/sandbox/src/backend/nomad_ch.rs:2213-2464` (T-7 adds `TaskDriverMode { RawExec, ChPlugin }` + 65 LOC parallel jobspec block; +241/-89 in working tree per test-cov r11)
- **Symptom**: post-T-8 (bash wrapper removed) the cleanup spans 6 zones across the 5072-LOC file — surgical deletion across all 6. Architectural recommendation: land R10-A4 module split BEFORE T-8 so the deletion becomes `rm jobspec/rawexec.rs`.
- **Action**: stage R10-A4 nomad_ch.rs split as a hard prerequisite for T-8. PR ordering: R10-A4 → T-7 lands → T-8 cluster validation → T-8 wrapper removal.

### [R11-A3] (IMPORTANT, architecture-r11, 8th-cycle carry of R4-A1) AppState builder still 7 with_* + new_fixture; R8-API1 wrapper resolved into sandbox-agent (orthogonal)
- **Source**: 2026-05-25 architecture-r11
- **Resolution clarification**: r10's open question whether R8-API1's `boot_init_sandbox_id` would clean up AppState builder — NO. The wrapper lives in `crates/sandbox-agent/src/lib.rs`, not controller AppState. R4-A1 unchanged.

### [R11-A4] (IMPORTANT, architecture-r11) C1-FOLLOWUP's `claim_orphan_transient_for_recovery` should move to recovery.rs
- **Source**: 2026-05-25 architecture-r11
- **File**: `crates/sandbox/src/db.rs:2520` + `restore_handler.rs:332` (`read_snapshot_row`)
- **Symptom**: db.rs grew +105 LOC since C1-FOLLOWUP landed; recovery semantics now span db.rs (3108 LOC) AND restore_handler.rs. Recovery is its own bounded context.
- **Action**: extract `crates/sandbox/src/recovery.rs` housing `claim_orphan_transient_for_recovery` + `read_snapshot_row` + `transient_state_lease_expired_sandboxes`. db.rs shrinks to schema/CRUD only.

### [R11-A5] (CLOSED) Cross-crate ErrorEnvelope drift since r10: zero
- **Source**: 2026-05-25 architecture-r11
- **Resolution**: confirmed no field drift between sandbox + sandbox-agent envelope definitions since r10. R10-A5 verdict (keep duplicated) stands.

### [R11-T1] (CRITICAL, test-coverage-r11) R10-T1 unaddressed + new 8th spawn_blocking site without panic handling
- **Source**: 2026-05-25 test-coverage-r11
- **Files**: `crates/sandbox/src/restore_handler.rs:294` (R10-C2 spawn_blocking; `let _ = …await` discards JoinHandle without panic-conversion — 8th spawn_blocking site, only one without explicit panic handling)
- **Symptom**: R10-T1 (R10-C1+C2 integration tests structural-only) now 3rd-round critical. Combined with this 8th panic-recovery gap, the integration coverage hole is materially worse than r10 reported.
- **Action**: subsumed by R10-T1's mandate to write a true do_restore_inner → CasLost rollback integration test.

### [R11-T2] (CRITICAL, test-coverage-r11, 3rd-round carry) Persist(Some(_)) chain still untested
- **Source**: 2026-05-25 test-coverage-r11 (continuation of R9-T1, R10-T2)
- **File**: `crates/sandbox/src/restore_handler.rs:581-617`
- **Symptom**: all 5 pg_e2e callsites of `restore_sandbox` still pass `persist=None`. 3 cycles without progress on the integration-through-`restore_sandbox` requirement.
- **Action**: integration test that constructs `RealRestoreBackend::with_nomad_handle(...)`, populates `persist` with sealed record + fake snapshot artifact, calls `restore_sandbox`, asserts unseal + clock_resync_post_restore + register_restored all invoked with correct args.

### [R11-T3] (IMPORTANT, test-coverage-r11) R7-API2 capability pin covers only 1 of 3 tiers — CLOSED eb26db31
- **Source**: 2026-05-25 test-coverage-r11
- **File**: `crates/sandbox-agent/src/version.rs` + the new `mandatory_clock_resync_v1_present` test
- **Symptom**: `proxy.ws-v1` + `auth.ed25519-v1.1` (named as "feature-detected" by R7-API2 commit body) have no equivalent regression pins.
- **Action**: add 2 more capability-presence tests in the same module.
- **Resolution (eb26db31)**: Added `mandatory_proxy_ws_v1_present` and `mandatory_auth_ed25519_v1_1_present` mirroring the R7-API2 (c8000537) pattern. Const-presence asserts only, no wire-shape touch. Sandbox-agent lib tests 240 → 242.

### [R11-T4] (IMPORTANT, test-coverage-r11, drift evidence for R9-T6) derive_agent_url already drifted
- **Source**: 2026-05-25 test-coverage-r11
- **Files**: `crates/sandbox/src/restore_handler.rs:1153` (hardcodes `7777`) + `crates/sandbox/src/backend/nomad_ch.rs:1532` (uses `AGENT_PORT` const)
- **Symptom**: R9-T6 predicted silent drift; it's already there. Two formulas not byte-equivalent in their dependency.
- **Action**: parity test would fail today; one-line fix (use `AGENT_PORT` const in both) + the test pins the invariant.

### [R11-T5] (MINOR, test-coverage-r11) R9-S4 family test fragmentation — extract helper first (now retro)
- **Source**: 2026-05-25 test-coverage-r11
- **Status**: R9-S4d shipped before this recommendation could be applied. Now 8 near-identical tests across 4 files. R11-Q2/R11-A1 helper extract would consolidate going forward.

### [R11-S1] (CRITICAL, security-r11, elevation of R9-S4d) — CLOSED at b4c3ef27 — admin token uid check landed

### [R11-S2] (MINOR, security-r11) `host_id` file at db.rs:1094-1105 reads with no mode/uid/shape validation — CLOSED at 85e4f2f9
- **Source**: 2026-05-25 security-r11
- **File**: `crates/sandbox/src/db.rs:1094-1105` (reader) + `:1129-1138` (writer emits 0o600)
- **Symptom**: writer is correct (0o600 mode); reader is symmetric-free. HA peer-identity spoofing → bypass of `claim_orphan_transient_for_recovery` self-host_id fence.
- **Action**: add a mode + uid check on the reader. Mode 0o600 (matches writer); uid==0 (or relax to euid since the controller writes its own — match the writer's authority).
- **Fix (85e4f2f9)**: `enforce_host_id_file_mode` helper added in db.rs (mirrors `enforce_password_file_mode` shape at db.rs:812); reader path gated on `metadata().mode() == 0o600` + `metadata().uid() == 0`. 3 new tests mirror R9-S4c naming: `host_id_read_rejects_loose_permissions`, `host_id_read_rejects_non_root_owned_file`, `host_id_read_accepts_root_owned_0o600_file_when_running_as_root`. Two existing positive-arm tests (`from_env_generates_host_id_when_absent`, `from_env_loads_host_id_from_persistent_file`) gated to skip when non-root, matching the R9-S4 family idiom. Sandbox lib tests: 319 → 322.

### [R11-S3] (MINOR, security-r11, posture) chunk_aad missing sandbox_id + snapshot_taken_at
- **Source**: 2026-05-25 security-r11
- **File**: `crates/sandbox/src/snapshot_aead.rs:324-330` (`chunk_aad` returns `b"zsbx-snap" || chunk_index_le`)
- **Symptom**: Poly1305 AAD doesn't bind sandbox_id or snapshot_taken_at. Amplifies R9-S2: under DEK collision (same-second re-snapshot), AAD without these fields means there's no last-line fence against cross-snapshot chunk-swap.
- **Action**: bind both into AAD. Wire-incompatible — needs a version bump (header `cipher_id="v2"` if AAD shape changes).

### [R11-Q2 / R11-A1] (UNIFIED) Extract `read_root_owned_secret_file` helper — closes 4 sites' duplication + 8+ tests' fragmentation

### [R11-P2 + R11-P3] (CLOSED at 3d5c527f) BufReader/BufWriter on GCS download + SHA helpers

---

## NEW r12 ROUND FINDINGS (added by pilot cycle 2026-05-25 r7 — code-quality r12, api-surface r12, concurrency r12)

### [R12-I1] (CRITICAL — T-8 blocker, concurrency-r12) Wake path hardcodes `Driver: "raw_exec"` — split-brain on T-8 cutover — CLOSED at `b3bf741c`
- **Source**: 2026-05-25 concurrency-r12
- **File**: `crates/sandbox/src/restore_handler.rs:1323` (`build_restore_nomad_job_json` hardcodes `"Driver": "raw_exec"`); does NOT call `task_driver_mode_from_env()` like the cold-boot builder at `nomad_ch.rs::build_nomad_job_json_with`.
- **Symptom**: under T-8's `SANDBOX_TASK_DRIVER=ch_plugin` cutover + bash-wrapper removal, CREATEs go through the Go plugin but RESTOREs still try `raw_exec` → split-brain on the same `vm_index` slot. With the wrapper removed, every wake fails with "no such driver: raw_exec" or "wrapper not found".
- **Action**: collapse the two builders into a single `build_nomad_job_json_with(... restore_from: Option<&Path>, mode: TaskDriverMode)` that T-7 already designed for; or add the env consultation + ChPlugin branch to `build_restore_nomad_job_json`. The merge approach is cleaner and closes the R10-A4 nomad_ch.rs split for free.
- **Blocks**: T-8b-cutover. T-8b-stress should also be done with this fix in place, otherwise stress results are invalid.
- **Resolution (`b3bf741c`)**: approach (b) — added `TaskDriverMode` arg to `build_restore_nomad_job_json` mirroring the cold-boot builder's RawExec/ChPlugin match-on-mode. Production caller `submit_restore_job` reads `task_driver_mode_from_env()` once per submit. Wake-path under ChPlugin emits `Driver: "ch"` + typed Config with `restore_from = <alloc_dir>`. 5 new tests pin both modes (lib 322 → 327). Approach (a) merge deferred — the wake builder's substantive divergence (no `ZSBX_SANDBOX_ID`/`ZSBX_PUBKEY_HEX`, restore-specific Meta, externally-passed memory/cpus to match snapshot-saved values) made the field-set merge non-trivial. The remaining R10-A4 builder-duplication carry stays open; this PR closes the T-8 blocker only.

### [R12-M1] (MINOR, concurrency-r12) Rollback closure 3-layer silent-fail compounded (R10-S2 + R11-C1 + R11-C2 + R12-M1)
- **Source**: 2026-05-25 concurrency-r12
- **File**: `crates/sandbox/src/restore_handler.rs:294-298` (R10-C2 spawn_blocking)
- **Symptom**: 3 other spawn_blocking sites in the same file use `unwrap_or_else(|p| Err(format!("spawn_blocking panic: {p:?}")))`. The rollback path uses `let _ = …await` — JoinError on panic silently discarded. Compounds R10-S2 + R11-C1 (silent-fail-OPEN footgun) + R11-C2 (new 2-await drop window) into a three-layer silent-fail stack.
- **Action**: match the other 3 sites' pattern. 4-line edit.

### [R12-M2] (MINOR, concurrency-r12) task_driver_mode_from_env called per-CREATE — should cache at NomadCHBackend construction
- **Source**: 2026-05-25 concurrency-r12
- **File**: `crates/sandbox/src/backend/nomad_ch.rs` (T-7 call site)
- **Symptom**: theoretical env-Mutex micro-contention + footgun if env mutates mid-flight. Not a load-bearing perf bug, but the right pattern is read-at-startup.
- **Action**: store TaskDriverMode in NomadCHBackend struct, read once at construction. T-7's tests already use a `T7_ENV_LOCK` mutex — adapt to set the field, not the env, in tests.

### [R12-Q1] (CLOSED at `46e0fa2a`) `Database::open_pool` TODO refreshed
- **Status**: **CLOSED**. R12-Q1 fixer rewrote the comment at `crates/sandbox/src/db.rs:492-508` (post-edit line range). The stale "next round picks it up" promise is gone; the comment now points to R11-P1 in this file as the tracking entry and names the actual blocker (`compio_postgres::Pool` is `!Send` + `!Sync`, so the cache must be a per-compio-worker `thread_local!`, not an `Arc`/`OnceLock`). Comment-only change; no test impact (327 passed / 0 failed). R11-P1 itself remains OPEN — the actual pool-cache refactor is its scope.

### [R12-Q2] (MINOR, code-quality-r12) T-7 magic strings unextracted
- **Source**: 2026-05-25 code-quality-r12
- **File**: `crates/sandbox/src/backend/nomad_ch.rs` (T-7 additions)
- **Symptom**: `"raw_exec"`, `"ch"`, `"ch_plugin"`, `"SANDBOX_TASK_DRIVER"` literals not extracted to consts. Env-value match arm silently fallible on typo (defaults to RawExec).
- **Action**: extract to module-level consts. 4-line addition.

### [R12-Q3] (MINOR, code-quality-r12) ENV_LOCK pattern duplicated (T-7 vs db.rs::tests)
- **Source**: 2026-05-25 code-quality-r12
- **Action**: 2nd copy — not urgent until 3rd.

### [R12-API1] (MINOR, api-surface-r12) `readyz` sibling of R10-API4 in handlers.rs
- **Source**: 2026-05-25 api-surface-r12
- **File**: `crates/sandbox/src/handlers.rs:132-139`
- **Symptom**: emits `{"status":"backend-unhealthy"}` on 503 — only `HttpResponse::ServiceUnavailable` site bypassing `error_envelope`. Cluster with R10-API4 (controller-side `readyz`) for single decision.
- **Action**: either bring both to §10.0 envelope OR document the probe-shape carve-out with invariant tests.

### [R12-API2] (TRIVIAL, api-surface-r12) stale `db.rs` "hyphenated form" comment shifted to line 2859 (was 2839)
- **Action**: 30-char edit. 4th-round carry.

### Closed this cycle:
- [R11-T3] CLOSED at `eb26db31` — capability presence pins for proxy.ws-v1 + auth.ed25519-v1.1
- [R11-S2] CLOSED at `85e4f2f9` — host_id reader mode+uid check (with defense-in-depth bonus: original silently regenerated on any read failure; new path surfaces permission errors as Validation)
- [R12-I1] CLOSED at `b3bf741c` — wake-path SANDBOX_TASK_DRIVER feature flag (T-8 blocker; split-brain CREATE-vs-RESTOREs eliminated)

---

## NEW r12 ROUND 2 FINDINGS (added by pilot cycle 2026-05-25 r8 — architecture r12, test-coverage r12, performance r12)

### [R12-A1] (CRITICAL, architecture-r12) R12-I1 approach (b) produced byte-for-byte builder duplication + UNSYNCHRONIZED env-mutex
- **Source**: 2026-05-25 architecture-r12
- **Files**: `crates/sandbox/src/restore_handler.rs:1276-1457` (R12-I1 wake-path builder) vs `crates/sandbox/src/backend/nomad_ch.rs:2311-2502` (T-7 cold-boot builder). PLUS: `R12_I1_ENV_LOCK` in restore_handler tests + `T7_ENV_LOCK` in nomad_ch tests — both serialize the SAME env var `SANDBOX_TASK_DRIVER` but don't share — cross-module test race possible.
- **Symptom**: near-byte-for-byte dual jobspec builders; future schema changes need to land in BOTH. Env mutex is silently buggy under cargo test default parallelism.
- **Action**: refactor to a shared `build_nomad_job_json_for(JobspecRequest { restore_from: Option<&Path>, mode: TaskDriverMode, ... })` in `nomad_ch.rs`. Or — better — accept R12-A3's suggestion of "TaskDriverMode as backend struct field at construction" and make the env-read disappear at runtime entirely (tests construct backend with explicit mode; no env mutation needed). Consolidate to ONE env mutex (or eliminate via the struct-field approach).

### [R12-A2] (IMPORTANT, architecture-r12) R11-A1 helper extraction now MANDATORY — fifth secret-loader site exists
- **Source**: 2026-05-25 architecture-r12
- **File**: `crates/sandbox/src/db.rs:1161` `enforce_host_id_file_mode` (R11-S2 closure at `85e4f2f9`)
- **Symptom**: r11 said "4 sites" — actually 5 now. The 5th (`enforce_host_id_file_mode`) uses 0o600 not 0o400 (different mode constant). 8+ tests across 4 files.
- **Action**: extract `read_root_owned_secret_file(path, mode: u32, expected_len: usize)` helper. Take mode as arg to handle the 0o400 vs 0o600 split. Migrate all 5 callers. Consolidate tests.

### [R12-A3] (IMPORTANT, architecture-r12, subsumes R12-M2) TaskDriverMode should be a backend struct field
- **Source**: 2026-05-25 architecture-r12
- **Files**: `crates/sandbox/src/backend/nomad_ch.rs::NomadCHBackend`, `crates/sandbox/src/restore_handler.rs::RealRestoreBackend`
- **Symptom**: `task_driver_mode_from_env()` is called per-CREATE and per-RESTORE; both call sites pass to a freshly-rebuilt jobspec. Should be read once at backend construction.
- **Action**: add a `task_driver_mode: TaskDriverMode` field to both backend structs. Populate in `AppState::from_config` (resolve-at-boot, like every other config field). Tests construct backend with explicit mode (no env mutation, no race). Closes R12-M2, R12-A1's env-mutex problem, and removes one of the dual env locks.

### [R12-A4] (IMPORTANT, architecture-r12) nomad_ch.rs crossed 5000 LOC — split is T-8 PREREQUISITE
- **Source**: 2026-05-25 architecture-r12
- **File**: `crates/sandbox/src/backend/nomad_ch.rs` (5371 LOC, +448 since r11)
- **Symptom**: post-R12-A1, T-8b-cutover (bash wrapper removal + RawExec branch deletion) spans 8 zones across 3 modules (nomad_ch.rs, restore_handler.rs, scripts).
- **Action**: split nomad_ch.rs into the sketched module structure (see architecture r11). MUST land before T-8b-cutover to keep the deletion surgical instead of error-prone.

### [R12-A5] (MINOR, architecture-r12) Recovery layer scattered; host_id reader belongs in recovery.rs
- **Source**: 2026-05-25 architecture-r12
- **Files**: db.rs::{claim_orphan_transient_for_recovery, enforce_host_id_file_mode, load_or_generate_host_id} + sweep.rs + restore_handler.rs
- **Action**: extract recovery.rs.

### Module size table (cycle r8 measurement)
| File | r11 LOC | r12 LOC | Δ |
|---|---|---|---|
| nomad_ch.rs | 4923 | 5371 | +448 |
| db.rs | 3108 | 3298 | +190 |
| restore_handler.rs | 2367 | 2680 | +313 |
| lib.rs | 2267 | 2412 | +145 |
| 3 files >2500 LOC (was 1); 1 file >5000 LOC (was 0) |||

### [R12-T1] (CRITICAL, 4th-round, test-coverage-r12) restore_sandbox end-to-end through CasLost rollback still untested
- **Source**: 2026-05-25 test-coverage-r12 (4th cycle)
- **File**: `crates/sandbox/src/restore_handler.rs:263-313` (rollback closure)
- **Symptom**: R12-I1's +5 tests are builder-shape only (call `build_restore_nomad_job_json(...)` directly + assert returned `serde_json::Value`). None constructs a Database, none calls restore_sandbox, none reaches the rollback closure at line 294. The R10-C2 spawn_blocking discarded JoinHandle is still test-unreachable.
- **Action**: refile of R10-T1/R11-T1. Use the `spawn_fake_nomad` harness from R10-C1 + a `StubRestoreBackend::fail_submit`/`fail_livez` mode to drive the rollback.

### [R12-T2] (CRITICAL, 4th-round, test-coverage-r12) Persist(Some(_)) chain at restore_handler.rs:581-617 still 100% uncovered
- **Source**: 2026-05-25 test-coverage-r12 (4th cycle)
- **Action**: refile of R9-T1/R10-T2/R11-T2. Integration test with persist=Some + sealed record fixture + assertions on unseal + clock_resync + register_restored call order.

### [R12-T3] (IMPORTANT, NEW, test-coverage-r12) submit_restore_job callsite integration untested
- **Source**: 2026-05-25 test-coverage-r12
- **Symptom**: R12-I1 added the wake-path env→mode→jobspec wiring but no integration test drives the full chain (env→build jobspec→POST→fake nomad→state-map assertion).
- **Action**: ~50 LOC extension of `spawn_fake_nomad` harness from R10-C1.

### [R12-T4] (IMPORTANT, 5TH-round, test-coverage-r12) derive_agent_url drift now THREE hardcoded 7777 sites — getting worse
- **Source**: 2026-05-25 test-coverage-r12 (5th cycle)
- **Files**: `crates/sandbox/src/restore_handler.rs:1153, 1162, 1241` (was 1 site in r11, now 3)
- **Action**: extract a const `AGENT_PORT: u16 = 7777` to a shared location (e.g., a `consts` module); use everywhere. Parity test asserts every derived agent URL uses the const.

### [R12-T5] (MINOR, test-coverage-r12) R12-I1 tests in restore_handler.rs would orphan post-R10-A4 split
- **Source**: 2026-05-25 test-coverage-r12
- **Action**: after R10-A4 nomad_ch.rs split, move R12-I1's tests (mod r12_i1_tests) to the new jobspec module.

### Test trajectory: integration test files unchanged for 3 consecutive cycles
- 100% of sandbox lib test growth (+31 over r9→r12) was unit-level in `src/`
- `crates/sandbox/tests/` and `crates/sandbox-agent/tests/` saw ZERO new tests in r10, r11, r12
- The integration gap is structural debt the per-finding pattern can't close

### [R12-P1] (IMPORTANT, performance-r12) R11-P2 BufWriter wrapped WRITE side only; READ side still 8 KiB — CLOSED at 94a8a043
- **Source**: 2026-05-25 performance-r12
- **File**: `crates/sandbox/src/snapshot_store_gcs.rs:438-458` (`download_to_disk`)
- **Symptom**: R11-P2 (3d5c527f) wrapped the destination File in BufWriter (1 MiB). But std lib's `io::copy(reader, writer)` uses BufferedCopySpec when one side is buffered — reader side falls back to 8 KiB scratch buffer. 1 GB download = ~131072 read(2) calls + 1024 write(2) calls. Half the win was unrealized.
- **Action**: symmetric 1-line fix — wrap the source reader: `BufReader::with_capacity(1 << 20, r.into_reader())`.
- **Resolution**: CLOSED at 94a8a043. Reader now wrapped in `BufReader::with_capacity(1 << 20, r.into_reader())`; both sides of `io::copy` are buffered, so BufferedCopySpec uses 1 MiB chunks throughout. 327/327 lib tests pass (unchanged; perf-only).

### [R11-P1 thread-local feasibility CONFIRMED]
- **Source**: 2026-05-25 performance-r12 verification
- **Verified**: `crates/compio-postgres/src/pool.rs:14-17,341` — Pool is `!Send + !Sync` by design (Rc<TcpStream>). Per-worker `thread_local!<RefCell<Option<Rc<Pool>>>>` is the only viable cache shape. Matches architecture r11's sketch.

### Closures this cycle
- [R10-Q5] CLOSED at `0cc7af52` — sandbox-agent/proxy.rs dead _ref_imports deletion (3-round carry)
- [R12-Q1] CLOSED at `46e0fa2a` — db.rs::Database::open_pool TODO refresh pointing to R11-P1
