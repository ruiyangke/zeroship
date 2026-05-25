# Sandbox snapshot-restore architecture review — 2026-05-25 r32

**Reviewer**: architecture-r32 (post-T-8 wrapper-attribution scrub, post-CREATE-trace instrumentation)
**HEAD**: `b172cee0` (worktree `.worktrees/sandbox-snapshot-restore`, READ-ONLY).
**Predecessor**: r31 at `7c44cc78` — `docs/reviews/sandbox-snapshot-restore-architecture-2026-05-25-r31.md`.
**Scope**: `crates/sandbox/**` only.

---

## Summary

Six commits since r31. Source delta under `crates/sandbox/src/**` totals 141 +/ 90 - across four files; ~117 LOC of that is doc-comment text (R32-M1 scrub at `3bc689ca` + `0fec9bc3`) and ~31 LOC is the r32-T1 trace-point insertion in `nomad_ch.rs`. The cadence quick-win (`4ac1e526`, 250→100 ms parse-error sleep) is one constant change. The cluster trace report (`8d82ecde`) and cycle-51/52 paperwork (`b172cee0`) touch only `docs/`.

Source-LOC at HEAD:

- `crates/sandbox/src/backend/nomad_ch.rs` — 8322 LOC (+39 vs r31).
- `crates/sandbox/src/restore_handler.rs`   — 5333 LOC (+7).
- `crates/sandbox/src/lib.rs`               — 3184 LOC (unchanged).
- `crates/sandbox/src/backend/mod.rs`       — 678 LOC (+1).
- `crates/sandbox/src/config.rs`            — 1774 LOC (+4).

Both r30 IMPORTANT carries (r31-A1 = ZSBX_* env block; r31-A2 = `NomadStopPermits` on `AppState`) are bit-identical at HEAD. The wrapper-attribution scrub is purely textual — zero runtime-shape change, zero new types, zero surface delta. The CREATE-trace points add two `tracing::info!` calls inside existing functions on the existing CREATE path with no new function/type/trait introduced.

Net: 5 findings (0 CRITICAL, 2 IMPORTANT carries, 3 MINOR — 1 carry, 1 NEW, 1 closed-since-r31 restated for clarity). Per r31's decision-3, downgrading r31-A1 → MINOR is NOT yet warranted: see "Why r31-A1 stays IMPORTANT" below.

---

## CRITICAL

None.

---

## IMPORTANT

### [r32-A1 CARRY of r30-A1 / r31-A1] ZSBX_* env block still emitted in both jobspec builders

Status at `b172cee0`: bit-identical to r31. Both emission sites unchanged:

- `crates/sandbox/src/backend/nomad_ch.rs:2814-2842` (12 keys + `ZSBX_RESTORE_FROM` branch).
- `crates/sandbox/src/restore_handler.rs:2549-2563` (9 keys).

The R32-M1 scrub touched the rustdoc *around* these emissions (e.g., `nomad_ch.rs:50-53` legacy note, `restore_handler.rs:2028-2032` memory_mb plumbing) but did NOT delete the env block itself. The doc-comment self-warning at `nomad_ch.rs:2782-2784` ("Largely redundant with the typed Config block below … kept for debugging") survives the scrub verbatim.

**Why this stays IMPORTANT (not downgraded per r31 decision-3)**: r31 said "if r31-A1 still untouched at r32, downgrade to MINOR." Two pieces of new evidence argue against:

1. **The post-cutover scrub touched the surrounding comments without rewriting the env block.** That is precisely the "fix the instance, leave the trap" antipattern r29 named — a sweep that updates the documentation around a known-stale construct without touching the construct. Downgrading now would tell the next reviewer the env block has been *re-assessed and approved*; the truth is that nobody re-examined it.
2. **The trace point at `nomad_ch.rs:1191` (`submit_done`) now emits `sandbox_id` + `job` + `elapsed_ms` via structured `tracing::info!`** — the right shape. So we have first-party empirical evidence that structured tracing is the working pattern for this layer. The env block's "debugging" justification has been undercut in the same commit window.

**Severity**: IMPORTANT, unchanged. ~70 LOC delete + ~10 LOC `tracing::info!` substitutions. If r33 still has no progress, the right call is to land the deletion as a forcing function, not to demote the finding.

