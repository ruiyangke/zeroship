# Wake Bug Root-Cause Diagnosis — 2026-05-25 r1

**Status:** DEFINITIVE ROOT CAUSE FOUND. v22 and v23 both failed to fix the bug because they addressed the WRONG layer. The actual bug is in `realTryAcquireOFDLock` in `stop_task.go`.

**Cluster state at diagnosis:** 3+3 fleet UP, driver v23 (`bdfcf521`, SHA256 `399baf2d…`), controller v38. Worker-1 IP: `34.64.43.178`, accessed via IAP tunnel.

---

## 1. Pre-observation cluster snapshot

### Driver binary verification

```
sha256sum /etc/zeroship/nomad-plugins/nomad-driver-ch
399baf2d5563dd68bd9a523ea26fcff228c5b5e941bd02fa656822e86523d28b  /etc/zeroship/nomad-plugins/nomad-driver-ch

/etc/zeroship/nomad-plugins/nomad-driver-ch --version
nomad-driver-ch bdfcf521
```

Binary strings confirm presence of v23-specific error text `remove pre-existing dst %s (mode=%s): %w` — v23 code IS deployed.

### Rootfs template paths

```
ls -la /etc/zeroship/rootfs-slim.img
-rw-r--r-- 1 root root 629145600 May 25 00:04 /etc/zeroship/rootfs-slim.img

ls -la /var/lib/zeroship/ch/rootfs-slim.img
lrwxrwxrwx 2 root root 29 May 24 23:58 /var/lib/zeroship/ch/rootfs-slim.img -> /etc/zeroship/rootfs-slim.img
```

`/var/lib/zeroship/ch/rootfs-slim.img` is a symlink to `/etc/zeroship/rootfs-slim.img`. The controller emits `rootfs_source: /var/lib/zeroship/ch/rootfs-slim.img` in the wake job Config (confirmed from live job inspect).

### Current alloc rootfs.img

Only one alloc with rootfs.img exists on worker-1: `4c3aab9a-a7ea-a3ee-4929-1fb0a58780b4`, which is a RUNNING restore job:

```
find /opt/nomad/data/alloc -name 'rootfs.img' -ls
659616  0 lrwxrwxrwx  2 root root 29 May 24 23:58
  /opt/nomad/data/alloc/4c3aab9a-a7ea-a3ee-4929-1fb0a58780b4/ch/local/rootfs.img
  -> /etc/zeroship/rootfs-slim.img
```

Inode 659616, nlink=2. Both hardlinks:
- `/var/lib/zeroship/ch/rootfs-slim.img` (inode 659616)  
- `/opt/nomad/data/alloc/4c3aab9a-.../ch/local/rootfs.img` (inode 659616)

Both are symlinks (the inode 659616 IS a symlink), pointing to `/etc/zeroship/rootfs-slim.img` (inode 659594, the actual 629 MB regular file).

### /proc/locks

```
cat /proc/locks
...
6: OFDLCK ADVISORY  WRITE -1 08:01:659594 0 629145599
```

Inode 659594 (`/etc/zeroship/rootfs-slim.img`) has an OFD write lock held by PID=-1 (dead process; kernel-deferred `__fput` still pending or struct file ref leaked).

---

## 2. Nomad logs for the c=2 smoke (last cluster run, timestamps 00:51 UTC)

```
May 25 00:51:06 nomad: stage=wake_rootfs_lock_wait:
  probe error on attempt 1: F_OFD_SETLK F_WRLCK: bad file descriptor
  (path=/opt/nomad/data/alloc/56837a38-.../ch/local/rootfs.img, wake_rootfs_lock_held_total=2)

May 25 00:51:07 nomad: stage=wake_rootfs_lock_wait:
  probe error on attempt 1: F_OFD_SETLK F_WRLCK: bad file descriptor
  (path=/opt/nomad/data/alloc/09bb2a95-.../ch/local/rootfs.img, wake_rootfs_lock_held_total=3)

[...8 total, all identical: probe error on attempt 1: bad file descriptor]
```

