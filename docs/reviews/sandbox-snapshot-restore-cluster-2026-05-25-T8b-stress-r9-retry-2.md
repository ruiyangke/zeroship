# T-8b-stress-r9-retry-2 cluster validation — 2026-05-24 (heredoc fix verified / **ABORT — dispatch missing `install-ch-plugin-driver=1` metadata flag**)

**Verdict:** **ABORT pre-stress** — per dispatch directive "If only qemu/raw_exec/exec/java/docker returned, the fix didn't work — STOP, teardown, document, exit." The driver-list check returned exactly that 5-driver set with no `ch`. Teardown completed cleanly; no stress run was attempted; **r24-A2-S2+S3 efficacy remains UNKNOWN**.

**However — the literal interpretation ("the fix didn't work") is misleading.** The STARTUP-HEREDOC-LEAK fix at `a3cfca10` *did* land correctly: the systemd unit file on worker-1 is syntactically clean, no `command not found` leak markers, and `zsbx-ctl.service` is `active (running)` 2 min after boot with all expected `Environment=` lines intact. The real reason `ch` is absent from the driver list is that the **driver-install block was never executed**, because `INSTALL_CH_PLUGIN_DRIVER` defaults to `0` and the dispatch did not pass `EXTRA_WORKER_METADATA="install-ch-plugin-driver=1"` to `provision-gcp-cluster.sh`. This is a dispatch-instruction defect, not a regression in the fix.

The original r9 review (`def11cb4`) likewise omitted this metadata flag yet observed driver-side failures ("ch: Unhealthy because of failed task"). That observation is inconsistent with `INSTALL_CH_PLUGIN_DRIVER=0` semantics; the r9 conclusion that "ch driver never loaded due to heredoc leak" appears to have conflated two independent issues. Reconciliation deferred — see Diagnosis below.

## Summary

| Field | Value |
|---|---|
| Verdict | **ABORT pre-stress** (driver-list check failed per dispatch directive) |
| Heredoc fix landed? | **YES** — systemd unit clean, controller `active (running)` |
| `ch` driver in node driver list? | **NO** — only `docker, exec, java, qemu, raw_exec` |
| Root cause (this run) | `INSTALL_CH_PLUGIN_DRIVER=0` (default); dispatch missing `EXTRA_WORKER_METADATA="install-ch-plugin-driver=1"` |
| Stress cycles attempted | **0** (aborted pre-run) |
| r24-A2-S2+S3 efficacy | **UNKNOWN** — still untested |
| Cost estimate | ~$0.10 (6 instances × ≈8 min provision+probe+teardown) |
| Teardown | ALL 6 instances + 3 static IPs gone (`gcloud compute instances list --filter='name~zsbx-'` returns empty) |

## Pre-flight (all PASS)

| Gate | Expected | Actual | Result |
|---|---|---|---|
| `git rev-parse HEAD` | `a3cfca10` or newer | `a3cfca10d0b4745ec854356d53bb1daa8f7131ca` | PASS |
| `bash crates/sandbox/scripts/lint.sh` | exit 0 | exit 0, "OK — 7 script(s) clean" | PASS |
| `gsutil stat gs://…/nomad-driver-ch.v19` | exists | size=20259000, md5=PkNJdE+ZT43Fji2C1haOWQ== | PASS |
| `bash -n gcp-worker-startup.sh` | exit 0 | exit 0 | PASS |
| Heredoc-body unescaped-backtick scan (`awk` per dispatch) | no output | (no output) | PASS |

Audit-trail entry: `stress-r9-retry-2 2026-05-24T22:31:38+00:00 provision-start`.

## Setup

- **Worktree HEAD**: `a3cfca10d0b4745ec854356d53bb1daa8f7131ca` (`sandbox/scripts: escape backticks in zsbx-ctl.service heredoc (STARTUP-HEREDOC-LEAK)`).
- **Driver**: `nomad-driver-ch.v19` at `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v19` (MD5 `3e4349744f994f8dc58e2d82d6168e59`, matches r24-A2 build).
- **Controller**: `zeroship-sandbox.snapshot-v36`.
- **Cluster shape**: SERVER_COUNT=3 (n2-standard-4) + WORKER_COUNT=3 (n2-standard-32, nested-virt) @ asia-northeast3-a.
- **Provision command** (exact, per dispatch):
  ```
  SERVER_COUNT=3 WORKER_COUNT=3 bash crates/sandbox/scripts/provision-gcp-cluster.sh
  ```
  **NOTE — instructions did not set `EXTRA_WORKER_METADATA`.** The script's default is empty (`crates/sandbox/scripts/provision-gcp-cluster.sh:62`), so `install-ch-plugin-driver` was never written into the instance metadata, so `gcp-worker-startup.sh:97` resolved `INSTALL_CH_PLUGIN_DRIVER=0`, so the driver-install block at `gcp-worker-startup.sh:177-223` was skipped entirely.