### [r32-A2 CARRY of r30-A2 / r31-A2] `nomad_stop_permits` on `AppState` still unconditional

Status at `b172cee0`: bit-identical. `lib.rs:258-259` keeps the non-`Option` field; `from_config` (untouched) still allocates a sized `NomadStopPermits` for Docker/K8s deployments that have no `nomad_ch_handle()`. `nomad_stop_permits()` accessor at `lib.rs:306-310` (r30-A1 commentary) is unchanged.

No new evidence in either direction. The rustdoc still defends "process-global resource budget"; the budget still bounds calls to Nomad's `POST /shutdown` and nothing else. The api-surface-r32 reviewer (cycle 52 paperwork at `b172cee0`) re-flagged this as R30-API1 "3 rounds running" with the same recommendation shape — move ownership into the backend; drop the `AppState` field; gate the accessor on `Backend::NomadCh`.

**Severity**: IMPORTANT, unchanged. ~30 LOC.

---

## MINOR

### [r32-A3 NEW] `alloc_first_seen` trace lacks `sandbox_id` correlation field

The r32-T1 trace landed at `1d58ab53`. Two new emit sites:

- `nomad_ch.rs:1194-1199` — `submit_done`: emits `sandbox_id = %sandbox_id, job = %job_id, elapsed_ms`. Correct shape.
- `nomad_ch.rs:3102-3106` — `alloc_first_seen`: emits `job = %job_id, elapsed_ms`. **No `sandbox_id` field.**

Both are inside `NomadCHBackend::create` → `wait_for_alloc_running(...)` (a free function in the same module). The free function does not currently receive `sandbox_id` — it takes `nomad_addr` and `job_id`. To add the field, plumb a `sandbox_id: &str` parameter through `wait_for_alloc_running`'s signature (one caller in `nomad_ch.rs`, one in `restore_handler.rs` via `wait_for_alloc_running_blocking`).

