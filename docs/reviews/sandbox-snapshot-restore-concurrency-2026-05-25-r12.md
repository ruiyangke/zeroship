# Sandbox/snapshot-restore — concurrency r12 review

Date: 2026-05-25 (UTC)
HEAD at audit: `ae946cba`
Round 12 of N. Read-only. Branch `feat/sandbox-snapshot-restore`.

## Summary
- 4 findings (0 critical NEW, 1 important NEW, 2 minor NEW, 1 verification).
- T-7 (`5fe36805`) audited: jobspec construction stays sync; no new locks, no new awaits, no new state mutation, no new spawn / spawn_blocking sites in production code. Test seam `T7_ENV_LOCK` is safe under parallel `cargo test` (disjoint env-var set vs. `db.rs::tests::ENV_LOCK`). [R12-I1] surfaces a feature-flag-coverage gap: the env switch applies to `nomad_ch::build_nomad_job_json` (cold-boot CREATE) but NOT to `restore_handler::build_restore_nomad_job_json` (wake path), leaving the restore alloc hard-coded to `Driver: "raw_exec"` even under `SANDBOX_TASK_DRIVER=ch_plugin` — a deployment-mode split-brain when T-8 flips the flag.
- All r11 carry-forwards remain open. R4-A2 `LeasedVmSlot` RAII is now **9+ cycles open** — would close R10-C1+C2, R11-C1+C2, R11-I1, R12-M1, plus the 6 C3 widenings in one PR.
- No changes to `restore_handler.rs` or `registry.rs` since r11; R11-C1 / R11-C2 / R10-Q3 carry forward verbatim.

## T-7 concurrency audit (5fe36805)

**Verdict: T-7 introduces no new concurrency surface.** Verified by reading the full diff (+541/-93 LoC in `crates/sandbox/src/backend/nomad_ch.rs`).

- `task_driver_mode_from_env()` (`nomad_ch.rs:2239-2244`) — single `std::env::var()` call, no `&self`, no lock acquisition, no `.await`. Returns `TaskDriverMode` enum. Pure function.
- `TaskDriverMode` (`nomad_ch.rs:2226-2230`) — `#[derive(Debug, Clone, Copy, PartialEq, Eq)]` enum with two unit variants. No interior mutability. No `Drop`. Send + Sync trivially.
- `build_nomad_job_json_with(...)` (`nomad_ch.rs:2311-2502`) — replaces the prior monolithic `build_nomad_job_json`. Sync function, takes args by `&` (no clone, no move), returns `serde_json::Value`. Calls into `cfg.nomad_ch.runtime_dir.join(...)`, `.display()`, `cpus_boot(cfg.cpus)`, `serde_json::json!{...}` — all sync, no locks. The `match mode { ... }` is a straight-line value-construction; no shared state read or write.
- `build_nomad_job_json(...)` (`nomad_ch.rs:2279-2303`) — now a thin wrapper: reads env once via `task_driver_mode_from_env()`, then delegates to `_with`. Called from `nomad_ch.rs:718` (sync, inside `NomadCHBackend::create` — itself `async fn` but the call is sync within it, no `.await`).
- The only production caller (`nomad_ch.rs:718`) reads env on every CREATE. `std::env::var()` acquires libstd's internal env-table Mutex under the hood; at the controller's expected CREATE QPS this is a non-issue but stylistically wasteful (see [R12-M2]).
- Tests `nomad_job_spec_uses_raw_exec_by_default` (4097) and `nomad_job_spec_uses_ch_when_flag_set` (4119) acquire `T7_ENV_LOCK` via `with_task_driver_env`; the lock is local-to-module (`nomad_ch.rs:4073`) and guards **only** `SANDBOX_TASK_DRIVER`. The 4 prior tests that previously called the env-reading helper were refactored to `build_nomad_job_json_with(..., TaskDriverMode::RawExec)` to avoid the env race (commit message confirms this; verified at `nomad_ch.rs:3854, 3952, 3986, 4042`).
- Cross-lock check: `T7_ENV_LOCK` (nomad_ch.rs tests) and `ENV_LOCK` (db.rs tests, `db.rs:2741`) guard **disjoint env-var sets** (`SANDBOX_TASK_DRIVER` vs `{SANDBOX_DATABASE_URL, SANDBOX_HA_*, SANDBOX_PERSIST_DIR, SANDBOX_HOST_ID, ...}`). No test should need both; **no deadlock potential** under parallel `cargo test`. Both use `unwrap_or_else(|e| e.into_inner())` poison-recovery, so even a panicking test under the lock doesn't wedge subsequent tests in the same process.