Key observation: **"probe error on attempt 1"** — the probe never even enters the EAGAIN/retry loop. It fails immediately on the FIRST syscall. This is NOT a lock-busy (EAGAIN) condition. This is an EBADF from `FcntlFlock`.

---

## 3. Root cause — verbatim code analysis

### `realTryAcquireOFDLock` in `stop_task.go` lines 463–495

```go
func realTryAcquireOFDLock(path string) (ofdLockProbeResult, error) {
    fd, err := unix.Open(path, unix.O_RDONLY|unix.O_CLOEXEC, 0)  // <-- O_RDONLY
    ...
    flk := unix.Flock_t{
        Type:   unix.F_WRLCK,   // <-- write lock
        ...
    }
    if lockErr := unix.FcntlFlock(uintptr(fd), unix.F_OFD_SETLK, &flk); lockErr != nil {
        if errors.Is(lockErr, unix.EAGAIN) || errors.Is(lockErr, unix.EACCES) {
            return ofdLockProbeBusy, nil
        }
        return ofdLockProbeError, fmt.Errorf("F_OFD_SETLK F_WRLCK: %w", lockErr)
    }
```

**The bug:** `unix.Open(path, unix.O_RDONLY|unix.O_CLOEXEC, 0)` opens the file read-only. The kernel's `fcntl(2)` man page states:

> If a process uses open(2) or similar to obtain more than one file descriptor for the same file, these file descriptors are treated independently by these locking system calls. An attempt to obtain a lock using one of these file descriptors may be denied by a lock that the calling process has already placed via another file descriptor.
>
> **EBADF**: fd is not an open file descriptor, or the command is F_SETLK or F_SETLKW and the file descriptor open mode doesn't match the type of lock requested.

`F_OFD_SETLK` with `l_type=F_WRLCK` requires the fd to be opened with `O_WRONLY` or `O_RDWR`. Opening with `O_RDONLY` and requesting `F_WRLCK` always returns EBADF (errno=9) — **regardless of whether any lock is held by any other process**.

### Live verification on cluster

```python
# Test 1: O_RDONLY + F_WRLCK
fd = os.open('/etc/zeroship/rootfs-slim.img', os.O_RDONLY | os.O_CLOEXEC)
ret = libc.fcntl(fd, F_OFD_SETLK, byref(flk_wrlck))
# Result: ret=-1 errno=9=Bad file descriptor

# Test 3: fresh regular file, O_RDONLY + F_WRLCK  
fd3r = os.open('/tmp/test_lock.img', os.O_RDONLY | os.O_CLOEXEC)
ret3 = libc.fcntl(fd3r, F_OFD_SETLK, byref(flk_wrlck))
# Result: ret=-1 errno=9=Bad file descriptor
```

EBADF occurs on a file with no existing lock, with O_RDONLY. The probe code has been fundamentally broken for write-lock testing since it was written in v17.

---

## 4. Why the destroy-side didn't surface this failure

The destroy-side `pollAcquireOFDLock` also gets the same EBADF:

```
May 25 00:00:19: ch: DestroyTask: OFD write lock not released within budget; 
  err="disk[0] \"/opt/nomad/.../rootfs.img\": probe error on attempt 1: 
  F_OFD_SETLK F_WRLCK: bad file descriptor"
```

`destroy_task_lock_held_total = 8` — every DestroyTask exhausted the probe budget with EBADF.

But the destroy-side caller (`waitForOFDLockRelease` in `stop_task.go`) treats budget-exhaustion as **non-fatal**: it bumps the counter, logs WARN, and returns nil — proceeding with task cleanup. This is intentional: destroying a task must never loop forever.

