# Sandbox/snapshot-restore — performance r33 review

Date: 2026-05-25 (UTC). HEAD: `05eced23`.
Driver pin: v25. Controller: v40 (per r32-T1 trace artifact).
Prior: `docs/reviews/sandbox-snapshot-restore-performance-2026-05-25-r32.md`.

Round 33. Code-side commits since r32 baseline:

- `2faaf39b` parallelise cold-boot `mkfs.ext4` (R32-P1 Knob A, CLOSED).
- `d5d4d532` thread `sandbox_id` through `wait_for_alloc_running` trace
  emits (observability; no perf delta).

No fresh cluster run was dispatched between r32 and r33; the R32-P1
`~1–2 s CREATE saving` is **unvalidated on a cluster**. The
controller-side analysis below holds independently of that
measurement, but R33-V1 (below) gates the next perf-pin bump.

---

## Summary

**1 new IMPORTANT (R33-P1), 1 new MINOR (R33-P2), 1 validation hold
(R33-V1). R32-P1 CLOSED. R32-P2 / R32-P3 / R30-P1 carry unchanged.**

- **R33-V1 NEW IMPORTANT (validation hold)** — R32-P1 (`2faaf39b`)
  expects ~1.0–1.5 s p50 win on c=1 cold-boot CREATE. No cluster
  driven since `7c44cc78` (v25 r1 baseline, 8.9 s). Fix committed,
  effect **unmeasured**. Queue an r32-T1-style 3-cycle trace
  against the next pin (≥ v41).

- **R33-P1 NEW IMPORTANT — In-VM boot 5–8 s dominates CREATE
  (77–81 % of wall per r32-T1) and is the next perf opportunity, but
  it's out of THIS crate.** Controller-side trims exhausted past
  R32-P1. The in-tree paths that can move it (kernel cmdline
  `console=ttyS0` → `console=null`; systemd → direct `init.sh` exec
  from initramfs) live in `crates/sandbox/scripts/bake-rootfs.sh`
  and the nomad-driver-ch worktree — outside the brief's
  `crates/sandbox/src/**` scope guard.

- **R33-P2 NEW MINOR** — `wait_for_alloc_running` 100 ms cadence
  vs r32-T1 measured 717/713/915 ms `alloc → running` window. Drop
  to 50 ms saves ~25–50 ms p50; bounded by the same noise floor
  that hid the r32 cadence quick-win.

- **R32-P1 RESOLVED** — `2faaf39b`. `std::thread::scope` × 2;
  ~28 LOC. Lib 551/551. Cluster measurement deferred (R33-V1).

- **R32-P2 carry MINOR** — three sync FS ops in `do_restore_inner`
  + equivalent in `wake_machine::drive`. Unchanged.

- **R32-P3 carry** — async-restore-ack remains shipped default;
  sync mode vestigial. Wake-path user-facing latency already
  decoupled from controller wall.

- **R30-P1 / R29-P2 / R29-P3 / R30-P2 / R30-P3** — unchanged.

- **A3 / R5-P1 sync-I/O** — no new offenders. `store.get`,
  `submit_restore_job`, and `http_*_unsigned` are all already on
  `spawn_blocking`.

---

## CRITICAL

None.

---

## IMPORTANT

### R33-V1 NEW IMPORTANT — R32-P1 perf delta UNMEASURED on cluster

`2faaf39b` landed (~28 LOC, lib 551/551 clean) targeting ~1.0–1.5 s
p50 on c=1 cold-boot first-sandbox-per-user. The v25 r32-T1 pin
(controller v40 at `1d58ab53`) **predates** the change.

**If validation fails** (≤ 0.8 s p50 move), two hypotheses become
live: (a) `home.img` mkfs is shorter than estimated (≤ 0.5 s) so the
joint wall was always waiting on `workspace.img`; (b) fsync_dir
contention on the shared parent (`user_home_dir_root`) serialises the
two children inside the kernel even though leaf paths are disjoint
(parents are `host_dir` vs `user_home_dir_root/<user_id>` — distinct,
so this is weak but not impossible).

**Action**: bump controller pin to include `2faaf39b`, re-run
r32-T1's 3-cycle c=1 trace, attribute via the existing `submit_done`
emit minus `create_started`. ~$0.10, ~10 min.

