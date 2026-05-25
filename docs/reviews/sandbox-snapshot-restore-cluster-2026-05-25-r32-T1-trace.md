# T-32-T1: CREATE-path trace (1+1 cluster, c=1 ×3, controller v40)

**Run**: 2026-05-25 04:17–04:20 UTC. 1-server + 1-worker, asia-northeast3-a.
**Controller**: `zeroship-sandbox.snapshot-v40` (gitSHA `1d58ab53`),
SHA256 `2294cfaba7fa5945f9c96aefdf3f6d3ce6c0a2a11a9477f7e992e40d71c7f396`.
**Driver**: v25 (pinned at `a395f1b5`).
**Harness**: `snapshot_stress.py --cycles 3 --concurrency 1 --label r32-T1-trace`.
**Cost**: ~$0.10. Cluster torn down post-run.

## The question

Where does the 8.9s CREATE p50 actually go? Specifically — is "~3s
Nomad scheduling" a real bottleneck or interpolation?

**Answer: it's interpolation. Nomad scheduling is ~100ms. In-VM
boot is 5-8s.**

## Trace points added (commit `1d58ab53`)

- `submit_done`        — after `submit_nomad_job` returns
- `alloc_first_seen`   — first poll where /v1/job/<id>/allocations returns non-empty array
- `alloc running`      — first poll where any alloc has ClientStatus="running" (existing)
- `agent_ready`        — wait_for_agent_livez completes (existing)

## Raw measurements (3 cycles, ms from `create_started`)

| Cycle | submit_done | alloc_first_seen | alloc running | agent_ready | wall |
|-------|------------:|-----------------:|--------------:|------------:|-----:|
| 1     | 8           | 101              | 717           | 5548        | 6293 |
| 2     | 3           | 101              | 713           | 7682        | 8633 |
| 3     | 4           | 101              | 915           | 8980        | 9905 |

(Harness reports `create_ms` = wall-clock end-to-end POST→201. The
small gap between `agent_ready` and wall is HTTP serialization + the
final pg row commit + 201 response.)

## Decomposed timing (Δ between adjacent emits)

| Segment | C1 | C2 | C3 | What it covers |
|---|---:|---:|---:|---|
| **prep + submit_nomad_job** | 8ms | 3ms | 4ms | pubkey gen, vm_index reserve, host_dir mkdir, JSON build, HTTP POST to Nomad |
| **submit → alloc_first_seen** | 93ms | 98ms | 97ms | Nomad: raft commit job, eval enqueue, scheduler pickup, plan apply, alloc returned in /allocations response |
| **alloc_first_seen → alloc running** | 616ms | 612ms | 814ms | Nomad client poll, plugin StartTask RPC, driver `start_task.go` (tap setup, exec cloud-hypervisor, API socket probe), ClientStatus update |
| **alloc running → agent_ready** | 4831ms | 6969ms | 8065ms | **CH boots kernel, kernel boots, init.sh execs, sandbox-agent binds 7777, controller `/livez` probe sees 200** |

## Findings

### Finding 1 — Nomad scheduling is NOT a 3s problem

Eval + placement + plan apply + alloc visible = **<100ms** (likely
<50ms; our poll cadence caps observation at 100ms granularity). My
earlier "3s Nomad scheduling" estimate was off by **30×**.

### Finding 2 — Client+driver dispatch is ~700ms, broadly as expected

Driver `start_task.go` does:
- tap setup (~100ms)
- exec cloud-hypervisor (~200-500ms on cold fs)
- API socket probe (≤500ms budget, returns sooner)
- return TaskHandle to Nomad client

Plus the client→server `ClientStatus="running"` update. **700ms is
on-spec, no leverage here.**

### Finding 3 — In-VM boot dominates CREATE: 77-81% of wall

Per cycle:
- C1: 4831 / 6293 = 77%
- C2: 6969 / 8633 = 81%
- C3: 8065 / 9905 = 81%

This is `(CH spawn → kernel decompress → kernel boot → systemd → init.sh
→ sandbox-agent binds 7777 → controller's livez probe sees 200)`.
**5-8 seconds.** Massive variance across 3 cycles suggests warm-vs-cold
page cache, fs cache, kernel exec latency.

### Finding 4 — `alloc_first_seen` poll-cadence-bound

All three cycles report exactly 101ms for `alloc_first_seen`. That's
the first 100ms poll inside `wait_for_alloc_running` firing. The true
Nomad eval+placement latency is **somewhere between 0 and 100ms**.
To get a sharper number we'd need either:
- (a) shorter poll cadence (50ms? 25ms? burn CPU for ~50ms more info)
- (b) Nomad metrics ingest: `nomad.eval.dequeue` + `nomad.client.allocrunner.taskrunner.started` histograms via `/v1/metrics`

Recommend (b). Don't tighten the poll just for diagnostics.

## Where the leverage actually is

Forget "Nomad is slow". The leverage is **in-VM boot (5-8s)**:

| Phase inside the VM | Estimated cost | Attack |
|---|---:|---|
| CH spawn vCPUs + memory map | ~0.5-1s | Out of scope (CH-internal) |
| Linux kernel boot | ~1-2s | Stripped initramfs; kernel cmdline `quiet` already set; `console=ttyS0` is dev-debug only — moving to `console=null` saves 200-500ms |
| systemd / init early stages | ~0.5-1s | Replace systemd with direct `init.sh` exec; ~1s win, larger change |
| init.sh mounts + env | ~0.2s | minimal |
| sandbox-agent Go runtime startup | ~0.1-0.2s | already minimal |
| First /livez probe at 50ms cadence | ~50ms slack | already tuned |

**Realistic 5-8s → 3-5s if we trim kernel boot + systemd**. That's a
multi-day workstream, not a one-line tweak.

## What didn't show up

- No EvalBroker stall (the canonical Nomad slow-scheduling pathology)
- No driver fingerprint wait (ch driver was warm-detected)
- No client-server heartbeat batching delay
- No raft commit lag

The cluster is healthy; Nomad is doing what it should.

## Teardown

```
gcloud compute instances list --filter='name~zsbx-' → 0 instances
```

Verified 0 residuals. ~$0.10 spent.

## Recommendations (deferred backlog)

- **r32-T2** (IMPORTANT): wire Nomad `/v1/metrics` scrape into the
  driver-metrics file exporter; eliminate the poll-cadence-bound
  estimate of `alloc_first_seen`.
- **r32-T3** (DESIGN-LEVEL): in-VM boot optimization workstream —
  trim kernel cmdline `console=ttyS0`, evaluate direct-exec
  `sandbox-agent` from initramfs. Estimated 2-3s win, multi-day cost.
- **r32-T4** (TRIVIAL): the driver-side trace (start_task entry /
  ch_spawned / handle_returned) would attribute the 700ms client+driver
  segment more precisely. Land in the nomad-driver-ch worktree.
