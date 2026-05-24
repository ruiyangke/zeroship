# Sandbox snapshot-restore code-quality review — 2026-05-25 r25

**Reviewer**: code-quality-r25 (cron-pilot)
**HEAD**: `038ff3c7` (worktree tip; prompt cited `03d3470f`, the T1 bundle landed in between as `7b5d84f5`, `97fcbcda`, `038ff3c7` — treated read-only per prompt)
**Prior round**: r24 (HEAD `e6363fce`)
**Lens**: code-quality
**Scope since r24**: v14/v34 disk-image preflight bundle (`30960451` controller fsync_dir + assert_disk_image_present + submit_restore_job preflight; `364ead22` scripts pin) · `1c255a00` R24-I1 close (error_code+error_message threaded into terminal-overwrite WARN) · `03d3470f` reviewer artifacts · in-flight T1 sandbox_admin_ro (`7b5d84f5`, `97fcbcda`, `038ff3c7`) — **not reviewed per prompt**.

## Summary

- **7 findings**: 0 critical, 2 important, 5 minor.
- **R24-I1 CLOSED** at `1c255a00`. The terminal-Failed Ok(0) WARN now carries `error_code = code.as_str()` and `error_message = %message` (wake_machine.rs:199-200). Single-line operator triage no longer needs to grep-correlate two events by `wake_id`. Asymmetry note: the Phase::Ok WARN (`:140-147`) intentionally omits these fields (no error context in the success path); the `set_state` WARN (`:534-540`) also omits them (no error info in scope outside an `Err` arm). The targeted close is appropriate.
- **R24-I2 (lib-test flakiness) UNCHANGED.** `fresh_dir()` at `restore_handler.rs:2794-2802` is still a manual `std::env::temp_dir().join(...)` + `create_dir_all`; no `tempfile::TempDir` swap, no `fsync_dir` on the staged image's parent in the test path. The flakiness window observed in r24's repeated invocations remains theoretically possible. Reclassified-to-concurrency-r24-C per prompt header.
- **R24-I3 (duplicate `read_snapshot_row` + `SnapshotRowMeta`) UNCHANGED.** Still two SELECT shapes (`wake_machine.rs:691-714` vs. `restore_handler.rs:613-665`), schema-evolution drift surface still asymmetric. Architecture r24-A4 also surfaced it; v14/v34 bundle did NOT touch the duplication.
- **No new `unwrap()` / `expect()` in production code.** All 17 new `.unwrap()` calls since r24 baseline (`e6363fce`) are in `#[test]` / `#[cfg(test)]` blocks: `assert_disk_image_present_*` direct-helper tests at nomad_ch.rs:4826-4940, `stage_disk_image_preconditions` test helper at restore_handler.rs:2867-2891, two new `submit_restore_job_rejects_missing_*` tests. The production `submit_restore_job` preflight and the new `assert_disk_image_present` / `fsync_dir` helpers route every fallible call through `map_err(|e| format!(...))` + `?`.
- **WakeErrorCode wire-code coverage is uniform across all 8 variants** (db.rs:1588-1607). `as_str` ↔ `from_str_opt` round-trip is verified by `wake_error_code_roundtrip_all_variants_known` at db.rs:3577-3591; `wire_code` mapping is locked by `wire_code_for_all_variants_is_nonempty_snake_case` at db.rs:3597-3635 with all 8 variants enumerated explicitly. No `_ => ...` catch-all in any of the three impl blocks — exhaustive `match` everywhere, which is the right shape: adding a new variant compiles-errors all three sites.
- **WakeMachine ergonomics R25-M2 (new): the 3 `Ok(rows) if rows == 0 =>` WARN+counter call-sites at wake_machine.rs:139-156, :184-213, :533-551 are now 2.5x longer per site after R24-I1's payload-threading.** Each site has identical scaffolding (`Ok(_) => {}` / `Err(e) => { tracing::warn!(...) }` arms differ only in their `state`/`attempted_state` field name and per-site error context) — a `match_terminal_overwrite!(...)` helper macro OR an `async fn record_wake_state_update(self, state, code: Option, message: Option) -> Result<(), DbErr>` method would collapse the 60+ LOC of scaffolding into 3 single-call sites. **But** the trade-off: the macro/method needs to vary the WARN fields per site (Phase::Failed adds error_code+error_message; Phase::Ok adds nothing; set_state adds nothing), so the DRY win is partial. Carry forward as cosmetic; breakeven not yet crossed.
- **R25-I1 (new IMPORTANT)**: `restore_handler.rs:2061-2070` `submit_restore_job` preflight **re-inlines path derivation** that already exists as `pub(crate)` helpers `workspace_image_path` (nomad_ch.rs:3525-3527) and `user_home_image_path` (nomad_ch.rs:3516-3521). The 3-strike cross-emitter parity discipline this bundle was sold on (`user_id`, `rootfs_source`, `workspace.img + home.img`) **requires the derivers to be the single source of truth** — but the new preflight derives its own paths inline. If `<user_home_dir_root>/<user_id>/home.img` becomes (say) `<user_home_dir_root>/<user_id>/v2/home.img` in a future migration, the helper updates but the preflight does not. Same shape as r21-A1's cold-boot vs restore Config-emitter drift, same shape as R24-I3's `read_snapshot_row` duplicate.
- **R25-I2 (new IMPORTANT)**: `submit_restore_job` preflight error message uses **bare `sandbox_id` (UUID Display = dashed form)** at restore_handler.rs:2078 and :2087, while the same file at restore_handler.rs:633-636 and :731-734 uses `sandbox_id_typed = format!("sbx_{}", uuid_to_base62(&sandbox_id))`. An operator grepping the controller log for `sbx_<base62>` (the typed-id form every other emitter uses) will miss every preflight rejection. The cost of fixing is a 3-line `let sandbox_id_typed = format!("sbx_{}", zeroship_core::typed_id::uuid_to_base62(&sandbox_id));` hoist at the top of `submit_restore_job` — already the pattern elsewhere in the same file.
- **`cargo build -p zeroship-sandbox --tests --release`**: not re-run this round (prompt is read-only); r24 reported 2 warnings unchanged from r22 (the `SandboxAuth` unused-import + `WAKE_JOBS_T_KEEP` unused-const carries). T1 bundle landings (`7b5d84f5`, `97fcbcda`, `038ff3c7`) touched `lib.rs` / `admin_handlers.rs` / `auth.rs`-equivalent and are out-of-scope.

