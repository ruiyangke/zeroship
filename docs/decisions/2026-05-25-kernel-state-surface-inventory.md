# ADR — Kernel-State Surface Inventory: Per-Surface Sweepers Gate Wrapper Retirement

- **Date:** 2026-05-25
- **Status:** Accepted
- **References:**
  - Architecture reviews `r24-A2`, `r25-A2` under `docs/reviews/sandbox-snapshot-restore-architecture-2026-05-25-r{24,25}.md`
  - Cluster reviews `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-stress-{r1,r2}.md`
  - Sibling ADR: `docs/decisions/2026-05-25-restore-debug-playbook.md` (stress-as-gate, observability-before-architecture)
  - Commits: `177ff165` (driver v14 defensive tap cleanup + `taps_orphaned_total` counter),
    `d638b10f` (controller v34 — leak host_dir on stop, verbatim driver-msg propagation),
    `e82bffd7` (controller v34 — `run_host_dir_gc_once` sweeper task),
    `c729c2b8` (v14 driver + v34 controller pin bump for T-8b-stress-r3),
    `30960451` (v33 controller-side disk-image preflight — defense-in-depth, mechanism refuted by stress-r2),
    `3d03cb90` (driver v13 init-time tap pre-delete on EEXIST)

## Context

T-8b-cutover is the one-way migration that retires `crates/sandbox/scripts/nomad-vm-wrapper.sh` and
flips `SANDBOX_TASK_DRIVER=ch_plugin` to the default. The wrapper has historically owned **deterministic,
comprehensive** teardown of every kernel-state surface a sandbox VM creates: `ip link del` for the tap,
`rm -rf` for the per-sandbox host_dir, implicit cgroup teardown via raw-exec exit, mount-point
implicit-cleanup via process exit. The Go driver (`nomad-driver-ch`) replicates this surface-by-surface
in `DestroyTask`, plus init-time recovery in `StartTask`.

Stress-r1 and stress-r2 surfaced two kernel-state leaks that the wrapper handled implicitly but the Go
driver did not:

1. **host_dir leak / cleanup-vs-retry race** (Bug 1, 49/60 CREATE failures at stress-r1). Closed at
   v34 (`d638b10f` + `e82bffd7`) by moving cleanup to a sweeper-owned 5-min GC with a 1-hour mtime
   grace.
2. **tap interface leak** (Bug 2, 9 stranded `zsbx-nm-<idx>` interfaces per worker post-stress-r1).
   Half-closed at v13 (`3d03cb90`, init-time pre-delete on EEXIST); fully closed at v14 (`177ff165`,
   defensive vm_index-keyed cleanup in `DestroyTask` plus `taps_orphaned_total` counter).

Both surfaces follow the same pattern: a stranded resource from a prior alloc's incomplete teardown is
silently re-used by the next alloc with stale config, and the failure surfaces mid-spawn at an
unrelated layer (CH exits -1 mid-resume; preflight stat returns ENOENT).

The architectural question is not whether v14 + v34 close those two surfaces — they do. The question is
**what other kernel-state surfaces does the driver own, and what is the closure path per surface?**
Without an enumeration, "wrapper retirement structurally unblocked" reduces to "the next stress cycle
flushes whichever surface the wrapper still cleans implicitly." Three consecutive cycles (r19-A1,
r20-A1, r23-A2) have made that claim; all three were refuted by the next stress cycle.

## Decision

**Per-surface sweeper pattern, not unified reaper.** Each kernel-state surface gets its own
EEXIST-safe re-entry path in the driver's `StartTask` + its own teardown path in `DestroyTask` + (where
applicable) its own controller-side sweeper task that mirrors `spawn_host_dir_gc` (`e82bffd7`).

**Rationale.** Each surface has a different reap mechanism and a different grace policy:

| Surface | Reap mechanism | Grace policy |
| --- | --- | --- |
| tap | `ip link del` | instant (no live process holds a handle) |
| host_dir | `remove_dir_all` | mtime ≥ 1h AND no pending wake_job AND terminal sandbox state |
| cgroup | rmdir (or systemd) | refcount-aware (process must have exited) |
| mount-ns | umount + rmdir | mount-aware (umount before rm; partial-failure means leave) |
| vsock CID | kernel auto-clean | implicit (no API to enumerate or force) |
| jailer chroot | rmtree | refcount-aware (chroot must not be entered) |
| PID files | rm | best-effort (stale PID is fine; rm is for hygiene) |

A unified reaper would force cross-surface ordering dependencies (umount BEFORE rmdir, kill process
BEFORE cgroup rmdir, drain socket BEFORE chroot rmtree) that change failure semantics under partial
failure. The per-surface sweeper inherits the simpler invariant: each sweeper runs independently, each
emits its own orphaned/reaped counter pair, each lives or dies on its own enumeration mechanism.

This is the **shape v34's `run_host_dir_gc_once` already takes** (`e82bffd7`). The decision here is to
commit to the pattern as the future-surface mould rather than backfill a unified reaper.

## Kernel-state surface inventory

| Surface | Where created | Where cleared today | EEXIST-safe re-entry? | Sweeper-replicable? | Status |
| --- | --- | --- | --- | --- | --- |
| **tap** (`zsbx-nm-<idx>`) | driver `setupTapForVM` | driver `DestroyTask::removeTapFn` + v14 defensive vm_index-keyed cleanup | YES — driver v13 pre-deletes on EEXIST (`3d03cb90`) | YES — orphan-scan `ip link show type tun`, filter `zsbx-nm-*` not in active set | **CLOSED** (driver v14 at `177ff165`; `taps_orphaned_total` counter) |
| **host_dir** (`/var/zeroship/ch/<sid>/`) | controller `create_ext4_image_if_missing` | controller `sweep::run_host_dir_gc_once` (5-min poll, 1-hour mtime grace) | YES — controller skips create if path exists, re-uses contents | YES — sweeper-owned with terminal-state + no-pending-wake gates | **CLOSED** (controller v34 at `d638b10f` + `e82bffd7`) |
| **cgroup** (Nomad-managed) | Nomad alloc setup (raw-exec inheritance) | Nomad alloc teardown (implicit on process exit) | UNKNOWN — driver assumes Nomad cleans, no audit | Probably NOT — Nomad-owned, no driver-side enumeration | **OPEN — audit driver assumes Nomad cleans cleanly under retry / vm_index reuse** |
| **mount-ns** (overlay + virtiofs) | driver during CH spawn (per-VM mount-namespace if jailer is used; otherwise the CH process inherits the alloc's mount-ns) | driver `DestroyTask` (implicit via process exit + Nomad alloc-dir teardown) | UNKNOWN | UNKNOWN — depends on whether driver uses a private mount-ns per VM or shares the alloc's | **OPEN** |
| **vsock CID** | driver (per-VM unique CID injected into CH config) | implicit on CH process exit (kernel auto-cleans CID allocations) | YES — kernel auto-cleans on process exit; no driver action required | NO — no `vsock` enumeration API for orphan-scan | **OPEN — confirm kernel auto-clean is sufficient under abnormal exit (OOM-killed, kernel panic during VM exit, abandoned vsock socket)** |
| **jailer chroot** (if used) | driver setup hook (only if `--jailer` is enabled) | driver `DestroyTask` rmtree | UNKNOWN | YES (if in use) — orphan-scan `<jailer-root>/<idx>` | **OPEN — verify whether jailer is in use under the v14 driver; if so, audit `DestroyTask` rmtree EEXIST safety and add `jailer_chroot_orphaned_total` counter** |
| **PID files** (CH api-socket, ch.sock) | driver during CH spawn | driver `DestroyTask` `os.Remove` | YES — driver tolerates ENOENT on remove | YES — filesystem scan `<runDir>/*.sock`, `<runDir>/*.pid` cross-checked against active alloc set | **OPEN — driver has best-effort cleanup but no orphan counter; stale unix-domain sockets in runDir are visible but not enumerated** |
| **systemd transient units** | NOT in use | N/A | N/A | N/A | **N/A** — driver uses `os/exec` not `systemd-run --scope`; surface excluded from inventory unless transient-unit usage is reintroduced |
| **iptables / nft rules** | NOT in use (host bridge handles SNAT) | N/A | N/A | N/A | **N/A** — driver attaches tap to the host bridge; no per-VM iptables / nft rule is created. Re-audit if per-VM filtering is added. |
| **loop devices** | NOT in use (CH attaches `.img` as virtio-blk via file-backed device, no `losetup`) | N/A | N/A | N/A | **N/A** — surface excluded unless the driver switches to losetup-backed disk attachment |
| **network namespaces** | NOT in use (tap attaches to host bridge, no per-VM netns) | N/A | N/A | N/A | **N/A** — re-audit if the driver moves to per-VM netns isolation |

Status summary: **2 CLOSED** (tap, host_dir), **5 OPEN** (cgroup, mount-ns, vsock CID, jailer chroot,
PID files), **4 N/A** (systemd transient units, iptables/nft rules, loop devices, network namespaces).
Re-audit triggers documented per N/A row so a future driver-feature addition reopens the surface.

## Per-surface closure roadmap

For each OPEN surface, the closure deliverable is the same four-part pattern (mirrors the v14 tap and
v34 host_dir closures):

1. **EEXIST-safe re-entry test in `nomad-driver-ch` tests.** Mirror v14's
   `TestDestroyTask_DefensiveTapCleanup_*` pattern in `nomad-driver-ch/stop_task_test.go`. The test
   asserts that `StartTask` on a vm_index that previously failed mid-teardown observes the orphan,
   cleans it, increments the orphan counter, and proceeds without manual intervention.
2. **Orphan counter.** `{surface}_orphaned_total{worker}` exposed via `ch/metrics.go` (mirrors
   `tapsOrphanedTotal` from `177ff165`). The counter increments when the re-entry path discovers a
   stranded resource AND successfully reaps it; failures route to a separate `{surface}_reap_failed_total`
   if the surface needs distinguishing the two outcomes.
3. **Sweeper task in controller** (if the surface is enumerable and reap is not race-bound to
   `StartTask`). Mirror `spawn_host_dir_gc` (`e82bffd7`) — dedicated OS thread, private compio
   runtime via `detach_isolated`, observes `state.shutdown_requested()` between iterations, gated on
   `state.database.is_some()`. Polling cadence and grace policy decided per-surface; the host_dir
   choice (5-min poll, 1-hour mtime grace) is a starting point, not a default.
4. **Grace policy decision.** Codified in the sweeper's docstring + a constant
   (`SANDBOX_{SURFACE}_GC_GRACE_SECS` env override, floor 60s). Options: mtime threshold,
   refcount-aware (verify no live process holds the surface), mount-aware (verify no live mount on
   the surface), instant (no live consumer possible — e.g., tap with no in-flight StartTask). Choice
   must be load-bearing on the surface's failure semantics under partial-failure.

The deliverable per OPEN surface is one driver-side commit (re-entry + counter) and (where applicable)
one controller-side commit (sweeper task + grace constant). EEXIST-safe re-entry is the minimum bar;
sweeper coverage closes the orphan-accumulation vector.

## Cutover gate

T-8b-cutover is **BLOCKED** until ≥4 of the 5 OPEN surfaces reach CLOSED status with the four-part
deliverable. The 2 already-CLOSED surfaces (tap, host_dir) represent the lower-bound of the audit;
they do NOT count toward the gate.

Closure order (proposed, by risk):

1. **cgroup** — Nomad-owned, but a stress-validated audit confirming Nomad reliably cleans under
   retry + vm_index reuse + abnormal alloc exit is the cheapest closure path. Deliverable: a stress
   re-run with `cat /sys/fs/cgroup/.../tasks` post-cycle for each Nomad-owned cgroup; non-zero
   contents = audit fail.
2. **PID files** — driver has best-effort `os.Remove`; closure deliverable is `pid_files_orphaned_total`
   plus an `os.RemoveAll(<runDir>/*.sock)` defensive sweep at `StartTask` entry.
3. **mount-ns** — depends on whether the driver uses a private mount-ns per VM. If yes, closure
   deliverable is `mount_ns_orphaned_total` + audit of `/proc/*/mountinfo` post-cycle. If no
   (the CH process inherits the alloc's mount-ns, which Nomad cleans), the row collapses to N/A
   with the re-audit trigger documented.
4. **jailer chroot** — closure depends on whether jailer is in use under v14. If not in use, the row
   collapses to N/A. If in use, deliverable is `jailer_chroot_orphaned_total` + a `<jailer-root>/<idx>`
   filesystem-scan sweeper.
5. **vsock CID** — kernel auto-clean is the documented mechanism; closure deliverable is a stress run
   that confirms zero stranded CIDs post-run via `ss -K | grep AF_VSOCK`. If non-zero, escalate to
   driver-side enumeration via `/proc/net/vsock` or equivalent.

The cutover commit (wrapper retirement + `SANDBOX_TASK_DRIVER=ch_plugin` default flip) cites this
table and includes a "≥4 of 5 OPEN surfaces CLOSED" assertion in its commit message.

## r19-A1 leak ledger demotion

`sandbox_vm_index_leaks_total` has read `0` for 11 of the last 15 cluster cycles. Per architecture
review `r25-A5` / `r25-A6`, the counter is over-instrumented (it tracks a leak class that is
empirically dormant) while five new surfaces in this inventory are under-instrumented.

This ADR rebalances the counter set:

- **REMOVE** `sandbox_vm_index_leaks_total` and the three lying-comment sites at
  `nomad_ch.rs:{1081,1162,1184}` (per `r24-A5` carry).
- **ADD** `host_dir_leaked_total` + `host_dir_reaped_total` counter pair on the v34 sweeper. The
  sweeper currently emits `(scanned, reaped)` from `run_host_dir_gc_once` but does not expose either
  via the `metrics` module. Symmetry argues for the same `inc_*` + test-only accessor pattern that
  `tapsOrphanedTotal` uses.
- **ADD** the `{surface}_orphaned_total` counter per OPEN surface as those surfaces close (per
  roadmap step 2 above).

The vm_index counter removal lands as the same PR that adds the host_dir counter pair. The other
five additions land per-surface alongside the closure deliverables.

## Cross-references

- `r24-A2` (architecture-r24): enumeration motivated by stress-r1's tap leak + workspace.img
  staging-window race. Source review at
  `docs/reviews/sandbox-snapshot-restore-architecture-2026-05-25-r24.md`.
- `r25-A2` (architecture-r25): "sweeper-owned cleanup is the right shape for host_dir but does NOT
  generalize" — promoted the pattern decision to an ADR. Source review at
  `docs/reviews/sandbox-snapshot-restore-architecture-2026-05-25-r25.md`.
- `r25-A5` (leak-ledger demotion track) and `r25-A6` (host_dir counter symmetry) — both fold into the
  leak-ledger rebalance section above.
- Sibling ADR: `docs/decisions/2026-05-25-restore-debug-playbook.md` codifies the
  observability-before-architecture rule and stress-as-gate rule that this ADR's cutover gate depends
  on.
- Commits cited verbatim: `177ff165` (driver v14 tap defensive cleanup + `tapsOrphanedTotal`),
  `3d03cb90` (driver v13 tap pre-delete on EEXIST), `d638b10f` (controller v34 leak host_dir +
  verbatim driver-msg), `e82bffd7` (controller v34 host_dir GC sweeper), `c729c2b8` (v14 driver +
  v34 controller pin bump), `30960451` (v33 controller-side disk preflight — mechanism refuted by
  stress-r2, retained as harmless defense-in-depth).
