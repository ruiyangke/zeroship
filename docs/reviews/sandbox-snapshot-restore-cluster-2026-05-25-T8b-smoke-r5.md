# T-8b-smoke-retry-r5 cluster validation — 2026-05-25 r5 (controller v20 / C-3 fix, 1+1 fleet)

**Sprint:** T-8b-smoke-retry-r5 — fifth 1-worker smoke after C-3 (snapshot-store
`spawn_blocking` panic) landed in controller v20.
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `953a5fa3` (this commit is the v19 → v20 pin bump; parent `28c2c865`).
**Driver:** unchanged — `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v4`, SHA `b71e33e0011571e424e34f232d3c5e35d36eadafb8e4b5c74d5961c89d5f2016`, gitSHA `ec6a1de4` (C-2 fix descendant).
**Verdict:** **FAIL on WAKE — but C-3 is confirmed fixed: SNAPSHOT landed 1/1 end-to-end for the first time across the T-8b series.** New bug surfaced one stage deeper: a `vm_index_unavailable (cluster exhausted at vm_index=1)` race because the wake handler reserves the source slot before the detached source-teardown releases it.
**Recommendation:** **NO-GO for T-8b-stress. PAUSE the smoke loop.** This is the 5th iteration / 4th distinct new bug in 5 cycles, exceeding the sprint brief's "4 cycles in a row produce a new bug → document the pattern and stop" trigger. Route the next sprint to a focused C-4 fix in `crates/sandbox/src/{snapshot_handler,restore_handler}.rs` (wake-vs-source-teardown vm_index race).

## TL;DR

C-3 (`compio::runtime::spawn_blocking` panic inside `TieredSnapshotStore::put`'s
detached L2 upload) is **gone**. The L2 upload now runs on a plain
`std::thread::Builder::spawn` and the `Tiered::put` sync trait method completes
in 6311 ms with no panic. SNAPSHOT returned `200` with a valid artifact (config
+ memory-ranges + memory.bin on disk, sha256 `b3cf1b1bb965f4087800ff4e91b0f8279bee80f066602860f06c694196bb8b18`,
1073847239 bytes).

The smoke then progressed to WAKE — and the controller returned 503:

```
{"error":"vm_index_unavailable",
 "message":"no vm_index available to host the restored sandbox",
 "requested":1}
```

Controller log root cause (`/var/log/zeroship-sandbox.log` @ 01:55:40):

```
01:55:40.483  sandbox/nomad-ch stop: started
              sandbox_id=019e57b1-c159-77e2-a093-2d23179b5bb7  vm_index=1
01:55:40.559  admin/wake: handler failed
              sandbox_id=019e57b1-c159-77e2-a093-2d23179b5bb7
              error="vm_index unavailable (cluster exhausted at vm_index=1)"
01:57:10.656  sandbox/nomad-ch vm_index released  vm_index=1
01:57:10.656  sandbox/nomad-ch stop: complete  elapsed_ms=90173
```

This is a fresh, distinct bug (call it **C-4**) in the snapshot/wake handler
seam:

- `POST /admin/sandboxes/{id}/snapshot` returns success **immediately** when the
  artifact is on disk, but spawns a *detached* `teardown_source_for_snapshot`
  which keeps the vm_index reserved until the agent host-fence clears (90 s in
  this run — `host_fence: cleared … elapsed_ms=60158` plus job-purge).
- `POST /admin/sandboxes/{id}/wake` calls `reserve_vm_index(snap.vm_index)`
  (`restore_handler.rs:389`) which per § 5.0/5.1 v1 design **forces the source
  slot** (cross-worker fallback documented but not implemented in v1).
- The wake arrives 76 ms after the source-teardown began. The source still holds
  vm_index=1, so `Allocator::reserve` returns Err → `VmIndexUnavailable`.

This is the 4th *distinct* bug in 5 cycles (r3 was a re-test of C-2, not a new
bug):

| Cycle | Driver/Controller fixed | New bug surfaced |
|---|---|---|
| r1 | bash wrapper, install gate, plugin handshake | C-1: driver spawns `--config` arg form |
| r2 | C-1 (long-argv) | C-2: driver hands CH disk path not on disk |
| r3 | (re-test of C-2; no fix in flight) | — (confirmed C-2) |
| r4 | C-2 (rootfs materialization + pre-flight stat) | C-3: snapshot-store panics in `spawn_blocking` |
| r5 | C-3 (`std::thread::Builder::spawn` for L2 detach) | C-4: wake races source-teardown for vm_index; minor C-5 GCS scope 403 |