The concurrency-r32 reviewer at `b172cee0` flagged this as R32-M1 (concurrency-naming collision with the wrapper-cleanup R32-M1 — pilot's commit message at `b172cee0` already proposes the rename "r32-Mcc" for the concurrency one). Both reviewers agree: emit-shape parity matters because the c=1 cluster runs that motivate this trace cross-correlate by `sandbox_id` against the controller's existing CREATE-error emits, and `job_id` is derivable from `sandbox_id` but not vice-versa in the journal-grep workflow.

**Architectural note**: this is the first time a *free function* outside the `NomadCHBackend` impl block is emitting a CREATE-path trace. The shape is fine, but if more free functions start emitting, the "where does the CREATE trace live" mental map fragments. Worth pinning in a rustdoc on `NomadCHBackend::create` that trace emits may also originate from helpers it calls.

**Severity**: MINOR. ~5 LOC (signature change + one extra `sandbox_id = %sandbox_id`).

### [r32-A4 CARRY of r31-A4 / r30-A3] Stale "wrapper" rustdoc residue — STATUS: substantially closed at R32-M1

The R32-M1 scrub (`3bc689ca` + `0fec9bc3`) rewrote 22 stale references in `nomad_ch.rs`, 9 in `restore_handler.rs`, 1 in `backend/mod.rs`, and 2 in `config.rs` — covering exactly the r31-A4 site list (r31 named 10+5 sites; the scrub caught all of them and several more). Surviving "wrapper" occurrences:

- `nomad_ch.rs:454, 3728, 4940-4941, 4974` — generic English (`compio spawn panic-catch wrapper`, `surface that wrapper in the return type`, etc.). Not the bash script.
- `nomad_ch.rs:52, 1417` — explicit legacy/deleted-bash-wrapper notes preserved by design (deletion history breadcrumb).
- `restore_handler.rs:518, 2982, 3013, 3832-3837, 4526` — generic English (typed-outcome wrapper, bounded-retry wrapper, transport error wrapper, etc.).
- `restore_handler.rs:4805` — explicit "deleted with the wrapper at T-8 cutover" comment, deliberate.
- `lib.rs:108, 408, 455, 966, 971, 1315, 3130` — AEAD/builder wrappers, unrelated.

**Verdict**: scrub complete; surviving hits are correct usage. Closing the finding.

**Severity**: closed at `0fec9bc3`. Restated here only for the closure trail.

### [r32-A5 NEW — observation, not actionable] Post-scrub doc-clarity check

Sampling rewritten rustdoc at `nomad_ch.rs:43-58`, `restore_handler.rs:9-30`, `backend/mod.rs:483-495`, `config.rs:332-358`:

1. **Network derivation** now single-sourced: controller derives tap/IP/MAC, passes via `TaskConfig.Net[0]`. Previous "wrapper computes + controller computes" split gone.
2. **Path-bearing fields** (`disks[].path`, `fs[].socket`, `serial.file`) consistently attributed to `TaskConfig.{Disks,Fs,Serial}` (`restore_handler.rs:1262-1268`, `backend/mod.rs:483-495`). The "wrapper sed-rewrites at exec" pattern is purged.
3. **Pubkey injection** at `nomad_ch.rs:47-55`: legacy note in parens, non-load-bearing. First-time reader's model is "ch driver", not "wrapper-then-ch-driver".
4. **One new artifact**: test rename (`derive_mac_matches_wrapper_pattern` → `..._pinned_format`) creates archaeological mismatch with pre-rename reviewer paperwork. Zero cost, noted for future bisect.

No structural arch shift hidden in the doc-only diff. **Severity**: observation; no action.

---

## Module-boundary stability

`backend/nomad_ch.rs` ↔ `restore_handler.rs` ↔ `lib.rs` boundary at `b172cee0`:

- `AppState::nomad_stop_permits` accessor (`lib.rs:306-310`) still the only cross-module surface for the Nomad teardown semaphore. Unchanged since r30.
- `restore_handler::RealRestoreBackend` still holds `NomadCHConfig` by value (`restore_handler.rs:2026-2029`) and reconstructs the env block independently of `NomadCHBackend::create`. r32-A1 / r30-A1 is exactly this duplication.
- `build_restore_nomad_job_json` (free function in `restore_handler.rs`) is the only restore-side jobspec builder. Symmetric with `build_nomad_job_json` (or its in-impl equivalent) in `nomad_ch.rs`. Two builders, one wire shape.
- The new free functions touched by r32-T1 (`wait_for_alloc_running`, `wait_for_alloc_running_blocking`) sit in `nomad_ch.rs` and are called by both the cold-boot path (in-impl) and the restore path (via the blocking wrapper in `restore_handler.rs`). Module boundary preserved; r32-A3 is the only new wrinkle.

No drift since r31. The module-split question (r30-A4) is unchanged — `restore_handler.rs` at 5333 LOC is +7 vs r31 and remains below the ~6 k threshold.

---

## Net assessment

**Doc-only scrub is doc-only.** R32-M1's 117-LOC churn is entirely comment text + 2 test renames; production behavior is unchanged. The layering story reads more cleanly: post-T-8 the comments now match the binary (ch driver, not wrapper). Net positive without surface risk.

**r32-T1 is correct minimum-viable observability.** Two emits, both in the right places (just after submit, on first alloc visibility). The `sandbox_id` omission at `alloc_first_seen` (r32-A3) is a 5-LOC fix.

**Both r30 IMPORTANT carries unchanged.** Per r31 decision-3 the call was "if r31-A1 still untouched at r32, downgrade to MINOR." On review, the post-cutover scrub *touched the comments around* r32-A1's env block without rewriting it — that's evidence the env block was not re-assessed, not evidence the smell is benign. Holding at IMPORTANT.

**Three decisions for r33**:

1. **If r32-A1 still untouched at r33** — and the post-cutover scrub did not surface a reason to keep the env block — land the deletion as a forcing function. Two cycles of "next cycle we will" is enough.
2. **r32-A3 is a 5-LOC fix.** Sweep it with the next perf or observability commit; do not let it accrete into a separate ticket.
3. **r32-A2 (NomadStopPermits)** has now been re-flagged by two distinct reviewers (architecture + api-surface) for three rounds. Treat as triple-confirmed; schedule the ~30-LOC backend-ownership move.