## Findings (NEW since r11)

### [R12-I1] T-7 feature-flag coverage gap — `build_restore_nomad_job_json` (wake path) ignores `SANDBOX_TASK_DRIVER`, leaving restore allocs hard-coded to `raw_exec` even when `ch_plugin` is requested (IMPORTANT, concurrency-r12)

- **Files**: `crates/sandbox/src/restore_handler.rs:1252-1379` (`build_restore_nomad_job_json`), specifically `:1321-1326` (`"Driver": "raw_exec"`, `"Config": { "command": cfg.wrapper_path.display().to_string() }` — hard-coded, no env consult).
- **Comparison**: T-7's switch lives in `crates/sandbox/src/backend/nomad_ch.rs::build_nomad_job_json` (cold-boot CREATE path). It reads `task_driver_mode_from_env()` and branches Driver + Config accordingly. The restore-path builder is a separate function in a separate file (per its own comment at `:1246-1251`: "We don't share the helper because the restore path doesn't have a `user_id`/`project_id` to plumb through Meta") — and that separation means the T-7 switch never reaches it.
- **Cluster-rollout shape (T-8 flip)**: when an operator sets `SANDBOX_TASK_DRIVER=ch_plugin` (as T-8a-controller's `gcp-worker-startup.sh` does conditionally on `INSTALL_CH_PLUGIN_DRIVER=1`):
  - Cold-boot CREATE jobspecs ship with `Driver: "ch"` → the Go plugin's typed driver handles VM lifecycle.
  - Wake / RESTORE jobspecs ship with `Driver: "raw_exec"` + `command: /etc/zeroship/nomad-vm-wrapper.sh` → still expects the bash wrapper.
- **Failure mode under partial cutover**: if T-8a's `gcp-worker-startup.sh` runbook eventually removes the bash wrapper (the natural end-state once ch_plugin is verified), every wake-path alloc submission fails Nomad's driver validation (`raw_exec` task with `command` pointing at a non-existent path), or worse, the wrapper exists but is stale and races the new Go driver's host-side state under the same `vm_index`. **Concurrency angle**: if both drivers can coexist on the same worker (transitional phase), a CREATE-via-ch-plugin sandbox followed by a RESTORE-via-raw-exec on the same `vm_index` races two different host-side processes for the same TAP / IP / image paths — exactly the multi-driver, single-vm_index collision shape R5-A2's LeasedVmSlot was designed to prevent.
- **Why important, not critical**: today's HEAD has `INSTALL_CH_PLUGIN_DRIVER=1` gated off in production (T-7's commit message: "feature flag stays off until T-8 validates on cluster"), so the bad path is unreachable on current deployments. But this is exactly the shape r11's pattern-observation called out: T-7 added the switch in *one* place, the *other* place that ALSO needs it sits in a different file owned by the restore path. The next person who flips the flag (or who adds a feature to the cold-boot path) won't see the wake path silently drift.
- **Tests**: the T-7 test suite (7 new tests, `nomad_ch.rs:4095-4338`) pins the switch behavior on the cold-boot builder only — there's no equivalent test that asserts `build_restore_nomad_job_json` either (a) also honors the flag, or (b) is documented as deliberately not honoring it. Without a regression pin, drift is undetectable.
- **Action**:
  - (a) Plumb `TaskDriverMode` (or `task_driver_mode_from_env()`) into `build_restore_nomad_job_json` and add a matching `match mode { RawExec | ChPlugin }` branch that emits the typed Config block for the restore path (the `restore_from` typed field already exists on the Go driver's `TaskConfig` per T-7's own field map). Add 2 regression tests symmetric with the cold-boot ones.
  - (b) Alternative: collapse `build_restore_nomad_job_json` into `build_nomad_job_json_with` by passing `restore_from: Some(...)` from the restore handler. The cold-boot signature already supports it (T-7 added the param). This eliminates the duplicate builder and the drift hazard in one move.