Sprint brief's stop-condition is **exceeded** (we're past the 4-cycle trigger).

## Pre-req verification

| Pre-req | Expected | Observed |
|---|---|---|
| Controller v20 in GCS | `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v20` exists | **OK** — `Creation Time 2026-05-24T01:51:49Z`, hash `MD5 qouOMNz/TdXHQjw8K43E6A==` |
| Controller v20 build SHA | match local build | **OK** — local SHA256 `282457b9fc5ece1a54ee9484368ebfbdbb1d087d0e0b5f8daf5b8d309ff5b032`, ELF, 16,452,296 B |
| Controller v20 interp | `/lib64/ld-linux-x86-64.so.2` (B16 portable) | **OK** — `readelf -p .interp` confirmed |
| Driver v4 unchanged | SHA `b71e33e0…d5f2016`; gitSHA `ec6a1de4` (C-2 descendant) | **OK** — worker `sha256sum` + `--version` both match |
| `CONTROLLER_OBJECT=zeroship-sandbox.snapshot-v20` in provision script | yes | **OK** — `[provision] … controller=zeroship-sandbox.snapshot-v20` |
| `plugin "nomad-driver-ch"` loaded on worker | `ch true true ready` | **OK** — `nomad node status -self -verbose` shows `ch  true  true  ready  2026-05-24T01:54:49Z` |
| Daily budget marker line count | < 10 | 4 → 5 (this run; line `2026-05-24T01:52:25+00:00 T-8b-smoke-retry-r5 provision`) |
| `shellcheck --severity=error` clean | yes | **OK** — `lint.sh: OK — 7 script(s) clean at --severity=error` |

## Build + upload (Step 1-3)

```
docker run --rm -v "$PWD:/src" -w /src rust:slim-bookworm \
  bash -c 'apt-get update -qq && apt-get install -y -qq pkg-config libssl-dev && \
           cargo build --release -p zeroship-sandbox && \
           readelf -p .interp target/release/zeroship-sandbox'
```

Build wall: ~5 min (full crate graph; release profile). Output:

```
Finished `release` profile [optimized] target(s) in 32.80s
String dump of section '.interp':
  [     0]  /lib64/ld-linux-x86-64.so.2
```

Local artifact:

```
target/release/zeroship-sandbox   16,452,296 B (~16.4 MB)
SHA256: 282457b9fc5ece1a54ee9484368ebfbdbb1d087d0e0b5f8daf5b8d309ff5b032
```

Upload:

```
gcloud storage cp target/release/zeroship-sandbox \
  gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v20 \
  --content-type=application/octet-stream
```

GCS metadata after upload:

```
gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v20:
  Creation Time: 2026-05-24T01:51:49Z
  Hash (CRC32C): WAD1RA==
  Hash (MD5):    qouOMNz/TdXHQjw8K43E6A==
```

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
zsbx-prod-worker-1  asia-northeast3-a  n2-standard-32  10.178.0.18  RUNNING
```

Provision log: `/tmp/t8b-smoke-r5-provision.log`.

## Cluster validation

| Check | Expected | Observed |
|---|---|---|
| Sandbox `/livez` on worker:9091 | 200 `{"status":"ok"}` | **`{"status":"ok"}`** |
| Worker `/etc/zeroship/nomad-plugins/nomad-driver-ch --version` | `nomad-driver-ch ec6a1de4` (C-2 descendant) | **`nomad-driver-ch ec6a1de4`** |
| Worker plugin binary sha256 | `b71e33e0011571e424e34f232d3c5e35d36eadafb8e4b5c74d5961c89d5f2016` | **match** |
| `nomad node status -self -verbose` ch driver row | `ch true true ready` | **`ch  true  true  ready  2026-05-24T01:54:49Z`** |
| Controller config | `vm_index_floor:1, vm_index_ceil:12` | **`vm_index_floor:1, vm_index_ceil:12`** (12 taps available) |
| Worker metadata `vm-index-ceil` | `12` | **`12`** |
| Nomad task driver used | `ch` (not `raw_exec`) | **`ch`** — `client.driver_mgr.nomad-driver-ch: ch: StartTask: spawned … tap=zsbx-nm-1` |

C-3 confirmed: no `not in a compio runtime` panic in the snapshot store path. The
controller boot warning `snapshot_store: AEAD DISABLED — guest RAM plaintext on
disk + GCS (kek env unset)` is documented for v20 and unrelated to the panic
that r4 hit.

## Smoke (single cycle, concurrency=1, cycles=1)

```
sudo python3 /opt/stress/snapshot_stress.py --concurrency 1 --cycles 1 --label T-8b-smoke-r5
```

```
# T-8b-smoke-r5: concurrency=1, cycles=1, total=1
# base_url=http://127.0.0.1:9091
# elapsed: 12.9s