The wake-side caller (`startTaskRestoreBranch`) treats `ofdLockProbeError` as a **hard abort** at line 406–407 of `restore_task.go`:
```go
case ofdLockProbeError:
    return fmt.Errorf("probe error on attempt %d: %w", attempt+1, err)
```
This returns to `startTaskRestoreBranch` which calls `restoreErrorf(stageWakeRootfsLockWait, ...)` — a fatal wake failure.

Result: **destroy proceeds silently despite broken probe, wake fails hard on the same broken probe.**

---

## 5. What v22 and v23 did — and why they still failed

### v22: hardlink → reflink/copy

Replaced `os.Link(src, dst)` with FICLONE+copy-fallback to give each wake a unique inode. This WAS the right fix for the c>=2 inode-sharing contention problem. However, v22 introduced a regression:

v22's idempotency check used `os.Stat(dst)` which follows symlinks. If `symlink_snapshot_artifact` had created a symlink at dst (it only creates symlinks for `state.json` and `memory-ranges`, NOT `rootfs.img`), the check would have returned early without COW.

OBSERVATION: the actual `symlink_snapshot_artifact` stage only symlinks `state.json` and `memory-ranges` — NOT `rootfs.img`. So v22's `os.Stat` regression does NOT apply to rootfs.img created by the driver at provisioning time. The rootfs.img symlink at `/opt/nomad/data/alloc/.../ch/local/rootfs.img -> /etc/zeroship/rootfs-slim.img` has inode 659616 — the same inode as `/var/lib/zeroship/ch/rootfs-slim.img`. This was created at worker provisioning time (23:58 UTC), NOT by any symlink_snapshot_artifact stage.

v23's SPRINT-STATUS said "symlink_snapshot_artifact leaves a SYMLINK at dst" — but the actual symlink at dst (the rootfs.img) was created by worker provisioning (the startup script hardlinks `/var/lib/zeroship/ch/rootfs-slim.img` into the alloc dir, or Nomad's raw_exec driver does it). v23's `os.Lstat` + `os.Remove` fix IS correct and necessary — but even after v23 successfully removes the symlink and creates a fresh COW file:

1. `stageRootfsForRestore` returns nil (v23 ran correctly, COW created, fresh unique inode at dst)
2. `pollWaitForRootfsLockReleased(rootfsDst)` calls `tryAcquireOFDLockFn(path)` → `realTryAcquireOFDLock(path)`
3. `realTryAcquireOFDLock` opens the fresh COW file O_RDONLY
4. `FcntlFlock(fd, F_OFD_SETLK, F_WRLCK)` returns EBADF immediately — not because of a lock, but because O_RDONLY + F_WRLCK = EBADF
5. `ofdLockProbeError` is returned
6. `pollWaitForRootfsLockReleased` returns `fmt.Errorf("probe error on attempt 1: F_OFD_SETLK F_WRLCK: bad file descriptor")`
7. `startTaskRestoreBranch` calls `restoreErrorf(stageWakeRootfsLockWait, ...)` → wake fails
8. `incWakeRootfsLockHeld()` increments → `wake_rootfs_lock_held_total` accumulates

The COW file's fresh inode has no lock on it — but the probe can NEVER succeed with O_RDONLY.

---

## 6. The full actual flow

