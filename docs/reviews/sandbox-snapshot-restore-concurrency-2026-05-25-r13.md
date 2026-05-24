# Sandbox/snapshot-restore — concurrency r13 review

Date: 2026-05-25 (UTC)
HEAD at audit: `e887b8ee`
Round 13 of N. Read-only. Branch `feat/sandbox-snapshot-restore`.

## Summary
- 3 findings (1 critical NEW, 1 important NEW, 1 verification).
- R12-I1 fix (b3bf741c) **introduced a real cross-module test-time env-mutex race**: a second module-local Mutex (`R12_I1_ENV_LOCK` in `restore_handler.rs`) serialises the same process-wide env var (`SANDBOX_TASK_DRIVER`) that `T7_ENV_LOCK` already guards in `nomad_ch.rs`. Because both modules belong to the same `cargo test -p zeroship-sandbox` binary and env reads/writes are process-global in Rust, parallel tests **DO** race. The concurrency r12 verdict ("PASSES concurrency review") was correct ONLY for the T-7 patch as it landed; the b3bf741c fix that landed under the r12 review window reintroduces the surface architecture r12 surfaced. File as [R13-C1].
- Re-sampled 5 fresh `registry.rs` sites: lock-ordering audit confirms NO inversion across `by_sandbox` ↔ `preview_secrets`/`preview_audit`/`by_user_project`. Bare `.unwrap()` on writes remains sub-optimal but benign (re-affirms r12's verdict on a different sample set).
- New shape surfaced by re-reading R11-P1: under c=20 wake-storm, `do_restore_inner`'s 5 separate `open_pool()` calls each eagerly open `min_idle=2` pg conns → **200+ in-flight pg connections** against the default `max_connections=100`. The rollback-path's own `open_pool()` is **also** starved, so the silent-fail rollback shape compounds at the conn-exhaustion edge. File as [R13-I1].
- `do_restore_inner` await count: **7**. No delta from r10/r11/r12.
- All r12 carry-forwards remain open. R4-A2 LeasedVmSlot RAII now **10+ cycles open**.

## R12-I1 env-mutex cross-module race verification

**Verdict: real race window. The two mutexes do NOT share state and DO guard overlapping process-global state.**

Concrete shape verified at HEAD:

- `crates/sandbox/src/backend/nomad_ch.rs:4073`
  ```rust
  static T7_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
  // ... :4082-4093
  fn with_task_driver_env<R>(value: Option<&str>, f: impl FnOnce() -> R) -> R {
      let _g = T7_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
      match value {
          Some(v) => unsafe { std::env::set_var("SANDBOX_TASK_DRIVER", v) },
          None => unsafe { std::env::remove_var("SANDBOX_TASK_DRIVER") },
      }
      let out = f();
      unsafe { std::env::remove_var("SANDBOX_TASK_DRIVER") };
      out
  }
  ```

- `crates/sandbox/src/restore_handler.rs:2478`
  ```rust
  static R12_I1_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
  // ... :2484-2496
  fn with_task_driver_env<R>(value: Option<&str>, f: impl FnOnce() -> R) -> R {
      let _g = R12_I1_ENV_LOCK
          .lock()
          .unwrap_or_else(|e| e.into_inner());
      match value {
          Some(v) => unsafe { std::env::set_var("SANDBOX_TASK_DRIVER", v) },
          None => unsafe { std::env::remove_var("SANDBOX_TASK_DRIVER") },
      }
      let out = f();
      unsafe { std::env::remove_var("SANDBOX_TASK_DRIVER") };
      out
  }
  ```

These are **two distinct `Mutex<()>` instances** in **two different module-scope statics**, both compiled into the same `zeroship-sandbox` test binary. `cargo test` runs library tests in parallel on a shared thread pool by default (jobs ≈ logical CPUs). `std::env::set_var` mutates a process-wide table.

Race interleaving (real, not theoretical):

```
Thread A (running nomad_ch::tests::nomad_job_spec_uses_ch_when_flag_set)
  takes T7_ENV_LOCK
  set_var("SANDBOX_TASK_DRIVER", "ch_plugin")
  enters f() → calls task_driver_mode_from_env()
                          ↓
Thread B (running restore_handler::tests::nomad_restore_job_spec_uses_raw_exec_by_default)
  takes R12_I1_ENV_LOCK          ← DIFFERENT MUTEX, NO MUTUAL EXCLUSION VS T7_ENV_LOCK
  remove_var("SANDBOX_TASK_DRIVER")
  enters f() → calls task_driver_mode_from_env()
            → reads SANDBOX_TASK_DRIVER == UNSET → returns RawExec
            (but the test was meant to assert behavior under flag-set!)
                          ↓
Thread A's read happens after B's remove_var → returns RawExec
  but the test asserted ChPlugin → FAILS
```

Symmetric inversions:
- A holds T7_ENV_LOCK and asserts `Driver == "ch"`; B's `remove_var` flips the env mid-test → A's `task_driver_mode_from_env()` returns RawExec → test_ch fails.
- B holds R12_I1_ENV_LOCK and asserts `Driver == "raw_exec"` by default; A's `set_var("ch_plugin")` flips the env mid-test → B's helper reads ChPlugin → test_default fails.
- Both helpers `remove_var` on the way out. If A finishes its `remove_var("SANDBOX_TASK_DRIVER")` while B is still inside f() under the ch_plugin branch (B's test wanted ChPlugin) → B's `task_driver_mode_from_env()` returns RawExec → test_ch_set fails.

This is **not theoretical**: both modules' test files mutate the SAME named env var via `unsafe { std::env::set_var }` / `remove_var`. The locks are not shared. Reproduction requires only that the two test functions land on different `cargo test` worker threads simultaneously (likely on every multi-core CI box).

**Why this matters for concurrency review (not just test flake)**: the b3bf741c commit message explicitly says "cross-crate sharing of the symbol would expose pub(crate) test internals" — but this isn't cross-crate; both modules are in the same crate. Sharing one mutex within the crate is a `pub(crate)` add to `nomad_ch::tests::T7_ENV_LOCK` (or lift it to a `crates/sandbox/src/test_env.rs` shared `pub(crate)` module). The CR-time rationale for the duplicate was incorrect.

This is also EXACTLY the shape concurrency r12 declared safe under "Cross-lock check: T7_ENV_LOCK (nomad_ch.rs tests) and ENV_LOCK (db.rs tests) guard **disjoint env-var sets** … No test should need both; no deadlock potential." That holds for the `T7 ↔ db.rs::ENV_LOCK` pair (disjoint vars). The new pair `T7 ↔ R12_I1_ENV_LOCK` overlaps on `SANDBOX_TASK_DRIVER`.

## Findings (NEW since r12)

### [R13-C1] T7_ENV_LOCK ↔ R12_I1_ENV_LOCK same-env-var race (CRITICAL-test-only, concurrency-r13)

- **Files**:
  - `crates/sandbox/src/backend/nomad_ch.rs:4073` (`T7_ENV_LOCK`) + `:4081-4093` (`with_task_driver_env`).
  - `crates/sandbox/src/restore_handler.rs:2478` (`R12_I1_ENV_LOCK`) + `:2484-2496` (`with_task_driver_env`).
- **Tests at risk** (every one of these can flip its expected mode mid-execution under parallel `cargo test`):
  - `nomad_ch::tests::nomad_job_spec_uses_raw_exec_by_default` (:4098)
  - `nomad_ch::tests::nomad_job_spec_uses_ch_when_flag_set` (:4119, expected ChPlugin)
  - `restore_handler::tests::nomad_restore_job_spec_uses_raw_exec_by_default` (:2507)
  - `restore_handler::tests::nomad_restore_job_spec_uses_ch_when_flag_set` (:2552)
- **Shape (verified)**: the two module-local `Mutex<()>` statics serialise within their own module ONLY. Both modules' helpers do `unsafe { set_var("SANDBOX_TASK_DRIVER", ...) }` / `remove_var("SANDBOX_TASK_DRIVER")`. The env table in libstd is process-global; the two locks do not exclude each other.
- **Severity (test-only-but-CI-blocking)**: this won't burn production — the production code reads env once per submit and the controller's systemd unit pins the value at boot. But it **will burn CI**: the next `cargo test -p zeroship-sandbox` on a multi-core machine has a non-trivial probability of:
  - one of the four tests flipping from "passes" to "fails" intermittently;
  - or worse, a state where set_var by one thread leaks into a CRATE test that doesn't take either lock (e.g., any future test in `nomad_ch.rs` that incidentally reads `task_driver_mode_from_env` without explicitly setting the env).
- **Recovery sample**: the helper at line 4091 does `remove_var` on exit. If thread A holds T7 and thread B holds R12_I1 and they run interleaved, A's `remove_var` on exit could happen WHILE B is still inside f() expecting the var SET — surface: B reads "RawExec" when ChPlugin was intended.
- **Why the existing comment doesn't help**: `restore_handler.rs:2473-2477`'s comment says "We don't share the same mutex symbol across crates" — but `nomad_ch` and `restore_handler` are in the SAME crate. The comment misreads the surface. Cross-CRATE sharing is impossible without `pub` (and not desired); cross-MODULE sharing within the same crate is `pub(crate)` and exactly what's needed here.
- **Action** (one-line CR after T-8b):
  - **Option (a)**: lift `T7_ENV_LOCK` to `pub(crate)` and import it in `restore_handler::tests`. 2 lines moved, 1 line added on the import side. The crate already has the `#[allow(unsafe_code)]` machinery in both modules.
  - **Option (b)**: introduce `crates/sandbox/src/test_env.rs` with `pub(crate) static SANDBOX_TASK_DRIVER_LOCK: Mutex<()> = Mutex::new(());` and a `pub(crate) fn with_task_driver_env<R>(...) -> R` helper. Both modules call into it. Tightest shape — duplication of the helper body also disappears.
  - **Option (c)**: run the affected restore_handler tests serial (`#[serial]` or via `cargo test -- --test-threads=1` in CI). Looser, doesn't address the underlying duplication, but a 0-LoC patch if needed before a real fix.
- **Recommendation**: option (b). The duplicated `with_task_driver_env` body is 12 lines × 2 = a maintenance liability, and the next env var that needs serialising (e.g., `SANDBOX_HA_*` already in `db.rs::tests::ENV_LOCK`) would benefit from a single home for the pattern.

### [R13-I1] Pool-churn (R11-P1) becomes a concurrency cliff under c≥20 wake-storm — rollback path competes for the same starved conn budget (IMPORTANT, concurrency-r13)

- **Files**: `crates/sandbox/src/db.rs:510-516` (per-call `open_pool`), `crates/compio-postgres/src/pool.rs:265-300` (eagerly opens `min_idle.max(1)` conns), `crates/sandbox/src/restore_handler.rs:299-301` (rollback-path `update_sandbox_status` also opens its own pool).
- **Concurrency surface (verified)**:
  - `do_restore_inner` makes 5 separate `open_pool()` calls per wake (`db.rs:1404, 1447, 1478, 1509, 2353` reachable + r12-V1's per-call audit). Each Pool's constructor eagerly opens `config.min_idle.max(1)` connections (`pool.rs:265-283`); default `min_idle = 2` (`pool.rs:73`).
  - Per wake: minimum 5 × 2 = **10 fresh pg conns**. At c=20 concurrent wakes: **200 conns** in flight simultaneously.
  - The crate's own deferred-tracker comment at `db.rs:494-509` acknowledges the per-call cost but bills it as latency-only ("Current pattern is correct, just slow"). The note **misses** the conn-exhaustion side effect at scale.
- **Conn-exhaustion cliff**: Postgres default `max_connections = 100`. Once the wake-storm crosses ~10 concurrent restores (5×2×10 = 100), new `open_pool` calls hit `FATAL: sorry, too many clients already` → `do_restore_inner` returns `RestoreHandlerError::Database(Pg(...))` → falls into the rollback closure at `restore_handler.rs:268-313`.
- **Rollback also opens its own pool**:
  - The closure calls `teardown_restore` (sync, no pg) in spawn_blocking — fine.
  - THEN calls `db.update_sandbox_status(sandbox_id, target, g1, None).await` at `:299-301`. That method opens its OWN pool (`db.rs:1478, open_pool` — verified to call `self.open_pool().await?`).
- **Cascade**: rollback's `open_pool` competes for the same starved pg resource that just failed the primary path. If the starvation persists for a few hundred ms (Postgres' conn-recycling pace), the rollback's `update_sandbox_status` ALSO returns `Pg(...)`, which lands in the `error!` log at `restore_handler.rs:303-309` ("rollback failed; row may be wedged in `restoring` until lease-takeover sweep") — and the row sits in `Restoring` until the `claim_orphan_transient_for_recovery` sweep notices.
- **Compounds with carry-forwards**:
  - **R11-C2** (rollback 2-await drop window): the second await IS this `update_sandbox_status`. If it errors AND the future is cancelled at the conn-exhaustion edge, no log, no recovery, just a wedged row.
  - **R12-M1** (rollback spawn_blocking `let _ =`): the JoinError on `teardown_restore`'s panic is also silently dropped. Stacked silent-fails.
  - **R7-C1** (detached teardown): under c≥20 wake-storm, detached teardown tasks also call backend methods that may chain to db.update_*. Three competing call paths for the same starved pool resource.
- **Why not critical**: production worker-pool sizing is presumably `c=8` or `c=16` in current cluster smokes (T-8b ran on a 1-worker fleet); the c=20 threshold isn't reached in current scale tests. But the deferred note's "correct, just slow" framing UNDERSELLS the cliff: 5 conns/restore × c-workers, multiplied by the rollback path's own pool, means the working margin to `max_connections` is much thinner than it looks.
- **Action**:
  - **Short-term** (concurrency-safe, perf-neutral): lower `Database::pool_max` default from 16 → small (4–8), AND lower `min_idle.max(1)` default from 2 → 1 (in this crate's call sites, not in `compio-postgres`). At min_idle=1 the per-call cost is 1 fresh conn instead of 2; cuts the c=20 inflight conn count from 200 → 100, which sits exactly at PG default — still tight but not over-cliff.
  - **Medium-term** (R11-P1 proper): a `thread_local!` cached `Pool` per compio worker thread, populated lazily and never dropped for the lifetime of the worker. Closes both R11-P1 (perf) and R13-I1 (conn-cliff) in one move. The deferred tracker already names this fix; promote it from "performance" to "concurrency-correctness" so it's not stuck behind perf-budget gating.
  - **Long-term** (broader): a single per-controller `Arc<Pool>` cached on `Database` (currently blocked by `Pool: !Send + !Sync`). Worth re-checking whether compio-postgres can relax to `!Send + Sync` (immutable Pool handle, all `Send` internal state behind a `Mutex` or atomics).
- **Why important, not critical**: the cliff only fires at c≥10 with default PG and at c≥20 with PG raised to `max_connections=200`. Today's smoke is c=1, T-8b cutover is c≤8 per worker. Not on the critical path right now; but the rollback compounding is a structural concern the deferred R11-P1 framing didn't surface.

### [R13-V1] do_restore_inner await count holds at 7 — R12-I1 fix added no new awaits (VERIFICATION, concurrency-r13)

- **Files**: `crates/sandbox/src/restore_handler.rs:376-639` (`do_restore_inner`).
- **Awaits enumerated**:
  1. `:429` — `spawn_blocking(store.get).await`
  2. `:521` — `spawn_blocking(submit_restore_job).await` (now also reads `SANDBOX_TASK_DRIVER` via `task_driver_mode_from_env()` inside the blocking closure — sync, no new await)
  3. `:539` — `spawn_blocking(wait_for_livez).await`
  4. `:582` — `persist.unseal(sandbox_id).await`
  5. `:593` — `clock_resync_post_restore(...).await`
  6. `:625` — `db.update_sandbox_status(...).await`
  7. `:627` — `db.clear_snapshot_metadata(...).await`
- **r10/r11/r12 era**: 7. **HEAD**: 7. **Delta: 0**.
- **R12-I1 fix shape (b3bf741c)** added the env read INSIDE `submit_restore_job` (line :1116 — `let mode = ...task_driver_mode_from_env();`), which runs inside the existing `spawn_blocking` closure at `:513-520`. It is a SYNC operation inside an already-blocking thread — no `.await` added in `do_restore_inner`'s body. Cancel-window count is unchanged.
- **Action**: none. Carry-forward [R11-I1] still applies (the 3 spawn_blocking awaits' side-effects-on-drop subtlety).

## Detached-spawn audit

Surveyed `compio::runtime::spawn\b` in `crates/sandbox/src/`. **Census unchanged from r12. No new sites since r11.**

```
admin_handlers.rs:1311  — R7-C1 detached teardown task. Open carry-forward.
lib.rs:989, 1072, 1283  — control-plane / heartbeat / sync loops. Out of restore path.
lib.rs:2145              — Background long-lived task (graceful-shutdown path).
main.rs:115              — main loop spawn.
preview_ws.rs:53         — `use ... spawn;` (import only; sites are elsewhere).
registry.rs:829          — preview-URL housekeeping. Out of restore path.
sweep.rs:227, 563        — sweep / claim-orphan loops.
```

No new spawn site introduced by b3bf741c. The R12-I1 fix touches only sync data-flow inside an existing `spawn_blocking` closure.

## Registry.rs RwLock unwrap re-sample (R10-Q3 carry-forward)

Re-sampled 5 fresh sites (different from r12's 5):

1. `:189-192` `Sandbox::touch()` — `last_used.write()` guarded by `if let Ok(mut t)`. On poison, silently NO-OPs (touch is best-effort). **Defensive — correct shape.**
2. `:362-376` `ensure_preview_secret` — outer `by_sandbox.read().unwrap()` HELD WHILE acquiring `s.preview_secrets.write().unwrap()`. **Two-lock chain, outer→inner ordering**. Same ordering used by all other callers (`:347-350`, `:385-390`, `:428-430`, `:474-476`). No deadlock. Bare unwrap; recoverable on poison; sub-optimal.
3. `:362-376` (same site) — write-then-deref pattern: `let mut w = s.preview_secrets.write().unwrap(); if let Some(ring) = w.as_ref()`. Holds write the entire path; reads under write are fine. **Safe.**
4. `:385-413` `secret_for_version` — same outer-read / inner-write ordering. Lazy clear of `previous` under the write guard is atomic with respect to other writers. **Safe.** Bare unwrap; sub-optimal.
5. `:558-567` `remove` — outer `by_sandbox.write()` THEN inner `by_user_project.write()`. Verified by reading `:298-299` (`insert_inner`): SAME outer-then-inner ordering (`by_sandbox.write()` then `by_user_project.write()`). **No lock-ordering inversion across the crate.** Bare unwrap on both writes; sub-optimal but recoverable.

**Verdict (re-affirms r12)**: registry's lock ordering is consistent — every multi-lock path takes `by_sandbox` (read or write) FIRST and inner per-sandbox / by_user_project locks SECOND. No inversion to introduce a deadlock. Bare `.unwrap()` on writes is recoverable + practically benign (HashMap inserts don't panic under normal load). **Out of restore/snapshot path.** Stays as code-quality debt.

## R11-S2 host_id reader (post-85e4f2f9) concurrency check

- **Files**: `crates/sandbox/src/db.rs:1075-1188` (host_id load + mode/uid enforce).
- **Call site**: `load_or_generate_host_id()` is invoked from `Database::new` at `:369`, exactly once during controller startup, before `Database` is shared across compio workers. **No new concurrent surface introduced**.
- **HA peer takeover path**: `claim_orphan_transient_for_recovery` at `:2569-2698` reads `self.config.host_id` (cloned into `Self` at construction), NOT the on-disk file. The on-disk read happens once at process start; subsequent CAS uses the in-memory value. **No new race with peer takeover.**
- **Crash-restart shape**: if peer-A's controller dies, peer-B's controller comes up reading its OWN `<state_dir>/host_id` file (different filesystem, different host). The mode/uid check is per-host: peer-B enforces 0o600 + uid==0 on peer-B's local file. No cross-host file sharing. **No new race.**
- **Audit verdict**: R11-S2 closure is concurrency-safe. The added mode/uid check is on a startup-only path with no shared state.

## Carry-forward (escalation status)

| Finding | Open Since | Cycles | Severity Trajectory |
|---|---|---|---|
| **R4-A2 / R5-A2** LeasedVmSlot RAII | r4 | **10+** (incident-class) | Would close R10-C1, R10-C2, R11-C1, R11-C2, R11-I1, R12-M1, 6 C3 widenings in one PR. **Cost-of-doing-nothing grows monotonically — every concurrency round since r4 has added a finding that LeasedVmSlot would dissolve.** |
| **R11-C1** `unregister_restored` silent-fail-OPEN on `nomad_handle=None` | r11 | 2 | Open. |
| **R11-C2** rollback closure 2-await window | r11 | 2 | Open. **Compounded by [R13-I1]**: the second await (`update_sandbox_status`) is itself a conn-exhaustion candidate under c≥20. |
| **R10-Q3** registry.rs 35+ bare RwLock unwraps | r10 | 3 | Open. Re-sampled 5 fresh sites this round; verdict unchanged: out of restore path, safe-or-benign. Code-quality, not concurrency-critical. |
| **R10-S2** spawn_blocking JoinError swallow | r10 | 3 | Open. **Compounded by [R12-M1]** (rollback closure asymmetric `let _ =`). |
| **R7-C1** detached teardown task | r7 | 6 | Open. **Compounded by [R13-I1]** — detached teardown's downstream pg calls compete with primary + rollback paths for the same starved pool budget. |
| **C3** cancel-unsafety in `do_restore_inner` | r3 | 10 | Subsumed by LeasedVmSlot. |
| **R10-M2** spawn_blocking panic-format `Any { .. }` | r10 | 3 | Open. |
| **R12-I1** (test-time env race) | r12-fix | 0 (NEW shape) | Filed as [R13-C1] above. The architecture r12 concern was correct. |
| **R11-P1** pool churn | r11 | 2 | Open. Re-classified by [R13-I1] from "perf, code-quality" to "concurrency-correctness at c≥10". |

## do_restore_inner await count
- HEAD count: **7**
- r10-C2 era: **7**
- r11 count: **7**
- r12 count: **7**
- **Delta: 0**
- Notes:
  - R11-C2's 2-await widening lives in the CALLING SHELL at `restore_handler.rs:294-310`, OUTSIDE `do_restore_inner`. Not counted here.
  - R12-I1 fix added a sync `task_driver_mode_from_env()` read INSIDE the existing spawn_blocking closure at `:513-520`. No new `.await`.

## Pattern observation

R12-I1 fix shape illustrates an additive-fix accretion pattern that the team should explicitly retire:

**"add a mirror lock when you cross a module boundary"** is the pattern; it's been applied **twice** in this crate now (`db.rs::ENV_LOCK` ↔ `nomad_ch::tests::T7_ENV_LOCK` — disjoint vars, fine — and now `T7 ↔ R12_I1_ENV_LOCK` — overlapping var, race).

The mitigation is a single crate-internal test-env helper module. Land it during the R13-C1 fix; future env-touching tests in any sandbox module pull from the same `pub(crate)` symbol.

LeasedVmSlot RAII would dissolve the entire family of restore-path concurrency findings (R11-C1, R11-C2, R12-M1, the C3 widenings, and R13-I1's rollback-side compounding). 10+ cycles of patching adjacents around the missing RAII abstraction; each round adds one to two new findings that would not exist if the abstraction were in place.

## Status block (one-liner)

```
Round 13:
  NEW: R13-C1 (T7_ENV_LOCK ↔ R12_I1_ENV_LOCK same-env-var race — test-flake, CI-blocking;
               the b3bf741c fix landed a duplicated module-local mutex against the same
               process-global var, race window confirmed),
       R13-I1 (R11-P1 pool churn reclassified: c≥10 concurrent wakes saturate default
               PG max_connections=100; rollback path competes for the same starved pool),
       R13-V1 (do_restore_inner await count holds at 7; R12-I1 fix added zero new awaits).
  R12-I1 FIX AUDIT: the fix closes the architecture concern (split-brain at T-8 cutover)
                    but introduces a test-time concurrency hazard (R13-C1). The CR comment
                    rationalising the duplicate mutex misreads "cross-crate" vs "cross-
                    module"; both modules are in the same crate and can share via pub(crate).
  CARRIED: R4-A2 LeasedVmSlot (10+ cycles, incident-class — would close 8+ findings
                                including R13-I1's rollback-side compounding),
           R11-C1 (unregister_restored silent-fail-OPEN, 2 cycles),
           R11-C2 (rollback 2-await window, 2 cycles, now compounded by R13-I1),
           R11-I1 (spawn_blocking cancel-semantics nuance, 2 cycles),
           R10-Q3 (registry RwLock unwraps — re-sampled 5 fresh sites; verdict unchanged),
           R10-S2 (JoinError swallow, 3 cycles, compounded by R12-M1),
           R7-C1 (detached teardown, 6 cycles, now compounded by R13-I1),
           R11-P1 (pool churn — re-classified concurrency-correctness via R13-I1).
```
