# Sandbox/snapshot-restore — concurrency r33 review

Date: 2026-05-25 (UTC).
HEAD at audit: `fe8c9216` (worktree `.worktrees/sandbox-snapshot-restore`,
branch `feat/sandbox-snapshot-restore`).
Round 33 of N. READ-ONLY.

Scope since r32 (`8d82ecde` → `fe8c9216`):

- `2faaf39b` sandbox/nomad-ch: parallelise cold-boot `mkfs.ext4` via
  `std::thread::scope` inside the existing `compio::runtime::spawn_blocking`
  closure at `nomad_ch.rs:1133` (R32-P1 close).
- `fe8c9216` reviewer paperwork only (no source changes).

Focus this round:
- `std::thread::scope` block — borrow / lifetime correctness, shared-state
  data race surface, error-channel atomicity.
- mkfs.ext4 subprocess concurrency vs. prior controller state — any
  shared host_dir hazard? Cross-CREATE collision on `home.img`?
- R30-I1 (`gc_stop_chunked` async `backend.stop(id).await` unguarded).
- Tightened cadence sustained behaviour (carry from r32 50/100ms flip).

Prior: `…concurrency-2026-05-25-r32.md`.

## Summary

- **2 findings** (0 NEW CRITICAL, 1 NEW IMPORTANT, 0 NEW MINOR,
  1 verification-clean close on the threading shape itself).
- **R33-I1 (NEW IMPORTANT)**: `home.img` is **per-USER**, not per-sandbox
  (`nomad_ch.rs:954`). Two concurrent cold-boot CREATEs for the same user
  race on the same `<user_home_dir_root>/<user_id>/home.img` path via the
  parallelised mkfs from R32-P1. The pre-existing sequential code had the
  same race surface across distinct CREATE futures; R32-P1 doesn't
  introduce it but **widens the window** by pulling both mkfs onto the
  hot path simultaneously. See §[R33-I1] below.
- **R32-P1 thread-scope shape** itself is **borrow-clean** — verified
  against the std `thread::scope` 1.63+ contract.
- **R30-I1 STILL OPEN** — no movement; only commit since r32 touching
  source is `2faaf39b` (mkfs parallelisation, separate file path from
  `registry.rs:975`).
- 100ms cadence in `wait_for_alloc_running` and 50ms in
  `wait_for_agent_livez` continue clean over the r32→r33 interval (no
  new poll-loop touches; no contention surface introduced by R32-P1).

## IMPORTANT

### [R33-I1] (NEW IMPORTANT) Parallel mkfs of `home.img` widens a pre-existing per-user race

- **File**: `crates/sandbox/src/backend/nomad_ch.rs:1155-1174` (R32-P1)
  combined with `:954-957` (`user_home_img` derivation).

- **Shape**: `user_home_image_path(&self.cfg.nomad_ch.user_home_dir_root,
  user_id)` is a per-USER path. The same user submitting N concurrent
  cold-boot CREATEs gets N CREATE futures, each with its OWN
  `spawn_blocking` closure that schedules its OWN `std::thread::scope`
  spawning a `home_h` thread targeting the **same path**. The
  `workspace_img` half is fully isolated (per-sandbox dirent under
  `host_dir`), so the per-sandbox half is safe. The `home.img` half is
  NOT.

- **Race window analysis**:

  ```rust
  pub(crate) fn create_ext4_image_if_missing(path: &Path, size_gb: u32)
      -> Result<(), String>
  {
      if path.exists() {                       // CHECK
          return assert_disk_image_present(path);
      }
      let truncate_status = ... .arg(path) ... // truncate
      ...
      let mkfs_status = ... .arg(path) ...     // ACT (mkfs.ext4 -q -F)
      ...
      fsync_dir(parent)?;                      // fsync
      assert_disk_image_present(path)
  }
  ```

  Classic TOCTOU. Two siblings can both observe `!path.exists()`, both
  invoke `truncate -s NG <path>` (truncate(1) truncates in place, no
  exclusivity), and both invoke `mkfs.ext4 -q -F <path>`. `mkfs.ext4
  -F` does NOT take an exclusive lock on the device/file — it happily
  rewrites the superblock of a file another process is currently
  formatting. The losing thread can write its superblock over the
  winner's freshly-formatted ext4, then the post-stage
  `assert_disk_image_present` passes for both, BUT the on-disk image
  may now have a partially overwritten group descriptor / inode table
  from the second `mkfs.ext4 -q -F` racing with the first.

  R32-P1 widens the window because **before**, the two mkfs ran
  sequentially inside one spawn_blocking, so for a given sandbox the
  workspace mkfs ran first, then home mkfs — total time on the critical
  path ~2× a single mkfs. Now the two run concurrently INSIDE one
  CREATE. Across two CREATEs by the same user, the home mkfs of CREATE
  #2 can land while CREATE #1's home mkfs is still in flight (whereas
  before, CREATE #2's home mkfs landed AFTER CREATE #2's workspace
  mkfs, which was further along after CREATE #1's serial wall).

  Concretely: c=4 stress with one user, all cold-boot. Each CREATE's
  spawn_blocking closure runs on the blocking-thread pool roughly in
  parallel. Each one's `std::thread::scope` spawns a `home_h` thread.
  Four `mkfs.ext4 -q -F <same-path>` processes can be in flight. The
  pre-R32-P1 code had at most two at once for the same user (because
  workspace mkfs serialised half the cold-boot wall). After R32-P1 it
  is N.

