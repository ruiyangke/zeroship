# Sandbox/snapshot-restore — api-surface r22 review

Date: 2026-05-25 (UTC).
HEAD at audit: `0e0eeffa` (last reviewed: r21 at `8bc11768`).
Round 22 catchup (was r21). Landings since r21 in `crates/sandbox/**`,
`crates/sandbox-agent/**`, `crates/core/**`:

- R10-API4 ACTUAL fix at sandbox-agent (`00a00d01`)
- R22-I1 terminal-overwrite tracing + metric (`f98611fb`)
- C-7-LT-12a `rootfs_source` restore-path Config field (`7fd661c9`)
- r25-r31 reviewer artifacts (orchestration only — out of scope)

Read-only.

## Summary

- **R10-API4 closure VERIFIED.** `crates/sandbox-agent/src/handlers.rs:498-516`
  now routes all three `readyz` paths through `error_envelope::error_response`.
  Controller (`crates/sandbox/src/handlers.rs:132-142`) and agent
  (`sandbox-agent/src/handlers.rs:498-516`) emit identically-shaped
  §10.0 errors on `/readyz`: `{error:"<code>", message:"<prose>"}`.
  The cross-service wire-shape drift flagged at R21-API1 is closed.
- **C-7-LT-12a `rootfs_source` field**: 11th field in restore-path
  ChPlugin Config (`restore_handler.rs:2371`). Absent from cold-boot
  builder (cold-boot derives the path via env-baked `ZSBX_ARTIFACT_DIR`
  + driver-hardcoded `chRootfsSourceName`). Adds a **third intentional
  divergence** to the field-list parity contract; previously two
  (`pubkey_hex`, `restore_from`). Test pin asserts presence + value at
  `restore_handler.rs:3634-3637` and the cross-side invariant at
  `restore_handler.rs:3654-3687`. Cold-boot has NO mirror pin for
  rootfs_source's absence, so the next "field forgotten on one side"
  regression still cannot be caught structurally.
- **R22-T1 (field-list parity test)**: 3-round consensus now —
  api-surface r21 R21-API2 + architecture r22 r22-A3 + test-coverage
  r22 R22-T1 + the r21-A1 deferred.md debt note. Still not landed.
  ETA: ~30 LOC, ~30 min. Sketch in R22-API1 below. **Highest-leverage
  open finding.**
- **r17-Q3 `DataIntegrity(String)` variant**: surveyed for cross-crate
  consumers — none. Used only by `crates/sandbox/src/db.rs` itself
  (the only crate that consumes `DatabaseError` outside the
  `zeroship-sandbox` crate is its own integration test files, which
  import via `zeroship_sandbox::db::*`). Sandbox-agent does not depend
  on `zeroship-sandbox::db`. Visibility downgrade considered + dismissed
  (see Considered + dismissed below).
- **`pub`-token count**: r21 = 1065, **r22 = 1067**. Δ = +2 — both
  in `crates/sandbox/src/metrics.rs` (`inc_wake_terminal_overwrite_blocked`
  + `wake_terminal_overwrite_blocked_value`, R22-I1). Both follow
  the established `#[doc(hidden)] pub fn *_value` convention (mirrors
  `vm_index_leak_value`, `takeover_orphan_value`, etc).
- **Backlog**: r21 = 5 → r22 = 4. Two closures (R10-API4 actually
  fixed; R21-API1 closed by the same commit), two carries remain
  (R20-API1 schema-marker; R19-API2 `pub(crate)` sweep), R21-API2
  now triple-consensus as R22-API1.

## CRITICAL

None.

## IMPORTANT

### R22-API1 — field-list parity contract test (3-round consensus) — RECOMMEND NOW

- **Where**: cold-boot builder `crates/sandbox/src/backend/nomad_ch.rs:2436-2462`;
  restore-path builder `crates/sandbox/src/restore_handler.rs:2353-2382`.
- **Status**: r21 flagged as R21-API2 (api-surface). r22 architecture
  flagged independently as r22-A3 (l.125-147). r22 test-coverage
  flagged independently as R22-T1 (l.50-70). Three lenses + r21-A1's
  own deferred.md debt note (l.1809) = unanimous. Not yet landed.
- **r22 evidence the gap still bites**: C-7-LT-12a (`7fd661c9`) added
  `rootfs_source` to restore-path Config only. That's the third
  intentional divergence after `pubkey_hex` and `restore_from`. The
  test pinned the field's *value*, but not the *parity contract*.
  A hypothetical future field added to cold-boot but forgotten on
  restore (the symmetric case of r21-A1) still passes both pinned
  presence lists.
