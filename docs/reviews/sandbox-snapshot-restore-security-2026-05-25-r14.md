# Sandbox/snapshot-restore — security r14 review

Date: 2026-05-25 (UTC)
HEAD at audit: `91ce9be5` (one ahead of the hunt-list's `d673e043`;
the C-6 fix `91ce9be5` landed during this review's wall-clock
window — see § 5).
Round 14 of N (security lens). Read-only.
Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`,
`crates/sandbox/scripts/**`.

## Summary

1 new IMPORTANT (R14-S1, the **runtime-starvation DoS as a class** —
C-6 demonstrated that ANY long-running detached task on the
single-threaded ntex-worker compio runtime starves co-located wake
handlers; the `91ce9be5` C-6 fix closes the specific snap-teardown
arm by spawning a dedicated OS thread + own runtime, but does NOT
generalize the pattern; sibling `spawn(...).detach()` sites
mentioned in the commit message still co-locate). 1 new MINOR
posture (R14-S2, C-4's 60×2 s retry budget now demonstrably amplifies
runtime-starvation attacks: every co-located wake spins its full
120 s in the retry loop, holding additional ntex worker-connection
slots). 0 new CRITICAL.

The C-6 fix at `91ce9be5` (admin_handlers.rs:1339-1385) mirrors
C-3's pattern from `snapshot_store_gcs.rs::Tiered::put`
(`std::thread::Builder::new().spawn(...)` + per-thread
`compio::runtime::Runtime::new().block_on(...)`). Same uid/gid/cwd
inheritance as C-3 (no `setresuid`/`unshare`/`chroot` between spawn
and run); same controller-process-memory access (Arc<AppState>
clone moved in); same VM-instance-metadata identity for the OAuth
token Nomad-purge / GCS-list calls inside `stop_inner`. **Zero new
unsafe surface, zero new privilege boundary changed.** Verdict: no
security delta on the C-6 fix itself — confirmed per hunt-list
item #5.

R9-S1 / R9-S2 / R9-S3 carry forward unchanged. R10-S1 (5 symlink-
follow loaders) re-verified: `std::fs::metadata` still on the
five `load_*` paths in `db.rs:831,1166` + `lib.rs:947` +
`persist.rs:341` + `snapshot_aead.rs:190` (the lib.rs/db.rs sites
remain the original secret-file loaders; the other 31
`std::fs::metadata` calls at HEAD are test-helpers / size-check /
device-stat sites and outside the symlink-follow finding).
R13-S1 (worker SA still default Compute Engine SA; provision script
unchanged at `:275-289`). R13-S2 (no per-bearer rate-limit on
snapshot/wake; still only `MintRateLimiter` wired at `lib.rs:69,
399,757`).

R12-S1 (ENV_LOCK partial close) unchanged at HEAD —
`db.rs::ENV_LOCK` and `nomad_ch.rs::TASK_DRIVER_ENV_LOCK` still
disjoint per-env-key mutexes.

## Hunt-list disposition (security lens)

### 1. Runtime starvation as a DoS vector (hunt #1)

C-6's root cause (per cluster smoke-r7 timeline at
`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke
-r7.md:97-178`) is a structural property of the ntex-worker
single-threaded compio runtime: a detached future on the same
runtime as a sleep-based retry loop will starve the retry loop
if the detached future's first poll dives into a long sync call
(here, ureq's `/shutdown` connection-timeout polled inside the
agent's `stop_inner` http_signed_async wrapper). The wake-handler
phase tracing introduced at `8e7f0b53` proved the wedge: `phase=
pre_reserve_vm_index` emitted, then **zero phase lines for the
full 60 s client deadline**, despite the retry loop being a tight
`for attempt in 1..=60 { reserve; compio::time::sleep(2s).await; }`.

The fix at `91ce9be5` decouples the snap-teardown arm by
spawning a fresh OS thread with its own short-lived compio
runtime. **The structural class is NOT closed in general** —
the commit message itself lists sibling `compio::runtime::spawn(...
).detach()` sites at `lib.rs:989/1072/1283`, `sweep.rs:227/563`,
`registry.rs:829`, and `nomad_ch.rs:2002` that were audited and
*deemed safe for steady-state* (top-of-loop `compio::time::sleep`
yields before any awaited blocking call), but the audit's
soundness depends on the assumption that none of those sites
will ever evolve to add a sync-heavy await early in the body.

**Can an attacker trigger this intentionally?** Yes, with the
following constraints:
- Attacker needs a **valid sandbox-token or sandbox-admin-token**
  (both operator-issued bearer creds; not anonymous attacker
  surface). admin_check at `admin_handlers.rs:225-294` gates
  every admin endpoint including `/snapshot` and `/restore`.
- Attacker drives sustained `POST /snapshot` requests. Each
  snapshot's `teardown_source_for_snapshot` now goes onto a
  *fresh OS thread* (post-`91ce9be5`), so no co-location with
  the ntex-worker runtime where wake handlers live. **The C-6
  fix eliminates the snap-teardown-as-starvation-source arm.**
- The remaining starvation-source candidates are the sibling
  detach sites enumerated in `91ce9be5`'s commit message. None
  of them are easily attacker-triggered: `lib.rs:989` is the
  Hub-link reconcile loop, `lib.rs:1072` the host-id-rotation
  watchdog, `lib.rs:1283` the ha-leader watchdog, `sweep.rs:227`
  the snapshot-orphan sweeper, `sweep.rs:563` the idle-eviction
  sweeper, `registry.rs:829` the route-registry pull loop. All
  loop on top-of-body `compio::time::sleep` whose first await
  yields control back to the runtime; the inner work runs in
  small bounded chunks.
- The single attacker-controllable starvation surface that
  remains is **`nomad_ch.rs:2002` Drop guard** (create-failure
  teardown), which the C-6 commit explicitly flagged as "lower-
  risk but candidate for similar treatment if a regression
  surfaces". An attacker who can drive sustained CREATE failures
  could chain those Drop guards onto the worker runtime, but the
  CREATE flow already has VmIndexAllocator-level back-pressure
  (12 slots/worker per R13-S2) that bounds throughput before
  the runtime would saturate.

**Security characterization**: structural DoS class is partially
closed at `91ce9be5`. The reachable attack surface post-`91ce9be5`
is the `nomad_ch.rs:2002` Drop guard — bounded by VmIndexAllocator
back-pressure and gated behind admin auth. **Posture, not
finding** — but see R14-S1 for the documentation gap (the
commit lists the sibling sites but doesn't elevate them).

### 2. C-4 bounded retry as DoS amplifier (hunt #2)

Pre-`91ce9be5`: a malicious snapshot's detached teardown starves
the runtime → ALL co-located wake handlers spin in C-4's
`reserve_vm_index_with_retry` 60 × 2 s = 120 s budget AND tie up
ntex worker-connection slots. Cluster smoke-r7 showed exactly
this shape: the wedged wake handler dropped at 60 s (stress
client side timed out and tore the connection) but the
controller-side handler would have continued spinning until
either (a) the slot freed at ~02:55:46 and the retry loop
finally got polled, or (b) the 120 s exhaustion budget elapsed.

**Connection-slot amplification**: ntex's worker accept loop
defaults to a finite connection pool per worker. A wedged wake
handler holds (i) one ntex connection slot, (ii) one in-flight
HTTP request, (iii) the controller's pg connection it acquired
for the row read + CAS. Sustaining ~6 concurrent wedged wakes
per worker exhausts the typical 100-connection pg pool (cf. C-6
candidate `(c) pg-pool churn in update_sandbox_status hitting
max_connections=100` enumerated in `8e7f0b53`'s commit message
and falsified by the trace but still real as an exhaustion
surface).

Post-`91ce9be5`: the snap-teardown arm no longer starves the
runtime, so legitimate wakes get polled and the retry loop
succeeds bounded-late as designed. The amplification surface
collapses to the same scope as R13-S2 — admin-auth-gated DoS,
bounded by VmIndexAllocator + the 120 s caller-retry envelope.

**Verdict**: amplification is real but the multiplier is gated
by the same admin-auth fence as the trigger. **Posture issue
(R14-S2)**: 120 s retry budget on a saturated worker means
every wake hold-time × concurrency consumes the pg pool faster
than the legitimate steady-state predicts. Worth surfacing as
SLO-class capacity-planning input, not as a security finding.

### 3. Phase tracing log-volume DoS (hunt #3)

`8e7f0b53` added 14 (some restore paths emit 17) `tracing::info!`
lines per wake at INFO level. At c=20 sustained the math is
~17 × 20 wakes/s × ~250 bytes/line ≈ 85 KB/s of wake-handler
log output. Bounded; below journald's typical 1 MB/s default
rate-limit; gated by admin-bearer auth (same as the wake
endpoint itself). **Not a security issue** per the hunt-list's
own characterization.

### 4. Re-check carry-forwards (hunt #4)

#### R10-S1 symlink residual — UNCHANGED

The 5 secret-file loaders (`db.rs:831` `master_key`, `db.rs:1166`
`pg_password`, `lib.rs:947` `admin_token`, `persist.rs:341`
`auth_token`, `snapshot_aead.rs:190` `root_kek`) all still call
`std::fs::metadata(path)` (which follows symlinks) for the uid /
mode pre-check, then a separate `read_to_string` / `read` /
`File::open` for the actual content. TOCTOU + symlink-pivot
window unchanged. Verified by re-grep at HEAD.

The 31 additional `std::fs::metadata` hits at HEAD are
distributed across:
- `snapshot_aead.rs:901,910,1230,1262` — test-helpers, size /
  uid inspection (not secret loaders).
- `db.rs:2911,2948,3049,3089,3171,3204,3242` — test-helpers
  (`runner_uid` checks under `#[cfg(test)]`).
- `lib.rs:1500` — test-helper (`runner_uid`).
- `restore_handler.rs:638` — staged-file-size diagnostic
  (post-`store.get` Bug-#14a tracer; not a secret loader).
- `snapshot_store.rs:190,548,549,595,596,630` — store-side
  size/ino/inode inspection.
- `snapshot_handler.rs:552` — memory-ranges artifact size for
  the metering counter (post-snapshot; not a secret).
- `persist.rs:1089,1121` — test-helpers.
- `config.rs:481` — `wrapper_path` mode check (the
  `nomad-vm-wrapper.sh` mode; not a secret content read).
- `snapshot_store_gcs.rs:352,589,996` — GCS upload size pre-
  flight (not a secret).
- `nomad_ch.rs:1824,4423,4429` — api-socket existence + image
  mtime (not a secret).
- `backend/docker.rs:913` — test-only permissions check.

None of these 31 sites change R10-S1's blast radius. **R10-S1
carry-forward at the same 5 sites** — `O_NOFOLLOW + fstat` is
still the remediation.

#### R13-S1 worker SA — UNCHANGED

`provision-gcp-cluster.sh:286` still emits `--scopes=storage-rw,
logging-write,monitoring-write` with no `--service-account`. The
worker VM inherits the default Compute Engine SA, which carries
`roles/editor` project-wide by default. **No change** between
r13 and r14.

#### R13-S2 vm_index DoS — UNCHANGED

`MintRateLimiter` still the only limiter wired at `lib.rs:69,
399,757`. No per-bearer rate-limit on snapshot/wake. Post-
`91ce9be5`, the runtime-starvation arm of this DoS is closed,
but the vm_index-saturation arm remains (60 slots cluster-wide
at 12×5 workers; ~0.67 wakes/s saturation envelope). **No
change** between r13 and r14.

### 5. C-6 fix-in-flight security boundary (hunt #5)

The hunt-list flagged commit `a36b6de1aa6340eaa` as in-flight at
review time. The actual landed C-6 fix is `91ce9be5` (parented
on `d673e043`, the hunt-list's nominal HEAD). I audited the
landed diff:

**`std::thread::Builder::new().spawn(...)` vs the prior
`compio::runtime::spawn(...).detach()` — security delta**:

Both primitives:
- Run in the controller process (same pid).
- Inherit the controller's uid/gid/cwd/umask (no `setresuid` /
  `setresgid` / `unshare` / `chroot` between spawn and run).
- Have full access to the controller's process memory; the
  `Arc<AppState>` clone moved into the closure carries the
  pg pool, the snapshot store handle, the backend trait
  object, the rate-limiter state, the admin-token comparator,
  the route registry — same as before.
- Have full access to the controller's filesystem view.
- Use the VM's instance-metadata OAuth token for the
  Nomad-purge HTTP call + the GCS-list calls inside
  `stop_inner` — same identity (the VM-level SA, see R13-S1).

The new compio runtime spawned via `compio::runtime::Runtime::new()`
is a per-thread runtime: it creates its own io_uring instance,
its own waker / executor state, but it has **no special
privilege**. Same uid as the parent; the io_uring fd inherits
the parent process's cgroup / namespace assignments.

The only new failure mode is `thread::Builder::spawn` returning
`Err` on ENOMEM/EAGAIN, which the code handles correctly:
`tracing::error!` + drop the teardown (vm_index leaks until
orphan-prune, same loss-of-availability surface as a teardown
failure pre-`91ce9be5`).

**Zero new `unsafe` blocks** in `crates/sandbox/src/admin_handlers
.rs` introduced by `91ce9be5` (verified by `git diff d673e043
91ce9be5 -- crates/sandbox/src/admin_handlers.rs | grep unsafe`:
zero hits).

**Verdict**: no security boundary changed. C-6 fix is a
runtime-affinity decoupling, structurally identical to C-3
(`c890c015`) which the r13 review already cleared. Confirmed
per hunt-list expectation.

### 6. `unsafe` audit — re-run since r13

`Grep '^\s*unsafe\s*\{'` on the production tree at HEAD `91ce9be5`:

Production unsafe blocks (unchanged from r13):
- `crates/sandbox-agent/src/dropuser.rs` — 16 unsafe blocks
  (libc setuid/setgid/prctl/setrlimit/etc.; module-scoped
  `#![allow(unsafe_code)]`; explicit `unsafe fn pre_exec_lockdown`).
- `crates/sandbox-agent/src/files.rs` — 7 unsafe blocks
  (OwnedFd::from_raw_fd around openat2).
- `crates/sandbox-agent/src/exec.rs` — 2 unsafe blocks
  (Command::pre_exec closure, geteuid).
- `crates/sandbox-agent/src/handlers.rs` — 1 unsafe block
  (`libc::settimeofday` in /_clock_resync; R9-S6 surface).

Test-only unsafe blocks (unchanged from r13):
- `crates/sandbox/src/db.rs` — 3 unsafe blocks (ENV_LOCK env-set).
- `crates/sandbox/src/backend/nomad_ch.rs` — 3 unsafe blocks
  (TASK_DRIVER_ENV_LOCK env-set, relocated by `c5b9cb9d`).

`91ce9be5` (C-6 fix) added zero unsafe. `8e7f0b53` (phase
tracing) added zero unsafe. `00161cea` (R14-API1 visibility
demotion) added zero unsafe. `370fdbba` (orphan metric fn
deletion) removed zero unsafe (the deleted fns were
test-accessor wrappers around `AtomicU64::load`).

**Confirmed: zero new unsafe surface since r13.**

## Findings (NEW since r13)

### [R14-S1] Sibling `spawn(...).detach()` sites NOT generalized by C-6 fix — `nomad_ch.rs:2002` Drop guard is admin-reachable runtime-starvation source (IMPORTANT, security-r14)

- **Files**: `crates/sandbox/src/admin_handlers.rs:1339-1385`
  (C-6 fix landed at `91ce9be5`); `crates/sandbox/src/lib.rs:989,
  1072,1283` (Hub-link reconcile / host-id watchdog / ha-leader
  watchdog detach sites); `crates/sandbox/src/sweep.rs:227,563`
  (snapshot-orphan sweeper + idle-eviction sweeper);
  `crates/sandbox/src/registry.rs:829` (route-registry pull
  loop); `crates/sandbox/src/backend/nomad_ch.rs:2002` (Drop
  guard create-failure teardown).
- **Symptom**: `91ce9be5` closes the snap-teardown-as-starvation
  arm of the C-6 class by spawning a dedicated OS thread + own
  compio runtime, mirroring C-3's pattern. The commit message
  audits the six sibling `spawn(...).detach()` sites and labels
  five as "safe for steady-state" (top-of-loop
  `compio::time::sleep` yields before inner work) and one
  (`nomad_ch.rs:2002`) as "lower-risk but candidate for similar
  treatment if a regression surfaces". **The audit is not
  belt-and-suspenders**: it assumes the loops at the five
  "steady-state safe" sites will never evolve to add a
  sync-heavy await early in the body. Each site is a code-review
  fence, not a structural fence — a future PR adding e.g. a
  ureq HTTP call before the top-of-body sleep at any of the six
  sites re-opens the structural class.
- **Threat model**: admin-bearer-holder drives sustained CREATE
  failures. Each Drop-guard run at `nomad_ch.rs:2002` is a
  detached teardown on the ntex-worker runtime. If the
  attacker can force the create to fail mid-way (e.g.
  malformed config that gets through the controller's
  validation but rejected by the Nomad driver), the Drop guard
  fires, the teardown's first await is `stop_inner` →
  `http_signed_async("/shutdown")` → same ureq-blocking shape
  that wedged C-6. Co-located wake handlers spin in C-4's
  retry budget; legitimate wakes time out. The threat model is
  **identical to pre-`91ce9be5`** but the trigger is
  CREATE-failure instead of snapshot.
- **Why this is IMPORTANT not MINOR**: the C-6 fix as a
  *general defense* against the runtime-starvation class
  would require either (a) spawning every detached teardown on
  its own thread + runtime (the C-3 / C-6 pattern, structural),
  or (b) wrapping the inner sync sinks in `spawn_blocking`
  with rigorous discipline that no new await is added before
  the wrap. The current `91ce9be5` does neither for the
  sibling sites; it documents the audit in the commit message
  but doesn't fence the audit in code. An attacker can still
  reach the class via the Drop guard arm.
- **Action**:
  (a) Apply the C-6 / C-3 pattern (`std::thread::Builder::spawn`
      + per-thread `compio::runtime::Runtime::new().block_on`)
      to the `nomad_ch.rs:2002` Drop guard. This is the only
      *attacker-reachable* sibling site; the other five are
      steady-state loops not driven by admin input.
  (b) Audit the five steady-state loops periodically (or
      assert structurally via a clippy lint) that no
      `spawn(...).detach()` body adds an await before the
      top-of-body `compio::time::sleep`. Belt for (a).
  (c) Document in `crates/sandbox/src/lib.rs` (the AppState
      assembly point) the invariant: "any detached future on
      the controller process MUST either (i) be a steady-state
      loop with `sleep` as its first await, or (ii) run on a
      dedicated OS thread via `std::thread::Builder::spawn` +
      `compio::runtime::Runtime::new()`". Operator-readable
      contract for future maintainers.

### [R14-S2] C-4's 120 s retry budget amplifies any runtime-starvation source via pg-pool exhaustion + ntex connection-slot hold (MINOR posture, security-r14)

- **Files**: `crates/sandbox/src/restore_handler.rs:128-160`
  (`VmIndexRetryPolicy`, 60 × 2 s = ~120 s budget);
  `crates/sandbox/src/restore_handler.rs:265-306`
  (`reserve_vm_index_with_retry`, the polling site).
- **Symptom**: post-`91ce9be5`, the **runtime-starvation
  amplification surface remains structurally present** even
  though the snap-teardown arm is closed. Any future
  regression (R14-S1 sibling site, future detach addition,
  Drop-guard arm) that re-introduces starvation will be
  amplified by C-4's retry budget: every co-located wake
  handler holds (i) one ntex connection slot for 120 s,
  (ii) one pg-pool connection acquired by the prior
  `update_sandbox_status(Restoring)` write at
  `restore_handler.rs:cas_restoring_ok`, (iii) one in-flight
  HTTP request in the ntex worker accept queue. With
  `max_connections=100` on the pg pool (cluster smoke-r7
  candidate-root-cause table), ~80 concurrent wedged wakes
  exhaust the pool; legitimate non-wake controller endpoints
  (livez, route-registry pull, metering report) start failing
  with pool-acquisition timeouts.
- **Threat model**: amplification multiplier on R14-S1 / any
  future R14-class regression. Without R14-S1 actually
  triggering, R14-S2 is dormant. Bounded-bad DoS gated by
  admin-auth.
- **Why MINOR posture and not IMPORTANT**: (1) requires a
  starvation-source trigger from R14-S1 first; (2) the
  amplification is bounded by pg pool size which is operator-
  tunable; (3) the 120 s budget envelope is a designed-in
  bound — the retry succeeds bounded-late, not bounded-wrong,
  even under amplification. The structural posture is worth
  noting because the C-4 fix's design point ("envelope the
  source teardown") only holds if the teardown is *actually
  making progress*; under runtime starvation it isn't.
- **Action**:
  (a) Capacity-planning input: the documented `max_connections`
      on the pg pool should be ≥ 2 × (worker_count × VM_INDEX_CEIL)
      to envelope the worst-case "all wakes wedged in C-4
      retry" scenario. At 5 workers × 12 slots = 60 wakes max,
      `max_connections ≥ 120` would prevent the pool-exhaustion
      amplification.
  (b) Bound C-4's retry on its slot-acquisition attempt count
      to release the pg connection if not already released
      (the current code path at `:265-306` holds the
      restore-row context across the retry, including any
      acquired pg connection). Re-acquire fresh on each retry
      attempt to free the pool for other endpoints.
  (c) Belt: add a metric
      `restore_retry_budget_exhausted_total{outcome}` so an
      operator can detect amplification from the metrics
      surface independently of the pg-pool's own metrics.

## Verified open carry-forward (unchanged at HEAD `91ce9be5`)

- **R9-S1** (CRITICAL → currently UNREACHABLE on cluster) —
  `nomad-vm-wrapper.sh:476-498` anchored prefix regex on the
  AEAD config.json carve-out. AEAD layer remains OFF in
  cluster (`SANDBOX_SNAPSHOT_ROOT_KEK_PATH` not set by
  `gcp-worker-startup.sh`; A1 in deferred). Cluster smoke-r7
  re-verified by trace: SNAPSHOT now reaches `store.put` end-
  to-end (post-C-3), but AEAD layer still bypassed. Once A1
  lands, R9-S1 becomes immediately attacker-reachable. Carry
  forward.
- **R9-S2** (IMPORTANT) — `snapshot_aead.rs::derive_dek` /
  `derive_nonce_prefix` keyed on 1-second timestamp.
  Re-snapshot within same wall-clock-second → nonce reuse.
  Unchanged at HEAD.
- **R9-S3** (IMPORTANT) — `snapshot_handler.rs:417` writes
  `Some("v1")` into `snapshot_aead_dek_id` regardless of
  whether AEAD root KEK is present. With cluster AEAD OFF,
  every snapshot row in pg is mislabeled as `dek_id=v1` while
  the artifact is plaintext. Unchanged at HEAD (`Some("v1")`
  still on the only emitting line, verified by
  `grep -n 'Some("v1")' snapshot_handler.rs` → line 417 sole
  hit).
- **R9-S5** (IMPORTANT → partially closed) — restore env block
  lacks `ZSBX_SANDBOX_ID` under raw_exec mode
  (`restore_handler.rs:1334-1349`). ChPlugin partial-close via
  typed `sandbox_id` Config field at `:1403` (r12 finding).
  Unchanged.
- **R10-S1** (IMPORTANT) — 5 secret-file loaders all use
  `std::fs::metadata` (follows symlinks) then a separate
  `read`/`open` that re-resolves. Re-verified at HEAD by
  enumerating the 36 `std::fs::metadata` call-sites; 5 secret-
  loader sites unchanged.
- **R10-S2** (IMPORTANT) — `restore_handler.rs:294-298`
  `let _ = spawn_blocking(...).await`. JoinError swallow
  unchanged.
- **R10-S3** (MINOR) — `restore_handler.rs:1168-1211`
  `teardown_restore` step (3) `release_vm_index` runs
  regardless of step (1) `nomad_delete_blocking` outcome.
  Unchanged.
- **R11-S3** (MINOR posture) — `snapshot_aead.rs::chunk_aad`
  binds only `"zsbx-snap" || chunk_index`; not sandbox_id /
  taken_at. Unchanged.
- **R12-S1** (IMPORTANT → partially closed) —
  `db.rs::ENV_LOCK` (9 keys) + `nomad_ch.rs::
  TASK_DRIVER_ENV_LOCK` (1 key) still disjoint per-env-key
  mutexes. Rust-2024 stdlib `set_var` contract still violable
  test-side under concurrent disjoint-key mutation. Unchanged
  from r13.
- **R13-S1** (IMPORTANT) — worker VM GCS scope `storage-rw` on
  the default Compute Engine SA. Provision script unchanged.
- **R13-S2** (MINOR posture) — no per-bearer rate-limit on
  snapshot/wake. C-6 fix's amplification multiplier (R14-S2)
  is in addition to this carry-forward, not a replacement.
- **R9-S6 / S7 / S8** (MINOR) — `/_clock_resync` agent-body
  journald leak, `/livez|/readyz|/metrics` unauthenticated,
  admin endpoints lack per-bearer rate-limit. Unchanged.

## Closed by recent commits since r13

- **C-6** (cluster smoke-r7 → r8 gate) at `91ce9be5` —
  `admin_handlers.rs::snapshot_sandbox` detached teardown now
  on dedicated OS thread + per-thread compio runtime,
  mirroring C-3. **Security delta: none** (no privilege change;
  zero new unsafe; same uid/gid/cwd; same VM-instance-metadata
  OAuth identity). The structural class is *partially* closed
  — R14-S1 above flags the sibling-site gap.
- **R11-API1 expanded** at `370fdbba` — 3 orphan `#[doc(hidden)]
  pub` fns deleted from `metrics.rs`. No security relevance
  (the fns were `AtomicU64::load` test accessors with zero
  attack-surface contribution; deletion is API hygiene, not
  security).
- **R14-API1** at `00161cea` — `with_nomad_handle` +
  `with_shared_allocator` demoted from `pub` to `pub(crate)`.
  Reduces test-only API surface; no production reachability
  change.
- **C-6 phase tracing** at `8e7f0b53` — INFO-level trace at 14
  phase boundaries on the wake path. Log-volume DoS surface
  ruled out per hunt-list #3.

## Counts

- CRITICAL: 0 new (R9-S1 carry, still unreachable on cluster
  because AEAD layer is OFF).
- IMPORTANT: 1 new (R14-S1); carry: R9-S2, R9-S3, R9-S5
  (raw_exec arm), R10-S1, R10-S2, R12-S1 (partial), R13-S1.
- MINOR: 1 new (R14-S2); carry: R10-S3, R11-S3, R13-S2,
  R9-S6/S7/S8.
- Total NEW this round: 2.

r13-closed at HEAD: 1 (C-6 snap-teardown arm at `91ce9be5`;
sibling-site arm remains as R14-S1). r9-carry: 7 (R9-S1/S2/S3/
S5/S6/S7/S8). r10-carry: 4 (R10-S1/S2/S3 + S6 unchanged from
r13). r11-carry: 1 (R11-S3). r12-carry: 1 (R12-S1 partial).
r13-carry: 2 (R13-S1, R13-S2).
