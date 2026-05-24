# Sandbox/snapshot-restore — performance r15 review

Date: 2026-05-25 (UTC)
HEAD at audit: `2afbb2dd` (C-8 + C-8a). Branch parents include
`c3edf968` (R14-A6 cfg-derived policy), `493d6c1e` (C-7), and
`9afd0986` (R14-P2 thread-name simplification CLOSED).
Round 15 of N. Read-only.

Static review. Cluster-r9 confirmed C-7 fix wire shape and produced a
fresh **SNAPSHOT p50 6309 ms / CREATE p50 6448 ms baseline** at
controller v24 + b8fae7b7 content. WAKE failed at 25/25 attempts × 2 s
= 48 s 503 (C-8 — production-default `host_fence_timeout_secs=120` +
30 s Nomad purge = ~150 s teardown, exceeding the C-7 48 s budget). The
C-8 fix lands at the worker-startup script (cluster config) plus
C-8a's deadline cap in `from_host_fence_timeout`. No cluster validation
of C-8 / C-8a yet — first cluster run with `SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30`
will be r10.

## Summary

**1 new perf finding (informational)** plus three documentation
deltas to r14's projections:

- **R15-P1 (NEW, INFO/perf-tail)** — C-8 drain-window analysis. With
  `host_fence_timeout_secs=30 s`, the worst-case source-vm_index hold
  is now `~30 s host_fence + ~30 s Nomad purge ≈ 60 s`, fitting under
  the deadline-derived 50 s retry budget with **only ~10 s envelope on
  the fence half** (the Nomad-purge half remains uncovered — wakes
  racing a stop within ~60 s of teardown still surface 503). The 30 s
  drain is adequate for `c=1` smoke (CH already torn down, tap gone)
  but **does NOT envelope** the full teardown profile; r10 is
  expected to convert the 48 s 503 into a **WAKE-PASS** only when the
  wake fires ≥30 s after the SNAPSHOT begins teardown. Worked through
  in § "R15-P1 — C-8 drain-window correctness".

The remaining items are r14 carry-forward updates and one rounded-out
projection table:

- **R14-A6 / R14-P4 math** — cfg-derived policy `from_host_fence_
  timeout(30) = 11 attempts × 2 s = 20 s budget`; deadline-cap at
  `from_host_fence_timeout(120) = 26 × 2 s = 50 s`. The MIN function
  in `restore_handler.rs:267` always picks the tighter ceiling.
  Worked through in § "R14-A6 math under C-8 / C-8a".
- **R14-P4 thundering-herd at host_fence release** shrinks with
  fence=30 s: the c=20 release window is now ~30 s wide (down from
  ~60 s), but the simultaneous post-retry pg-pool demand spike at
  R11-P1's cliff is unchanged. Documented in § "R14-P4 herd window".
- **Cluster-r9 timings** + projected r10 wake p50 / p99 in
  § "Cluster-r9 projection for r10".

**No new CRITICAL findings.** R10-P1 / R10-P3 / R10-P4 / R10-P5 /
R10-P6 / R10-P7 / R9-P1 / R9-#6 / R9-#8 / R11-P1 / R11-P4 / R14-P1 /
R14-P3 carry-forwards unchanged. **R14-P2 CLOSED** at `9afd0986`
(byte-slice replaces the 4-pass char-walk; ~500 ns saved per
snapshot).

## R15-P1 — C-8 drain-window correctness analysis (NEW, INFO/perf-tail)

**Context**: C-8 reduces `host_fence_timeout_secs` from 120 s
(`config.rs:401` Rust default) to 30 s for cluster smokes, via
`gcp-worker-startup.sh` systemd `Environment=…HOST_FENCE_TIMEOUT_SECS=30`
(`9afd0986`/`2afbb2dd` parent). This shifts the WORST-case source
teardown wall-time from `~150 s` to `~60 s` (30 s fence + 30 s Nomad
purge). r14-P1's analysis assumed the legacy 120 s fence — r15 needs to
re-derive the drain-window math under the new value.