- **Sketch** (~30 LOC, in `restore_handler.rs` test module): build
  both fixtures; extract `Config.as_object().keys()` as `BTreeSet<&str>`
  from each; assert `cold.difference(warm).is_empty()` AND
  `warm.difference(cold) == {"rootfs_source"}`. Note: `pubkey_hex`
  and `restore_from` are present in BOTH builders (differing *values*
  only — empty vs. populated), so the symmetric difference is
  `{"rootfs_source"}` only.
- **ETA**: <1 hour. Suggested title: `sandbox/jobspec: parity
  contract test (R22-T1 / R22-API1 / r22-A3)`.

### R20-API1 (2-round carry) — no `_zsbx_path_schema_version` on snapshot artifacts

- **Where**: `crates/sandbox/src/snapshot_store.rs:51-64` —
  `SnapshotMetadata` unchanged. No rewriter-contract version field
  emitted on capture.
- **State post-r22**: still LIVE. r22 added `rootfs_source` as a
  driver-side rewriter input (`7fd661c9`); the rewriter footprint now
  spans (controller jobspec emitter, driver hclspec validator, driver
  restore-branch stager). Three rewriter sites, still no version on
  the artifact they all read from.
- **Action**: unchanged from r20/r21 — emit
  `_zsbx_path_schema_version: 1`; gate driver-side rewriter on it.
- **Severity**: IMPORTANT, doubly motivated by C-7-LT-12a.

## MINOR

### R22-API2 — controller-side `readyz` test still synthesises response inline (test-binding gap unfixed)

- **Where**: `crates/sandbox/src/handlers.rs:1390-1414` —
  `readyz_200_body_is_status_ok` builds `HttpResponse::Ok().json(...)`
  directly; `readyz_503_body_is_envelope_compliant` calls
  `error_response(...)` directly. Neither calls `readyz(state).await`.
- **Contrast**: agent-side fix landed with **proper handler-bound
  tests** at `crates/sandbox-agent/src/handlers.rs:1117-1164` (three
  tests, all use `test::call_service(&app, req)` to invoke the actual
  handler).
- **Symptom**: a future edit to controller-side `readyz()` that
  changes the wire shape (e.g., adds a `retry_after` field) passes
  the existing tests but breaks the wire contract.
- **Recommendation**: port the agent-side `test::call_service` pattern
  to controller-side. ~15 LOC. Parent test module already imports
  ntex test infra — add a minimal `State` fixture (or reuse one if
  it exists).
- **Severity**: MINOR — the binding gap was flagged at r21; agent-side
  closed it correctly; controller-side did not. Test-coverage r22
  may have flagged similar.

### R22-API3 — `rootfs_source` field documentation asymmetric vs. cold-boot

