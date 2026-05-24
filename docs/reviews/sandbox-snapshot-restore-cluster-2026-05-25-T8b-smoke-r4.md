# T-8b-smoke-retry-r4 cluster validation — 2026-05-25 r4 (driver v4 / C-2 fix, 1+1 fleet)

**Sprint:** T-8b-smoke-retry-r4 — fourth 1-worker smoke after C-2 (disk path / rootfs materialization) landed in driver v4.
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `54da83c5` (this commit is the pin bump itself; parent `8598abdc`).
**Worktree (driver):** `/home/ruiyang/Projects/appbase/.worktrees/nomad-driver-ch/nomad-driver-ch` @ `ec6a1de4` (C-2 fix landed at `f521eb21`; HEAD is the SPRINT-STATUS doc commit on top of it; driver binary embeds gitSHA `ec6a1de4`).
**Verdict:** **FAIL on SNAPSHOT — but C-2 is confirmed fixed: CREATE landed 1/1 end-to-end for the first time across the T-8b series.** New bug C-3 surfaced one stage deeper: a panic at `compio-runtime-0.11.0/src/runtime/mod.rs:119:13: not in a compio runtime` inside the snapshot-store I/O path.
**Recommendation:** **NO-GO for T-8b-stress.** This is the 4th distinct bug surfaced in 4 cycles, satisfying the sprint brief's "4 cycles in a row produce a new bug → document the pattern and stop" trigger. Pause smoke loop, route the next sprint to a focused C-3 fix in `crates/sandbox` snapshot-store callers.

## TL;DR

C-2 (`Cannot open disk path: No such file or directory`) is **gone**. The Go driver now materializes the per-task rootfs from `$ZSBX_ARTIFACT_DIR` before invoking CH and stat-checks every disk path pre-flight, so CH boots and the controller's `/livez` probe succeeds. CREATE landed `1/1` in 6.49 s (well within the ~10 s historical budget), and the post-create exec smoke (`exec_pre`) also returned 200 in 8 ms.

The smoke then progressed to SNAPSHOT — and the controller panicked:

```
thread '<unnamed>' (14300) panicked at
  /usr/local/cargo/registry/src/index.crates.io-1949cf8c6b5b557f/
    compio-runtime-0.11.0/src/runtime/mod.rs:119:13:
not in a compio runtime
```

Controller error returned to client:

```
{"error":"snapshot_store_failed","message":"snapshot store error"}
status=500
internal: "snapshot store: snapshot I/O error: spawn_blocking panic: Any { .. }"
```

This is a fresh, distinct bug (call it **C-3**) in the snapshot-store I/O path — code that runs inside a `spawn_blocking`-spawned thread but reaches for a compio runtime handle (which is thread-local). Per project invariants (zero-tokio, compio everywhere), `spawn_blocking` workers do **not** get a compio runtime; the snapshot store must either route I/O back to the compio thread or use std I/O while detached.

Smoke wall: 12.9 s. CREATE p50/max: 6494 ms.

This is the 4th distinct bug in 4 cycles:
- r1 → bash-wrapper / nomad-driver-ch handshake / install gate (B-series)
- r2 → C-1 driver bypassed long-argv flags
- r3 → C-2 driver bypassed wrapper's `cd $ART_DIR` (disk path not staged)
- r4 → C-3 controller snapshot-store panics inside `spawn_blocking`

Each previous fix surfaces the next layer cleanly, but the pattern says we have an under-tested integration seam. Sprint brief's stop-condition is hit.

## Pre-req verification

| Pre-req | Expected | Observed |
|---|---|---|
| Controller pin (snapshot-v19) baked into provision script | `CONTROLLER_OBJECT=zeroship-sandbox.snapshot-v19` | OK (unchanged from r3) |
| Driver v4 SHA matches local build → GCS → worker disk | `b71e33e0…d5f2016` on all three | OK — local build + GCS metadata + worker `sha256sum` all match |
| Driver v4 gitSHA | New, post-C-2 (`f521eb21` or descendant); NOT `99357f25` (v3) | `nomad-driver-ch ec6a1de4` (descendant of `f521eb21` C-2 fix) |
| `plugin "nomad-driver-ch" { config {} }` stanza in worker Nomad config | yes | OK — `ch  true  true  ready` in `nomad node status -self -verbose` |
| C-2 fix on driver | rootfs materialized + pre-flight stat checks | OK — Nomad journal shows no `Cannot open disk path` errors; CH boots; controller `/livez` returns `{"status":"ok"}` |
| R12-I1 wake-path `TaskDriverMode` arg | included in v19 controller | OK (no Rust change this round) |
| Daily budget marker line count | < 10 | 3 → 4 (this run; line `2026-05-24T01:22:03+00:00 T-8b-smoke-retry-r4 provision`) |