- **Recommendation**: option (b) — it's already 90% of the way there because T-7 already added the `restore_from: Option<&Path>` arg. The remaining work is plumbing `user_id` / `project_id` through the restore call (they exist on `RestoreHandler` via `snap.user_id`) and dropping `build_restore_nomad_job_json` entirely.

### [R12-M1] Rollback closure's `spawn_blocking(...)`.await result is silently discarded with `let _ =` — JoinError on panic is unreachable to logs (MINOR, concurrency-r12)

- **Files**: `crates/sandbox/src/restore_handler.rs:294-298`.
- **Shape**:
  ```rust
  let _ = compio::runtime::spawn_blocking(move || {
      backend_for_teardown.teardown_restore(sandbox_id_for_teardown, snap_vm_index);
  })
  .await;
  ```
- **Issue**: the closure body is sync and returns `()`, so the join is `Result<(), JoinError>`. `let _ =` discards both the `Ok(())` and the `Err(JoinError)`. If `teardown_restore` panics — which could happen on a poisoned `state.write()` lock that doesn't have the `unwrap_or_else(|p| p.into_inner())` poison-recover (`nomad_ch.rs:1709` has it; `release_vm_index` and `nomad_delete_blocking` paths are bare `.unwrap()` on at least one site each — see [R10-Q3 carry-forward]) — the panic is captured by the blocking pool, materialised as a `JoinError`, and then `let _` throws it away. Operator gets NO log evidence that rollback panicked: the next thing in logs is the `update_sandbox_status(target, g1, None)` write at `:299-301`, which proceeds normally as if teardown had succeeded.
- **Contrast with siblings**: the other 3 `spawn_blocking(...).await` sites in `do_restore_inner` (`:430`, `:522`, `:540`) and `clock_resync_post_restore` (`:1667`) all `.unwrap_or_else(|p| Err(format!("spawn_blocking panic: {p:?}")))` and propagate. The rollback closure is the **only** spawn_blocking site that mutes JoinError. Asymmetric.
- **Combined with [R11-C1]+[R11-C2]+[R10-S2]**: the rollback path now has THREE silent-fail layers stacked:
  1. R10-S2 carry: JoinError swallow at `:298` (this finding).
  2. R11-C1: `nomad_handle.is_some()` skip → state-map cleanup silently no-ops if wiring regresses.
  3. R11-C2: 2-await drop window → pg row stuck in `Restoring` if future drops between `:298` and `:301`.
- **Why minor, not critical**: today's prod wiring has `nomad_handle = Some(...)` and `teardown_restore`'s internal calls either log on failure (`nomad_delete_blocking` warns and continues) or are statistically very unlikely to panic (release_vm_index back to `IntervalSet`, state.write() on a healthy RwLock). The JoinError swallow shows up only if a developer adds a panic-prone op to `teardown_restore`'s body; with the path used today it's invisible.
- **Action**: replace `let _ = compio::runtime::spawn_blocking(...).await;` with the standard pattern from `:540`:
  ```rust
  if let Err(p) = compio::runtime::spawn_blocking(move || { ... }).await {
      tracing::error!(sandbox_id = %sandbox_id, panic = ?p,
          "restore rollback: teardown_restore spawn_blocking panicked");
  }
  ```
  2 lines, symmetric with the 3 other sites in the same function, and one fewer silent-fail in the rollback stack. Optionally make `teardown_restore` return `Result<(), String>` so the body's internal failures surface upward too — but that's R4-A2 territory.