- **Where**: restore-path comment (`restore_handler.rs:2343-2351`)
  explains *why* the field is explicit (snapshot config.json names
  the GC'd source-alloc dir). Cold-boot has NO matching comment
  saying "we DON'T emit this; the driver derives it from env."
- **Why surface-relevant**: future maintainers reading cold-boot
  `nomad_ch.rs:2436-2462` see `vm_index, kernel, cpus, memory_mb,
  restore_from, sandbox_id, user_id, workspace_img, user_home_img,
  pubkey_hex, subnet_base_octet, disks, fs, net` and naturally wonder
  "where does rootfs go?" — the answer (driver-side
  `chRootfsSourceName` constant + `ZSBX_ARTIFACT_DIR` env) is invisible
  here. Adds a "did you forget rootfs_source?" foot-gun.
- **Recommendation**: 3-line comment in cold-boot builder noting
  "rootfs source is derived by the driver from `ZSBX_ARTIFACT_DIR` +
  `chRootfsSourceName`; restore-path emits it explicitly because the
  snapshot's config.json lost that handle (C-7-LT-12a)."
- **Severity**: MINOR — comment-only; the parity test (R22-API1)
  partially mitigates by encoding the divergence machine-readably.

### Considered + dismissed

- **`DataIntegrity(String)` → `pub(crate)`**: structurally tempting
  (no cross-crate consumer; only used by `db.rs` + `tests/sandbox_pg_e2e.rs`),
  but `DatabaseError` itself is `pub` and the variant is reachable via
  match on a returned `Result<_, DatabaseError>` — downgrading the
  variant would require turning the whole enum into a `pub(crate)`
  +pub-facade pattern. Not worth it for one variant. The rustdoc
  frames it as "schema/code drift" — operator-grep-able as intended.
  **Re-evaluate** if r22 follow-up adds a third single-crate variant.
- **R10-API4 carry from r21**: closed at `00a00d01`. The agent-side
  tests at `:1117-1164` bind to the actual handler (better than
  controller-side, see R22-API2).
- **r17-Q3 carry-into-r22**: variant landed at r21 (`17d65f83`);
  r22 added no new consumers. No surface action.
- **`pub fn wake_terminal_overwrite_blocked_value`** (R22-I1):
  `#[doc(hidden)] pub fn` matches the established test-accessor
  pattern (`vm_index_leak_value`, `takeover_orphan_value`, etc).
  Net +2 pub; no surface-design concern.

## §10.0 envelope state post-r22

Both `/readyz` endpoints §10.0-compliant. Controller
(`handlers.rs:132-142`): 200 `{status:ok}` / 503
`{error:backend_unhealthy,message:...}`. Agent (`sandbox-agent
handlers.rs:498-516`): 200 `{status:ok}` / 503 `{error:draining|reaper_down,
message:...}`. Each crate carries its own `error_envelope.rs`; rationale
for the sibling vs. `zeroship-core` lift documented at
`sandbox-agent/src/error_envelope.rs:34-42`.

## Cross-lens consensus

- **architecture r22 (r22-A3)**: same finding as R22-API1. Architecture
  owns the carve-out decision; api-surface owns the test shape.
- **test-coverage r22 (R22-T1)**: same finding as R22-API1. Test-coverage
  owns the test landing; api-surface signs off on the assertion shape.
- **code-quality r22 (R22-I1)**: closed at `f98611fb`. Counter +2
  pub adds match the existing test-accessor pattern.
- **concurrency r22**: no concurrency surface in this round's
  landings.
- **security r22**: no security flag on `rootfs_source` —
  cluster-internal artifact path, controller-emitted, driver-validated.

## Lens hand-off

- **To architecture r22**: R22-API1 / r22-A3 are the same test. One
  lens lands it.
- **To code-quality r22**: R19-API2 `pub → pub(crate)` sweep still
  open. Cheapest open finding; not blocked on cluster work.
- **To test-coverage r22**:
  1. **R22-API1 / R22-T1** — land the field-list parity test
     (~30 LOC, sketch above). Closes 3-lens consensus.
  2. **R22-API2** — port the agent-side `test::call_service` pattern
     to controller-side `readyz` tests (~15 LOC). Re-bind the
     envelope assertion to the actual handler.
- **To security r22**: R20-API1 schema-marker carry unchanged.
  Driver-side rewriter validation is the natural enforcement point
  now that there are three rewriter sites.

## Trend

- **`pub`-token count**: r21 = 1065; **r22 = 1067** (Δ = +2;
  R22-I1 metric accessors).
- **`Result<_, String>`**: sandbox 166 (r21: 161); sandbox-agent
  14 (unchanged).
- **Net new wire envelope kinds r21→r22**: 2 (`draining`,
  `reaper_down` on agent-side `/readyz`).
- **Wire-visible body-shape changes r21→r22**: 3 (agent `/readyz`
  503-draining, 503-reaper-down, 200 — all moved to §10.0 envelope;
  200 changed from `"ready"` → `"ok"` to match controller).
- **New restore-path Config fields r21→r22**: 1 (`rootfs_source`).
  Restore Config now carries 14 fields; cold-boot 13.
- **Closure velocity r21→r22**: 2 fully closed (R10-API4 actual,
  R21-API1). One added (C-7-LT-12a parity divergence). Net backlog
  decrease.
- **Backlog open-item count**: r21 = 5; **r22 = 4** (R20-API1
  carry, R19-API2 carry, R22-API1/T1 consensus, R22-API2 new MINOR).

## Two most-critical citations

1. **`crates/sandbox/src/restore_handler.rs:2371`** + cold-boot
   absence at **`crates/sandbox/src/backend/nomad_ch.rs:2436-2462`** —
   `rootfs_source` lives only in restore-path Config (intentional;
   cold-boot derives via env). The third intentional divergence;
   pinned by value but not by parity contract. R22-API1 sketches the
   contract test that closes the remaining vector.
2. **`crates/sandbox-agent/src/handlers.rs:498-516`** + tests at
   **`:1117-1164`** — agent-side `/readyz` now §10.0-compliant AND
   the test pins call the actual `readyz(state).await` handler (not
   just `error_response` directly). Closes R10-API4 fully and sets
   the pattern controller-side should adopt (R22-API2).
