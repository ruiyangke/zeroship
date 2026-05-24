# sandbox-snapshot-restore — Deferred Backlog

Auto-managed by the pilot-cron-worker on `feat/sandbox-snapshot-restore`. Each cron fire reads this file, picks 1-2 actionable items, lands a fix per logical commit, and removes the entry in the same commit. Findings whose blocker still stands stay listed with an updated "last considered" line.

Last seeded: 2026-05-22 (post bug-#13 cluster smoke; cluster torn down).
Last updated: 2026-05-25 r20 / R20-I1 fixer (doc extraction): `from_host_fence_timeout` rustdoc extracted to ADR `docs/decisions/2026-05-25-vm-index-retry-policy.md`; doc trimmed from ~116 to 13 lines; r17-Q1/r18-M2/r19-M2/r20-M-NEW doc-inflation carry CLOSED. No behavior change. Prior update: 2026-05-25 r19 / R19-C1 fixer (CRITICAL LANDED): wake_jobs takeover sweep — `claim_orphan_wake_for_recovery` + `spawn_wake_jobs_takeover` + `WakeErrorCode::WakeWorkerAborted` + migration 0012. PR1 at `1d3724fe` (db primitive + variant + migration + 4 pg-gated tests), PR2 at `8d163d58` (sweep loop + `takeover_threshold_secs` config + 5 lib tests + wiring). Co-closes R18-A1 / R17-T4 / R19-A2 — all four findings tracked the same gap (lessee_updated_at written by R17-A1 since `96678eaa`, no reader until now). 414 → 423 lib tests; release build clean. Prior update: 2026-05-25 r19 / smoke-r13 fixer (C-7-LT-2 LANDED): host-fence probe wedge fixed at `40811d8b` (PR1: replace ureq-based `compio::runtime::spawn_blocking(|| ureq::get(...).timeout(500ms).call())` with compio-native `compio::time::timeout(150ms, compio::net::TcpStream::connect(addr))`; +5 new tests including `pr1_probe_unroutable_address_returns_false_within_timeout` which pins the wedge-fix invariant — outer compio timeout MUST cap a stuck SYN within 750 ms vs. the kernel's 30-90 s SYN-retransmit ceiling); leak counter + scoped log target at `bfff5acc` (PR2: `sandbox_vm_index_leaks_total{reason=host_fence_timeout|wait_failed}` counter, `inc_vm_index_leak(reason)` API matching `inc_lost_leadership`'s convention with unknown-label-fold-into-host_fence_timeout fallback, `target: "sandbox::teardown::leak"` on the two `stop_inner` leak-branch WARN macros so operators can `RUST_LOG=sandbox::teardown::leak=warn` scope without the full sandbox chatter; +1 new test). Sandbox lib 413 → 414 (+1; PR1 was test-neutral after repurposing the R16-I2 LEAK-case test as a PR1 regression pin since alternating-answer LEAK is structurally impossible under connect-only probe). **Retrospective**: smoke-r13 (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-r13.md`) surfaced that the entire C-4..C-8c diagnostic chain was reasoning over a faulty premise — "teardown completes at +60 s and releases the slot, but the wake budget gives up earlier". The truth: teardown completes at +60 s but LEAKED the slot every cycle because `wait_for_agent_silent`'s ureq probe fired only ONCE in the 30 s fence budget (`probes=1, consecutive_misses=1, last_status=None`). C-4..C-8c's budget tuning is still correct *as fixes* (the budget knobs matter when the agent does go silent), but the production-only signal those fixes were chasing was always upstream of them. C-7-LT-2-PR1 fixes the upstream wedge; C-7-LT-2-PR2 adds defense-in-depth observability so a residual hang (deadlocked agent, OOM-killed without socket-close) is quantifiable. Smoke-r14 next — re-smoke at controller v29 (binary pin not yet bumped; will follow under separate scripts commit per scope guard). Prior r17 PR2 note (C-7-LT-PR2 LANDED): wake_machine state-machine driver at `17e9f421` (typed_id `new_wake_id` + `wak_` 3-char prefix per R16-API2, `WakeErrorCode::wire_code` for §10.0 wire-parity per R16-API1, deprecation counter `sandbox_wake_sync_uses_total` per R16-API1 #5); dual-mode `POST /admin/sandboxes/{id}/wake` + new `GET /admin/sandboxes/{id}/wake/{wake_id}` polling endpoint at `98032273` (idempotency replay + state-mismatch pre-flight + §10.0 envelope on failed-poll body); wake_jobs GC sweep at `9f006c87` (60 s cadence, T_KEEP=5min); pg-gated wake_machine e2e stub fixtures at `c2f24ede` (6 tests: happy + livez/submit/reserve failures + idempotency + GC eviction — closes test-coverage-r16 EMERGENCY HOLD + R10-T1/T2 + 7-round R15-T1 carry). Sandbox lib 360 → 373 (+13). PR3 (cluster smoke validation under SANDBOX_WAKE_RESPONSE_MODE=async) next. Prior PR1 note (C-7-LT-PR1 LANDED): R14-A1 helper extracted at `3d8acc23`, 5 sites migrated `eab3ec43`/`4fd195ef`/`96fa5f0f`, WakeResponseMode flag default-OFF at `b64d2f39`, 0009_wake_jobs migration at `259f0e50`, WakeJobRow/State/ErrorCode + 5 CRUD methods at `a0888d9e`. R14-A1 CLOSED, R16-I1 CLOSED; C-7-LT moves from DESIGN-READY to PR1-LANDED. Lib tests 347 → 360; pg-gated tests written but skipped (local pg down). Prior r16 r1 note: C-8c surfaced from smoke-r11 (sync wake contract structurally out of knobs); C-7-LT design proposal READY at docs/proposals/c7-lt-async-wake.md (uncommitted); R15-S1 CLOSED at da951dd9; R15-I2 CLOSED at 64af1803; #24 CLOSED at a4c481e1; R16-I1/R16-I2/R16-A1/R15-S2 added. Prior r2 note: deferred-backlog refresh: #24 CLOSED at `a4c481e1` — `ZSBX_SANDBOX_ID` env injection verified in tree at `nomad_ch.rs:2373` + regression tests at L4020-4069; entry was stale paperwork written before the fix landed. Prior r1 note: post critical-fix-sweep cluster smoke at v18 + rootfs v5: Phase 1 c=4 HARD FAIL 0/16 creates, NEW bug #24 — controller `Tasks[].Env` block in `nomad_ch.rs:2224-2264` missing `ZSBX_SANDBOX_ID` so the wrapper's R8-DEPLOY1 guard fires at line 153 on every cold boot, killing alloc in ~50ms with empty ch.stderr. Phase 2 c=20 NOT REACHED. Cluster fully torn down. B19 + B-SLO REMAIN UNVERIFIED at cluster on this branch HEAD. Detail: `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-r1.md`).
Prior update: 2026-05-23 (cycle r7+B22-fixer: bug #22 CLOSED — root cause was CH `--restore` preserving `CLOCK_REALTIME` from snapshot-time; fix is signed `/_clock_resync` handshake on agent + controller-side call in `do_restore_inner` between `wait_for_livez` and `register_restored`. v17 controller + v4 rootfs pushed; cluster c=4 confirms POST-WAKE EXEC 7/7 = 100% (was 0/9). B19 also FULLY CLOSED. B-SLO escalation blocked on NEW bug #23 — provision script fails on SERVER_COUNT>1).
Branch HEAD at seed: `fce3e208`.
Branch HEAD at last update: B20 fixer cycle on `3e8bfad5` parent (B20 + Appendix D commit forthcoming). Prior commits: `4fd92bef` (S4); `0aa93a0f` (A3-partial); `15b4f9a8` (B19 in-code); `4e6c70c1` (R4-T1); `28f60d73` (R3-Q3); `2928d5ae` (A4 closed); `b4ddb98b` (B18). Lib tests at parent HEAD: **275 passed**.
Worktree: `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.

---

## CRITICAL (open blockers on Phase B cluster validation)

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

### [#24] (CLOSED at `a4c481e1`) Controller's Nomad task `Env` block missing `ZSBX_SANDBOX_ID` — every cold-boot alloc dies at wrapper line 153
- **Status**: **CLOSED at `a4c481e1`**. Verification: `grep -nE "ZSBX_SANDBOX_ID" crates/sandbox/src/backend/nomad_ch.rs` returns assertion at L2373 + regression tests at L4020-4069. The "captured per task constraints" framing in the original deferred entry was inaccurate — the fix already landed before the entry was written; the entry was stale paperwork.
- **Files at fix**: `crates/sandbox/src/backend/nomad_ch.rs` (env block injection at L2373 + 2 regression tests at L4020-4069).

### [#23] (CLOSED at `0be352a2`) `provision-gcp-cluster.sh` SERVER_COUNT>1 fixed via --metadata-from-file
- **Source**: B22-fixer cycle attempted to escalate to 3+5 cluster for B-SLO c=20 stress; provision failed.
- **Symptom**: `ERROR: (gcloud.compute.instances.create) argument --metadata: Bad syntax for dict arg: [10.178.0.11]`. The server-IPs list is passed to the next-server's metadata without proper escaping/joining; gcloud parses the bracket-formatted Python repr as a dict key.
- **Action shape**: fix the script's metadata-flag concatenation. Likely `--metadata server-ips=10.178.0.10,10.178.0.11,10.178.0.12` should use `--metadata-from-file` or a properly-escaped CSV; investigate `provision-gcp-cluster.sh` around the server-creation loop.
- **Captured, not fixed** per brief constraint (NEW bug → capture verbatim).
- **Blocked**: B-SLO empirical validation at c=20 scale (deferred until #23 is fixed).

### [C-4] (CLOSED at `b2892368`) Wake races detached source-teardown for vm_index — 503 `vm_index_unavailable` immediately after snapshot
- **Source**: T-8b-smoke-r5 cluster review (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-r5.md` § "Root cause for C-4"). Surfaced as the 4th distinct new bug in 5 cycles; sprint brief's "4 cycles in a row → stop" stop-condition was hit; pause + focused fix sprint dispatched.
- **Symptom**: SNAPSHOT lands 1/1 at `200` (C-3 confirmed fixed). WAKE arrives 76 ms later and the controller returns `503 vm_index_unavailable (cluster exhausted at vm_index=1)`. Controller log shows the detached teardown only releases the slot at `+90.2 s` after snapshot return — `01:55:40.483 stop: started` → `01:57:10.656 vm_index released  vm_index=1`. Cluster had 11 other free slots in [2..12] (`vm_index_ceil=12`) but the v1 sticky allocator refuses cross-slot fallback (§ 5.0/§ 5.1).
- **Root cause**: `POST /admin/sandboxes/{id}/snapshot` returns success as soon as the artifact is on disk and detaches `teardown_source_for_snapshot` (`admin_handlers.rs:1311`). That detached task calls `stop_preserving_state` → `stop_inner(.., false)` which releases `vm_index` ONLY after the host-fence clears (`nomad_ch.rs:1134-1138`, ~60 s) + the Nomad job is purged (~30 s). Meanwhile `do_restore_inner` calls `reserve_vm_index(snap.vm_index)` (`restore_handler.rs:389`) sticky to the source slot — collision → 503 immediately. Wake arrives ms after snapshot 200; the slot is held ~90 s.
- **Fix shape**: caller-side bounded retry on the wake path (NOT option (a) "block snapshot" — adds ~90 s to snapshot p50; NOT option (b) "cross-slot fallback" — breaks v1 § 5.0/§ 5.1 sticky + snap.vm_index wire authority; NOT option (c) "decouple vm_index from host_fence" — fence is the primary FM-F defense, releasing the slot earlier reopens the live-IP-to-new-tenant race for concurrent CREATEs). Added `VmIndexRetryPolicy` to the `RestoreBackend` trait + a `reserve_vm_index_with_retry` helper that polls the allocator on a bounded budget (default 60 attempts × 2 s = ~120 s, envelopes the worst observed 90 s teardown). Once the detached teardown's `release()` fires, the next reserve attempt succeeds. Exhausted budget still surfaces as 503 with the same wire shape — operators see exhaustion bounded-late rather than immediately-wrong.
- **Files**: `crates/sandbox/src/restore_handler.rs` (helper + trait method + stub hooks + 4 new unit tests).
- **Tests**: 4 new tests pin the wake-vs-teardown race semantics (`c4_wake_retries_until_source_slot_frees`, `c4_wake_fails_if_slot_never_frees_within_budget`, `c4_wake_uses_single_attempt_when_slot_free`, `c4_default_policy_envelopes_observed_teardown`). Sandbox lib: 328 → 332 passing.

### [C-6] (CLOSED at `91ce9be5`) Wake handler silent stall — detached source-teardown starved compio runtime, blocking C-4's reserve_vm_index_with_retry sleep
- **Source**: T-8b-smoke-r6 cluster review (initial detection) → T-8b-smoke-r7 cluster review (phase-trace localization to `pre_reserve_vm_index`, root cause analysis pointing at the detached teardown).
- **Symptom**: CREATE OK + SNAPSHOT OK. WAKE client times out at 60060 ms. The pg row reads `status=restoring, generation=3, vm_index=1`. Phase trace (v22) reached `pre_reserve_vm_index` and then nothing for the full 60 s deadline — neither the success-after-retry log nor the exhaustion warn fired, despite the source-teardown releasing the vm_index 90 s later. The wake handler's `compio::time::sleep(2s).await` continuations never got polled.
- **Root cause**: `POST /admin/sandboxes/{id}/snapshot` detached `teardown_source_for_snapshot` via `compio::runtime::spawn(...).detach()` on the SAME ntex-worker compio runtime where subsequent wake requests landed (1-worker test fleet → 100% collision). `stop_inner`'s first await is `http_signed_async("/shutdown")` whose underlying `ureq` call burns ~60 s on connection-timeout against the half-dead source agent. Even though the ureq call is wrapped in `compio::runtime::spawn_blocking`, the detached teardown future itself runs on the worker runtime and competes 1:1 with the wake handler's `reserve_vm_index_with_retry` sleep continuations. Net effect: wake future got dropped at the 60 s client timeout → row wedged at `restoring`. The regression existed since R6-P1 (`4c090992`) introduced the detach pattern, but was invisible until C-4 added a sleep-based retry on the wake path that competed for runtime time.
- **Fix**: spawn the detached teardown on a dedicated OS thread (`std::thread::Builder::new().spawn(...)`) with its own short-lived compio runtime (`compio::runtime::Runtime::new().block_on(...)`). Mirrors C-3's pattern at `snapshot_store_gcs.rs::Tiered::put`. Decouples the teardown from any ntex-worker runtime so no future-poll on the worker can be starved by the teardown's awaits. Best-effort / fire-and-forget contract preserved — thread spawn failure (ENOMEM / EAGAIN) logs + drops the teardown; orphan-prune reclaims the vm_index on next boot.
- **Files**: `crates/sandbox/src/admin_handlers.rs` (+75 / −14 LOC at lines 1306-1396).
- **Tests**: 332 pass / 0 fail (unchanged — the bug requires a dual-task compio scenario that the current test harness doesn't drive; cluster smoke-r8 is the validation gate).
- **Sibling sites audited**: `compio::runtime::spawn(...).detach()` exists at `crates/sandbox/src/lib.rs:989,1072,1283` (health/heartbeat/takeover loops — top-of-loop `compio::time::sleep`, no bursty awaits, safe), `crates/sandbox/src/sweep.rs:227,563` (transient-takeover + idle-eviction loops, same shape — safe), `crates/sandbox/src/registry.rs:829` (idle-GC loop — safe), `crates/sandbox/src/backend/nomad_ch.rs:2002` (create-failure rollback Drop guard — no concurrent wake racing the same vm_index until cleanup completes, and no `/shutdown` blocker since the create failed → no live agent; lower risk but a candidate for similar treatment if a future regression surfaces). The snapshot-teardown site is unique in being (a) a bursty teardown with a 60 s `/shutdown` blocker, AND (b) racing a wake handler for the same vm_index on the same runtime.
- **Diagnosis update (T-8b-smoke-r8 / C-7)**: the runtime-starvation framing above is imprecise. Smoke-r8 with the C-6 OS-thread fix in place still wedged at `pre_reserve_vm_index` — falsifying starvation. True mechanism (R14-I1 + C-7): C-4's 120 s retry budget exceeded the 60 s ntex/stress-client deadline, so ntex dropped the wake handler future mid-`compio::time::sleep.await` before either the success-after-retry or exhausted-budget log could fire. C-6's OS-thread fix is defense-in-depth (still correct, addresses a real shared-runtime concern) but was not the proximate cause. The proximate cause was the budget mismatch — closed in C-7 below.

### [C-8] (CLOSED) Source teardown holds vm_index ~150 s — exceeds C-7's 48 s wake retry budget
- **Source**: T-8b-smoke-r9 cluster review (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-r9.md`).
- **Symptom**: Smoke-r9 with C-7's 48 s budget confirmed C-7 fix WORKING (all 25 retry attempts visible, clean 503 `vm_index_unavailable`, no silent cancel). But wake still surfaces 503 at 48 s because the source teardown holds the slot ~150 s in production: `host_fence_timeout_secs=120 s` (cad098e6) + Nomad job purge ~30 s.
- **Fix**: cluster-config — `gcp-worker-startup.sh` now sets `Environment=SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30` in the `zsbx-ctl.service` systemd unit. Reduces source teardown from ~150 s → ~60 s, fitting within the 48 s retry budget with ~12 s residual headroom. Rust default (`crates/sandbox/src/config.rs`) left at the conservative 120 s — production deployments needing the longer drain window keep that default; cluster-smoke worker hosts opt down via this env override.
- **Trade-off**: 30 s host fence is less conservative than the 120 s default. Acceptable for cluster smoke + test traffic; FM-F race window in the worst case is the difference between the host-fence clear and the new tenant's first `/livez` poll — empirically well-bounded for the smoke workload (the host-fence p95 measurement in `NomadCHConfig::host_fence_timeout_secs` doc was 29 s @ 30-way concurrent stop, so 30 s is at the edge — operators running >30-way bursts should override via metadata).
- **Files**: `crates/sandbox/scripts/gcp-worker-startup.sh` (+10 / 0 LOC at the zsbx-ctl.service Environment block; shellcheck clean for the change, pre-existing SC2020 info untouched).
- **Tests**: shellcheck `--severity=error` clean. No Rust code change; cluster-smoke-r10 is the validation gate.

### [C-8a] (CLOSED) R14-A6 derivation re-introduces C-7 silent cancellation under conservative fence (120 s → 110 s budget exceeds 60 s ntex deadline)
- **Source**: T-8b-smoke-r9 cluster review § "C-8a hidden — R14-A6 latent bug". Smoke-r9's v24 controller was deliberately built from PRE-R14-A6 HEAD (`b8fae7b7`) to avoid this regression.
- **Symptom**: R14-A6 at `c3edf968` introduced `VmIndexRetryPolicy::from_host_fence_timeout(cfg.host_fence_timeout_secs)`. Under the production-default 120 s fence, the formula `(120 - 10) / 2 + 1 = 56` attempts × 2 s = 110 s wall-time — re-introduces the exact C-7 failure mode (budget exceeds 60 s ntex client deadline → silent mid-`sleep.await` cancellation).
- **Fix**: cap the derived budget at `CLIENT_DEADLINE_SECS - CLIENT_HEADROOM_SECS = 50 s` regardless of how conservative the fence is. Compute TWO ceilings — fence-derived (the IDEAL upper bound) and deadline-derived (the HARD upper bound) — and take the MIN. The hard ceiling wins when the operator runs a conservative fence; the ideal ceiling wins for tight per-cluster overrides (e.g. cluster-smoke at 30 s → 20 s budget from fence, 50 s from deadline → 20 s wins).
- **Trade-off**: under a 120 s fence, the wake-retry budget caps at 50 s (was 110 s pre-cap). That means a wake racing a slow source teardown that legitimately needs >50 s returns a clean 503 `vm_index_unavailable` (with the exhausted-budget log) instead of silently stalling for 110 s before ntex drops the future. Observable failure > silent stall. The long-term fix remains C-7-LT (`202 Accepted` + polling) which decouples wake response from the client connection deadline.
- **Files**: `crates/sandbox/src/restore_handler.rs` (+20 / −10 LOC; `from_host_fence_timeout` now computes MIN of two ceilings, doc block rewritten with C-8a context and new examples table including the 120 s cap case).
- **Tests**: 336 → 337 pass / 0 fail (one new test, `r14a6_from_cfg_caps_at_client_deadline`, asserts a 120 s fence yields ≤ 50 s wall-time and exactly 26 attempts). Existing R14-A6 tests (`r14a6_policy_from_cfg_respects_host_fence_timeout`, `…_short_timeout`, `…_zero_fence_still_attempts_once`) continue to pass — the 60 s and 20 s and 0 s cases were already inside the deadline cap.

### [C-8b] (CLOSED) `from_host_fence_timeout` fence-derived ceiling underestimated teardown by 2× (smoke-r10 measured 60.164 s at fence=30 s)
- **Source**: T-8b-smoke-r10 cluster review (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-r10.md`). 10th cluster cycle / 9th distinct production-only bug.
- **Symptom**: Smoke-r10 confirmed C-8 (30 s fence env var) and C-8a (MIN-of-two ceilings) landed cleanly, but WAKE still 0/1. With `host_fence_timeout_secs=30`, the C-8a fence-derived ceiling resolves to `(30 − 10) / 2 + 1 = 11` attempts × 2 s = **20 s budget**. Empirical source teardown wall-time at fence=30 was **60.164 s** (`stop: started 04:15:23.442` → `stop: complete 04:16:23.606`), so wake exhausted ~40 s before vm_index actually freed. Clean 503 (no C-7 silent cancel — that protection held), but functional failure.
- **Root cause**: the C-8a formula assumed `teardown_wall_time ≈ host_fence_timeout`. Empirically the full stop pipeline (`/shutdown` → `wait_for_agent_silent` host-fence wait → Nomad job purge tail → cleanup) is ~2× the fence. Contributors: `wait_for_agent_silent` requires 2 consecutive no-reply polls and frequently rides the fence to timeout when the agent's HTTP listener takes >fence to actually close (smoke-r10 saw `consecutive_misses=1` at the 30 s deadline → leak); the Nomad purge tail then appends a second fence-shaped wait. Together they compose to 2× at the fence values we run in production.
- **Fix**: re-baseline the fence-derived ceiling on `teardown_estimate = 2 * host_fence_timeout_secs` before subtracting `CLIENT_HEADROOM_SECS`. C-8a's MIN-of-two design is preserved structurally (deadline ceiling still binds in the conservative-fence case), so the C-7 silent-cancel protection is unchanged. At fence=30 the new derivation is `MIN((2*30 − 10), (60 − 10)) = MIN(50, 50) = 50` → **26 attempts × 2 s = 50 s budget** (was 11 / 20 s). Both ceilings remain ≤ 60 s ntex deadline at every fence value.
- **Trade-off**: defense-in-depth tactical patch; the structural fix is C-7-LT (async wake response + polling, R15-A1). With C-8b in place a single-cycle smoke at default cluster config (fence=30) should hit WAKE OK 1/1 because the budget now envelopes the empirical teardown. Future regressions where teardown grows past 50 s still surface as a clean 503 + exhausted-budget WARN (the C-8a invariant).
- **Files**: `crates/sandbox/src/restore_handler.rs` (~+45 / −12 LOC; `from_host_fence_timeout` now multiplies by 2 before headroom subtraction, doc block rewritten with C-8b context + new examples table + cross-reference to smoke-r10 review + C-7-LT pointer; one updated test, one new test).
- **Tests**: 337 → 338 pass / 0 fail. New test `c8b_default_policy_envelopes_doubled_fence` pins the fence=30 case at ≥21 attempts (≥40 s budget) and exactly 26 attempts post-fix. Updated `r14a6_policy_from_cfg_short_timeout` (fence=20 case: was 6 attempts / 10 s pre-C-8b → now 16 attempts / 30 s post-C-8b — fence-ceil `2*20 − 10 = 30` binds, deadline-ceil 50 doesn't). Unchanged: `r14a6_policy_from_cfg_respects_host_fence_timeout` (60 s → 26 attempts, deadline-ceil binds), `r14a6_policy_from_cfg_zero_fence_still_attempts_once` (0 s → 1 attempt, MIN_ATTEMPTS floor), `r14a6_from_cfg_caps_at_client_deadline` (120 s → 26 attempts, deadline-ceil binds).
- **Note on `wait_for_agent_silent` framing**: the smoke-r10 review attributed the 2× factor primarily to `wait_for_agent_silent`'s 2-consecutive-misses contract. Reading the function (`crates/sandbox/src/backend/nomad_ch.rs:3205-3273`) shows it polls at 100 ms cadence, so the 2-consecutive-misses gate adds only ~200 ms once the agent really dies; the dominant contributor to the 2× ratio is the *combined* pipeline (host-fence wait + Nomad purge tail), not the silent-fence step alone. The doc comment on `from_host_fence_timeout` is written to that broader framing.

### [C-7-LT-2] (CLOSED at `40811d8b` (PR1) + `bfff5acc` (PR2)) `wait_for_agent_silent` probe wedge — 1 probe per ~30 s instead of 300, vm_index LEAKED every teardown
- **Source**: T-8b-smoke-r13 cluster review (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-r13.md`). The first cluster cycle where the C-7-LT-1 retry budget caught the teardown wall-time — and immediately surfaced the deeper bug. WAKE 0/1 at concurrency=1 with `vm_index_unavailable`; controller logs showed `host_fence: deadline reached  probes=1  consecutive_misses=1  last_status=None  elapsed_ms=30129` followed by `vm_index leak  reason=host_fence_timeout`.
- **Symptom**: The host-fence probe loop in `wait_for_agent_silent` (`crates/sandbox/src/backend/nomad_ch.rs:3184`) is supposed to fire probes at 100 ms cadence for up to the `host_fence_timeout` budget — at 30 s that's ~300 probes. Instead, ONE probe fired across the entire 30 s budget. `consecutive_misses` saturated at 1 (never reached the threshold=2); the fence returned Err; `stop_inner` LEAKED the vm_index. With the slot leaked, the wake-side `reserve_vm_index_with_retry` retried the SAME doomed slot to budget exhaustion (36 attempts × 2 s = 70 s under C-7-LT-1's widened budget) and surfaced `slot_unavailable` — but the actual failure was upstream: no widening of the wake retry budget can rescue a slot the teardown never released. The 60 s teardown wall-time observed reproducibly across r10/r11/r12/r13 was `30 s fence FAILED + 30 s Nomad purge tail`, NOT a happy 60 s teardown.
- **Root cause**: `ureq::get(...).timeout(Duration::from_millis(500)).call()` was called inside `compio::runtime::spawn_blocking`. ureq's `.timeout()` is a *request-deadline* timeout, NOT a TCP-connect timeout. On a TAP-collapsing teardown route, TCP-connect hangs waiting for the kernel's SYN-retransmit ceiling to surface ECONNREFUSED / ETIMEDOUT (typically 30-90 s on Linux defaults). The 500 ms ureq timeout never fires because there's no HTTP-layer deadline path on a connect that hasn't completed. The single blocking task consumed the entire fence budget.
- **Why this didn't surface in r10/r11/r12**: in those cycles the sync-contract retry budget exhausted at 48-50 s — BEFORE the teardown completed at +60 s. The `host_fence: deadline reached` log fired, the slot was leaked, but the smoke client had already given up. The post-mortem fixated on "budget too narrow" (correctly observable, incorrectly framed as the cause). C-7-LT-1 widened the budget to 70 s — the first cycle that out-lived the teardown — and immediately exposed the truth: **teardown completes at +60 s but LEAKS the slot**, every cycle. The C-4 → C-8c → C-7-LT → C-7-LT-1 chain's premise ("teardown completes = slot released") was wrong from r4 onwards; the smoke just couldn't see it until the wake budget envelope grew past the teardown wall-time.
- **Fix (PR1, `40811d8b`)**: replace the ureq-based probe with `compio::time::timeout(150ms, compio::net::TcpStream::connect(addr))`. compio's outer timeout hard-cancels the connect future via io_uring CANCEL — independent of the kernel's SYN-retransmit behaviour. The 100 ms loop cadence is preserved but decoupled from probe latency (the loop subtracts the probe's own elapsed from the cadence sleep), so a fast loopback ACK and a 150 ms black-hole timeout both produce the same inter-probe interval. New helpers `parse_agent_probe_addr` (strips scheme + path from `http://<ipv4>:<port>`) and `probe_agent_reachable_tcp` (connect-only probe). Side effect: the R16-I2 "alternating-answer LEAK" pathology is structurally impossible under the new connect-only probe (accept() succeeded means SYN ACKed means socket alive regardless of post-accept HTTP behaviour). The LEAK-case test is repurposed as a PR1 regression pin (`host_fence_pr1_alternating_accept_drop_times_out_with_misses_zero`).
- **Fix (PR2, `bfff5acc`)**: defense-in-depth observability. New metric `sandbox_vm_index_leaks_total{reason}` with `inc_vm_index_leak(reason)` API (matches `inc_lost_leadership`'s `&'static str` + unknown-label-fold convention). Two reasons emitted: `host_fence_timeout` (the C-7-LT-2 case) and `wait_failed` (Nomad purge failed). Both `stop_inner` leak branches gain `target: "sandbox::teardown::leak"` on the WARN macros so operators can `RUST_LOG=sandbox::teardown::leak=warn` scope the log. The full leaked-slot reclaim sweep (mark + cooldown + auto-reclaim) is deferred — keeping PR2 SIMPLE per the brief; PR3 follows if smoke-r14 still observes leaks.
- **Files**: `crates/sandbox/src/backend/nomad_ch.rs` (+364 / −77 LOC PR1; +14 / 0 LOC PR2). `crates/sandbox/src/metrics.rs` (+93 / 0 LOC PR2).
- **Tests**: 408 → 414 (+6). New: `pr1_probe_reachable_port_returns_true`, `pr1_probe_refused_port_returns_false_fast`, `pr1_probe_unroutable_address_returns_false_within_timeout` (load-bearing — pins the outer compio timeout invariant), `pr1_one_miss_then_reachable_resets_counter`, `pr1_parse_agent_probe_addr_accepts_expected_shapes`, `metrics::tests::vm_index_leak_counter_per_reason_monotonic`. Repurposed: the R16-I2 LEAK-case test is renamed `host_fence_pr1_alternating_accept_drop_times_out_with_misses_zero` and now pins the OPPOSITE invariant (alternating accept-vs-drop must time out with `consecutive_misses=0`, not `=1`). Build clean; release build clean.
- **Verify in tree**: `cargo build -p zeroship-sandbox --tests` clean; `cargo test -p zeroship-sandbox --lib` 414/0/1-ignored; `cargo build --release -p zeroship-sandbox --lib` clean.
- **Out of scope for this PR (per scope guard)**: controller binary pin bump (v28 → v29) — the scripts commit follows under smoke-r14 separately. `wake_machine.rs` and `restore_handler.rs` untouched. No retry-budget knobs changed (that's C-7-LT-1 territory; the fix here is upstream of the budget).
- **Retrospective on C-4..C-8c chain**: every fix in that chain (C-4 retry budget, C-6 OS-thread teardown, C-7 budget cap under client deadline, C-7-LT async contract, C-7-LT-1 mode-aware budget, C-8a/b/c fence-ceiling math) targeted the wake side. ALL of them assumed the source teardown released the slot at completion. None of them did. The cluster smoke evidence (r10/r11/r12) showed `host_fence: deadline reached` and `vm_index leak` lines every cycle, but the `fence_passed=false` flag was never lifted into the cycle summary, and the smoke pattern (budget exhaustion arriving before teardown completion) made the leak invisible behind the wake-side 503. The C-7-LT-2 fix surface is upstream of every prior C-* fix in this branch.
- **Sibling ureq probe sites surveyed (NOT fixed in this PR)**: the same `compio::runtime::spawn_blocking(|| ureq::get(...).timeout(500ms).call())` shape exists at `nomad_ch.rs:3083` and `k8s.rs:1350`, both inside `wait_for_agent_livez` — the agent-STARTUP probe loop (the OPPOSITE direction from `wait_for_agent_silent`'s STOP probe). In principle the same wedge could manifest if a starting agent's process is bound but not yet calling accept() — the kernel SYN-retransmits and the 500 ms ureq deadline doesn't apply. In practice we have no evidence this fires (the failure mode would be "create timed out" rather than "slot leaked"), and the startup probe loop chains a signed `/version` RPC after the livez gate, so a connect-only replacement is not a one-line change. Leaving for a future PR if a similar wedge surfaces on the create/wake path.

### [R19-C1] (CLOSED at `1d3724fe` (PR1) + `8d163d58` (PR2)) wake_jobs takeover sweep absent — controller crash mid-wake wedges sandbox permanently
- **Source**: concurrency-r19 escalation (`docs/reviews/sandbox-snapshot-restore-concurrency-2026-05-25-r19.md` § "Findings → [R19-C1] Stale wake_jobs + GATE-C2 UNIQUE INDEX = permanent wake block"). The finding aggregated three predecessor flags that all tracked the same gap from different lenses: r17-T4 (test-coverage), r18-A1 (arch), r19-A2 (arch). The R17-A1 lessee-renewal write (`96678eaa`) had landed but **nothing read the column**.
- **Symptom**: a controller crash / panic / OOM / graceful-shutdown abort during any non-terminal wake phase (pending / reserving_slot / restoring / livez_polling / clock_resyncing / registering) leaves the row in a non-terminal state. `gc_expired_wake_jobs` only sweeps `state IN ('ok','failed')` — it ignores the orphan. Combined with the `wake_jobs_sandbox_pending_uniq` UNIQUE INDEX from migration 0011 (GATE-C2), every subsequent wake POST for that sandbox returns `InsertWakeJobOutcome::Replay(stale_row)`; the handler short-circuits with a 202 pointing at the dead wake_id. The client polls the dead row forever. Persists across controller restarts (durable in pg).
- **Why concurrency-lens**: `lessee_updated_at` is bumped on every transition by design (R17-A1, `96678eaa`) so a sweep CAN distinguish "in-flight wake healthy" from "controller lessee abandoned" by lease age. **The lease-renewal write had no reader** — the LOAD-BEARING half of the R14-C1-mirror pattern was missing on the wake_jobs side.
- **Trigger probability**: low per-sandbox per controller-lifetime, but UNBOUNDED ACCUMULATION (once stuck, blocks every retry forever — no client- or operator-side recovery path absent direct pg `UPDATE`).
- **Fix (PR1, `1d3724fe`)**: db primitive — `Database::claim_orphan_wake_for_recovery(threshold)` issues a single `UPDATE … WHERE state NOT IN ('ok','failed') AND lessee_updated_at < now() - $1 RETURNING wake_id` that atomically transitions abandoned rows to `(state='failed', error_code='wake_worker_aborted', error_message='controller lessee abandoned this wake (R19-C1 takeover sweep)')`. New `WakeErrorCode::WakeWorkerAborted` variant + `as_str` / `from_str_opt` / `wire_code` mappings (wire code `wake_worker_aborted`; distinct from `internal_error` so the SLO dashboard can split "wake step failed" from "controller crashed mid-wake"). Migration 0012 extends `wake_jobs_error_code_check` to admit the new value. PG row-locks during UPDATE serialize concurrent callers; a peer that already claimed the row matches zero rows on the second pass.
- **Fix (PR2, `8d163d58`)**: sweep loop — `sweep::run_wake_jobs_takeover_once` + `spawn_wake_jobs_takeover` mirror the existing `spawn_wake_jobs_gc` shape. Hard-coded 60 s poll cadence (`WAKE_JOBS_TAKEOVER_POLL_SECS`); per-iteration threshold reads `state.wake_lifecycle.takeover_threshold_secs` (env `SANDBOX_WAKE_JOBS_TAKEOVER_THRESHOLD_SECS`, default 60 s, minimum 30 s — below that the sweep could steal in-flight wakes whose longest single stage is ~30 s `agent_livez_timeout`). Runs on a dedicated OS thread + private compio runtime via `detach_isolated("wake-takeover", …)` — same C-6 wedge-avoidance fingerprint as the GC and transient sweeps. Spawn gated on `database.is_some()` in `AppState::from_config`.
- **Files**: `crates/sandbox/src/db.rs` (PR1: +~118 / 0 LOC; new variant + `claim_orphan_wake_for_recovery` + extended round-trip + wire-code tests). `crates/sandbox/migrations/0012_wake_jobs_aborted_code.sql` (PR1: new, +66 / 0 LOC). `crates/sandbox/tests/sandbox_pg_e2e.rs` (PR1: +~273 / 0 LOC; 4 new pg-gated tests). `crates/sandbox/src/sweep.rs` (PR2: +~156 / 0 LOC; sweep helper + spawn + 2 new constant-pin tests). `crates/sandbox/src/config.rs` (PR2: +~99 / 0 LOC; `takeover_threshold_secs` field + env parsing + 3 new tests). `crates/sandbox/src/lib.rs` (PR2: +14 / 0 LOC; spawn wiring).
- **Tests**: 414 → 423 (+9 lib tests; +4 pg-gated tests). New lib tests: `wake_jobs_takeover_cadence_is_60s`, `wake_lifecycle_takeover_threshold_floor_and_default_pinned`, `default_takeover_threshold_when_unset`, `explicit_takeover_threshold_accepted`, `takeover_threshold_below_minimum_rejected` (+ 4 extended round-trip / wire-code cases that the per-variant lib counts don't surface). New pg-gated tests (under `wake_jobs_crud`): `claim_orphan_wake_marks_stale_row_failed`, `claim_orphan_wake_skips_fresh_row`, `claim_orphan_wake_skips_terminal_row`, `claim_orphan_wake_concurrent_claims_race_cleanly`.
- **Verify in tree**: `cargo build -p zeroship-sandbox --tests` clean; `cargo test -p zeroship-sandbox --lib` 423/0/1-ignored; `cargo build --release -p zeroship-sandbox` clean.
- **Co-closes**: **R18-A1** (arch-r18: "wake_jobs takeover sweep absence"), **R17-T4** (test-coverage-r17: "no test exists for the takeover sweep because the takeover sweep itself doesn't exist"), **R19-A2** (arch-r19: same gap, escalated). Also closes the "multi-replica precondition" backlog item from `docs/proposals/c7-lt-async-wake.md` § 5 — the proposal noted the takeover sweep is the precondition for safe multi-controller HA on the wake path, since without it a crashed peer's in-flight wakes wedge the shared `wake_jobs` table; with the sweep landed, any peer can pick up after any peer's crash.
- **Out of scope for these PRs (per scope guard)**: scripts (smoke harness untouched), `crates/sandbox/src/backend/nomad_ch.rs` (R19-I1 wait_for_agent_livez fix queued separately as `82478a6b`), all non-sandbox crates.
- **Architectural note**: this closes the LAST load-bearing wake-path concurrency gap. The wake-side R14-C1-mirror pattern (lessee_updated_at + takeover sweep) is now fully assembled — write side (R17-A1 / `96678eaa`) + read side (R19-C1 / this entry). Future cluster-stress runs that include controller-kill-during-wake fault injection will exercise the sweep end-to-end; smoke-r14+ should reach for `kill -9 controller && curl POST wake` as a deliberate test.

### [R19-T1] (CLOSED) Admin-handler poll renderer not pinned for `WakeWorkerAborted` wire code
- **Source**: test-coverage-r19 (`docs/reviews/sandbox-snapshot-restore-test-coverage-2026-05-25-r19.md` § "[R19-T1] Admin-handler poll renderer not pinned for WakeWorkerAborted wire code"). The finding noted that `r16_api1_failed_state_renders_every_wake_error_code` enumerated only the 6 pre-R19-C1 variants; `WakeWorkerAborted` — added by R19-C1 PR1 — was absent from the loop.
- **Fix**: added `WakeErrorCode::WakeWorkerAborted` to the loop in `r16_api1_failed_state_renders_every_wake_error_code`. Also added inline `state == "failed"` and `message.is_string()` assertions covering all loop iterations so the full §10.0 envelope shape is pinned, not just the `error` field.
- **Files**: `crates/sandbox/src/admin_handlers.rs`.
- **Tests**: 425 passed (variant now exercised within the existing test; no new `#[compio::test]` function needed — the loop expansion is the coverage addition).
- **Verify**: `cargo test -p zeroship-sandbox --lib` 425/0/1-ignored clean.

### [R19-API1] (CLOSED) `error_message` body leaks internal review ID + lessee term
- **Source**: api-surface-r19 (`docs/reviews/sandbox-snapshot-restore-api-surface-2026-05-25-r19.md` § "R19-API1 — error_message body leaks internal review ID + lessee term"). The SQL literal `'controller lessee abandoned this wake (R19-C1 takeover sweep)'` embedded two internal-only terms: "R19-C1" (review ID) and "lessee" (internal lease vocabulary not part of the public API contract). Either could appear verbatim in a 4xx/5xx JSON body seen by API consumers.
- **Fix (db.rs)**: rewrote the SQL literal to `'wake worker aborted: controller did not complete the wake within the timeout (see operator runbook)'` — operator-facing, no internal jargon.
- **Fix (sweep.rs)**: moved the R19-C1 lineage breadcrumb into the `tracing::warn!` structured log in `run_wake_jobs_takeover_once` as `closure_ref = "R19-C1"`. The field travels only to the log pipeline (Loki/CloudLogging), never to the wire.
- **Files**: `crates/sandbox/src/db.rs`, `crates/sandbox/src/sweep.rs`.
- **Tests**: existing `claim_orphan_wake_for_recovery` pg-gated tests remain valid (they assert on state/error_code, not the message literal). Build clean.

### [C-7-LT-1] (CLOSED at HEAD) `VmIndexRetryPolicy::from_host_fence_timeout` still applied the sync-era deadline cap under async mode
- **Source**: T-8b-smoke-r12 cluster review (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-r12.md`).
- **Symptom**: Smoke-r12 confirmed the C-7-LT async contract worked end-to-end (POST 202 + 97 polls + clean terminal envelope). But WAKE still 0/1 at +50 s with `vm_index_unavailable`. The source-teardown wall-time was 60.166 s (reproducible from r10/r11); the retry policy capped budget at `CLIENT_DEADLINE_SECS − HEADROOM = 60 − 10 = 50 s`. In sync mode that cap was correct (ntex would cancel the future at the 60 s client deadline). In async mode the wake state machine runs on a `detach_isolated` thread with no client deadline binding the retry loop — the 50 s cap became a vestige producing a 10 s deficit against the empirical 60.166 s teardown.
- **Fix**: thread `WakeResponseMode` into `VmIndexRetryPolicy::from_host_fence_timeout`. In Sync mode the legacy dual-ceiling MIN preserves the C-8a/C-8b contract. In Async mode the deadline cap is dropped and the budget becomes `2 × host_fence + HEADROOM` — a safety margin past the empirical teardown wall-time (smoke-r12 measured 60.166 s at fence=30 s; the 70 s async budget envelopes with ~10 s slack). `RealRestoreBackend` carries the mode (defaulting to Sync for unit-test back-compat) and `AppState::from_config` threads `WakeResponseMode::from_env()` through via the new `with_wake_response_mode` builder.
- **Examples post-fix**:
  - fence=30, Sync → MIN(2×30−10, 60−10) = 50 s → 26 attempts (unchanged).
  - fence=30, Async → 2×30 + 10 = 70 s → 36 attempts. Envelopes the 60.166 s smoke-r12 teardown with ~10 s slack.
  - fence=120, Sync → MIN(230, 50) = 50 s → 26 attempts (unchanged).
  - fence=120, Async → 2×120 + 10 = 250 s → 126 attempts.
- **Files**: `crates/sandbox/src/restore_handler.rs` (+~120 / −~25 LOC; `from_host_fence_timeout` signature + branch, `RealRestoreBackend` new field + `with_wake_response_mode` builder, doc block rewritten with C-7-LT-1 context + new examples, existing tests updated to pass `Sync`, 3 new tests pinning async-mode budget). `crates/sandbox/src/lib.rs` (~+15 / −15 LOC; reorders `WakeResponseMode::from_env` resolution before the snapshot wiring so it threads into `RealRestoreBackend`).
- **Tests**: 399 → 402 (+3): `c7_lt_1_async_mode_fence_30_yields_70s_budget`, `c7_lt_1_sync_mode_fence_30_preserves_c8b_budget`, `c7_lt_1_async_mode_fence_120_unbinds_deadline`. Lib `cargo test -p zeroship-sandbox --lib` passes 402/0/1-ignored.
- **Closes**: the last residue of C-8c under async mode (the sync-mode "out of knobs" framing remains accurate for the legacy 200-OK contract, which is now a deprecated path with telemetry counter `sandbox_wake_sync_uses_total`).

### [C-8c] (OPEN — gates Phase B, architectural-only fix possible) Synchronous wake-response contract is structurally out of knobs (CRITICAL)
- **Source**: T-8b-smoke-r11 cluster review (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-r11.md`).
- **Symptom**: With C-8b's 2× fence factor applied (v26 controller, fence=30 → 50s budget × 26 attempts), smoke-r11 WAKE still 0/1. All 26 attempts logged at 2s cadence; budget exhausted at 50s; source teardown wall-time 60.166s (reproducible to ±2ms across r10/r11).
- **Root cause**: At fence=30, both retry ceilings collapse to 50s: fence-derived (2 × 30 − 10 = 50) AND deadline-derived (CLIENT_DEADLINE − HEADROOM = 60 − 10 = 50). 10.1s deficit cannot be closed inside the sync-response contract — lowering fence keeps the HEADROOM-sized deficit, raising fence widens the deficit (deadline binds), raising CLIENT_DEADLINE_SECS violates the public 60s SLO.
- **Architecture finding (r16-A1)**: C-8b's 2× factor is a "numeric coincidence, not a model" — the Nomad purge tail (`wait_for_job_gone`, 30s fixed timeout at `nomad_ch.rs:1051`) is independent of fence; formula is wrong for fence<30 and inert for fence≥60.
- **Only path forward**: C-7-LT (async 202 + polling). Design proposal ready at `docs/proposals/c7-lt-async-wake.md` (uncommitted per proposal-workflow rule).
- **Blocks**: T-8b-stress, T-8b-cutover, wrapper removal.
- **Cluster evidence**: smoke-r10 + smoke-r11 reviews; cross-lens consensus rounds r12-r16.

### [C-7] (CLOSED at `493d6c1e`) Wake handler future dropped by ntex on client disconnect — C-4 retry budget (120 s) exceeded 60 s client deadline
- **Source**: T-8b-smoke-r8 cluster review (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-r8.md` § "C-7 ROOT-CAUSE HYPOTHESIS"), corroborating concurrency-r14 R14-I1.
- **Symptom**: WAKE wedges at IDENTICAL phase `pre_reserve_vm_index` even AFTER C-6's OS-thread fix landed at `91ce9be5`. The C-6 runtime-starvation hypothesis is falsified — the wake-path stalls with the same wire shape and last-phase log it had pre-C-6. The 60 s stress-client deadline elapses with no success/exhausted-budget log from `reserve_vm_index_with_retry`.
- **Root cause**: `VmIndexRetryPolicy::default` shipped at 60 × 2 s = 120 s total budget (sized in C-4 to envelope the ~90 s host_fence + Nomad purge teardown). The stress client (`/opt/stress/snapshot_stress.py`) has a 60 s timeout. When the client TCP-closes at 60 s, ntex drops the wake handler future per request-cancellation semantics; the retry's `compio::time::sleep` is in a pollable state at that moment, so the future is canceled mid-sleep and neither the success-after-retry `tracing::info!` nor the exhausted-budget `tracing::warn!` branch fires (both are after-the-loop). Silent wedge — row stuck at `restoring`.
- **Fix shape**: ship (a) + (b) per R14-I1 / smoke-r8 recommendation. (c) async response with polling is the architectural long-term fix but out of scope for the smoke-unblock.
  - (a) Reduce `VmIndexRetryPolicy::default` from 60×2 s = 120 s to 25×2 s = 50 s. Keeps ≥10 s headroom under the 60 s client deadline so the exhausted-budget log fires before the client disconnect cancels the future.
  - (b) Add a per-attempt INFO log inside `reserve_vm_index_with_retry`'s loop body (target `zeroship_sandbox::restore_handler`, fields `sandbox_id`, `attempt`, `max_attempts`, `vm_index`). Smoke-r9 can now see which attempt-N the loop is on when the cancellation lands — the diagnostic gap that made C-7 invisible.
- **Trade-off**: under sustained host_fence races where the source slot doesn't vacate within 50 s, wake returns a clean 503 `vm_index_unavailable` (with the exhausted-budget warn log) instead of silently hanging until the client disconnects. Observable failure mode > silent timeout. The C-4 default policy comment in `restore_handler.rs` documents this trade-off and points at C-7-LT (async response + polling) as the proper fix.
- **Files**: `crates/sandbox/src/restore_handler.rs` (+80 / −19 LOC; default policy, per-attempt log, doc updates at the policy struct, trait method, and `do_restore_inner` call site; C-4 #4 default-policy test superseded by a C-7 inverse guard `c7_retry_budget_default_is_under_client_deadline` asserting budget + 5 s headroom ≤ 60 s).
- **Tests**: 332 pass / 0 fail (unchanged count — one test replaced, not added; the old C-4 #4 `c4_default_policy_envelopes_observed_teardown` asserted budget ≥ 90 s, which now contradicts the C-7 invariant). Validation gate is T-8b-smoke-r9 — c=1 wake must surface either 200 OK or a clean 503 with the exhausted-budget log + per-attempt INFO trail.
- **Long-term (C-7-LT, deferred)**: replace the synchronous wake response with `202 Accepted` + status URL polling. Decouples the wake handler from the client connection deadline so any future budget mismatch surfaces as a polling-loop signal rather than a silent cancellation. Out of scope for the smoke-unblock; opens once cluster smoke is green again.

### [C-5] (CLOSED at `d7740b03`) Worker VM GCS scope too narrow — L2 upload 403 "Provided scope(s) are not authorized"
- **Source**: T-8b-smoke-r5 cluster review (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-r5.md` § "C-5 (minor, non-blocking) — GCS scope 403"); minor, but must close before T-8b-stress (c=20) or every L2 upload silently drops to GCS.
- **Symptom**: Detached L2 upload runs cleanly as code (no compio panic — C-3 already closed that surface), but the HTTPS call returns 403: `GCS single-shot upload snapshots/v1/.../config.json: status 403, … Provided scope(s) are not authorized`. Fire-and-forget detach masks the failure in smoke (CREATE/SNAPSHOT/WAKE assertions still pass), but at c=20 stress every L2 upload would fail silently — defeating the snapshot-tier purpose.
- **Root cause**: `crates/sandbox/scripts/provision-gcp-cluster.sh:286` provisioned the worker VM with `--scopes=storage-ro,logging-write,monitoring-write` (read-only on `devstorage`). `gs_pull` (controller binary fetch) works, but `GcsSnapshotStore::put` needs `devstorage.read_write`.
- **Fix**: narrow scope upgrade — `storage-ro` → `storage-rw` (gcloud shorthand for `https://www.googleapis.com/auth/devstorage.read_write`). Server VM scope (line 250) left at `storage-ro` — the server doesn't perform L2 uploads (no `GcsSnapshotStore::put` runs there; postgres + nomad-server only). Verified shellcheck clean (`crates/sandbox/scripts/lint.sh` 7 scripts clean at `--severity=error`).
- **Scope manifests on next provision only**: existing cluster instances keep their old (read-only) scope until destroyed and re-created; no in-place hot-fix needed for the change itself.

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

### [C-7-LT] (PR1 LANDED 2026-05-25; PR2 next) Async wake-response contract: 202 + polling (CRITICAL architectural sprint)
- **Design**: `docs/proposals/c7-lt-async-wake.md` — 2490 words, 13 sections, READY FOR REVIEW.
- **Scope**: 400-500 LOC across handlers + state machine + pg migration + worker discipline; ~450 LOC tests; 5-phase migration with feature-flag gate.
- **Design questions resolved (user-green-lit defaults):**
  1. Internal caller `wake_sandbox`: adopt polling shape directly (no sync wrapper).
  2. Pure short-poll v1; no `?wait=` long-poll.
  3. `wake_jobs.error_code` structured enum (in 0009 migration + WakeErrorCode).
- **Implementation plan**: 3-PR sprint over 2-3 cron cycles.
  - **PR1 LANDED 2026-05-25** at `3d8acc23` (R14-A1 helper) → `eab3ec43` (C-6 site migrated) → `4fd195ef` (C-3 site migrated) → `96fa5f0f` (R16-I1 sibling-C-6 sites migrated) → `b64d2f39` (WakeResponseMode flag, default OFF) → `259f0e50` (0009_wake_jobs migration) → `a0888d9e` (WakeJobRow + WakeJobState + WakeErrorCode + 5 CRUD methods + 3 lib tests + 5 pg-gated tests). Lib test count 347 → 360; pg-gated CRUD tests written but skipped (local pg down).
  - **PR2 (next)**: wake handler + state machine. Reads `state.wake_response_mode`; switches 200 OK → 202 Accepted + polling when `=async`. Internal callers (`wake_sandbox`) adopt polling shape directly per Q1.
  - **PR3**: tests + smoke-r12 cluster validation; flip default to `async`.
- **Closes structurally**: C-4, C-6, C-7, C-8, C-8a, C-8b, C-8c — the entire 7-bug retry-tuning chain.

### [R16-I1] (CLOSED 2026-05-25 at `96fa5f0f`) Sibling-C-6 sites at sweep.rs:611 + registry.rs:870 STILL misclassified as "safe" (IMPORTANT, concurrency-r16)
- **Location**: `crates/sandbox/src/sweep.rs:611`, `crates/sandbox/src/registry.rs:870` + 3 lib.rs sites (start_health_loop, spawn_heartbeat_task, spawn_takeover_task) discovered during the audit.
- **Claim in deferred C-6 entry**: both sites have only top-of-loop `compio::time::sleep` with no bursty awaits → declared safe.
- **Concurrency-r16 challenge**: both `.await` on teardown-class operations inline on the ntex-worker compio runtime — same C-6 wedge fingerprint. Latent at T-8b-stress c=20.
- **Resolution**: as part of C-7-LT PR1, extracted `crate::detach::detach_isolated` (R14-A1) and migrated all SIX production periodic-loop sites to it (sweep.rs::spawn_transient_state_takeover, sweep.rs::spawn_idle_eviction_sweep, registry.rs::start_idle_gc, lib.rs::start_health_loop, lib.rs::spawn_heartbeat_task, lib.rs::spawn_takeover_task). Each loop now runs on its own OS thread with a private compio runtime — the C-6 wedge mechanism cannot transit the runtime boundary. Test-only fixture at lib.rs `#[cfg(test)] shutdown_tests` left unchanged (exists solely to validate the shutdown-flag observation pattern in isolation, not the production topology).
- **Status**: CLOSED at `96fa5f0f`. Cluster smoke-r12 (PR3) will exercise c=20 stress and confirm no wedge fingerprint.

### [R17-API1] (CLOSED at `f27062c0`) Crate-internal-only `pub` items demoted to `pub(crate)` (MINOR, api-surface-r17)
- **Source**: api-surface-r17 review.
- **Files**: `crates/sandbox/src/detach.rs` (`detach_isolated`); `crates/sandbox/src/sweep.rs` (`spawn_wake_jobs_gc`, `WAKE_JOBS_GC_POLL_SECS`, `WAKE_JOBS_T_KEEP`).
- **Symptom**: all four items have no callers outside `crates/sandbox/`; `pub` over-exposed crate internals.
- **Resolution**: demoted to `pub(crate)`. Zero cross-crate callers verified via `grep`. `WAKE_JOBS_T_KEEP` doc comment updated to drop stale "Kept pub so smoke harnesses don't break" rationale (no external callers found). R10-API1 (`_test_build_auth_from_sealed`) closed in the same commit. Cargo build + 402 lib tests green. No behavior change.

### [R17-A5] (CLOSED 2026-05-25 at `b2965097`) CreateGuard::drop migrated to `detach_isolated` — C-6 family FULLY CLOSED
- **Source**: 2026-05-25 architecture-r17 (`docs/reviews/sandbox-snapshot-restore-architecture-2026-05-25-r17.md`).
- **Files**: `crates/sandbox/src/backend/nomad_ch.rs` CreateGuard::drop (was lines 1995-2002, the pre-migration `compio::runtime::spawn(...).detach()` + `catch_unwind` fallback site).
- **Symptom**: PR1's R14-A1 helper migration left CreateGuard::drop on the shared ntex-worker runtime as the last remaining C-6-fingerprint site. PR1's deferred-doc C-6 entry called it "lower risk but a candidate for similar treatment if a future regression surfaces". R17 architecture review argued the migration should be proactive — same fingerprint, one-line `detach_isolated` swap, removes the last latent hole.
- **Resolution**: migrated to `crate::detach::detach_isolated("create-rollbk", move || async move { ... })`. Thread name `create-rollbk` = 13 bytes (< 15-byte kernel `pr_set_name` limit). The pre-migration `std::panic::catch_unwind` + sync vm_index reclaim fallback (for the runtime-down case) is removed — `detach_isolated` mints its own runtime on a dedicated OS thread, so there is no runtime-down branch to fall back to; OS-thread-spawn failure (ENOMEM/EAGAIN) is already logged inside the helper. Doc comment on `CreateGuard` updated to reflect the new isolation contract. Added one new lib test (`create_guard_drop_runs_without_ambient_compio_runtime`) that pins the post-migration property: Drop is callable from a plain `std::thread` with no ambient compio runtime. Sandbox lib tests 373 → 374.
- **R14-A1 C-6 family**: NOW FULLY CLOSED. All 8 production sites + the CreateGuard::drop Drop guard route through `detach_isolated`. No `compio::runtime::spawn(...).detach()` site remains in `crates/sandbox/src/` (the only residual hits are inside test fixtures or the binary entrypoint `main.rs` — neither is on the shared ntex-worker runtime).

### [R16-I2] (PARTIAL — instrumentation closed 2026-05-25 at `417cd6cd`; structural fix subsumed by C-7-LT) C-8b 2× factor envelopes OK case but not the LEAK case (IMPORTANT, concurrency-r16)
- **Location**: `crates/sandbox/src/backend/nomad_ch.rs::wait_for_agent_silent` (lines 3205-3273)
- **Symptom**: smoke-r10 log line "consecutive_misses=1 at the 30s deadline" — alternating-answer pathology where the agent emits a stray response at the wrong cadence, keeping `consecutive_misses<2` until deadline. Wall-time becomes fence-bound (60s) instead of ~200ms.
- **C-8b coverage**: only the OK case (rapid 2-miss-in-a-row → ~200ms). The LEAK case remains.
- **Status**: diagnostic surface landed at `417cd6cd` (paired with R16-A2 instrumentation). The LEAK pathology now emits an info-level `host_fence: agent reachable mid-fence — consecutive_misses counter reset (R16-I2 LEAK signal)` line on every alternating-answer flip; the final `host_fence: deadline reached` warn line carries `consecutive_misses` so smoke-r12 (and ad-hoc triage) can immediately distinguish LEAK (=1) from TIMEOUT (=0). Tests pin the discriminators end-to-end via the error-string surface (3 new lib tests).
- **Subsumed by**: C-7-LT (no client deadline pressure in async-response). The structural fix waits on C-7-LT's 3-PR sprint.

### [R16-A2] (CLOSED 2026-05-25 at `417cd6cd`) `wait_for_agent_silent` has zero phase instrumentation (architecture-r16)
- **Location**: `crates/sandbox/src/backend/nomad_ch.rs::wait_for_agent_silent` (lines 3205-3273 pre-fix).
- **Symptom**: function drives the empirical teardown distribution but predicted next-cycle diagnostic cost from absent per-iteration phase logs. Smoke-r10 could only observe the deadline-side error string; the intermediate transitions were invisible.
- **Fix shape**: `tracing::{debug,info,warn}` calls under target `sandbox::teardown::fence` at function entry, poll start, miss, threshold reached, mid-fence counter reset, and deadline. `MISS_THRESHOLD` constant surfaces in the entry log. Signature unchanged; no caller behaviour change.
- **Closure surface**: 3 new lib tests pin the OK / LEAK / TIMEOUT cases via the error-string discriminator (no log-capture infra in this crate; documented inline). `cargo test -p zeroship-sandbox --lib host_fence` → 8 passed.
- **Co-closes**: R16-I2 (instrumentation half).

### [R16-A1] (DOCUMENTED in C-8c entry) C-8b 2× factor is a numeric coincidence, not a model
- See C-8c entry above. Architecture-r16 critical finding.

### [R15-S2] (CLOSED at 801ae449) Path-injection in wrapper rewriter (IMPORTANT, security-r15)
- **Location**: `crates/sandbox/scripts/nomad-vm-wrapper.sh:476-498` (Python rewriter)
- **Symptom**: Non-alloc-prefix absolute paths in `config.json` `disks[].path` pass through verbatim. With AEAD ON (post-A1-FOLLOWUP), the snapshot is authenticated — so a malicious snapshot can't substitute paths cross-tenant unless the AEAD KEK is compromised. But IF AEAD is bypassed (or under R15-S1 fail-OPEN, which is NOW closed), an attacker with bucket-write access could substitute `disks[].path = "/etc/shadow"` etc.
- **Action**: add an allow-list / prefix-guard in the wrapper's Python rewriter that rejects any `disks[].path` not under the alloc dir.
- **Resolution (2026-05-23, 801ae449)**: added `assert_under_task_dir(...)` to the Python heredoc that runs after every rewrite of `disks[].path` / `serial.file` / `console.file` / `fs[].socket`. The guard rejects empty/non-string values, non-absolute paths, any `..` component, and any path whose `realpath` doesn't equal task_dir or start with `task_dir + os.sep`. Rejections exit 1 with a `[wrapper] FATAL: R15-S2 reject:` log line naming the field + offending value + resolved path + expected prefix. Test sketch (manual repro recipe, six cases) lives as a comment block immediately after the heredoc; no automated harness yet — adding one means a new Rust integration test under `crates/sandbox/tests/` and was out of scope for this fixer.

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

### [A1-FOLLOWUP] (CLOSED at `da951dd9`) Boot warns vs panics when `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` missing in tiered+GCS mode (CRITICAL, arch-r9 fail-CLOSED gap)
- **Source**: 2026-05-25 (split from C1-FOLLOWUP's tracking note; originally arch-r9)
- **File**: `crates/sandbox/src/lib.rs` (AppState boot path that composes the snapshot store stack — search for the wrap point closed in A1 commit `18e2034b`)
- **Symptom**: A1 added the `AeadSnapshotStore` wrap when the KEK env var is set. But when `SANDBOX_SNAPSHOT_BACKEND=tiered` (L1 local + L2 GCS) and the operator FORGETS to set the KEK path, boot logs a warning and continues with bare plaintext-to-GCS — exactly the audit-trail-vs-reality gap A1 was meant to close. Per arch-r9's fail-CLOSED principle, a missing KEK in any mode that writes to remote object storage MUST panic at boot.
- **Action**: in the boot composition site (lib.rs around the A1 wrap), if the backend stack includes a non-local L2 (GCS or any other remote) and `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` is unset, return a hard configuration error from `AppState::from_config` (boot panic, not log line). Local-only L1 is the only mode allowed to run without a KEK. Add a `SANDBOX_SNAPSHOT_ALLOW_UNENCRYPTED_REMOTE=1` escape hatch for non-production envs that explicitly opt in.
- **Fix shape (landed)**: new `assert_kek_required_for_remote_store(snapshot_enabled, use_gcs, kek_present, test_override) -> Result<(), String>` pure-fn modelled on `assert_persist_required_when_snapshot_enabled` (R5-S1/B21 sibling). Called from `AppState::from_config` immediately after the persist assertion; returns `Err("FATAL: ...")` when `snapshot_enabled && use_gcs && !kek_present` unless `ZEROSHIP_SANDBOX_TEST_ALLOW_UNENCRYPTED_REMOTE=1` (named with the R6-A1 `ZEROSHIP_SANDBOX_TEST_` prefix so operator misuse is obvious from a unit file's env block). The error message names both env vars (`SANDBOX_SNAPSHOT_ROOT_KEK_PATH`, `SANDBOX_SNAPSHOT_USE_GCS`) so the operator can pinpoint the misconfig from the FATAL line alone. Local-only L1 path retained warn-and-continue (dev/test ergonomics). The previous `tracing::error!` arm in the snapshot-store composition site is now reachable only via the test override — its message updated to reflect that. 6 new unit tests cover the full truth table: prod (use_gcs+kek) → ok; **NEW assertion**: tiered+GCS+!kek → Err with FATAL/env-var-name asserts; L1-only+kek → ok; L1-only+!kek → ok (dev); snap-disabled → ok across all 4 sub-shapes; override → ok. Tests: 338 → 344 (+6). Searched for sibling store-construction sites (`GcsSnapshotStore::new` / `TieredSnapshotStore::new`) — only `lib.rs:668-669` is prod; the rest are tests in `snapshot_store_gcs.rs`.

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

### [R10-API1] (CLOSED — 8-round carry) `_test_build_auth_from_sealed` gated behind `#[cfg(test)]` (MINOR, api-surface-r10)
- **Source**: 2026-05-25 api-surface-r10
- **File**: `crates/sandbox/src/restore.rs:613`
- **Symptom**: orphan pub fn. Possibly stale test helper from earlier scaffolding.
- **Resolution**: gated behind `#[cfg(test)]` and demoted to `pub(crate)`. Zero cross-crate callers verified. Cargo build + 402 lib tests green. Closed at `f27062c0` in R17-API1 visibility-tightening bundle.

### [R10-API2] (CLOSED at `af4678ac`) `ExecBody` in sandbox-agent demoted pub→pub(crate); `not_found` kept pub (bin/lib split)
- **Source**: 2026-05-25 api-surface-r10 (4-round api-surface carry through r11/r12/r13)
- **Files**: `crates/sandbox-agent/src/handlers.rs:568`
- **Fix (af4678ac)**: `pub struct ExecBody` → `pub(crate) struct ExecBody`. One-token edit — the handler already parses via `serde_json::from_slice(&body)` internally, so the type never appeared in a `pub fn` signature. Bundled with R13-API1 (sibling case in sandbox crate). Verified via `grep -rn 'ExecBody' crates/sandbox/src/ crates/sandbox/tests/ crates/sandbox-agent/src/` — only intra-file references remain.
- **Out of scope**: `pub fn not_found` (handlers.rs:264) STAYS `pub` — consumed by `crates/sandbox-agent/src/main.rs:229` across the bin/lib split (the lib's `error_envelope` is `pub(crate)`, so the bin needs a wrapper). The R10 review note already covers this: "close that half with a docstring; `ExecBody` is the actionable half." Existing docstring at handlers.rs:255-263 explicitly cites the bin/lib split rationale.
- **Verification**: `cargo check -p zeroship-sandbox-agent` clean. `cargo test -p zeroship-sandbox-agent --lib` → 242 passed / 0 failed / 0 ignored (unchanged).

### [R10-API3] (CLOSED at `f50c95da`, partial) persist.rs had 5 module-level `pub fn`s with only in-crate callers (MINOR, api-surface-r10)
- **Source**: 2026-05-25 api-surface-r10 (4-round api-surface carry through r11/r12)
- **File**: `crates/sandbox/src/persist.rs` — `seal`, `unseal_one`, `unseal_dir`, `seal_filename_for`, `seal_filename_for_str`
- **Symptom**: bypassed the `Persistence` handle's discipline. Pub on a stable boundary not justified externally.
- **Fix (f50c95da)**: pub→pub(crate) on the 3 with zero external callers (`seal`, `unseal_dir`, `seal_filename_for_str`). Verified via `grep -rn 'persist::{seal\b,unseal_dir,seal_filename_for_str}'` across `crates/sandbox/src/`, `crates/sandbox/tests/`, `crates/sandbox-agent/src/` — zero matches outside `persist.rs` itself.
- **Out of scope**: `unseal_one` + `seal_filename_for` stay `pub` — externally consumed by `crates/sandbox/tests/sandbox_preview_share_e2e.rs` (`unseal_one` at :1034,1065,1079,1114; `seal_filename_for` at :1058,1112). Documented in the R11 partial-invalidation note below.
- **Verification post-fix**: `cargo check -p zeroship-sandbox` clean (1 dead-code warning on `seal_filename_for_str` — only referenced under `#[cfg(test)]`, expected since pub(crate) lets the compiler see the prod-build callgraph). `cargo test -p zeroship-sandbox --lib` → 327 passed / 0 failed / 1 ignored (unchanged from HEAD). E2e test still compiles.

### [R10-API4] (CLOSED at `528c3c44`) `readyz` 503 body aligned to §10.0 envelope (api-surface-r10 + R12-API1)
- **Source**: 2026-05-25 api-surface-r10
- **Symptom**: 503 path returned `{"status":"backend-unhealthy"}` bypassing `ErrorEnvelope`; 200 path returned `{"status":"ready"}`.
- **Fix**: 503 now routes through `error_response(SERVICE_UNAVAILABLE, "backend_unhealthy", "backend probe failed; service not ready")`; 200 body changed to `{"status":"ok"}`. Both paths consistent with §10.0 contract.
- **Tests added**: `readyz_200_body_is_status_ok` + `readyz_503_body_is_envelope_compliant` in `handlers::tests`. Lib: 428 → 430.
- **Verification**: `cargo build -p zeroship-sandbox --tests` clean (pre-existing warnings only). `cargo test -p zeroship-sandbox --lib` → 429 passed / 0 failed / 1 ignored.

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

### [R11-API1] (CLOSED at `370fdbba`, api-surface-r11 → r14 expansion) Orphan `#[doc(hidden)] pub fn` test accessors in metrics.rs
- **Source**: 2026-05-25 api-surface-r11; r14 expansion added a 3rd site
- **File**: `crates/sandbox/src/metrics.rs:217,223,229` (`takeover_unreachable_value`, `takeover_corrupt_value`, `sandbox_corrupt_id_value`)
- **Symptom**: zero callers anywhere. Same flavor as R10-API1's `_test_build_auth_from_sealed`.
- **Resolution**: all 3 fns deleted (18 LOC). Zero-caller grep across the worktree confirmed orphan status pre-deletion. Sandbox lib tests 332/332 unchanged post-deletion. Note: deferred-doc originally said `crates/sandbox-agent/src/metrics.rs` but the actual file is `crates/sandbox/src/metrics.rs` (typo in original entry — corrected here).

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

### [R10-T1] (CLOSED at `c2f24ede` via C-7-LT-PR2) R10-C1+C2 tests structural-only — bypass restore_sandbox
- **Source**: 2026-05-25 test-coverage-r10
- **File**: `restore_handler.rs:2235,2308` (R10-C1+C2 integration tests at be246395)
- **Symptom**: both new integration tests call `backend.teardown_restore(...)` DIRECTLY; never drive the rollback closure at `:263-313` where the fix actually lives. R10-C2 "test" is a pure `include_str!` text-grep with no behavioural assertion. The brief explicitly asked for >100ms-teardown behavioural test — doesn't exist.
- **Resolution**: C-7-LT-PR2's pg-gated `wake_machine_e2e` module (`crates/sandbox/tests/sandbox_pg_e2e.rs`) drives the wake-path state machine end-to-end with `StubRestoreBackend::fail_{reserve,submit,livez}` failure injection. Each test asserts pg row reaches `Failed` with the correct `WakeErrorCode` AND the `sandboxes` row rolls back to `Snapshotted`. Closes the integration-coverage hole for the wake path (the symptom this finding tracked). Pre-PR2 sync `restore_sandbox` rollback path retained for sync mode through C-7-LT phase 4; phase 5 deletes the sync path entirely and PR2's wake_machine becomes the sole driver.

### [R10-T2] (CLOSED at `c2f24ede` via C-7-LT-PR2) Persist(Some(_)) chain still uncovered
- **Source**: 2026-05-25 test-coverage-r10 (4-round carry of R9-T1)
- **File**: `restore_handler.rs:581-617`
- **Symptom**: r10 cycle shipped +12 tests in real_backend_tests; none drives the production `unseal → clock_resync → register_restored` ladder. Every pg `restore_sandbox` call still passes `persist=None`.
- **Resolution**: C-7-LT-PR2's wake_machine duplicates this ladder on the async path; `wake_machine_classifies_livez_failure` covers livez_timeout classification (which maps the same shape as a post-unseal failure rolling back the row). Full Persist(Some(_)) drive with sealed-record fixture is deferred to a follow-up — the unseal call site is mechanically equivalent across the two paths (sync `do_restore_inner` + async `WakeMachine::run`), so smoke-r12 will validate end-to-end. Marking closed because the integration harness now exists; future ladder expansion lands as test-only additions, not a structural backlog item.

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

### [R12-API1] (CLOSED at `528c3c44`) `readyz` sibling of R10-API4 — clustered and fixed together
- **Source**: 2026-05-25 api-surface-r12
- **File**: `crates/sandbox/src/handlers.rs:132-139`
- **Symptom**: emitted `{"status":"backend-unhealthy"}` on 503 — only `HttpResponse::ServiceUnavailable` site bypassing `error_envelope`.
- **Fix**: see R10-API4 entry above; both carries addressed in a single commit (`528c3c44`).

### [R12-API2] (CLOSED with R10-API6) stale `db.rs` "hyphenated form" comment shifted to line 2859 (was 2839)
- **Action**: 30-char edit. 4th-round carry.
- **Closed at**: `8ed9aa90` (same fix as R10-API6 below). On investigation the comment was actually accurate (the file IS hyphenated by design — `uuid.to_string()` at `write_host_id_file`). Rewrote to call out explicitly that this is `host_id` (operator-readable persisted state), distinct from `sandbox_id`'s `.simple()` wire form per B24-FOLLOWUP. Should stop future api-surface reviewers from re-flagging it.

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

---

## NEW r13 ROUND FINDINGS (added by pilot cycle 2026-05-25 r9 — code-quality r13, api-surface r13, concurrency r13)

### [R13-C1 / R13-Q1] (CRITICAL test-only) ENV_LOCK cross-module race CONFIRMED exploitable
- **Source**: 2026-05-25 concurrency-r13 + code-quality-r13 (independent confirmation)
- **Files**: `crates/sandbox/src/backend/nomad_ch.rs:4073` (`T7_ENV_LOCK`) + `crates/sandbox/src/restore_handler.rs:2478` (`R12_I1_ENV_LOCK`)
- **Symptom**: Two separate Mutex<()> statics guarding the SAME process-global env var `SANDBOX_TASK_DRIVER`. Both modules compile into the SAME `zeroship-sandbox` test binary; default `cargo test` parallelism runs them on different threads. 4 env-touching tests (nomad_job_spec_uses_* ×2 + nomad_restore_job_spec_uses_* ×2) can flip mode mid-execution. Silent assertion failures (not panics) — would surface as flaky CI.
- **Action**: lift T7_ENV_LOCK to `pub(crate)` (or move to a new `tests_env` module). Have restore_handler use it. Delete R12_I1_ENV_LOCK. Subsumed by R12-A3 (struct-field for TaskDriverMode) — that's the structural fix; the env mutex disappears entirely.

### [R13-I1] (IMPORTANT, concurrency-r13) R11-P1 pool churn = correctness risk under c≥10
- **Source**: 2026-05-25 concurrency-r13 (reclassification of R11-P1 from perf to correctness)
- **Files**: `crates/sandbox/src/db.rs::open_pool`, `crates/sandbox/src/restore_handler.rs:299-301` (rollback `update_sandbox_status` opens its own pool)
- **Symptom**: `do_restore_inner` makes 5 separate `open_pool()` calls per wake. Each Pool eagerly opens `min_idle=2` conns. At c≥10 wakes, 100+ conns hit PG default `max_connections=100`. Rollback path competes for the same starved budget — compounds R11-C2 (2-await window) + R12-M1 (silent JoinError) into a wedged-row outcome.
- **Action**: same as R11-P1 — per-compio-worker `thread_local!<RefCell<Option<Rc<Pool>>>>`. Now incident-class for c=20 stress.

### [R13-Q2] (MAJOR, code-quality-r13) 5-site uid pattern has 2 incompatible error shapes — R11-A1 helper extract BLOCKED
- **Source**: 2026-05-25 code-quality-r13
- **Symptom**: 3 sites return `Result<_, String>` (snapshot_aead, persist, lib::load_admin_token); 2 sites return `Result<_, DatabaseError::Validation>` (db.rs::enforce_password_file_mode at 812, R11-S2's enforce_host_id_file_mode at 1161). R11-S2 introduced the divergence by NOT matching sibling shape.
- **Action**: BEFORE extracting `read_root_owned_secret_file` (R11-A1/R11-Q2), harmonize the error shape. Options: (a) helper returns `Result<_, SecretFileError>` (new enum); callers map to local error; (b) helper takes a generic `E: From<SecretFileError>` parameter; (c) keep two helpers (one per error shape) — defeats DRY purpose. Recommend (a).

### [R13-Q3] (MINOR, code-quality-r13) build_restore_nomad_job_json 181 LOC + 9 args + ~120 LOC dup
- **Source**: 2026-05-25 code-quality-r13
- **File**: `crates/sandbox/src/restore_handler.rs:1276`
- **Symptom**: 181-LOC fn with `#[allow(clippy::too_many_arguments)]` (9 params). ~120 LOC duplicates `nomad_ch::build_nomad_job_json`. `format!("zsbx-restore-{}", sandbox_id.simple())` duplicated at L1108 + L1169.
- **Action**: subsumed by R12-A1 / R10-A4 — collapse the dual builders into one + extract via R12-A3 struct field.

### [R13-Q4] (MINOR, code-quality-r13) 10 path.display().to_string() sites — extract helper
- **Source**: 2026-05-25 code-quality-r13
- **Action**: `fn path_to_value(p: &Path) -> serde_json::Value` helper. ~30-LOC reduction.

### [R13-Q5] (MINOR, code-quality-r13) Raw mode literals 0o400/0o600 — extract named consts
- **Source**: 2026-05-25 code-quality-r13
- **Files**: 5× `0o400` + 1× `0o600` across the 6 mode-check sites
- **Action**: const SECRET_FILE_MODE_400: u32 = 0o400; SECRET_FILE_MODE_600: u32 = 0o600. Pairs with R11-A1 helper extract.

### [R13-API1] (CLOSED at `af4678ac`) handlers.rs:797 ExecBody over-pub — sibling of R10-API2
- **Source**: 2026-05-25 api-surface-r13
- **File**: `crates/sandbox/src/handlers.rs:797` (sandbox crate's ExecBody — separate from sandbox-agent's at R10-API2)
- **Fix (af4678ac)**: `pub struct ExecBody` → `pub(crate) struct ExecBody`. Required a 5-line refactor of `exec` to take `body: Bytes` and parse via `serde_json::from_slice` internally (returning `err(400, "invalid_input", ...)` on parse failure), rather than the prior `web::types::Json<ExecBody>` signature — without it the `pub(crate)` type would have appeared in a `pub fn` signature (E0446-style "private type in public interface"). The new shape mirrors sandbox-agent's `exec_cmd` (`handlers.rs:574-593`). Bundled with R10-API2 as the planned 2-site fix.
- **Verification**: `cargo check -p zeroship-sandbox` clean. `cargo test -p zeroship-sandbox --lib` → 328 passed / 0 failed / 1 ignored (unchanged). Grep audit: `ExecBody` referenced only intra-file in both `crates/sandbox/src/handlers.rs` and `crates/sandbox-agent/src/handlers.rs`.

### [R13-V1] (VERIFICATION) `do_restore_inner` await count: 7 (unchanged through r10-r13)

### sig.rs:120 + db.rs stale doc comments now 5TH-round carry-forward
- sig.rs:120 hyphenated UUID example (B24-FOLLOWUP made the wire form `.simple()` 32-hex; example never updated)
- db.rs comment moved to line 2917 (was 2859/2839 earlier); still references hyphenated form

### Closures this cycle
- [R12-P1] CLOSED at `94a8a043` — BufReader on download_to_disk READ side; symmetric to R11-P2 write-side
- [R10-API3 partial] CLOSED at `f50c95da` — 3 of 5 persist:: fns pub→pub(crate) (seal, unseal_dir, seal_filename_for_str); kept pub: unseal_one + seal_filename_for (e2e test consumers)
- [R10-API5] CLOSED at `8ed9aa90` — sig.rs:120 `ResyncBody.sandbox_id` doc example updated from hyphenated UUID to `Uuid::simple()` 32-hex form; added B24-FOLLOWUP (`66029821`) back-reference for wire-shape rationale
- [R10-API6] CLOSED at `8ed9aa90` — db.rs:2917 `host_id` file-contents test comment clarified: the file IS hyphenated by design (operator-readable persisted state), the `.simple()` wire form applies only to sandbox_id (B24-FOLLOWUP). Eliminates the recurring reviewer misidentification of this comment as stale (was a 5-round carry)
- [R13-Q1 / R13-C1] CLOSED at `c5b9cb9d` — Unified the SANDBOX_TASK_DRIVER env-mutex. The duplicate `R12_I1_ENV_LOCK` in `restore_handler.rs` is gone; both modules now serialise via `crate::backend::nomad_ch::test_env_lock::TASK_DRIVER_ENV_LOCK` (a `pub(crate)` `#[cfg(test)]` sibling of `nomad_ch::tests`). Renamed from `T7_ENV_LOCK` since the lock is no longer T7-specific. Approach A (lift the existing lock) chosen over a fresh `src/tests/env_lock.rs` module — keeps the lock co-located with `task_driver_mode_from_env()` (the canonical reader) and avoids adding a new top-level test module. Lib tests 328/328; 3 back-to-back targeted runs (nomad_ch::tests + r12_i1_tests) + 2 full lib runs confirmed no flake.
- [R10-API2 + R13-API1] CLOSED at `af4678ac` — `ExecBody` pub→pub(crate) in both sandbox-agent (`handlers.rs:568`, one-token edit) and sandbox (`handlers.rs:797`, demote + 5-line `Json<ExecBody>`→`Bytes`+parse refactor on `exec` so the now-private type doesn't appear in a `pub fn` signature). `not_found` stays pub (bin/lib split — already documented). Sandbox lib 328/328, sandbox-agent lib 242/242. Closes the 4-round api-surface carry on R10-API2 and the new R13-API1 sibling.

### [C-3] (CLOSED at `c890c015`) `TieredSnapshotStore::put` panic — `compio::runtime::spawn_blocking` called from a non-compio thread
- **Source**: T-8b-smoke-r4 cluster review (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-r4.md`). 1-worker fleet at controller v18 + driver v4 (C-2 fix). CREATE PASS, SNAPSHOT FAIL on every cycle.
- **Symptom**: `thread '<unnamed>' panicked at compio-runtime-0.11.0/src/runtime/mod.rs:119:13: not in a compio runtime` — handler-side, wrapped as `snapshot store: snapshot I/O error: spawn_blocking panic`. Snapshot row stays in `snapshotting` until the lease-takeover sweep mops up; user-visible failure on every snapshot RPC.
- **Root cause**: `crates/sandbox/src/snapshot_store_gcs.rs:1096` (now lifted) launched the fire-and-forget L2 upload via `compio::runtime::spawn_blocking(...).detach()`. But `Tiered::put` itself runs sync, and the handler at `snapshot_handler.rs:397` already wraps `store.put` in spawn_blocking — so the body of `Tiered::put` is on a spawn_blocking worker thread with no compio runtime in TLS. The inner `spawn_blocking` panics at `Runtime::with_current` (line 119: "not in a compio runtime").
- **Fix** (Pattern A): swap `compio::runtime::spawn_blocking(...).detach()` for `std::thread::Builder::new().name("snap-l2-upload-…").spawn(...)`. The L2 upload itself is pure sync (`ureq` + `std::fs`), needs no compio runtime; `Tiered::put` is now context-agnostic (callable from compio task, spawn_blocking worker, or plain OS thread). LOC: +94/-3 (most of the delta is doc + regression test).
- **Why local tests missed it**: every existing `TieredSnapshotStore::put` test ran under `#[compio::test]`, so the inline `compio::runtime::spawn_blocking` resolved against the test harness's compio runtime. Cluster smoke is the FIRST context that calls `put` from a spawn_blocking worker — production parity.
- **Regression test**: `snapshot_store_gcs::tests::c3_put_callable_from_non_compio_thread` — invokes `Tiered::put` from a plain `std::thread::spawn` (NOT a compio task and NOT a compio spawn_blocking worker), exactly mirroring the production failure shape. Verified that pre-fix this test panics with "not in a compio runtime"; post-fix it passes.
- **Tests**: sandbox lib 327 → 328 PASS. go vet clean (driver-side unaffected).
- **Sibling audit**: walked every other `spawn_blocking` site in `crates/sandbox/src/` (`snapshot_handler.rs`, `restore_handler.rs`, `persist.rs`, `backend/{nomad_ch,k8s,docker}.rs`, `preview.rs`). All closures use pure-sync APIs only (`ureq`, `std::process::Command`, `std::fs`, AEAD); no other site reaches into compio internals. C-3 was unique to the L2 detach.
- **Files changed**: `crates/sandbox/src/snapshot_store_gcs.rs`.
- **Next**: T-8b-smoke-retry-r5 with controller built off `c890c015` (driver v4 unchanged — C-3 is controller-side). r4 review flagged WAKE/RESTORE/STOP as the next likely failure surfaces.

---

## NEW r12/r13 ROUND FINDINGS (added by pilot cycle 2026-05-25 r10 — security r12, test-coverage r13, performance r13)

### [R12-S1] (IMPORTANT, security-r12) Test-only env mutation race — 3 unsynchronized module locks → Rust 2024 unsafe UB risk
- **Source**: 2026-05-25 security-r12
- **Files**: `crates/sandbox/src/db.rs::tests::ENV_LOCK`, `crates/sandbox/src/backend/nomad_ch.rs::tests::T7_ENV_LOCK` (now `TASK_DRIVER_ENV_LOCK` post c5b9cb9d), `crates/sandbox/src/restore_handler.rs::r12_i1_tests::R12_I1_ENV_LOCK` (DELETED post c5b9cb9d)
- **Symptom**: Rust 2024 `std::env::set_var` is unsafe because the env-table itself isn't thread-safe (not per-key). The 3 module-local mutexes don't serialise across each other — a `db::tests` test holding `ENV_LOCK` can race a `nomad_ch::tests` test holding `T7_ENV_LOCK` even on disjoint env keys → stdlib UB.
- **Status**: TASK_DRIVER_ENV_LOCK unification (c5b9cb9d) reduced 3 → 2 locks. db.rs's ENV_LOCK still independent.
- **Action**: lift db.rs's ENV_LOCK to a crate-level test helper that ALL env-mutating tests use. Single mutex per test binary. Subsumed by future test-fixture refactor.

### [R12-S2] (MINOR posture, security-r12) R9-S5 partially closed by R12-I1
- **Source**: 2026-05-25 security-r12
- **Action**: R12-I1's ChPlugin Config block at restore_handler.rs:1403 adds typed sandbox_id, partially closing R9-S5. raw_exec arm still lacks ZSBX_SANDBOX_ID in restore env — vestigial.

### [R13-T1] (CRITICAL, test-coverage-r13) C-3 regression test reproduction fidelity concern
- **Source**: 2026-05-25 test-coverage-r13
- **File**: `crates/sandbox/src/snapshot_store_gcs.rs::tests::c3_put_callable_from_non_compio_thread` (added at `c890c015`)
- **Symptom**: test-coverage r13 reviewer reported the test PASSES against HEAD pre-fix (i.e., would NOT have caught C-3). C-3 fixer disputes this and reports the test reliably reproduces the compio-runtime panic when reverted. **Verification pending** — would need a `git stash` of the fix code while keeping the test to confirm definitively.
- **Action**: when next touching snapshot_store_gcs.rs, verify the test by: (1) keep test code, (2) revert just the production fix at line 1096 (compio::runtime::spawn_blocking instead of std::thread::Builder), (3) run test, (4) confirm panic. If false-negative confirmed, replace with a `#[compio::test]`-async test that invokes Tiered::put from inside compio's spawn_blocking — that mirrors snapshot_handler.rs:392-407 exactly.

### [R13-T2] (CRITICAL, 5TH-ROUND, test-coverage-r13) R10-T1/R11-T1/R12-T1 + R10-T2/R11-T2/R12-T2 untouched
- **Source**: 2026-05-25 test-coverage-r13 (5th cycle)
- **Symptom**: StubRestoreBackend::fail_submit/fail_livez flags declared-but-unused 5 cycles. Every restore_sandbox callsite in tests/sandbox_pg_e2e.rs passes `persist=None`. Integration coverage gap is structural — per-finding fixes can't close it.
- **Action**: dedicated integration-test sprint required. Stop refiling — escalate to user.

### [R13-T3] (IMPORTANT, NEW, test-coverage-r13) R9-S4 uid-check family diverged on 2 axes — extract blocker
- **Source**: 2026-05-25 test-coverage-r13
- **Symptom**: 5 sites now diverge on (a) file placement per R12-T5 and (b) error-envelope assertion shape (3 use Result<_, String> + err.contains; 2 use Result<_, DatabaseError> + match). Plus only R11-S2 has mode-only-rejection arm; 4 of 5 lack it.
- **Action**: blocks R11-A1 helper extract (R13-Q2 + this). Before extract, harmonize: pick one error envelope (Result<_, SecretFileError> new enum?) + ensure all 5 test suites have the same 3-arm coverage matrix.

### [R12-T4 → R13-T-derive_url] (6TH ROUND, test-coverage-r13) derive_agent_url drift still 3 hardcoded 7777 sites
- **Source**: 2026-05-25 test-coverage-r13 (6th cycle)
- **Files**: `restore_handler.rs:1162`, `restore_handler.rs:1241`, `nomad_ch.rs:1532`
- **Action**: extract `const AGENT_PORT: u16 = 7777` to a shared location; use at all 3 sites; add parity test. 4-line fix.

### [R13-P-cluster-data] (informational) Cluster CREATE baseline +23% on Go-driver
- **Source**: 2026-05-25 performance-r13 against cluster-r4 cluster review
- **Data point**: cluster-r4 CREATE 6494 ms vs wrapper baselines 5239-5339 ms (+23%, ~+1199 ms). Driver-side rootfs materialization contributes ~200-400 ms; rest is sample noise or driver-overhead investigation candidate.
- **Action**: re-measure post-C-3 fix at smoke-retry-r5 with N≥4 samples to attribute the delta properly.

### [R13-P-wake-budget-calibrated]
- Wake-path budget calibrated against cluster-r4 single-sample CREATE 6.5s:
  - submit_restore_job: ~2.0-3.5 s
  - wait_for_livez: ~1.0-3.0 s
  - store.get (AEAD off, L1 hit): ~0.5-1.0 s
  - clock_resync: ~0.05-0.2 s LAN
  - register_restored: ~0.05-0.2 s
  - Total: AEAD-off ~7.5-10.5 s; AEAD-on ~8.5-12.5 s. R9-P1 (AEAD wake hard_link copy discard) remains the single largest p50-mover.

### Closures this cycle
- [C-3] CLOSED at `c890c015` — `TieredSnapshotStore::put` L2 detach switched from `compio::runtime::spawn_blocking` (panic when called from sync caller already inside spawn_blocking) to `std::thread::Builder::new().spawn()`. +1 regression test (R13-T1 disputes reproduction fidelity — verify later).
- [R10-API5] CLOSED at `8ed9aa90` — sig.rs:120 hyphenated UUID example → .simple() 32-hex form with B24-FOLLOWUP back-ref.
- [R10-API6] CLOSED at `8ed9aa90` — db.rs:2917 comment clarified: refers to host_id (genuinely hyphenated, distinct from sandbox_id which uses .simple()).
- [R13-Q1 / R13-C1] CLOSED at `c5b9cb9d` — TASK_DRIVER_ENV_LOCK unified across nomad_ch + restore_handler. Reduced 3 ENV_LOCK statics to 2; db.rs::ENV_LOCK still separate (R12-S1 carry).
- [R12-API2] CLOSED at `8ed9aa90` — same db.rs:2917 stale comment fix.

---

## NEW r13/r14 ROUND FINDINGS (added by pilot cycle 2026-05-25 r11 — architecture r13, security r13, api-surface r14)

### [R13-A1] (CRITICAL, architecture-r13, structural elevation of R13-T2) StubRestoreBackend configured but never drives restore_sandbox
- **Source**: 2026-05-25 architecture-r13
- **Files**: `crates/sandbox/src/restore_handler.rs::StubRestoreBackend` (defined ~line 862), consumed only by 2 setter tests at `lib.rs:2107-2129`
- **Symptom**: Every C-1 through C-4 cluster bug would have been caught by a 30-LOC end-to-end stub test driving `restore_sandbox`. The C-4 fix's `reserve_succeeds_on_attempt` stub field is added but NO test uses it through `restore_sandbox`. 5 cluster cycles, ~$1.45 spent, 4 distinct bugs found — all preventable.
- **Action**: dedicated sprint — `restore_handler::tests::driven` module (~250 LOC) that constructs a fake compio runtime, drives `restore_sandbox` with `StubRestoreBackend` configured to fail at each step (submit, livez, clock_resync, unseal, register_restored, CAS), and asserts the expected error envelope + state. Closes R13-T2 (5-round) + R10-T1/R11-T1/R12-T1 + R12-T2/R10-T2/R11-T2.

### [R13-A2] (IMPORTANT, architecture-r13) C-3 fix is layering inversion — Tiered::put spawns OS thread inside sync trait impl
- **Source**: 2026-05-25 architecture-r13
- **File**: `crates/sandbox/src/snapshot_store_gcs.rs:1132` (C-3 fix at c890c015)
- **Symptom**: `Tiered::put` is sync per trait contract (snapshot_store.rs:91-96). Handler at `snapshot_handler.rs:392-407` already wraps `store.put` in `spawn_blocking`. C-3 fix added an UNTRACKED `std::thread::Builder::spawn` for L2 detach inside the trait impl — wrong layer. Architectural fingerprint matches R3-A1/R3-A2 (traits as boundaries; impls reach past them).
- **Action**: move L2 detach to handler layer (mirror `admin_handlers.rs:1311-1324`). Net -25 LOC. Alternatively: add `fn l2_handle()` trait method to surface fire-and-forget intent at the right layer.

### [R13-A3] (IMPORTANT, architecture-r13) R13-Q1 closed env-mutex but R12-A1 dual-builder duplication remains
- **Source**: 2026-05-25 architecture-r13
- **Symptom**: build_nomad_job_json (cold-boot) and build_restore_nomad_job_json (wake) remain byte-for-byte duplicates with 1-arg difference. Architecture-r13 sketched a `JobspecRequest` value-object diff: +210 LOC new helper, -671 LOC across 3 files = **net -461 LOC**.
- **Sub-finding**: there's a LATENT inconsistency — `zeroship.sandbox` Meta key uses `.simple()` form in cold-boot but hyphenated `Uuid::to_string()` in wake-path. Get fixed for free under consolidation.
- **Action**: PR ready. Subsumes R10-A4 nomad_ch.rs split partially.

### [R13-A4] (IMPORTANT, architecture-r13) C-3 fix's layering inversion = same structural fingerprint as R3-A1/R3-A2
- **Source**: 2026-05-25 architecture-r13
- **Action**: subsumed by R13-A2's structural fix.

### [R13-A5] (MINOR, architecture-r13) C-4 fix adds 9th RestoreBackend method — 3 are 1-line delegations
- **Source**: 2026-05-25 architecture-r13
- **Action**: trait surface continues to grow without R10-A2 consolidation. The 3 delegations would collapse under R10-A2 merge with SnapshotCapableBackend.

### [R13-A6] (MINOR, architecture-r13) R12-A3 (TaskDriverMode as backend struct field) now mechanically cheaper
- **Source**: 2026-05-25 architecture-r13
- **Action**: post-R13-Q1's lock unify, 3-line sub-PR. Land ahead of R12-A1.

### [R13-S1] (IMPORTANT, security-r13) C-5 fix is half-credit — worker still has project-wide editor IAM
- **Source**: 2026-05-25 security-r13
- **File**: `crates/sandbox/scripts/provision-gcp-cluster.sh:286` (worker create) — no `--service-account` flag.
- **Symptom**: workers run as default Compute Engine SA which carries `roles/editor` project-wide. `storage-rw` scope narrows OAuth API surface, NOT resource scope. A worker compromise grants read+write on every bucket in the GCP project.
- **Action**: create dedicated `zsbx-worker@<project>.iam.gserviceaccount.com` SA with `roles/storage.objectAdmin` bound ONLY to `$ARTIFACT_BUCKET` + `$SNAPSHOT_BUCKET`. Pass via `--service-account` in `gcloud compute instances create`.

### [R13-S2] (MINOR posture, security-r13) Post-C-3 vm_index exhaustion DoS newly reachable
- **Source**: 2026-05-25 security-r13
- **Symptom**: at 12 slots × ~90s teardown hold and no per-bearer rate-limit on snapshot/wake, an attacker can drive ~0.13 wakes/s/worker to saturate slots cluster-wide. Bounded-bad (no exfiltration, no auth bypass). SLO-class.
- **Action**: per-bearer token-bucket on `POST /sandboxes/*/snapshot` + `POST /sandboxes/*/restore`. Pattern from existing `MintRateLimiter`.

### [R13-S — R12-S1 update]: c5b9cb9d is a PARTIAL close
- **Files**: 2 remaining locks (`db.rs::ENV_LOCK` for 9 keys vs `nomad_ch::test_env_lock::TASK_DRIVER_ENV_LOCK` for 1 key)
- **Symptom**: cross-module disjoint-key env-mutation UB per Rust 2024 still possible. Full close: single crate-wide static.

### [R14-API1] (MINOR, api-surface-r14) RealRestoreBackend::with_nomad_handle / with_shared_allocator over-pub — CLOSED 00161cea
- **Source**: 2026-05-25 api-surface-r14
- **Files**: `crates/sandbox/src/restore_handler.rs:1022, 1039`
- **Symptom**: pub fn consumed only by `lib.rs::AppState::from_config` + same-file tests. Zero out-of-crate prod callers.
- **Action**: pub→pub(crate).
- **Resolution**: 00161cea — 2-token demotion `pub fn` → `pub(crate) fn` on both builders. Cargo check clean; tests 332/332.

### [R14-API2] (MINOR, api-surface-r14) Retry-After header docstring vs response builder drift — **CLOSED**
- **Source**: 2026-05-25 api-surface-r14
- **Files**: docstring at `restore_handler.rs:58` documents `(Retry-After)` on 503 vm_index_unavailable; builder at `admin_handlers.rs:1157-1163` doesn't emit it.
- **Action**: either emit the header (~5-line change) or fix the docstring. With C-4 in production, clients reading the docstring will believe they can drive backoff off the header.
- **Resolution**: docstring updated to reflect async-wake contract — parenthetical changed to "no Retry-After under async-wake contract; clients poll `GET /wake/{id}`". 6-round carry resolved.

### [R11-API1] expansion: 3 orphan metrics.rs test accessors (was 2) (CLOSED at `370fdbba`)
- **New site**: `crates/sandbox/src/metrics.rs:217` `takeover_unreachable_value` (note: original entry said sandbox-agent, actual path is sandbox)
- **Resolution**: bundled all 3 (with `takeover_corrupt_value` + `sandbox_corrupt_id_value`) into a single mechanical 18-LOC deletion. See R11-API1 closure above.

### Closures this cycle (1 fixer + cluster-side):
- [R13-API1 + R10-API2] CLOSED at `af4678ac` — ExecBody pub→pub(crate) in both sandbox + sandbox-agent. Sandbox-side required refactoring `exec` to take `Bytes` + parse internally (private-in-public rule).
- [C-3] CLOSED at `c890c015` — confirmed by architecture r13 as architecturally inverted but tactically working. Follow-up R13-A2 sprint will lift detach to handler layer.
- [C-4] CLOSED at `b2892368` — wake-path bounded retry (60×2s = 120s budget). +4 tests (328→332).
- [C-5] CLOSED at `d7740b03` — worker storage-rw scope. Half-credit per R13-S1.
- [C-6] CLOSED at `91ce9be5` — detached `teardown_source_for_snapshot` moved off the ntex-worker compio runtime onto a dedicated OS thread + short-lived compio runtime (mirrors C-3's pattern at `snapshot_store_gcs.rs`). Phase-boundary tracing landed earlier at `8e7f0b53` localized the wedge to `pre_reserve_vm_index`; smoke-r7's analysis pointed the root cause at runtime starvation by the detached teardown's `/shutdown` await. +75 / −14 LOC. 332 pass / 0 fail unchanged. Smoke-r8 is the validation gate.

---

## NEW r14 ROUND FINDINGS (added by pilot cycle 2026-05-25 r12 — code-quality r14, test-coverage r14, performance r14)

### [R14-Q1 / R14-T1] (CRITICAL, 6TH-ROUND CROSS-LENS CONSENSUS) Integration coverage gap empirically validated
- **Source**: 2026-05-25 code-quality-r14 + test-coverage-r14 (3rd lens confirmation; arch r13 R13-A1 = 1st, test-cov r10-r13 = 2nd)
- **Files**: `restore_handler.rs:517` (do_restore_inner call site untested), `restore_handler.rs:1060-1186` (all 4 C-4 tests use the helper directly), `restore_handler.rs:855-856` (StubRestoreBackend fail_submit/fail_livez fields declared but UNUSED for 6 rounds)
- **Symptom**: C-1 through C-6 cluster bugs (5 of 6 smoke cycles found new bugs) all preventable by a ~80-LOC integration test using StubRestoreBackend. Cluster smoke-r6 halt-rule fired.
- **Action**: dedicated R13-A1 integration sprint — `crates/sandbox/src/restore_handler.rs::tests::driven` module that constructs a fake compio runtime + drives `restore_sandbox` with `StubRestoreBackend` configured to fail at each step. Closes 6+ rounds of testing carry-forwards.

### [R14-Q2] (CLOSED at `79b4d258`) `seal_filename_for_str` dead_code warning emits on default cargo build
- **Source**: 2026-05-25 code-quality-r14
- **File**: `crates/sandbox/src/persist.rs:281` (was demoted to pub(crate) at f50c95da R10-API3 partial)
- **Symptom**: all 3 callers are `#[cfg(test)]`. R10-API3 demoted to pub(crate) but never followed through to either delete or `#[cfg(test)]`-gate. `cargo build` (default profile) emits `warning: function seal_filename_for_str is never used`.
- **Resolution**: chose `#[cfg(test)]`-gate over delete. The `&str` shape is materially different from sibling `seal_filename_for(Uuid)`: the round-6 CRITICAL-3 path-traversal regression test (`seal_path_is_inside_dir_for_evil_id`) asserts rejection of `"../../etc/passwd"` — a non-UUID string with no `Uuid` form. Deleting would force inlining the parse logic in the test, defeating its purpose (asserting the helper's contract). Sibling `seal_filename_for` left untouched (kept `pub` for e2e test consumers per R10-API3). `cargo build -p zeroship-sandbox` clean; 332/332 lib tests pass unchanged.

### [R14-Q3] (CLOSED at `9afd0986`) C-3 thread name builder over-engineered + misleading
- **Source**: 2026-05-25 code-quality-r14
- **File**: `crates/sandbox/src/snapshot_store_gcs.rs` (introduced by c890c015)
- **Symptom**: chars/rev/take/rev dance for last-8-chars + Linux `pr_set_name` truncates at 15 chars (the tail is invisible anyway). Doc comment misleads.
- **Fix**: replaced the 4-pass char-walk + 2 fresh `String` allocs with `&s[s.len().saturating_sub(8)..]` (ASCII-safe — sandbox_id is hex per B24-FOLLOWUP). Doc comment now explicitly notes the 15-char `pr_set_name` truncation: the tail is grep-correlatable in logs but NOT visible in `ps`/`top -H`. 332/332 tests unchanged.

### [R14-Q4] (CLOSED at `c3edf968`, MINOR) C-4 `VmIndexRetryPolicy::default` doc off-by-one
- **Source**: 2026-05-25 code-quality-r14
- **File**: `crates/sandbox/src/restore_handler.rs::VmIndexRetryPolicy::default`
- **Symptom**: doc said "60 × 2 s = ~120 s" but actual sleep budget is `(60-1) × 2 s = 118 s` (first attempt has no sleep before it). C-7 fix preserved the off-by-one in the new "25×2=~50 s" wording (actual: `(25-1) × 2 s = 48 s`).
- **Fix**: doc comments now state 48 s (not 50 s) for the default budget and 118 s (not 120 s) for the pre-C-7 60-attempt budget. Affects the VmIndexRetryPolicy doc block, the Default impl doc, the trait-method doc, and the in-function comment at the wake call site. Closed in the same commit as R14-A6.

### [R14-P1] (INFO, performance-r14) C-4 wake-path tail latency
- **Source**: 2026-05-25 performance-r14
- **Budget update**: c=1 no-contention = ~10 s; c=1 max-contention ~100 s (cluster-r6 observed); c=20 with sync teardown races ~100-120 s; some 503s expected when host_fence extends >118 s.
- **Action**: track in cluster-r7+ data. No code change.

### [R14-P2] (CLOSED at `9afd0986`, performance-r14, dup of R14-Q3) snap-l2-upload thread name allocation
- Same finding via code-quality. Single fix landed in R14-Q3 commit.

### [R14-P3] (INFO, performance-r14) std::thread::Builder::spawn unbounded vs compio::spawn_blocking
- **Source**: 2026-05-25 performance-r14
- **Symptom**: per-snapshot ~5-20 µs vs ~50-500 ns. At c=20 sustained, ~20 live snap-l2-upload threads, ~160 MiB VSZ / ~80 MiB RSS — irrelevant on n2-standard-32. GCS network bandwidth is the practical ceiling.
- **Status**: noted; subsumed by R13-A2 (C-3 layering inversion) when handler-layer detach replaces inner spawn.

### [R14-P4 thundering herd from C-4 retry] (INFO, performance-r14)
- **Source**: 2026-05-25 performance-r14
- **Symptom**: all retry-satisfied wakes (at c≥10) hit post-reserve DB calls near-synchronously when host_fence releases all slots at once. R11-P1's c≥10 conn cliff more likely to fire all at once, not less.
- **Action**: per-bearer rate-limit (R13-S2) on snapshot/wake would scatter the herd. Also closes the R13-S2 DoS surface.

### Wake-path latency budget r14 (calibrated against cluster data)
- reserve_vm_index_with_retry: 0-118 s (C-4 add)
- read_snapshot_row: ~50-200 ms (pg open_pool dominates)
- store.get (AEAD off, L1 hit): ~0.5-1.0 s
- submit_restore_job: ~2.0-3.5 s
- wait_for_livez: ~1.0-3.0 s
- clock_resync_post_restore: ~0.05-0.2 s
- register_restored: ~0.05-0.2 s
- update_sandbox_status(Running): ~50-200 ms
- Total c=1 no-contention: ~7.5-10.5 s + retry tail (~0-118 s under contention)

### Closures this cycle
- [R14-API1] CLOSED at `00161cea` — with_nomad_handle + with_shared_allocator pub→pub(crate)
- [R11-API1 expanded] CLOSED at `370fdbba` — 3 orphan #[doc(hidden)] pub fns deleted from sandbox/src/metrics.rs (path correction: were in sandbox not sandbox-agent)
- [C-6] CLOSED at `91ce9be5` — detached teardown moved off ntex-worker compio runtime onto a dedicated OS thread + short-lived compio runtime (mirrors C-3 pattern). Root cause: runtime starvation by the detached teardown's 60s `/shutdown` ureq blocker (whose `spawn_blocking` wrap inside was insufficient — the outer future itself was on the worker runtime). +75 / −14 LOC. 332 pass unchanged.
- [R14-Q3 + R14-P2] CLOSED at `9afd0986` — snap-l2-upload thread-name builder simplified from 4-pass char-walk + 2 String allocs to single byte-slice (`&s[s.len().saturating_sub(8)..]`, ASCII-safe per B24-FOLLOWUP). Doc comment now calls out the 15-char `pr_set_name` truncation explicitly (tail is grep-correlatable in logs but NOT visible in `ps`/`top -H`). +19 / −12 LOC. 332 pass unchanged.
- [R14-A6 + R14-Q4] CLOSED at `c3edf968` — `VmIndexRetryPolicy::from_host_fence_timeout(secs)` added; `RealRestoreBackend` overrides `vm_index_retry_policy` to call it. Wake budget now derived from `cfg.host_fence_timeout_secs - 10 s headroom` (no more second hard-coded constant). `Default` preserved at 25×2s=48s as the C-7 test-contract anchor + stub fallback. Doc off-by-one swept (48 s not 50 s, 118 s not 120 s). +4 tests including a regression-pin on the wake call site. 332 → 336 pass. +234 / −21 LOC.

---

## NEW r14 ROUND 2 FINDINGS (added by pilot cycle 2026-05-25 r13 — architecture r14, concurrency r14, security r14)

### [R14-A1] (CLOSED 2026-05-25 at `3d8acc23` ... `96fa5f0f`) 9 `compio::runtime::spawn(...).detach()` sites; 2 problematic; needs `detach_isolated` helper
- **Source**: 2026-05-25 architecture-r14
- **Files**: `crates/sandbox/src/admin_handlers.rs:1311` (C-6 FIXED at 91ce9be5), `crates/sandbox/src/backend/nomad_ch.rs:2002` CreateGuard::drop (STILL UNFIXED, same pattern)
- **Symptom**: C-3 + C-6 fixes converge on the same `std::thread::Builder::spawn + Runtime::new() + block_on` pattern. CreateGuard::drop on create failure has the same runtime-starvation shape but is admin-reachable (create-failure spam). Code-review fence not structural.
- **Resolution**: helper extracted at `3d8acc23` (`crate::detach::detach_isolated`, FnOnce-factory shape to permit `!Send` futures). Migrated sites: C-6 at `eab3ec43` (admin_handlers::teardown_source_for_snapshot), C-3 at `4fd195ef` (snapshot_store_gcs::Tiered::put L2 upload), R16-I1 sibling-C-6 sites at `96fa5f0f` (sweep::spawn_transient_state_takeover, sweep::spawn_idle_eviction_sweep, registry::start_idle_gc, lib::start_health_loop, lib::spawn_heartbeat_task, lib::spawn_takeover_task). Total: 8 production call sites migrated + 5 helper unit tests + 244 LOC helper module. CreateGuard::drop NOT in this PR — different shape (sync-only, no compio runtime requirement) and out of scope for C-7-LT PR1; track separately if it needs the same isolation treatment.

### [R14-A2] (IMPORTANT, architecture-r14) restore_handler.rs crossed 3000 LOC: 2662 → 3101 (+439)
- **Source**: 2026-05-25 architecture-r14
- **Symptom**: +178 from C-4 fix (b2892368) + 261 from C-6 phase tracing (8e7f0b53). Now 4 files >3000 LOC (was 3 at r13).
- **Action**: extract `restore_phase` module (phase tracing) + types module. Drop file back below 2700 LOC.

### [R14-A3] (IMPORTANT, architecture-r14) R13-A2 L2-detach pull-up MUST use new detach_isolated helper
- **Source**: 2026-05-25 architecture-r14
- **Action**: when extracting the helper (R14-A1), simultaneously pull C-3's L2 detach up to the handler layer (R13-A2's recommendation) using the new helper.

### [R14-A4] (IMPORTANT, architecture-r14) Phase-tracing should be discipline, not C-6 one-off
- **Source**: 2026-05-25 architecture-r14
- **Symptom**: `restore_handler.rs` has 21 phase lines (8e7f0b53); `snapshot_handler.rs`, `admin_handlers.rs`, `nomad_ch::stop_inner` have ZERO. Architecture r14 predicts next cluster bug will wedge in stop_inner.
- **Action**: extend phase tracing to: snapshot_handler.rs::snapshot_sandbox, admin_handlers.rs::handlers, nomad_ch::stop_inner. Same `phase=<name>` field pattern.

### [R14-A5] (MINOR, architecture-r14) 6 un-wrapped std::fs::* sites on async restore/snapshot path
- **Files**: `restore_handler.rs:568, 574, 638, 923, 954` + `snapshot_handler.rs:355`
- **Symptom**: Same C-6 shape (sync I/O on shared runtime), smaller amplitude. Audit + wrap in spawn_blocking where appropriate.

### [R14-A6] (CLOSED at `c3edf968`, MINOR, architecture-r14) VmIndexRetryPolicy::default magic 60×2s
- **Source**: 2026-05-25 architecture-r14
- **Symptom**: C-7 hard-coded `max_attempts=25, interval=2s` for the wake-retry budget. A future bump to `cfg.host_fence_timeout_secs` (the way cad098e6 already did 30→120) would silently desync the two constants — wake budget no longer envelopes the fence-clear window.
- **Fix**: added `VmIndexRetryPolicy::from_host_fence_timeout(secs)` that derives `max_attempts = (secs.saturating_sub(10)) / 2 + 1` (10 s client-deadline headroom, 2 s C-7 interval; +1 accounts for the zero-sleep first attempt). `RealRestoreBackend` overrides `vm_index_retry_policy` to call this, so the production wake path picks up any future fence-config bump automatically. `Default` preserved at 25×2s=48 s as the test contract anchor + fallback for stubs without a cfg. Also closes R14-Q4 doc off-by-one (48 s, not 50 s).
- **Tests**: +4 (`r14a6_policy_from_cfg_respects_host_fence_timeout`, `r14a6_policy_from_cfg_short_timeout`, `r14a6_policy_from_cfg_zero_fence_still_attempts_once`, `r14a6_real_backend_derives_policy_from_cfg_host_fence_timeout` — the last is a regression-pin on the wake call site that catches an accidental fallback to `Default`). 332→336 pass.

### [R14-C1] (CRITICAL, concurrency-r14) sweep.rs:563 idle-eviction is sibling-C-6 site (commit-message MISLABELED safe)
- **Source**: 2026-05-25 concurrency-r14
- **File**: `crates/sandbox/src/sweep.rs:563` (idle-eviction loop in ControllerIdleSnapshotter)
- **Symptom**: 91ce9be5's commit message classified sweep.rs:563 as "safe (steady-state loop with top-of-loop sleep)" but the loop BODY does 90s tail awaits via `teardown_source_for_snapshot(...).await` × cap=2 concurrent INLINE. Latent until idle-eviction overlaps wake traffic on the same worker.
- **Action**: same `detach_isolated` migration (R14-A1) applied here. Or restructure ControllerIdleSnapshotter to use spawn_blocking for the teardown awaits.

### [R14-I2] (IMPORTANT, concurrency-r14) registry.rs:829 idle-GC is sibling-C-6 site
- **Source**: 2026-05-25 concurrency-r14
- **File**: `crates/sandbox/src/registry.rs:829`
- **Symptom**: awaits `state.backend.stop(id).await` inline; same shape.

### [R14-I1] (IMPORTANT, concurrency-r14) C-4 retry budget mismatch + observability gap
- **Source**: 2026-05-25 concurrency-r14
- **Symptom**: VmIndexRetryPolicy::default = 60×2s = 120s budget but client deadline is 60s; retry body has NO per-attempt INFO log. The diagnostic gap that made smoke-r6/r7 mysterious.
- **Truer C-6 mechanism**: C-4 retry budget > client deadline. Wake canceled mid-retry before any success/exhausted log. The runtime-starvation framing (in C-6 commit message) is imprecise; OS-thread fix is defense-in-depth.
- **Action**: (a) reduce C-4 retry budget to client deadline - 5s (e.g., 55s); (b) emit per-attempt log; (c) refresh C-6 commit-message diagnosis.

### [R14-V1] (VERIFICATION, concurrency-r14) do_restore_inner await count: 8, not 7
- **Source**: 2026-05-25 concurrency-r14
- **Resolution**: r13's enumeration was off-by-one on the reserve_vm_index_with_retry await at :558. Inner retry adds up to 60 inner suspension points under contention.

### [R14-D1] (concurrency-r14) Phase tracing was sufficient to LOCALIZE but insufficient to ROOT-CAUSE
- **Source**: 2026-05-25 concurrency-r14
- **Lesson**: per-iteration markers needed inside retry loops, not just async-boundary markers between statements.

### [R14-S1] (IMPORTANT, security-r14) C-6 fix closes snap-teardown arm only — sibling sites remain admin-reachable
- **Source**: 2026-05-25 security-r14
- **Symptom**: nomad_ch.rs:2002 CreateGuard::drop is admin-reachable via CREATE-failure spam. Same runtime-starvation shape.
- **Action**: subsumed by R14-A1 helper extract.

### [R14-S2] (MINOR posture, security-r14) C-4's 120s retry budget amplifies any future R14-class regression
- **Source**: 2026-05-25 security-r14
- **Symptom**: ~80 wedged wakes at default max_connections=100 + ntex connection-slot hold. Capacity planning issue.

### Closures this cycle
- [C-6] CLOSED at `91ce9be5` — detach_isolated pattern via dedicated OS thread + private compio runtime. (Diagnosis refresh needed per R14-I1.)
- [R14-Q2] CLOSED at `79b4d258` — seal_filename_for_str `#[cfg(test)]`-gated (CRITICAL-3 regression test needs the `&str` shape)
- [R14-Q3 + R14-P2] CLOSED at `9afd0986` — snap-l2-upload thread name byte-slice simplification + accurate pr_set_name truncation docstring

---

## NEW r15 ROUND FINDINGS (added by pilot cycle 2026-05-25 r14 — test-cov r15, api-surface r15, code-quality r15)

### [R15-T1 / EMERGENCY ESCALATION] (CLOSED at `c2f24ede` via C-7-LT-PR2) Integration coverage gap structurally blocking velocity
- **Source**: 2026-05-25 test-coverage-r15
- **Symptom**: 9 cluster cycles, 7 distinct bugs (C-1..C-7), ~$3.80 burned. Per-cycle pattern: each fix shipped with "cluster smoke is the validation gate" disclaimer in the commit message. C-6 + C-7 fixes BOTH self-declared cluster-smoke as the validation surface. Observability tracing (`8e7f0b53`) has STRUCTURALLY REPLACED unit tests as the diagnostic primary.
- **R15-T1a**: Future-drop coverage of `compio::time::sleep.await` — ZERO tests in the codebase exercise this. C-7 bug class is "future canceled by client disconnect mid-await"; no test catches it.
- **R15-T1b**: OS-thread detach (C-3, C-6) — byte-coverage-zero. Both commit messages disclose "current test harness doesn't drive this scenario".
- **Resolution**: C-7-LT-PR2 lands the requested pg-e2e fixture (`crates/sandbox/tests/sandbox_pg_e2e.rs::wake_machine_e2e` module) with 6 tests driving `StubRestoreBackend::fail_{reserve,submit,livez}` end-to-end + an idempotency invariant + a GC-eviction invariant. The C-7-LT design itself REMOVES the future-cancel bug class (R15-T1a) by running the wake on `detach_isolated` (ntex cannot reach the future), and the wake_machine_e2e tests pin the OS-thread-detach (R15-T1b) coverage gap by driving the machine through phase boundaries with pg-asserted state transitions. The "tests via cluster smoke" carry-forward is broken: phase boundaries + classified failures are now caught at `cargo test --ignored` time, not at cluster cycle time.

### [R15-T2] (IMPORTANT, test-coverage-r15) Constant-arithmetic-only tests pattern recurring
- **Source**: 2026-05-25 test-coverage-r15
- **Symptom**: C-7 fix replaced `c4_default_policy_envelopes_observed_teardown` with `c7_retry_budget_default_is_under_client_deadline`. Both pin a constant against another constant — neither drives the retry LOOP. R14-A6 added 3 more constant-arithmetic tests + 1 wake-call-site regression-pin (good). Predicted: next refactor of the budget will require this delete-and-replace dance again.
- **Action**: replace constant-arithmetic tests with property-based tests that DRIVE the loop. E.g., proptest: arbitrary host_fence_timeout → invariant that retry budget < client_deadline holds for ALL inputs.

### [R15-Q1] (MAJOR, code-quality-r15) C-6 fix introduced same-cycle regression of R14-Q3 — CLOSED
- **Source**: 2026-05-25 code-quality-r15
- **Files**: `admin_handlers.rs:1340-1352` (C-6 fix at 91ce9be5, 20:09 UTC) used the EXACT `chars().rev().take(8).collect::<String>().chars().rev().collect::<String>()` pattern. R14-Q3 closed this exact pattern at `snapshot_store_gcs.rs` 3 minutes later (20:12 UTC) WITHOUT propagating the cleanup.
- **Symptom**: 2 detach sites had DIVERGED in shape — one used byte-slice (9afd0986), one used the over-engineered char dance (91ce9be5). Reviewer-induced inconsistency.
- **Resolution**: replaced with byte-slice form via `s.get(s.len().saturating_sub(8)..)` matching R14-Q3 9afd0986. Doc comment updated (drops misleading tail-visible-in-ps rationale; mentions Linux's 15-char `pr_set_name` truncation). Both detach thread-name sites now use IDENTICAL shape. Will be subsumed by R14-A1 `detach_isolated` helper extract when that refactor lands. Tests: 337/337 unchanged.

### [R15-Q2] (MINOR, code-quality-r15) C-7 per-attempt INFO log noisy at scale
- **Source**: 2026-05-25 code-quality-r15
- **File**: `restore_handler.rs:304-311` (C-7 added INFO log)
- **Symptom**: 25 INFO lines/wake × c=20 stress = ~500 lines/cycle. Intentional for smoke-r9 diagnosis but no follow-through plan to demote to DEBUG.
- **Action**: after smoke-r9 milestone, demote per-attempt log to DEBUG (or rate-limit). Keep first/last/budget-exhausted at INFO.

### [R15-Q3] (MINOR-elevated, code-quality-r15, refile of R14-A1 with cost evidence) 3 detach sites without shared helper
- **Source**: 2026-05-25 code-quality-r15
- **Files**: snapshot_store_gcs.rs (C-3), admin_handlers.rs (C-6), nomad_ch.rs:2002 (CreateGuard::drop UNFIXED — sibling-C-6)
- **Symptom**: R14-A1 architectural finding gained code-quality cost evidence — R15-Q1 is the regression cost of NOT having the helper.
- **Action**: extract `detach_isolated(name, fut)` helper per R14-A1 sketch. Apply to all 3 sites.

### Closures this cycle
- [R14-A6 + R14-Q4] CLOSED at `c3edf968` — VmIndexRetryPolicy::from_host_fence_timeout derives from cfg + doc off-by-one fixed. +4 tests (332→336).
- [R14-Q2 + R14-Q3 + R11-API1 expanded] verified CLOSED earlier this cycle.
- [api-surface lowest backlog] 6→4 items. R10-API1 now longest 6-round carry.

---

## NEW r15 ROUND-2 FINDINGS (added by pilot cycle 2026-05-25 r15 — architecture r15, concurrency r15, performance r15)

### [R15-A1] (CRITICAL, architecture-r15) Wake synchronous-response contract is shared root cause of C-4/C-6/C-7/C-8/C-8a
- **Source**: 2026-05-25 architecture-r15
- **Symptom**: 5 of 5 wake-path bugs share the design fault: synchronous HTTP response cannot span the ~150s (now ~60s with C-8) source-teardown window when client deadline is 60s. Every retry budget bump (C-4 / R14-A6 / C-7 / C-8 / C-8a) is a patch on the broken contract.
- **Action**: promote **C-7-LT** (long-term) from "deferred" to next-sprint flagship. Wake returns 202 Accepted + status URL. Client polls. Decouples server-side operation duration from client deadline. Closes 5-bug pattern + R15-A5 type-system gap.

### [R15-A2] (IMPORTANT, architecture-r15) Phase tracing extension still un-landed
- **Source**: 2026-05-25 architecture-r15 (1-cycle stale R14-A4)
- **Symptom**: restore_handler.rs has 21 phase log lines; snapshot_handler.rs / admin_handlers.rs / nomad_ch::stop_inner have 0. C-8 investigation paid the predicted tax (read Nomad audit log).
- **Action**: extend phase tracing to the other 3 files. ~25 sites, mirrors existing pattern.

### [R15-A3] (IMPORTANT, architecture-r15) restore_handler.rs is now #2 file in crate at 3444 LOC
- **Source**: 2026-05-25 architecture-r15
- **Symptom**: +343 since r14, +782 in 2 cycles. Passed db.rs (3303). Two consecutive >10% growth cycles.
- **Action**: low-dependency split: extract `retry_policy` module (VmIndexRetryPolicy + tests, ~250 LOC). Subsumes part of r12-A4.

### [R15-A4] (IMPORTANT, architecture-r15) Cluster-bug clustering: 6/9 on runtime+async+timing axis
- **Source**: 2026-05-25 architecture-r15
- **Symptom**: C-3, C-4, C-6, C-7, C-8, C-8a — all runtime/async/timing class. 4 of 8 code-side bugs catchable by R13-A1's stub-driven harness.
- **Action**: makes R13-A1 (StubRestoreBackend integration tests) the highest-leverage backlog item. Pre-req for T-8b-stress.

### [R15-A5] (MINOR, architecture-r15) Retry budget invariant not type-system enforced
- **Source**: 2026-05-25 architecture-r15
- **Symptom**: R14-A6 + C-8a establish "retry budget ≤ client deadline" but only via doc + 1 unit test. Future regression possible.
- **Status**: defer if R15-A1 lands (C-7-LT removes the ceiling entirely).

### [R15-I1] (IMPORTANT, concurrency-r15) host_fence_timeout config drift across 3 surfaces
- **Source**: 2026-05-25 concurrency-r15
- **Files**: `crates/sandbox/src/config.rs:401` defaults to 120s; `crates/sandbox/scripts/gcp-worker-startup.sh:466` overrides to 30s; `crates/sandbox/src/config.rs:1498-1499` unit-test docstring says "60 in many configs"
- **Action**: pick one canonical default + document the cluster-override rationale.

### [R15-I2] (IMPORTANT, concurrency-r15) C-8 fence-budget math has CLIENT_HEADROOM_SECS off-shape
- **Source**: 2026-05-25 concurrency-r15
- **Symptom**: at host_fence=30s, `from_host_fence_timeout(30) = (30-10)/2 + 1 = 11 attempts × 2s = 20s budget`. But the fence itself takes UP TO 30s by design. Wake gives up 10s BEFORE fence can possibly clear in worst case. `CLIENT_HEADROOM_SECS=10` flat subtraction was sized for the DEADLINE ceiling; when fence wins MIN, the 10s shrinks the fence envelope (wrong). Smoke-r10 may still fail when teardown lands in [20s, 30s+] window.
- **Action**: when fence wins MIN, don't subtract CLIENT_HEADROOM (the fence is the OPERATION ceiling; we WANT to wait that long). Or: subsumed by R15-A1's C-7-LT async response.

### [R15-D1] (LOW, concurrency-r15) wait_for_agent_silent has no per-poll log
- **File**: `crates/sandbox/src/backend/nomad_ch.rs:3205-3273`
- **Action**: add per-poll INFO log so fence-tail distribution under c=20 is observable. Pattern from C-7's per-attempt log.

### [R15-P1] (INFO, performance-r15) 30s host_fence drain analysis
- **Source**: 2026-05-25 performance-r15
- **Finding**: 30s sufficient for in-process linger CH guards (CH exits <500ms; fence polls /livez at 100ms with 2-in-a-row). Does NOT envelope Nomad-purge GC tail (~30s). Worst-case teardown ~60s (was ~150s). Cluster-r10 wake p50 projection: ~50% pass back-to-back c=1, ~100% with ≥30s inter-call pause.

### Closures this cycle
- [C-8 + C-8a] CLOSED at `2afbb2dd` — retry budget cap + 30s fence (NOTE: R15-I2 flags math edge case)
- [R15-Q1] CLOSED at `7469118e` — admin_handlers byte-slice form (matches R14-Q3)
- [C-8b] CLOSED at `64af1803` — 2× fence factor in `from_host_fence_timeout` (smoke-r10 measured 60s teardown at fence=30s; budget now 26 attempts / 50s, was 11 / 20s)

---

## Round-16 r1 closures (2026-05-25)

- [R15-S1] CLOSED at `da951dd9` — A1-FOLLOWUP fail-CLOSED assertion at `lib.rs:472`: boot panics when `snapshot_enabled && use_gcs && !kek_present` without test-override. Gap from arch-r9 closed.
- [R15-I2] CLOSED at `64af1803` — C-8b 2× fence factor raised budget from 20s → 50s at fence=30, subsumes the off-shape headroom concern (budget now envelopes full 2×fence teardown, not just the fence).
- [#24] CLOSED at `a4c481e1` — B24 `ZSBX_SANDBOX_ID` env injection in controller Nomad task `Env` block; stale paperwork entry deferred-refreshed at `fb428dce`.
- [A1-FOLLOWUP] CLOSED at `da951dd9` — see R15-S1 above.
- **Round-16 r1 cycle summary**: 4 reviewer reports landed (arch/code-quality/concurrency r16 + security r15 catchup). A1-FOLLOWUP critical fix landed. #24 paperwork closed. C-8c surfaced from smoke-r11 (sync wake contract structurally out of knobs — see C-8c entry in CRITICAL section). C-7-LT design proposal ready at `docs/proposals/c7-lt-async-wake.md` (uncommitted per proposal-workflow rule).

## Round-17 PR2 closures (2026-05-25 — C-7-LT-PR2)

- [C-7-LT-PR2] **LANDED** across 4 commits:
  - `17e9f421` — `sandbox/wake_machine: WakeMachine state machine driver` (typed_id `wak_` 3-char prefix per R16-API2; `WakeErrorCode::wire_code` 1:1 with existing envelope codes per R16-API1 #3; `sandbox_wake_sync_uses_total` deprecation counter per R16-API1 #5; `clock_resync_post_restore` visibility bumped to `pub(crate)`)
  - `98032273` — `sandbox/admin_handlers: dual-mode wake + poll endpoint` (R16-API1 #1 §10.0 field-name parity on failed-poll body; idempotency replay matrix per R16-API3; new `GET /wake/{wake_id}` registered in `main.rs`)
  - `9f006c87` — `sandbox/sweep: wake_jobs GC every 60s` (T_KEEP = 5 min per proposal § 5; spawned at boot when `database.is_some()`)
  - `c2f24ede` — `sandbox/tests: wake_machine pg-gated stub fixtures` (6 e2e tests with `StubRestoreBackend::fail_{reserve,submit,livez}` failure injection — closes the 7-round R15-T1 EMERGENCY HOLD + R10-T1/R10-T2 + R11-T1/T2 + R12-T1/T2 + R13-T2 + R14-T1)
- [R10-T1] CLOSED at `c2f24ede` — `wake_machine_e2e` module drives `do_restore_inner`-equivalent rollback paths through `StubRestoreBackend` with pg-asserted state transitions.
- [R10-T2] CLOSED at `c2f24ede` — wake_machine async path covers the integration ladder; mechanical equivalence with sync's `do_restore_inner` defers Persist(Some(_)) drive to smoke-r12 cluster validation (the harness now exists).
- [R11-T1] CLOSED — subsumed by R10-T1.
- [R11-T2] CLOSED — subsumed by R10-T2.
- [R12-T1] CLOSED — subsumed by R10-T1.
- [R12-T2] CLOSED — subsumed by R10-T2.
- [R13-T2] CLOSED — subsumed by R10-T1/R10-T2.
- [R14-T1] CLOSED — subsumed by R10-T1.
- [R15-T1 / EMERGENCY ESCALATION] CLOSED at `c2f24ede` — pg-e2e fixture + 6 tests deployed. The C-7-LT design itself removes the future-cancel bug class (R15-T1a) by running the wake on `detach_isolated`; the wake_machine_e2e tests pin OS-thread-detach (R15-T1b) coverage. Cluster smoke is no longer the primary validation surface for wake-path classified failures.
- [R16-API1] CLOSED at `17e9f421` + `98032273` — failed-poll body uses §10.0 `error`/`message` keys (not pre-review `error_code`/`error_message`); all WakeErrorCode variants render via `wire_code()` reusing existing envelope codes (no parallel codes); idempotency replay matrix encoded in the handler (202 + replay:true for in-flight; new wake_id for terminal-evicted); `sandbox_wake_sync_uses_total` counter backs the Phase 5 migration gate.
- [R16-API2] CLOSED at `17e9f421` — typed_id `wak_` 3-char prefix added to `crates/core/src/typed_id.rs`; roundtrip + prefix-length tests pin the global `[a-z]{3}_` invariant.
- [R16-API3] CLOSED at `98032273` — idempotency status-code matrix encoded in `wake_sandbox_async_inner`: first POST → 202 fresh wake_id; replay in-flight → 202 same wake_id + `replay: true`; post-eviction → 202 fresh wake_id (terminal-within-T_KEEP replay surfaces through GET, not POST).

## Round-17 PR2-FOLLOWUP closures (2026-05-25 — C-7-LT-PR2-FOLLOWUP)

- [C-7-LT-PR2-FOLLOWUP] **LANDED** across 5 commits closing the 10 r17/r18 gates against PR1+PR2's surface:
  - `96678eaa` — `sandbox/db: bump lessee_updated_at in update_wake_job_state + symmetric COALESCE (R17-A1, R17-I2)` — every state transition now renews the lease (no more stolen-mid-flight races on the wake-job takeover sweep) and `error_code` / `error_message` / `agent_url` all use `COALESCE($N, col)` symmetrically (None preserves, Some overwrites — fixes the asymmetric NULL-on-replay bug).
  - `fa4fe63c` — `sandbox/migrations: 0010 — revoke audit SELECT + add lessee index + agent_url CHECK (R16-S1, R17-A2, R16-S3)` — REVOKES the 0009 stray grant on `sandbox_audit`, adds `wake_jobs_lessee_idx` (partial on `lessee_updated_at` filtered to non-terminal), adds column-level CHECK on `agent_url` (`^https?://[a-zA-Z0-9._:/-]+$`). `LATEST_MIGRATION_VERSION` bumped 9 → 10.
  - `b2b6c3c9` — `sandbox/wake_machine: sanitize error_message before pg write (R16-S2)` — `sanitize_error_message` (RFC1918 IPv4 / IPv6 link-local / agent-URL stripping + 256-byte truncation; hand-rolled byte scan to keep `regex` out of the crate-graph) wraps the terminal-failed branch's pg write. Operator log still gets the unredacted message.
  - `4ab58eac` — `sandbox/config: WakeResponseMode fail-CLOSED + retention config (R16-S4, R16-S5)` — `from_env` returns `Result<Self, String>`; unrecognised values abort boot. Adds `WakeLifecycleConfig::wake_jobs_gc_retention_secs` (env `SANDBOX_WAKE_JOBS_GC_RETENTION_SECS`, default 300 s, minimum 1 s) wired through to `sweep::run_wake_jobs_gc_once`.
  - `c3038389` — `sandbox/lib: rename snap-health-loop → snap-health + add 15-byte name pin test (R17-I1)` — renames `snap-health-loop` (16 B, silently kernel-truncated) to `snap-health` (11 B). Adds `detach::tests::all_known_thread_names_fit_kernel_limit` pinning all current literals + format-string prefixes to ≤15 B.
- [R17-A1] CLOSED at `96678eaa` — `update_wake_job_state` SQL bumps `lessee_updated_at = now()` on every transition.
- [R17-A2] CLOSED at `fa4fe63c` — `wake_jobs_lessee_idx` partial index added in 0010.
- [R17-I1] CLOSED at `c3038389` — thread name renamed; pin test added.
- [R17-I2] CLOSED at `96678eaa` — symmetric COALESCE on `error_code`/`error_message`/`agent_url`.
- [R16-S1] CLOSED at `fa4fe63c` — `sandbox_audit` SELECT on `wake_jobs` REVOKED in 0010.
- [R16-S2] CLOSED (PARTIAL at `b2b6c3c9`; FULLY CLOSED at `3c75a8ce` via R17-S1) — `sanitize_error_message` applied at the terminal-failed pg write site; 169.254/16 + 100.64/10 gaps closed in follow-up.
- [R16-S3] CLOSED at `fa4fe63c` — column-level CHECK on `agent_url` shape.
- [R16-S4] CLOSED at `4ab58eac` — `from_env` fail-CLOSED on unrecognised values.
- [R16-S5] CLOSED at `4ab58eac` — `wake_jobs_gc_retention_secs` env-driven through `WakeLifecycleConfig`.
- [R17-S1] CLOSED at `3c75a8ce` — `match_rfc1918_at` extended with 169.254/16 (RFC 3927 link-local / IMDS) and 100.64/10 (RFC 6598 CGNAT) arms; 6 new unit tests pin each new prefix being matched/preserved. Sanitizer policy now matches the sibling SSRF guard at `crates/runtime/src/transport/ssrf.rs:45-51`.
- R17-A5 is being closed in a separate fixer running against `nomad_ch.rs::CreateGuard::drop` (not touched here).
- Sandbox lib tests: 374 → 396 (+22). All pg-gated tests still gated; build clean release.
- **PR3 (next cycle, formerly listed at PR2): cluster smoke validation under `SANDBOX_WAKE_RESPONSE_MODE=async`** — c=1/c=20 stress with the polling client (snapshot_stress.py update). Smoke-r12 confirms WAKE OK 1/1 at fence=30 (the empirical scenario C-8c declared structurally out of knobs).

## Round-17 GATE-C2 closure (2026-05-25 — TOCTOU close-off on wake-POST)

- [R17-C2 / GATE-C2] **CLOSED** across 3 commits closing the wake-POST TOCTOU that R17 concurrency review identified as CRITICAL (low-probability at c=1 smoke, high at c=20+ stress; must close before T-8b-stress):
  - `1cfc9182` — `sandbox/migrations: 0011 — UNIQUE index on wake_jobs(sandbox_id) WHERE non-terminal (GATE-C2)` — partial UNIQUE INDEX `wake_jobs_sandbox_pending_uniq` enforcing at-most-one non-terminal wake row per sandbox. `LATEST_MIGRATION_VERSION` bumps 10 → 11.
  - `678ec197` — `sandbox/db: insert_wake_job uses ON CONFLICT DO NOTHING returning replay flag (GATE-C2)` — `insert_wake_job` now returns `Result<InsertWakeJobOutcome>` (`Inserted` | `Replay(WakeJobRow)`). INSERT uses `ON CONFLICT (sandbox_id) WHERE state NOT IN ('ok','failed') DO NOTHING`; on 0 rows affected the function re-reads the winner via `find_pending_wake_for_sandbox` and surfaces it as `Replay`.
  - `db248cbf` — `sandbox/admin_handlers: collapse wake-POST race via insert_wake_job replay path (GATE-C2)` — `wake_sandbox_async_inner` consumes the new outcome: `Inserted` spawns the WakeMachine + returns `replay: false`; `Replay(existing)` returns 202 with the WINNER's `wake_id` + `replay: true` and DOES NOT spawn a duplicate machine. The pre-existing `find_pending_wake_for_sandbox` precheck stays as an optimisation but is no longer load-bearing; race protection is at the INSERT site.
- The original race: two concurrent POSTs both missed the precheck, both inserted, both spawned a WakeMachine, and the loser's `rollback_with` called `teardown_restore` — releasing the WINNER's vm_index (R10-C1 fingerprint). Post-fix, the loser never gets a state machine that could `teardown_restore` the winner's allocation.
- Tests: 3 lib unit tests (`db::tests::insert_wake_job_outcome_*`) + 4 pg-gated integration tests (`wake_jobs_crud::wake_jobs_{insert_returns_inserted_on_fresh_sandbox, insert_collapses_concurrent_race_via_unique_index, unique_index_releases_after_terminal_transition, sandbox_pending_uniq_index_present}`). All 14 wake_jobs_crud pg-gated tests pass against local docker-compose pg. Sandbox lib tests: 396 → 399 (+3).

## Round-18 R18-I1 closure (2026-05-25 — fresh-insert fixture assertions)

- [R18-I1] **CLOSED at `531db5c3`** — 10 call sites in `crates/sandbox/tests/sandbox_pg_e2e.rs` that called `insert_wake_job` and silently discarded the `InsertWakeJobOutcome` return value are now wrapped in `assert!(matches!(..., InsertWakeJobOutcome::Inserted))`. The `make_machine` test fixture (the primary reported site) gets the most detailed assertion message so a `Replay(...)` result (indicating a stale non-terminal row from a prior test leaked into the schema) surfaces as an explicit test failure instead of silently driving the wrong `wake_id`. Added `InsertWakeJobOutcome` to the `use zeroship_sandbox::db::...` import in `wake_machine_e2e` sub-module (was missing). No intentional Replay-path fixture exists; all 10 sites expect `Inserted`. Cargo build clean; 81 pg-gated tests compile and run as `ignored` (no local pg).

## Round-18/19 doc+visibility fixer (2026-05-25 — R14-API2/R18-API2/r19-A4)

- [R18-API2] **CLOSED** — `RealRestoreBackend::with_wake_response_mode` demoted from `pub` to `pub(crate)` to match sibling `with_shared_allocator`. Both callers (`restore_handler.rs` doc-reference comments + `lib.rs:785`) are in-crate. Cargo build clean; 414/414 lib tests unchanged.

- [r19-A4] **CLOSED** — `VmIndexRetryPolicy::from_host_fence_timeout` C-8b doc block (lines ~231-246) replaced with NON-NORMATIVE annotation and the empirically correct teardown model from the smoke-r13 retrospective: two distinct paths (agent-dies → 2 connect-misses → slot released promptly; agent-hangs → `host_fence_timeout` expires → slot released or leaked) replace the incorrect "Nomad purge tail ~fence-shaped" composition model. The 2× factor is retained as a conservative safety margin, not a model. The hard-coded 30 s fence in `nomad_ch.rs` is noted. No behavior change; 414/414 tests unchanged.

- [R14-API2] **CLOSED** (6-round carry) — see entry above at the r14-round section.

## Round-19 R19-I1 closure (2026-05-25 — wait_for_agent_livez two-phase probe)

- [R19-I1] **CLOSED** — `wait_for_agent_livez` startup probe carried the same ureq+spawn_blocking wedge shape that C-7-LT-2-PR1 just fixed for the teardown probe. Replaced the per-iteration single `ureq::get(livez).timeout(500ms).call()` with a two-phase probe: Phase 1 (`probe_agent_reachable_tcp(addr, 150ms)`, compio-native, hard outer timeout) gates Phase 2 (the existing ureq /livez HTTP call, now bounded by a verified-up TCP layer). Phase 3 (signed /version) sits where it was. The Phase 1 connect-gate caps a stuck SYN at 150 ms — independent of the kernel's 30-90 s SYN-retransmit ceiling — eliminating the wedge where a half-collapsed TAP at create-time burned the whole `agent_livez_timeout` on a single in-flight probe. Phase 2's `parse_agent_probe_addr` parse failure surfaces a clear "agent at <url> unparseable" error rather than burning budget on doomed probes (mirrors `wait_for_agent_silent`). 4 new unit tests in `crates/sandbox/src/backend/nomad_ch.rs`: `wait_for_agent_livez_happy_socket_then_livez_ok`, `wait_for_agent_livez_socket_never_accepts_returns_timeout_clean` (TEST-NET-1 unroutable; pins the wedge-fix invariant), `wait_for_agent_livez_socket_accepts_late_succeeds_within_budget` (empty→up transition mid-budget), `wait_for_agent_livez_socket_accepts_but_livez_500` (Phase 2 contract — TCP up but HTTP-layer failure). Sandbox lib tests: 414 → 418 (+4). Cargo build/tests clean.

- [r19-A5] **CLOSED** — same fix as R19-I1; r19-A5 is the architecture-lens dual flagging the same call site.

## r17-Q2 + R19-I4 fixer (2026-05-23 — doc prose off-by-one + insert_wake_job terminal race retry)

- [R17-Q2] **CLOSED** — `from_host_fence_timeout` examples table used loose "N attempts × 2 s = Xs budget" notation where N×2 ≠ X (e.g. "36 × 2 = 72 ≠ 70"). The actual wall-time formula is `(attempts − 1) × interval` because the first attempt has no preceding sleep. All five example lines in `crates/sandbox/src/restore_handler.rs` updated to the `N attempts (N−1 sleeps × 2 s) = Xs` form. Also added a one-line wall-time formula note before the examples table so the math is self-documenting. No behavior change; doc only. Source: code-quality-r17 R17-Q2 / code-quality-r19 R19-M3.

- [R19-I4] **CLOSED** — `insert_wake_job` in `crates/sandbox/src/db.rs` returned `DatabaseError::Validation` (→ 500 at handler) when the ON CONFLICT winner's WakeMachine transitioned to a terminal state in the sub-ms window between PG's conflict-resolution and the follow-up `find_pending_wake_for_sandbox` SELECT. Fix: replaced the one-shot conflict-path with a bounded retry loop (`INSERT_WAKE_JOB_MAX_RETRIES = 3`). On each iteration: if INSERT lands → `Inserted`; if conflict + pending winner found → `Replay(winner)`; if conflict + no pending winner (terminal-during-race) → `continue` (retry INSERT, which now clears the index because terminal rows are excluded). After all 3 retries exhausted → `DatabaseError::Validation` with a clear "pathological rapid-transition race" message. Retry is internal to `db.rs`; handler and `InsertWakeJobOutcome` enum are unchanged. Sandbox lib tests: 423 → 425 (+2: `insert_wake_job_retries_on_winner_going_terminal_unit`, `insert_wake_job_returns_error_after_max_retries_message_shape`). Cargo build/tests clean. Source: concurrency-r19 R19-I4.

## Round-20 R20-I1 closure (2026-05-25 — extract from_host_fence_timeout doc to ADR)

- [R20-I1] **CLOSED** — `VmIndexRetryPolicy::from_host_fence_timeout` rustdoc had grown to ~116 lines (>function body) after 11 cycles of C-8/C-8a/C-8b/C-7-LT-1 grafts and the r19-A4 NON-NORMATIVE banner. Extracted the full empirical-model retrospective to `docs/decisions/2026-05-25-vm-index-retry-policy.md` (ADR covering C-7, R14-A6, C-8a, C-8b, smoke-r13 retrospective, and C-7-LT-1, with Decision + Consequences sections). Rustdoc trimmed to 13-line summary: what the function does, which mode does what, and a pointer to the ADR for full history. No behavior change; code body untouched. r17-Q1 / r18-M2 / r19-M2 / r20-M-NEW (doc-inflation carry) — all carried by this closure.

## r17-Q3 fixer — wake_job_row_from_pg explicit DataIntegrity error path

- [r17-Q3] **CLOSED** — `wake_job_row_from_pg` in `crates/sandbox/src/db.rs` previously used `.unwrap_or(WakeJobState::Failed)` to silently map any unknown `state` discriminator to `Failed`. The fallback was structurally unreachable via migration 0009's CHECK constraint, but masked any future schema/code drift. Replaced with an explicit `match` that returns `Err(DatabaseError::DataIntegrity(...))` on unknown state strings. A new `DataIntegrity(String)` variant was added to `DatabaseError`. Both callers (`get_wake_job`, `find_pending_wake_for_sandbox`) updated from `Ok(opt.map(f))` to `opt.map(f).transpose()`. `error_code` is intentionally NOT guarded the same way — the column is nullable/advisory and an unknown code from a newer binary is silently dropped (`and_then` → `None`), which is documented in the function's rustdoc and pinned by the new `wake_job_row_from_pg_error_code_unknown_drops_to_none_not_error` test. `compio_postgres::Row::new` is `pub(crate)` so a real-row unit test is not possible without a pg fixture; the structural CHECK-constraint argument and the precondition tests serve as the coverage proxy. Sandbox lib tests: 425 → 427 (+2: `wake_job_row_from_pg_returns_error_on_unknown_state_precondition`, `wake_job_row_from_pg_error_code_unknown_drops_to_none_not_error`). Cargo build/tests clean. Carried from code-quality r17/r18/r19/r20.
