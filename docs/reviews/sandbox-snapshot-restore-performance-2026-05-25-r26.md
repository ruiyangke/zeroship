# Sandbox/snapshot-restore — performance r26 review

Date: 2026-05-25 (UTC).
HEAD at audit: `01b6a744`.
Driver HEAD: `9ee26130` (driver v16 upload complete, T-8b-stress-r5 r4-A reap-wait).
Prior: `docs/reviews/sandbox-snapshot-restore-performance-2026-05-25-r25.md`.

Round 26 evaluates **three new landings since r25**, plus revisits the two CRITICAL carries (R25-C1 pool-per-call, R25-I1 try_create blocking) against the in-flight stress-r5 cluster.

1. `b5ec01a1` — `sandbox/wake-machine` deletes `WakeSnapshotMeta` + local `read_snapshot_row` (R26-I1 DRY collapse; +21/-56 LOC).
2. `1e8fa7e8` — `sandbox/scripts` bumps driver v15→v16 (T-8b-stress-r5 r4-A reap-wait pin).
3. **Driver-side**: `e7ce7f1f` adds `waitForReap` in `DestroyTask` — bounded poll on `h.exitDone` for **25 × 200ms = 5s** before declaring the task terminal. `9af429c7` adds `nomad_driver_ch_destroy_task_unreaped_total` counter.

---

## Summary

**7 findings, 2 CRITICAL (carry), 1 IMPORTANT (carry), 4 MINOR (3 closed-zero-cost, 1 grace-zero-impact).**

- **R26-M1 NEW MINOR** — r4-A `waitForReap`: typical ≤ sub-second; p99 capped at 5s wall; counter-bumped on exhaustion. Driver-side, does NOT consume controller ntex worker budget.
- **R26-M2 NEW MINOR ZERO-COST** — R26-I1 SnapshotRowMeta DRY collapse: ~10-byte extra String per wake row read. Confirmed negligible.
- **R26-M3 NEW MINOR** — r3-A 5s boot fetch defensibility: **Eager wins** for hot-path elimination + operator legibility.
- **R26-C1 / R25-C1 CRITICAL carry** — pool-per-call concentration. No pg-pooling refactor since r25. **Stress-r5 readiness gated on either landing R25-C1 OR pre-running with `max_connections=300+`.**
- **R26-I2 / R25-I1 IMPORTANT carry** — `try_create` sync block on ntex worker still open.
- **R26-M4 NEW MINOR — STOP ACK-vs-wall accounting** — 19ms p50 is CAS-lost / idempotent-on-missing early-return wall, NOT a full backend.stop. Path-split instrumentation needed.
- **R26-T1 NEW MINOR — lock-release timing contract** — WAKE p99 cost of single lock-conflict retry: ~76-91s wall (~46s + ~20s `--restore` fail + ~10-15s next CH boot).

---

## CRITICAL

### R26-C1 (R25-C1 / R23-P1 carry, sustained elevation) — pool-per-call still open at stress-r5 readiness gate

**File:Line:** `crates/sandbox/src/db.rs:543-549` (`open_pool` body); 34 call sites; ~14 opens per wake path; 2 opens per host_dir GC scan tick.

**Status @ r26:** NO REFACTOR LANDED since r25. `open_pool` still constructs a fresh `Pool::connect_with_config` per call.

**Stress-r5 cluster math:** WAKE rate climbs from 5% to potentially 80-100% post r4-A. **At c=20 burst × 14 conns × ~8ms = ~280 conns/sec sustained on the pinned controller's pg.**

**The 280-conns/sec projection is unchanged from r25** — but now the wedge that masked it is removed. This is the round where pool-per-call moves from "would be exposed at next green run" to "WILL be exposed during stress-r5 if it goes green at c=20".

**Recommendation:** land R25-C1 option 2 (`compio::sync::OnceCell<Pool>` per-thread) BEFORE the next stress sprint OR gate on pg-tuning workarounds (`max_connections=300+` + pgbouncer transaction-mode).

**Priority:** CRITICAL — sustained from r25. The r4-A fix unmasks this.

---