### [R12-M2] `task_driver_mode_from_env()` is called per-CREATE rather than cached at backend construction — small env-Mutex contention + footgun if env mutates mid-flight (MINOR, concurrency-r12)

- **Files**: `crates/sandbox/src/backend/nomad_ch.rs:2239-2244` (helper), `:2301` (sole production caller, transitively from `build_nomad_job_json` at `:2279`), `:718` (calls `build_nomad_job_json` per-CREATE).
- **Shape**: every `NomadCHBackend::create()` invocation reads `std::env::var("SANDBOX_TASK_DRIVER")`. Under libstd, env reads acquire a global Mutex around the env-table (`std::sys::pal::unix::os::ENV_LOCK`). At controller-realistic CREATE QPS (single-digit per controller-second baseline) this is microscopic; under a future stress-test or recovery storm the path is a sequential bottleneck.
- **Footgun**: env vars can technically mutate at runtime via `std::env::set_var` (which the test fixtures DO; production has no such call but a future ops-tool that writes env in-process opens the surface). If the env flips between (a) cold-boot CREATEs (one mode) and (b) a wake-path CREATE on the same backend instance moments later (different mode after a non-rollback env flip), the cluster ends up with mixed-driver allocs the operator did not deliberately request. The right shape is "read-once at backend construction; immutable after."
- **Why minor**: today's production deployments set `SANDBOX_TASK_DRIVER` via systemd unit (`gcp-worker-startup.sh:442`) at boot; it never mutates in-process. The contention is theoretical, the footgun is theoretical. But fixing this is a 3-line change with strict structural improvement.
- **Action**: add `task_driver_mode: TaskDriverMode` as a field on `NomadCHBackend`, populate from `task_driver_mode_from_env()` in the constructor (`NomadCHBackend::new` or wherever `Self { ... }` is built), and pass it to `build_nomad_job_json_with` directly. Drop `build_nomad_job_json` (the env-reading wrapper) entirely; callers pass the field explicitly. This closes both the contention micro-issue and the in-process-mutation footgun.

### [R12-V1] Await-count for `do_restore_inner` verified at 7 — no NEW widening since r10 (VERIFICATION, concurrency-r12)

- **Files**: `crates/sandbox/src/restore_handler.rs` lines:
  1. `:429` — `spawn_blocking(store.get).await`
  2. `:521` — `spawn_blocking(submit_restore_job).await`
  3. `:539` — `spawn_blocking(wait_for_livez).await`
  4. `:582` — `persist.unseal(sandbox_id).await`
  5. `:593` — `clock_resync_post_restore(...).await`
  6. `:625` — `db.update_sandbox_status(...).await`
  7. `:627` — `db.clear_snapshot_metadata(...).await`
  Verified by `awk` over the `fn do_restore_inner` body. Count: **7**. r10-C2 era count: **7**. r11 count: **7**. **Delta: 0**. Confirmed by `git log --oneline crates/sandbox/src/restore_handler.rs` returning a single commit (`be246395`) — no changes since r11's audit.
- **R11-C2 rollback closure's 2-await widening is OUTSIDE `do_restore_inner`** — it lives in the calling shell at `:294-310`. The count above counts the inner only, matching r11's convention.
- **Action**: none. Carry-forward [R11-I1] still applies (spawn_blocking awaits 1-3 have the "side-effects always land on future-drop" subtlety, which is structurally a LeasedVmSlot problem).

## Detached-spawn audit

Surveyed all `compio::runtime::spawn` and `compio::runtime::spawn_blocking` sites in `crates/sandbox/src/`. **No new sites added since r11.** Census matches r11:

- `restore_handler.rs:294` — R10-C2 rollback wrap. Closure body verified SYNC (calls `teardown_restore` which uses `ureq::delete`, `state.write().unwrap_or_else()`, `release_vm_index` — all sync). No async ops inside the blocking thread → no silent fail. **JoinError discarded at `let _ =`** — see [R12-M1].
- `restore_handler.rs:426, 513, 536, 1602` — pre-r10 spawn_blocking wraps; unchanged.
- `admin_handlers.rs:1311` — R7-C1 detached teardown, still no JoinSet/Semaphore/shutdown signal. Carry-forward.
- `registry.rs:829` — preview-URL housekeeping detached spawn. Out of restore/snapshot path.
- `lib.rs:989, 1072, 1283, 2145`, `sweep.rs:227, 563`, `main.rs:115` — background loop spawns (heartbeat, sweep, control plane sync). Out of restore/snapshot hot path.
- `backend/nomad_ch.rs:2002, 2093, 2875, 2888, 2903, 2978, 3089, 3223` — internal `submit_nomad_job` / probe / DELETE / livez polling. Unchanged.

No new spawn sites introduced by T-7 (the commit touches only `build_nomad_job_json_with`'s body, which is sync) or by the GCS BufReader/BufWriter commit (which touches only sync I/O paths inside an existing spawn_blocking closure).

## Registry.rs RwLock unwrap audit (R10-Q3 carry-forward)

- **Count**: `grep -c '\.unwrap()' crates/sandbox/src/registry.rs` → 42 total; ≈31 on `RwLock::read()` / `RwLock::write()` acquisitions.
- **Sampled 5 sites**:
  1. `:196` — `*self.last_used.read().unwrap()` — Instant deref. Reader-only; cannot trigger poison from a reader panic (only writer panics poison). Effectively safe. **Bare unwrap is benign here.**
  2. `:298` — `self.by_sandbox.write().unwrap().insert(...)` — HashMap insert under write lock. If poisoned, prior writer aborted mid-op; HashMap structurally remains valid because the operation is atomic at the std level. Recoverable. **Bare unwrap is sub-optimal; `unwrap_or_else(|p| p.into_inner())` would be drop-in safe.**
  3. `:347` — `self.by_sandbox.read().unwrap()` — read-only; cannot panic from reader poisoning. **Safe.**
  4. `:349` — `*s.preview_secrets.write().unwrap() = secrets` — full overwrite; pre-state irrelevant. Recoverable. **Sub-optimal; into_inner() pattern is drop-in safe.**
  5. `:559` — `let mut sandboxes = self.by_sandbox.write().unwrap();` — write lock; recoverable. **Sub-optimal.**
- **Verdict**: the bare `.unwrap()` on `RwLock::read()` sites is **defensible** (read-only access cannot panic-poison). The bare `.unwrap()` on `RwLock::write()` sites is **sub-optimal but practically benign**: HashMap operations don't panic under normal load (only allocator-OOM, in which case the process is going down anyway), and the operations are short / non-await-spanning. The contrast with `nomad_ch.rs:1709`'s `unregister_restored` (which uses `unwrap_or_else(|p| p.into_inner())`) is real and inconsistent.
- **Why not concurrency-critical**: registry.rs is on the preview-URL / sandbox-lifecycle path, NOT the snapshot/restore path. None of these sites is reachable from `do_restore_inner` / `do_snapshot_inner`. **Out of scope for concurrency-r12 lens.**
- **Action**: stays under "code-quality" debt. A 3-pass refactor (replace `.write().unwrap()` → `.write().unwrap_or_else(|p| p.into_inner())` everywhere) is straightforward and would normalise the crate. Not blocking.

## Carry-forward (escalation status)

| Finding | Open Since | Cycles | Severity Trajectory |
|---|---|---|---|
| **R4-A2 / R5-A2** LeasedVmSlot RAII | r4 | **9+** (incident-class) | Would close R10-C1, R10-C2, R11-C1, R11-C2, R11-I1, R12-M1, 6 C3 widenings in one PR. **The cost-of-doing-nothing now exceeds the cost-of-doing.** |
| **R11-C1** `unregister_restored` silent-fail-OPEN on `nomad_handle=None` | r11 | 1 | Open. Production wiring is `Some(...)` today, so unreachable on prod, but the silent-fail shape is the same R7-S2 hazard the team explicitly removed elsewhere. |
| **R11-C2** rollback closure 2-await window | r11 | 1 | Open. Same cancel-window shape R10-C2 was meant to close in `do_restore_inner` — the wrap regressed the same invariant into the rollback path. |
| **R10-Q3** registry.rs 35+ bare RwLock unwraps | r10 | 2 | Open. **Re-assessed at [R10-Q3 carry-forward] above**: read-locks are safe (reader panics don't poison); write-locks are sub-optimal but practically benign. Code-quality, not concurrency-critical. **Out of restore/snapshot path.** |
| **R10-S2** spawn_blocking JoinError swallow | r10 | 2 | Open. **Compounded at [R12-M1]** — the rollback closure at `:294-298` is the latest silent-fail site, asymmetric with the other 3 spawn_blocking sites in the same file. |
| **R7-C1** detached teardown task | r7 | 5 | Open. No JoinSet/Semaphore/shutdown signal. Compounds with [R11-C2]: if request future drops mid-rollback AND detached teardown is still running, two concurrent tasks on the same sandbox_id's state map with no coordination. |
| **C3** cancel-unsafety in `do_restore_inner` | r3 | 9 | Still 6 widenings (no 7th this round). Subsumed by LeasedVmSlot. |
| **R10-M2** spawn_blocking panic-format `Any { .. }` | r10 | 2 | Open. 4+ sites still duplicated. |

## Await count for do_restore_inner
- HEAD count: **7**
- r10-C2 era: **7**
- r11 count: **7**
- **Delta: 0** (no new widening to `do_restore_inner`'s body)
- Note: R11-C2's 2-await widening lives in the CALLING SHELL at `:294-310`, OUTSIDE `do_restore_inner`. Not counted here.

## Pattern observation — additive-fix accretion continues

R11's framing called this out as "additive-fix accretion." T-7 deliberately does NOT touch the restore path — but in doing so, it cements the **second** half of a split-brain (cold-boot opts into ch_plugin; wake does not). [R12-I1] is a NEW shape: not concurrency-bug but feature-flag-coverage gap, with concurrency implications under partial cutover (mixed-driver allocs on the same vm_index).

The cycle has stayed steady at "1 concurrency-shape per round" for 4 rounds running (R9-C1, R10-C1/C2, R11-C1/C2, R12-I1+M1). LeasedVmSlot RAII would dissolve the entire family.

## Status block (one-liner)

```
Round 12:
  NEW: R12-I1 (T-7 flag covers cold-boot only — restore path hard-codes raw_exec),
       R12-M1 (rollback spawn_blocking JoinError discarded at let _ =),
       R12-M2 (task_driver_mode read per-CREATE — should be cached at backend ctor),
       R12-V1 (await count for do_restore_inner verified at 7, no new widening).
  T-7 AUDIT: passes concurrency review. No new locks, awaits, spawns, or shared-state
             mutations introduced by 5fe36805. Test seam T7_ENV_LOCK is deadlock-safe
             wrt db.rs::ENV_LOCK (disjoint env-var sets).
  CARRIED: R4-A2 LeasedVmSlot (9+ cycles, incident-class — would close 8+ findings),
           R11-C1 (unregister_restored silent-fail-OPEN, 2 cycles),
           R11-C2 (rollback 2-await window, 2 cycles),
           R11-I1 (spawn_blocking cancel-semantics nuance, 2 cycles),
           R10-Q3 (registry RwLock unwraps — re-assessed: out of restore path,
                   safe-or-benign in practice; stays as code-quality debt),
           R10-S2 (JoinError swallow, 3 cycles, now compounded by R12-M1),
           R7-C1 (detached teardown, 5 cycles).
```
