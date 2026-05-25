# Sandbox/snapshot-restore — concurrency r20 review

Date: 2026-05-25 (UTC).
HEAD at audit: `fde4f51c` (branch `feat/sandbox-snapshot-restore`;
prompt cited `dea68995` — two later landed commits, `6fbfafb3` and
`fde4f51c`, are pure test/error-message polish, no concurrency
delta).
Round 20 of N. READ-ONLY.
Scope: R19-C1-PR1/PR2 (`1d3724fe` + `8d163d58`), R19-I1 two-phase
probe (`82478a6b`), R19-I4 INSERT retry (`f2485210`).

Prior: `…concurrency-2026-05-25-r19.md`.

## Summary

- **9 findings** (1 CRITICAL, 3 IMPORTANT, 5 MINOR). R19-C1 LANDED in
  shape but a follow-on race opens between the takeover-sweep terminal
  write and the still-alive original `WakeMachine`'s terminal write
  (no CAS guard on `update_wake_job_state` against terminal columns).
  R19-I1 LANDED clean; phase 1 → phase 2 transition is sound.
  R19-I4 LANDED clean; the 3-attempt loop bound is correct.
- **R19-C1 closure**: structurally CLOSED in the wedge sense (stuck
  rows now get claimed). New race opened in the success-overlap path:
  `WakeMachine::set_state` and `drive` write `update_wake_job_state`
  without `WHERE state NOT IN ('ok','failed')` — see **R20-C1**.
- **R19-I1 closure**: CLOSED. Phase 1 TCP gate + Phase 2 ureq HTTP
  has correct phase-transition semantics; the only socket-close-race
  scenario (rare, agent crashes mid-handshake) materialises as an
  ECONNREFUSED → ureq Err in <1 ms, no wedge. PASS.
- **Multi-controller sweep contention**: PG row-locks serialize
  parallel `claim_orphan_wake_for_recovery` UPDATEs; non-terminal
  predicate inside WHERE re-evaluates on the second caller; second
  sees 0 rows. **PASS.**
- **`detach_isolated` proliferation**: 8 perpetual threads at process
  start (added `wake-takeover` for R19-C1-PR2) + up to ~`c` ephemeral
  `wake-…` threads at c=20 stress + bounded `create-rollbk`/
  `snap-teardown-…` threads. Memory cost real but acceptable
  (~200-400MB private runtimes at peak); see R20-M3.

## Per-prompt-question audit

### Q1 — R19-C1 takeover sweep races

**Q1.a Mid-sweep controller restart, half-updated pg row?** The SQL is
a SINGLE `UPDATE … WHERE … RETURNING wake_id` (db.rs:3329-3346). PG
guarantees atomicity at the row level; the controller crashing
between the wire write and the response observation just means we
don't *know* how many we claimed — but the row is fully committed or
fully untouched. **No half-updated row.** Safe.

**Q1.b Two-controller concurrent sweeps, row-lock resolution?** Both
controllers' sweep loops fire at the ~60 s cadence. They issue the
same `UPDATE … WHERE state NOT IN ('ok','failed') AND
lessee_updated_at < …`. PG acquires a row-level exclusive lock during
UPDATE; second waits; predicate re-evaluates after first commits
(`state` is now `'failed'`) and returns 0 matches. **No double-claim,
no spurious counter bump.** Safe.

The pg-gated test
`claim_orphan_wake_concurrent_claims_race_cleanly`
(`tests/sandbox_pg_e2e.rs:4844`) pins this exact contract — back-to-
back claims sum to exactly one match. PASS.

### Q2 — R19-I4 retry-loop × R19-C1 takeover interaction

**Q2.a Caller A inserts row X (pending) → controller crashes →
takeover sweep marks X failed → Caller A's POST returns 500?** A's
POST already returned 202 with X's wake_id; the controller crashed
mid-machine, not mid-handler. The poll endpoint on the dead wake_id
will now return state=failed/wake_worker_aborted (post-sweep) instead
of polling forever. **This is the intended R19-C1 fix and works.**

