# Sandbox/snapshot-restore — api-surface r23 review

Date: 2026-05-25 (UTC).
HEAD at audit: `dd2079a9` (last api-surface review: r22 at `0e0eeffa`).
Round 23 catchup (was r22). Read-only.

User-stated HEAD `03d3470f` has been overtaken in-tree by:

- T1 `sandbox_admin_ro` (committed in three steps: `7b5d84f5` auth enum,
  `97fcbcda` AppState boot, `038ff3c7` e2e matrix + deferred close).
- R24-T1 stress harness scripts (`61492e54`, `dd2079a9` — out of scope
  for api-surface, no wire change).
- v14/v34 host_dir lifecycle + driver-side `ch/stop_task.go` work is
  **uncommitted** (working-tree modifications to
  `crates/sandbox/src/backend/nomad_ch.rs`, `restore_handler.rs`,
  `sweep.rs`). Per the read-only directive these files are **not
  audited** for the in-flight changes; only their committed surface
  through HEAD `dd2079a9` is reviewed below.

## Summary

- **R22-API1 / R22-T1 CLOSED at `b6c55d93`.** Field-list parity test
  for ChPlugin Config landed in `restore_handler.rs` (~111 LOC).
  Pinned: `cold ^ restore == {rootfs_source}`. The 3-round consensus
  (api-surface r22 R21-API2 + architecture r22 r22-A3 + test-cov
  R22-T1) is structurally closed; future field forgotten on one side
  fails the test.
- **T1 (admin RO role) HTTP-layer LANDED.** `AdminRole::{Full,
  ReadOnly}` enum, `admin_check_required` with constant-time bearer
  comparator, `assert_distinct_admin_tokens` boot guard, 9 new e2e
  matrix tests, 11 new lib unit tests. All new `pub` declarations are
  `pub(crate)` or `pub fn` builder-shaped (mirroring
  `with_admin_token` / `admin_token()` from A5). **No pub-surface
  leaks; no `#[doc(hidden)] pub fn` accessors needed**.
- **New §10.0 envelope kind: `admin_api_disabled`.** Emitted on 503
  when an endpoint's required role has zero configured tokens (Full
  requested with `admin_token=None`, OR ReadOnly requested with both
  None). Wire shape: `{error: "admin_api_disabled", message:
  "admin api disabled (SANDBOX_ADMIN_TOKEN_PATH...)"}`. **Compliant**
  with the §10.0 envelope contract. Distinct from `pg_disabled` /
  `unauthorized` / `insufficient_role` — operator tooling can branch.
- **New §10.0 envelope kind: `insufficient_role`.** Emitted on 403
  when a presented bearer matches the RO token but the endpoint
  requires Full. Distinct from `unauthorized` (401) so operator
  tooling can split "unknown bearer" from "valid but wrong role."
- **`export_user` is now `Full`-gated.** The endpoint reclassifies
  from the prior "any admin bearer" to `AdminRole::Full`
  (`admin_handlers.rs:835`). Wire-visible: RO bearer on
  `/admin/users/{id}/export` now returns 403 `insufficient_role`
  (was 200). The success body shape (200, FLAT JSON, no envelope) is
  unchanged. The error-body shape on 4xx/5xx remains the §10.0
  envelope via `err_safe` / `err`. **Audited: clean**.
- **R24-I1 wake error_message threading**: `wake_machine.rs:185-203`
  now carries `error_code` + `error_message` in the terminal-overwrite
  WARN log. Surface-irrelevant (tracing target only, not on the
  wire); no api-surface impact.
- **Controller-side disk preflight (`30960451`)**: emits free-text
  `Result<(), String>` only. The error rolls up through
  `RestoreHandlerError::Backend(_)` → `WakeErrorCode::RestoreFailed`
  → wire code `restore_backend_failed`. **The driver-msg text is
  propagated verbatim into the §10.0 `message` field** (see
  R23-API1 below).
