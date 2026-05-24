# Sandbox/snapshot-restore — performance r27 review

Date: 2026-05-25 (UTC).
HEAD at audit: `568c1357`.
Driver HEAD pin (per `gcp-worker-startup.sh`): v18 (`4b99b334…b58349`).
Controller version: v36 (`zeroship-sandbox.snapshot-v36`).
Prior: `docs/reviews/sandbox-snapshot-restore-performance-2026-05-25-r26.md` (HEAD `01b6a744`).
Cluster signal: T-8b-stress-r8 RED, 3/60 e2e OK (5.0 %) — `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-stress-r8.md`.

Round 27 closes the two r25/r26 sustained carries (R26-C1 pool-per-call, R26-I2 try_create blocking) and evaluates side effects from r7-A pg bump + r7-C-followup start_housekeeper + Option C Phase 2/4 (driver-side staging) + T5 fingerprint check.

## Summary

**9 findings, 0 CRITICAL (2 closed), 2 IMPORTANT (1 NEW, 1 CARRY), 5 MINOR (2 NEW closed-zero-cost, 3 CARRY).**

The two big r25→r26 perf wins landed in r27:
- **R26-C1 CLOSED at `ee702d5f` + `8c0b361e`** — thread-local `Rc<Pool>` cache + `start_housekeeper`. Cluster validated at T-8b-stress-r8: pg sat with 275 idle conns under `max_connections=500`, no `too many clients` FATALs.
- **R26-I2 CLOSED at `73725aa3`** — `spawn_blocking` around mkdir + mkfs.ext4 + fsync_dir bundle in `try_create`.

One new IMPORTANT surfaced:
- **R27-P1 NEW IMPORTANT** — T5 (`035c3564`) adds one extra signed-HTTP GET to the WAKE hot path (~10s ureq timeout, +~50-200ms typical wall) BEFORE clock_resync. The probe is correctly defensive against rollout skew, but it's now serial with clock_resync — both running before `register_restored`. The 10s timeout is per-call so a wedged agent stalls the WAKE for 10s before falling through `Skipped {reason: "transport_error"}`. Hot-path-add measured against cluster `WAKE 45.6s p50` (smoke gate) = +0.1-0.4% typical.

One latent perf-affecting bug closed:
- **R27-M2 LATENT** at `821cc9bd` — `wake_machine.rs` sanitize functions were doing `out.push(bytes[i] as char)` Latin-1 cast that allocated wrong codepoint widths. Bug never triggered (Nomad bodies are UTF-8 JSON), but the fix at `utf8_char_len_at` (wake_machine.rs:983) costs one extra `is_char_boundary`-class read per non-match byte. Net perf delta on error paths: immeasurable; correctness win.

## Carry table

| Finding | Status @ r27 | Evidence |
|---|---|---|
| **R26-C1 / R25-C1 / R23-P1 / R11-P1** thread-local pg pool | **CLOSED** at `ee702d5f` + `8c0b361e` (start_housekeeper) | `crates/sandbox/src/db.rs:62-105` (thread_local), `:617-642` (open_pool), `:657-672` (pool_audit). Cluster: T-8b-stress-r8 pg state 275 idle / 500 cap, no FATAL. |
| **R26-I2 / R25-I1 / R23-A2** `try_create` spawn_blocking | **CLOSED** at `73725aa3` | `crates/sandbox/src/backend/nomad_ch.rs:842-866` (Phase-2 ternary; `else` arm runs spawn_blocking). |
| R16-P3 Fuse encrypt + SHA | **OPEN** — no work landed. | (snapshot_handler/aead crate, not touched since r26) |
| R16-P5 gzip pre-AEAD | **OPEN** — no work landed. | (snapshot_aead.rs, not touched since r26) |
| R17-P2 Active-set cache | **OPEN** — no work landed. | (sweep.rs, not touched since r26) |
| R5-P1b SHA + BufReader path | **OPEN** — no work landed. | (snapshot_handler.rs, not touched since r26) |
| R26-M4 STOP path-split instrumentation | **OPEN** — no harness change yet. | (stress harness needs the split; controller code path is unchanged) |
| R26-T1 Surface `destroy_task_unreaped_total` | **OPEN** — driver counter exists; harness doesn't surface. | (driver-side, not in this crate) |

The two CRITICAL/IMPORTANT carries that gated stress-r5/r6 readiness are both CLOSED. The four R16/R17/R5 backlog items are unchanged — they remain TODO but were not on the r25-r26 priority list.

---