**Q2.b Caller B POSTs after sweep claimed X.**
`find_pending_wake_for_sandbox` filters non-terminal (db.rs:3232-
3236); the swept terminal row is invisible → fresh INSERT attempted.
Sandbox row likely in `Restoring` (CAS happened before the crash) →
handler's 409 state_mismatch fires (admin_handlers.rs:1614-1631). B
gets 409 until the transient-state takeover sweep
(`spawn_transient_state_takeover`) rolls the sandbox back to
`Snapshotted`. Two-sweep coordination — there's no deadlock, just
window during which retries 409. Acceptable but worth tracing
(see **R20-M1**).

**Q2.c A retries during the takeover sweep run?** A's INSERT shares
the same `wake_jobs_sandbox_pending_uniq` UNIQUE INDEX. While the
sweep's UPDATE row-lock is held, A's INSERT sees no non-terminal row
(sweep hasn't committed) OR a non-terminal row (sweep mid-flight, row
not yet visible) — either way the partial UNIQUE INDEX serialises
cleanly. **No write-write conflict.** R19-I4's 3-attempt loop handles
the sub-ms terminal-during-race window (db.rs:3057-3119). Safe.

### Q3 — R19-I1 phase 1 → phase 2 transition

Phase 1: `probe_agent_reachable_tcp(probe_addr, 150ms)` — compio TCP
connect + drop. ACK at time T means agent's listener accepted SYN+ACK
at T. Phase 2: ureq.call(/livez) opens a *separate* TCP socket at
T+Δ (a few μs after spawn_blocking schedule).

**Edge case — agent closes listener between Phase 1 ACK and Phase 2
connect**: Phase 2's connect gets RST immediately → ureq returns Err
in <1 ms → loop sleeps 150 ms cadence → next iteration's Phase 1
detects loss. **No wedge.** PASS.

**Edge case — HTTP layer wedged after TCP up**: Phase 2's ureq timeout
is 500 ms request-deadline. In the worst case the loop burns 500 ms
per iteration on this site, vs. the 30 s outer
`agent_livez_timeout_secs` budget = 60 iterations max. Loop cadence
becomes irregular but BOUNDED. PASS per design.

**Verified by** `wait_for_agent_livez_socket_never_accepts_returns_timeout_clean`
+ `wait_for_agent_livez_socket_accepts_late_succeeds_within_budget`
(nomad_ch.rs:5095, 5138). Tests pin both Phase 1 and Phase 2 wedge
shapes.

### Q4 — Carry-forward verification

| Finding | r19 status | r20 status |
|---|---|---|
| R19-C1 (stale wake_jobs + GATE-C2 wedge) | OPEN | **CLOSED — wedge** (`1d3724fe` + `8d163d58`); **new R20-C1 opened on the success-overlap path** |
| R19-I1 (livez startup ureq wedge) | OPEN | **CLOSED** (`82478a6b`) |
| R19-I2 (defense-in-depth post-GATE-C2) | CARRY | CARRY (still structurally unreachable, R4-A2 dissolves long-term) |
| R19-I3 (register_restored coverage) | CARRY | CARRY (test-coverage lens) |
| R19-I4 (insert_wake_job terminal race → 500) | OPEN | **CLOSED** (`f2485210`); 3-attempt loop semantics correct |
| R19-M1/M2/M3/M4 | various | unchanged from r19 |

**R19-C3 — not in r19.** r19's status block lists only R19-C1 as a
critical; "C3" appears to be a prompt typo. No deferred C3.

### Q5 — PR1 `detach_isolated` proliferation at c=20

Call sites enumerated from grep:

Static (process-lifetime threads, spawned once at start):
1. `snap-health` (lib.rs:1176)
2. `snap-heartbeat` (lib.rs:1263)
3. `snap-takeover` (lib.rs:1480)
4. `snap-transient` (sweep.rs:256)
5. `wake-gc` (sweep.rs:335)
6. `wake-takeover` (sweep.rs:440) — **NEW R19-C1-PR2**
7. `snap-idle-evict` (sweep.rs:779)
8. `snap-idle-gc` (registry.rs:836)

