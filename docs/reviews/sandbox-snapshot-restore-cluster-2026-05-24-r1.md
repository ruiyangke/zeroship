# Phase B cluster validation — 2026-05-24 r1

**Branch HEAD:** `164d346a` — `sandbox/config: alloc_running_timeout_secs 60→120 default (T3)`
  (note: prompt referenced HEAD `b048b491` for this cycle; branch advanced one commit before
  the cluster run began. `b048b491` and the B15 fix `eaf5ea83` are both in history.)
**Controller binary uploaded:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v12`
  (14_540_984 bytes; built from HEAD `164d346a` on this NixOS host)
**B15 fix validation:** **NOT REACHED** — blocked by new bug #16 (see below)
**Operator:** pilot-cron-worker autonomous

## TL;DR

Cluster validation **aborted before any sandbox API call could execute**. The
v12 controller binary uploaded to GCS is dynamically linked against a Nix-store
ELF interpreter that does not exist on the GCE Ubuntu worker image, so
`/usr/local/bin/zeroship-sandbox` immediately fails with `cannot execute:
required file not found` (kernel's misleading text for missing
`PT_INTERP` loader). Smoke FAIL, stress not attempted. Teardown clean.
B15 remains **unverified** on a real cluster.

## Phase 1: 1+1 smoke (c=1)

- Provision time: **150 s** (sentinel hit at server=60s, worker=90s)
- Cycle outcome: **FAIL** — never executed; controller never reached `/livez`
- create_ms / snapshot_ms / wake_ms / stop_ms: **n/a** (controller never started)
- Notable journal lines (verbatim, from worker `zsbx-smoke-worker-1`):

  Startup-script line 166 (the `--help` sanity check), 2026-05-23 03:15:09 UTC:
  ```
  /tmp/metadata-scripts3477209354/startup-script: line 166:
    /usr/local/bin/zeroship-sandbox: cannot execute: required file not found
  ```

  `zsbx-ctl.service`, 03:15:10 UTC:
  ```
  zsbx-ctl.service: Failed to execute /usr/local/bin/zeroship-sandbox: No such file or directory
  zsbx-ctl.service: Failed at step EXEC spawning /usr/local/bin/zeroship-sandbox: No such file or directory
  zsbx-ctl.service: Main process exited, code=exited, status=203/EXEC
  zsbx-ctl.service: Failed with result 'exit-code'.
  ```

  (Despite the kernel's "No such file or directory" wording, `stat
  /usr/local/bin/zeroship-sandbox` showed a regular file of size
  14_540_984 with `Birth: 2026-05-23 03:15:03 +0000` — the file is
  there; what is missing is its `PT_INTERP` loader.)

## Phase 2: 3+5 stress (c=20)

**Not attempted.** Per pre-flight plan, Phase 2 was conditional on Phase 1
PASS. Phase 1 FAILed at controller-start; provision-cost-and-no-evidence
was not worth re-spending on a 3+5 escalation with the same artifact.

## SLO comparison

- Wake p50 target ≤ 1.0 s (proposal § 10.2); **observed: n/a — no wake executed**
- Cold-boot p50 baseline 4243 ms; **wake speedup not measurable this cycle**
- All targets: **FAIL by no-data**

## B15 fix verification

- Before fix (per `2026-05-23-r1.md`): 0/N wake successes due to `host_dir` wipe
- After fix (this run): **UNKNOWN** — could not reach the wake path
- Evidence: wrapper `[wrapper] FATAL: workspace image missing` count: **0** (wrapper never invoked; no Nomad alloc started)

The B15 fix `eaf5ea83` (`stop_preserving_state` for snapshot teardown) is
in branch history and `b048b491` adds a pg-gated integration test that
exercises it locally. Cluster-level confirmation still pending.

## Teardown

- `zsbx-smoke` prefix torn down: **yes** (instances `zsbx-smoke-server-1`,
  `zsbx-smoke-worker-1` deleted; reserved IP `zsbx-smoke-server-1-ip` released)
- `zsbx-stress` prefix torn down: **n/a** (never provisioned)
- Residual instances matching `^zsbx-`: **0** (verified via
  `gcloud compute instances list --filter='name~"^zsbx-"'`)

## Estimated cost

- Provision + smoke + teardown wall-clock: **~13 minutes**
  (server boot 03:14, worker startup 03:13:28–03:16:10, teardown begin
   03:23:45 PDT = 03:23:45+07h = 10:23:45 UTC; running ~10 min after
   worker boot)
- n2-standard-4 server @ ~$0.17/hr × 13 min ≈ **$0.04**
- n2-standard-32 worker (nested-virt) @ ~$1.55/hr × 13 min ≈ **$0.34**
- **Total: ~$0.38** (well under the $30 cap)

## NEW BUG ENTRY — bug #16: controller binary is Nix-store-linked, unrunnable on GCE Ubuntu

**Status:** open, blocks all GCP cluster validation cycles until fixed.
**Found in:** 2026-05-24 r1 cluster run.
**Symptom:** `zsbx-ctl.service` exits 203/EXEC on a fresh GCE worker; the
startup script's pre-systemd `--help` sanity check also dies with
`cannot execute: required file not found`. The file IS on disk.

**Verbatim diagnosis:**
```
$ file target/release/zeroship-sandbox
target/release/zeroship-sandbox: ELF 64-bit LSB pie executable, x86-64,
  version 1 (SYSV), dynamically linked,
  interpreter /nix/store/jms7zxzm7w1whczwny5m3gkgdjghmi2r-glibc-2.42-51/lib/ld-linux-x86-64.so.2,
  for GNU/Linux 3.10.0, not stripped