### R33-P1 NEW IMPORTANT — In-VM boot dominates CREATE (5–8 s); next leverage point is out of this crate's scope

**Source**: r32-T1 trace §"Finding 3". `alloc running → agent_ready`:
C1 4831 ms (77 %), C2 6969 ms (81 %), C3 8065 ms (81 %).

The slice is `(CH spawn → kernel boot → systemd → init.sh →
sandbox-agent binds 7777 → controller livez 200)`. Controller code
in `crates/sandbox/src/` ends at `http_post_json_unsigned` for the
agent /livez probe; the budget inside the VM is owned by:

- **Kernel cmdline** (assembled by nomad-driver-ch `start_task.go`):
  `console=ttyS0` adds ~200–500 ms; `quiet` already set. Switching
  to `console=null` for production allocs would trim it. Out of
  scope here.
- **systemd** in the rootfs (built by
  `crates/sandbox/scripts/bake-rootfs.sh`): 0.5–1 s of early-boot;
  direct `init.sh` exec from initramfs is a 1–2 s win and a multi-day
  refactor. Scripts dir is in tree but outside `src/**`.
- **CH spawn**: ~0.5–1 s of vCPU+memory setup. CH-internal.

**No in-tree controller-side change can trim the in-VM boot slice.**
r32-T1's "5–8 s → 3–5 s if we trim kernel boot + systemd" lands in
two other worktrees. Surfaced for backlog visibility; r32-T3 already
captures the design-level workstream.

### R30-P1 carry IMPORTANT — permit-hold documentation gap

Unchanged. `nomad_ch.rs:1465-1468`. ~20 LOC docstring.

---

## MINOR

### R33-P2 NEW MINOR — `wait_for_alloc_running` 100 ms poll cadence vs ~700 ms `alloc → running` window

**File:Line**: `crates/sandbox/src/backend/nomad_ch.rs:3235`.

r32-T1 measured `alloc_first_seen → alloc running` as 613–815 ms
across 3 cycles. Current happy-path cadence is 100 ms; trailing-edge
slack up to 100 ms (p50 ~50 ms).

**Knob**: drop to 50 ms (matches the `wait_for_agent_livez` cadence
after `a6e517b2`). ≤ 5 LOC, ~25–50 ms p50 saving.

**Why MINOR**: the r32 cadence quick-win was unmeasurable in c=1
N=3 noise floor; same shape of saving here. Don't go to 25 ms
(r32-T1 §"Finding 4").

### R32-P2 carry MINOR — Three sync FS ops on compio worker (do_restore_inner) + wake_machine equivalent

**File:Line**: `crates/sandbox/src/restore_handler.rs:870-882, 985`
and `crates/sandbox/src/wake_machine.rs:321-347, 379-388`.

Unchanged from r32. The wake_machine path runs inside
`detach_isolated`'s dedicated thread+runtime
(`detach.rs:76-110`), not the shared ntex worker — so sync FS ops
only stall one isolated thread per wake. Back-pressure is bounded
by `c × ~3 ops × ~0.5 ms` ≈ 30 ms at c=20, distributed across c
threads. Lower priority than r32's framing implied; still trivial
to fold into the existing spawn_blocking. ~15 LOC.

### R32-P3 carry — Async-restore-ack already shipped; sync mode vestigial

Unchanged. `wake_machine.rs:104-108`, `lib.rs:1202`. The sync
`wake_response_mode` branch retains test coverage but the production
path is async. No perf concern; bookkeeping.

### Carry MINORs unchanged from r29-r32

| Tag | Notes |
|---|---|
| R30-P2 | `NomadStopPermitGuard::Drop` silent discard (`nomad_ch.rs:657-668`) |
| R30-P3 | `dec_nomad_stop_permits_in_use` convention (`metrics.rs:496-528`) |
| R29-P2 | Doubled blocking-pool peak on wake join (`wake_machine.rs:518-530`) |
| R29-P3 | `transport_error` prefix-match stringly-typed (`restore_handler.rs:3083-3098`) |

---

## RESOLVED this round