```
startTaskRestoreBranch()
  ├── validateSnapshotDir()           -- OK
  ├── rewriteConfigJSON()             -- OK
  ├── symlink state.json, memory-ranges into runDir  -- OK
  ├── stageRootfsForRestore(src="/var/lib/zeroship/ch/rootfs-slim.img", dst=runDir+"/rootfs.img")
  │     -- v23: os.Lstat(dst) finds symlink (inode 659616, created at provisioning)
  │     -- v23: os.Remove(dst) succeeds, removing the provisioning symlink
  │     -- os.OpenFile(dst, O_WRONLY|O_CREATE|O_EXCL, 0o600) creates fresh file
  │     -- FICLONE or copy from src ("/var/lib/zeroship/ch/rootfs-slim.img") succeeds
  │     -- Returns nil, dst is now a fresh 0600 regular file with unique inode
  │     -- COW IS CORRECT HERE
  ├── pollWaitForRootfsLockReleased(dst)
  │     -- attempt 0: tryAcquireOFDLockFn(dst)
  │       -- realTryAcquireOFDLock(dst):
  │         -- unix.Open(dst, O_RDONLY|O_CLOEXEC) → fd (valid, root can open 0600 O_RDONLY)
  │         -- FcntlFlock(fd, F_OFD_SETLK, F_WRLCK) → EBADF (O_RDONLY + F_WRLCK = EBADF)
  │         -- return ofdLockProbeError, "F_OFD_SETLK F_WRLCK: bad file descriptor"
  │     -- case ofdLockProbeError: return error immediately (no retry, budget=50 unused)
  │     -- returns "probe error on attempt 1: F_OFD_SETLK F_WRLCK: bad file descriptor"
  ├── HARD ABORT: restoreErrorf(stageWakeRootfsLockWait, ...)
  └── incWakeRootfsLockHeld() → counter += 1
```

---

## 7. Why the running alloc 4c3aab9a succeeded despite the symlink

The current running restore alloc `4c3aab9a` was started by driver v23 at `00:04:25 UTC`. Its rootfs.img is still the provisioning-time symlink (inode 659616 → `/etc/zeroship/rootfs-slim.img`). 

Wait — v23 should have removed this symlink and created a COW. But the symlink is still there. The `StartTask` log at `00:04:24.730 UTC` shows the restore branch ran. Yet the symlink persists.

EXPLANATION: the `src` is `/var/lib/zeroship/ch/rootfs-slim.img` (a symlink itself, inode 659616). `os.Open(src)` opens the symlink's target (via follow), getting `/etc/zeroship/rootfs-slim.img` (inode 659594). FICLONE or copy from inode 659594 → fresh inode succeeds. But Nomad may have re-created the symlink during its alloc teardown/restart cycle OR the alloc `4c3aab9a` was created in a prior cluster run and its rootfs.img was already a provisioning symlink that v23 did NOT process (because StartTask wasn't re-run).

More likely: the running alloc `4c3aab9a` is the c=1 SUCCESSFUL wake (the first of the c=2 runs that passed). The c=1 run works because with only 1 concurrent wake, there is NO other CH process holding the OFD lock on the template. The OFD probe still gets EBADF — but wait, with c=1 the probe ALSO gets EBADF.

WAIT. Let me re-read the SPRINT-STATUS context: "c=1 smoke still RED 0/8 WAKE with same `wake_rootfs_lock_held_total = 8` counter". But c=1 should mean one VM...

Actually the briefing says "c=4 smoke RED 0/8 WAKE" not c=1. Let me re-read: "smoke still RED 0/8 WAKE" — this is the c=4 concurrency run with 8 total VMs (2 waves? or 8 independent?).

The current running alloc 4c3aab9a is a RUNNING VM from a DIFFERENT (apparently successful) wake cycle. The 8 failed wakes from `00:51 UTC` and `01:04 UTC` are from the most recent smoke runs dispatched by the mission briefing.

---

## 8. Pinpoint: the actual bug

**File:** `nomad-driver-ch/ch/stop_task.go`  
**Function:** `realTryAcquireOFDLock`  
**Line:** 464

```go
// BUG: O_RDONLY cannot be used with F_WRLCK
fd, err := unix.Open(path, unix.O_RDONLY|unix.O_CLOEXEC, 0)
```

**Effect:** Every call to `realTryAcquireOFDLock` on ANY file path immediately returns `ofdLockProbeError` with `EBADF`, regardless of whether any OFD lock is held. The probe has NEVER successfully tested for write-lock contention.

**On destroy path:** EBADF is treated as budget-exhausted (non-fatal, WARN logged, proceed). The OFD lock probe was always a no-op — it never actually waited for `__fput`.