=== T-8b-smoke-r5 (N=1) ===
CREATE OK: 1/1
  create p50/p95/p99/max: 6461 / 6461 / 6461 / 6461 ms
SNAPSHOT OK: 1/1
  snapshot p50/p95/p99/max: 6311 / 6311 / 6311 / 6311 ms
WAKE OK: 0/1
POST-WAKE EXEC OK: 0/0
STOP OK: 0/1
FAILED WAKES: 1
  [1x] code=503: {"error":"vm_index_unavailable", … "requested":1}
```

**Verdict on CREATE: PASS** (1/1, no disk-NotFound, no CH crash). C-2 fix
still holds.
**Verdict on SNAPSHOT: PASS** (1/1, no `spawn_blocking` panic, artifact on disk
and sha256-valid). C-3 fix validated. Single-sample p50 = 6311 ms.
**Verdict on WAKE: FAIL** (0/1, vm_index race — new bug, C-4).

### Root cause for C-4

The snapshot endpoint returns success **as soon as the CH-side snapshot artifact
is on disk**, but launches a *detached* `teardown_source_for_snapshot` task
that keeps `vm_index` reserved until the agent host-fence clears + the Nomad
job is purged. In this run: 90 s of detached work (60.2 s host-fence + ~30 s
Nomad purge).

```
01:55:34.123  CREATE agent_ready          vm_index=1
01:55:40.483  SNAPSHOT returns 200 to client; detached teardown starts
01:55:40.559  WAKE arrives; reserve_vm_index(1) → 503
01:57:10.656  teardown complete; vm_index=1 released (90.2 s after snapshot)
```

The `restore_path` (`restore_handler.rs:389`) honours v1 § 5.0/5.1 — `reserve_vm_index(snap.vm_index)` is sticky to the source slot and cross-worker
fallback is documented but not implemented. So even though the cluster has 11
*other* free slots in the floor-ceil [1,12] range, wake refuses to take any of
them.

Three viable fix shapes (defer to a focused C-4 sprint):

- (a) Make `POST /snapshot` block until the source vm_index is released
  (changes snapshot's wire latency from "artifact-on-disk" to "source-cleanly-down";
  ~90 s in this profile — bad for SLO unless we also speed up teardown).
- (b) Move the wake's `reserve_vm_index` from sticky-source to allocator
  fallback when the source is mid-teardown (extends § 5.0 in v1; needs the
  vm_index allocator to expose a "any free slot" form).
- (c) Make snapshot synchronously release vm_index (decouple "vm_index hold"
  from "host_fence clear" — fence can run async without the index reserved
  because the Nomad job is already gone).

(c) seems most surgical; the agent host-fence is a defensive cleanup, not a
correctness requirement for slot release. Final shape is the C-4 sprint's call.

### C-5 (minor, non-blocking) — GCS scope 403

The detached L2 upload itself succeeded *as a code path* (no compio panic, the
`std::thread::Builder::spawn` worker ran cleanly) but the HTTP call returned
403:

```
tiered: L2 upload failed (v1 fire-and-forget; GCS PR adds retry)
  sandbox_id=sbx_033M0usRvYYo8IPhM0ghmx
  error="GCS single-shot upload snapshots/v1/.../config.json:
         status 403, … Provided scope(s) are not authorized"
