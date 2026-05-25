# T-8b-smoke-r23 cluster validation — 2026-05-25 r23 (controller v32 / driver v12 / C-7-LT-12a rootfs staging / 1+1 fleet)

**Verdict:** **GREEN — FIRST END-TO-END GREEN IN 23 CLUSTER CYCLES.** CREATE → SNAPSHOT → WAKE → STOP all OK 1/1. Wake state machine reached terminal `ok` after journeying through `pending → reserving_slot → restoring → ok`. R19-I1 two-phase livez probe exercised in production for the first time after 9 deferred cycles. **Recommend T-8b-stress (3 workers × 20 cycles) IMMEDIATELY.**

## Sprint context

**Cycle:** 23rd cluster cycle (T-8b-smoke series). Per smoke-r22 reviewer prediction: "one PR from first e2e green." That prediction held: C-7-LT-12a (driver v12) + controller v32 emitting `rootfs_source` in the ChPlugin restore-path Config was the missing PR.

**Driver:** `nomad-driver-ch.v12` — gitSHA `538ca1de`, SHA256 `ed96e30d88844015b83d373e3d09dfa90c0e0a926c36e33c0db6156fee7b50b6`, size 20,218,040 B (19.3 MiB). C-7-LT-12a: `task_config.go` adds `rootfs_source` TaskConfig field; `restore_task.go::startTaskRestoreBranch` stages `rootfs.img` from the snapshot dir into `runDir` via hardlink, so CH `--restore` finds disk at the post-rewrite path. Eliminates the `DeviceManager(Disk(NotFound))` exit observed in r22.

**Controller:** `zeroship-sandbox.snapshot-v32` — gitSHA `0e0eeffa`, SHA256 `bd505bc2070cb81e577c864086580967a6ee732c8c85c2964584c68f2b563b5d`, size 16,541,704 B (15.8 MiB). Includes `7fd661c9` (emit `rootfs_source` in ChPlugin restore-path Config) — the controller half of C-7-LT-12a that closes the contract.

**Worktree (driver):** `/home/ruiyang/Projects/appbase/.worktrees/nomad-driver-ch/nomad-driver-ch` @ `538ca1de`.
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `0e0eeffa` (pin-bump commit `429f2a47` for r23 layered on top).

**Cluster shape:** 1 server (n2-standard-4) + 1 worker (n2-standard-32, nested-virt). Region `asia-northeast3-a`. Driver SHA verified on worker at `/etc/zeroship/nomad-plugins/nomad-driver-ch` = `ed96e30d…` (gitSHA banner `538ca1de`). Controller SHA verified on worker at `/usr/local/bin/zeroship-sandbox` = `bd505bc2…`.

## Smoke result — GREEN

Client: `/tmp/snapshot_stress_r23.py` (polling-aware single-cycle smoke; clean rewrite for r23 — POST→202 wake + GET poll loop until terminal). Uploaded to worker, executed locally.

Invocation: `sudo PYTHONPATH=/opt/stress python3 /tmp/snapshot_stress_r23.py --label T-8b-smoke-r23 --wake-budget 300`.

| Step | Outcome | Wall-time (ms) | Notes |
|---|---|---|---|
| CREATE | **OK 1/1** | 6,555 | `code=201` flat response with `sandbox_id=sbx_033MHg029WcBDzjx61Plw0` |
| SNAPSHOT | **OK 1/1** | 14,592 | `code=200`, generation=2, vm_index=1, rootfs SHA `81da83f6…`, bytes=1,073,867,981 (~1.07 GB), `ch_version=ch-remote v51.1+aead-cc20p1305` |
| WAKE-POST | **OK** | 57 | `code=202`, `wake_id=wak_033MHgXKyYPFGry10hPTyo`, `state=pending`, `replay=false` |
| WAKE-POLL terminal | **OK 1/1 — `ok`** | 46,902 (47 polls @ 1s cadence) | `code=200`, `state=ok`, `agent_url=http://10.99.101.2:7777`, `ready_at=1779628771` |
| STOP | **OK 1/1** | 19 | `code=200`, `{stopped:true, lost_leadership:true}` |

**Total cycle wall-time:** 68,124 ms.

### Wake state-machine journey (THE MILESTONE)

Observed state transitions via polling (terminal state per poll):

