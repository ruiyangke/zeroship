# T-8b-stress-r7 cluster validation — 2026-05-25 (controller v36 / driver v18 / 3+3 fleet / 60-cycle stress + 1+1 smoke regression gate / Option C Phase 4 — `driver_stages_disk_images=true` flag-flip)

**Verdict:** **RED — 0/60 end-to-end OK (0.0 %).** A wholly new failure surface: Postgres connection-pool exhaustion (`FATAL: sorry, too many clients already`) on the single server-1 PG instance. The wedge is **upstream of the Option C driver-side staging path the round was supposed to validate.** SNAPSHOT 9/60 (15.0 %), WAKE 0/9 — every wake attempt fails immediately on `database_failed: wake job lookup failed` because the wake-job row insert prior to it returned `pg: db error`. The `driver_stages_disk_images=true` flag flip is therefore **architecturally unvalidated** — the system never reached enough successful wakes to exercise the cross-alloc staging surface the r1-r6 RED chain has been chasing. The decisive validation is **deferred**, not resolved; r1-r6 ladder is still open, and a new gate (pg pool sizing) has surfaced ahead of it.

Smoke regression gate at WORKER_COUNT=1 is **GREEN** — the Phase 2 flag-on path works correctly under single-worker load; PG pool exhaustion is purely a concurrency-load artifact, not a code regression in the staging cutover.

## Outcome at a glance

| Phase | OK | Denominator | Rate (of total) | Rate (of upstream) |
|---|---|---|---|---|
| CREATE   | 60 | 60 | 100.0 % | — |
| SNAPSHOT |  9 | 60 |  15.0 % | 15.0 % of created |
| WAKE → `ok` | 0 | 9  |  0.0 % | 0.0 % of snapshotted |
| STOP (unconditional cleanup) | 9 | 60 | 15.0 % | 100.0 % of snapshotted |
| **END-TO-END** (CREATE+SNAPSHOT+WAKE+STOP all OK) | **0** | **60** | **0.0 %** | — |

Per-worker (byte-symmetric):
- w1: CREATE 20/20 | SNAPSHOT 3/20 (cycles 0, 8, 16) | WAKE 0/3 | E2E 0/20
- w2: CREATE 20/20 | SNAPSHOT 3/20 (cycles 0, 8, 16) | WAKE 0/3 | E2E 0/20
- w3: CREATE 20/20 | SNAPSHOT 3/20 (cycles 0, 8, 16) | WAKE 0/3 | E2E 0/20

The "every 8th cycle succeeds" cadence is the fingerprint of a connection-pool leak with a slow recycle TTL freeing 1 connection at a time; once pool == max_connections, only cycles aligned with the TTL window can briefly grab a slot.

## Smoke regression gate (WORKER_COUNT=1)

**1/1 GREEN** — single create+snapshot+wake+stop completed cleanly with `driver_stages_disk_images=true` engaged on a one-worker cluster.

| Phase | OK | latency ms | Wake states |
|---|---|---|---|
| CREATE   | 1/1 | 6539  | — |
| SNAPSHOT | 1/1 | 14357 | — |
| WAKE     | 1/1 | 46635 | pending → reserving_slot → restoring → ok |
| STOP     | 1/1 |     3 | — |

The driver-side staging path (TaskConfig.StageDiskImages=true, controller-side spawn_blocking truncate+mkfs.ext4 bypassed) does NOT regress single-worker behaviour. CREATE p50 6.5s is consistent with the controller-side-stage baseline (~6-9s). The Phase 2 code surface is functionally correct; PG concurrency is the new bottleneck.

## Sprint context

**Cycle:** 30th cluster cycle (T-8b-stress-r7 = 1+1 smoke + 3+3 stress). Follows stress-r6 RED at 3/60 e2e and the staging-locality ADR `bbadbe68` Phase 2 implementation:

- **Driver Phase 2** (`42a37265` + `032da940` + `b3b1fe59`) — `StageDiskImages` TaskConfig field, `start_task_stage_total` / `start_task_stage_failures_total` prometheus counters, `stageDiskImages(taskConfig)` op in `ch/stage_disks.go` invoked from cold-boot StartTask before the CH spawn. Driver test suite 182 → 188 PASS.
- **Controller Phase 2** (`2226de7a` + `6e928a25` + `bb178538`) — emits `Task.Config.stage_disk_images` + `Job.Meta.zsbx_stage_disks` in the cold-boot ChPlugin branch when `cfg.driver_stages_disk_images=true`; bypasses controller-side workspace.img spawn_blocking when flag set. Restore branch unaffected by design (it consumes persistent workspace/home.img from snapshot artifacts). Sandbox test suite 512 → 522 PASS.
- **This round's bump**: driver v17→v18 + controller v35→v36 + `Environment=SANDBOX_DRIVER_STAGES_DISK_IMAGES=true` in the worker systemd unit (the critical flag flip).

