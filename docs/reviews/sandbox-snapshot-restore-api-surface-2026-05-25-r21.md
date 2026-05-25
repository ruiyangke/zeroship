# Sandbox/snapshot-restore — api-surface r21 review

Date: 2026-05-25 (UTC).
HEAD at audit: `8bc11768` (last reviewed: r20 at `b18782f6`).
Round 21 — three landings since r20:
- r17-Q3 (`17d65f83`) — `DatabaseError::DataIntegrity(String)` variant
- R10-API4 + R12-API1 readyz §10.0 (`528c3c44`)
- r21-A1 (`fcac5355`) — `user_id` emission on restore-path Config

Read-only. Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`,
`crates/core/**`.

## Summary

- **Three closures verified**, with one **scope-mismatch** finding:
  the R10-API4 closure at `528c3c44` only addresses the controller-side
  `readyz` (`crates/sandbox/src/handlers.rs:132-142`); the
  original R10-API4 site (`crates/sandbox-agent/src/handlers.rs:498-510`)
  is still emitting `{"status":"draining"}` / `{"status":"reaper-down"}`
  / `{"status":"ready"}` — pre-fix shape. R10-API4 was prematurely
  marked CLOSED in `deferred.md:911-918`. R12-API1 (controller-side)
  is the only carry actually fixed.
- **r17-Q3 `DataIntegrity(String)` variant** added to `pub enum
  DatabaseError` (`db.rs:246-316`). Net surface delta: +1 enum variant,
  pub. **No cross-crate consumers** — used only by the
  `zeroship-sandbox` crate's own integration tests at
  `crates/sandbox/tests/sandbox_pg_e2e.rs` (CasLost / SelfTakeoverRefused
  / NotFound / BootTimeout matches; new variant not yet asserted).
  Variant is reachable via match on `db::DatabaseError`; the rustdoc
  signals "schema/code drift" — operator-grep-able as intended.
- **r21-A1 `user_id` restore-path emission** verified live at
  `restore_handler.rs:2358`. Test pin asserts
  `config["user_id"] == "usr_alice"` at
  `restore_handler.rs:3607-3612`. Cold-boot has its own pin at
  `nomad_ch.rs:4493-4496` (`Some("alice")`).
  **Fixtures diverge** (`"alice"` cold-boot vs `"usr_alice"` restore) —
  both pin *presence*, neither pins *the same value or the
  same typed-id constraint*.
- **R20-A1 (three-rewriter / schema-version marker) STILL OPEN**.
  No `_zsbx_path_schema_version` on snapshot artifacts; `SnapshotMetadata`
  (`snapshot_store.rs:51`) unchanged. The r21-A1 fix is the
  two-emitter manifestation of the same uncontracted multi-writer
  surface flagged at r20.
- **2 new findings** (R21-API1: agent-side readyz still pre-§10.0;
  R21-API2: cold-boot ↔ restore-path field-list parity has no
  contract test). 1 IMPORTANT carries from r20 (R20-API1 schema marker).
- **`pub`-token count**: r20 = 1080 (re-measured at `b18782f6` for
  consistency with this round's method, supersedes r20's reported
  "1064" which used a slightly different scope) → **r21 = 1081**.
  Δ = +1 — accounted for by `DatabaseError::DataIntegrity`.
- **Backlog**: r20 = 4 → r21 = 5 (1 carry closed, 2 new carries,
  R20-API1 still open).

## CRITICAL

None.

## IMPORTANT

### R21-API1 — agent-side `readyz` still emits pre-§10.0 wire shape; R10-API4 prematurely marked CLOSED (NEW)

- **Where**: `crates/sandbox-agent/src/handlers.rs:498-510`. The
  bodies emitted are still:
  - 503 draining: `{"status":"draining"}`
  - 503 reaper-down: `{"status":"reaper-down"}`
  - 200 ready: `{"status":"ready"}`
- **R10-API4's site of record** (per
  `docs/reviews/sandbox-snapshot-restore-api-surface-2026-05-25-r10.md:136`)
  is the **agent-side** file. The closure note in
  `deferred.md:911-918` claims R10-API4 was fixed at `528c3c44`, but
  that commit only touched `crates/sandbox/src/handlers.rs:132-142`
  (the **controller-side** readyz — R12-API1's site).
- **Symptom**: the controller-side admin endpoint contract is now
  §10.0-compliant; the agent-side readyz still drifts. Operators
  parsing JSON bodies from `/readyz` get one shape from the
  controller and another from the agent, on the same wire.
- **Evidence**: agent has its own `ErrorEnvelope` at
  `crates/sandbox-agent/src/error_envelope.rs:55-95` — readyz is the
  only HTTP handler in the agent that bypasses it. `proxy.rs` and
  `handlers.rs` error paths all route through it.
- **Severity**: IMPORTANT — wire-contract drift between two endpoints
  named identically. Either bring agent-side in line (the strict
  reading from r10) OR add the carve-out comment that r10
  recommended (probe-shape, body is operator-visible). The current
  state is "claimed fixed but unfixed at the original site."
- **Recommendation**:
  1. **Re-open R10-API4** in `deferred.md` and either:
     - Apply the same `error_response(SERVICE_UNAVAILABLE,
       "draining" | "reaper_down", "...")` pattern at
       `sandbox-agent/src/handlers.rs:498-510`, **OR**
     - Add the carve-out comment at line 498 documenting that this
       endpoint intentionally bypasses §10.0 because Kubernetes /
       Nomad probes read status code only, not body. Either is fine;
       silently leaving it is what the original finding asks to fix.
  2. Pin a test in `sandbox-agent` mirroring the one added at
     `crates/sandbox/src/handlers.rs:1390-1414` — current readyz
     contract is unpinned in the agent.

## MINOR

### R21-API2 — cold-boot ↔ restore-path Config field-list parity has no contract test (NEW)

- **Where**: cold-boot builder at
  `crates/sandbox/src/backend/nomad_ch.rs:2436-2462`; restore-path
  builder at `crates/sandbox/src/restore_handler.rs:2344-2371`.
- **Status post-r21-A1**: field lists are now identical except for
  two intentional divergences documented in code comments:
  - Cold-boot emits `pubkey_hex = <session pubkey>`; restore emits
    `pubkey_hex = ""` because CH ignores `--cmdline` on `--restore`
    (`restore_handler.rs:2361-2362`).
  - Restore emits `restore_from = alloc_dir`; cold-boot emits
    `restore_from = ""` (or whatever the caller passes;
    `nomad_ch.rs:2433-2435`).
- **Problem**: every other field (`vm_index`, `kernel`, `cpus`,
  `memory_mb`, `sandbox_id`, `user_id`, `workspace_img`,
  `user_home_img`, `subnet_base_octet`, `disks`, `fs`, `net`) must
  match between the two builders, or the driver's parser branches
  diverge between cold-boot and wake. r21-A1 is the proof — `user_id`
  fell out of parity for one commit-cycle and only smoke-r17 caught
  it. The two test pins (`nomad_ch.rs:4486-4519` cold-boot;
  `restore_handler.rs:3586-3613` restore-path) are independent — each
  asserts a presence list, but a new field added to one and forgotten
  on the other passes both pins.
- **Recommendation**: a contract test in either file that builds
  both fixtures and asserts
  `cold_boot_fields.symmetric_difference(restore_fields) ==
   {"pubkey_hex", "restore_from"}` (the two intentional divergences).
  Adds one test, catches every future "user_id"-style miss.
  Approximately 30 LOC, no new public surface. **This is the
  "field-list contract test" the r21-A1 closure note itself flags as
  the natural follow-up** (`deferred.md:1809`, "Debt note").
- **Severity**: MINOR — r21-A1 fixed today's symptom; this test
  closes the regression vector. Pairs naturally with R20-A1's
  schema-marker work.

### R20-API1 (1-round carry) — no `_zsbx_path_schema_version` on snapshot artifacts; three-rewriter ownership unresolved

- **Where**: `crates/sandbox/src/snapshot_store.rs:51-64`
  (`SnapshotMetadata` — `ch_version` only, no rewriter-contract
  version). Cluster review T-8b-smoke-r15 recommended emitting the
  marker before driver-side `RewriteRestoreConfigPaths` ships.
- **State**: LIVE unchanged. The two-emitter problem flagged
  internally by r21-A1's commit message (`fcac5355`) is the
  controller-side mirror of the same root cause: multi-writer shared
  artifact without a version contract.
- **Recommendation**: unchanged from r20 — emit
  `_zsbx_path_schema_version: 1` on capture, gate the rewriter on it.
  Now doubly motivated (one rewriter pair on the controller side, three
  rewriter sites overall including the driver and wrapper).

### Considered + dismissed

- **`DatabaseError::DataIntegrity(String)`** — public variant on `pub
  enum` (`db.rs:246-316`); only consumed by the crate's own
  `tests/sandbox_pg_e2e.rs`. No cross-crate surface drift; rustdoc
  frames as "schema/code drift" — grep-able + operator-meaningful.
- **Restore fixture `"usr_alice"` vs cold-boot `"alice"`** — both
  pin presence; builder doesn't `validate_typed_id` internally
  (boundary check upstream). Fixture noise, not a surface flaw.
- **`current_schema_version`** at `db.rs:593` is the pg migration
  sequence (12); distinct from the proposed snapshot-config marker.

## §10.0 envelope state post-r21

Controller readyz 503 now reads
`{"error":"backend_unhealthy","message":"backend probe failed; service not ready"}`,
pinned by `readyz_503_body_is_envelope_compliant`
(`handlers.rs:1398-1414`). 200 emits `{"status":"ok"}`. Agent-side
readyz drift: see R21-API1.

**Test-binding gap**: the new readyz tests synthesise the response
inline (build the body directly / call `error_response` directly)
rather than calling `readyz(state).await`. They pin the envelope
shape but not the binding from `readyz()` to it. Future edits that
change `readyz()` to emit a different shape pass these tests.
Flagged for test-coverage r21.

## Cross-lens consensus

- **architecture r21**: R21-API1 (agent vs controller readyz drift)
  is structurally architectural — two services name an endpoint
  differently. Api-surface owns wire-shape; architecture owns the
  carve-out-vs-§10.0 decision.
- **code-quality r21**: r17-Q3 closure aligns with the
  loud-failure-over-silent-fallback preference. No new findings.
- **concurrency r21**: no concurrency surface in this round's
  landings. R19-API2 `pub(crate)` sweep remains bundle-ready.
- **test-coverage r21**: R21-API2 (parity test) and the readyz
  test-binding gap are test-coverage targets; r21-A1 closure note
  already names parity test as debt.
- **security r21**: agent-side readyz emits `reaper-down` over the
  wire — operator-debuggable, not sensitive. No security flag.

## Lens hand-off

- **To architecture r21**: own the "agent-side readyz: §10.0 or
  carve-out" decision. Either is fine; the silent drift is what
  api-surface flags.
- **To code-quality r21**: R19-API2 `pub → pub(crate)` sweep still
  open (1-round carry from r20). Cheapest open finding.
- **To test-coverage r21**:
  1. Pin a `readyz(state).await` end-to-end test in controller-side
     `handlers.rs` so the new envelope shape is bound to the actual
     handler, not just to `error_response`. ~10 LOC.
  2. Pin a field-list parity test between
     `nomad_ch::build_chplugin_jobspec` and
     `restore_handler::build_restore_nomad_job_json` (R21-API2). ~30 LOC.
- **To security r21**: R20-API1 schema-marker carry — the driver
  can refuse to operate on configs without the marker, closing the
  "untrusted snapshot-config ingestion" path. Unchanged from r20
  hand-off.

## Trend

- **`pub`-token count**: r20 (re-measured at `b18782f6`) = 1080;
  **r21 = 1081**. Δ = +1 (DataIntegrity variant).
- **`Result<_, String>`** (unchanged from r20 snapshot): sandbox
  161, sandbox-agent 14.
- **Net new wire envelope kinds r20→r21**: 1 (`backend_unhealthy` —
  controller readyz only; agent-side readyz unchanged).
- **Wire-visible body-shape changes**: 2 (controller readyz 200
  `"ready"` → `"ok"`; controller readyz 503
  `{"status":"backend-unhealthy"}` → §10.0 envelope).
- **Closure velocity**: r20→r21 = +3 nominal (r17-Q3, R10-API4
  partial, R12-API1), but only 2 fully closed (r17-Q3, R12-API1).
- **Backlog open-item count**: r20 = 4; **r21 = 5** (R20-API1
  carry, R19-API2 carry, R21-API1 new, R21-API2 new, R10-API4
  re-opened).

## Two most-critical citations

1. **`crates/sandbox-agent/src/handlers.rs:498-510`** —
   agent-side `readyz` still emits pre-§10.0 wire shape
   (`{"status":"draining"}` / `{"status":"reaper-down"}` /
   `{"status":"ready"}`). R10-API4's site of record was here, not
   the controller-side site that `528c3c44` fixed. R10-API4
   prematurely marked CLOSED in `deferred.md:911`; re-open.
2. **`crates/sandbox/src/restore_handler.rs:2344-2371` ↔
   `crates/sandbox/src/backend/nomad_ch.rs:2436-2462`** —
   the two ChPlugin Config emitters. r21-A1 brought `user_id` to
   parity (`restore_handler.rs:2358`, pinned at
   `restore_handler.rs:3607-3612`); R21-API2 above recommends a
   contract test that pins symmetric_difference to the two
   intentional divergences only (`pubkey_hex`, `restore_from`).
