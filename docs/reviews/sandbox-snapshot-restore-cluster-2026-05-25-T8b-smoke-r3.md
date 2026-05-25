# T-8b-smoke-retry-r3 cluster validation — 2026-05-25 r3 (driver v3 / C-1 fix, 1+1 fleet)

**Sprint:** T-8b-smoke-retry-r3 — third 1-worker smoke after C-1 (the `--config` bug surfaced in r2) was fixed in driver v3.
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `866baf42`.
**Worktree (driver):** `/home/ruiyang/Projects/appbase/.worktrees/nomad-driver-ch/nomad-driver-ch` @ `99357f25`.
**Verdict:** **FAIL — smoke failure on CREATE again. C-1 confirmed fixed (CH no longer rejects `--config`); new bug C-2 surfaced: VM never boots because CH is launched with a non-existent disk path.**
**Recommendation:** **NO-GO for T-8b-stress.** Driver-side fix required — translate the disk path correctly (point at the worker's pre-staged `rootfs-slim.img` / artifact dir, the same files the bash wrapper consumes) before the next smoke.

## TL;DR

C-1 (`unexpected argument '--config'`) is **gone**. The Go driver now spawns `cloud-hypervisor` with the long-form CLI flags, and CH parses them. CH then fails fast on a downstream issue:

```
Exit Code: 1, Exit Message: "exit status 1:
cloud-hypervisor:   0.042680s: <main> ERROR:
  /home/runner/work/.../cloud-hypervisor/src/lib.rs:23 --
  Fatal error: VmBoot(VmBoot(DeviceManager(
    Disk(Os { code: 2, kind: NotFound,
              message: \"No such file or directory\" }))))
Error: Cloud Hypervisor exited with the following chain of errors:
  0: Error booting VM
  1: The VM could not boot
  2: Error from device manager
  3: Cannot open disk path
  4: No such file or directory (os error 2)"
```

CH gets ~42 ms into boot before it tries to open the disk image, finds it missing on disk, and aborts. All 3 controller retries hit the same failure (different sandbox IDs, same VM-image-path bug); after the 3rd attempt the controller returns 503 `create_retry_budget_exhausted` to the smoke client.

Smoke wall: 92.9 s. p50 timings: not measurable (CREATE never landed; SNAP/WAKE never attempted).

This is a fresh driver bug (C-2). C-1 was a correct step forward — exhaust it, surface the next layer. C-2 lives in `StartTask` disk-arg assembly: the path the driver hands to CH does not resolve to a file the worker has staged. The bash wrapper avoids this by `cd "$ZSBX_ARTIFACT_DIR"` and referencing `rootfs-slim.img` as a relative name in `$PWD`; the Go driver presumably either writes the wrong absolute path or doesn't honour `ZSBX_ARTIFACT_DIR` the same way.

## Pre-req verification (all 5 PASS)

| Pre-req | Expected | Observed |
|---|---|---|
| Controller pin (snapshot-v19) baked into provision script | `CONTROLLER_OBJECT=zeroship-sandbox.snapshot-v19` | OK (unchanged from r2) |
| Driver v3 SHA matches local build | `0090d87e…fdac93c1` on local + GCS + worker disk | OK — all 3 match (see Step 2 below) |
| Driver v3 gitSHA | `99357f25` (post C-1 fix); NOT `2982e0b9` | OK — `nomad-driver-ch 99357f25` reported by binary on worker |
| `plugin "nomad-driver-ch" { config {} }` stanza in worker Nomad config | yes (loader-required for Nomad 2.0.2) | OK — `ch Detected=true Healthy=true ready` in `nomad node status` |
| R12-I1 wake-path `TaskDriverMode` arg (b3bf741c) | included in v19 controller | OK (no Rust change this round; same v19 controller as r2) |
| C-1 fix landed on driver | gitSHA 99357f25 (long-argv flags, no `--config`) | OK — Nomad journal shows CH spawned without `--config`, exits on disk-not-found rather than arg-parse |
| Daily budget marker line count | < 10 | 2 → 3 (this run; line `2026-05-24T00:53:36+00:00 T-8b-smoke-retry-r3 provision`) |

## Provision (Step 6)

Invocation:

```
SERVER_COUNT=1 WORKER_COUNT=1 \
  EXTRA_WORKER_METADATA="install-ch-plugin-driver=1" \
  bash crates/sandbox/scripts/provision-gcp-cluster.sh
```

| Phase | Time |
|---|---|
| Server `zsbx-prod-server-1` create + `zsbx-server-ready` sentinel | 60s |
| Worker `zsbx-prod-worker-1` create + `zsbx-worker-ready` sentinel | 45s |
| Provision wall (network + IP reserve + both sentinels, sequential) | ~3m05s (start 00:53:46Z, "cluster up" line 00:56:51Z) |

Final fleet:

```
NAME                ZONE               MACHINE_TYPE    PRIVATE_IP   STATUS
zsbx-prod-server-1  asia-northeast3-a  n2-standard-4   10.178.0.10  RUNNING
zsbx-prod-worker-1  asia-northeast3-a  n2-standard-32  10.178.0.16  RUNNING
```

Provision log: `/tmp/t8b-smoke-r3-provision.log`.

## Cluster validation

| Check | Expected | Observed |
|---|---|---|
| Sandbox `/livez` on worker:9091 | 200 | **200** |
| Worker `/etc/zeroship/nomad-plugins/nomad-driver-ch --version` | `nomad-driver-ch 99357f25` | **`nomad-driver-ch 99357f25`** |
| Worker plugin binary sha256 | `0090d87e…fdac93c1` (matches local + GCS) | **match** |
| `nomad node status -self -verbose` ch driver row | `ch  true  true  ready` | **`ch  true  true  ready  2026-05-24T00:56:30Z`** |
| Worker attribute `platform.gce.attr.install-ch-plugin-driver` | `1` | **`1`** |
| Nomad `client.driver_mgr.nomad-driver-ch: ch: StartTask:` lines for the 3 attempts | 3 spawns by driver `ch` (NOT raw_exec) | **3 spawns observed, all driver=`ch`** (`@module=ch`, `task_name=ch`) |

So: the alloc IS running on the `ch` driver. C-1 is fixed.

## Smoke (single cycle, concurrency=1, cycles=1)

```
sudo python3 /opt/stress/snapshot_stress.py --concurrency 1 --cycles 1
```

```
# elapsed: 92.9s
=== snapshot-stress (N=1) ===
CREATE OK: 0/1
SNAPSHOT OK: 0/0
WAKE OK: 0/0
POST-WAKE EXEC OK: 0/0
STOP OK: 0/1
FAILED CREATES: 1
  [1x] code=503: {"error":"create_retry_budget_exhausted",
        "message":"backend.create: 3 attempts failed; last error:
         agent at http://10.99.101.2:7777 never returned 200 on /livez
         (expected fp=561cb8bd25626ce2cc1be340d4e0c4d4)"}
```

**Verdict: FAIL.**

Root cause (per Nomad journal — verbatim, all 3 attempts identical):

```
Exit Code: 1, Exit Message: "exit status 1:
cloud-hypervisor:   0.042680s: <main> ERROR:
  /home/runner/work/cloud-hypervisor/cloud-hypervisor/cloud-hypervisor/src/lib.rs:23 --
  Fatal error: VmBoot(VmBoot(DeviceManager(
    Disk(Os { code: 2, kind: NotFound,
              message: \"No such file or directory\" }))))
Error: Cloud Hypervisor exited with the following chain of errors:
  0: Error booting VM
  1: The VM could not boot
  2: Error from device manager
  3: Cannot open disk path
  4: No such file or directory (os error 2)"
```

Sequence: Nomad reports `StartTask: spawned ch_pid=N` at T+0, then `VM exited: exit_code=1` at T+≤100 ms for every attempt. CH never reaches kernel handoff because the disk path it was told to open doesn't exist on the worker filesystem.

p50 timings: **not measurable** (no successful CREATE).

## Teardown (Step 7)

```
[teardown] project=suger-dev zone=asia-northeast3-a prefix=zsbx-prod
[teardown] deleting instances: zsbx-prod-server-1 zsbx-prod-worker-1
[teardown] releasing internal addresses: zsbx-prod-server-1-ip
[teardown] remaining instances matching ^zsbx-prod-: 0
[teardown] OK: cluster fully torn down
```

Residual count (`gcloud compute instances list --filter='name~"^zsbx-"'`): **0**.
Residual address count (`gcloud compute addresses list --filter='name~"^zsbx-"'`): **0**.
Teardown log: `/tmp/t8b-smoke-r3-teardown.log`.

## Cost estimate

| Resource | Spec | Wall | Approx hourly | Cost |
|---|---|---|---|---|
| `zsbx-prod-server-1` | n2-standard-4 (`asia-northeast3-a`) | ~6 min | ~$0.20/h | ~$0.02 |
| `zsbx-prod-worker-1` | n2-standard-32, nested-virt | ~5.5 min | ~$1.55/h | ~$0.14 |
| Misc (PD, network egress, IP reservation) | — | — | — | ~$0.01 |

**Total: ~$0.17** for the smoke-r3 cycle.

## GO/NO-GO for T-8b-stress

**NO-GO.** The C-1 fix landed and worked as intended — CH no longer aborts on argv parse — but the next layer of the driver's StartTask plumbing (disk path / artifact-dir resolution) is wrong. CH fails fast at ~42 ms on `Cannot open disk path: No such file or directory`. Until C-2 lands and a 1-worker smoke shows `CREATE OK: 1/1` end-to-end, running a 3-worker stress fleet would only burn budget reproducing the same defect three times in parallel.

Next sprint should target C-2 in the driver worktree:

- Identify which arg the driver feeds to `cloud-hypervisor --disk path=…` and confirm it resolves to a file present in the worker's `ZSBX_ARTIFACT_DIR` (`/var/lib/zeroship/ch/...` per the `zsbx-ctl` env block).
- Cross-reference `nomad-vm-wrapper.sh`: it `cd`'s into the artifact dir and references `rootfs-slim.img` by basename. The Go driver likely needs to either (a) honor the same `cwd` + relative-name contract, or (b) compose the absolute path from `ZSBX_ARTIFACT_DIR` + a known basename.
- After driver v4 lands, retry the 1-worker smoke (budget will then be at 4 entries, still under cap).

## Commits

- Sandbox pin bump (this sprint): `866baf42 sandbox/scripts: bump driver pin v2 → v3 (T-8b-driver-v3-upload)`
- Driver HEAD: `99357f25 nomad-driver-ch/start_task: spawn CH with long-argv flags (C-1 fix)` (untagged in driver worktree; uploaded as `nomad-driver-ch.v3`)