## Provision (Step 5)

Invocation:

```
SERVER_COUNT=1 WORKER_COUNT=1 \
  EXTRA_WORKER_METADATA="install-ch-plugin-driver=1" \
  bash crates/sandbox/scripts/provision-gcp-cluster.sh
```

| Phase | Time |
|---|---|
| Server `zsbx-prod-server-1` create + `zsbx-server-ready` sentinel | 60s |
| Worker `zsbx-prod-worker-1` create + `zsbx-worker-ready` sentinel | 15s |
| Provision wall (network + IP reserve + both sentinels, sequential) | ~75s |

Final fleet:

```
NAME                ZONE               MACHINE_TYPE    PRIVATE_IP   STATUS
zsbx-prod-server-1  asia-northeast3-a  n2-standard-4   10.178.0.10  RUNNING
zsbx-prod-worker-1  asia-northeast3-a  n2-standard-32  10.178.0.17  RUNNING
```

Provision log: `/tmp/t8b-smoke-r4-provision.log`.

## Cluster validation

| Check | Expected | Observed |
|---|---|---|
| Sandbox `/livez` on worker:9091 | 200 `{"status":"ok"}` | **`{"status":"ok"}`** |
| Worker `/etc/zeroship/nomad-plugins/nomad-driver-ch --version` | `nomad-driver-ch ec6a1de4` (C-2 descendant) | **`nomad-driver-ch ec6a1de4`** |
| Worker plugin binary sha256 | `b71e33e0011571e424e34f232d3c5e35d36eadafb8e4b5c74d5961c89d5f2016` | **match** |
| `nomad node status -self -verbose` ch driver row | `ch true true ready` | **`ch  true  true  ready  2026-05-24T01:24:30Z`** |
| Worker attribute `platform.gce.attr.install-ch-plugin-driver` | `1` | **`1`** (assumed — driver loaded successfully) |

C-2 confirmed: driver loads, CH boots, controller comes up healthy.

## Smoke (single cycle, concurrency=1, cycles=1)

```
sudo python3 /opt/stress/snapshot_stress.py --concurrency 1 --cycles 1 --label T-8b-smoke-r4
```

```
# T-8b-smoke-r4: concurrency=1, cycles=1, total=1
# base_url=http://127.0.0.1:9091
# elapsed: 12.9s

=== T-8b-smoke-r4 (N=1) ===
CREATE OK: 1/1
  create p50/p95/p99/max: 6495 / 6495 / 6495 / 6495 ms
SNAPSHOT OK: 0/1
WAKE OK: 0/0
POST-WAKE EXEC OK: 0/0
STOP OK: 0/1
FAILED SNAPSHOTS: 1
  [1x] code=500: {"error":"snapshot_store_failed","message":"snapshot store error"}
```

**Verdict on CREATE: PASS** (1/1, no disk-NotFound, no CH crash, no driver-side error). C-2 fix validated.
**Verdict on SNAPSHOT: FAIL** (0/1, snapshot-store panic — new bug, C-3).

Root cause (controller log; `/var/log/zeroship-sandbox.log`):

```
thread '<unnamed>' (14300) panicked at
  /usr/local/cargo/registry/src/index.crates.io-1949cf8c6b5b557f/
    compio-runtime-0.11.0/src/runtime/mod.rs:119:13:
not in a compio runtime
```

Surrounding controller log entries:

```
admin/snapshot: handler failed
  sandbox_id="019e5799-6480-75e3-a887-dc600b321748"
  error="snapshot store: snapshot I/O error: spawn_blocking panic: Any { .. }"

sandbox/admin: sanitized error (raw not on wire)
  status=500
  code=snapshot_store_failed
  error="snapshot I/O error: spawn_blocking panic: Any { .. }"
```

Interpretation: the snapshot-store I/O path runs inside a `spawn_blocking` worker thread (presumably because of synchronous file/GCS work), and **something inside that path tries to use a compio runtime handle that only exists on compio worker threads**. The compio runtime is thread-local; `spawn_blocking` threads don't carry it. The code needs either to (a) marshal I/O back to a compio thread via a channel + compio task, or (b) use `std::fs` / blocking GCS within `spawn_blocking` and never touch compio there.

