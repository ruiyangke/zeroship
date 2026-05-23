# Concurrency Review (r4) — sandbox snapshot/restore

**Branch:** `feat/sandbox-snapshot-restore` (HEAD `29afea72`)
**Scope:** `crates/sandbox/**`
**Date:** 2026-05-24 UTC · **Reviewer:** code-critic (Opus)
**Prior rounds:** r1 (11) · r2 (8) · r3 (7)

---

## Prior-round closure status

- **r3 #1 (C3: 90s cancel-window on `do_restore_inner`)** — **still open**.
  The 4 §10.0-envelope commits (`64db0d30`, `c0296c76`, `5330acd9`, `2928d5ae`) touch only `error_response` call sites; B18 (`469e22c8`, `b4ddb98b`) shares the allocator but does not introduce an RAII guard. The blocking `wait_for_alloc_running_blocking` + `wait_for_livez_blocking` (`restore_handler.rs:388-390`) plus the pre-`submit_restore_job` `std::fs::remove_dir_all` / `create_dir_all` / `rewrite_config_json` (lines 303-379) remain unguarded. See finding 1 — the window **grew** by one further `.await` boundary at `clear_snapshot_metadata` (line 400).
- **r3 #2 (B17 wrapper background subshell un-reaped)** — **still open**. `nomad-vm-wrapper.sh:388-419` unchanged since `dec489a1`; trap at `:278-289` still kills only `$CH_PID`. See finding 2.
- **r3 #3 (B18 root-cause hypothesis: two allocators)** — **CLOSED**.
  Confirmed: `nomad_ch.rs:269-340` `VmIndexAllocator` now exposes `reserve()` with in-flight collision detection; `lib.rs:617-639` wires the same `Arc<Mutex<VmIndexAllocator>>` into `RealRestoreBackend::with_shared_allocator` (`restore_handler.rs:764-770`). Two unit tests (`restore_handler.rs:1332,1372`) pin the shape. Verified no Mutex held across `.await` in any of `alloc/reserve/release` call sites (sync `.lock().unwrap_or_else(...)` then immediate use).
- **r3 #4 (T7 `join_all` chunk cap=4 vs pg pool=16)** — **partially closed**. Pool-headroom analysis still valid; sweep doesn't take the allocator mutex (see finding 5), so no deadlock. Window risk remains around L2 stuck uploads.
- **r3 #5 (C1 lease-takeover dead)** — **still open**. `db.rs:2305` `update_lessee` still zero non-test callers; nothing in `29afea72`..`469e22c8` touches it.
- **r3 #6 (RealRestoreBackend success-path slot leak)** — **partially closed**. The B18 fix routed `release_vm_index` to the shared pool, so leaks now affect the create-side pool too — same severity, larger blast radius. B19 (state-map register missing) compounds: the restored VM is never inserted into `state`, so the regular `stop`/`stop_preserving_state` paths return idempotent-Ok WITHOUT calling release. See finding 3.
- **r3 #7 (`snapshot_rows_chunked` shutdown checked once per chunk)** — **still open**. `sweep.rs:478-480` unchanged.

---

## CRITICAL

### 1. C3 widened: `clear_snapshot_metadata` is a NEW post-success `.await` with no guard
`restore_handler.rs:397-409`. The B18 commits added no scope-guard; the §10.0 commits added no scope-guard. The C3 window remains 90s wide AND the new `db.clear_snapshot_metadata(sandbox_id, g2).await` at line 400 extends it: if the handler future drops between the `update_sandbox_status(Running)` success at line 397-399 and the clear-metadata await at line 400, the row is `running` but the pg `snapshot_*` columns still point at an artifact that's been consumed. A subsequent idle-snapshot then attempts to PUT atop a record that already references a (now-stale) sealed snapshot. Worse: with B18 closed, `reserve_vm_index` succeeded against the shared allocator — but no `release_vm_index` runs on early drop. **Smallest fix unchanged from r3**: `scopeguard::guard` around the inner body, defused on `Ok` at line 411.

### 2. B17 wrapper subshell — un-reaped, no PID capture (r3 #2 verbatim)
`scripts/nomad-vm-wrapper.sh:388-419`. Verified verbatim against HEAD `29afea72`: no `BG_PID=$!` capture, no `kill $BG_PID` in `cleanup`, no `wait $BG_PID`. The `cleanup` trap at lines 278-289 still kills only `$CH_PID`. Under a Nomad SIGTERM during the 10s `ch-remote ping` polling window (lines 391-398), the subshell continues polling a dead socket for up to 10s while the parent's 5s kill-grace expires → SIGKILL of the parent wrapper bash, leaving the subshell as a PID-1 zombie. Equally bad: the trailing 4.3s `ip link set up` retry loop (lines 414-418) races CH's vCPU init and continues running after a clean stop, producing log noise that looks like a hang.