### R32-P1 — parallel `mkfs.ext4 ×2` on cold-boot CREATE

`2faaf39b` ships Knob A. ~28 LOC of `std::thread::scope` inside the
existing spawn_blocking. Expected p50 win 1.0–1.5 s; cluster
measurement gated on R33-V1.

---

## OPEN / LATENT carries

R16-P3 / R16-P5 / R17-P2 / R5-P1b — snapshot-side bandwidth + sweep
caches; unchanged. Knob B (`driver_stages_disk_images=true` default
flip, `config.rs:1011`) — Phase-4 cluster validation gate, unchanged.

---

## Where the next CREATE-p50 second comes from

Stacked against r32-T1's c=1 cold-boot 6.3–9.9 s wall:

| Slice | Best-case win | Where |
|---|---:|---|
| **R32-P1 parallel mkfs (CLOSED)** | 1.0–1.5 s | `nomad_ch.rs:1133-1180` |
| **R33-P2 alloc-running cadence** | 0.025–0.05 s | `nomad_ch.rs:3235` |
| **R32-P2 fold sync FS ops** | 0.001–0.03 s | `restore_handler.rs:870-882, 985` |
| **R33-P1 in-VM boot trim** | 2.0–3.0 s | **Out of this crate's scope** |
| **Knob B (driver-side staging)** | 1.0–1.5 s | `config.rs:1011` (Phase-4) |

The next ~2.5 s of recoverable CREATE p50 (Knob B + R33-P1) lives
outside `crates/sandbox/src/**`. Inside the crate, R33-V1 → R33-P2 →
R32-P2 is the full r33 backlog.

---

## Wake-path: still at controller-side floor

No new findings. r32-T1's measurement of in-VM boot dominance applies
to CREATE; the wake path's 49 s p50 (v25) is dominated by CH-internal
`--restore` (~42 s) which is structurally addressable only inside the
CH binary. Async-restore-ack (R32-P3) already decouples user-facing
latency from this wall when clients use the polling protocol.

---

## Net assessment

R32-P1 is the only landed perf-shape change this round and remains
unmeasured on cluster (R33-V1). The next controller-side win (R33-P2,
~25–50 ms) is bounded by the c=1 N=3 noise floor; defer to a c=1 N=20
baseline. The next big wins (R33-P1 in-VM boot ~3 s; Knob B driver-
side staging ~1–1.5 s) are not in this crate's scope.

R32-P2's wake_machine half is downgraded from r32's framing — the
detached runtime bounds its blast radius to a single thread per
wake. Still worth folding for hygiene.

**r33 measurement ask**: drive an r32-T1-style 3-cycle c=1 CREATE
trace against a controller pin including `2faaf39b` to validate the
parallel-mkfs delta. ~$0.10.

---

## To perf r34 backlog

1. **R33-V1** — cluster-measure R32-P1 against a fresh pin.
   **IMPORTANT (NEW r33).**
2. **R33-P2** — drop `wait_for_alloc_running` happy-path cadence
   100 → 50 ms. ≤ 5 LOC, ~25–50 ms p50. **MINOR (NEW r33).**
3. **R32-P2** — fold three sync FS ops into the existing store.get
   spawn_blocking (sync path) + the wake_machine equivalent.
   ~15 LOC. **MINOR carry (downgraded).**
4. **R33-P1** — in-VM boot trim (kernel cmdline + systemd-bypass).
   Cross-worktree (nomad-driver-ch + rootfs scripts). **IMPORTANT
   (NEW r33) — out of in-crate scope.**
5. **Knob B** — flip `driver_stages_disk_images` default once
   Phase 4 is green; delete the spawn_blocking branch. Phase-3 gated.
6. **R32-P3** — confirm/retire sync `wake_response_mode`.
   **MINOR carry.**
7. **R30-P1** — permit-hold documentation gap. ~20 LOC docstring.
   **IMPORTANT carry.**
8. **R29-P2 / R29-P3 / R30-P2 / R30-P3** — carries, unchanged.

In-crate LOC for r33 actionable items: ~20 LOC (R33-P2 + R32-P2).
Knob B is a deletion gated on Phase 4. R33-P1 is the next big number
but lives outside `crates/sandbox/src/**`.