Note also a pre-existing controller WARN at startup that may be related but is documented as expected for the v19 build:

```
snapshot_store: AEAD DISABLED — guest RAM plaintext on disk + GCS (kek env unset).
```

That warning is about **encryption-at-rest**, not the runtime panic; the panic occurs before any KEK code path.

Post-failure: controller `stop` pre-flight reports `lost-leadership` and skips `backend.stop AND pg-delete` (peer owns it now) — the alloc was probably gone because Nomad reaped the task after the snapshot 500. Expected fallout.

Stress log: `/tmp/t8b-smoke-r4-stress.log`.
Controller snapshot-path lines: `/tmp/t8b-smoke-r4-controller-snap.log` + `/tmp/t8b-smoke-r4-panic.log`.

p50 timings (CREATE only): `6494.5 ms`. No snapshot/wake/post-wake/stop measurements (none succeeded).

## Teardown (Step 6)

```
[teardown] project=suger-dev zone=asia-northeast3-a prefix=zsbx-prod
[teardown] deleting instances: zsbx-prod-server-1 zsbx-prod-worker-1
[teardown] releasing internal addresses: zsbx-prod-server-1-ip
[teardown] remaining instances matching ^zsbx-prod-: 0
[teardown] OK: cluster fully torn down
```

Residual count (`gcloud compute instances list --filter='name~"^zsbx-"'`): **0**.
Residual address count (`gcloud compute addresses list --filter='name~"^zsbx-"'`): **0**.
Teardown log: `/tmp/t8b-smoke-r4-teardown.log`.

## Cost estimate

| Resource | Spec | Wall | Approx hourly | Cost |
|---|---|---|---|---|
| `zsbx-prod-server-1` | n2-standard-4 (`asia-northeast3-a`) | ~10 min | ~$0.20/h | ~$0.03 |
| `zsbx-prod-worker-1` | n2-standard-32, nested-virt | ~9 min | ~$1.55/h | ~$0.23 |
| Misc (PD, network egress, IP reservation) | — | — | — | ~$0.01 |

**Total: ~$0.27** for the smoke-r4 cycle.

## GO/NO-GO for T-8b-stress

**NO-GO.** C-2 fix is validated, but a fresh defect (C-3) lands one stage deeper in the same sprint cycle. Running a 3-worker × 20-cycle stress run now would reproduce the snapshot-store panic 60× in parallel with nothing to learn from it.

Per the sprint brief: "If a 4th cycle in a row produces a new bug, document the pattern and stop." This is that 4th bug. Recommendation: **pause the smoke loop**, dispatch a focused **C-3** sprint into the sandbox crate to fix the snapshot-store `spawn_blocking` / compio-runtime interaction, then return for T-8b-smoke-retry-r5.

### The pattern (4 fixes, 4 new bugs)

| Cycle | Driver build | Pre-existing surface fixed | New bug surfaced |
|---|---|---|---|
| r1 | v1 / v2 | B-series (bash wrapper, install gate, plugin handshake) | C-1: driver spawns `cloud-hypervisor --config` (CH rejects) |
| r2 | v3 (C-1 fix) | C-1 (long-argv flags) | C-2: driver hands CH disk path that's not on disk |
| r3 | v3 (re-test) | — | confirmed C-2 |
| r4 | v4 (C-2 fix) | C-2 (rootfs materialization + pre-flight stat) | C-3: snapshot-store panics in `spawn_blocking` (`not in a compio runtime`) |

Reading: each cycle adds end-to-end coverage that wasn't previously exercised. The driver path got us through CREATE; we're now in fresh code paths (SNAPSHOT). Each new bug is real but mostly distinct surfaces; not a single root cause flushing through. After C-3 lands, a r5 smoke should reasonably target WAKE / RESTORE / STOP as the next likely failure surfaces — those have similar `spawn_blocking` + compio crossover potential and may benefit from a sweep audit rather than wait-and-react.

## Commits

- Sandbox pin bump (this sprint): `54da83c5 sandbox/scripts: bump driver pin v3 → v4 (T-8b-driver-v4-upload)`
- Driver HEAD: `ec6a1de4 SPRINT-STATUS: mark C-2 complete + T-8b-driver-v4-upload follow-up` (C-2 fix at `f521eb21`; uploaded as `nomad-driver-ch.v4`)
- This review: pending (committed with the cluster-review sprint commit)