## CRITICAL

None this round. (Both prior CRITICAL/CRITICAL-adjacent carries closed.)

---

## IMPORTANT

### R27-P1 NEW IMPORTANT — T5 `/version` probe adds 10s budget + serial RTT to WAKE hot path

**File:Line:** `crates/sandbox/src/wake_machine.rs:485-536` (call site), `crates/sandbox/src/restore_handler.rs:3093-3273` (`verify_agent_version_post_restore`).

**Code shape (wake_machine.rs:499-505):**

```rust
match crate::restore_handler::verify_agent_version_post_restore(
    &agent_url,
    &sealed.signing_key_bytes,
    crate::restore_handler::CONTROLLER_GIT_COMMIT,
)
.await
```

This is on the WAKE happy path, executing AFTER `wait_for_agent_livez` returns 200 and BEFORE `clock_resync_post_restore`. Both T5 and clock_resync are serial signed-HTTP RTTs against the same agent.

**Cost components:**
1. **spawn_blocking wrap** (`restore_handler.rs:3159`) — fine; same shape as clock_resync, costs ~50µs of cross-thread handoff.
2. **`ureq::get(...).timeout(10s)`** (`restore_handler.rs:3172-3177`) — happy-path ~5-50ms agent RTT in cluster, 10s ceiling on wedged-agent.
3. **Per-call alloc** — `format!("{agent_url}/version")` (3154), `format!("{ts}")` (3174 indirect), nonce hex alloc, signature alloc, `body.into_string()` alloc. ~5-7 short allocations on the spawn_blocking thread per WAKE. Negligible vs RTT.

