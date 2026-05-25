# Sandbox/snapshot-restore — concurrency r26 review

Date: 2026-05-25 (UTC).
HEAD at audit: `d0d7abd6` (worktree
`.worktrees/sandbox-snapshot-restore`, branch
`feat/sandbox-snapshot-restore`). Round 26 of N. READ-ONLY.

Scope since r25 (`a482f00d` → `d0d7abd6`):

- **r3-A node-affinity 4-commit chain** (`883df7fe` + `9b623f44` +
  `d71f1a8c` + `b562d3a1`) — new
  `Backend::from_config_full(cfg, persist, node_id)` entry point,
  boot-time `fetch_local_nomad_node_id` lookup, jobspec Constraints
  emission on cold-boot + restore paths. Closes T-8b-stress-r3's 78%
  cross-node placement failure at WORKER_COUNT=3.
- **R22-S1 sanitize widening** (`7647cd4d`) — `strip_filesystem_paths`
  + `strip_typed_ids` passes added to `sanitize_error_message`. Pure
  fn over `&str`, no concurrency surface.
- **R25-T4 sweeper test-helper extraction** (`901dfbf2`) — splits
  `run_host_dir_gc_once` into pure `classify_host_dir_entry` (FS
  gates) + `host_dir_eligible_by_db` (DB gates) helpers; 12 new lib
  tests covering the eligibility matrix.
- **R24-A1 grace tighten** (`34b52cf1`) — `HOST_DIR_GC_GRACE_SECS`
  default 3600 → 600. Constant-only change. No concurrency surface.
- **R3-C / R20-S3 / driver v15 / controller v35** — scripts-only,
  out-of-scope for this lens.

In flight: stress-r4 cluster sprint (`crates/sandbox/scripts/*` only;
no controller-source overlap with this lens).

Prior: `…concurrency-2026-05-25-r25.md`.

## Summary

- **5 findings** (0 new CRITICAL, 1 new IMPORTANT, 3 MINOR, 1
  defer).
- **R25-I1 (sweeper TOCTOU invariant untested) — STILL OPEN**.
  No new pg-gated test for terminal-sandbox refuse + no new
  pure-unit test for the terminal triad lifecycle pin since r25
  close. The two-test ask from r25 is still the right hand-off;
  arch-r25-A6 owner unchanged.
- **R25-I2 (Phase::Ok terminal-overwrite divergence) — STILL OPEN**.
  No changes to `wake_machine.rs:120-156` (the `Phase::Ok`
  terminal-write site). The R22-S1 widening (`7647cd4d`) touches
  only `sanitize_error_message`; the divergence shape is unchanged.
  Carry to r27.
- **R26-I1 (NEW IMPORTANT) — r3-A boot lookup is a 5-second
  serial `await` on cold Nomad-agent paths; controller restart
  latency increases by up to 5s + 1 connection-establish RTT
  before any subsequent boot work**. Sound mitigation (`Err` is
  non-fatal, counter bumps) but boot now has a new external
  dependency on `/v1/agent/self` reachability. The window matters
  for `systemd` restart-on-failure loops where short bootloops
  amplify the perceived outage. See R26-I1.
- **R26-M1 (NEW MINOR) — `Backend::from_config_full` threads
  `local_nomad_node_id` consistently across 3 constructors;
  reverse-pass verified clean**. All three call sites
  (`from_config_full`, `from_config_with_persist` → `None`,
  `from_config` → `None`) install `Option<String>` immutably on
  `NomadCHBackend`. No race where boot completes but the value is
  None mid-creation. See R26-M1.
- **R26-M2 (NEW MINOR) — R25-T4 helper extraction did NOT
  introduce any race the integrated form prevented**. The
  metadata read remains the single lock-of-truth on the readdir
  entry; the pure helper accepts decomposed primitives but the
  caller still owns `entry`'s lifetime through the rm-rf call.
  See R26-M2.
- **R26-M3 (NEW MINOR) — R26-I1 (code-quality r26 IMP)
  `read_snapshot_row` DRY — concurrency angle CLEAN**. Both
  `SnapshotRowMeta` (sync path) and `WakeSnapshotMeta` (machine
  path) use the same READ COMMITTED single-statement SELECT
  against `sandbox.sandboxes`. Both are equally race-unaware
  (no FOR UPDATE, no generation predicate) — the duplication is
  a code-quality concern but NOT a concurrency divergence. See
  R26-M3.