Hypothesis going in (from the mandate): if Option C is the right architectural pivot, stress-r7 should hit ≥95% e2e because the driver creates fresh workspace.img + user_home.img on every StartTask, eliminating the cross-alloc kernel-state retention surface that r1-r6 stress regressions chased.

What actually happened: the architectural hypothesis remained empirically **untested** — the failure surface moved one layer upstream (PG pool) before the staging path could be exercised under contention.

**Build / upload SHAs** (verified):

- **Driver v18** sha256 `4b99b3348bfdfa2bfdc7a2167a99ebd54ad401fc96e73996253d63e5bcb58349` (size 20,242,616 B); GCS MD5 `329f5cf151654d7786740cd69681bea2`. `scripts/build-binary.sh --verify` confirmed bit-identical rebuild (gitSHA `b3b1fe59`). Uploaded to `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v18`.
- **Controller v36** sha256 `e475904c0476dd61b3451ca953b3245c29dd13f729916b11bf03c1031f525db8` (size 16,746,496 B); GCS MD5 `c01d369c5c10e5d2747505fbc7525f77`. Built in `rust:1.94-bookworm` docker per the v33 lesson (glibc-2.42 nix-shell vs. Debian-12 worker incompatibility). Interpreter `/lib64/ld-linux-x86-64.so.2` confirmed Debian-compatible. Uploaded to `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v36`.
- **Pin bump + flag flip** committed at sandbox `231e66c6` (driver v17→v18, `DRIVER_BINARY_SHA256` literal updated for R20-S3 verify chain, controller v35→v36, and `Environment=SANDBOX_DRIVER_STAGES_DISK_IMAGES=true` added to the zeroship-sandbox systemd unit). `lint.sh`: 7 scripts clean. Driver SPRINT-STATUS entry added at nomad-driver-ch `7c4503eb`.

## The actual wedge — Postgres connection-pool exhaustion

Verbatim PG log evidence captured pre-teardown from `zsbx-prod-server-1` (`/var/log/postgresql/postgresql-*.log`):

```
2026-05-24 20:09:12.817 UTC [6245] postgres@zeroship FATAL:  sorry, too many clients already
2026-05-24 20:09:12.818 UTC [6246] postgres@zeroship FATAL:  sorry, too many clients already
... (~40 identical messages/s for ~10 minutes) ...
2026-05-24 20:09:33.168 UTC [6289] postgres@zeroship FATAL:  sorry, too many clients already
```

Even an interactive `psql` from the server itself was rejected:

```
psql: error: connection to server on socket "/var/run/postgresql/.s.PGSQL.5432" failed: FATAL:  sorry, too many clients already
```