Per-event (bounded by traffic):
- `wake-{tail}` (admin_handlers.rs:1712) — 1 per wake POST
- `create-rollbk` (nomad_ch.rs:2022) — 1 per failed create
- `snap-l2-upload-{id}` (snapshot_store_gcs.rs:1143) — 1 per L2 push
- `snap-teardown-{id}` (admin_handlers.rs:1354) — 1 per teardown

At c=20 stress (steady state): 8 perpetual + up to ~20 wake-driving +
~20 create-rollback (worst case) + a handful of L2 uploads = ~50
concurrent OS threads, each with a private compio runtime. Per-runtime
memory overhead is non-trivial (io_uring ring + queue ≈ a few MB).
Total ~200-400 MB just for runtime overhead at peak. **Acceptable for
correctness, observable for capacity planning.** See **R20-M3**.

## Findings

### [R20-C1] WakeMachine terminal write clobbers takeover-sweep terminal write

- **Files**: `wake_machine.rs:128-144` (`drive` terminal=ok),
  `wake_machine.rs:161-177` (`drive` terminal=failed),
  `wake_machine.rs:487-500` (`set_state` intermediate),
  `db.rs:3167-3207` (`update_wake_job_state` — **no `WHERE state…`
  CAS guard**).
- **Shape**: takeover sweep at time T transitions wake_id W's row to
  `(state=failed, error_code=wake_worker_aborted, error_message='wake
  worker aborted: controller did not complete the wake within the
  timeout (see operator runbook)', lessee_updated_at=T, updated_at=T)`.
  The ORIGINAL WakeMachine — still alive (the sweep's "lessee
  abandoned" inference is based on `lessee_updated_at` staleness, not
  a heartbeat) — eventually progresses past whatever phase blocked
  the heartbeat (e.g., `store.get` on a slow GCS) and calls
  `update_wake_job_state(Ok, None, None, Some(agent_url))`. The SQL
  has no `WHERE state NOT IN ('ok','failed')` guard; the row writes
  through. **Three corruption paths**:
  1. terminal flip: `(failed, wake_worker_aborted, …)` → `(ok,
     wake_worker_aborted, …, ready_at=now())`. `error_code` is
     COALESCE-preserved (None param keeps existing); the row now
     reports a successful wake WITH a non-null
     `wake_worker_aborted` error_code and a sticky
     "wake worker aborted" error_message — contradictory to dashboards
     that join state + error_code.
  2. inverse flip: original WakeMachine fails at a late phase, writes
     `(failed, RestoreFailed, "real error…")`, overwriting the
     sweep's `wake_worker_aborted` breadcrumb — the operator audit
     trail loses the "controller crash" lineage.
  3. intermediate write: the original WakeMachine's `set_state`
     (best-effort, swallows errors) flips the row from `failed` →
     `LivezPolling` (or similar) — **resurrecting a terminal row to
     non-terminal**. This re-triggers the GATE-C2 UNIQUE INDEX block
     for any subsequent POST until the sweep re-claims it (60 s
     later). Sandbox wedged for the round-trip.
- **Trigger probability**: any wake whose single-phase wall time
  exceeds the takeover threshold (`takeover_threshold_secs` default
  60 s; floor 30 s). The only phase that legitimately exceeds 30 s
  is `Restoring` (sync `store.get` on multi-GB GCS blob + sync
  `submit_restore_job`); a slow GCS or large snapshot puts it within
  reach of the 60 s default. With `agent_livez_timeout_secs` set high
  by an operator, `LivezPolling` is also exposed.
- **Why concurrency-lens**: the wedge half of R19-C1 is closed, but
  the OVERLAP half (sweep + machine both alive) introduces a row-
  level write-write race with no CAS guard.