## IMPORTANT

### R26-I2 (R25-I1 / R23-A2 carry, sustained elevation) — `try_create` sync block on ntex worker still open

**File:Line:** `crates/sandbox/src/backend/nomad_ch.rs:670-768`; `crates/sandbox/src/handlers.rs:225`.

**Status @ r26:** No `spawn_blocking` wrap. `create_ext4_image_if_missing` cold-path is still `truncate -s 20G` + `mkfs.ext4` (~1-3s subprocess wall) executed inline on ntex worker.

**Stress-r5 math:** Under r3-A node-pin, all c=20 lands on the controller's ntex pool. At 4-8 workers default, pool saturates after 4-8 concurrent CREATEs; remaining queue behind ~3-5s sync block per cold-boot first-sandbox-per-user CREATE. p99 sibling-request add: ~9-15s.

**Recommendation:** wrap `try_create` body in `compio::runtime::spawn_blocking` at `nomad_ch.rs:670` (~30 LOC, low risk).

**Priority:** IMPORTANT, CRITICAL-adjacent.

---

## MINOR

### R26-M1 — r4-A driver `waitForReap` in DestroyTask: 5s p99 budget

**File:Line:** `nomad-driver-ch/ch/stop_task.go:77-79, 278-307, 406`.

| Reap latency | `waitForReap` wall | Comment |
|---|---|---|
| Already-reaped | ~1 µs | Common case after graceful SIGTERM |
| <200 ms | ~200 ms (one cycle) | Healthy SIGKILL → reap |
| Sub-second | 200-800 ms | Typical post-SIGKILL on loaded host |
| Pathological | 5 s + WARN + counter bump | r4-A unmasked failure — proceeds anyway |

**Where the 5s lives:** inside driver process, AFTER Nomad invokes driver-side termination. Controller's `state.backend.stop(id).await` returns BEFORE Nomad calls `DestroyTask` — controller-side wall bounded by `wait_for_job_gone` (30s) + `host_fence` (120s).

**Indirect win:** eliminates ~46-91s WAKE p99 retry on the wedged-reap path that drove stress-r4 RED.

**Is 5s defensible?** Shorter (1s) surfaces false-positive counter bumps; longer (30s) adds wall without unblocking anything (a 30s reap is symptomatic of a kernel bug). 5s wins on operator legibility.

**CLOSED** — driver-side, bounded, counter-instrumented, indirect win on next-WAKE wall.

### R26-M2 — R26-I1 DRY collapse: ~10-byte extra String per wake row read

`artifact_path` String holds snapshot artifact URI (80-150 chars). One alloc per wake row read = ~80-150 bytes heap + ~24 bytes stack.

Wake-path never accesses `snap.artifact_path` — silently ignored.

Against ~46s WAKE wall: **<<1e-9 ratio. Immeasurable.**

**The DRY win:** -35 LOC, eliminates known-drifted duplicate carried through 4+ review rounds.

**CLOSED ZERO-COST.**

### R26-M3 — r3-A 5s boot fetch defensibility

**Eager (current):** Boot wall add 5-50ms typical / 5s worst case; hot path zero; failure mode = boot WARN + None; counter at boot.

**Lazy alternative:** Boot wall 0ms; first CREATE/WAKE pays ~5-50ms (or 5s on unreachable agent); failure mode = user-facing 500s.

**Eager wins on:** user-facing latency, failure mode, operator alerting. Lazy wins only on boot wall under healthy-Nomad-agent — negligible (<1% of boot total).

**5s cap is defensible.** The concurrency-r26 finding (ntex workers start AFTER from_config returns) is a FEATURE: ensures no request lands on a controller with `local_nomad_node_id = None` mid-fetch.

**CLOSED — eager is the right shape.**

### R26-M4 — STOP ACK accounting: 19ms p50 is fast-path, not full-stop wall

**File:Line:** `crates/sandbox/src/handlers.rs:651, 658-665` (CAS-lost early-return); `:667-675` (full ladder); `crates/sandbox/src/backend/nomad_ch.rs:1051-1064` (idempotent-on-missing).