---

## IMPORTANT

### 3. R4-A2 / B19 race: state-map removal + vm_index release straddle 60-120s — and the wake path NEVER inserts
`nomad_ch.rs:982-986` (state remove SYNC under lock) → 60s `wait_for_job_gone` (`:1039-1044`) → 0-120s `wait_for_agent_silent` (`:1089-1093`) → vm_index release at `:1126-1129`. Two locked structures span the fence with no unified RAII. B19 (deferred, open) magnifies this: `restore_handler::do_restore_inner` (`:286-412`) does NOT install a record into `NomadCHBackend::state` after `wait_for_livez` succeeds. A post-wake `stop`/`stop_preserving_state` hits the `None => return Ok(())` idempotent branch at `:988-989`, **never calling `release_vm_index`** — every successful wake permanently consumes a shared-pool slot. With B18's shared allocator the leak hits the create-side `alloc()` pool floor=1, ceil=12 → exhaustion at ~12 wakes per controller boot (matches deferred-file observation "10 wakes with c=4 × 4 stress"). The B18 fix tightened slot bookkeeping but did not introduce the RAII pattern R4-A2 calls for.

### 4. B18 fix: shared `Arc<Mutex<VmIndexAllocator>>` is NOT held across `.await`
`restore_handler.rs:786-789, 803-808` + `nomad_ch.rs:649-664, 1126-1129`. Audited every call site of `vm_index_allocator.lock()`: each is `.lock().unwrap_or_else(...).{alloc,reserve,release}()` with the `MutexGuard` dropped synchronously (the methods are all `fn`, no `async`). No `.await` between lock and drop. Under 5-worker × 20-cycle stress, peak contention is bounded by the per-call critical section (a `BTreeSet` insert/remove + an integer compare) — sub-microsecond. **Not a concern.** This is a clean fix shape.

### 5. T7 `join_all` cap=4 — no deadlock against the shared allocator
`sweep.rs:470-524` + the per-row `snapshot_one` path (`:307-387`). `snapshot_one` calls `lookup_source_vm_ops`, `snapshot_sandbox`, `teardown_source_for_snapshot` — none of these take `vm_index_allocator.lock()`. The allocator is only touched by `create()` (`:649-664`), `stop_inner` (`:1126-1129`), `reserve_vm_index`, `release_vm_index`. **No deadlock path** between the 4 in-flight sweep futures and the create/stop traffic. Pool contention against pg (D-17 `pool_max=16`) remains the limiting factor under stuck L2 uploads — concurrency-wise the shape is sound.

### 6. A4 envelope migration — zero new shared state introduced
Spot-checked `admin_handlers.rs::list_sandboxes` post-`64db0d30` and `preview_handlers.rs::proxy_session_*` post-`c0296c76`: every `error_response(...)` call is a pure builder (`error_envelope.rs:116-122`) that synchronously constructs `HttpResponse`. No new locks, no Arc clones, no async boundaries within the helpers themselves. The 74 call sites are concurrency-neutral. **A4 introduced ZERO new concurrency surface.**

---

## MINOR

### 7. Wake-time idle-eviction theoretical race
`sweep.rs:329-349`. If a sandbox is `Running` post-wake but the sweep's `idle_eligible_sandboxes` query returns it (its `last_activity_at` is whatever the wake handler stamped), and the wake handler's `clear_snapshot_metadata` (`restore_handler.rs:400`) is still in flight when the sweep's `lookup_source_vm_ops` resolves, the sweep can snapshot a row whose pg `snapshot_*` columns still point at the prior (consumed) artifact. The `StateMismatch` tolerance at `sweep.rs:372-381` absorbs this gracefully, but operators see the spurious "state moved before CAS" debug log on every wake-eligible row. Not a correctness bug; signal-to-noise issue under load.

---

**Summary:** 7 findings — 2 CRITICAL (C3 widened, B17 wrapper subshell verbatim), 4 IMPORTANT (R4-A2/B19 RAII gap, B18 fix audited clean, T7 deadlock-free, A4 concurrency-neutral), 1 MINOR. **r3 #3 CLOSED**; **r3 #4 and #6 partially closed**; r3 #1, #2, #5, #7 remain open. Top two: **(1)** `restore_handler.rs:286-412` still cancel-unsafe — C3 grew by one more `.await` boundary at `:400` post-B18; **(2)** `nomad_ch.rs:982-986` state-map remove + `:1126-1129` vm_index release span 60-120s with no RAII guard, and `do_restore_inner` never inserts the restored sandbox into `state`, so successful wakes permanently leak the shared allocator slot (B19 compounds R4-A2).