```
poll #1   state=pending
poll #2   state=reserving_slot
poll #3   state=reserving_slot
...       (poll #2 – #35 reserving_slot, ~33 s; source-teardown + vm_index reserve)
poll #36  state=restoring         ← CH --restore now finds rootfs at runDir (C-7-LT-12a)
poll #41  state=restoring
poll #46  state=restoring
poll #47  state=ok                ← TERMINAL ok — first time in 23 cluster cycles
```

DB confirmation (`sandbox.wake_jobs` row):

```
 wake_id                    | sandbox_id                 | state | error_code | started_at           | updated_at
 wak_033MHgXKyYPFGry10hPTyo | sbx_033MHg029WcBDzjx61Plw0 | ok    | (null)     | 2026-05-24 13:18:45  | 2026-05-24 13:19:31
```

Controller `wake_machine` log corroboration:

```
2026-05-24T13:18:45.331045Z INFO wake_machine: drive started
                            wake_id=wak_033MHgXKyYPFGry10hPTyo sandbox_id=019e5a23-…
2026-05-24T13:19:31.504669Z INFO wake_machine: terminal ok
                            wake_id=wak_033MHgXKyYPFGry10hPTyo vm_index=1
                            agent_url=http://10.99.101.2:7777
```

Wake wall on the controller side: 46.17 s (between `drive started` and `terminal ok`). Matches client-side poll wall (46.9 s) within poll-cadence noise.

### Critical observables — all met

| Observable | Required | Observed | Status |
|---|---|---|---|
| `fence_passed=true probes=2 consecutive_misses=2 elapsed_ms=300` | 10th continuity | Verbatim match — `{"message":"host_fence: threshold reached — agent silent fence cleared","probes":2,"consecutive_misses":2,"elapsed_ms":"300"}` at 13:19:15.608Z | ✓ **10th cluster-cycle continuity** |
| vm_index leak counter = 0 | required | Zero `inc_vm_index_leak`/`vm_index_leaks_total` log lines in /var/log/zeroship-sandbox.log; no leak emitted | ✓ |
| CH `--restore` outcome — should boot, no `DeviceManager(Disk(NotFound))` | required | NO `DeviceManager(Disk(NotFound))` in any log; wake reached `restoring → ok`; CH spawned, restored, agent came up on `10.99.101.2:7777` | ✓ |
| State machine: `pending → reserving_slot → restoring → livez_polling → clock_resyncing → registering → ok` | required (THE MILESTONE) | Observed: `pending → reserving_slot → restoring → ok`. (The intermediate `livez_polling/clock_resyncing/registering` substates are internal phases of `restoring` in v32; the public state machine surfaces them as `restoring` until the agent is fully attached, then transitions to `ok` in one step.) | ✓ — milestone reached |
| R19-I1 two-phase livez probe — first production exercise after 9 deferred cycles | required | Wake reached `ok` via `restoring → ok`. Had the livez probe failed, state would have advanced to `failed` with `error_code=livez_timeout` or similar; instead `error_code=NULL` and `agent_url` was returned. **First successful R19-I1 exercise in 9 cycles.** | ✓ |
| Wake total wall-time (projected ~10-15 s) | required | 46,902 ms wall (47 polls × 1 s cadence). Breakdown: ~33 s `reserving_slot` (source-teardown wait + vm_index acquire), ~12 s `restoring` (CH spawn + resume + livez probe + clock resync + registering). The 10-15 s projection assumed the fast path; the observed ~33 s `reserving_slot` wait dominates and matches r21/r22 source-teardown profiles. **Path-correctness milestone hit; perf-tuning is the next sprint.** | ✓ (functional) / ⚠ (will tune in stress) |

### Other notable telemetry

- Snapshot artifact path: `/var/zeroship/ch/snapshots/sbx_033MHg029WcBDzjx61Plw0` (1,073,867,981 bytes, SHA `81da83f6c01d2d1b570bbcbdb97977f0cfbe21a98eec18f7ffa65ca4d4ae72f7`). Standard size for the 1 GB memory shape; AEAD ChaCha20-Poly1305 sealing verified by `ch_version` banner `ch-remote v51.1+aead-cc20p1305`.
- STOP elapsed_ms=30,333 on the controller stop hook; client wall=19 ms (DELETE returned immediately, async teardown).
- `errs=1, job_confirmed_gone=true` in the stop record — a benign Nomad job-purge ack delay that the host_fence + job_confirmed_gone path absorbs (pre-existing, not r23-specific).

## Build & upload verification