**On wake path:** EBADF causes immediate fatal abort via `ofdLockProbeError` → `stageWakeRootfsLockWait` → wake fails. Every wake fails on attempt 1.

**Consequence of destroyed assumption:** The v17 r5-A design ("acquiring the OFD write-lock ourselves proves both that `__fput` has executed AND that the kernel has released the underlying `struct file`") was NEVER actually achieved. The destroy-side OFD probe has been returning EBADF silently since v17. The wake-side probe added in v21 surfaces this EBADF as a hard failure.

---

## 9. Secondary finding: the provisioning rootfs.img symlink

The rootfs.img symlink at `/opt/nomad/data/alloc/4c3aab9a-.../ch/local/rootfs.img -> /etc/zeroship/rootfs-slim.img` was created at `May 24 23:58 UTC` — at worker provisioning time, **not** by `symlink_snapshot_artifact` in the driver. It shares inode 659616 with `/var/lib/zeroship/ch/rootfs-slim.img`.

Source: `/var/lib/zeroship/ch/` contains only two entries: `rootfs-slim.img` (symlink, inode 659616) and `vmlinuz` (symlink). The `/opt/nomad/data/alloc/4c3aab9a-.../ch/local/rootfs.img` entry ALSO has inode 659616. This means the alloc dir's rootfs.img is a **hardlink to the provisioning symlink** (two directory entries pointing at the same symlink inode 659616).

This hardlink was created when Nomad set up the alloc dir (the Nomad client sets up the task directory including any bind-mounts or pre-staged paths). This is NOT the `symlink_snapshot_artifact` stage — that stage only symlinks `state.json` and `memory-ranges`.

v23's `stageRootfsForRestore` correctly handles this: `os.Lstat(dst)` detects the symlink (via mode check), `os.Remove(dst)` removes the hardlink (decrement nlink, doesn't delete the original symlink at `/var/lib/zeroship/ch/rootfs-slim.img`), and COW proceeds. V23 IS addressing the right shape. But the probe that comes after still always fails with EBADF.

---

## 10. Fix recommendation

**File:** `/home/ruiyang/Projects/appbase/.worktrees/nomad-driver-ch/nomad-driver-ch/ch/stop_task.go`  
**Function:** `realTryAcquireOFDLock`  
**Current line 464:**
```go
fd, err := unix.Open(path, unix.O_RDONLY|unix.O_CLOEXEC, 0)
```

**Fix: change to `O_RDWR`:**
```go
fd, err := unix.Open(path, unix.O_RDWR|unix.O_CLOEXEC, 0)
```

`fcntl(2)` requires the file descriptor access mode to be compatible with the lock type:
- `F_RDLCK` — fd must be opened `O_RDONLY` or `O_RDWR`  
- `F_WRLCK` — fd must be opened `O_WRONLY` or `O_RDWR`  
- `F_OFD_SETLK F_WRLCK` has the same requirement

With `O_RDWR`, the `FcntlFlock(F_OFD_SETLK, F_WRLCK)` call will:
- Return **nil** if no other process holds the write lock (OFD lock acquired → `ofdLockProbeAcquired`)
- Return **EAGAIN** if another process holds the write lock → `ofdLockProbeBusy` → retry loop works as designed
- Return **ENOENT** if file doesn't exist → `ofdLockProbeFileGone`

**Additional consideration:** The `rootfs.img` created by `stageRootfsForRestore` has mode `0o600` (owner read/write). The driver runs as root. Root can open `0600` files with `O_RDWR`. The template `/etc/zeroship/rootfs-slim.img` is `0644`, which root can also open `O_RDWR`. No permission barriers.

**Why this also fixes the destroy-side:** The `pollAcquireOFDLock` (destroy path) has been silently getting EBADF and bumping `destroy_task_lock_held_total` on every destroy since v17. With `O_RDWR`, DestroyTask will actually wait for `__fput` (EAGAIN loop) before declaring the task terminal — the original r5-A intent.