```

The worker VM's GCS access scope is too narrow — `gs_pull` works (read), but
`gs_put` to `snapshots/v1/*` fails (write). The provision script grants the
worker access to the artifact bucket via `--service-account` and default
scopes; the bucket-write scope needs adding. Fire-and-forget, so this didn't
fail the smoke directly, but T-8b-stress would lose every L2 upload to GCS
and we'd be storing 60+ × 1 GB snapshots locally only. Track as **C-5** and
fix alongside C-4.

Stress raw JSON + stderr: `/tmp/t8b-smoke-r5-stress.log`.
Controller log (full, 213 lines): `/tmp/t8b-smoke-r5-controller.log`.
Controller wake-path lines: `/tmp/t8b-smoke-r5-controller-wake.log`.
Controller full lifecycle: `/tmp/t8b-smoke-r5-controller-full.log`.
Nomad client lifecycle: `/tmp/t8b-smoke-r5-nomad.log`.

p50 timings (single sample, sandbox lifecycle to first failure):
| Phase | ms |
|---|---|
| CREATE (cold boot, agent_ready) | 6461 |
| exec_pre (post-create echo) | 8 |
| SNAPSHOT (pause + ch-remote snapshot + L1 write + L2 detach) | 6311 |
| WAKE | **(failed — 76 ms to 503)** |

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
Teardown log: `/tmp/t8b-smoke-r5-teardown.log`.

## Cost estimate

| Resource | Spec | Wall | Approx hourly | Cost |
|---|---|---|---|---|
| `zsbx-prod-server-1` | n2-standard-4 (`asia-northeast3-a`) | ~5 min | ~$0.20/h | ~$0.02 |
| `zsbx-prod-worker-1` | n2-standard-32, nested-virt | ~4 min | ~$1.55/h | ~$0.10 |
| Misc (PD, network egress, IP reservation) | — | — | — | ~$0.01 |

**Total: ~$0.13** for the smoke-r5 cycle.

## GO/NO-GO for T-8b-stress

**NO-GO. PAUSE the smoke loop.**

The sprint brief explicitly says: *"If smoke STILL fails (5th iteration, 4th
cycle), capture failure verbatim. This would be a 4th-in-a-row new bug + 4
attempts on the cluster smoke. Document the pattern, stop, recommend pause for
user input."*

We have hit exactly that condition: r5 is the 5th iteration, C-4 is the 4th
**new** bug in 4 distinct fix-cycles (r3 was a re-test of C-2 with no fix in
flight, so it doesn't count toward the new-bug count). Running a 3-worker × 20-
cycle stress run now would reproduce the wake-race 60× with nothing to learn
from it.

### The pattern (5 cycles, 4 fixes, 4 new bugs + 1 minor sibling)

| Cycle | Fix landed in flight | Fix surface | New bug surfaced |
|---|---|---|---|
| r1 | none | — | B-series + C-1 (driver spawn arg form) |
| r2 | C-1 (long-argv) | driver | C-2: driver hands CH disk path not on disk |
| r3 | (no new fix) | — | confirmed C-2 |
| r4 | C-2 (rootfs materialization + pre-flight stat) | driver | C-3: snapshot-store panics in `spawn_blocking` |
| r5 | C-3 (`std::thread::Builder::spawn` L2 detach) | controller | C-4: wake races source-teardown for vm_index; C-5: GCS scope 403 |

Reading: each cycle clears one stage of the lifecycle (driver-spawn, driver-CH-
boot, snapshot-store-write) and reaches a fresh code path one step deeper. The
WAKE path is brand new in r5 — no prior smoke ever got past SNAPSHOT.

After C-4 lands, the next likely failure surfaces are:

- post-wake exec (restored guest's `zeroship-agent` not picking up after the
  reload — would be a sandbox-agent / cloud-init / IP-handoff bug).
- STOP after wake (sandbox state machine in `snapshotted_suspect` or stuck CAS).

These also have `spawn_blocking` + compio crossover and may benefit from an
audit sweep rather than wait-and-react. Worth scoping into the C-4 sprint.

## Commits

- Sandbox pin bump (this sprint): `953a5fa3 sandbox/scripts: bump controller v19 -> v20 (T-8b-ctl-v20-upload)`
- C-3 fix (now in controller v20): `c890c015 sandbox/snapshot-store: detach L2 upload via std::thread::spawn (C-3 fix)`
- R13-Q1 env-mutex unify (now in v20): `c5b9cb9d sandbox: unify SANDBOX_TASK_DRIVER env-mutex across nomad_ch + restore_handler (R13-Q1)`
- R10-API5/6 + r12/r13 reviewer artifacts (now in v20): merged through `28c2c865`
- Driver HEAD (unchanged): `ec6a1de4` (descendant of `f521eb21` C-2 fix; uploaded as `nomad-driver-ch.v4`)
- This review: pending (commit follows)