$ ldd target/release/zeroship-sandbox
        linux-vdso.so.1
        libc.so.6 => /nix/store/jms7…/lib/libc.so.6
        /nix/store/jms7…/lib/ld-linux-x86-64.so.2 => /nix/store/jms7…/lib64/ld-linux-x86-64.so.2
        libgcc_s.so.1 => /nix/store/ab37…/lib/libgcc_s.so.1

$ readelf -l target/release/zeroship-sandbox | grep -A1 INTERP
  INTERP   0x350 0x350 0x350 0x53 0x53  R  0x1
   [Requesting program interpreter:
      /nix/store/jms7zxzm7w1whczwny5m3gkgdjghmi2r-glibc-2.42-51/lib/ld-linux-x86-64.so.2]

$ cat /etc/os-release | head -2
ANSI_COLOR="0;38;2;126;186;228"
BUILD_ID="25.11.20260510.8fd9daa"
```

The local build host is NixOS 25.11. `cargo build --release` under Nix
produces binaries hard-coded to the Nix-store `ld-linux-x86-64.so.2`
plus Nix-store glibc / libgcc — none of which exist on the GCE Ubuntu
worker image. The kernel reports this as ENOENT on `execve`, which
both bash ("cannot execute: required file not found") and systemd
("No such file or directory") propagate, hiding the real cause.

**Why this slipped past prior cluster cycles:**
- v6, v11 and earlier objects in the bucket were uploaded by a build
  that did NOT have the Nix-store interpreter — likely from a different
  host (Linux container? CI?) or via `patchelf --set-interpreter` /
  `nix-build`-style RUNPATH wrapping. v11 worked end-to-end at the
  controller-process level (it just hit bug #15 on wake).
- v12 is the first object built directly on the NixOS dev host. The
  upload step (`gsutil cp target/release/zeroship-sandbox`) does not
  validate portability.

**Possible fixes (do not implement this cycle — pilot decision):**
1. Build the controller inside a glibc-2.31/2.35 chroot or Docker image
   (e.g., `rust:1.XX-bookworm`) for cluster uploads; bake into a
   `make controller-cluster` target.
2. `patchelf --set-interpreter /lib64/ld-linux-x86-64.so.2 --remove-rpath
   target/release/zeroship-sandbox` post-build (works if it only needs
   glibc symbols ≤ the GCE image's glibc version — not guaranteed
   since the Nix glibc is 2.42 and Ubuntu 24.04 LTS ships 2.39).
3. Switch the cluster artifact to a `musl` static build:
   `cargo build --release --target x86_64-unknown-linux-musl -p zeroship-sandbox --bin zeroship-sandbox`.
   This is the most robust option; binary becomes a single static ELF.
4. Add a sanity-gate to `provision-gcp-cluster.sh`: before `gsutil cp`,
   run `readelf -l <binary> | grep INTERP` and refuse if it contains
   `/nix/`. Cheap pre-flight that would have caught this in seconds.

**Recommendation:** fix (3) (`x86_64-unknown-linux-musl`) is the
canonical answer for "ship a sandbox controller to a heterogeneous
fleet"; fix (4) is the cheap guard regardless. Both should land before
any new GCP attempt.

## Side observations (not new bugs)

- Provision script `provision-gcp-cluster.sh` and `gcp-worker-startup.sh`
  in the working tree have uncommitted edits that bump default
  `CONTROLLER_OBJECT` from `snapshot-v6` to `snapshot-v11` and switch
  rootfs to `rootfs-slim.img.virtio-blk-v3`. The cycle override
  `CONTROLLER_OBJECT=zeroship-sandbox.snapshot-v12` was passed via env,
  so this had no effect on the run — but the defaults are now stale.
- `zsbx-ctl.service` is `Restart=no`. With bug #16 fixed the unit
  starts fine, but pilot may want to consider `Restart=on-failure` +
  `RestartSec=10s` to ride out transient gs_pull / DB races.

## Files of interest

- `/tmp/provision-smoke.log` — full provision transcript (this run)
- `/tmp/worker-status.log`, `/tmp/worker-debug.log`,
  `/tmp/worker-zsbx-ctl-journal.log` — verbatim worker journals
- `/tmp/teardown-smoke.log` — teardown transcript
- `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v12` — broken
  binary, kept on bucket for forensic / fix-validation
- Local: `target/release/zeroship-sandbox` (Nix-linked, do not re-upload)

---

## Appendix A — bug #16 fix attempt + re-run (2026-05-24 r2)

**Build approach:** Docker cross-build in `rust:slim-bookworm` image
(note: brief said `rust:bookworm-slim`, that tag does not exist on
Docker Hub; the correct tag is `slim-bookworm`). Build invocation:

```bash
docker run --rm \
  -v /home/ruiyang/Projects/appbase:/work \
  -w /work/.worktrees/sandbox-snapshot-restore \
  -e CARGO_TARGET_DIR=/work/.worktrees/sandbox-snapshot-restore/target/docker-build \
  rust:slim-bookworm bash -c '
    set -e
    apt-get update -qq && apt-get install -y -qq pkg-config libssl-dev clang git
    cargo build --release -p zeroship-sandbox --bin zeroship-sandbox
  '