- **Why pre-R32-P1 wasn't entirely safe either**: The race surface
  existed in concept; we just got lucky on c=1 cluster runs because
  one user typically had ≤1 cold-boot in flight at a time. The first
  CREATE wrote `home.img`, subsequent CREATEs hit the idempotent
  `path.exists()` skip. T-8b stress c=4-c=16 is where this surface
  was always going to bite, and R32-P1 increases the probability per
  unit time of the collision window opening.

- **Carry**: NEW for r33.

- **Severity**: IMPORTANT. Not catastrophic (the second writer's mkfs
  produces a valid ext4 superblock; the worst observable case is the
  user losing whatever inodes the first writer had created in the
  ~ms between the two mkfs's `assert_disk_image_present` checks).
  But the failure mode is silent corruption observable only at next
  guest mount, and it's directly on the cold-boot hot path. WORTH
  fencing before c≥4 cluster default.

- **Recommended fix** (~20 LOC):

  Either (preferred) gate `home.img` staging behind a per-user mutex
  in the controller (a `DashMap<UserId, Mutex<()>>` lazily inserted),
  so concurrent CREATEs by the same user serialise on the home-image
  staging step only. The workspace-image step (the slow, parallelism-
  worthy half) stays parallel. Net effect: c=4 with 1 user keeps the
  perf win for the per-sandbox workspace_img (3 of 4 CREATEs see
  zero added wall), only the home.img mkfs serialises, and it's
  already idempotent-skip on warm-boot.

  Or (simpler) take `O_CREAT | O_EXCL` on the path FIRST as a lock
  file (`<path>.staging`); the loser waits-with-timeout for the
  winner to fsync, then takes the idempotent path. `O_CREAT|O_EXCL`
  is atomic on local NVMe + Linux. ~15 LOC.

  Or (laziest) accept the race because `mkfs.ext4 -F` of a fresh
  ext4 filesystem is functionally idempotent at the on-disk-bytes
  level for the same `-b 4096` / same size case. This is what the
  current code IMPLICITLY relies on. The risk surface is the
  inter-call inode allocation if the guest mounts mid-mkfs, which
  pre-mount the controller controls — we don't mount until after
  both `assert_disk_image_present` returns Ok.

  Stance: the "laziest" answer is defensible but should be EXPLICIT
  in the code comment. The current R32-P1 comment claims "disjoint
  paths and disjoint dirents" — that's TRUE per-sandbox but FALSE
  per-user. Worth at minimum rewording the safety claim to be honest
  about the per-user-mkfs collision and pointing at the
  idempotent-mkfs-on-fresh-ext4 invariant the code de-facto relies on.

## Items NOT findings (verified clean this round)

### [N/A] `std::thread::scope` borrow shape — VERIFIED CLEAN

The scope block at `nomad_ch.rs:1155-1174` borrows three names from the
enclosing `spawn_blocking` closure:

- `workspace_img: PathBuf` (owned, declared at `:1140` inside the
  closure) — borrowed by `workspace_h` only. Not aliased.
- `user_home_img_owned: PathBuf` (owned, moved in via `move ||` at
  `:1133`) — borrowed by `home_h` only. Not aliased.
- `workspace_img_size_gb: u32` (`Copy`) — copied into both closures
  trivially via the closure-capture coerce-to-value rule. Not a
  shared mutable.

Both `s.spawn` closures take `FnOnce + Send + 'scope`. The borrows
satisfy `'scope` because std's `scope` function joins all children
before returning (RFC 3151 / std 1.63 semantics). The two
`workspace_h` / `home_h` `ScopedJoinHandle`s are dropped (via
explicit `.join()`) inside the scope, so the implicit `Drop`-time
join doesn't double-fire. No leaked thread, no after-scope use.

`unwrap_or_else(|p| Err(format!("…panic: {p:?}")))` handles the
`Box<dyn Any + Send>` panic payload from a thread-internal panic
without unwinding the parent `spawn_blocking` future. Matches the
outer `unwrap_or_else` at `:1180` that absorbs spawn_blocking's
own panic. Two-layer panic catch is consistent.

**No data race on `&workspace_img`, `&user_home_img_owned`, or
`&workspace_img_size_gb`** — each thread reads from disjoint
locals (or a `Copy` value). The thread-scope IS borrow-clean as
the commit message claims. The race surface I flagged in R33-I1
is at the FILESYSTEM layer (per-user path collision across CREATE
futures), not at the Rust-memory-safety layer. The compiler is
correct that this code is sound; the bug is in the assumption
that two `mkfs.ext4` subprocesses on the same file don't fight.

**Clean.**

### [N/A] R30-I1 status — NO MOVEMENT

`registry.rs:975` still has unguarded `state.backend.stop(id).await`.
Only `2faaf39b` (sandbox-side mkfs parallelisation) and `fe8c9216`
(reviewer paperwork) since r32. The catch_unwind fix carry-forward
recommendation stands.

### [N/A] Cadence sustained behaviour — CLEAN

No poll-loop touches since r32. The 50ms / 100ms cadences from
`a6e517b2` continue running clean. R32-P1's parallel-mkfs does NOT
add any new poll-loop or wake source — `std::thread::scope` parks
the parent thread on each `.join()`, no busy-wait, no compio timer
involvement. Wake-rate budget unchanged.

## Carry table (delta)

| Finding | Source | r33 state |
|---|---|---|
| R30-I1 gc_stop_chunked join_all panic blast | r30 IMP | **STILL OPEN** (no movement since r29) |
| R30-M2 snap-idle-gc shutdown_requested() | r30 minor | carry |
| R30-M3 state.sandboxes.get touches last_used | r30 minor | carry |
| R31-M1 "default 5 s" stale comments | r31 minor | carry |
| R31-M2 vm_index_ceil rustdoc "default 155" | r31 minor | carry |
| R32-M1 alloc_first_seen lacks sandbox_id | r32 minor | carry |
| **R33-I1** parallel mkfs of per-user home.img | r33 IMP | **NEW** |
| Older carry: R20-I2, R20-I3, R24-I1, R25-I1, R25-I2, R26-I1, R27-I1, R27-I2, R27-M1, R27-M2 | older | carry |

## Status block

```
Round 33 (2faaf39b mkfs parallelisation; fe8c9216 paperwork only):

  CLOSED THIS ROUND:
    (none)

  NEW IMPORTANT:
    R33-I1 parallel mkfs of per-user home.img — std::thread::scope
      in R32-P1 widens a pre-existing TOCTOU race on
      <user_home_dir_root>/<user_id>/home.img. Two concurrent
      cold-boot CREATEs by the same user can race two
      `mkfs.ext4 -q -F <same-path>` subprocesses. Fence options:
      per-user mutex (preferred), O_CREAT|O_EXCL staging lock,
      or accept-and-document the idempotent-mkfs-on-fresh-ext4
      invariant the code de-facto relies on.

  STILL OPEN (carry, unchanged):
    R30-I1 gc_stop_chunked join_all panic blast
      — async backend.stop(id).await at registry.rs:975
      still unguarded.
    R31-M1 "production default 5 s" stale comments.
    R31-M2 vm_index_ceil rustdoc default fix.
    R32-M1 alloc_first_seen omits sandbox_id.

  STILL OPEN (older carry):
    R20-I2, R20-I3, R24-I1, R25-I1, R25-I2, R26-I1,
    R27-I1, R27-I2, R27-M1, R27-M2, R30-M2, R30-M3

  ASK:
    (1) R33-I1: fence per-user home.img staging.
        Recommended: per-user DashMap<UserId, Mutex<()>>
        around the home_h spawn; keep workspace_h
        unconditionally parallel.
    (2) R30-I1 carry: catch_unwind around backend.stop(id)
        .await in AppStateGcStopper::stop_one.
    (3) R32-M1 carry: plumb sandbox_id into
        wait_for_alloc_running.
    (4) R31-M1 + R31-M2 carry: docstring/comment fixups.
```

## ASK clarifications for the user

R33-I1 is the meaningful new finding. The R32-P1 commit message claim
"disjoint paths and disjoint dirents" is true per-sandbox (workspace.img)
but **not** per-user (home.img). The pre-existing sequential code had
the same race surface across distinct CREATE futures by the same user;
R32-P1 doesn't introduce the race but increases the per-unit-time
collision probability under c≥2 same-user cold-boot stress.

Severity is IMPORTANT not CRITICAL because `mkfs.ext4 -F` on the same
fresh path with the same `-b 4096` parameters is de-facto byte-stable
for the on-disk superblock structure, and the controller doesn't mount
the image until both threads' `assert_disk_image_present` returns. The
worst observable failure is silent corruption that surfaces at guest
mount inside the VM. Worth fencing before c≥4 same-user default.

R30-I1 remains the only other IMPORTANT-level finding open.