Two fast-paths dominate in stress-r4 (57/60 RED):
1. **CAS-lost** — pre-flight CAS lands on row not owned by this controller. ~5-15ms.
2. **Idempotent-on-missing** — state map has no entry. ~5-15ms.
3. **Full ladder** — agent /shutdown + stop_nomad_job + wait_for_job_gone (30s) + host_fence (120s). ~500ms-150s.

**Stress-r5 expectation:** p50 STOP wall **INCREASES** as wake success rate climbs (more sandboxes reach success path → more DELETEs hit full ladder). NOT a regression — path-mix shift.

**Recommendation:** stress harness should split STOP p50 by path (CAS-lost vs idempotent-on-missing vs full-ladder). ~10 LOC harness change.

### R26-T1 — WAKE p99 cost of lock-conflict retry: ~76-91s wall

Per retry: ~46s WAKE p50 + ~20s `--restore` `AlreadyLocked` fail + ~10-15s next CH boot.

Pre-r4-A: ~3/60 wakes hit this in stress-r4. At c=20 with 5% wedge incidence, p99 explodes — 1 in 20 wakes pays ~92s.

Post-r4-A: drives wedge incidence to ~0 for sub-5s reaps; counter `destroy_task_unreaped_total` flags the residual tail.

**Recommendation:** stress-r5 reporting must include `destroy_task_unreaped_total` as a per-run-end summary to correlate WAKE p99 against residual wedge incidence.

---

## Cross-lens consensus

- **Concurrency r26**: validated reap-wait runs in driver process, not controller's runtime. 5s is per-DestroyTask cap (no global serialisation).
- **Security r27**: `destroy_task_unreaped_total` is operator-facing only; WARN log has no user-controlled data.
- **Test-coverage r27**: R27-T1 destructive test needed to wedge reap and assert budget-exhaust + counter + cleanup.
- **Code-quality r26**: R26-I1 closure validated (R26-M2 zero hot-path delta).

---

## Carry table (key items)

| Finding | Win | Effort | Status @ r26 |
|---|---|---|---|
| **R26-C1** Per-thread pg pool | ~112 ms/wake + ~16 ms × N_subdirs/tick | ~150 LOC | **CRITICAL — gates stress-r5 readiness** |
| **R26-I2** `try_create` spawn_blocking | ~3-5 s/CREATE ntex block @ c=20 | ~30 LOC | **IMPORTANT — sustained** |
| R16-P3 Fuse encrypt + SHA | ~3 s/SNAPSHOT | ~80 LOC | TODO |
| R16-P5 gzip pre-AEAD | SNAPSHOT ~3.5 s + WAKE ~2 s + 95% storage | ~200 LOC | TODO |
| R17-P2 Active-set cache | ~300-500 ms/wake | ~60 LOC | TODO |
| **R26-M4** STOP path-split instrumentation | observability | ~10 LOC | **NEW** |
| **R26-T1** Surface `destroy_task_unreaped_total` | observability | ~5 LOC harness | **NEW** |
| ~~R26-I1~~ SnapshotRowMeta DRY | code quality | — | **CLOSED b5ec01a1** |
| ~~r4-A reap-wait~~ | eliminates ~46-91s WAKE p99 retry | — (driver v16) | **CLOSED** |

---

## Net assessment

Two landings advance stress readiness without changing controller-side perf math:
- **R26-I1 DRY collapse** — zero hot-path delta.
- **r4-A driver reap-wait** — closes dominant stress-r4 wedge. 5s p99 budget, sub-second typical, counter-instrumented.

**Two CRITICAL/IMPORTANT carries remain open.** r4-A UNMASKS R26-C1 by removing the wedge that prevented stress-r4 from reaching c=20 sustained-wake. **Stress-r5 readiness call: land R26-C1 BEFORE stress-r5 green run OR gate on `max_connections=300+` + pgbouncer transaction-mode.**

Two new observability gaps surfaced (R26-M4 STOP p50 path-split, R26-T1 counter surfacing). Stress-r5 reporting recommendations.

No new perf regressions introduced by r25→r26 landings.