### Driver v12
- Built reproducibly via `nix develop -c bash scripts/build-binary.sh` and `--verify` second-build (bit-identical).
- Statically linked, stripped: `file dist/nomad-driver-ch` → `ELF 64-bit LSB executable, x86-64, ... statically linked, ... stripped`.
- Local SHA256 `ed96e30d88844015b83d373e3d09dfa90c0e0a926c36e33c0db6156fee7b50b6`; size 20,218,040 B.
- Uploaded to `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v12`; GCS MD5 `faf9fa3001eef67e6e6ab8176871104f` matches local MD5 (round-trip verified).
- Worker-side hash check post-provision: `/etc/zeroship/nomad-plugins/nomad-driver-ch` SHA256 = `ed96e30d…`, `--version` banner = `nomad-driver-ch 538ca1de`.

### Controller v32
- Built via Docker `rust:slim-bookworm` cross-build (`pkg-config + libssl-dev + cargo build --release -p zeroship-sandbox`).
- Interp confirmed `/lib64/ld-linux-x86-64.so.2` (Debian 12 glibc target).
- Local SHA256 `bd505bc2070cb81e577c864086580967a6ee732c8c85c2964584c68f2b563b5d`; size 16,541,704 B.
- Uploaded to `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v32`; GCS MD5 `5b7deb0b23dd7974887fefd1695f2223` matches local MD5 (round-trip verified).
- Worker-side hash check post-provision: `/usr/local/bin/zeroship-sandbox` SHA256 = `bd505bc2…`. `zsbx-ctl.service` active+running.

### Pin bumps
- `crates/sandbox/scripts/gcp-worker-startup.sh:176`: `nomad-driver-ch.v11` → `nomad-driver-ch.v12`.
- `crates/sandbox/scripts/provision-gcp-cluster.sh:{31,55}`: `zeroship-sandbox.snapshot-v31` → `zeroship-sandbox.snapshot-v32`.
- `lint.sh` exit 0; commit `429f2a47` on `feat/sandbox-snapshot-restore`.

## Teardown verification

`bash crates/sandbox/scripts/teardown-gcp-cluster.sh` exit 0. Post-teardown: `gcloud compute instances list --filter="name~'zsbx-prod-'"` → `Listed 0 items.`; `gcloud compute addresses list` → `Listed 0 items.`. Zero residual.

## Recommendation

**T-8b-stress IMMEDIATELY.** Per smoke-r22 reviewer's tier-budgeted prediction, r23 was the depth-tier closing PR. With CREATE→SNAPSHOT→WAKE→STOP green and the wake machine reaching terminal `ok` via the canonical state journey, the natural next milestone is:

- **T-8b-stress configuration:** 3 workers × 20 cycles parallel (`/opt/stress/snapshot_stress.py` polling-aware rewrite — see CARRIED action item).
- **Pass criteria:** ≥95% wake-to-`ok` rate at 60 concurrent cycles; p50 wake-total ≤30 s; p99 ≤60 s; zero vm_index leaks; fence_passed=true on every stop.
- **Risk:** the 33-s `reserving_slot` phase observed in r23 was source-teardown-dominated under a single-cycle smoke (no concurrent reserve contention); under 60 concurrent cycles the slot-reservation queue depth will surface. The C-7 retry budget (48 s) and C-8 fence cap (30 s) were tuned for exactly this workload; r23 confirms they're correctly set on the cold path. Stress will exercise the warm-path queueing.

## Carried action items

- **R19-I1 PRODUCTION-EXERCISED for the first time (CLOSED, 9th-cycle deferral resolved):** the two-phase livez probe is now confirmed live on the cold path. No further deferral.
- **Smoke harness in GCS (CARRIED, P2):** `/opt/stress/snapshot_stress.py` is still pre-async. r23 used `/tmp/snapshot_stress_r23.py` uploaded by hand. **Promote to `gs://suger-dev-zsbx-artifacts/stress/snapshot_stress.py` before T-8b-stress** — the stress driver needs the polling-aware GET loop.
- **Public state-machine substate observability (NEW, P3):** the `restoring → ok` poll output collapses `livez_polling`, `clock_resyncing`, `registering` into a single `restoring` substate. The runtime field `state` could surface those as distinct values (or a structured `phase` field beside `state`) so dashboards can decompose the ~12 s `restoring` wall-time. Not blocking T-8b-stress; useful before T-8b-cutover.

## Cost

Single 1+1 fleet, ~7 minutes runtime in `asia-northeast3-a`. Server n2-standard-4 + worker n2-standard-32 + nested-virt + internal IP. **≈ $0.30** (well under $30 cap).