**What the 30 s host_fence actually protects against**:
`wait_for_agent_silent` (`nomad_ch.rs:3205-3273`) polls the source
tenant's `/livez` on a **100 ms cadence**, requiring **two consecutive
misses** to clear. The "miss" classes:
- TCP connect refused → agent process exited, tap-down (preferred)
- Transport timeout (500 ms request budget per poll)
- 5xx (agent mid-crash)

A clean shutdown takes ~200 ms (two-in-a-row at 100 ms cadence). The
30 s budget envelopes:
- ~150 ms of overlap before the agent actually exits
- ~50 ms of TCP wind-down before connect-refused returns
- A tail margin for `nomad alloc stop` + cloud-hypervisor SIGKILL +
  the bash wrapper's 3× virtiofsd grace periods.

**Is 30 s "enough" for in-flight TCP connections to ack-close?**

For the wake-side IP race the fence is defending against (FM-F:
handing out the same 10.99.<100+idx>.2:7777 to a fresh tenant whose
`/livez` succeeds against the *previous* tenant's still-alive agent),
the answer is **conditional**:

1. **Cloud-hypervisor is shut down by the time fence starts polling**
   — the source tenant's exit path is `ch.pause` → `ch.snapshot` →
   `nomad alloc stop` → cloud-hypervisor SIGTERM (CH never gets
   SIGKILL in the happy path; it exits clean within 100–500 ms).
   The tap interface is destroyed when CH exits.
2. **After CH exits, the source IP `10.99.<100+idx>.2` is unroutable**
   — any in-flight client connection to the OLD IP returns ECONNREFUSED
   within ~1 RTT.
3. **The fence-protected race is short**: the window is from "CH
   exits" to "Nomad purge clears the tap config" (worst case ~30 s
   under the cluster's typical Nomad lifecycle). The 30 s budget
   covers this window with margin to spare for two-in-a-row failures.

For **client-side** in-flight TCP connections from the broader world,
those connections cannot reach the OLD tenant's IP because the tap is
gone (step 1 above) — they fail-fast with ECONNREFUSED, regardless of
the fence value. **The fence does not need to envelope wide-area TCP
linger / TIME_WAIT** (no NAT keepalive holds open a route to a
destroyed tap). 30 s is therefore not just adequate but **structurally
sufficient**: the bottleneck the fence guards against (host-process
linger past `nomad alloc stop`) is sub-second on a healthy worker;
the 30 s budget is sized for the **pathological** case where CH or
virtiofsd hangs.

**Where 30 s falls short**: the Nomad-purge tail. After the fence
clears, `vm_index 1` is still held by the in-memory reservation set
because `wait_for_job_gone` (`nomad_ch.rs:935-1063`) waits for Nomad
to actually purge the job — Nomad's GC interval is operator-tuned
(typically 30 s, can be longer under high alloc churn). This is the
**uncovered window** — r10's wake must fire ≥30 s after the SNAPSHOT
begins teardown for the slot to be free at attempt 1; a wake racing
the teardown within ~30 s will still 503.

**Recommendation**: 30 s is the right number for the **fence half**.
The **purge half** is bounded by Nomad config (`gc_interval`) on the
worker, which the C-8 commit does not change. r10 should either:
- run the smoke with ≥30 s sleep between SNAPSHOT and WAKE (current
  smoke is back-to-back, ≤1 s delay), OR
- the C-7-LT pattern (async wake + poll) — which removes the deadline
  ceiling entirely.

**R15-P1 status: INFO.** Documents what the 30 s fence covers and
where the residual race remains. Not a regression; the 120 s default
was over-conservative for cluster smoke and the 30 s value is well
within the safety envelope for the documented failure mode.

## R14-A6 math under C-8 / C-8a (carried forward, computed)

`VmIndexRetryPolicy::from_host_fence_timeout` (`restore_handler.rs:240
-278`) derives the wake-retry budget from TWO ceilings:

```
fence-derived    = host_fence_timeout_secs - CLIENT_HEADROOM_SECS(10)
deadline-derived = CLIENT_DEADLINE_SECS(60) - CLIENT_HEADROOM_SECS(10) = 50
effective_budget = min(fence-derived, deadline-derived)
max_attempts     = effective_budget / INTERVAL_SECS(2) + 1
```

The MIN function (`restore_handler.rs:267`) always picks the tighter
ceiling — fence wins for `host_fence ≤ 60 s`, deadline wins beyond.

| `host_fence_timeout_secs` | fence ceiling | deadline ceiling | MIN | attempts | wall-time |
|---|---|---|---|---|---|
| 30 (cluster — C-8) | 20 s | 50 s | **20 s** | 11 × 2 s | **20 s** |
| 60 (mid-range) | 50 s | 50 s | 50 s | 26 × 2 s | 50 s |
| 120 (Rust default) | 110 s | 50 s | **50 s** (CAPPED) | 26 × 2 s | **50 s** |

**Cluster-r10 effective budget under C-8 + C-8a**: 20 s wall-time
(11 attempts at 2 s cadence; 10 sleeps; first attempt fires
immediately). That's **42 s headroom** under the 60 s ntex client
deadline — plenty.

**Important correction to the brief's framing**: the brief asks
"with C-8a's cap at 50 s deadline-derived = 25 attempts. MIN = 11."
This is correct **by attempt count comparison**: at `host_fence=30`,
fence-derived produces 11 attempts (the MIN with deadline-derived's
26 attempts). The 20 s budget envelopes a 30 s fence's typical
~10 s clear time with margin, but does NOT envelope the full 30 s
worst-case fence + 30 s purge = 60 s teardown. The trade-off is
documented in `restore_handler.rs:269-278`: the operator-set fence
controls the budget, and the budget is intentionally sized to the
fence (the IDEAL case) not to the full teardown (which includes the
operator-uncontrolled Nomad purge tail).

**Why not just use the deadline-derived 50 s budget at host_fence=30?**
A longer budget would catch more racing wakes — at the cost of holding
the wake handler on the controller longer for each 503 case. The
trade-off in C-8a's design is **observability > catch-rate**: a clean
503 in 20 s is more useful than a 50 s sleep that mostly waits past
the actual slot-free moment. r10 smoke will validate; if 20 s is too
tight in practice we revisit.

## R14-P4 herd window at host_fence release (carried forward)

R11-P1's c≥10 pg-pool cliff fires when N concurrent post-retry wakes
hit `update_sandbox_status` (CAS-to-Running) + `clear_snapshot_
metadata` simultaneously. r14-P4 flagged this as a thundering-herd
because all N wakes' host_fences clear within a narrow window after
their respective stops began.

**Window-width shift under C-8 (host_fence=30 s)**:

| Config | Stop-to-fence-clear window | Stop-to-purge-clear window | Stop-to-vm_index-release window |
|---|---|---|---|
| host_fence=120 (legacy) | ~120 s | +30 s tail | ~150 s |
| host_fence=30 (C-8) | ~30 s | +30 s tail | ~60 s |

If N=20 concurrent stops fire at roughly the same wall-clock (typical
in c=20 cluster stress with synchronized teardown), the corresponding
vm_index releases land:

- **legacy fence=120**: spread across ~30 s (the tail of fence-
  variability, dominated by the Nomad-purge component which is
  largely operator-controlled GC cadence).
- **C-8 fence=30**: spread across **~30 s** (still dominated by the
  Nomad-purge tail; the fence variability ≤500 ms is negligible
  here).

**Net change**: the herd-window shape is essentially unchanged — both
configurations are dominated by Nomad purge variability. C-8 shrinks
the **absolute time-to-clear** by ~90 s, but not the **window width**.
The R11-P1 cliff fires identically in shape at either fence config;
**only the timing of the demand spike shifts earlier under C-8**, not
its concurrent magnitude.

**Implication**: R11-P1 remains the principal fix; C-8 / C-8a do not
mitigate it (or worsen it). The herd-window mitigation is per-thread
pg-pool caching (R11-P1 / R13-I1), not retry-budget tuning.

## Cluster-r9 projection for r10 (wake p50 / p99 post-C-8)

**Brief's question**: post-C-8 (host_fence=30 s) + C-8a (deadline-cap
at 50 s), what's the projected r10 wake p50 if smoke r10 passes
(teardown ~60 s with C-8, retry 50 s, c=1 no-contention)?