```

First-pass build SUCCESS on first try (no iteration needed). Compile
time inside container: ~3 min (apt) + 49.54s (cargo) = ~4 min wall.

**Binary verification:**

```
readelf -p .interp target/docker-build/release/zeroship-sandbox
  → /lib64/ld-linux-x86-64.so.2     ← Debian 12 / Ubuntu standard
file target/docker-build/release/zeroship-sandbox
  → ELF 64-bit LSB pie executable, x86-64, version 1 (SYSV),
    dynamically linked, interpreter /lib64/ld-linux-x86-64.so.2,
    for GNU/Linux 3.2.0
size: 15,676,656 bytes
```

Bug #16 root cause (Nix-store PT_INTERP) is **resolved** by this
build approach.

**Uploaded:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v13`
(15.0 MiB, 2026-05-23T03:41:49Z).

### Smoke (1+1, c=1) — PARTIAL PASS

Cluster: `zsbx-smoke`, 1 server (n2-standard-4) + 1 worker
(n2-standard-32), `asia-northeast3-a`. Provisioned with
`CONTROLLER_OBJECT=zeroship-sandbox.snapshot-v13` via env override.

- **Provision:** OK. Server sentinel hit in 60s, worker sentinel in 15s.
- **Controller start:** **OK.** `systemctl is-active zsbx-ctl` →
  `active`, `curl http://127.0.0.1:9091/livez` → `{"status":"ok"}`.
  Controller stayed up 19+ minutes with no restarts. **Bug #16
  verification: PASS.**
- **Cycle outcome:** **FAIL** at wake step.
- **Timings:**
  - `create_ms`: 5295 (HTTP 201)
  - `exec_pre_ms`: 8.6 (HTTP 200)
  - `snapshot_ms`: 47794 (HTTP 200; artifact bytes=1073847160 = 1.07 GB,
    sha256=`1ef33f…6764c`, ch-remote v51.1)
  - `wake_ms`: 37422 (**HTTP 500**)
  - `exec_post`, `stop_ms`: skipped (depend on wake)
- **B15 verification:** **N/A this cycle.** Wake reached the restore
  step but failed at the post-restore agent-liveness probe (a different
  failure mode than B15). Cannot confirm B15 PASS until the new wake
  bug is fixed.
- **Wake failure (verbatim):**

  ```
  {"error":"restore_backend: restore: agent at http://10.99.101.2:7777
   never returned 200 on /livez (last=http://10.99.101.2:7777/livez:
   Connection Failed: Connect error: No route to host (os error 113))"}
  ```

