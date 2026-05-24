# Sandbox snapshot-restore code-quality review — 2026-05-25 r26

**Reviewer**: code-quality-r26 (cron-pilot)
**HEAD**: `add6d5ef` (prompt-quoted floor) — most recent worktree tip is `2ead52c2` (scripts-only, out-of-scope per prompt's "DO NOT propose changes to in-flight files (scripts/* — stress sprint)").
**Prior round**: r25 (HEAD `038ff3c7`).
**Lens**: code-quality.
**Scope since r25**: 5 substantive landings —
- `883df7fe` sandbox/lib: cache local Nomad node_id at boot (r3-A precursor)
- `9b623f44` sandbox/nomad-ch: emit Constraints block on cold-boot (r3-A)
- `d71f1a8c` sandbox/restore-handler: emit Constraints block on restore-path (r3-A)
- `b562d3a1` docs: close r3-A in deferred backlog
- `34b52cf1` sandbox/sweep: tighten HOST_DIR_GC_GRACE_SECS default 3600→600 (R24-A1)
- `901dfbf2` sandbox/sweep: unit tests for host_dir GC eligibility matrix (R25-T4)
- `3d431eb8` sandbox/nomad-ch: prefer Driver Failure events over Alloc Unhealthy (r3-C)
- `022f778a` sandbox/{nomad-ch,restore_handler,wake_machine}: surface preflight failures via StagingPathMissing + typed-id form (R25-I1, R25-I2, R25-S1)
- `79871194` sandbox/db: add WakeErrorCode::StagingPathMissing variant (R23-API1)

Reviewer artifacts at `326f4d4f`, `a482f00d`, `92c45d26`, `add6d5ef` consulted only for context (not in-tree code).

## Summary

- **8 findings**: 0 critical, 2 important, 6 minor / cosmetic carry.
- **R25-I1 CLOSED** at `022f778a`. The `submit_restore_job` preflight now uses `workspace_image_path` / `user_home_image_path` (verified by direct inspection — see "R25-I1 close evidence" below). Single-source-of-truth derivers restored; no inline path joins remain at the preflight site.
- **R25-I2 CLOSED** at `022f778a`. The preflight error message now uses the `sbx_<base62>` typed-id form (verified via `RestoreHandlerError::StagingPreflight { sandbox_id_typed: String }` shape at `restore_handler.rs:117-126` + the `Display` impl). The wire-message path is path-free; tracing-only sites carry the verbatim host path.
- **R25-I3 STILL OPEN** (latent risk). `restore_handler.rs:748-806` defines `SnapshotRowMeta { artifact_path, sha256, vm_index, user_id }` + `async fn read_snapshot_row(db, sandbox_id) -> Result<SnapshotRowMeta, RestoreHandlerError>`. `wake_machine.rs:727-770` defines `WakeSnapshotMeta { sha256, vm_index, user_id }` + `async fn read_snapshot_row(db, sandbox_id) -> Result<WakeSnapshotMeta, String>`. Two distinct functions with the same name, two structs with overlapping field sets, two SELECT shapes against the same `sandbox.sandboxes` table. The wake-path even hand-rolls the typed-id formatting (`sandbox_id_typed` helper at `:705-710`) because `SnapshotRowMeta` is `pub(super)`-scoped and a visibility bump "ripples through the sync path". Carry as **R26-I1** below.
- **r3-A code shape is clean**. `fetch_local_nomad_node_id` (`nomad_ch.rs:3144-3157`) + `parse_nomad_agent_self_node_id` (`:3170-3199`) follow the idiomatic split-IO-from-parse pattern. Pure parser is unit-testable (5 tests at `:7195-7281`); HTTP wrapper is one `http_get_unsigned` + non-200 guard + delegate. Defensive about Nomad-version drift (accepts both `stats.client.node_id` and `Stats.Client.NodeID` shapes). Boot wiring at `lib.rs:670-698` correctly demotes Err to WARN + counter (`inc_nomad_node_id_lookup_failure`) and threads `Option<String>` through; jobspec builders gracefully omit Constraints on `None`. **No production unwrap()/expect()** in any of the r3-A code.
- **`Backend::from_config_full` vs. `from_config_with_persist` IS a slippery slope** but **fixable cheaply**. Three constructors now: `from_config(cfg)` → `from_config_with_persist(cfg, None)`, `from_config_with_persist(cfg, persist)` → `from_config_full(cfg, persist, None)`, `from_config_full(cfg, persist, local_nomad_node_id)`. The cascade is 1-level today; a 4th orthogonal field (e.g., a backend-tuneable from a future ADR) means a 4th constructor. Carry as **R26-I2** below.
- **r3-C `extract_failed_task_event_msgs` two-pass selection is well-shaped**. The hard-coded event-type list lives in `is_diagnostic_event_type` (`nomad_ch.rs:2920-2932`) — case-insensitive `matches!` on 6 strings (`"driver failure"`, `"task setup failure"`, `"setup failure"`, `"failed validating task"`, `"failed artifact download"`, `"exec plugin"`). This is the right shape: an enum buys nothing because the values are Nomad-emitted constants from upstream `nomad/structs/structs.go` (we're a consumer, not the source-of-truth). A const slice `&[&str]` could replace the inline `matches!` arm without changing semantics, but a `matches!` with literal pattern is *equally* exhaustive and slightly faster (compiler synthesizes a trie). **R25-T4 / r3-C close confirmed; carry as a non-finding.**
- **`is_diagnostic_event_type` predicate is the right shape**. The prompt asks "should we apply the typed-staging fixer's `WakeErrorCode` triangle here?" — answer: NO. The `WakeErrorCode` triangle (`as_str` / `from_str_opt` / `wire_code` impls + exhaustive-match invariant) is *our* wire surface — we own the values so an enum is the right shape and exhaustive-match catches drift. But the diagnostic event-type list is *Nomad's* wire surface — we observe values *they* emit. An enum here would lie: a Nomad upgrade that adds `"DNS Failure"` or renames `"Task Setup Failure"` → `"task_setup_failure"` would silently miss the new value and the fallback path (last non-empty DisplayMessage) would still surface the cause. The matches! shape is the right one for consumer-side allow-lists. **Recommend as gold standard for any future `is_X_event_type` predicate over Nomad/Docker/K8s-emitted constants.**
- **R25-T4 sweeper helper extraction is idiomatic**. `classify_host_dir_entry` (`sweep.rs:948-974`) is a pure function returning a `HostDirEntryDecision` enum (`Skip` / `UnderGrace { uuid, age_secs }` / `Candidate { uuid, age_secs }`). 5 ordered gates, each documented. `host_dir_eligible_by_db` (`:986-1000`) is a 14-line pure function with `match` over `SandboxStatus`. Both are `pub(crate)` so the table-test in `tests/sweep_*.rs` can pin every gate without spinning up a `Database`. Clean shape; no smell.
- **R34 grace constant doc-comment is clear**. `sweep.rs:881-905`: prior 3600 s → 30-120 GB stranded; new 600 s → 5-20 GB stranded. The "20-60× margin over alloc-start wall" framing is correct. **Minor nit**: the prompt's "~22GB at 600s grace" figure is slightly above the doc's stated upper bound (20 GB). Either the doc undershoots or the prompt rounds; not worth a finding. Carry forward as understood.
- **wake_machine.rs Phase::Failed handler — no regression on R24-M4 (WARN field consistency)**. Verified at `wake_machine.rs:184-213`: the Phase::Failed Ok(0) WARN carries `error_code = code.as_str()` and `error_message = %message` (`:199-200`). The Phase::Ok branch (`:139-156`) intentionally omits these — there is no error to thread. The `set_state` branch (`:566-584`) also intentionally omits these — no `Err` arm is in scope. All three call-sites use the same `target: "sandbox::wake::terminal_overwrite_blocked"`, the same `attempted_state = ?state` field. Wire-format is consistent. R24-M4 close confirmed at `1c255a00`.
- **No new unwrap()/expect() in production code since r25**. Diff at `crates/sandbox/src/` from `038ff3c7..add6d5ef`: 4 new `.expect(...)` calls — all in `#[test]` / `#[cfg(test)]` blocks (`parse_nomad_agent_self_node_id_lowercase_keys` x2, `restore_path_emits_constraints` array-shape assertion, host_dir GC test sentinel read). Production `fetch_local_nomad_node_id` routes every fallible call through `?` + `map_err(|e| format!(...))`.
- **R26-A1 (proposed `BackendFailureDetail` trait + 4 impls)**: read-only assessment. **Verdict: too much abstraction overhead for current call-site count (3 + 1 = 4 phase-failure edges with structured-detail needs); right pattern at ≥6 edges.** See **R26-A1 ground-truth assessment** below.

## CRITICAL

None.

## IMPORTANT

### [R26-I1] `read_snapshot_row` STILL duplicated across `restore_handler.rs` and `wake_machine.rs` — carry from R24-I3 / R25-I3, no movement

- **Files**: `crates/sandbox/src/restore_handler.rs:748-806` (`SnapshotRowMeta` + sync-path reader); `crates/sandbox/src/wake_machine.rs:727-770` (`WakeSnapshotMeta` + async-path reader).
- **State**: open since r24; r25 noted "phase 5 deletes the sync path" was the architectural promise; phase 5 has not landed. r26: still no movement. The `022f778a` bundle touched both files but did NOT collapse the duplication — the new `StagingPathMissing` wire-code is plumbed through both readers, and the `submit_restore_job` preflight now uses `sandbox_id_typed` consistent with `read_snapshot_row` at `:774-777`, but the two `read_snapshot_row` functions remain distinct.
- **Snippet** — sync-path reader (`restore_handler.rs:748-806`):
  ```rust
  #[derive(Debug, Clone)]
  struct SnapshotRowMeta {
      artifact_path: String,
      sha256: [u8; 32],
      vm_index: i16,
      user_id: String,
  }

  async fn read_snapshot_row(
      db: &Database,
      sandbox_id: Uuid,
  ) -> Result<SnapshotRowMeta, RestoreHandlerError> {
      // ... pool_app + sandbox_id_typed + query_opt
      let row = client.query_opt(
          "SELECT snapshot_artifact_path, snapshot_sha256, snapshot_vm_index, user_id \
             FROM sandbox.sandboxes \
            WHERE sandbox_id = $1::TEXT AND deleted_at IS NULL",
          &[&sandbox_id_typed],
      ).await ... ;
      // ... NULL-guards + sha-len check
      Ok(SnapshotRowMeta { artifact_path: p, sha256: sha, vm_index: v, user_id: u })
  }
  ```
  vs. async-path reader (`wake_machine.rs:727-770`):
  ```rust
  #[derive(Debug, Clone)]
  struct WakeSnapshotMeta {
      sha256: [u8; 32],
      vm_index: i16,
      user_id: String,
  }

  async fn read_snapshot_row(
      db: &Database,
      sandbox_id: Uuid,
  ) -> Result<WakeSnapshotMeta, String> {
      // ... same pool_app + sandbox_id_typed flow ...
      let row = client.query_opt(
          "SELECT snapshot_sha256, snapshot_vm_index, user_id \
             FROM sandbox.sandboxes \
            WHERE sandbox_id = $1::TEXT AND deleted_at IS NULL",
          &[&sandbox_id_str],
      ).await ... ;
      // ... same NULL-guards + sha-len check
      Ok(WakeSnapshotMeta { sha256: sha, vm_index: v, user_id: u })
  }
  ```
- **Issue (verbatim from r24 → r25 → r26)**: two SELECTs against the same table differ only by an extra column (`snapshot_artifact_path`) and an extra `RestoreHandlerError` variant in the error path. Sister-emitter drift: a 0008 migration that adds a NOT-NULL `snapshot_format_version` column requires updating BOTH SELECTs + BOTH `Option<…>` destructures + BOTH NULL-guard tuples — easy to miss. The wake-machine doc-comment at `:712-725` acknowledges the dup ("the proposal § 7 phase 5 deletes the sync path entirely, after which this becomes the sole reader") but phase 5 has been the answer for ≥3 review rounds.
- **Why this is still IMPORTANT in r26**: every review round adds a feature that requires the two readers to stay in lockstep. r3-A could plausibly add a `snapshot_local_node_id` column in a future iteration (verify the snapshot was taken on the same node it'll restore on); R26-A1's `BackendFailureDetail` trait would require structured-detail in `SubmitRestoreError::Preflight` which threads back through `SnapshotRowMeta`. Each round of feature work makes the divergence cost compound.
- **Fix shape** (read-only proposal — three options, ordered by my preference):
  1. **`pub(crate)`-bump `SnapshotRowMeta` + `read_snapshot_row`** and add a `RestoreHandlerError::Internal(format!(...))` → `String` map at the wake-path call-site. 1 visibility token bump, 4 LOC delete in wake_machine.rs, +3 LOC at the call-site. Minimum-blast-radius option.
  2. **Wait for phase 5** (the documented plan). The risk: phase 5 has been a promise for ≥3 rounds and the cost compounds every time we add a column.
  3. **Split into a shared `pub(crate) fn read_snapshot_row_min(db, sid) -> Result<MinMeta, String>` + a sync-path-only wrapper** that adds `artifact_path` via a second column-extract. Reduces duplicate SELECT to single SELECT-with-extra-projection; minor code uglification.
- **Recommendation**: option (1). Move both `SnapshotRowMeta` and `read_snapshot_row` to `pub(crate)`, delete the wake-machine duplicate, accept the trivial `RestoreHandlerError → String` map at the wake-machine call-site.
- **Severity**: IMPORTANT — three review rounds is enough. Latent-risk magnitude rises monotonically with each schema field added.

### [R26-I2] `Backend::from_config_full` vs. `from_config_with_persist` vs. `from_config` — 3-level cascade is a slippery slope; collapse via builder

- **Files**: `crates/sandbox/src/backend/mod.rs:182-234` (three constructors).
- **Snippet**:
  ```rust
  pub fn from_config(cfg: &SandboxConfig) -> Result<Self, String> {
      Self::from_config_with_persist(cfg, None)
  }

  pub fn from_config_with_persist(
      cfg: &SandboxConfig,
      persist: Option<Arc<crate::persist::Persistence>>,
  ) -> Result<Self, String> {
      Self::from_config_full(cfg, persist, None)
  }

  pub fn from_config_full(
      cfg: &SandboxConfig,
      persist: Option<Arc<crate::persist::Persistence>>,
      local_nomad_node_id: Option<String>,
  ) -> Result<Self, String> {
      // match cfg.backend ...
  }
  ```
- **Issue**: 3-level constructor cascade where each level adds exactly one parameter and threads `None` through to the level below. This is the "telescoping constructor anti-pattern" from Effective Java — works at 2 levels, problematic at 3, untenable at 4. The 3rd level (`from_config_full`) was added by r3-A (`883df7fe`) to thread `local_nomad_node_id`; a future ADR for, say, "controller-side preempt budget" (the perf-r24 follow-up) would mint a 4th level. Each level imposes a versioning decision on every test/example call-site ("which constructor do I call?"). The mid-level `from_config_with_persist` is now a 2-line shim — its only callers (10 test/example sites — verified by grep) pass `persist` and want defaults for the rest.
- **Why this matters now (not later)**: at 3 levels the smell is recoverable for ~30 LOC. At 4 it's recoverable for ~60. At 5 it's a refactor. The discipline cost rises super-linearly with each level. R26-A1's proposed trait + the perf-r24 backend-tuneable would both want to thread state into `Backend::new` — minting `from_config_extra_full` and `from_config_extra_extra_full` next round.
- **Fix shape — builder option**:
  ```rust
  pub struct BackendBuilder<'a> {
      cfg: &'a SandboxConfig,
      persist: Option<Arc<crate::persist::Persistence>>,
      local_nomad_node_id: Option<String>,
      // future: preempt_budget, etc.
  }

  impl<'a> BackendBuilder<'a> {
      pub fn new(cfg: &'a SandboxConfig) -> Self {
          Self { cfg, persist: None, local_nomad_node_id: None }
      }
      pub fn with_persist(mut self, p: Arc<crate::persist::Persistence>) -> Self {
          self.persist = Some(p); self
      }
      pub fn with_local_nomad_node_id(mut self, id: Option<String>) -> Self {
          self.local_nomad_node_id = id; self
      }
      pub fn build(self) -> Result<Backend, String> { /* current match */ }
  }
  ```
  Production: `BackendBuilder::new(&config).with_persist(p).with_local_nomad_node_id(id).build()`. Tests: `BackendBuilder::new(&cfg).build()`. Net effect: zero LOC at simple test sites, single named-parameter clarity at production. ~40 LOC builder, ~10 LOC delete from `from_config*`.
- **Fix shape — minimum-blast-radius option** (`Default`-able config struct):
  ```rust
  #[derive(Default)]
  pub struct BackendExtras {
      pub persist: Option<Arc<crate::persist::Persistence>>,
      pub local_nomad_node_id: Option<String>,
  }

  pub fn from_config(
      cfg: &SandboxConfig,
      extras: BackendExtras,  // BackendExtras::default() at test sites
  ) -> Result<Self, String> { ... }
  ```
  Production: `Backend::from_config(&config, BackendExtras { persist: Some(p), local_nomad_node_id: Some(id), ..Default::default() })`. Tests: `Backend::from_config(&cfg, BackendExtras::default())`. ~15 LOC delta net.
- **Severity**: IMPORTANT — code-smell, not a bug. Worth doing **before** R26-A1 lands (R26-A1 would add a 4th constructor or a 5th-arg-thread); doing both in the same PR is the right shape. The 3-level cascade is also load-bearing for the prompt's "future X is more important than today's X" question — yes, this *will* be R26-I2 next round if untouched.

## MINOR

### [R26-M1] r3-A boot wiring threads `local_nomad_node_id` through 4 fields (lib.rs AppState + restore_handler RealRestoreBackend + nomad-ch backend + Constraints emitters) — partial-DRY opportunity

- **Files**:
  - `crates/sandbox/src/lib.rs:225` (`AppState::local_nomad_node_id: Option<String>`)
  - `crates/sandbox/src/lib.rs:670-698` (boot fetch + WARN + counter)
  - `crates/sandbox/src/lib.rs:700-704` (passed to `Backend::from_config_full`)
  - `crates/sandbox/src/lib.rs:958` (re-passed to `RealRestoreBackend::with_local_nomad_node_id`)
  - `crates/sandbox/src/lib.rs:1026` (stored on `AppState`)
  - `crates/sandbox/src/backend/nomad_ch.rs:202` (`NomadCHBackend::local_nomad_node_id: Option<String>`)
  - `crates/sandbox/src/backend/nomad_ch.rs:425-428` (`with_local_nomad_node_id` builder)
  - `crates/sandbox/src/backend/nomad_ch.rs:434-436` (read accessor)
  - `crates/sandbox/src/backend/nomad_ch.rs:792` (passed to `build_nomad_job_json_with`)
  - `crates/sandbox/src/restore_handler.rs:2071` (`RealRestoreBackend::local_nomad_node_id: Option<String>`)
  - `crates/sandbox/src/restore_handler.rs:2120-2125` (builder mirror)
  - `crates/sandbox/src/restore_handler.rs:2315` (passed to `build_restore_job_json_with`)
- **Issue**: a single `Option<String>` propagates through 11 distinct sites — `AppState` field, `Backend` enum constructor parameter, `NomadCHBackend` field + builder + accessor + Constraints emit-site, `RealRestoreBackend` field + builder + Constraints emit-site. Each of the 11 sites was correctly authored (verified inspection — no field-name typos, no missed `.clone()` calls, builder shape mirrored). But the surface area means a future change ("the node_id should refresh on Nomad-agent leadership transition") modifies all 11.
- **Why this is MINOR not IMPORTANT**: the propagation is *correct* and *consistent*; the duplication is structural to Rust's ownership model (we can't share a single mutable `Option<String>` across `AppState` + two backend types without a `RwLock<Option<String>>` or an `Arc<RwLock<Option<String>>>`, which would be the bigger smell). The fix shape would be a `NodeIdHandle = Arc<RwLock<Option<String>>>` shared across the three holders — but that's premature until the "refresh on leadership transition" requirement materializes. Today's `Option<String>` snapshot-at-boot is the right shape.
- **Carry as a watch-item**: if a future ADR needs node_id refresh, the shape changes; otherwise leave as-is. Severity: MINOR / cosmetic.

### [R26-M2] `is_diagnostic_event_type` — `to_ascii_lowercase` allocation on hot path

- **File**: `crates/sandbox/src/backend/nomad_ch.rs:2920-2932`.
- **Snippet**:
  ```rust
  fn is_diagnostic_event_type(ty: &str) -> bool {
      // Lower-case once; full-string equality on each candidate.
      let t = ty.trim().to_ascii_lowercase();
      matches!(
          t.as_str(),
          "driver failure"
              | "task setup failure"
              | "setup failure"
              | "failed validating task"
              | "failed artifact download"
              | "exec plugin"
      )
  }
  ```
- **Issue**: `to_ascii_lowercase()` allocates a `String`. Called per-event-per-failed-task during `extract_failed_task_event_msgs` — typical alloc shape is ≤10 events × 1-2 failed tasks = 10-20 small allocations per `wait_for_alloc_running_blocking` failure path. Not a hot path (it only fires on actual failures, ≤1/wake), but the alloc is wholly avoidable: `str::eq_ignore_ascii_case` is the idiomatic predicate.
- **Fix shape**:
  ```rust
  fn is_diagnostic_event_type(ty: &str) -> bool {
      let trimmed = ty.trim();
      const TYPES: &[&str] = &[
          "Driver Failure",
          "Task Setup Failure",
          "Setup Failure",
          "Failed Validating Task",
          "Failed Artifact Download",
          "Exec Plugin",
      ];
      TYPES.iter().any(|t| trimmed.eq_ignore_ascii_case(t))
  }
  ```
  ~no LOC delta, zero allocations. The const slice form also makes the canonical list visible at-glance (the current `matches!` arm hides them inside a pattern).
- **Severity**: MINOR cosmetic + micro-perf. Carry forward; bundle with R26-I1 collapse round.

### [R26-M3] `extract_failed_task_event_msgs` — `Vec::with_capacity(task_states.len())` over-allocates

- **File**: `crates/sandbox/src/backend/nomad_ch.rs:2840-2904`, specifically `:2845`.
- **Snippet**:
  ```rust
  let mut out: Vec<String> = Vec::with_capacity(task_states.len());
  for (task_name, ts) in task_states {
      let failed = ts["Failed"].as_bool().unwrap_or(false);
      if !failed { continue; }
      // ...
  }
  ```
- **Issue**: `task_states.len()` is the count of ALL tasks; the loop only pushes for FAILED tasks. A Nomad alloc typically has 1-3 tasks total but ≤1 failed (the canonical Nomad-CH alloc has exactly 1 "ch" task). Over-allocates by 1-2 slots ~always. Hot path is the failure-extract path, low traffic.
- **Fix**: drop the `with_capacity` hint entirely (the default `Vec` growth is fine for 0-2 items) or use `Vec::with_capacity(1)` (the common case).
- **Severity**: MINOR cosmetic micro-perf. Negligible impact. The current shape is harmless but mis-signals intent ("we expect N pushes" when N is usually 1).

### [R26-M4] r3-A precursor doc-comments duplicate the failure-shape narrative across `fetch_local_nomad_node_id` + `parse_nomad_agent_self_node_id` + `with_local_nomad_node_id` + the Constraints emitter sites

- **Files**: `nomad_ch.rs:3107-3158` (fetch doc), `:3159-3199` (parse doc), `:411-428` (builder doc), `:2559-2578` (cold-boot emit doc), `restore_handler.rs:4340-4410` (restore emit doc).
- **Issue**: each site re-explains the "78% cross-node failure at WORKER_COUNT=3 → r3-A fix" narrative. ~30 LOC of duplicated doc. The narrative is the right level of detail for one of those sites; the others should refer back. Current shape: each function reader sees the full story; future code-comment-doc-drift surface is real (e.g., the stress-r4 actual rate may differ from 78%).
- **Fix**: pick one site as the canonical narrative (recommend `fetch_local_nomad_node_id` since it's the deepest leaf) and replace the others with `/// See [\`fetch_local_nomad_node_id\`] for the r3-A architectural rationale.` ~80 LOC delta net.
- **Severity**: MINOR cosmetic.

### [R26-M5] `fsync_dir` Linux-only doc-comment — R25-M1 carry, unchanged

- **State**: `nomad_ch.rs:3664-3685` unchanged since r25. Doc-comment still describes "On Linux this issues `fsync(dirfd)`" but the function is unconditionally compiled. Cosmetic carry from r25; nobody builds this crate on Windows so the lie is hypothetical.
- **Severity**: MINOR / cosmetic. Carry.

### [R26-M6] r22-/r24-/r25-carry items — r26 status

- **R22-M1** (`DataIntegrity(String)` opaque payload): OPEN; no breakeven.
- **R22-M2** (terminal→terminal pg-gated unit test gap at db.rs layer): partial close at `234c3bdf`; db.rs layer still missing.
- **R22-M3** (restore-path `user_id` no `validate_typed_id`): OPEN. C-N-W1 fix added `assert_disk_image_present` (catches "file missing") but did NOT lift `validate_typed_id`; security-r26 owns.
- **R21-M3** (`closure_ref` field naming): OPEN; cosmetic.
- **R20-M1..M4** carry chain: OPEN; cosmetic.
- **R24-M1** (no-op test body `wake_terminal_overwrite_blocked_counter_starts_at_zero`): OPEN. metrics.rs:490-502 unchanged.
- **R24-M2** (sanitize-before-guard-check ~2 µs/call): OPEN cosmetic.
- **R24-M3** (doubled-sentence error chain): OPEN. The R25-I2 close at `022f778a` *partially* addressed this by stripping the path-bearing inner clause, but the outer wrapper at `submit_restore_job` still does `format!` over the typed Display. Acceptable for now.
- **R24-M4** (`classify_failure _ =>` arm): OPEN. The `022f778a` bundle added a `StagingPreflight { .. } => WakeErrorCode::StagingPathMissing` arm (`wake_machine.rs:691`) but the `_ => WakeErrorCode::Internal` catch-all at `:696` is still present. Cosmetic / defense-in-depth.
- **R25-M2** (3-callsite WARN-counter scaffolding DRY): OPEN. The 3 sites at wake_machine.rs:139-156, :184-213, :566-584 share the same `Ok(rows) if rows == 0 => warn + counter` shape. Breakeven not crossed; no 4th caller.
- **R25-M1** (`fsync_dir` Linux-only doc): carried as R26-M5 above.

## R25-I1 close evidence

For the record (so the carry-table doesn't have to reread):

- Before `022f778a` (r25 critique snapshot): `restore_handler.rs:2061-2070` had `let workspace_img = host_dir.join("workspace.img");` and `let user_home_img = self.cfg.user_home_dir_root.join(user_id).join("home.img");` re-inlining the path joins.
- After `022f778a`: the production preflight path no longer surfaces in `submit_restore_job` — instead, the error is routed via `SubmitRestoreError::Preflight { which: "workspace.img", path: PathBuf, source: String }` → `RestoreHandlerError::StagingPreflight { which, sandbox_id_typed: String }` (`:117-126`). The path-bearing detail flows through `SubmitRestoreError::log_detail` at `:212-220` (tracing-only); the wire surface carries only `which` (`&'static str`) + `sandbox_id_typed`. No host paths cross the wire boundary. The `workspace_image_path` / `user_home_image_path` helpers are the canonical derivers; the preflight either calls them (cold-boot path at `nomad_ch.rs:586/:721`) or operates on derived paths that originate from them.
- Cosmetic side effect: `SubmitRestoreError` and `RestoreHandlerError::StagingPreflight` are now siblings with overlapping `which` fields. This is a deliberate two-layer design (wire-vs.-internal); not a smell.

## R25-I2 close evidence

- Before `022f778a`: `submit_restore_job` preflight `format!`'d `sandbox_id` directly (rendering as bare UUID).
- After `022f778a`: the preflight raises `SubmitRestoreError::Preflight` which is converted to `RestoreHandlerError::StagingPreflight { sandbox_id_typed: String }` at the wake-machine boundary; the `Display` impl renders `"staging image missing: {which} for {sandbox_id_typed}"`. Wire form is `sbx_<base62>` — verified by grep + manual trace at `:117-126`. Closed.

## R26-A1 ground-truth assessment — `BackendFailureDetail` trait + 4 impls

The architecture-r26 review proposes:

> **Architectural lift**: ... the right shape is **one trait `BackendFailureDetail` with `fn log_detail(&self, sandbox_id_typed: &str)` + `fn wire_summary(&self) -> String`**, and each variant a small struct implementing it. ... ~150-200 LOC.

**My code-quality verdict: defer for now, revisit at ≥6 phase-failure edges.** Detail:

**Pro: the pattern works.** The `SubmitRestoreError::log_detail` shape at `restore_handler.rs:212-220` is exactly the trait method the architecture review proposes. The two responsibilities (log structured detail controller-side; emit path-free wire summary) are correctly factored. The trait would lock the contract: every backend-failure shape MUST implement both methods, the wire summary MUST be path-free.

**Con: today's call-site count is 1, not 4.** Verified: `wake_machine.rs` rollback edges at `:452`, `:484`, `:498` all use the older shape `rollback_with(g1, snap.vm_index, WakeErrorCode::X, e: String)`. The string `e` flows from `wait_for_livez` / `clock_resync_post_restore` / `register_restored` — currently unstructured but ALSO currently free-text strings from the backends. To make `LivezTimeoutDetail` a struct that implements `BackendFailureDetail`, you'd need to either:
1. Change the backend method signatures (e.g., `wait_for_livez(...) -> Result<(), LivezTimeoutDetail>`) — invasive surface change across the Docker/K8s/Nomad-CH backends.
2. Parse the string at the rollback edge — fragile.

So R26-A1 proposes implementing a trait that has 4 *intended* impls but, today, would have 1 actual impl (`SubmitRestoreError::Preflight`) plus 3 struct-mints with `BackendFailureDetail::wire_summary(&self) -> String { self.legacy_string.clone() }` placeholders.

**Quantitative ground-truth**: I count 4 wake-phase rollback edges (livez, clock_resync, register, plus the rollback_and_classify generic) versus 1 already-typed edge. R26-A1's "~150-200 LOC" estimate is roughly right for the trait + 4 struct shells, but the underlying backend-method signature changes (which the architecture review doesn't fully cost) are 3-4× that.

**Code-smell-density argument against the trait today**:
- Trait + 4 small structs adds ~6 type-names to the public(crate) surface.
- Each struct duplicates ~3 fields with the legacy string error shape.
- The dispatch site (`rollback_with_detail`) gains a `Box<dyn BackendFailureDetail>` (allocation + dynamic dispatch) where today's `String` is a single move.
- The win is uniform structured-logging contract — but tracing already gives us that via field names; `rollback_with(g1, vm_index, code, e)` at the call site can do `tracing::warn!(target: "...", error = %e, code = %code.as_str(), ...)` without a trait.

**When to revisit**: when a 5th and 6th phase-failure-detail need to carry structured fields (e.g., a future "VFIO-handoff-failed" phase that needs the device-id, or a "tap-leak-detected" phase that needs the vm_index + bridge name), the line crosses. At ≥6, the trait pays for itself. Today's 4 (with 3 in placeholder form) is below the line.

**Recommendation for r26**: do NOT land `BackendFailureDetail` trait now. Instead:
1. Land R26-I1 (collapse `read_snapshot_row` duplication) — concrete, mechanical, 1-PR.
2. Land R26-I2 (collapse `from_config*` constructor cascade) — concrete, mechanical, 1-PR.
3. Defer R26-A1 to a "structured-failure-detail" milestone keyed to ≥6 phase-failure edges.

If architecture-r26 wants to ship R26-A1 now, the code-quality lens would not block — the pattern IS right, the call-site count is just below the abstraction-payoff threshold. **NEUTRAL hand-off**, not opposed.

## Cross-lens consensus

- **r3-A landed clean.** No production unwrap/expect, defensive Nomad-version-drift handling, gracefully-degraded fallback, observable WARN + counter on Err path. The 11-site `local_nomad_node_id` propagation is correct in every site (read every one); the structural duplication is R26-M1 (a watch-item, not a bug).
- **r3-C `extract_failed_task_event_msgs` two-pass selection is the right shape.** `is_diagnostic_event_type` is the right predicate shape for consumer-side allow-lists (NOT the WakeErrorCode-style triangle, which is for wire-owned enums). R26-M2 (alloc-free `eq_ignore_ascii_case`) is cosmetic.
- **R25-T4 sweeper helpers are textbook idiomatic Rust.** Pure functions, decision enum, exhaustive matches, test-driveable from a stdlib-only fixture. No findings.
- **R25-I1 + R25-I2 close confirmed.** Both via `022f778a`; the new `StagingPathMissing` wire code is properly plumbed.
- **R25-I3 carries as R26-I1.** Three rounds of "phase 5 will fix this" — the cost compounds, recommend `pub(crate)` bump + delete-the-duplicate now.
- **`Backend::from_config_full` IS a slippery slope** (R26-I2). 3-level cascade is recoverable at ~30 LOC today; 4-level would be ~60. Recommend builder pattern or `Default`-able extras struct.
- **No new unwrap()/expect() in production since r25.** All 4 new `.expect(...)` calls are in `#[test]` blocks.
- **R34 grace doc-comment math is internally consistent** (3600 s → 30-120 GB, 600 s → 5-20 GB at 10/min × 50-200 MB/dir). Prompt's "~22 GB at 600s" is slightly above the doc upper bound but well within rounding.
- **R26-A1 (`BackendFailureDetail` trait) defer-not-block.** Pattern is right; call-site count too low. Revisit at ≥6 phase-failure-detail edges.

## Lens hand-off — architecture / concurrency / api-surface / test-coverage / performance / security

1. **Architecture**: R26-I1 (`read_snapshot_row` duplicate) is shape-identical to architecture-r24-A4 / r25-A1; recommend collapsing now. R26-I2 (`from_config*` cascade) is a builder-pattern decision; lighter-touch is the `Default`-able extras struct.
2. **Concurrency**: no new concurrency surface in this round's commits. R24-I2 (lib-test flakiness) carries; concurrency-r24-C owns.
3. **Api-surface**: `fetch_local_nomad_node_id` + `parse_nomad_agent_self_node_id` are `pub(crate)` (correct — not part of the public surface). `with_local_nomad_node_id` is `pub` on `NomadCHBackend` so `AppState::from_config` can re-wrap; acceptable. `BackendBuilder` (if R26-I2 lands as builder) would be `pub`.
4. **Test coverage**: r3-A tests are well-shaped (5 `parse_nomad_agent_self_node_id_*` tests, lib-test `restore_path_emits_constraints` asserts the JSON array shape directly). R25-T4 sweeper unit tests are well-shaped. No coverage gap surfaced this round.
5. **Performance**: R26-M2 (`is_diagnostic_event_type` alloc on hot-cold path), R26-M3 (`extract_failed_task_event_msgs` over-allocation) — both micro, both bundled.
6. **Security**: R22-M3 (`user_id` validate_typed_id) carries. `assert_disk_image_present` (closed by `022f778a`) catches "file missing" but does NOT lift `validate_typed_id` upstream of the `user_home_img` join. Defense-in-depth gap; security-r26 owns.

## Carried-finding status

| Finding | Source | r26 state |
| --- | --- | --- |
| r17-Q1 / r17-Q3 | r17 → r20/r21 | CLOSED. |
| R19-M1 / R19-M5 | r17/r18 → r22-M4 | OPEN — breakeven not crossed. |
| R20-I1 (ADR extract) | r20 → r21 | CLOSED. |
| R20-M1..M4 | r20 | OPEN; cosmetic. |
| R20-C1 (terminal-overwrite SQL guard) | r20 → r24 chain | CLOSED. |
| R21-M3 (`closure_ref` naming) | r21 → r25 | OPEN; cosmetic. |
| R22-I1 (terminal-overwrite invisibility) | r22 → r24 chain | CLOSED. |
| R22-M1 (`DataIntegrity(String)`) | r22 | OPEN; no breakeven. |
| R22-M2 (terminal→terminal db.rs unit) | r22 → R23-I1 | PARTIAL CLOSE; db.rs layer still open. |
| R22-M3 (restore-path validate_typed_id) | r22 → security-r21/r22/r25/r26 | OPEN; security-r26 owns. |
| R22-M4 cosmetic pile | r22 | OPEN; unchanged. |
| R22-T1 (field-list parity test) | r22 | CLOSED at `b6c55d93`. |
| R23-I1 (WakeMachine pg-gated e2e) | r23 | CLOSED at `234c3bdf`. |
| R23-API1 (`StagingPathMissing` wire variant) | r23-API | CLOSED at `79871194`. |
| R24-I1 (terminal-Failed WARN payload) | r24 | CLOSED at `1c255a00`. |
| R24-I2 (lib-test flakiness) | r24 → concurrency-r24-C | OPEN. |
| R24-I3 (`read_snapshot_row` duplicate) | r24 → R25-I3 → R26-I1 | OPEN — recommend close this round. |
| R24-M1 (no-op test body) | r24 | OPEN. |
| R24-M2 (sanitize-before-guard) | r24 | OPEN. |
| R24-M3 (doubled-sentence error) | r24 → R25-M3 | PARTIAL CLOSE via `022f778a`. |
| R24-M4 (`classify_failure _ =>`) | r24 → R25-M4 | OPEN. |
| R25-I1 (preflight re-inlines helpers) | r25 IMPORTANT | **CLOSED at `022f778a`**. |
| R25-I2 (preflight bare-UUID in error) | r25 IMPORTANT | **CLOSED at `022f778a`**. |
| R25-I3 (`SnapshotRowMeta` DRY) | r25 | OPEN — carried as **R26-I1**. |
| R25-M1 (`fsync_dir` Linux-only doc) | r25 | OPEN — carried as **R26-M5**. |
| R25-M2 (3-callsite WARN-counter scaffolding DRY) | r25 | OPEN; breakeven not crossed. |
| R25-S1 (path leak via wake_jobs.error_message) | r25 (security) | CLOSED at `022f778a` (typed `StagingPreflight` variant, path-free `Display`). |
| R25-T4 (sweeper helpers + unit-test matrix) | r25 (test-coverage) | CLOSED at `901dfbf2`. |
| R24-A1 (grace tighten 3600→600) | r24 (perf) | CLOSED at `34b52cf1`. |
| r3-A (node-affinity Constraints block) | architecture-r3 | CLOSED at `883df7fe`/`9b623f44`/`d71f1a8c`/`b562d3a1`. |
| r3-C (Driver Failure event preference) | architecture-r3 | CLOSED at `3d431eb8`. |
| **R26-I1** (`read_snapshot_row` duplicate — collapse now) | **NEW r26 IMPORTANT** | OPEN. |
| **R26-I2** (`from_config*` 3-level cascade) | **NEW r26 IMPORTANT** | OPEN. |
| **R26-M1** (`local_nomad_node_id` propagation surface) | **NEW r26 MINOR / watch** | OPEN. |
| **R26-M2** (`is_diagnostic_event_type` alloc) | **NEW r26 MINOR** | OPEN. |
| **R26-M3** (`extract_failed_task_event_msgs` over-allocation hint) | **NEW r26 MINOR** | OPEN. |
| **R26-M4** (r3-A doc duplication across sites) | **NEW r26 MINOR** | OPEN. |
| **R26-M5** (`fsync_dir` Linux-only doc) | r25-M1 carry | OPEN. |
| **R26-A1** (`BackendFailureDetail` trait + 4 impls) | architecture-r26 PROPOSAL | NEUTRAL hand-off — defer to ≥6-edge milestone. |

## Build state

- `cargo build -p zeroship-sandbox --tests --release`: not re-run (prompt is read-only).
- r25 carry: 2 warnings unchanged (`SandboxAuth` unused import + `WAKE_JOBS_T_KEEP` unused const).
- T1 bundle (`7b5d84f5` / `97fcbcda` / `038ff3c7`) is in-tree but out-of-scope per r25 prompt; T1 review owned elsewhere.

## Bottom line

r26 lands **clean on r3-A, r3-C, R24-A1, R25-T4, R25-I1, R25-I2, R25-S1**. Two real code-smell findings:
- **R26-I1** (`read_snapshot_row` duplicate): three rounds of "phase 5 will fix this" is too long; recommend `pub(crate)` bump + delete the wake-machine duplicate this round.
- **R26-I2** (`from_config*` 3-level cascade): builder-pattern or `Default`-able extras struct; 30 LOC today, 60 LOC if R26-A1 also lands.

R26-A1 (`BackendFailureDetail` trait, architecture-r26 proposal): pattern right, count low — **defer**, not block.

No critical findings. No production unwrap()/expect() regression. Score: code-quality lens reads HEAD as **production-ready modulo R26-I1 collapse**.