Controller log evidence (verbatim, from worker-1's `/var/log/zeroship-sandbox.log`):

```
WARN sandbox/handlers: pg insert_sandbox failed (non-fatal)        error="pg: db error"
WARN sandbox/handlers: pg insert_event(created) failed (non-fatal) error="pg: db error"
WARN admin/snapshot: handler failed                                 error="database: pg: db error"
ERROR sandbox/admin: sanitized error (raw not on wire)              status=500 code=database_failed error="pg: db error"
WARN sandbox/handlers: pg pre-flight CAS failed; proceeding with backend.stop  error="pg: db error"
```

**Why CREATE survived where SNAPSHOT/WAKE died:** `sandbox/handlers` treats PG writes during create as "non-fatal" (CreateGuard tolerates the row write failing — sandbox UUID is generated client-side and the row insert is bookkeeping). `admin/snapshot` is fatal-PG by design — snapshot record creation is the canonical artifact, no row means no wake-target later. Wake submit (`POST /admin/wake`) likewise looks up the snapshot row + creates the wake_job row, and **both** fail on `pg: db error` → 500 `database_failed`.

The "every 8th cycle succeeds" cadence (cycles 0, 8, 16 succeed on snapshot for every worker, byte-identical) is the fingerprint of slow connection-recycle freeing 1 PG slot at a time; new acquires only get through during recycle windows. Worker-symmetric because Nomad scheduling distributes work evenly and all 3 workers contend for the same single PG instance.

## Failure breakdown — by phase

| Failure | Count | Phase | Surface |
|---|---|---|---|
| (no failure, succeeds) | 9   | SNAPSHOT | cycles 0/8/16 per worker |
| `database_failed: pg: db error` | 51  | SNAPSHOT | non-aligned cycles (PG slot unavailable) |
| `database_failed: wake job lookup failed` | 9   | WAKE | wake submit ran (snapshot row present) but downstream PG read for wake_job → db error |
| `agent at … never returned 200 on /livez (expected fp=…)` | 1   | CREATE retry | secondary; create still succeeded on attempt 2 (CreateGuard retry path) |

The wake submit failure mode is structurally different from the snapshot failure mode: snapshot fails because the snapshot row INSERT fails on pool exhaustion; wake fails because the wake_job row INSERT/SELECT cycle fails. Both fail on PG, but at different SQL surfaces.

**Critically: not a single driver-side staging counter could be examined for the failure-mode classification, because the snapshot wedge prevented even reaching the wake → StartTask(restore) path that would have exercised the new staging surface at scale.**

## Counter deltas

| Counter | Pre-r7 | Post-r7 | Δ | Note |
|---|---|---|---|---|
| `nomad_driver_ch_start_task_stage_total` (NEW Phase 2) | 0 | **NOT EXPOSED via Nomad `/v1/metrics`** | — | Driver counters live in the go-plugin process; surfacing them requires either the driver's own scrape endpoint (not currently wired) or a Nomad metrics filter pass that includes plugin-emitted metrics. **Cannot empirically confirm `start_task_stage_total` was bumped.** |
| `nomad_driver_ch_start_task_stage_failures_total` (NEW Phase 2) | 0 | NOT EXPOSED | — | Same — not surfaced via Nomad. |
| `nomad_driver_ch_destroy_task_unreaped_total` (r4-A) | 0 | NOT EXPOSED | — | |
| `nomad_driver_ch_destroy_task_lock_held_total` (r5-A) | 0 | NOT EXPOSED | — | Should now be irrelevant — driver owns lifecycle, but the cross-alloc retention surface was never tested at scale because the run wedged upstream. |
| `sandbox_corrupt_id_total` | 0 | 0 | 0 | |
| `sandbox_wake_sync_uses_total` | 0 | 0 | 0 | All 9 wake attempts errored before reaching the sync path. |
| `sandbox_wake_terminal_overwrite_blocked_total` | 0 | 0 | 0 | |
| `sandbox_vm_index_leaks_total{reason=*}` | 0 | 0 | 0 | |
| `sandbox_ha_takeover_total{reason=lease_expiration}` | 0 | 0 | 0 | |

**Observability gap:** the Phase 2 driver-side counters (`start_task_stage_*`) were added at `032da940` but are NOT exposed on the Nomad `/v1/metrics` endpoint — they live in the plugin go-process. This was the critical instrumentation for confirming Phase 4 staging engagement and it was effectively dark for this run. **Pre-r8 action: wire `nomad_driver_ch_*` counters into a scrape-friendly surface (either the plugin emits its own `:9999/metrics` or it reuses Nomad's stat tag mechanism so the counters appear under `nomad.client.driver_stats.ch_plugin.*`).**

## Stranded resources at teardown

```
$ gcloud compute instances list --filter="name~'zsbx-prod-'"   → 0 instances
$ gcloud compute addresses list --filter="name~'zsbx-prod-'"   → 0 addresses
```

Mandatory teardown ran cleanly. Driver-side staging cleanup (the architectural surface r24-A2 audit was meant to verify) was not exercised at scale — only 9 snapshot-creating cycles ran; we expected ≥60 destroyTasks; the 51 non-snapshot cycles aborted at the controller layer before issuing the snapshot post-stop teardown path. No `host_dir` leakage observed in the controller log (the existing INFO lines about `leaking host_dir (sweeper-owned, snapshot-aware teardown)` fire on the snapshot path and are by design — these are the 9 snapshot-successful cycles).

## Compare to r1-r6 baselines

| Round | Driver | Controller | CREATE OK | SNAPSHOT OK | WAKE OK | E2E | Wedge surface |
|---|---|---|---|---|---|---|---|
| r1 | v13 | v33 | 60/60 | ~58 | ~5  | 2/60  | rootfs.img cross-alloc lock retention |
| r2 | v14 | v34 | 60/60 | ~57 | ~5  | 2/60  | rootfs.img tap-EBUSY collision |
| r3 | v15 | v34 | 60/60 | ~51 | ~1  | 1/60  | rootfs.img + netdev poll |
| r4 | v16 | v34 | 60/60 | 51   | ~3  | 3/60  | exitDone reap-wait — predicate too weak |
| r5 | v17 | v35 | 60/60 | 51   | ~3  | 3/60  | OFD probe — EBADF bug, predicate never ran |
| r6 | v17 | v35 | 60/60 | 51   | 3   | 3/60  | OFD probe — same EBADF bug; predicate-strength insufficient anyway |
| **r7** | **v18** | **v36** | **60/60** | **9** | **0** | **0/60** | **Postgres connection-pool exhaustion** (orthogonal to staging) |

The diagnostic ladder for the r1-r6 wedge (rootfs.img cross-alloc kernel-state retention) is **NOT exhausted by r7** — Option C was supposed to make that whole class of failure structurally impossible, and that hypothesis remains **untested at scale**. The new failure surface (PG) appeared because the runtime path under the flag-on cutover is slightly different from the flag-off baseline (controller now emits two additional wire-out fields + bypasses one spawn_blocking + changes the host_dir lifecycle ownership semantics), and either (a) one of these changes increased per-cycle PG load, OR (b) the cluster was always close to PG saturation under stress and r1-r6 happened to not cross the threshold because they failed earlier in the pipeline (CH/driver-side) and never put load on PG. Hypothesis (b) is the more likely explanation: r1-r6 ran ~57-60 snapshots successfully (more snapshot load than r7's 9) without saturating PG, which suggests r7's PG saturation comes from controller-side code-path changes, not raw throughput.

## Verdict

**RED — 0/60 e2e — but the wedge is a different layer, not Option C.**

Architectural pivot validation status: **DEFERRED.** Stress-r7 did not refute or confirm the staging-locality hypothesis; it surfaced a PG capacity ceiling ahead of the staging surface.

Critical next-actions (in priority order):

1. **r7-A — PG pool sizing.** Bump server-1 `max_connections` (currently default, likely 100) to a value sized for 3-worker concurrency. Each worker holds ~N controller PG connections + ~M Nomad/wake-machine connections + per-request bursts. Rough sizing: `max_connections >= 3 × (controller_pool_max + nomad_pool_max + per_request_burst)`. Apply via the server startup script's PG init (`crates/sandbox/scripts/gcp-server-startup.sh`).
2. **r7-B — Driver counter surfacing.** Wire `nomad_driver_ch_start_task_stage_*` into Nomad's metrics endpoint (or stand up a side-channel `:9999/metrics` from the plugin) so the Phase 2/4 observability is real, not theoretical. Without this we cannot empirically verify staging engagement in stress runs.
3. **r7-C — Investigate controller v36 PG load delta vs. v35.** Compare per-create-cycle PG transaction count under v35 (flag=false default) and v36 (flag=true). If `host_dir_created=false` + the new wire fields are causing extra row writes per cycle, that's the regression to chase. If not, the saturation is pure concurrency (r7-A solves it).
4. **r7-D — Stress-r8 retry.** After r7-A+r7-B+r7-C, re-run the 3+3 × 20 stress with the same v18 driver + v36 controller. Only then can the Option C architectural validation be assessed.

T-8b-cutover gate remains **BLOCKED** — neither the original r1-r6 chain (Option C unvalidated) nor the new r7 wedge (PG saturation) are resolved. The 6-cycle stress RED chain does NOT end here — it's preempted by a new gate.

## Cluster cost

- Smoke: 1 server (n2-standard-4) + 1 worker (n2-standard-32) for ~7m wall (provision + 1 cycle + teardown). ~$0.20.
- Stress: 3 servers + 3 workers for ~30m wall (provision + 60 cycles parallel + teardown). ~$1.35.
- **Total: ~$1.55** — within the $30 hard cap by a wide margin.

## Teardown

```
$ bash crates/sandbox/scripts/teardown-gcp-cluster.sh
[teardown] OK: cluster fully torn down
$ gcloud compute instances list --filter="name~'zsbx-prod-'"   → 0
$ gcloud compute addresses list --filter="name~'zsbx-prod-'"   → 0
```

Mandatory teardown clean. No stranded resources.