- **`pub`-token count**: r22 = 979 (recomputed under
  `grep -roE '\bpub\b' crates/sandbox/src crates/sandbox-agent/src`);
  r23 = 994. Δ = +15. All within the T1 commit set + R22-T1 parity
  test + 30960451 preflight helpers; **zero new wire-public types**.
  Of the 15: 7 in tests (#[test] fn — counted by the regex), 3 are
  `pub(crate)` (AdminRole, admin_check_required,
  assert_disk_image_present, assert_distinct_admin_tokens), 2 are
  `pub fn` AppState builders (with_admin_ro_token, admin_ro_token())
  mirroring the A5 admin_token pattern, and the remainder are in-test
  helpers.
- **Backlog**: r22 = 4 → r23 = 4. One closure (R22-API1 / R22-T1 at
  `b6c55d93`), one new finding (R23-API1 driver-msg-verbatim
  propagation), two carries (R20-API1 schema-marker, R19-API2
  `pub(crate)` sweep), one MINOR new (R23-API2 export reclassification
  documentation gap).

## CRITICAL

None.

## IMPORTANT

### R23-API1 — driver-msg verbatim propagation into `WakeErrorCode::RestoreFailed` lacks structural typing

- **Where**: error flow from
  `crates/sandbox/src/backend/nomad_ch.rs:3638` (`assert_disk_image_present`)
  through `submit_restore_job` (`restore_handler.rs:2071-2089`) into
  `RestoreHandlerError::Backend(String)`, classified by
  `wake_machine.rs:645` as `WakeErrorCode::RestoreFailed` → wire
  `restore_backend_failed`, with the raw driver-style string in the
  §10.0 `message` field.
- **Current shape on the wire**:
  ```json
  {
    "error": "restore_backend_failed",
    "message": "restore submit: workspace.img missing for sandbox sbx_XXX (snapshot teardown should have preserved it via stop_preserving_state; controller will not submit restore job that the driver's preflight would reject with a generic Failed-tasks rollup): disk image post-stage stat failed: /var/zeroship/ch/<sid>/workspace.img (No such file or directory (os error 2)); controller-side parity check for driver preflight",
    "state": "failed",
    "wake_id": "wak_...",
    "sandbox_id": "sbx_...",
    "updated_at": ...
  }
  ```
- **Why this matters for api-surface**: the `message` field is
  declared opaque in `error_envelope.rs` ("safe prose"); the actual
  contents leak (a) the controller's host_state_dir prefix
  (`/var/zeroship/ch/<sid>/`), (b) `os error 2` text from
  `std::fs::metadata` — the operating-system-locale-dependent kernel
  error string (would be Japanese on a JA_JP locale worker, breaks
  English-string `grep` operator runbooks), (c) the internal helper
  prose "controller-side parity check for driver preflight". The
  intent (per `admin_handlers.rs:328-344` `err_safe` doc) is that
  internal-detail text reaches journald via `tracing::error!`, NOT
  the wire envelope. The wake_machine fault-classification path
  bypasses `err_safe` because it's classified at the WakeMachine
  layer, not the HTTP handler layer.
- **Why the controller-side preflight (`30960451`) makes this worse**:
  pre-30960451, a missing workspace.img surfaced as a Nomad
  "Failed tasks" rollup with no path detail. Post-30960451 the
  controller explicitly surfaces the path string at the controller's
  submit site — which is correct for journald operator triage but
  the same string is now also on the public-wire `message`.
- **The right shape**: extend `WakeErrorCode` with a new
  `StagingPathMissing` variant (or sub-shape the existing
  `RestoreFailed` with a typed sub-kind). Wire code:
  `staging_image_missing` (new snake_case kind, distinct from
  `restore_backend_failed` so the SLO dashboard can split "preflight
  bug" from "ch-remote restore returned non-zero"). The `message`
  field carries only the fixed prose "controller-side disk image
  preflight failed; see operator logs for path"; the path goes only
  to `tracing::error!`. Mirrors r24-A1 Phase 2 (`error_code =
  staging_manifest_drift`).
- **Severity**: IMPORTANT. The leak is admin-bearer-gated so the
  blast radius is "Full bearer holder reads operator-internal paths
  they shouldn't" — but defense-in-depth on the §10.0 envelope is
  the established posture (cf. r4-S4 → `err_safe`). The same shape
  applied 30+ times in admin_handlers.rs; once at the
  wake-classification layer is one more.
- **Recommended action**: bundle with r24-A1 Phase 1 (the
  `StagingManifest` typed-contract proposal) since both close the
  same wire-shape gap from different angles. Phase 1 adds the typed
  struct; this finding adds the typed `WakeErrorCode` variant. Both
  land together so the drift-prevention (compile-time) and
  drift-reporting (typed wire code) ship in the same surface PR.

### R20-API1 (3-round carry) — no `_zsbx_path_schema_version` on snapshot artifacts

- **Where**: `crates/sandbox/src/snapshot_store.rs:51-64` —
  `SnapshotMetadata` unchanged through r23.
- **State post-r23**: still LIVE. The rewriter footprint added a
  fourth site this round: controller-side disk-image preflight
  (`nomad_ch.rs:3638`) is now a fourth rewriter / validator over the
  same path schema (cold-boot Config builder + restore-path Config
  builder + driver hclspec validator + controller preflight). Four
  validators, still no version stamp on the artifact they all read.
- **Action**: unchanged from r20/r21/r22 — emit
  `_zsbx_path_schema_version: 1` on capture; gate the driver-side
  rewriter on it. A non-matching version fails the driver's hclspec
  with a typed error before any disk image is touched.
- **Severity**: IMPORTANT, now quadruply motivated. The 30960451
  staging-window patch landed without a schema-marker check; if a
  Phase 2 r24-A1 `staging_manifest` field is added under the same
  path schema, that's a fifth rewriter.

## MINOR

### R23-API2 — `export_user` Full-role reclassification has no API-doc comment recording the wire-visible status-code change

- **Where**: `admin_handlers.rs:835` —
  `if let Err(r) = admin_check_required(&req, &state, AdminRole::Full)`.
  The doc comment at `:820-834` explains *why* export is Full (GDPR
  cascade aggregation, blast radius parity with delete), which is
  the right architectural rationale. **It does NOT name the
  wire-visible status-code change**: pre-T1 an RO bearer on this
  endpoint got 401 (no RO bearer existed) or 200 (with the prior
  single-bearer scheme); post-T1 it gets 403 `insufficient_role`.
- **Symptom**: a future operator runbook reader (or a script that
  hard-codes "any 4xx from export = bad bearer") sees the same
  endpoint return two different status codes across the T1
  deployment boundary and has nothing in the source to grep for.
- **Recommendation**: 4-line comment under `:824` along the lines
  of: "Wire change at T1 (2026-05-25): pre-T1 this endpoint accepted
  any bearer present in `admin_token`; post-T1 the RO bearer
  returns 403 `insufficient_role`. Operator tooling that branched
  on 200 vs. 401 must add a 403 arm." Comment-only, ~3 min.
- **Severity**: MINOR — wire change is correct, only documentation
  gap. Mirrors the audit log discipline; the deferred backlog
  `[T1]` entry has the boundary note, but the source-of-truth is
  the handler doc comment, not the backlog.

### R23-API3 — `WakeErrorCode::wire_code` vs. `WakeErrorCode::as_str` asymmetric for `SlotUnavailable` / `RestoreFailed` / `Internal` but `WakeWorkerAborted` matches both

- **Where**: `db.rs:1530-1607`. The wire-vs-internal mapping at
  `:1588-1607`:
  ```
  | internal variant         | wire code (existing)        |
  |--------------------------|------------------------------|
  | `SlotUnavailable`        | `vm_index_unavailable`       |
  | `SourceTeardownTimeout`  | `source_teardown_timeout`    |
  | `RestoreFailed`          | `restore_backend_failed`     |
  | `LivezTimeout`           | `livez_timeout`              |
  | `ClockResyncFailed`      | `clock_resync_failed`        |
  | `RegisterFailed`         | `register_failed`            |
  | `Internal`               | `internal_error`             |
  | `WakeWorkerAborted`      | `wake_worker_aborted`        |  ← same
  ```
  Three variants have asymmetric internal-vs-wire (`slot_unavailable`
  vs. `vm_index_unavailable`, `restore_failed` vs.
  `restore_backend_failed`, `internal` vs. `internal_error`); five
  are symmetric.
- **Why surface-relevant**: a future
  `WakeErrorCode::StagingPathMissing` (proposed in R23-API1 above)
  will face the same fork — pick the symmetric `staging_path_missing`
  or the asymmetric `staging_image_missing`. The rustdoc justifies
  the existing forks ("reuse the **existing** snake_case error
  codes already emitted by every other landed endpoint, do NOT
  invent parallel codes" at `:1560`). The rule reads correctly but
  the cases that follow it (`SlotUnavailable` → `vm_index_unavailable`
  because that's the cold-boot 503 wire code) and don't (`WakeWorkerAborted`
  → `wake_worker_aborted` because no prior endpoint emitted it) are
  the only two coherent classes. **Future variants must be
  classified by "is there a pre-existing wire code on another
  endpoint" before naming**.
- **Recommendation**: add a one-line rule at `:1561-1562`: "If the
  failure shape has a sibling wire code on another endpoint, reuse
  it (asymmetric internal/wire is fine). If it's novel, the wire
  code matches the internal variant snake_case (symmetric). Drift
  between the two forks is bug-shaped; spell out which one applies
  in the variant doc-comment." 3 lines of rustdoc, no behavior
  change.
- **Severity**: MINOR — the existing eight wire codes are correct;
  this is forward-pressure for the next variant.

### Considered + dismissed

- **`AdminRole` → `pub`**: today `pub(crate)`. External consumers
  (e.g., a tenant-facing crate that wanted to assert role gating)
  would need access. None exists today; the deferred backlog's
  Phase-5 plans don't surface one either. Keep `pub(crate)`. The
  builder-shape `with_admin_ro_token` / `admin_ro_token()` is the
  only cross-crate surface; tests can construct the enum locally via
  the doc-pinned ordering of bearer-resolution rules.
- **`admin_api_disabled` → 501 instead of 503**: 503 matches the
  prior `pg_disabled` convention (post-auth concern). 501 would
  imply "this endpoint is permanently unimplemented" which is
  wrong (it's wired but disabled by config). 503 is correct.
- **`insufficient_role` body should carry the role required**:
  considered (so operator tooling could log "needed Full, got RO"
  directly from the response). Dismissed — leaks the role-name
  taxonomy in a way that a future RBAC extension may want to keep
  internal. The status-code split (401 vs. 403) is the contract;
  the prose at `:284-289` is the implementation detail.
- **R22-API2 (controller-side `readyz` test still synthesises
  response inline)**: still open from r22; not bundled in this
  round's landings. Carry to test-coverage r23.
- **R22-API3 (`rootfs_source` documentation asymmetric)**: still
  open from r22; comment-only. Bundle with the next nomad_ch.rs
  doc-cleanup commit.

## §10.0 envelope state post-r23

### Inventory (current admin-side wire kinds)

The crate's admin surface now emits the following envelope kinds.
Underlines mark **net-new since r22**.

```
unauthorized              (401)  — bearer absent / not matching either token
insufficient_role         (403)  — bearer is valid RO but endpoint needs Full   ← NEW (T1)
admin_api_disabled        (503)  — required role has zero tokens configured     ← NEW (T1)
pg_disabled               (503)  — post-auth pg not configured (existing)
wake_not_found            (404)  — poll-wake row evicted / wrong sandbox
wake_wiring_unavailable   (503)  — wake feature wired but db None
invalid_sandbox_id        (400)  — typed_id parse failure
invalid_wake_id           (400)  — typed_id parse failure
invalid_user_id           (400)  — typed_id parse failure
feature_disabled          (501)  — snapshot_enabled=false
restore_backend_failed    (200 body or 500)  — `WakeErrorCode::RestoreFailed`
vm_index_unavailable      (200 body or 503)  — `WakeErrorCode::SlotUnavailable`
source_teardown_timeout   (200 body)         — async-only
livez_timeout             (200 body)         — async-only
clock_resync_failed       (200 body)         — async-only
register_failed           (200 body)         — async-only
internal_error            (200 body or 500)  — `WakeErrorCode::Internal`
wake_worker_aborted       (200 body)         — `WakeErrorCode::WakeWorkerAborted`
backend_unhealthy         (503)              — controller /readyz
draining                  (503)              — agent /readyz
reaper_down               (503)              — agent /readyz
database_failed           (500)              — sync wake path
database_error            (500)              — generic
pg_pool_unavailable       (503)              — gdpr export/delete pool
pg_tx_begin_failed        (500)              — gdpr export tx
pg_tx_isolation_failed    (500)              — gdpr export tx
pg_commit_failed          (500)              — gdpr export tx
export_sandboxes_failed   (500)              — gdpr export
export_shares_failed      (500)              — gdpr export
export_events_failed      (500)              — gdpr export (via err_safe)
export_tombstones_failed  (500)              — gdpr export
count_events_failed       (500)              — gdpr export
```

**Audit of T1-gated endpoints for envelope consistency**:

| Endpoint | Required | 401 shape | 403 shape | 503 shape | Success shape |
|---|---|---|---|---|---|
| `GET /admin/sandboxes` | ReadOnly | env | n/a (RO accepted) | `admin_api_disabled` env | flat |
| `GET /admin/sandboxes/{id}` | ReadOnly | env | n/a | `admin_api_disabled` env | flat |
| `GET /admin/users/{id}/sandboxes` | ReadOnly | env | n/a | `admin_api_disabled` env | flat |
| `GET /admin/users/{id}/shares` | ReadOnly | env | n/a | `admin_api_disabled` env | flat |
| `GET /admin/hosts` | ReadOnly | env | n/a | `admin_api_disabled` env | flat |
| `GET /admin/sandboxes/{id}/wake/{wake_id}` | ReadOnly | env | n/a | `admin_api_disabled` env | flat / env+extras |
| `POST /admin/sandboxes/{id}/snapshot` | Full | env | `insufficient_role` env | `admin_api_disabled` env | flat |
| `POST /admin/sandboxes/{id}/wake` | Full | env | `insufficient_role` env | `admin_api_disabled` env | flat |
| `POST /admin/sandboxes/{id}/cold-boot` | Full | env | `insufficient_role` env | `admin_api_disabled` env | feature_disabled env |
| `DELETE /admin/users/{id}` | Full | env | `insufficient_role` env | `admin_api_disabled` env | flat |
| `GET /admin/users/{id}/export` | Full | env | `insufficient_role` env | `admin_api_disabled` env | flat |

Every endpoint's error shape passes through `error_response` (the
sole §10.0 envelope factory in `error_envelope.rs`). **No envelope
drift detected on the T1 surface**.

## Cross-lens consensus

- **architecture r24 (r24-A1 Phase 1)**: same staging-path drift
  surface as R23-API1, viewed from the controller's typed-struct
  angle. Architecture owns the `StagingManifest` struct; api-surface
  owns the typed `WakeErrorCode` variant. Both close the same wire
  gap; land together.
- **architecture r24 (r24-A1 Phase 2)**: introduces
  `error_code=staging_manifest_drift` — already-typed wire code on
  the driver↔controller path. This is api-surface-relevant because
  it's a cross-process protocol message; r24-A1 Phase 2 is the right
  place to land the **typed enum on both sides** (driver Go + controller
  Rust) rather than two free-text protocols. **Recommend**: when r24-A1
  Phase 2 lands, the controller's serialized form is a `serde_json`
  payload of a `staging_manifest_drift` body whose shape is pinned
  via a `crates/core/` shared type if any future driver↔controller
  protocol message lands. Today, this is the FIRST such message; if
  it lands, the precedent is set.
- **concurrency r23 (R23-I1)**: terminal-overwrite e2e tests landed
  at `234c3bdf`. Counter wiring is now end-to-end verified; no api
  shape change (counter is internal-metric only).
- **code-quality r24 (R24-I1)**: closed at `1c255a00`. Threading
  error_code+error_message into the WARN log is tracing-only; no
  wire impact.
- **security r23**: no new wire surface from the T1 landings beyond
  the §10.0 kinds above. The constant-time bearer comparator and
  the boot-time distinct-tokens guard are both internal-only.

## Lens hand-off

- **To architecture r24**: R23-API1 + r24-A1 are the same wire gap
  from two angles. Bundle into one PR: r24-A1 Phase 1 adds typed
  `StagingManifest`, R23-API1 adds typed `WakeErrorCode::StagingPathMissing`.
- **To test-coverage r23**:
  - R22-API2 carry — controller-side `readyz` tests still synthesise
    response inline (not via `test::call_service`). Test-coverage
    owns the landing; agent-side pattern at
    `sandbox-agent/handlers.rs:1117-1164` is the template.
  - New: pin the §10.0 envelope inventory above as a single
    enumeration test that loads every error-emitting code path and
    asserts the body conforms to the envelope shape.
- **To security r24**: R20-API1 schema-marker carry (now 3-round +
  quadruply-motivated; preflight makes a fourth validator). The
  driver-side validator is the natural enforcement point.
- **To code-quality r23**: R19-API2 `pub → pub(crate)` sweep still
  open. Lowest-friction; not blocked on cluster work. The T1
  landings did NOT add to the open-`pub` count materially (all new
  `pub fn` are AppState builders following the established
  with_admin_token / admin_token() pattern; the only new
  `pub(crate)` items are AdminRole, admin_check_required,
  assert_disk_image_present, assert_distinct_admin_tokens).

## Backlog carry table

| ID | First round | Status r23 | Severity | Lens to own |
|---|---|---|---|---|
| R19-API2 | r19 | Open (carry) | MINOR | code-quality |
| R20-API1 | r20 | Open (3-round carry; quadruple motivation) | IMPORTANT | security |
| R22-API1 / R22-T1 / R21-API2 / r22-A3 | r21 | **CLOSED** at `b6c55d93` | — | — |
| R22-API2 | r22 | Open (carry) | MINOR | test-coverage |
| R22-API3 | r22 | Open (comment-only) | MINOR | code-quality |
| R23-API1 | r23 | **NEW** | IMPORTANT | architecture (bundle with r24-A1) |
| R23-API2 | r23 | **NEW** | MINOR | code-quality |
| R23-API3 | r23 | **NEW** (forward-pressure) | MINOR | code-quality |

Net: r22 open = 4 → r23 open = 6 (one closure, three new). The
backlog grew this round because T1 landed a substantial new
surface (two new envelope kinds + a role-reclassified endpoint),
and the staging-window patch surfaced a latent typing gap that the
prior surface didn't have a fault to expose. None of the new
items are CRITICAL; R23-API1 is the highest-leverage and is
already on the r24-A1 critical-path through architecture.

## Trend

- **`pub`-token count** (under
  `grep -roE '\bpub\b' crates/sandbox/src crates/sandbox-agent/src`):
  r22 = 979 (recomputed); **r23 = 994** (Δ = +15). Breakdown:
  T1 commits +9 (AdminRole enum + admin_check_required + 2 AppState
  accessors + assert_distinct_admin_tokens + 4 in-test fixtures);
  R22-T1 commit +3 (parity test fixture helpers); 30960451 +3
  (assert_disk_image_present + create_ext4_image_if_missing post-
  condition rewrites + fsync_dir). No new `pub` types on the
  cross-crate boundary.
- **`Result<_, String>`**: sandbox 166 (r22: 166); sandbox-agent
  14 (unchanged). 30960451's new helpers return `Result<_, String>`
  — consistent with the existing pattern (sandbox-side error
  handling is colloquially `String`-shaped; the typed-error story
  for the host-staging error path is what R23-API1 proposes).
- **Net new wire envelope kinds r22→r23**: 2 (`insufficient_role`,
  `admin_api_disabled`). Both from T1.
- **Wire-visible body-shape changes r22→r23**: 1 (`export_user`
  reclassified Full → RO bearer now 403 not 200).
- **New `WakeErrorCode` variants r22→r23**: 0. (R23-API1 proposes
  `StagingPathMissing`; not yet landed.)
- **New rewriter sites for `_zsbx_path_schema_version`**: +1
  (controller preflight at `nomad_ch.rs:3638`). r22 was 3, r23 is
  4. R20-API1 is now quadruply motivated.
- **Closure velocity r22→r23**: 1 fully closed (R22-API1/T1/R21-API2/r22-A3,
  3-round consensus + 1 deferred debt note → unified close at
  `b6c55d93`). 3 added (R23-API1 / R23-API2 / R23-API3). Backlog
  growth driven by new surface (T1 envelope kinds + 30960451
  preflight typing gap), not by drift.
- **Backlog open-item count**: r22 = 4; **r23 = 6** (R19-API2 carry,
  R20-API1 carry, R22-API2 carry, R22-API3 carry, R23-API1 new,
  R23-API2 new, R23-API3 new).

## Two most-critical citations

1. **`crates/sandbox/src/admin_handlers.rs:170-273`** — the
   `admin_check_required` function defines the entire T1 role-gate
   contract: 401 / 403 / 503 envelope shapes, constant-time
   comparator, RO/Full hierarchy. The 11 endpoint sites (lines 411,
   534, 593, 687, 758, 835, 1004, 1342, 1551, 1869, 2005) read this
   single source-of-truth — no per-handler reimplementation. The
   wire-visible envelope inventory (§10.0 state above) is fully
   determined by this function's `Err(error_response(...))` calls.
   Anything that wants to change the role taxonomy or add a
   third role goes here.

2. **`crates/sandbox/src/db.rs:1493-1607`** + classifier at
   **`crates/sandbox/src/wake_machine.rs:641-655`** — the wake-error
   typed surface. `WakeErrorCode` is the structured contract;
   `wire_code()` maps it to the §10.0 envelope's `error` field;
   `classify_failure` is the (single) site that decides which
   variant a `RestoreHandlerError` maps to. R23-API1 lands here as
   a new variant + classifier arm; R23-API3 is forward-pressure on
   the rustdoc rule for picking the next wire-code name. Any future
   wake failure mode that wants a distinct wire identity goes
   through these three files.