- **R25-C1 (perf-r25 CRITICAL: pool-per-call at c=20)
  — DEFER from concurrency lens**. The pool-per-call cost is a
  perf concern; the concurrency-lens recheck ("does pg
  backpressure cause unsafe state in wake_machine?") finds NO
  unsafe state. Every pg-error path through
  `read_snapshot_row` / `update_sandbox_status` /
  `update_wake_job_state` routes to a typed error → `Phase::Failed
  { code: Internal | RestoreFailed, message: ... }` → terminal
  write with sandbox-row rollback. See R26-DEF1.

## CRITICAL

None this round. The r3-A node-affinity bundle is concurrency-sound:
the boot lookup is serial-but-bounded (5s timeout), the field
plumbing is immutable post-construction, and the
`from_config_full` constructor is the single legal write path. The
R25-T4 helper extraction preserves the integrated form's
lock-of-truth (single `entry.metadata()` call per dir).

## IMPORTANT

### [R25-I1] (carry — UNCHANGED) Sweeper TOCTOU invariant load-bearing but **never tested**

- No code changes to `sweep.rs:1038-1102`, `admin_handlers.rs:1728-
  1771`, `db.rs:3127-3193` since r25 close.
- No new pg-gated test pinning the "wake handler refuses
  terminal sandbox + zero wake_jobs inserted" invariant.
- No new pure-unit test extending
  `recovery_target_pins_proposal_table` (sweep.rs:1513-1531) to
  the terminal triad
  (`Stopped`/`Lost`/`Orphan` → no outgoing transition).
- The 12 new R25-T4 tests at `sweep.rs:1580-1849` pin the
  **eligibility matrix** (FS gates + DB-eligibility helper) but
  do NOT pin the handler-side invariant that closes the TOCTOU
  window. They're correct tests for the destination they were
  written for (the eligibility helpers); the R25-I1 ask is one
  layer above.
- **Status**: STILL OPEN. The arch-r25-A6 hand-off and the
  concurrency-r25 two-test pin remain the right shape.
  Concurrency-r26 raises no new objection; promotes the test-
  coverage ask from r25 to r26 carry list.
- **Carry to r27** IF stress-r4 surfaces no host_dir-related
  symptoms; promote to CRITICAL IF an operator-triage incident
  surfaces a "host_dir reaped while wake in-flight" shape.

### [R25-I2] (carry — UNCHANGED) `Phase::Ok` terminal write blocked by sweep → sandbox row already `Running` + vm_index held → cluster diverges from wake_jobs row

- No code changes to `wake_machine.rs:120-156` (the Phase::Ok
  terminal-write site) since r25 close.
- The R22-S1 widening (`7647cd4d`) added two passes to
  `sanitize_error_message` but it's a pure-fn change on the
  failed-arm code path (the Ok arm passes `None` for message and
  doesn't call sanitize at all — verified at wake_machine.rs:133-
  136).
- **R20-I3 / R21-I1 / R24-M2 Restoring-phase watchdog**: STILL
  OPEN (8th cycle). The cold-cache `store.get` is still a
  multi-second blocking call with no `lessee_updated_at = now()`
  heartbeat. Stress-r4 will pressure this if WORKER_COUNT=3 with
  cold blob cache survives the r3-A landing. Currency-r26
  endorses r25's promotion-IF condition: if stress-r4 surfaces
  ONE Restoring-phase >60s, promote watchdog to IMPORTANT.
- **Status**: STILL OPEN. Arch-r25 owns the design call on
  current-behavior vs atomic two-row CAS. No new evidence; no
  delta from r25.
- **Carry to r27**.

### [R26-I1] (NEW IMPORTANT) Boot lookup is a serial 5-second `await` that adds external dependency on `/v1/agent/self` reachability

- **Files**:
  - `crates/sandbox/src/lib.rs:670-698` — boot-path lookup site.
  - `crates/sandbox/src/backend/nomad_ch.rs:3144-3157` —
    `fetch_local_nomad_node_id` body (`http_get_unsigned` with
    `Duration::from_secs(5)`).
  - `crates/sandbox/src/backend/nomad_ch.rs:3203-3211` —
    `http_get_unsigned` uses `compio::runtime::spawn_blocking`.
- **The shape**:
  ```rust
  // lib.rs:670-698 — inside AppState::from_config, BEFORE
  // Backend::from_config_full / backend.probe() / orphan
  // cleanup / restore-at-startup.
  let local_nomad_node_id: Option<String> =
      match crate::backend::nomad_ch::fetch_local_nomad_node_id(
          &config.nomad_ch.nomad_addr,
      )
      .await
      {
          Ok(id)  => { /* trace + Some(id) */ },
          Err(e)  => { /* metric + warn + None */ },
      };
  ```
- **Concurrency impact**:
  1. **Boot is sequential**. The `.await` blocks the
     controller's compio runtime startup task for the full HTTP
     RTT (up to the 5s ureq timeout) before any subsequent boot
     work (backend probe, orphan cleanup, restore-at-startup,
     ntex bind) can begin.
  2. **Failure path is sound but slow**. If the local Nomad
     agent is unreachable (firewall block, agent restart in
     progress, DNS hiccup), the boot path waits for the full 5s
     timeout, bumps the counter, logs a WARN, and proceeds with
     `None`. The fail-non-fatal stance is correct per the commit
     message — a controller restart shouldn't fail because the
     local Nomad restarted. BUT the user-visible "controller
     reboot took 30s longer than usual" surfaces as an outage
     amplifier in `systemd` restart-on-failure loops.
  3. **No retry, no parallel-probe**. The boot lookup is
     fire-once-and-move-on. If the agent comes up 100ms after the
     5s timeout fires, the field stays `None` for the entire
     controller uptime and the operator-bumped metric is the
     ONLY signal. **Recovery requires a full controller
     restart** — not the same shape as the periodic health-probe
     loop (`backend::Backend::probe`) which retries.
  4. **No degradation handshake with the restore-at-startup
     path**. The restore-at-startup loop at lib.rs:733-742 fires
     while `local_nomad_node_id == None` if boot lookup failed.
     Every restart-restore submitted by that loop omits the
     Constraints block, falling back to random cross-node
     placement at exactly the moment a controller is most
     vulnerable (post-restart, no warm caches).
- **Practical likelihood**:
  - Cold-boot the cluster: every controller startup hits this
    path; if the local Nomad agent is also cold-booting, race
    is possible. On GCE / EC2 cold-boot times for Nomad agent
    are typically <2s; the 5s timeout has comfortable
    margin.
  - Rolling restart: controller restart with Nomad agent steady
    — RTT is <50ms, no measurable boot impact.
  - Operator-triage / pager-bait scenario: Nomad agent crashes
    AND restarts during the controller's restart window. Window
    is sub-30s in the worst case (Nomad's own startup is
    fast); controller boot lookup may catch the agent's "not
    yet listening" window and fail with `None`. Subsequent
    wake-path placement degrades silently to random until
    operator notices the metric and triggers a fresh controller
    restart.
- **What watching catches**: the
  `sandbox_nomad_node_id_lookup_failures_total` counter (added
  at `883df7fe`). Operator alerts on `rate > 0` catch the
  degraded shape. **GAP**: the counter is monotonic and
  cumulative — a controller that successfully looked up node-id
  on first boot but later restarted into a failure window
  doesn't reset. A delta-rate alert handles this; an absolute-
  value alert does not.
- **Recommendations** (in increasing order of cost):
  1. **Doc-comment** at lib.rs:670 naming the 5s worst-case boot
     latency cost (cheap; 3 LOC; closes operator-triage
     puzzlement when "controller boot took 30s longer than
     usual" surfaces in a postmortem).
  2. **Surface the value on `/readyz`** so an external probe can
     gate "controller is fully cluster-functional" on the node-
     id-lookup-success shape, distinct from "controller binary
     is listening." (medium; ~10 LOC; arch hand-off).
  3. **Background-retry loop** alongside the existing
     `backend::Backend::probe` periodic loop, refreshing the
     value when it transitions from `None → Some`. (larger;
     ~40 LOC; requires `AppState.local_nomad_node_id` to become
     interior-mutable behind a `parking_lot::RwLock` or
     `arc_swap::ArcSwap`. **THIS IS A SHAPE CHANGE** — the
     field is currently set ONCE at construction; making it
     hot-mutable means every read site needs an explicit
     `.read()` / `.load()`. Threads through `RealRestoreBackend.
     local_nomad_node_id` and `NomadCHBackend.local_nomad_node_id`
     too.)
- **Severity**: IMPORTANT. The boot path now has a new external
  dependency, the failure mode silently degrades restore
  placement, and there's no recovery without an operator
  restart. The path of least cost is recommendation (1) — a
  doc-comment + the existing metric — but recommendation (3) is
  the right long-term shape. Arch-r26 owns the call.
- **Cross-lens**:
  - **perf r26**: the 5s timeout dominates boot latency on
    cold-Nomad-agent paths; is the timeout the right floor? A
    1s timeout would be friendlier for systemd restart loops,
    at the cost of more spurious `None` falls.
  - **test-coverage r26**: there is no fault-injection test
    pinning the boot path's behaviour when
    `fetch_local_nomad_node_id` fails. The parser shape tests
    landed at `883df7fe` pin the
    `parse_nomad_agent_self_node_id` Err shapes (good), but the
    integration with the boot path's metric bump + WARN logging
    is untested. ~30 LOC mocking a 404 / 500 / timeout response.
- **Carry mechanism**: NEW for r26. Arch hand-off for the
  shape-change call; concurrency owns the description.

## MINOR

### [R26-M1] `Backend::from_config_full` threads `local_nomad_node_id` consistently across 3 constructors — no race where boot completes but value is None mid-creation

- **Files**:
  - `crates/sandbox/src/backend/mod.rs:188-234` —
    `Backend::from_config` / `from_config_with_persist` /
    `from_config_full`.
  - `crates/sandbox/src/lib.rs:700-704` — production caller.
- **The pin task**: "do all 3 constructors thread the
  `local_nomad_node_id` consistently? Any race where boot
  completes but the value is None mid-creation?"
- **Analysis**:
  1. The three constructors form a delegation chain:
     ```
     from_config(cfg)
       → from_config_with_persist(cfg, None)
           → from_config_full(cfg, None, None)
     ```
     Each constructor adds default-`None` for the parameters it
     doesn't expose. The "no race" is structural: there is no
     intermediate state where a backend exists but is missing
     its `local_nomad_node_id`. The `NomadCHBackend::new(cfg,
     persist)?` constructor sets `local_nomad_node_id: None` by
     default (nomad_ch.rs:407), and the builder
     `with_local_nomad_node_id(value)` either replaces it with
     `Some(id)` (when boot succeeded) or with `None` (when boot
     failed). Either way, the resulting `Backend::NomadCh(Arc<
     NomadCHBackend>)` is fully-constructed and atomically
     handed to `AppState`.
  2. **The boot path holds the only legal write**. Once the
     `Arc<NomadCHBackend>` lands in `AppState.backend`, the field
     is immutable. No `with_local_nomad_node_id_mut` getter or
     `&mut self` setter; the builder takes `mut self` (owned
     consumption) and returns `Self`. After `Backend::NomadCh(
     std::sync::Arc::new(...))` the `Arc` makes interior
     mutation impossible without an `Arc::get_mut` (and the
     immediate `Arc::new` wrap means refcount is already 1, so
     `Arc::get_mut` would work — but no code path attempts it).
  3. **Restore-side mirror**: `RealRestoreBackend` is a sibling
     struct that also stores `local_nomad_node_id: Option<String>`
     and is built via `.with_local_nomad_node_id(...)` at
     lib.rs:958. Same shape: `Option<String>` set once,
     `RealRestoreBackend` wrapped in `Arc<dyn RestoreBackend>`,
     immutable thereafter.
  4. **The two clones**: lib.rs:703 (`local_nomad_node_id.clone()`
     for the cold-boot backend) and lib.rs:958
     (`local_nomad_node_id.clone()` for the restore backend) — both
     clones hand out **fresh `String` allocations** (or `None`),
     so the two backends do NOT share interior state. A future
     refactor that tried to "make the field mutable on the
     backend" would have to install a shared `Arc<RwLock<…>>`
     and update both clone sites; otherwise the two backends
     would diverge.
  5. **Test-fixture safety**: the lib-test `new_fixture` defaults
     the field to `None` (per commit `883df7fe`'s commit message);
     unit tests that exercise the field set it via plain
     assignment after construction. This is the legitimate
     "no boot lookup ran" shape — equivalent to the boot
     failure path.
- **Severity**: MINOR (confirmation). No action required.
- **Carry mechanism**: NEW for r26 (confirmation note); part of
  the r3-A consensus.

### [R26-M2] R25-T4 helper extraction did NOT introduce any race the integrated form prevented — the readdir entry is still the lock of truth

- **Files**:
  - `crates/sandbox/src/sweep.rs:1014-1223` —
    `run_host_dir_gc_once` body.
  - `crates/sandbox/src/sweep.rs:948-1000` — the two pure
    helpers (`classify_host_dir_entry`, `host_dir_eligible_by_db`).
- **The pin task**: "did splitting `run_host_dir_gc_once` into
  pure helpers introduce ANY race that the integrated form
  prevented? E.g., the integrated form had a single lock-of-
  truth on `entry.metadata()`; the helper form re-reads or
  accepts metadata as a param?"
- **Analysis**:
  1. **The single-metadata-read is preserved**. The integrated
     form (pre-r25-T4) called `entry.metadata()` once at the
     top of each loop iteration, extracted `mtime_secs` +
     `is_dir`, and used them through to the rm-rf decision.
     The new shape (post-r25-T4) at sweep.rs:1074-1091 keeps
     the SAME single `entry.metadata()` call inside the loop,
     decomposes the result into `(mtime_secs, is_dir)` LOCALS,
     and passes them to `classify_host_dir_entry(name, is_dir,
     mtime_secs, now_secs, grace_secs)`. The helper is pure —
     it does NOT re-stat the path, it does NOT take an
     `entry` reference, it consumes only `&str` + bools +
     u64s.
  2. **`entry.path()` is read AFTER the classifier returns
     `Candidate`** (sweep.rs:1197). The path comes from the
     same `entry` handle whose `metadata()` produced the
     `mtime_secs` used in the classifier — i.e., the rm-rf
     target IS the same dir whose mtime we just checked, no
     fresh readdir or path resolution in between.
  3. **No race window introduced**. The integrated form's
     lock-of-truth was: "the `entry` from `read_dir().next()`
     binds name, metadata, and path to the same on-disk
     handle for the lifetime of the loop iteration." The
     helper form preserves this — the helper accepts data
     extracted from the entry but does NOT replace the entry
     as the lifetime owner. **The classifier is a pure
     classifier on already-extracted data, not a fresh FS
     read.**
  4. **Where COULD a race have been introduced**: if the
     refactor had moved `entry.metadata()` INTO the classifier
     (taking `entry: &DirEntry`), the classifier would have
     re-stat'd the path, opening a window between the readdir
     and the stat where the dir could be replaced. The actual
     refactor did NOT do that. Verified by reading both forms.
  5. **DB-side helper**: `host_dir_eligible_by_db(row, has_pending_wake)
     -> bool` accepts already-fetched data; the two SELECTs
     (`get_sandbox_row` + `find_pending_wake_for_sandbox`)
     happen in the loop body at sweep.rs:1133-1173, BEFORE
     the helper is called. This is the **same SELECT pair**
     the integrated form did, in the same order, with the
     same TOCTOU shape — i.e., R25-I1's window is preserved
     (which is correct; R25-T4 was a test-coverage refactor,
     not a race-fix). The helper extracted the eligibility
     PREDICATE, not the SELECT timing.
  6. **The 12 new tests** (sweep.rs:1601-1849) drive the
     helpers with synthetic inputs; they pin the eligibility
     decision logic against future refactors (e.g., adding a
     new `SandboxStatus` variant without updating the terminal
     set, dropping `users` from the FS skip list). They do
     NOT need to pin race-shape because the race-shape lives
     in the integrated loop, not the helpers.
- **Severity**: MINOR (confirmation). The extraction is
  concurrency-clean.
- **Carry mechanism**: NEW for r26 (confirmation).

### [R26-M3] `read_snapshot_row` DRY — concurrency angle CLEAN (both paths equally race-unaware)

- **Files**:
  - `crates/sandbox/src/restore_handler.rs:748-806` —
    `SnapshotRowMeta` + sync-path `read_snapshot_row` (selects
    `artifact_path, sha256, vm_index, user_id`).
  - `crates/sandbox/src/wake_machine.rs:712-769` —
    `WakeSnapshotMeta` + machine-path `read_snapshot_row`
    (selects `sha256, vm_index, user_id` — no `artifact_path`).
- **The code-quality-r26 R26-I1 question (this lens's recheck)**:
  "any concurrency angle (one race-aware, the other not)?"
- **Analysis**:
  1. **Both SELECTs are single-statement `query_opt`**:
     ```sql
     SELECT … FROM sandbox.sandboxes
       WHERE sandbox_id = $1::TEXT AND deleted_at IS NULL
     ```
     Pool acquisition (`pool_app().await + pool.get().await`)
     is identical between the two functions. Same isolation
     level (READ COMMITTED, the workspace default), same MVCC
     snapshot timing (statement-start of the SELECT).
  2. **Neither takes a row lock**. No `FOR UPDATE`, no
     `FOR SHARE`. Both are pure reads, returning the
     last-committed version per row.
  3. **Neither checks `generation` for read-skew**. If the
     row is concurrently being updated (e.g., the wake_machine
     CAS to `Restoring` at wake_machine.rs:285 is racing), the
     SELECT returns whichever side of the UPDATE committed
     first. **Both functions are equally exposed to this**
     race — neither carries a generation predicate, neither
     wraps the read in a transaction with the subsequent
     CAS.
  4. **The race itself is bounded by the sandbox-row state
     machine**. The CAS to `Restoring` only succeeds when the
     row's current state is `Snapshotted`/`SnapshottedSuspect`
     AND `generation == g0` (where `g0` is the value the
     reader observed). So a stale read of `snap.sha256` or
     `snap.vm_index` doesn't cause an inconsistent CAS — the
     subsequent `update_sandbox_status(Restoring, g0, …)`
     fails-CAS if any update raced, surfacing as `Err` →
     `Phase::Failed { code: Internal, message: "CAS
     snapshotted→restoring" }`.
  5. **Divergence between the two readers is a code-quality
     concern, NOT a concurrency one**. The sync-path reader
     fetches `artifact_path` (used by `do_restore_inner` at
     restore_handler.rs:813); the machine-path reader does NOT
     fetch `artifact_path` because the machine derives the
     artifact path from `cfg + sha256` directly. A future
     evolution where the artifact-path column becomes
     load-bearing in the machine path would need to add a
     fourth column to the machine reader — a divergence
     bug, but not a race.
- **Severity**: MINOR (confirmation; concurrency lens raises
  no objection to the duplication). Code-quality r26 owns the
  DRY ask; arch-r26 owns the question of whether the two
  readers should converge (e.g., bump `SnapshotRowMeta` to
  `pub(crate)`, retire the machine-side copy).
- **Carry mechanism**: NEW for r26 (cross-lens
  confirmation).

## DEFER

### [R26-DEF1] Perf-r25 C1 (pool-per-call at c=20 stress, 280 conns/sec) — concurrency-lens recheck CLEAN; defer to perf-r26

- **Files**:
  - `crates/sandbox/src/db.rs:543-557` — `open_pool` /
    `pool_app` (per-call transient pool open).
  - `crates/sandbox/src/db.rs:525-542` — the R11-P1 deferral
    comment.
- **The concurrency-lens question (recheck of perf-r25 C1)**:
  "same code path that performance flagged, but lens is 'what
  happens when conns saturate' vs 'what's the cost per call'.
  Does pg backpressure cause unsafe state in wake_machine?"
- **Analysis**:
  1. **Every pg call is fallible**. Both pool acquisition
     paths (`open_pool` → `Pool::connect_with_config`) and
     client acquisition (`pool.get()`) return
     `Result<_, DatabaseError>`. On error:
     - `read_snapshot_row` (restore_handler.rs:768-787) →
       `RestoreHandlerError::Internal(format!("pool_app: {e}"))`
       → caller routes to `Phase::Failed { code: Internal }`
       → terminal write with sandbox-row rollback (vm_index
       released, sandbox row CAS'd back from `Restoring`).
     - `read_snapshot_row` machine-path (wake_machine.rs:738-
       739) → `Err(String)` → `Phase::Failed { code: Internal,
       message: ... }` → terminal write (sandbox row
       UNCHANGED in `Snapshotted` — the failure is pre-CAS).
     - `update_sandbox_status` (wake_machine.rs:286-294) → CAS
       failure routes to `Phase::Failed` (sandbox row left in
       whatever state the database committed last).
     - `update_wake_job_state` (wake_machine.rs:130-156) → on
       error logs `tracing::error!` and returns Ok-but-stale;
       the wake_jobs row is left at its last in-flight state
       until the takeover sweep claims it. Sandbox-row +
       vm_index state is independent of this write.
  2. **No fast-path that bypasses error handling**. Every
     query_opt / execute in the wake state machine is
     wrapped in `?` or explicit `match` — no `.unwrap()`,
     no swallowed errors that would leave the machine in
     a half-written state.
  3. **Pg-backpressure surface = `Pool::connect_with_config`
     timeout**. Under c=20 stress with 280 conns/sec, the pg
     server may refuse new connections (max_connections
     hit) → `connect_with_config` returns `Err` → the wake
     machine surfaces `Phase::Failed { code: Internal,
     message: "pool_app: ..." }`. The terminal write to
     `wake_jobs` is also a pg call which may itself fail —
     but the failure-of-terminal-write is already covered
     by the `tracing::error!` arm at wake_machine.rs:206-
     211, which leaves the wake_jobs row at its last
     in-flight state for the takeover sweep to claim.
  4. **No unsafe state is reachable from pg-backpressure**.
     The unsafe state the question is probing for ("a
     half-written wake that leaves a live VM with a
     `failed` audit row" — R25-I2's shape) is reachable
     from sweep-races, NOT from pg-backpressure. Pg
     backpressure fails the SELECT side; sweep-races
     succeed on the wake_machine side but get overwritten
     by the sweep's claim. Different shapes.
  5. **The 280-conns/sec figure is a PERFORMANCE concern**
     (cost-per-restore, connection-pool exhaustion at the
     pg side, listen-queue overflow on the controller's
     ntex worker). The concurrency-lens recheck finds no
     correctness defect; the wake machine handles
     pg-backpressure as just another `Phase::Failed`.
- **Severity**: DEFER (no concurrency finding). Perf-r26
  owns the cost-per-call ask (R11-P1 is the long-standing
  deferral).
- **Carry mechanism**: NEW for r26 (cross-lens recheck;
  confirms no concurrency angle).

## Items NOT findings (verified clean this round)

### [N/A] Driver v15 `waitForTapAbsent` cross-process race (ip link delete async release)

- Cross-worktree (nomad-driver-ch v15 lives outside this
  worktree). The R20-S3 SHA256 verify in
  `scripts/gcp-worker-startup.sh` is sourced from the v15
  driver binary; the controller-side code does NOT observe
  the tap-link-release timing directly.
- The closest observability is in the wake_machine via
  `wait_for_livez` (which polls the agent's HTTP server
  inside the restored VM). The driver's tap release is
  upstream of livez — if the tap isn't fully released, the
  agent never starts, and `wait_for_livez` times out → the
  machine rolls back via `rollback_with(AgentLivezTimeout)`
  at wake_machine.rs:438-470, releasing the vm_index.
- **No double-stop pattern observable from the controller**.
  The controller's `RealRestoreBackend::teardown_restore`
  (called from `rollback_and_classify`) issues a Nomad job
  delete; the driver's tap-release is its own concern.
- **Concurrency-r26 raises no objection** — the v15 driver-
  side change is invisible to the controller's state machine.

### [N/A] R22-S1 sanitize widening

- `7647cd4d` adds `strip_filesystem_paths` + `strip_typed_ids`
  passes to `sanitize_error_message`. Pure fn over `&str`,
  invoked on the failed-arm path only (wake_machine.rs:165-
  181). Idempotent (per the new
  `sanitize_idempotent` test). No concurrency surface.

### [N/A] R24-A1 grace tighten (3600 → 600)

- `34b52cf1` is a constant-only change to
  `HOST_DIR_GC_GRACE_SECS`. No concurrency surface — the
  sweep cadence (5 min) is unchanged, the grace floor (60s)
  is unchanged. The tighter grace means terminal-state
  host_dirs are eligible sooner; the R25-I1 invariant
  (handler refuses terminal sandbox BEFORE wake_jobs
  insert) still closes the same window structurally.

## Cross-lens consensus

- **arch-r26 owns** the design call on **R26-I1 boot-lookup
  shape** (current `Option<String>` set-once vs. interior-
  mutable with periodic refresh) and the carry of R25-I1 /
  R25-I2 design questions from r25.
- **test-coverage r26 owns** the R25-I1 two-test pin and a
  new ask: integration test for R26-I1's
  `fetch_local_nomad_node_id` boot-failure path (parser-shape
  tests landed at `883df7fe` are GREEN; boot-integration tests
  are missing).
- **perf-r26 owns** the R26-DEF1 R11-P1 deferred fix (pool-
  per-call cache) and the R26-I1 timeout-value call (5s vs
  1s).
- **code-quality r26 owns** the R26-M3 DRY ask
  (`SnapshotRowMeta` vs `WakeSnapshotMeta` consolidation).
- **security r26**: no new vector. The R22-S1 widening closes
  the path-leak shape via sanitize; concurrency lens raises
  no parallel finding.
- **Cluster T-8b-stress-r4** (in flight, scripts/-only):
  concurrency-r26 has no script-side objections. The r3-A
  Constraints emission closes the cross-node placement race
  that caused stress-r3 to RED at 1/60.

## Carry table

| Finding | Source | r26 state |
|---|---|---|
| R19-C1 wedge-half | r19 → CLOSED r20 | CLOSED carry |
| R20-C1 terminal-overwrite (Failed side) | r20 → CLOSED r22+r23 e2e | fully CLOSED |
| R20-C1 (Ok side) — R25-I2 | r25 NEW-IMP | **STILL OPEN** (carry to r27); no code change |
| R20-I1 STATIC_NAMES | r20 → CLOSED | CLOSED carry |
| **R20-I2** sweep host-scoping | r20 NEW-IMP | **STILL OPEN — 8th cycle** |
| **R20-I3 / R21-I1 / R24-M2** Restoring watchdog | r20→r21→r22→r23→r24→r25→r26 | **STILL OPEN — 8th cycle** |
| R22-I1 observability-gap | r22 → CLOSED r23 | CLOSED carry |
| R22-M3 cross-controller claim | r22 minor | carry |
| R23-I1 terminal→terminal e2e (Failed side) | r23 → CLOSED r24 | CLOSED carry |
| R24-I1 fsync_dir doc-misframe | r24 NEW-IMP | **STILL OPEN — code-quality r26 owns** |
| R24-I2 lib-test flake | r24 → DEMOTED r25 | CLOSED-with-watchful-eye |
| R24-M1 sweep × machine race-into-terminal | r24 minor | carry |
| R24-M3 stranded taps (C-N-W2) | r24 minor | carry |
| R24-M4 Phase::Failed Ok(0) log payload | r24 → CLOSED r25 | CLOSED carry |
| R24-M5 detach migration sound | r24 minor | CLOSED carry |
| **R25-I1** sweeper TOCTOU invariant untested | r25 NEW-IMP | **STILL OPEN** (carry; no test landed) |
| **R25-I2** Phase::Ok terminal-overwrite divergence | r25 NEW-IMP | **STILL OPEN** (carry; no code change) |
| R25-M1 typed StagingPathMissing carry-through atomic | r25 minor | carry (doc-comment ask) |
| R25-M2 AdminRole::ReadOnly read consistency | r25 minor | carry (no action) |
| R25-M3 R24-I2 lib-test flake | r25 minor | CLOSED-with-watchful-eye |
| **R26-I1** boot-lookup serial-await + external-dep | r26 NEW-IMP | NEW |
| R26-M1 from_config_full threading consistent | r26 minor | NEW (confirmation) |
| R26-M2 R25-T4 helper extraction race-clean | r26 minor | NEW (confirmation) |
| R26-M3 read_snapshot_row DRY (concurrency angle) | r26 minor | NEW (cross-lens confirmation) |
| R26-DEF1 perf-r25 C1 concurrency recheck | r26 defer | NEW (cross-lens defer) |
| R19-I2, R19-I3, R20-M1..M5, R21-M1..M3, R22-M1 | older minor | carry |

## Status block

```
Round 26 (r3-A node-affinity LANDED + R22-S1 sanitize widening
          LANDED + R25-T4 helper-extraction LANDED + R24-A1 grace
          tighten LANDED + stress-r4 IN FLIGHT):
  CLOSED:
    (none — no IMP/CRITICAL closed this round).

  STILL OPEN:
    R20-I2 (sweep host-scoping — IMPORTANT, arch; 8th cycle),
    R20-I3 / R21-I1 / R24-M2 (Restoring watchdog — MINOR; 8th
      cycle; promote to IMPORTANT IF stress-r4 surfaces
      Restoring-phase stretch >60s),
    R24-I1 (fsync_dir doc-misframe — IMPORTANT, code-quality),
    R25-I1 (sweeper TOCTOU invariant untested — IMPORTANT;
      no new test landed),
    R25-I2 (Phase::Ok terminal-overwrite divergence — IMPORTANT;
      no code change).

  NEW (r26):
    R26-I1 (boot lookup is a serial 5-second await; external
      dependency on /v1/agent/self adds reboot-amplification
      risk; recovery requires controller restart — IMPORTANT;
      arch-r26 owns shape-change call),
    R26-M1 (Backend::from_config_full threads node_id consistently
      across 3 constructors — confirmation; no race),
    R26-M2 (R25-T4 helper extraction race-clean; single
      entry.metadata() preserved as lock-of-truth —
      confirmation),
    R26-M3 (read_snapshot_row DRY: both readers equally
      race-unaware; code-quality concern, not concurrency
      — cross-lens confirmation),
    R26-DEF1 (perf-r25 C1 pool-per-call: concurrency recheck
      finds no unsafe state under pg-backpressure; defer to
      perf-r26).

  CARRY:
    R19-I2, R19-I3, R19-I1 (production-unexercised 12th cycle),
    R20-M1..M5, R21-M1..M3, R22-M1, R22-M3, R23-M1, R24-M1,
    R24-M3, R24-M5, R25-M1, R25-M2, R25-M3.

  ASK:
    (1) test-coverage r26: ship R25-I1's two test pins
        (handler-side pg-gated + pure unit) — CARRY from r25.
    (2) test-coverage r26: integration test for
        fetch_local_nomad_node_id boot-failure path (mock
        404 / 500 / timeout response; assert counter bump +
        WARN log + None on field). ~30 LOC.
    (3) arch r26: call on R26-I1 shape — current
        Option<String> set-once vs interior-mutable with
        background-retry. Recommend: doc-comment + /readyz
        surface (recs 1+2); shape-change to mutable is the
        right long-term path but waits on operator-triage
        evidence.
    (4) arch r26: call on R25-I1 enforcement layer (carry
        from r25 — handler-only vs DB-side defense-in-depth).
    (5) arch r26: call on R25-I2 — current behavior vs atomic
        two-row CAS (carry from r25).
    (6) code-quality r26: R24-I1 fsync_dir doc-comment tighten
        + R25-M1 wake_machine agent_url invariant doc-comment
        + R26-M3 SnapshotRowMeta/WakeSnapshotMeta consolidation
        call.
    (7) R20-I3 / R21-I1 watchdog: re-evaluate after stress-r4
        — promote to IMPORTANT if cold-cache restore stretches
        Restoring past 60s.
    (8) perf r26: timeout-value call on R26-I1
        (5s vs 1s for fetch_local_nomad_node_id) +
        R11-P1 pool-per-call cache (carry across 15+
        cycles).
```

## ASK clarifications for the user

Three open design questions concurrency-r26 raised but could not
resolve from code alone:

1. **R26-I1 boot-lookup shape**: should
   `local_nomad_node_id` stay a set-once
   `Option<String>` (current; controller restart is the only
   recovery mechanism), or become interior-mutable
   (`Arc<ArcSwap<Option<String>>>`) with a background-retry
   loop that refreshes when `None → Some` becomes possible?
   The latter closes the "controller boots into a transient
   Nomad-agent unreachable window and stays degraded for the
   process lifetime" shape. Cost: ~40 LOC + a read-site
   change at every consumer (lib.rs:670, mod.rs:228,
   restore_handler.rs:2315, nomad_ch.rs:792). Concurrency-r26
   does NOT recommend until operator-triage evidence
   surfaces; the metric is sufficient triage glue.

2. **R26-I1 timeout-value call**: the current 5s
   `http_get_unsigned` timeout is comfortable margin against
   normal Nomad-agent cold-start but adds up to 5s of
   controller-restart latency on the unhappy path. A 1s
   timeout would tighten restart latency at the cost of more
   spurious `None` falls under cold conditions. Perf-r26
   should own the empirical call; concurrency-r26's lens
   raises no objection to either value.

3. **R25-I1 + R25-I2 carry**: both findings were named in
   r25; neither has landed a fix or a test in r26. Is
   stress-r4 the gate, or is this an explicit defer? The
   carry table treats both as STILL OPEN-IMPORTANT;
   promotion to CRITICAL waits on stress-r4 surfacing the
   exact shape each finding predicts.