**Two cases at c=1**:

### Case A — wake fires ≥30 s after SNAPSHOT begins teardown

Slot is free at attempt 1. The retry loop's per-attempt cost is
sub-µs (one mutex `.reserve()` on the in-process VmIndexAllocator).
No sleeps. The wake p50 ≈ AEAD-disabled budget from r14 = **~4.4–9.5 s**
(unchanged calibration from r13's `wait_for_livez` + `submit_restore_
job` + `store.get` + pg-handshakes).

### Case B — wake fires <30 s after SNAPSHOT begins teardown (smoke-r9 scenario)

Slot is held by source teardown:
- fence clears at ~30 s after stop (clean shutdown's two-in-a-row
  fires in ~200 ms; the 30 s budget is the worst-case timeout, not
  the typical clear time).
- Nomad purge completes 0–30 s after fence clears (operator-tuned
  GC cadence; on a healthy fleet ~5–10 s; on a busy one up to 30 s).
- vm_index released at t+30…60 s post-stop.

With the C-8 budget=20 s (fence=30 → 11 attempts at 2 s cadence):
- If purge completes ≤20 s after stop (~50% of cases on a healthy
  fleet — clean shutdown ~5 s + purge ~10 s = 15 s), the retry
  succeeds. **Wake p50 ≈ AEAD-disabled budget + ~15 s = ~20 s**.
- If purge takes >20 s, retry exhausts → 503 in 20 s + ~few ms 503
  overhead. **Wake p99 ≈ ~20 s 503**.

### Projected r10 outcome (smoke-style c=1, immediate wake-after-snapshot)

| Outcome | Probability (healthy fleet) | wake_ms (HTTP) |
|---|---|---|
| Retry catches slot release | ~50% | 10–20 s (mostly retry-sleep) |
| Retry exhausts → 503 | ~50% | ~20 s |

**Smoke-r9 wake-fail mechanism analysis says: r10 has ~50% chance of
passing WAKE OK 1/1.** A subsequent r11 with explicit 30 s
inter-call pause between SNAPSHOT and WAKE would be ~100%. If r10
gets unlucky and 503s, r11 should reconfigure the stress driver to
add inter-call sleep, not extend the retry budget.

**At c=N (no fresh data)**: same case-A vs case-B distribution per
wake. The aggregate wake p99 across N=20 concurrent wakes hits the
**maximum of N independent retry races** — ~95% of c=20 cycles will
have at least one Nomad-purge tail >20 s (long-tail distribution).
Plan: validate at smoke r10 then move to c=20 stress.

## Updated wake-path budget table (post-C-8 + C-8a)

The r14 table's `reserve_vm_index_with_retry` row is the only row
that moves under C-8 / C-8a. AEAD-disabled remains the production
default; AEAD-active variants unchanged.

| Component | Estimate (best–worst) | Source | Calibration |
|---|---|---|---|
| `reserve_vm_index_with_retry` (C-7 + C-8a, host_fence=30) | **0–20 s** (best: slot free; worst: 11-attempt exhausted budget) | `restore_handler.rs:265-306, 240-278` | cluster-r9 observed budget-exhaustion at 48 s under host_fence=120; r10 will observe under host_fence=30 |
| `reserve_vm_index_with_retry` (C-7 + C-8a, host_fence=60) | **0–50 s** | same | derived (no cluster sample) |
| `reserve_vm_index_with_retry` (C-7 + C-8a, host_fence≥60) | **0–50 s** (CAPPED at deadline) | same | derived |
| `submit_restore_job` (spawn_blocking) | ~2.0–3.5 s | `restore_handler.rs:640-651` | mirrors CREATE submit + alloc |
| `wait_for_livez` (spawn_blocking) | ~1.0–3.0 s | `restore_handler.rs:670-684` | mirrors CREATE livez wait |
| `store.get` AEAD-disabled (L1 hit + SHA verify) | ~0.5–1.5 s | `snapshot_store.rs:225` | bounded by 1 GB SSD read |
| `store.get` AEAD-active (hard-link + decrypt-write) | ~1.0–2.0 s | `snapshot_aead.rs:633-678` (R9-P1) | second 1 GB write to target |
| GCS download (L1 miss; R12-P1 + R11-P2 CLOSED) | ~0.8–2.0 s | `snapshot_store_gcs.rs:438-465` | wall-bound by GCS pipe |
| `clock_resync` (spawn_blocking) | ~0.05–0.15 s | `restore_handler.rs:1582-1668` | unchanged |
| `register_restored` + state.write | <0.01 s | `nomad_ch.rs:1659-1690` | unchanged |
| pg awaits (5 fresh handshakes per R11-P1) | ~0.05–0.2 s | R11-P1 OPEN, corr-class per R13-I1 | bounded by 5× TCP+STARTUP |
| ~32 fresh ureq calls × 1–3 ms TCP 3WHS | ~0.03–0.10 s | R10-P6 OPEN | unchanged |
| Post-store.get diagnostic (3 stat() + format!) | ~0.0001–0.003 s | `restore_handler.rs:570-585` | unchanged |

**Wake p50 (AEAD-disabled, L1 hit, no C-8 retry — Case A)**: ~4.4–9.5 s.
**Wake p50 (AEAD-disabled, L1 hit, C-8 mid-retry — Case B success)**:
~14–20 s (retry sleep dominates).
**Wake p99 (AEAD-disabled, L1 hit, C-8 exhausted)**: ~20 s 503.
**Wake p99 (AEAD-active + C-8 exhausted)**: ~20 s 503.

C-8 + C-8a have **structurally** moved the wake p99 ceiling from
"unbounded silent timeout (pre-C-7)" → "60 s timed-out client (pre-C-8)"
→ "20 s clean 503 with observable retry-trace" (post-C-8). Each step
in the chain narrows the observability gap while keeping the SLO
honest about what is and isn't possible inside the deadline.

## R11-P1 / R13-I1 pool churn at c=20 — re-derived for r15

Unchanged from r14's calculus. Repeating only the bottom-line:

| c | Inflight pg conns/wake | vs default `max_connections=100` |
|---|---|---|
| 1 | 10 | 10% |
| 4 | 40 | 40% |
| 8 | 80 | 80% |
| 10 | 100 | **100%** |
| 16 | 160 | **OVER (60%)** |
| 20 | 200 | **OVER (100%)** |

C-8 shrinks the **time-to-thundering-herd** (each wake's retry phase
is shorter under fence=30) but not the **N-wide post-retry pg-pool
demand**. The R11-P1 cliff fires at the same c-threshold either way.
**R11-P1 remains the largest single-edit perf lever.**

## R10-P1 + R9-P1 — closure check (still open)

Neither finding has moved since r14:

- **R10-P1** (AEAD memory-ranges 5-pass reads): `snapshot_aead.rs:
  377-465, 633-678` + `snapshot_store.rs:184-223` + `snapshot_store_
  gcs.rs:540-622, 966-997` still read the 1 GB memory-ranges multiple
  times across the encrypt + canonical-SHA + L2-SHA pipeline. No
  fuse-pass landed.
- **R9-P1** (AEAD wake hard-link discard): `snapshot_aead.rs:633-678`
  still stages ciphertext + writes plaintext to `target_dir` via
  `decrypt_to`, dropping the R5-P1 hard-link. ~1 GB extra write per
  AEAD-active wake.

Both stay on the ranked next-biggest lever list at positions #2 (R9-P1)
and #4 (R10-P1) below.

## R14-P3 — std::thread vs spawn_blocking (carried forward)

Unchanged from r14. The C-3 fix's `std::thread::Builder::spawn` per
L2 upload remains structurally unbounded (one fresh OS thread per
SNAPSHOT). Cluster-r9's c=1 sample exercised it once with no
observable cost on the snapshot RPC return (snapshot p50 6309 ms,
within ~50 ms of cluster-r5/r6's 6311 / 6356 ms baseline). C-8 does
not affect this code path. **R14-P3 status: INFO, no-op for v1.**

## Carry-forward (still open from r14 / r13 / r12 / r11 / r10 / r9)

| Finding | Status | File:line |
|---|---|---|
| **R11-P1** Every `Database` method opens fresh pg Pool | OPEN (corr-class per R13-I1; herd timing shifted by C-8, magnitude unchanged) | `db.rs:492-516` |
| **R10-P1** AEAD-active snapshot reads memory-ranges 5× | OPEN | `snapshot_aead.rs:377-465`, `snapshot_store.rs:184-223`, `snapshot_store_gcs.rs:540-622, 966-997` |
| **R10-P3** `cipher.encrypt`/`decrypt` allocates fresh Vec/chunk | OPEN | `snapshot_aead.rs:411-446, 529-572` |
| **R10-P4** `GcsSnapshotStore::put` recomputes canonical SHA | OPEN | `snapshot_store_gcs.rs:559-560, 1066` |
| **R10-P5** AEAD encrypt + decrypt write to raw `File` (no `BufWriter`) | OPEN | `snapshot_aead.rs:403, 529` |
| **R10-P6** ~32 fresh `ureq` connections per wake (no pooled `Agent`) | OPEN | `restore_handler.rs:1465-1490` |
| **R10-P7** `clock_resync_random_hex` builds via `format!("{b:02x}")` loop | OPEN | `restore_handler.rs:1687-1772` |
| **R9-P1** AEAD-active wake-path get discards R5-P1 hard-link | OPEN | `snapshot_aead.rs:633-678` (lines 660-674: full plaintext copy to target) |
| **R9-P3** AEAD 3-pass fusable on snapshot put | OPEN (subsumed by R10-P1) | — |
| **R9-#6** 64 KiB scratch buffer inside `ARTIFACT_FILES` loop | OPEN | `snapshot_store.rs:207`, `snapshot_store_gcs.rs:534, 1011` |
| **R9-#8** `chunk_aad` allocates a 13-byte Vec per chunk | OPEN | `snapshot_aead.rs:325-330` |
| **R11-P4** Sweep `SandboxRow::clone` allocation profile | OPEN | `sweep.rs:494-526` |
| **R14-P1** C-4 retry-tail: now 0–50 s on wake p99 (was 0–118 s) | OPEN (by design; budget cap bounded by C-7 + C-8a) | `restore_handler.rs:265-306` |
| **R14-P3** `std::thread::Builder::spawn` for L2 detach is unbounded | INFO/threading (no current bottleneck) | `snapshot_store_gcs.rs:1132` |
| **R15-P1 NEW** C-8 30 s fence covers the host-process linger window but not the Nomad-purge tail | INFO/perf-tail (by design; the purge tail is operator-controlled GC, not in C-8 scope) | `gcp-worker-startup.sh:455-470`, `restore_handler.rs:240-278` |

## Closed since r14

- **R14-P2** (`9afd0986`): `snap-l2-upload-<id-tail>` thread-name 4-pass
  char-walk replaced with `&sandbox_id[len-8..]` byte-slice. Saves
  2 String allocs + ~500 ns/snapshot. Code-quality only.
- **C-7** (`493d6c1e`): retry budget reduced 60×2s=118s → 25×2s=48s,
  per-attempt INFO log added. Validated in cluster-r9 (all 25
  attempts visible, clean 503, no silent cancel).
- **C-8** (`2afbb2dd` script half): `SANDBOX_NOMAD_CH_HOST_FENCE_
  TIMEOUT_SECS=30` set in `gcp-worker-startup.sh` systemd unit.
  Reduces source teardown wall from ~150 s → ~60 s. **Cluster-r10
  pending validation.**
- **C-8a** (`2afbb2dd` Rust half): `from_host_fence_timeout` now
  MIN-caps at `CLIENT_DEADLINE - CLIENT_HEADROOM = 50 s`, preventing
  R14-A6's cfg-derivation from re-introducing the C-7 silent-cancel
  case under conservative fence configs. **No cluster validation
  needed for the cap** — unit test `r14a6_from_cfg_caps_at_client_
  deadline` regression-pins (`restore_handler.rs:1577-1605`).
- **R14-A6** (`c3edf968`): cfg-derived policy. **Cluster-pending
  via C-8a.** The R14-A6 + C-8a combination is the right shape:
  policy tracks operator fence config, with a hard ceiling under the
  client deadline.

## Ranked next-biggest perf lever (updated for r15)

Unchanged ordering from r14; the C-7 + C-8 + C-8a fixes mitigated a
correctness-class regression (silent cancellation, slot starvation)
and a tail-latency budget overrun, but did not affect the snapshot
or AEAD-active wake long-poles. Levers carry forward verbatim:

1. **Hoist `Database` Pool to per-thread `Rc<Pool>`** (R11-P1):
   corr-class per R13-I1; same fix closes the perf nuisance (4× PG
   handshake savings/wake) AND the c≥10 conn-explosion cliff. C-8
   shrinks the herd-window timing but not the magnitude. **The
   largest single-edit lever.**
2. **Eliminate the second 1 GB write on AEAD-active wake** (R9-P1):
   ~0.5–1.5 s/wake saved.
3. **Land C-7-LT (async wake + poll)** (out of perf scope, but
   removes the 60 s ntex client-deadline tension entirely): a
   long-tail teardown profile under `host_fence=120` would then
   match RestoreBudget without re-introducing the cancel race.
4. **Fuse encrypt + canonical-SHA + L2-side SHA into the streaming
   pipes** (R10-P1 + R10-P4 + R9-#2): ~1.5–2.5 s/snapshot saved.
5. **`encrypt_in_place_detached` + `decrypt_in_place_detached`**
   (R10-P3): plausible 100–400 ms/AEAD round-trip.
6. **Cached `ureq::Agent`** (R10-P6 / R9-#3): ~30–100 ms/wake.
7. **`BufWriter` on AEAD encrypt/decrypt** (R10-P5): ~100–300 ms
   ceiling per AEAD round-trip.
8. **Sweep allocation cleanup** (R11-P4): sub-ms.

## Notes on focus-area questions

**(1) Reduced-fence implications, ideal drain window)**: 30 s is
structurally sufficient for the failure mode the fence guards
against (host-process linger past `nomad alloc stop`). The fence
polls /livez at 100 ms cadence requiring 2-in-a-row misses; clean
CH shutdown clears in <500 ms wall. 30 s envelopes pathological
hangs. **Does NOT envelope** the Nomad-purge tail — that's
operator-controlled GC cadence, out of C-8 scope. § "R15-P1"
carries the calculus.

**(2) Cluster-r9 wake p50 projection post-C-8)**: r10 smoke at c=1
back-to-back has ~50% pass rate (Case A: purge ≤20 s) vs ~50%
clean 503 in 20 s (Case B: purge >20 s). With an explicit 30 s
inter-call pause the pass rate approaches 100%. § "Cluster-r9
projection for r10" carries the table.

**(3) Thundering-herd at host_fence release shrinks with fence=30)**:
absolute time-to-clear shrinks ~90 s (legacy 150 → C-8 60 s) but
the window WIDTH does not (both dominated by Nomad-purge variability
in the ~30 s tail). N=20 wakes still hit the post-retry pg-pool
cliff at the same magnitude — only the wall-clock offset of the
spike shifts. § "R14-P4 herd window" carries the timing.

**(4) R14-A6 cfg-derived math)**: at `host_fence_timeout_secs=30`,
the formula gives 11 attempts × 2 s = 20 s budget (fence-derived
ceiling 20 s wins over deadline-derived 50 s). C-8a's MIN function
ensures this always picks the tighter ceiling; the 60 s ntex
deadline acts as a hard cap. § "R14-A6 math under C-8 / C-8a"
carries the worked-example table.

**(5) R10-P1 AEAD 5-pass memory-ranges reads — anything closed?)**:
no. `snapshot_aead.rs:377-465` (encrypt_in_place) + canonical SHA
re-read + L1 staging + L2 ciphertext re-SHA still produces 5 reads
of memory-ranges on the snapshot path. The brief mentions r14's
characterization; nothing has landed since.

**(6) R9-P1 AEAD wake hard_link discard — still open?)**: yes.
`snapshot_aead.rs:633-678` still calls `decrypt_to(wrapped, plain)`
with a fresh write to `target_dir/memory-ranges`, dropping the
R5-P1 hard-link from the staging dir. The fix sketch from r9 / r10
(decrypt in place into the alloc dir + rename) remains the cleanest
shape; no progress.