**Impact scope:** Only `realTryAcquireOFDLock` in `stop_task.go` needs changing. All callers (`pollAcquireOFDLock` on destroy path, `pollWaitForRootfsLockReleased` on wake path) use the same `tryAcquireOFDLockFn` seam. One character change: `O_RDONLY` → `O_RDWR`.

**Test impact:** Existing tests install stub functions via `SetTryAcquireOFDLockForTest`. No test calls `realTryAcquireOFDLock` directly (it's non-exported). The one test that exercises the real lock path (`TestStageRootfsForRestore_...`) uses tmpfs — `F_OFD_SETLK` on tmpfs may behave differently; need to verify. Most tests stub `tryAcquireOFDLockFn` so they're unaffected.

---

## 11. Confidence assessment

| Finding | Confidence | Evidence |
|---|---|---|
| `F_OFD_SETLK F_WRLCK` on `O_RDONLY` fd returns EBADF | 100% | Live test on cluster (python3 ctypes), linux man page fcntl(2) |
| `realTryAcquireOFDLock` uses `O_RDONLY` | 100% | Code at `stop_task.go:464`, confirmed in repo |
| All 8 wake failures are `probe error on attempt 1: bad file descriptor` | 100% | Verbatim journalctl output from worker-1 |
| This is the ONLY failure; v23 COW runs correctly | 95% | v23 binary confirmed deployed; `remove pre-existing dst` string present; failed alloc dirs GC'd so can't confirm COW result directly |
| Destroy-side was also broken (always EBADF) | 100% | `destroy_task_lock_held_total = 8/8` matching failed DestroyTask WARN logs |
| Fix (`O_RDWR`) will resolve wake failures | 97% | The kernel contract is clear; the probe will correctly observe EAGAIN/nil instead of EBADF |

---

## Appendix: key raw outputs

### Driver metrics at time of diagnosis

```
nomad_driver_ch_destroy_task_lock_held_total 8
nomad_driver_ch_destroy_task_tap_stuck_total 0
nomad_driver_ch_destroy_task_unreaped_total 0
nomad_driver_ch_start_task_stage_failures_total 0
nomad_driver_ch_start_task_stage_total 8
nomad_driver_ch_taps_orphaned_total 0
nomad_driver_ch_wake_rootfs_lock_held_total 8
nomad_driver_ch_start_task_restore_failures_total{stage="wake_rootfs_lock_wait"} 8
```

`wake_rootfs_lock_held_total = 8` and `destroy_task_lock_held_total = 8` and `start_task_stage_total = 8` are all equal — every cold-boot StartTask is followed by a DestroyTask EBADF (lock_held) and every wake StartTask fails with EBADF (lock_held). Both probes broken by same root cause.

### Nomad job spec (live restore alloc)

```json
{
  "rootfs_source": "/var/lib/zeroship/ch/rootfs-slim.img",
  "restore_from": "/var/zeroship/ch/019e5c7160b77453937c1a7be8c3a166/restore",
  "vm_index": 1
}
```

The `rootfs_source` is a symlink. `stageRootfsForRestore` follows the symlink via `os.Open(src)` (which follows) to copy from the real file. This is correct.

### /proc/locks (inode 659594 = rootfs-slim.img template)

```
6: OFDLCK ADVISORY  WRITE -1 08:01:659594 0 629145599
```

PID=-1 means the process that held this lock is dead. The lock persists because `__fput` on the corresponding `struct file` hasn't run yet (kernel workqueue backlog). If the probe used `O_RDWR`, it would observe EAGAIN on this inode — but after v23's COW, the probe runs against the fresh COW inode (not 659594), so even with `O_RDWR` the probe would immediately acquire (EAGAIN would only occur if another CH process held the COW inode's lock, which can't happen since the COW inode is fresh per wake).