- **Action**: add `AND state NOT IN ('ok','failed')` to the
  `update_wake_job_state` UPDATE's WHERE clause. Callers already
  treat a 0-row result tolerantly (`set_state` swallows; `drive` logs
  + continues). One additional invariant: after sweep claims, the
  original machine's writes become structurally no-ops; the row stays
  authoritative as the sweep's terminal record. **Also** for `drive`'s
  terminal=ok path, explicitly null `error_code`/`error_message` via
  a dedicated wire kind (a `clear_wake_error: bool` flag, or pass
  empty-string sentinels and let the SET clause overwrite). Otherwise
  even with the CAS guard in place, a non-overlap successful wake
  retains any prior intermediate error message in the row, which is
  the same shape r17-I2 documented.

### [R20-I1] `wake-takeover` static thread name missing from kernel-limit enumeration

- **File**: `detach.rs:234-250`
  (`all_known_thread_names_fit_kernel_limit::STATIC_NAMES`).
- **Shape**: R19-C1-PR2 added `crate::detach::detach_isolated("wake-
  takeover", …)` at `sweep.rs:440` but did not extend the static name
  allowlist in the kernel-limit enumeration test. The name is 13
  bytes so it fits the 15-byte `pr_set_name` limit — the test
  PASSES — but the enumeration doc-comment explicitly demands every
  new site be added (sweep.rs:225-230 "Enumeration is by-hand: grep
  for `detach_isolated\(` across `crates/sandbox/src/`, extract the
  literal / format-prefix, and add it here").
- **Why IMPORTANT not MINOR**: the missing entry breaks the
  "code-review catches anything not in the list" contract the test
  was designed around. The next site that DOESN'T fit the kernel
  limit could land silently if the team accepts an incomplete
  allowlist as the norm.
- **Action**: add `"wake-takeover", // sweep.rs::spawn_wake_jobs_
  takeover` to the `STATIC_NAMES` const.

### [R20-I2] `lessee` field not consulted by takeover sweep — global, not host-scoped

- **Files**: `db.rs:3317-3348` (`claim_orphan_wake_for_recovery`),
  `db.rs:3022` (`insert_wake_job` writes lessee from
  `db.host_id()`).
- **Shape**: the sweep's UPDATE has no `AND lessee = $host` clause;
  every controller's sweep claims abandoned rows owned by ANY
  controller. Combined with R20-C1, this means a slow-but-alive
  controller C1's wake row can be claimed by C2's sweep at the 60 s
  threshold even though C1 IS making progress (just hasn't bumped
  `lessee_updated_at` because no phase boundary fired in the
  threshold window). The intended design from R17-A1 was
  cross-controller (so a crashed controller's rows still get cleaned),
  but the absence of `lessee`-based heuristics — e.g., only claim
  rows whose lessee no longer appears in a live-controller registry —
  means the threshold is the sole defense.
- **Why IMPORTANT not CRITICAL**: combined with R20-C1's fix, the
  worst case is a no-op for the sweep (machine's writes become CAS
  no-ops). Without R20-C1's fix, this race is materially worse
  because cross-controller claims are more likely than same-
  controller claims.
- **Action** (longer-term): consider a controller-liveness table
  (`controller_heartbeats(host_id, last_seen_at)`) gated on a longer
  threshold; the sweep then claims only rows whose lessee is
  demonstrably gone. Defer; R20-C1's CAS guard is the immediate fix.

### [R20-I3] `update_wake_job_state` writes during `Restoring` phase have NO `lessee_updated_at` bump frequency

- **Files**: `wake_machine.rs:273` (`set_state(Restoring)`),
  `wake_machine.rs:303-358` (sync `store.get` + `submit_restore_job`
  inside Restoring).
- **Shape**: `set_state(Restoring)` fires once at line 273. The
  multi-second `store.get` (1 GB sync GCS read + AEAD decrypt) and
  `submit_restore_job` execute INSIDE the Restoring phase WITHOUT
  intermediate state writes, so `lessee_updated_at` stays at
  Restoring-entry time until LivezPolling fires. On a slow GCS or
  multi-GB snapshot, this single phase can exceed 60 s — exposing the
  wake to a takeover claim. Same shape for `agent_livez_timeout_secs`
  if operator sets > 60 s.
- **Why IMPORTANT not CRITICAL**: R20-C1's CAS guard fix makes the
  sweep claim a structural no-op for the running machine's later
  writes. But the wake's pg-visible row still shows `failed/wake_
  worker_aborted` for the rest of its true work — confusing for
  operators watching dashboards and a real UX trap for polling
  clients (they see `failed`, give up, then the machine actually
  succeeds and CAS updates the sandbox to Running while the
  wake_jobs row stays failed forever).
- **Action**: inside `store.get` and `submit_restore_job` blocking
  hops, periodically fire `set_state(Restoring)` via a watchdog
  task — even a 30 s tick is enough to keep `lessee_updated_at`
  current. Alternative: bump `takeover_threshold_secs` default to
  120 s (2× the longest single-phase wall time). The
  `MIN_TAKEOVER_THRESHOLD_SECS = 30` floor is also too aggressive —
  raise to 60 s minimum so an operator can't trip in-flight wakes
  by mis-tuning.

## MINOR

### [R20-M1] Caller B's "fresh INSERT then 409 state_mismatch" window

- **Files**: `admin_handlers.rs:1614-1631` (state_mismatch 409),
  `sweep.rs:251-282` (`spawn_transient_state_takeover`).
- **Shape**: after R19-C1 sweep claims wake_id W, the SANDBOX row
  is still in `Restoring` (the controller crashed mid-flight, never
  rolled the row back). The transient-takeover sweep runs on its
  own cadence (`SANDBOX_TRANSIENT_STATE_TIMEOUT_SECS` default per
  enum) and eventually CAS-rolls back to `Snapshotted`. During the
  window between wake-jobs sweep claim and transient sweep rollback,
  any retry POST returns 409. Two-sweep coordination is fine, just
  surface the relationship in tracing.
- **Action**: extend the takeover-sweep `tracing::warn!` to mention
  "sandbox row may still be in Restoring; transient-state takeover
  sweep will roll it back on its next cadence". Or: in the wake-jobs
  takeover SQL, also CAS-rollback the sandbox row (more invasive;
  defer).

### [R20-M2] `insert_wake_job` retry loop holds the pg client across `find_pending_wake_for_sandbox` round-trip

- **File**: `db.rs:3059-3119`.
- **Shape**: the retry loop reuses a single `pool.get()` client
  across all 3 attempts AND across the
  `find_pending_wake_for_sandbox` call between attempts. The latter
  opens a separate `pool.get()` (db.rs:3215). With pool size
  saturated under load, this could starve siblings. Holding one
  client for 3× round-trips × 2 queries each = up to 6 sequential
  pool transitions per call.
- **Why MINOR**: the loop bound (3) is hard and rapid (sub-ms race
  window); contention impact is negligible at realistic pool sizes
  (default 16+).
- **Action**: optional — pass the `client` reference to
  `find_pending_wake_for_sandbox` so the retry path uses a single
  pinned connection.

### [R20-M3] `detach_isolated` private-runtime memory at c=20 peak

- **Files**: enumerated in Q5 above; 8 perpetual + ~`c` ephemeral.
- **Shape**: each `detach_isolated` mints a NEW compio runtime on a
  fresh OS thread. Per-runtime overhead is io_uring queues + worker
  pool ≈ 2-4 MB. At c=20 with wake-storm: ~50 concurrent threads ×
  3 MB = ~150 MB private runtime overhead. Plus per-thread stack
  (default 8 MB). Total per-controller process footprint at peak:
  ~500 MB just from `detach_isolated` machinery.
- **Why MINOR**: doesn't break correctness; well within typical
  controller VM (32 GB+). But the proliferation is a one-way ratchet;
  every NEW concurrency fix that adds an `detach_isolated` site has
  the same multiplier under stress.
- **Action**: lazily measure RSS at c=20 (zerobench could grow a
  "controller process memory at wake-storm peak" metric). Defer
  until measured; R20-I3's watchdog ticks could partially live on
  a SHARED runtime if memory pressure surfaces.

### [R20-M4] R19-M4 SealedAuth Zeroize cross-await — CARRY

- **File**: `wake_machine.rs:410`. Activates only when `SealedAuth`
  impls ZeroizeOnDrop. Tracking pin only.

### [R20-M5] R19-M2 sanitizer hostname-trail — CARRY

- **File**: `wake_machine.rs:717-735`. Security-lens; r19 deferred.

## Cross-lens consensus

- **R20-C1** is the dominant remaining race surface. R19-C1 fixed
  the WEDGE half but the OVERLAP half (sweep claims row while
  machine is still alive) opened a row-level write-write race that
  `update_wake_job_state` does not guard against. One-line SQL
  predicate fix.
- **R20-I3** crosses correctness + observability: even with R20-C1's
  fix, an operator watching the wake_jobs table sees confusing
  `failed/wake_worker_aborted` rows for *successful* wakes whose
  single-phase wall time exceeded the takeover threshold.
- **R20-I2** is largely defense-in-depth post-R20-C1, but the
  cross-controller claim semantics deserve an arch-r20 note.
- **R20-I1** is a hygiene gap; flagged for the next code-quality
  pass.

## Lens hand-off

- **Architecture r20**: R20-I2 (cross-controller claim semantics —
  is the global sweep design right, or should claims be
  host-scoped via a heartbeat table?). R20-I3 sweep threshold
  vs. phase wall-time relationship.
- **Test-coverage r20**: R20-C1 — controller-still-alive +
  takeover-claims-mid-flight test. Drive a stub `WakeMachine` whose
  Restoring phase blocks 65 s while the sweep fires at 60 s; verify
  terminal row ends in the sweep's state, not the machine's.
- **Code-quality r20**: R20-I1 enumeration miss (one-line fix).
- **Security r20**: R20-M5 carry.

## Status block

```
Round 20 (R19-C1 + R19-I1 + R19-I4 LANDED):
  CLOSED:
    R19-C1 wedge half (1d3724fe + 8d163d58 — claim_orphan_wake +
      spawn_wake_jobs_takeover),
    R19-I1 (82478a6b — wait_for_agent_livez two-phase),
    R19-I4 (f2485210 — insert_wake_job 3-attempt retry).
  CARRY:
    R19-I2 → R20 (defense-in-depth post-GATE-C2, structurally
      unreachable; R4-A2 dissolves long-term),
    R19-I3 → R20 (register_restored coverage; test-coverage lens),
    R19-M2 → R20-M5 (sanitizer hostname-trail; security),
    R19-M4 → R20-M4 (Zeroize cross-await).
  NEW:
    R20-C1 (sweep terminal write clobbered by alive WakeMachine —
      no CAS guard on update_wake_job_state — CRITICAL),
    R20-I1 (wake-takeover missing from detach.rs enumeration),
    R20-I2 (sweep is host-global, no lessee/host scoping),
    R20-I3 (Restoring phase wall-time can exceed takeover threshold;
      LivezPolling too with operator mis-tuning),
    R20-M1 (caller-B 409 window between wake-sweep and transient-
      sweep),
    R20-M2 (insert_wake_job pg client hold across retry loop),
    R20-M3 (detach_isolated private-runtime memory at c=20 peak).

  ASK: (1) r21 GATE-C4 add `AND state NOT IN ('ok','failed')` to
       `update_wake_job_state` WHERE clause (R20-C1) — escalated;
       (2) r21 GATE-I3 watchdog inside Restoring phase OR raise
       MIN_TAKEOVER_THRESHOLD_SECS (R20-I3);
       (3) r21 hygiene PR for R20-I1 detach.rs allowlist;
       (4) decide R20-I2 host-scoped sweep design (arch-r20 lens).
```
