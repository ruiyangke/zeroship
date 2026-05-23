# Sandbox snapshot-restore cluster diagnostic — 2026-05-23 round 1

**Cycle ID**: 2026-05-23 r1 (bug-#14a/b cluster diagnostic round)
**Worktree HEAD**: `8ad3cf3f` (sandbox: diagnostic logging + tap-up retry on restore path)
**Cluster**: SMOKE 1+1 (`zsbx-prod-server-1`, `zsbx-prod-worker-1`, `asia-northeast3-a`)
**Snapshot binary**: `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v11`
**Wrapper**: `gs://suger-dev-zsbx-artifacts/nomad-vm-wrapper.sh` (committed restore-branch diagnostics + tap-up retry)
**Rootfs**: `gs://suger-dev-zsbx-artifacts/rootfs-slim.img.virtio-blk-v3`

## Outcome

**A new failure shape surfaced — call it bug #15.** The B14a/B14b
hypotheses are both refuted by evidence; the wake failures are caused
by `teardown_source_for_snapshot` `rm -rf`ing the per-sandbox
`host_dir` (which contains the workspace.img the restore alloc later
needs), then the restore wrapper's pre-CH `[ -f $ZSBX_WORKSPACE_IMG ]`
gate trips with exit 1 before any CH-restore work begins.

Per discipline rule "If a NEW (bug #15) shape surfaces, document it
and STOP" → no fix applied this cycle. Cluster torn down. Bug-#15
entry appended to `sandbox-snapshot-restore-deferred.md` for the next
cron cycle.

## Smoke results

Two stress runs, both showing the same wake failure mode:

```
Run 1 (concurrency=4, cycles=1, total=4):
  CREATE   OK: 4/4
  SNAPSHOT OK: 3/4   (1× timeout on snapshot endpoint)
  WAKE     OK: 0/3   ← all 3 wakes returned 500
  POST-EXEC OK: 0/0
  STOP     OK: 0/4
  Failure: "restore_backend: nomad alloc terminal status=failed: Failed tasks"

Run 2 (concurrency=1, cycles=1, after wrapper stderr-tee patch):
  CREATE   OK: 1/1
  SNAPSHOT OK: 1/1
  WAKE     OK: 0/1   ← still failing
  Failure: identical
```

## Diagnostic evidence — controller side (verbatim)

The controller's `restore: post-store.get staged files` tracing
(landed in commit `8ad3cf3f` for this cycle) prints file sizes
immediately after `store.get` returns. All three runs show the same:

```
{"timestamp":"2026-05-23T02:28:20.644847Z","level":"INFO",
 "message":"restore: post-store.get staged files",
 "sandbox_id":"019e52a8-5e2b-76a2-a4e1-b182c8c4aa64",
 "alloc_dir":"/var/zeroship/ch/019e52a85e2b76a2a4e1b182c8c4aa64/restore",
 "stat":"config.json=2804 bytes, memory-ranges=1073741824 bytes, state.json=102556 bytes"}

{"timestamp":"2026-05-23T02:28:21.176433Z","level":"ERROR",
 "message":"sandbox/admin","status":500,
 "error":"restore_backend: nomad alloc terminal status=failed: Failed tasks"}
```

**Reading**: store.get staged all three files correctly
(config.json + 1 GiB memory-ranges + state.json). Failure happens
~530 ms later via Nomad's "Failed tasks" terminal-status surfaced by
`wait_for_alloc_running`. So **H2 (files never staged) is refuted**.

## Diagnostic evidence — wrapper side (verbatim)

The committed `[wrapper] restore: …` echoes don't appear in
journalctl because Nomad's `raw_exec` driver routes the wrapper's
stderr to a per-alloc `task.stderr.0` under
`/opt/nomad/data/alloc/<alloc-uuid>/`, and the alloc dir is GC'd
within milliseconds of task completion. To capture the output we
patched the live wrapper on the worker to also `tee -a
/var/log/zsbx-wrapper.log`, then re-ran a single cycle:

```
$ sudo cat /var/log/zsbx-wrapper.log
[wrapper] === task start pid=14849 NOMAD_TASK_DIR=/opt/nomad/data/alloc/904d9368-bfd4-963f-916d-5ef9d54d463d/ch/local ZSBX_RESTORE_FROM=? ===
[wrapper] FATAL: command failed at line 418 (exit=130)
[wrapper] === task start pid=14954 NOMAD_TASK_DIR=/opt/nomad/data/alloc/edb02118-2eeb-195d-72d9-029aa4dc6c0e/ch/local ZSBX_RESTORE_FROM=/var/zeroship/ch/019e52ac466075f2a3a3aa6505ae3766/restore ===
[wrapper] FATAL: workspace image missing: /var/zeroship/ch/019e52ac466075f2a3a3aa6505ae3766/workspace.img
```

(The first FATAL exit-130 line is the snapshot's source alloc reacting
to controller SIGINT during teardown — expected; the second is the
restore alloc bailing in the workspace.img check at
`crates/sandbox/scripts/nomad-vm-wrapper.sh:222-225`.)

The wrapper exits 1 here, **before** any restore-branch instrumentation
(the file-presence check is upstream of line 308 where
`ZSBX_RESTORE_FROM`-branch instrumentation lives).

## Diagnostic evidence — host_dir layout (verbatim)

The worker's `/var/zeroship/ch/` tree after the smoke runs:

```
/var/zeroship/ch
├── 019e52a85e2b76a2a4e1b182c8c4aa64/   <— sandbox A: only `restore/` (no workspace.img)
│   └── restore/{config.json, memory-ranges, state.json}
├── 019e52a85e2b76a2a4e1b176cdc96f50/   <— sandbox B: only `restore/`
│   └── restore/{...}
├── 019e52a85e2b76a2a4e1b16f8ef30643/   <— sandbox C: only `restore/`
│   └── restore/{...}
├── 019e52a85e2b76a2a4e1b19c89ad4940/   <— sandbox D (snapshot TIMED OUT)
│   └── workspace.img                    ← STILL PRESENT — teardown never ran
├── 019e52ac466075f2a3a3aa6505ae3766/   <— smoke2 sandbox
│   └── restore/{...}
├── snapshots/
│   ├── sbx_<...>/  × 4   ← L1 store entries, all present
│   └── snap-stage/
└── users/usr_<...>/home.img × 5         ← per-user homes intact
```

The only sandbox host_dir that *retains* `workspace.img` is the one
whose snapshot **timed out** (the controller bailed before reaching
`teardown_source_for_snapshot`, so `stop()` and its
`remove_dir_all(host_dir)` never ran). Every other sandbox lost its
`workspace.img` to the post-snapshot teardown.

## Root cause (bug #15)

`crates/sandbox/src/admin_handlers.rs::snapshot_sandbox`, post-success
branch (line 1196-1206):

```rust
if let Err(e) = state
    .backend
    .teardown_source_for_snapshot(sandbox_id)
    .await
{
    tracing::warn!(...);
}
```

`teardown_source_for_snapshot` in `crates/sandbox/src/backend/mod.rs:407-419`
delegates to `b.stop(sandbox_id)` for the nomad-ch backend. `stop()`
in `crates/sandbox/src/backend/nomad_ch.rs:879..` runs the full
de-allocation including step 5 at lines 1063-1077:

```rust
// 5. Remove per-sandbox host dir. Per-user home dir is
//    intentionally **not** touched. ...
let host_dir_safe_to_rm = job_confirmed_gone && fence_passed;
if host_dir_safe_to_rm && sandbox.host_dir.exists() {
    if let Err(e) = std::fs::remove_dir_all(&sandbox.host_dir) {
        errs.push(...);
    }
}
```

`sandbox.host_dir` is `<host_state_dir>/<sandbox-uuid>/`, which is
the SAME directory that holds `workspace.img` (created at
`crates/sandbox/src/backend/nomad_ch.rs:660-663`:

```rust
let workspace_img = workspace_image_path(host_dir);
create_ext4_image_if_missing(&workspace_img, workspace_img_size_gb)?;
```

So the snapshot success path **destroys the workspace.img** as a
side-effect of reusing the full `stop()` flow for source teardown.
On wake/restore, the controller stages the snapshot into
`<host_dir>/restore/` (re-creating `<host_dir>` along the way) but
doesn't re-materialize `workspace.img`. The wrapper's defensive
`[ ! -f $ZSBX_WORKSPACE_IMG ]` check
(`crates/sandbox/scripts/nomad-vm-wrapper.sh:222-225`) catches the
absence and aborts.

## Why this wasn't caught earlier

- The virtio-blk pivot (commit landing in `feat/sandbox-snapshot-restore`)
  moved workspace storage from a virtiofs-mounted directory into a
  per-sandbox raw ext4 image under `host_dir`. Pre-pivot, the host_dir
  held only a virtiofsd socket which CH re-attaches on resume; losing
  it on teardown didn't matter because restore creates a fresh socket
  via a fresh virtiofsd.
- `teardown_source_for_snapshot` was implemented as "just call
  `stop()`" before the pivot, when host_dir was empty of restore-
  critical state.
- The snapshot/restore unit tests use `tempfile`-based stubs that
  don't exercise the `host_dir`-wipe interaction.
- Both bug-#14a (suspected "staging dir empty at wake") and bug-#14b
  (suspected "tap NO-CARRIER post-restore") were red herrings layered
  on top — the wrapper never gets far enough to demonstrate either.

## Hypotheses resolution

| Hypothesis                                            | Status     | Evidence |
|-------------------------------------------------------|------------|----------|
| **H1**: files staged but disappear before wrapper exec | Refuted   | Controller post-store.get logs 1 GiB memory-ranges + config + state staged successfully ~500 ms before the wake fails; the staged dir is still on disk after the failure (verified `ls -la /var/zeroship/ch/<id>/restore/`). |
| **H2**: store.get returns Ok without writing          | Refuted   | Same evidence — files are observably present + sized correctly. |
| **bug #15** (new): teardown wipes workspace.img       | **Confirmed** | Wrapper stderr: `[wrapper] FATAL: workspace image missing: /var/zeroship/ch/<id>/workspace.img`. Host_dir listing shows zero workspace.img files for any sandbox whose snapshot+teardown completed; the one sandbox whose snapshot timed out (teardown not reached) STILL has its workspace.img. Controller path: `snapshot_sandbox → teardown_source_for_snapshot → stop() → remove_dir_all(host_dir)` (`crates/sandbox/src/backend/nomad_ch.rs:1071`). |

## Recommended fix shapes (for the next cron cycle)

Two viable strategies, both well-scoped:

**Option A — selective teardown**. Add a new
`teardown_source_for_snapshot`-specific path on `NomadChBackend` that
runs steps 1-4 of `stop()` (Nomad purge + fence + vm_index release)
but **skips step 5's `remove_dir_all(host_dir)`**. The per-sandbox
dir survives so a later restore can re-attach to its `workspace.img`.
On final `stop` (when the sandbox row transitions terminal-not-
restorable), the host_dir gets removed then. Symmetric with how
`home.img` is intentionally not touched.

- Pro: minimal blast radius, doesn't change the cold-boot path or the
  restore-create path.
- Con: introduces a third "kind" of stop path; needs an explicit
  reaper for sandboxes that ARE snapshotted but never woken (orphan-
  prune sweep on next boot already handles this — check it touches
  host_dirs of `snapshotted` rows correctly).

**Option B — re-materialize workspace.img on restore**. Have
`build_restore_nomad_job_json` (or `restore_handler::restore_sandbox`
step 4.5) recompute `workspace_img = host_dir.join("workspace.img")`
and re-create the empty ext4 image if missing. This relies on the
snapshot already capturing the workspace state inside the VM's RAM
+ disk fs cache, but the **persistent disk content** (anything not in
the page cache at snapshot time) would be lost.

- Pro: even simpler — single call to `create_ext4_image_if_missing`
  in the restore path.
- Con: **silently loses on-disk workspace data**. Any creator who
  wrote files to /workspace, snapshotted, then woke would find an
  empty /workspace. That defeats the entire point of workspace as
  persistent storage.

**Recommendation**: Option A. Treat the workspace.img as state we
must preserve across the snapshot→wake gap, the same way we preserve
the per-user home.img.

A third "fix in scripts" path — make the wrapper recreate the image
on restore — is rejected for the same reason as Option B (data loss).

## Action this cycle

- **Cluster smoke**: 1+1 provisioned, drove 4+1 cycles, captured all
  three diagnostic streams (controller post-store.get tracing,
  wrapper restore-branch stderr via live-patched tee, host_dir
  layout). Cluster torn down post-capture.
- **Fix applied**: **none**. Per discipline ("if a NEW bug #15 shape
  surfaces, document it and STOP"), this cycle ends at diagnosis.
- **Diagnosis written**: this file.
- **Deferred backlog updated**: bug-#15 entry added to
  `docs/reviews/sandbox-snapshot-restore-deferred.md`. Bug-#14a/b
  entries demoted from CRITICAL — they may yet exist as second-order
  failures behind #15, but neither was empirically reproducible this
  cycle and there's no evidence either is independently blocking.
- **Cost this cycle**: ~$1.50 (1 server-VM 4-vCPU + 1 worker-VM
  32-vCPU + nested-virt licence × ~12 min).