## CRITICAL

None.

## IMPORTANT

### [R25-I1] `submit_restore_job` preflight re-inlines `workspace_image_path` / `user_home_image_path` — 3-strike-parity violated at the moment it ships

- **Files**: `crates/sandbox/src/restore_handler.rs:2061-2070` (the new preflight); `crates/sandbox/src/backend/nomad_ch.rs:3516-3527` (the canonical helpers, both `pub(crate)`); `crates/sandbox/src/backend/nomad_ch.rs:586, :721` (the production callers that DO use them).
- **Snippet** (new code that should call the helpers but doesn't):
  ```rust
  // restore_handler.rs:2061-2070 — new in 30960451
  let host_dir = self
      .cfg
      .host_state_dir
      .join(sandbox_id.simple().to_string());
  let workspace_img = host_dir.join("workspace.img");        // BUG
  let user_home_img = self
      .cfg
      .user_home_dir_root
      .join(user_id)
      .join("home.img");                                     // BUG
  ```
- **Issue**: the canonical helpers exist and are used by every other production site:
  ```rust
  // nomad_ch.rs:3525-3527
  pub(crate) fn workspace_image_path(host_dir: &Path) -> PathBuf {
      host_dir.join("workspace.img")
  }
  // nomad_ch.rs:3516-3521
  pub(crate) fn user_home_image_path(user_home_dir_root: &Path, user_id: &str) -> PathBuf {
      user_home_dir_root.join(user_id).join("home.img")
  }
  ```
  The commit message for `30960451` explicitly frames this as the third step in a "3-strike cross-emitter parity continuation after user_id and rootfs_source." But cross-emitter parity ONLY works if both emitters reference the same deriver. The preflight invents its own path-join chain, so a future schema migration (e.g. `home.img` → `home_v2.img` for a vfio-blk format change) updates the helper and breaks the preflight silently — the exact same failure shape r21-A1 closed for cold-boot vs. restore Config emitters and R24-I3 still tracks for `read_snapshot_row`.
- **Why this matters**: this is the same pattern as r21-A1 (two emitters of a Config field, no shared rule) and r22-T1's `expected_diff` parity test that locked it down. The preflight bundle was sold as the third strike in that cross-emitter discipline — landing it with re-inlined derivers WRITES THE FOURTH STRIKE into the codebase. The fix is mechanical: import the two helpers, replace the inline joins. The `host_dir` intermediate is shared with `restore_alloc_dir` semantics (sandbox host_state_dir prefix), so it should reuse that derivation too — but `backend::nomad_ch` doesn't currently expose a `host_state_path(host_state_dir, sandbox_id)` helper, so a one-line introduction would close that gap. Same shape as `workspace_image_path`'s narrow scope.
- **Suggested fix** (read-only — proposing only):
  ```rust
  use crate::backend::nomad_ch::{
      assert_disk_image_present, user_home_image_path, workspace_image_path,
  };
  let host_dir = self.cfg.host_state_dir.join(sandbox_id.simple().to_string());
  let workspace_img = workspace_image_path(&host_dir);
  let user_home_img = user_home_image_path(&self.cfg.user_home_dir_root, user_id);
  ```
  ~6 LOC delta. The `stage_disk_image_preconditions` test helper at restore_handler.rs:2880-2890 has the same re-inline shape (`host_dir.join("workspace.img")`, `user_home_dir.join("home.img")`); the same import-and-call swap closes it.
- **Severity**: IMPORTANT. Single-emitter parity violations are exactly what r21-A1 and r22-T1 were filed to close; landing a NEW violation in the same PR that announces "3-strike parity discipline" is a regression on the discipline itself. Architecture lens may want to take a look at lifting `<host_state_dir>/<sandbox_id_simple>` into a single `sandbox_host_dir(cfg, sid)` helper too.

### [R25-I2] `submit_restore_job` preflight error message uses bare-UUID `sandbox_id`, not the `sbx_<base62>` typed-id form every other emitter in the same file uses

- **File**: `crates/sandbox/src/restore_handler.rs:2071-2089` (the two `format!` wrappers); compare with `:633-636`, `:731-734` (the typed-id hoisting pattern used elsewhere in the same file).
- **Snippet**:
  ```rust
  // restore_handler.rs:2071-2089 — new preflight
  crate::backend::nomad_ch::assert_disk_image_present(&workspace_img).map_err(|e| {
      format!(
          "restore submit: workspace.img missing for sandbox {} \
           (...): {e}",
          sandbox_id                          // ← BARE UUID (dashed)
      )
  })?;
  crate::backend::nomad_ch::assert_disk_image_present(&user_home_img).map_err(|e| {
      format!(
          "restore submit: user_home.img missing for sandbox {} \
           user {} (...): {e}",
          sandbox_id, user_id                 // ← BARE UUID
      )
  })?;
  ```
  vs. the established pattern at `:633-636`:
  ```rust
  let sandbox_id_typed = format!(
      "sbx_{}",
      zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
  );
  ```
- **Issue**: `Uuid: Display` emits the dashed `01234567-89ab-cdef-...` form. Every other operator-facing log/error string in this crate uses the `sbx_<base62>` typed-id form (`sandbox_id = %sandbox_id` is OK because tracing's `%` formatter calls Display on the Uuid AT the structured-field site, where downstream operators search on the field name not the value — but a `format!` into a single error string DOES need the typed-id form for log-grep to find it). An operator triaging "client got 500 on POST /wake" greps `sbx_01h5x2…` in controller journald; the preflight rejection emits `0193abc1-89ab-…` and slips past the grep. The disambiguation between `sandbox_id` and `user_id` in the second message also reads ambiguous when both render as opaque hex strings.
- **Why this matters**: this is the bug invariant "typed_id everywhere" was filed to close (AGENTS.md key invariants list). The same file at `:633-636` and `:731-734` already builds the typed-id form; the new preflight regressed it. R22-M3 (security lens) is tracking the upstream `validate_typed_id` gap on the same `user_id`; combined with R25-I2's typed-id-stringify regression, the preflight error message has two correctness-of-presentation defects at the same site.
- **Suggested fix**:
  ```rust
  let sandbox_id_typed = format!(
      "sbx_{}",
      zeroship_core::typed_id::uuid_to_base62(&sandbox_id),
  );
  // ... then use {sandbox_id_typed} in both format! sites
  ```
  ~4 LOC delta. Hoist at the top of `submit_restore_job`. Identical to the established pattern at restore_handler.rs:633-636 and :731-734.
- **Severity**: IMPORTANT. Operator-facing log strings must be greppable on the typed-id form by the same key the gateway / control plane logs use. A diverging emitter is a triage hazard.

## MINOR

### [R25-M1] `fsync_dir` is described as "Linux-only" but compiles on every target — a Windows build would `File::open` a directory and panic at `sync_all` (acceptable today, but the docs lie)

- **File**: `crates/sandbox/src/backend/nomad_ch.rs:3664-3685`.
- **Snippet**:
  ```rust
  /// Implemented via [`std::fs::File::sync_all`] on a `File::open`-ed
  /// directory handle. On Linux this issues `fsync(dirfd)` (the kernel
  /// accepts fsync on directory fds since forever — it's the
  /// canonical way to flush dirent changes on ext4 / xfs / btrfs).
  fn fsync_dir(dir: &Path) -> Result<(), String> {
      let f = std::fs::File::open(dir)
          .map_err(|e| format!("open {}: {e}", dir.display()))?;
      f.sync_all()
          .map_err(|e| format!("fsync {}: {e}", dir.display()))?;
      Ok(())
  }
  ```
- **Issue**: the doc-comment describes "On Linux" but the function is unconditionally compiled. The whole `crates/sandbox/src/backend/nomad_ch.rs` module is implicitly Linux-only (calls `truncate(1)`, `mkfs.ext4(1)`, `/dev/urandom`, `setns(2)`-via-Nomad), and Cargo.toml has no `target_os` cfg-gating — so a future cross-compile-to-Windows attempt would surface much louder failures than `fsync_dir`. Two cosmetic improvements:
  1. Add `#[cfg(target_os = "linux")]` to the whole file, OR
  2. Move the "On Linux this issues `fsync(dirfd)`" claim out of the doc-comment (it's a portability hint that doesn't match the function's signature — the function is genuinely platform-coupled, not "happens to work on Linux"). A one-liner `// Linux-only (entire module is)` carries the same weight without overpromising.
- **Severity**: MINOR. Cosmetic. The Windows-build claim is hypothetical; nobody builds this crate on Windows.
- **Suggested fix**: trim the "On Linux this issues `fsync(dirfd)` (the kernel accepts fsync on directory fds since forever)" sentence to "On Linux directory fsync flushes dirent changes — the only platform we ship to." ~2 LOC.

### [R25-M2] `wake_machine.rs:139-156`, `:184-213`, `:533-551` — 3 callers of `update_wake_job_state` share the same `match Ok(rows) if rows == 0 =>` scaffolding; partial DRY opportunity

- **File**: `crates/sandbox/src/wake_machine.rs:139-156`, `:184-213`, `:533-551`.
- **Issue**: each call-site has 3 arms: `Ok(rows_affected) if rows_affected == 0 => { tracing::warn!(target: "sandbox::wake::terminal_overwrite_blocked", ...); crate::metrics::inc_wake_terminal_overwrite_blocked(); }`, `Ok(_) => {}`, `Err(e) => { tracing::warn!(...) }` (with one `tracing::error!` exception at the Phase::Ok and Phase::Failed sites). Total LOC of scaffolding across the 3 sites: ~60 LOC after R24-I1's `error_code` + `error_message` threading. The structural shape repeats exactly; only the per-site fields differ (`attempted_state` is named; Phase::Failed adds `error_code` + `error_message`; the Err-arm message varies between "fatal: poll will reflect last in-flight state" and "non-fatal: continuing").
- **Refactoring options** (all read-only proposals):
  1. **`async fn record_wake_state(self, state, code: Option, message: Option<&str>, agent_url: Option<&str>) -> ()`** — single helper on `WakeMachine` that encapsulates the 3-arm match. Variation: pass a `tracing::Level` for the Err-arm's fatality. Tradeoff: the per-site context strings would need to be parameters too. ~30 LOC helper, 3 × ~10 LOC saved at the sites — ~10 LOC net reduction.
  2. **Macro `match_terminal_overwrite!(self, db_call, state, error_ctx, fatality)`** — collapses the structural match into a macro invocation. Tradeoff: macros are harder to read at the call site and breakpoint-debug; the WARN line numbers all point to the macro expansion.
  3. **Status quo + a doc-comment on `update_wake_job_state` linking the 3 call-site contracts.**
- **Severity**: MINOR cosmetic. The 3 sites are far enough apart (lines 139, 184, 533) that the local reader doesn't see the duplication. Until R24-I1's payload-threading the scaffolding was only 8 LOC × 3 = 24 LOC; R24-I1 bumped per-site cost so the breakeven is closer than before but still not crossed. **Carry forward**; revisit if a 4th caller appears.

### [R25-M3] `assert_disk_image_present` error messages share a `disk image post-stage` prefix but the outer wrapper at `submit_restore_job` adds another sentence — R24-M3 carry, unchanged

- **Files**: `crates/sandbox/src/backend/nomad_ch.rs:3638-3661` (inner); `crates/sandbox/src/restore_handler.rs:2071-2089` (outer wrappers).
- **State**: r24 flagged the doubled-sentence framing as cosmetic; v14/v34 landed the preflight without revisiting it. The error chain still reads:
  ```
  "restore submit: workspace.img missing for sandbox <uuid> (snapshot teardown should
   have preserved it via stop_preserving_state; controller will not submit restore job
   that the driver's preflight would reject with a generic Failed-tasks rollup):
   disk image post-stage stat failed: /var/.../workspace.img (No such file or directory
   (os error 2)); controller-side parity check for driver preflight"
  ```
  Two narrative sentences glued by `: `, second ends with a redundant clause ("controller-side parity check for driver preflight") that duplicates the outer's framing. When this lands in the wake_job's `error_message` column it hits `sanitize_error_message`'s 256-byte cap (wake_machine.rs:726-755) and clips the outer message, leaving the operator reading only the redundant inner clause.
- **Severity**: MINOR / cosmetic. R24-M3 OPEN; unchanged. Same fix suggested last round.

### [R25-M4] `classify_failure` `_ => WakeErrorCode::Internal` arm — R24-M4 carry, unchanged

- **File**: `crates/sandbox/src/wake_machine.rs:641-655`.
- **State**: r24-M4 suggested expanding `_ =>` to an explicit `FeatureDisabled | StateMismatch { .. } | NotFound(_) =>` arm so the compiler errors on a future `RestoreHandlerError` variant. v14/v34 didn't touch this site. The inline doc at `:649-652` STILL promises defense-in-depth for the three named variants but the `_` swallows anything new.
- **Severity**: MINOR / cosmetic. R24-M4 OPEN; unchanged.

### [R25-M5] r22-/r24-carry items — r25 status

- **State**:
  - **R24-I1** (terminal-overwrite WARN missing error payload): **CLOSED at `1c255a00`**. The Phase::Failed Ok(0) WARN now carries `error_code = code.as_str(), error_message = %message` (wake_machine.rs:199-200). The R24-I1 follow-up question "should set_state and Phase::Ok WARNs also carry context" is moot — the success path has nothing to thread (Phase::Ok), and the intermediate state path has no error info (set_state runs before any error materializes). No further change needed.
  - **R24-I2** (lib-test flakiness from `fresh_dir` + intra-binary scheduler): OPEN. `fresh_dir` at restore_handler.rs:2794-2802 is unchanged. No `tempfile::TempDir` import in Cargo.toml. Concurrency-r24-C ownership per prompt.
  - **R24-I3** (duplicate `read_snapshot_row` + `SnapshotRowMeta`): OPEN. The wake_machine inline doc at `:669-682` still promises "phase 5 deletes the sync path" — phase 5 has not landed. R24-A4 (architecture lens) also tracks. v14/v34 did not address it.
  - **R24-M1** (`wake_terminal_overwrite_blocked_counter_starts_at_zero` test has no assertion): OPEN. metrics.rs:490-502 is unchanged. r24-M1 suggested deletion; v14/v34 didn't visit.
  - **R24-M2** (sanitize-before-guard-check): OPEN cosmetic. ~2 µs/call.
  - **R24-M3** (doubled-sentence error chain): OPEN. Carried as R25-M3 above.
  - **R24-M4** (classify_failure `_ =>` arm): OPEN. Carried as R25-M4 above.
  - **R22-M1** (`DataIntegrity(String)` opaque payload): OPEN; no breakeven.
  - **R22-M2** (terminal→terminal pg-gated unit test gap at db.rs layer): **PARTIAL CLOSE** at `234c3bdf` (WakeMachine layer covered); pure `update_wake_job_state(terminal_a, terminal_b)` unit at db.rs layer still missing. Test-coverage lens.
  - **R22-M3** (restore-path `user_id` no `validate_typed_id`): OPEN. C-N-W1's `assert_disk_image_present` fix did NOT add validator. Security r24 owns.
  - **R21-M3** (`closure_ref` field naming): OPEN; cosmetic carry.
  - **R20-M1..M4** carry chain: OPEN; cosmetic.
  - **R20 lib warnings** (unused `SandboxAuth` import, unused `WAKE_JOBS_T_KEEP` const): not re-checked this round (T1 bundle may have touched `restore.rs` for the admin token wiring; revisit next round).

## Cross-lens consensus

- **R24-I1 ships clean on its narrow scope.** The 12-LOC threading at wake_machine.rs:199-200 closes the operator-triage gap r24 filed; the Phase::Ok and set_state asymmetries are intentional (no error info exists in those scopes). Code-quality CLOSE confirmed.
- **The v14/v34 preflight bundle ships the right CONTRACT but the wrong CALL-SITE.** `assert_disk_image_present` is correctly placed (cold-boot AND restore both call it; both branches of `create_ext4_image_if_missing`'s skip-vs.-stage call it; the parity discipline is real). `fsync_dir` is the right answer to the staging-and-submit dirent visibility window. But the preflight call site in `submit_restore_job` violates two of the discipline's own invariants: (a) re-inlines path derivation that has dedicated helpers (R25-I1), (b) uses bare-UUID `sandbox_id` in error strings where every other emitter uses the typed-id form (R25-I2). Both are mechanical fixes (~10 LOC combined).
- **No new unwrap()/expect() in production.** Every fallible call routes through `map_err(|e| format!(...))?`. The 17 new `.unwrap()` calls since r24 are all in tests where panic-on-unexpected is the desired contract.
- **WakeErrorCode wire-code routing is uniform and exhaustively-matched.** All three impl blocks (`as_str`, `from_str_opt`, `wire_code`) enumerate every variant explicitly; no `_ => ...` catch-all anywhere — exhaustive matching means adding a 9th variant compiler-errors all three sites. Pinning tests at db.rs:3577-3635 verify the round-trip + wire-code shape. **This is the right pattern; recommend it as the gold-standard for any future error-code enum in the crate** (parallel with C-7-LT-12a's `expected_diff` parity test from r22).
- **`assert_disk_image_present` is the second strike** in what's becoming a "controller-side mirror of driver-side preflight" idiom (the first strike: user_id validation; the second strike: rootfs_source contract; this is the third in the commit-message framing but, by my count, the second STRUCTURAL strike — user_id is a validator, not a stat — so let's call it the second). The pattern is reusable: any time the driver's preflight surfaces a check as "Failed tasks" rollup, mirror it on the controller for a clean named error. Document this idiom in the architecture lens.

## Lens hand-off — architecture / concurrency / api-surface / test-coverage / performance / security

1. **Architecture**: R25-I1 (preflight re-inlines helpers) is shape-identical to r21-A1 (cold-boot vs. restore Config emitters) and R24-I3 (`read_snapshot_row` duplicate). Three datapoints of the same drift surface — architecture-r25 may want to file a meta-finding on "single-source-of-truth derivers for backend path layouts" with a typed wrapper (`struct SandboxDiskPaths { workspace_img, user_home_img, kernel_img, rootfs_img }`) that owns the derivation. Each new backend (Docker, K8s, Nomad+CH, future Nomad+Firecracker) gets one.
2. **Concurrency**: R24-I2 (lib-test flakiness) carries; concurrency-r24-C owns. No new concurrency surface in this round's commits.
3. **Api-surface**: `assert_disk_image_present` is `pub(crate)`, `fsync_dir` is private, `create_ext4_image_if_missing` is `pub(crate)` (unchanged). No new `pub` surface this round. Counter accessor `wake_terminal_overwrite_blocked_value()` is `#[doc(hidden)] pub` (test-only) — already baselined by api-surface r23.
4. **Test coverage**: R24-I2 (flaky tests) + R24-M1 (no-op-body test in metrics.rs) carries; test-coverage-r25 owns. The 6 new lib tests added by `30960451` (4 `assert_disk_image_present_*` + 2 `submit_restore_job_rejects_missing_*`) are well-shaped — direct helper tests + handler-level negative tests with zero-call assertion on the fake Nomad. Good pattern.
5. **Performance**: R25 finds no new perf-shape issues; R24-M2 (sanitize-before-guard-check) carries; `fsync_dir` adds one `open()` + `fsync()` per cold-boot disk-image-create, which is ~hundreds of µs on a healthy ext4 — well within the cold-boot budget. Not a regression.
6. **Security**: R22-M3 (restore-path `user_id` no `validate_typed_id`) is still OPEN. The C-N-W1 fix added `assert_disk_image_present` (catches "file missing") but did NOT lift `validate_typed_id` upstream of the `user_home_img` join, so `user_id = "../etc/passwd"` would still join to `<user_home_dir_root>/../etc/passwd/home.img` — which `assert_disk_image_present` would reject (file doesn't exist) but ONLY because the attacker hasn't planted a non-empty file there. The defense-in-depth gap is real. Security-r25 should still drive this.

## Carried-finding status

| Finding | Source | r25 state |
| --- | --- | --- |
| r17-Q1 / r17-Q3 | r17 → r20/r21 closed | CLOSED. |
| R19-M1 / R19-M5 | r17/r18 → r22-M4 | OPEN — breakeven not crossed. |
| R20-I1 (ADR extract) | r20 → r21 closed | CLOSED. |
| R20-M1..M4 | r20 → r22-M4 | OPEN; cosmetic carries. |
| R20-C1 (terminal-overwrite SQL guard) | r20 CRITICAL → r22-I1 → R24-I1 | CLOSED data-plane (`afa5da96`) + CLOSED observability (`f98611fb`) + CLOSED payload (`1c255a00`). |
| R21-M3 (`closure_ref` field naming) | r21 → r22 → r24 | OPEN; cosmetic carry. |
| R22-I1 (terminal-overwrite invisibility) | r22 IMPORTANT → r24-I1 payload | CLOSED at `f98611fb` (counter+WARN) + `1c255a00` (payload). |
| R22-M1 (`DataIntegrity(String)`) | r22 | OPEN; no breakeven. |
| R22-M2 (terminal→terminal db.rs unit) | r22 → R23-I1 partial close | PARTIAL CLOSE; db.rs layer still open. |
| R22-M3 (restore-path validate_typed_id) | r22 → security r21 R21-S1 | OPEN; security-r25 owns. |
| R22-M4 cosmetic pile | r22 | OPEN; unchanged. |
| R22-T1 (field-list parity test) | r22 | CLOSED at `b6c55d93`. |
| R23-I1 (WakeMachine pg-gated e2e) | r23 | CLOSED at `234c3bdf`. |
| R24-I1 (terminal-Failed WARN payload) | r24 IMPORTANT | **CLOSED at `1c255a00`**. |
| R24-I2 (lib-test flakiness) | r24 IMPORTANT → concurrency r24-C | OPEN. |
| R24-I3 (`read_snapshot_row` duplicate) | r24 IMPORTANT → arch r24-A4 | OPEN. |
| R24-M1 (no-op test body) | r24 | OPEN. |
| R24-M2 (sanitize-before-guard) | r24 | OPEN. |
| R24-M3 (doubled-sentence error) | r24 → R25-M3 | OPEN. |
| R24-M4 (`classify_failure _ =>`) | r24 → R25-M4 | OPEN. |
| R25-I1 (preflight re-inlines helpers) | **NEW r25 IMPORTANT** | OPEN. |
| R25-I2 (preflight bare-UUID in error) | **NEW r25 IMPORTANT** | OPEN. |
| R25-M1 (`fsync_dir` Linux-only doc) | **NEW r25 MINOR** | OPEN. |
| R25-M2 (3-callsite WARN-counter scaffolding DRY) | **NEW r25 MINOR** | OPEN. |