- **Controller journal (verbatim, full):**

  ```
  May 23 03:44:20 zsbx-smoke-worker-1 systemd[1]: Started zsbx-ctl.service - zeroship-sandbox controller.
  ```

  That is the **only** line. The controller process is running
  (`Main PID 13343 (zeroship-sandbo)`, 19min uptime, 1.2G memory) but
  emits nothing to stdout/stderr (systemd journal capture). Either
  logging is disabled, going to a file, or buffered. Worth investigating
  separately (low priority — not a blocker).

- **Nomad activity:**
  - 1st alloc `ade5a44b-…` (CREATE+SNAPSHOT): completed normally,
    GC'd via SIGINT after snapshot.
  - 2nd alloc `4dde790b-…` (WAKE/RESTORE): started 04:03:20Z, GC'd
    via SIGINT 32s later at 04:03:52Z. Exit code 130 = SIGINT (the
    controller cancelled the alloc after the liveness-probe timeout).
  - Both alloc stderr files already GC'd by the time of diagnostic
    capture (controller GC ran on stop).

### New bug — #17: agent unreachable after CH restore

- **Source:** smoke c=1 (this appendix).
- **Symptom:** After `ch-remote restore` succeeds and the VM resumes,
  the controller probes `http://<vm-ip>:7777/livez` and gets
  "No route to host (os error 113)" until the per-cycle wake timeout
  (~37s).
- **Hypothesis (not verified):**
  1. The taprt/network namespace for the restored VM is not re-attached
     correctly post-restore — VM is up, but its tap is unbridged.
  2. The `nomad-vm-wrapper.sh` `restore` branch may be missing a
     `tap up` / `ip link set zsbx-<idx> master zsbx-br0` step that
     `start` has but `restore` doesn't.
  3. The IP `10.99.101.2` falls in the platform's `vm-net` /10 — if
     the wrapper restore-branch only does `cloud-hypervisor restore`
     without re-asserting tap/bridge attachment, traffic has no path.
  4. Alternative: the agent inside the VM is bound to the original
     network interface that may not exist post-restore (rare but
     possible if CH `restore` re-assigns the tap to a different
     ifname).
- **Why NEW (not B15):** B15 was "wake reaches restore_backend and
  the restore_backend call fails (snapshot artifact issue)". This is
  "restore_backend call succeeds, but the resulting VM is on a tap
  with no path to the controller". Different layer.
- **Status:** captured here; **do not implement this cycle** per
  pilot constraints. Next cycle handles.
- **Inputs to next cycle:**
  - `crates/sandbox/scripts/nomad-vm-wrapper.sh` — diff `start` vs.
    `restore` branches for tap/bridge setup.
  - `crates/sandbox/src/backend/nomad_ch.rs` — does the restore path
    re-emit the tap interface name into the alloc env?
  - The 30+ second probe timeout is also worth tightening (or
    immediate-retry-on-EHOSTUNREACH) so the failure surfaces fast.

### Stress (3+5, c=20) — NOT RUN

Smoke failed at wake; per the brief ("If smoke fails: tear down,
document, exit"), stress phase skipped. Stress can re-run once #17
is fixed (no need to rebuild controller — v13 is portable).

### SLO comparison

- Wake p50 target ≤ 1.0 s; observed: **N/A (all failed).**
- Create p50: 5295 ms (target unknown for this phase; reference only).
- Snapshot p50: 47794 ms (~48s for a 1.07 GB artifact; equivalent to
  ~22 MB/s effective snapshot throughput).
- **Verdict: smoke FAIL (wake unusable).** Re-run blocked on #17.

### Cost estimate

- Smoke cluster: 1 × n2-standard-4 + 1 × n2-standard-32, ~20 min total.
- n2-standard-32 ≈ $1.55/hr, n2-standard-4 ≈ $0.19/hr.
- Total: ≈ (1.55 + 0.19) × (20/60) ≈ **$0.58**.
- Well under the $30 cap. ~$29 remaining budget for next cycle.

### Teardown

```
[teardown] OK: cluster fully torn down
remaining instances matching ^zsbx-: 0
```

No stragglers. Verified via `gcloud compute instances list
--filter='name~"^zsbx-"'` (empty).

### Files of interest (r2)

- `/tmp/docker-build.log` — Docker cross-build transcript
- `/tmp/provision-smoke-v13.log` — provision run with v13
- `/tmp/smoke-result-v13.log` — smoke 1-cycle output incl. RAW_JSON
- `/tmp/smoke-controller-state.log`, `/tmp/smoke-deep-diag.log` —
  worker journal + systemctl + alloc state
- `/tmp/teardown-smoke.log` — clean teardown
- `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v13` —
  portable controller, ready for re-use after #17 is fixed.