**Hot-path math:**
- T-8b-stress-r8 smoke `WAKE 45,649 ms` end-to-end (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-stress-r8.md:34`).
- T5 typical add: ~50-200 ms (one round-trip on the same agent process that just answered livez). Ratio: 0.1-0.4 %. **Immeasurable in cluster signal.**
- T5 worst case: 10 s (transport-error timeout) → `Skipped`, wake proceeds. Ratio: 22 % of the 45.6 s WAKE wall. Operator-visible.
- T5 NEVER trips the wedge that drives the current r8 RED (lock retention on rootfs.img is a CH restore failure, not a /version probe failure).

**The serial chain on success is now:**

```
restoring → wait_for_agent_livez (≤ livez_timeout) → verify_agent_version (≤ 10s) → clock_resync (≤ 10s) → register_restored → ok
```

Two consecutive 10s budgets is a 20s worst-case before reaching `register_restored`. Pre-T5 it was one 10s budget. The transport-error path is `Skipped` not `Mismatch`, so a 10s `/version` timeout doesn't fail the wake — it just delays it.

**Possible perf wins (if T5 becomes a measured tail-latency contributor):**
1. **Parallelize `/version` + `/_clock_resync`** with `futures::join!` — both already do `spawn_blocking` and target the same agent. Saves up to 10s on transport-error path, ~50-200ms on the happy path. ~15 LOC.
2. **Tighter timeout for `/version`** — the probe is additive (Skipped on failure means "wake proceeds"), so a 2-3 s budget would be sufficient and bounds the transport-error stall. Trade: real flaky-network sleeps may bypass the check more often. ~1 LOC.

**Recommendation:** observe T5 effect under stress-r9. If WAKE p99 widens >5%, parallelize. Don't pre-optimize.

**Priority:** IMPORTANT — new serial RTT on the WAKE hot path, defensive justification holds, but the 10s timeout is twice the budget the typical agent needs.

---

### R26-I2 (CLOSED) — `try_create` spawn_blocking sync-IO wrap

**File:Line:** `crates/sandbox/src/backend/nomad_ch.rs:842-866`.

**Status @ r27:** CLOSED at `73725aa3`. mkdir + mkfs.ext4 + fsync_dir bundle now lives inside `compio::runtime::spawn_blocking(move || -> Result<PathBuf, String> { ... })`. Only owned `PathBuf` clones cross the boundary; `&mut CreateGuard` stays on the ntex worker; `host_dir_created` flips AFTER spawn_blocking returns Ok. Per-CREATE wall is unchanged; sibling ntex workers stay free during the ~1-3 s mkfs window.

**Cross-validation:** Option C Phase 2 ternary at nomad_ch.rs:842 bypasses spawn_blocking entirely when `driver_stages_disk_images=true` (the driver does the mkfs on the alloc worker). Both branches are correct; under r8 the flag is ON (cluster validated at `T8b-stress-r8`). The spawn_blocking branch survives as the rollback path (`Phase 3 deletes the spawn_blocking branch entirely`).

**Cluster signal:** T-8b-stress-r8 reports `CREATE 6505 ms` smoke / `CREATE 100% 60/60` stress under flag ON — driver-side staging path. Under the flag OFF rollback path (Phase 2 default semantically), spawn_blocking would carry the same wall as before but free the ntex worker.

---

## MINOR

### R27-P2 NEW MINOR — thread-local `Rc<Pool>` retention: 275 idle conns observed, 91-conn cushion under `max_size=15`

**File:Line:** `crates/sandbox/src/db.rs:622-628` (`open_pool` `cfg.max_size = self.config.pool_max.max(2)`), `:662-668` (`pool_audit` same).

**Status:** CLOSED-ZERO-COST in throughput terms; documentation gap.

**Cluster observation (`T-8b-stress-r8.md:64-78`):**

```
SELECT count(*) FROM pg_stat_activity WHERE state IS NOT NULL → 275
  idle    274
  active    1
```

The 275 figure is `idle` conns retained by the thread-local cache across **3 controllers × N compio worker threads × `pool_max` per thread**. With `max_connections=500` (post r7-A), the cushion is ~225 conns. Pre-r7-A (cap=100), the same cache would have hit `too many clients`.

**The relationship is documented loosely in commit messages but not in db.rs source comments.** The thread-local cache's eviction shape is: `Rc<Pool>` lives for the lifetime of the compio worker thread. `start_housekeeper` (db.rs:640, 670) reaps idle CONNECTIONS within the pool down to `idle_timeout=600s` / `max_lifetime=1800s` defaults — but the `Pool` ITSELF lives forever (per-thread retention is by design). At steady state on a quiet controller, every cached pool retains AT LEAST 1 conn (the connection_count baseline floor in compio-postgres).

**Math @ T-8b-stress-r8:**
- 3 controllers × ~32 compio worker threads × ~3 cached conns avg = ~288 idle. Observed: 275. Consistent.
- Worst case with `pool_max=15` (cfg default): 3 × 32 × 15 = 1440 conns. Far above `max_connections=500`.

**Defense:** The thread-local pool only OPENS conns under demand. r7-A's `max_connections=500` was sized for the observed steady-state floor + bursting headroom. Operators must size `max_connections` to `controllers × worker_threads × steady_state_conns_per_pool + burst_headroom`.

**Recommendation:** add a comment to db.rs:617 documenting the `controllers × threads × pool_max` math for operator deployment guidance. ~10 LOC.

**Priority:** MINOR — observability/docs gap, no runtime cost.

---

### R27-P3 NEW MINOR — `start_housekeeper` race-loser pool gets a spawned task that immediately drops

**File:Line:** `crates/sandbox/src/db.rs:640-641` (open_pool) + `:670-671` (pool_audit).

**Code shape:**

```rust
pool.start_housekeeper();
Ok(install_pool(&POOL_APP_CELL, dsn.clone(), pool))
```

**Concern:** if two compio tasks on the same worker thread interleave around `Pool::connect_with_config().await`, both build a pool. Both then call `start_housekeeper()` on their local build. `install_pool` keeps the first one in the cache; the loser's `Rc<Pool>` drops on function exit. The loser's housekeeper task observes `Weak<Pool>::upgrade() → None` on its next tick and self-terminates.

**Cost components:**
1. **Spawned compio task overhead** — one extra task spawn (~100ns-1µs on compio).
2. **Conn handshake on the LOSER pool** — `Pool::connect_with_config` opens at least 1 conn on entry; that conn is closed when the Rc<Pool> drops (compio-postgres closes on Pool::drop). Per-race: 1 extra Postgres handshake + immediate close.

**Race frequency:** very low. The pre-await `cached_pool` check + post-await `install_pool` re-check leaves a window only when two tasks on the same compio worker BOTH miss the cache concurrently. Bounded by the once-per-worker-lifetime first-fill scenario. After warmup, races are zero.

**Indirect surface:** if a deployment's pg `max_connections` is sized tight, the race-loser handshake could intermittently bump conn count by 1-2 above expected. The r7-A `max_connections=500` cushion absorbs this; future tuning rounds (post-stress-greens) should preserve the cushion.

**Recommendation:** none — the cost is real but bounded; the code is correct. Document the loser-pool handshake cost in the rustdoc on db.rs:640. ~5 LOC.

**Priority:** MINOR — boundary-condition only.

---

### R27-M2 LATENT FIX — `utf8_char_len_at` Latin-1-cast bug in 6 sanitize sites

**File:Line:** `crates/sandbox/src/wake_machine.rs:983` (helper), `:1017, :1030, :1073, :1130, :1205, :1315` (6 call sites).

**Pre-fix:** every sanitize pass did `out.push(bytes[i] as char); i += 1;` on the non-match byte. For bytes ≥ 0x80, this cast the byte directly as a Unicode codepoint (Latin-1), corrupting multi-byte UTF-8 codepoints into Latin-1 supplement characters.

**Post-fix:** `let c_len = utf8_char_len_at(bytes, i); out.push_str(&msg[i..i + c_len]); i += c_len;`.

**Perf delta on error-path sanitize:**
- Pre-fix: 1 byte read + 1 char push per non-match byte (~4 µs for a 1KB error).
- Post-fix: 1 byte read + 1 byte-class branch + 1 slice copy of 1-4 bytes per non-match codepoint. For pure-ASCII (the common case), identical. For multi-byte glyphs, slightly faster than 4× single-byte pushes.

**Hot path?** Only on error paths (`wake_machine.rs:165`: `sanitize_error_message(message)` is called when wake_job state's `error_message` is non-empty). Wake success path skips sanitize entirely. Worst case: 4× per WAKE_FAILED + 4× per error read-back at the admin handler.

**Net delta:** correctness win + zero hot-path cost.

**CLOSED — fix already shipped at `821cc9bd`.**

---

### R27-M3 NEW MINOR — `BackendBuilder` adds zero perf cost vs telescoping constructors

**File:Line:** `crates/sandbox/src/backend/mod.rs:182-260` (R27-I1 builder at `df06d172`).

The `Backend::builder(&cfg).with_persist(p).with_local_nomad_node_id(id).build()` chain is a code-quality refactor; the `build()` method's match-on-`cfg.backend.as_str()` is unchanged. No allocation difference vs the prior 3-level cascade. Boot path (one call) is identical.

**CLOSED ZERO-COST.**

---

### R27-M4 NEW MINOR — `/metrics` endpoint allocates a 4KB+ String per scrape

**File:Line:** `crates/sandbox/src/metrics_export.rs:56-59` (render), `crates/sandbox/src/admin_handlers.rs:2055-2064` (route).

**Code shape:**
```rust
pub fn render() -> String {
    let mut out = String::with_capacity(4096);
    ...
}
```

Per scrape: `String::with_capacity(4096)` + ~16 `write_counter` calls each pushing ~150 bytes + `lost_leadership_by_op` map snapshot under Mutex. The Mutex hold is brief (the writer side increments under the same lock at metrics.rs:198).

**Hot path?** `/metrics` is admin-gated (`AdminRole::ReadOnly` at `admin_handlers.rs:2056`); Prometheus scrapers poll ~every 15s typical. At 4 conns/min, the 4KB alloc + render is immeasurable. The Mutex contention on `LOST_LEADERSHIP_BY_OP` (metrics.rs:57-58) is the only shared-state cost — under healthy clusters lost_leadership rate is ~0/sec, so contention is nil.

**Defense:** the precursor commit `3ec2762d` already comments "No allocation per atomic read; the only heap traffic is the single output `String`" — accurate.

**Recommendation:** none.

**CLOSED ZERO-COST.**

---

### R27-M5 NEW MINOR — `lost_leadership_value_for_op` returns 0 on Mutex `Err` instead of panicking — accurate trade

**File:Line:** `crates/sandbox/src/metrics.rs:399-403`.

```rust
pub fn lost_leadership_value_for_op(op: &'static str) -> u64 {
    let map = lost_leadership_by_op();
    let Ok(g) = map.lock() else { return 0 };
    g.get(op).map(|c| c.load(Ordering::Relaxed)).unwrap_or(0)
}
```

On a poisoned mutex (panic in a writer), this silently returns 0 instead of surfacing the error. For an observability counter accessor this is the right call — a panicking writer leaves the counter map in an inconsistent state, and the reader should return "no observation" rather than propagate poison.

Perf-wise: takes the Mutex on every call. Contention is nil (writer rate ≈ 0/sec at healthy). The exporter's `lost_leadership_snapshot_by_op` at metrics.rs:417 takes the same Mutex but returns the full snapshot in one lock — preferable for the exporter. Test paths use the per-op accessor.

**CLOSED ZERO-COST.**

---

### r1-DISC-3 test landing perf signal

**File:Line:** `crates/sandbox/tests/sandbox_pg_e2e.rs:5688-5944` (new `r26_c1_pool_cache` sub-module at `871752c7`).

**Test 1** (`pool_cache_returns_same_rc_within_thread`): zero perf signal. Asserts `Rc::ptr_eq(&p1, &p2)` after two `pool_app()` calls on the same compio worker. Pins the cache-hit predicate.

**Test 2** (`pool_cache_per_thread_isolated`): production-state oracle via `pg_stat_activity` filtered by test-unique `application_name`. Two `std::thread::spawn` workers each warm a pool; asserts >=2 distinct sandbox_app conns. **Demonstrates** the `controllers × threads × pool_max` retention math from R27-P2 — each thread holds its own Rc<Pool>.

**Test 3** (`pool_cache_dsn_tiebreaker_evicts_on_mismatch`): two Databases with DSNs differing only in `application_name`; same thread; second call's pool != first. Validates the eviction shape `set_role_dsns_for_test` depends on.

**Test 4 deferred** (housekeeper-reaps-idle): defensible. `PoolConfig` defaults `idle_timeout=600s` aren't reachable from the Database boundary, and a 10+ minute CI test is non-viable. The structural argument (compio-postgres housekeeper holds `Weak<Pool>`) plus the wiring inspection at db.rs:640/670 is the substitute.

**No perf-affecting code change.** Tests don't run by default (`#[ignore = "needs Postgres"]`). Net pure positive: closes a high-leverage predicate gap.

---

## Cross-lens consensus

- **Concurrency r27** (round-36): the thread-local Rc<Pool> wiring is sound under compio's single-threaded-per-worker model. `start_housekeeper`'s `Weak<Pool>` cleanly self-terminates race-loser pools (R27-P3 here).
- **Test-coverage r28** (round-35): R26-C1 cache predicate tests landed at `871752c7` close the only test-discipline-priority gap. R26-I2 spawn_blocking has no predicate test yet — the gap is "wrap is correct" not "wrap exists"; lower priority.
- **Architecture r28** (round-38): Option C Phase 2/4 staging-locality ADR architecturally bypasses the spawn_blocking branch on the cold-boot path under `driver_stages_disk_images=true`. The Phase 3 plan (delete the spawn_blocking branch) presumes Phase 4 cluster green; T-8b-stress-r8 RED at 5% e2e blocks that. Meanwhile both branches coexist and both are correct.
- **Security r-prior**: T5 fingerprint check is a security feature; the perf cost (R27-P1) is the trade. The 10s timeout is conservative for the "wait for partial-rollout to complete" semantic.
- **Code-quality r27**: R27-I1 BackendBuilder is purely an API-design change with zero perf delta.

---

## Net assessment

**Two big closes** — R26-C1 (thread-local pg pool) at `ee702d5f` + r7-C-followup `start_housekeeper` at `8c0b361e`, and R26-I2 (`try_create` spawn_blocking) at `73725aa3`. Both validated at cluster: T-8b-stress-r8 confirms pg state is no longer the wedge (275 idle / 500 cap, no FATAL).

**One new IMPORTANT** — T5 `/version` probe (R27-P1) inserts a serial signed-HTTP RTT into the WAKE hot path before clock_resync. Typical +0.1-0.4% wall add against the cluster's 45.6 s WAKE p50; worst-case 10s transport-timeout stall (still `Skipped`, not `Mismatch`). Parallelize-with-clock_resync option exists; defer until stress signal demands.

**Three new MINOR observations** — R27-P2 documents the `controllers × threads × pool_max` retention math (275 idle conns at cluster); R27-P3 documents the race-loser-pool 1-conn handshake cost; R27-M2 LATENT closes the bytes-as-Latin-1 sanitize bug at the same wall as before.

**Backlog stays open** — R16-P3 (Fuse encrypt + SHA), R16-P5 (gzip pre-AEAD), R17-P2 (active-set cache), R5-P1b (SHA + BufReader) all unchanged from r26. None are blocking stress-r8's restore-path wedge (which is a CH-side lock retention, not a controller-side perf issue).

**No new perf regressions introduced by r26→r27 landings.** The r7-A `max_connections=500` bump papers over the per-thread pool retention shape; r7-C-followup wires the housekeeper that should keep idle conns bounded post-warmup. R26-C1 + r7-C make the controller's pg surface stress-r5-clean — the remaining stress-r8 RED (3/60 e2e) is a CH/driver-side lock retention issue outside this crate's scope.
