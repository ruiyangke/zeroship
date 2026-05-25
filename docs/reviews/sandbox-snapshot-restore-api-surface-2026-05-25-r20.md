# Sandbox/snapshot-restore — api-surface r20 review

Date: 2026-05-25 (UTC, catchup)
HEAD at audit: `b18782f6` (last reviewed: r19 at `44d10fe2`).
Round 20 — catchup behind R19-API1 closure (`fde4f51c`).
Read-only. Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`,
`crates/core/**`.

## Summary

- **R19-API1 (1-round carry) CLOSED at `fde4f51c`**. Verified live at
  `db.rs:3275-3344` + `sweep.rs:397-405`:
  - `error_message` SQL literal is now `'wake worker aborted: controller
    did not complete the wake within the timeout (see operator runbook)'`
    — no "R19-C1" token, no "lessee" token. Operator-facing prose, points
    at the runbook for follow-up.
  - The R19-C1 lineage breadcrumb migrated to the structured tracing
    field `closure_ref = "R19-C1"` on the existing
    `tracing::warn!(target: "sandbox::wake::takeover", …)` — log-pipeline
    only, never on the wire.
  - The doc-comment at `db.rs:3282` mirrors the new wire literal exactly,
    keeping internal docs ↔ external surface in sync.
- **R19-T1 closure (test-coverage twin) verified incidental** at
  `admin_handlers.rs:2329-2347`: the §10.0 renderer matrix now includes
  `WakeWorkerAborted` and pins `state == "failed"` + `message.is_string()`
  for the whole loop — locks the envelope shape against future variant
  drift.
- **R19-API2 (carry) STILL OPEN**. `sweep.rs:388` `pub async fn
  run_wake_jobs_takeover_once` unchanged; visibility hygiene deferred to
  the next sweep (paired with `run_wake_jobs_gc_once` at `sweep.rs:296`).
- **R10-API4 + R12-API1 readyz §10.0 drift cluster (11th + 9th round)
  STILL OPEN**. Both sites verified verbatim at
  `crates/sandbox-agent/src/handlers.rs:498-510` +
  `crates/sandbox/src/handlers.rs:132-139` — no movement.
- **1 new IMPORTANT finding** (R20-API1: driver-side
  `RewriteRestoreConfigPaths` introduces a third path-rewriter without
  a versioned schema marker).
- **`pub`-token count** (loose grep via `git ls-files | xargs grep`):
  r19 = 1064 → **r20 = 1064**. Δ = 0. R19-API1 fix was string-literal
  + tracing-field only; no surface delta.
- **Backlog**: r19 = 4 → r20 = 4 (1 closed, 1 new, 2 carries unchanged).

## CRITICAL

None.

## IMPORTANT

### R20-API1 — driver-side `RewriteRestoreConfigPaths` lands a third config-rewrite layer without a versioned schema marker (NEW)

- **Where**: T-8b-smoke-r15 (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-r15.md:289-297`) recommends Option 1: the Go driver (`nomad-driver-ch`) gains
  `RewriteRestoreConfigPaths` that reads `restore/config.json` and
  substitutes path-bearing fields (`serial.file`, `console.file`,
  `disks[].path`, `fs[].socket`, `vsock.socket`, `api_socket`) to the
  new alloc dir before `cloud-hypervisor --restore`. Driver pin already
  bumped v5 → v6 at `dea68995`.
- **Existing rewrite layers on the controller side**:
  1. `crates/sandbox/src/restore_handler.rs:1166-1204`
     `rewrite_config_json` — Rust, controller-driven, rewrites
     `net[].tap` + `net[].mac` from `vm_index`. Explicitly **excludes**
     path-bearing fields with the comment at `restore_handler.rs:1177-
     1181`: *"path-bearing fields … require the runtime NOMAD_TASK_DIR
     which the controller cannot know at job-submit time. The wrapper
     handles path rewrites at exec time."*
  2. `crates/sandbox/scripts/nomad-vm-wrapper.sh:486-623` —
     bash + embedded python, exec-time, rewrites `disks[].path`,
     `serial.file`, `console.file`, and (legacy) `fs[].socket` using
     `NOMAD_TASK_DIR`. Asserts `assert_under_task_dir` on every rewrite.
  3. **NEW**: Go driver `RewriteRestoreConfigPaths` — same fields as
     the wrapper, different language, different invocation point
     (before CH spawn, inside the Go process rather than the wrapper
     shell).
- **Problem**: three rewriters now own overlapping fields, and the
  snapshot artifact carries **no schema version** that lets any one
  rewriter declare "I understand this config shape." `SnapshotMetadata`
  (`snapshot_store.rs:50-64`) carries `ch_version` (CH binary version)
  but no rewriter-contract version. Concrete risks:
  1. **Wrapper / driver double-rewrite**: if the driver lands but the
     wrapper is not retired, both rewrite `serial.file` — second writer
     wins, but if either's "task dir" detection diverges, the final
     value is ambiguous. The wrapper's `assert_under_task_dir` guard
     would fire on the driver's output if their dir conventions drift.
  2. **Version skew**: when v2 adds a new path-bearing field (the
     proposal already hints at `vsock.socket` + `api_socket`), a
     v1-vintage rewriter on either side silently leaves the field
     pointing at the dead alloc — CH fails identically to C-7-LT-4
     but with no breadcrumb identifying which rewriter to fix.
  3. **Audit ambiguity**: an operator inspecting a wedged sandbox's
     final `config.json` cannot tell *which* layer last touched it.
- **Severity**: IMPORTANT — no immediate functional break (the
  driver-side fix is the correct architectural call per the cluster
  review), but every new layer that touches a shared artifact without
  a version marker compounds the contract debt. Today the wire surface
  is "CH consumed it"; tomorrow it's a multi-writer interface.
- **Recommendation**:
  1. **Emit a versioned schema marker on snapshot capture**. Add a
     top-level field — e.g. `_zsbx_path_schema_version: 1` — to
     `config.json` at snapshot-time (controller-side, in
     `snapshot_store.rs` or the capture path). The driver checks the
     marker before rewriting; mismatch returns a typed error rather
     than silently producing a bad config.
  2. **Document the rewriter division of responsibility** in one
     place. The controller (`rewrite_config_json`) owns network-shape
     fields; the wrapper *or* the driver (exactly one) owns path-shape
     fields. The repository should not ship with both wrapper and
     driver rewriting paths.
  3. **Pin the field-coverage matrix as a unit test** that fails when
     a new path-bearing field is added to v1 CH configs without
     updating both the rewriter contract and the schema version.

## MINOR

### R19-API2 (1-round carry) — `pub async fn run_wake_jobs_takeover_once` still has zero external callers

- **Where**: `crates/sandbox/src/sweep.rs:388`.
- **State**: LIVE unchanged. Sibling `run_wake_jobs_gc_once` at
  `sweep.rs:296` carries the same shape — `pub`, doc claims test-driver,
  no external test callers.
- **Recommendation**: unchanged from r19 — bundle two `pub →
  pub(crate)` demotions with the next visibility sweep so the precedent
  (`spawn_*` + `run_*_once` are `pub(crate)`) compounds.

### Considered + dismissed

- **`closure_ref = "R19-C1"` in the tracing event at `sweep.rs:401`**.
  Tracing fields are log-pipeline only (sandbox::wake::takeover target);
  they never reach the wire envelope at `render_wake_poll_response`.
  Operator-internal lineage is the correct destination for the review-ID
  breadcrumb. Not flagged.
- **`current_schema_version` at `db.rs:593`** is the postgres migration
  sequence (currently 12), not a snapshot-config schema. Distinct surface,
  unchanged from r19, not in scope for R20-API1.
- **R19-I4 `INSERT_WAKE_JOB_MAX_RETRIES = 3` const + retry loop**
  (`db.rs` via `f2485210`). Internal-to-crate const; handler shape and
  `InsertWakeJobOutcome` enum unchanged. No surface impact.
- **r17-Q2 / R19-M3 off-by-one doc fix** (`restore_handler.rs:283-300`
  via `ce66c10f`). Doc-comment prose only; signature unchanged.
- **§10.0 renderer test enumeration** (`admin_handlers.rs:2329-2347`
  via `6fbfafb3`). Test-internal; pins the existing envelope shape
  against future drift, no new public surface.

## §10.0 envelope post-R19-API1

The takeover-claimed wake's `GET /wake/{id}` response now reads:
```json
{"error": "wake_worker_aborted",
 "message": "wake worker aborted: controller did not complete the wake within the timeout (see operator runbook)",
 "state": "failed", "wake_id": "...", "sandbox_id": "...", "updated_at": "..."}
```
Operator-facing, no internal vocabulary, points at a documented escape
hatch. The R19-T1 renderer test pins `state == "failed"` +
`message.is_string()` across every `WakeErrorCode` variant — invariant
in place against future copy-edits that might accidentally drop the
state field or stringify the message. R19-API1's "lens hand-off to
test-coverage r20" line ("once R19-API1 is resolved, pin the new fixed
phrasing as an invariant") remains worth doing — the current test pins
the *shape* but not the *operator-runbook breadcrumb*. A string-contains
assertion on "operator runbook" would lock the contract that this URL is
discoverable.

## Cross-lens consensus

- **architecture r20**: R20-API1 is structurally an architecture
  question too — three rewriters with no versioning is a layering
  smell. Api-surface scopes the wire-contract piece (schema marker
  field name + envelope); architecture should scope the
  ownership-rule (which layer wins, retire the redundant rewriter).
- **code-quality r20**: R19-API2's two `pub → pub(crate)` demotions
  remain bundle-ready; no movement.
- **concurrency r20**: R19-C1 resolution holds at the wire layer.
  The takeover sweep's error_message is now operator-actionable;
  client retry loops can branch on the stable `error_code =
  "wake_worker_aborted"` without parsing prose.
- **test-coverage r20**: R19-T1 closure (renderer matrix) pins the
  shape but not the runbook-breadcrumb prose. A 1-line
  `assert!(message.contains("operator runbook"))` would harden the
  R19-API1 fix against silent prose regression.
- **security r19**: confirmed `RewriteRestoreConfigPaths` is a driver
  Go-side export (mentioned only in the security review note).
  R20-API1's schema-marker recommendation is on the boundary
  between security (untrusted snapshot-config ingestion) and
  api-surface (versioned multi-writer contract).

## Lens hand-off

- **To architecture r20**: own the multi-rewriter ownership rule
  (R20-API1 fix-step 2). Three rewriters cannot all be load-bearing;
  one must retire. The driver-side option (per cluster-r15) is the
  smallest blast radius — that implies retiring the
  wrapper's path-rewrite block at `nomad-vm-wrapper.sh:486-623` in
  the same PR cycle.
- **To code-quality r20**: R19-API2 sweep remains the cheapest open
  finding (~2 LOC, 2 sites).
- **To test-coverage r20**: pin a `message.contains("operator
  runbook")` assertion in the `r16_api1_failed_state_renders_…`
  loop iteration for `WakeWorkerAborted` — locks the R19-API1 prose
  against silent regression.
- **To security r20**: a `_zsbx_path_schema_version` field is also a
  trust-boundary hook — the driver's rewriter can refuse to operate
  on configs without the marker, closing a "driver runs untrusted
  rewrite logic on adversary-supplied config" path.

## Trend

- **`pub`-token count**: r19 = 1064; **r20 = 1064**. Δ = 0
  (R19-API1 was prose + tracing-field only).
- **`Result<_, String>`** (unchanged from r19): sandbox 161,
  sandbox-agent 14.
- **Net new wire envelope kinds r19→r20**: 0.
- **Wire-visible prose changes**: 1 (`error_message` for takeover-
  claimed wake — internal terms removed).
- **Closure velocity**: r19→r20 = +1 (R19-API1 in `fde4f51c`).
  Below r19's recent peak; backlog steady at 4.
- **Backlog open-item count**: r19 = 4; **r20 = 4**.

## Two most-critical citations

1. **`crates/sandbox/src/db.rs:3275-3344` +
   `crates/sandbox/src/sweep.rs:397-405`** — R19-API1 fix landed:
   wire-visible `error_message` is operator-facing prose; the R19-C1
   lineage breadcrumb lives in the `closure_ref` tracing field on
   the `sandbox::wake::takeover` target.
2. **`crates/sandbox/src/restore_handler.rs:1166-1204` +
   `crates/sandbox/scripts/nomad-vm-wrapper.sh:486-623`** — the two
   in-tree config rewriters that the incoming driver-side
   `RewriteRestoreConfigPaths` will overlap with. R20-API1 above;
   schema-marker + ownership-rule fix needed before the driver-side
   rewriter ships.