- **Provision wall**: all 6 sentinels hit within ~1.5 min total.

## Pre-stress driver-list check (the gate that failed)

```bash
$ gcloud compute ssh zsbx-prod-worker-1 ... --command='nomad node status -self -json | jq -r ".Drivers | keys[]"'
docker
exec
java
qemu
raw_exec
```

Expected `ch` in the set. Got 5-element set with no `ch`. **Dispatch directive triggered: STOP, teardown, document, exit.**

## Heredoc-leak diagnosis (the fix DID work — and we can prove it)

The heredoc fix was about `gcp-worker-startup.sh:595-599`, which writes `/etc/systemd/system/zsbx-ctl.service`. After cluster boot:

1. **File content check**: `cat /etc/systemd/system/zsbx-ctl.service | grep -E "command not found|No such file"` → empty (no shell-error strings leaked from rustdoc-style backticks during heredoc expansion).
2. **systemd unit parses + runs**: `systemctl status zsbx-ctl.service` → `Active: active (running) since Sun 2026-05-24 22:35:10 UTC; 2min 22s ago`.
3. **Environment variables intact**: `systemctl show zsbx-ctl.service --property=Environment` returns the full 25-key environment block including `SANDBOX_BACKEND=nomad-ch`, `SANDBOX_SNAPSHOT_USE_GCS=true`, `SANDBOX_DRIVER_STAGES_DISK_IMAGES=true`, etc. No truncation, no shell-error contamination.
4. **Controller listens on :9091**: PID 13385 alive 2 min, no restarts.

Conclusion: `a3cfca10` fully closes STARTUP-HEREDOC-LEAK. The hypothesis in `def11cb4` ("workers booted WITHOUT the ch driver due to the heredoc") is **not what this retry observed** — and on closer reading of the original log evidence (driver-pull happened in r9 startup, allocs reached the ch driver and returned "Unhealthy"), the heredoc was almost certainly never the gating cause of the r9 RED outcome to begin with.

## Diagnosis

Two distinct issues are tangled in the dispatch chain:

1. **STARTUP-HEREDOC-LEAK (fixed)**. Real bug. Real fix. Verified. Not the cause of any observable r9 stress failure based on this retry's evidence.

2. **Dispatch-instruction defect (current blocker)**. Both r9 and r9-retry-2 dispatch templates omit `EXTRA_WORKER_METADATA="install-ch-plugin-driver=1"`. Without it the driver is never even pulled from GCS, much less registered with Nomad. To exercise r24-A2-S2+S3 (binding-wedge fix) the next dispatch MUST set this env var on the `provision-gcp-cluster.sh` invocation.

   - On r9: the review reports failures consistent with `ch` being registered (alloc dispatch produced "ch: Unhealthy because of failed task" not "no driver named ch"). That is incompatible with `INSTALL_CH_PLUGIN_DRIVER=0`. Either (a) r9 silently exported `EXTRA_WORKER_METADATA` from a shell env that retry-2 lacked, or (b) the r9 failure was actually a different driver path. Either way, the documented dispatch invocation is insufficient.

3. **r9-review verdict accuracy** also needs revisiting: the conclusion that heredoc-leak gated r9 is not supported by retry-2 evidence. A separate write-up is out of scope for this run.

## Verdict on r24-A2-S2+S3 efficacy

**Still UNKNOWN.** The binding-wedge fix was not reached because the driver itself was not installed on the workers. r24-A2-S2+S3 remains neither confirmed nor refuted.

## Required next-dispatch correction

The next stress retry MUST use:

```bash
EXTRA_WORKER_METADATA="install-ch-plugin-driver=1" \
SERVER_COUNT=3 WORKER_COUNT=3 \
  bash crates/sandbox/scripts/provision-gcp-cluster.sh
```

Failing that, every retry will repeat this abort.

Alternative: flip the default in `provision-gcp-cluster.sh:62` to `EXTRA_WORKER_METADATA=${EXTRA_WORKER_METADATA:-install-ch-plugin-driver=1}`. This is a 1-line script change but per dispatch constraint ("DO NOT touch source code or scripts/*") it is **out of scope for this retry**.

## Teardown

- `bash crates/sandbox/scripts/teardown-gcp-cluster.sh` → all 6 instances deleted + 3 internal IPs released.
- `gcloud compute instances list --filter='name~zsbx-'` → empty.

Audit-trail entry: `stress-r9-retry-2 2026-05-24T<ISO>Z ABORT teardown-complete`.

## Cost

~$0.10 estimated (6 instances × ~8 min total cluster lifetime). Well under the $30 hard cap.
