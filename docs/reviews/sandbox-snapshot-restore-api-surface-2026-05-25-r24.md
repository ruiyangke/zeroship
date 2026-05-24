# Sandbox/snapshot-restore — api-surface r24 review

Date: 2026-05-25 (UTC). HEAD at audit: `a482f00d` (prior api-surface
review r23 at `dd2079a9`). Read-only.

Worktree: `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.

Landed since r23:

- `79871194` — `WakeErrorCode::StagingPathMissing` variant + wire
  code `staging_image_missing` + migration 0013 (R23-API1 closure
  step 1).
- `022f778a` — producer chain wired: `SubmitRestoreError` typed
  enum at trait boundary, `RestoreHandlerError::StagingPreflight`,
  classifier + admin handler arms, path-free Display pin, typed-id
  form (R23-API1/R25-S1/R25-I1/R25-I2 closure step 2).
- `6476d18b` — backlog entries closed (deferred.md only).
- `a482f00d` — pilot reviewer artifacts (architecture r25,
  test-coverage r25); no source-of-truth impact.

Plus pre-r23-window items still on the surface checklist this round:

- `e82bffd7` — host_dir GC sweeper (5-min cadence) — observability
  surface (R24-SWEEP1 below).
- `d638b10f` — leak host_dir on stop + verbatim driver-msg
  propagation (already on r23's R23-API1 path).

## Summary

- **R23-API1 CLOSED at `79871194` + `022f778a`.** The 3-round
  api-surface carry (r21→r22→r23) lands the typed
  `WakeErrorCode::StagingPathMissing` variant with wire code
  `staging_image_missing` AND closes the security side-leak
  (host-path + locale `os error 2` no longer cross the wire).
  Audit checks pass on all four mandated invariants (wire envelope
  shape, structured `extra`, path-free `message`, typed-id form
  for `sandbox_id`). Detail at **R24-API1-VERIFY** below.
- **`SubmitRestoreError` is `pub` (cross-crate visible).** New
  surface type at `restore_handler.rs:159`. The `RestoreBackend`
  trait — also `pub` at `:397` — changed signature from
  `Result<(), String>` to `Result<(), SubmitRestoreError>`. Per
  AGENTS.md pre-launch no-back-compat directive this is fair game;
  no out-of-crate consumers exist (tests in `sandbox/tests/*` are
  the only external `RestoreBackend` users and they were
  recompiled in the same PR). **No CRITICAL finding; one MINOR
  note at R24-API2** about `pub` vs. `pub(crate)` and the small
  ergonomics gap between `SubmitRestoreError::preflight()` (kw
  constructor) and the rest of the crate's `Error`-shaped variants.
- **WakeErrorCode triangle pinned for all 9 variants** at three
  test sites (`db.rs:3623`, `db.rs:3652`, `admin_handlers.rs:2462`).
  The gold-standard recommendation from code-quality r25 is
  satisfied: round-trip + wire-code map + admin-render
  exhaustiveness — no `_` catch-all in any of the three
  `match`/array sites; adding variant #10 compile-errors all three.
- **AdminRole envelope consistency on POST endpoints**: sampled 3
  POST sites + 1 DELETE (`snapshot/{id}`, `wake/{id}`,
  `cold-boot`, `DELETE /users/{id}`). All four route through the
  same `admin_check_required` chokepoint at `admin_handlers.rs:170`
  with the same 3 envelope kinds (`unauthorized` 401,
  `insufficient_role` 403, `admin_api_disabled` 503). Zero drift —
  the §10.0 envelope shape is uniform across POST/DELETE.
- **Migration 0013 wire impact**: forward-only DROP+ADD on
  `wake_jobs_error_code_check`. The new domain value
  `staging_path_missing` is producer-gated; no row writes attempt
  it until both 79871194 (variant) and 022f778a (producer wiring)
  land. **Rolling-restart concern at R24-MIG1** below — the migration
  is idempotent but the producer cuts in atomically, so a controller
  on commit `022f778a^^` cannot serve a row produced by a peer on
  `022f778a` (it lacks the `from_str_opt` arm). Mid-deploy hazard
  exists.
- **Sweeper observability surface (`e82bffd7`)**: GC loop spawns at
  INFO, per-reap is INFO, per-tick aggregate (with non-zero
  scanned/reaped) is DEBUG. The `target` field is
  `sandbox::host_dir_gc` uniformly — operators can
  `RUST_LOG=sandbox::host_dir_gc=info` to follow reaps without the
  per-tick noise. **One MINOR observability gap at
  R24-SWEEP1** about the per-tick aggregate being DEBUG (not INFO)
  for the zero-reap case — operators see no "I'm alive" beat from
  the sweeper unless a reap happens. The startup INFO + per-reap
  INFO covers the "is the sweeper running" check, but a 5-min
  reap-free interval is opaque without `=debug`.
- **`pub`-token count**: r23 = 994 → r24 = 998. Δ = +4 (under
  `grep -roE '\bpub\b' crates/sandbox/src crates/sandbox-agent/src`).
  Three new: `SubmitRestoreError` (enum), `SubmitRestoreError::preflight`
  (constructor), `SubmitRestoreError::log_detail` (tracing helper).
  One was already there (`RestoreHandlerError::StagingPreflight`'s
  fields land under the pre-existing `pub enum RestoreHandlerError`
  declaration so no new `pub` token). All four sit on the same
  `RestoreBackend` trait boundary that's already `pub`; not new
  cross-crate surface area, but they ARE technically reachable from
  out-of-crate (see R24-API2).
- **`Result<_, String>`**: sandbox 166 (r23) → 182 (r24) on
  current count, but the producer-side `submit_restore_job`
  signature changed AWAY from `Result<_, String>` so the delta is
  driven by test fixture growth and `nomad_post_blocking` callers,
  NOT by new wire-shape regressions. The trend is otherwise stable.
- **Backlog**: r23 = 6 → r24 = 6 (one closure R23-API1, no new
  CRITICAL/IMPORTANT, three new MINOR/observability — net flat).

## CRITICAL

None.

## IMPORTANT

(No new IMPORTANT this round. R23-API1 closure converts the
prior IMPORTANT to a verify pass; R20-API1 / R22-API2 carries
remain in their existing severity.)

### R24-API1-VERIFY — R23-API1 closure: wire-shape conformance pass

**Mandate**: confirm the new `StagingPathMissing` variant renders
the wire envelope `{"error": "staging_image_missing", "message":
"<path-free>", ...}` with structured fields in `extra` and no
host path in `message`.

**Audit checks** (all pass):

1. **Wire `error` field = `staging_image_missing`** ✓
   At `db.rs:1643-1652` the `wire_code()` impl maps
   `StagingPathMissing → "staging_image_missing"`. Pinned at
   `db.rs:3674-3675` (`wake_error_code_wire_code_uses_existing_envelope_codes`).
   Admin handler render at `admin_handlers.rs:1990-2008` reads
   `row.error_code.unwrap_or(Internal).wire_code()` — so the
   wire `"error"` field is `staging_image_missing` on the async
   poll path. The sync POST path at `admin_handlers.rs:1305-1316`
   hard-codes the literal `"staging_image_missing"` so both async
   and sync paths emit the same code (locked by 3 separate tests).

2. **pg `wake_jobs.error_code` = `staging_path_missing`** ✓
   At `db.rs:1574-1575` the `as_str()` impl maps the variant to
   `"staging_path_missing"`. Migration 0013 at
   `migrations/0013_wake_jobs_staging_path_missing_code.sql:61-71`
   extends the CHECK domain to admit this value. Test at
   `db.rs:3623-3641` (`wake_error_code_as_str_round_trip`) pins
   the round-trip — internal pg form ≠ wire form by design.

3. **`message` is path-free** ✓
   At `restore_handler.rs:113-122` the `RestoreHandlerError::StagingPreflight`
   thiserror `Display` is `"staging image missing: {which} for
   {sandbox_id_typed}"`. No `path` field on the variant at all —
   the wake-machine boundary at `wake_machine.rs:408-425` calls
   `e.log_detail(&sandbox_id_typed)` (tracing-only, path-bearing)
   THEN destructures only `which` into the typed error. Pinned by
   the new test `staging_preflight_display_is_path_free` at
   `wake_machine.rs` (cited in commit `022f778a`).
   Async path renders this `message` verbatim via
   `admin_handlers.rs:1996-2000` (`row.error_message.clone()`).
   Sync path renders an equivalent literal at
   `admin_handlers.rs:1309`. Both forms are path-free.

4. **Structured fields in `extra`** ✓ for sync path; **partial** for
   async path. Sync path at `admin_handlers.rs:1311-1315` puts
   `{which, sandbox_id}` into `extra`. Async path at
   `admin_handlers.rs:2001-2006` puts `{state, wake_id,
   sandbox_id, updated_at}` into `extra` — `which` is NOT in the
   async `extra`. **Asymmetry**: async clients recovering `which`
   must parse it out of `message` ("staging image missing:
   workspace.img for sbx_..."). This is technically a minor
   structured-data gap but not a closure-blocker — see
   **R24-API3** for the recommendation.

5. **Locale leak gone** ✓ The verbatim `os error 2` /
   `metadata stat failed` strings are confined to the
   `SubmitRestoreError::Preflight.source` field which is consumed
   only by `log_detail()` (tracing-only). The wire `message` is
   constructed from `which` (`"workspace.img"`/`"user_home.img"`)
   + `sandbox_id_typed` — no `std::fs::metadata` text path.

**Verdict**: R23-API1 is structurally CLOSED. The 4 mandated
invariants hold under the as-shipped code; future drift breaks
at one of the three pin tests
(`wake_error_code_as_str_round_trip`,
`wake_error_code_wire_code_uses_existing_envelope_codes`,
`r16_api1_failed_state_renders_every_wake_error_code`) +
`staging_preflight_display_is_path_free`.

## MINOR

### R24-API2 — `SubmitRestoreError` is `pub` but only consumed within the `sandbox` crate

- **Where**: `crates/sandbox/src/restore_handler.rs:159`
  (`pub enum SubmitRestoreError`), `:222-251` (`pub fn preflight`,
  `pub fn log_detail`), `:218-220`
  (`impl std::error::Error for SubmitRestoreError`).
- **Snippet**:
  ```rust
  pub enum SubmitRestoreError {
      Preflight { which: &'static str, path: PathBuf, source: String },
      Other(String),
  }
  ```
- **Why minor**: the type is the trait `RestoreBackend`'s
  associated error. Trait is `pub` at `:397`, so its associated
  error MUST be `pub` for the trait to be implementable by
  out-of-crate callers. **But** no out-of-crate caller exists in
  the workspace — every `RestoreBackend` use is inside
  `crates/sandbox/src/` or `crates/sandbox/tests/` (verified by
  `grep restore_handler::RestoreBackend` across the workspace).
  If `RestoreBackend` itself were narrowed to `pub(crate)` (which
  it could be: every consumer is in-crate including the tests
  via `use zeroship_sandbox::restore_handler::RestoreBackend`,
  which COULD use `crate::restore_handler::RestoreBackend` if the
  tests moved inline), `SubmitRestoreError` could also be
  `pub(crate)`.
- **Trade-off**: tests in `crates/sandbox/tests/*` import the
  trait by absolute path:
  ```rust
  // sandbox_pg_e2e.rs:2704
  let backend_dyn: std::sync::Arc<dyn zeroship_sandbox::restore_handler::RestoreBackend> =
  ```
  Rust's integration tests (under `tests/`) are separate crates;
  the trait MUST be `pub` to be reachable. So `SubmitRestoreError`
  needs `pub` for the same reason `RestoreBackend` does. **Status**:
  the `pub` is justified by the integration-test design. Marking
  this as MINOR / no-action because it's structurally required
  by Rust integration-test mechanics.
- **The actual nit**: `SubmitRestoreError::preflight` is a
  `pub fn` constructor (`:240-244`) shaped like a builder, while
  the parallel `Error`-shaped types in the crate use bare
  `Self::Variant{..}` literals at producer sites. The
  constructor exists to colocate the field-order discipline
  (`which` before `path` before `source`) but it's the only
  `pub fn` constructor on an error type in the entire crate.
  Adding it doesn't hurt; removing it doesn't help. Comment-only
  observation, no action.
- **Severity**: MINOR (observation; no fix required).

### R24-API3 — Async wake-poll path omits `which` from `extra`

- **Where**: `admin_handlers.rs:1971-2019`
  (`render_wake_poll_response` — async poll handler) vs.
  `admin_handlers.rs:1305-1316` (`map_restore_error` — sync POST
  handler).
- **Sync path puts `which` in `extra`**:
  ```rust
  // admin_handlers.rs:1311-1315
  .with_extra(serde_json::json!({
      "which": which,
      "sandbox_id": sandbox_id_typed,
  }))
  ```
- **Async path does NOT**:
  ```rust
  // admin_handlers.rs:2001-2006
  .with_extra(serde_json::json!({
      "state": "failed",
      "wake_id": row.wake_id,
      "sandbox_id": row.sandbox_id,
      "updated_at": row.updated_at_secs,
  }))
  ```
  The async path's `extra` is shared across ALL wake-error
  variants — it can't carry a variant-specific `which` field
  without either (a) walking each variant in the handler, or
  (b) persisting the structured `which` to a new column on
  `wake_jobs` (option C per the architecture proposal — rejected
  because it requires a schema change).
- **Wire-visible impact**: async clients on
  `GET /admin/sandboxes/{id}/wake/{wake_id}` that want to branch
  on resource (`workspace.img` vs. `user_home.img`) must parse it
  out of `message`. The `message` form is stable
  (`"staging image missing: workspace.img for sbx_..."`) but
  message-parsing is brittler than a structured field.
- **Why MINOR not IMPORTANT**: the operator-actionable
  distinction is at the wire-code level
  (`staging_image_missing` ≠ `restore_backend_failed`). The
  inner `which` is a sub-resource detail. A future fault that
  needs the `which` distinction at machine-parseable depth would
  either (a) introduce a sub-code (`staging_workspace_missing` /
  `staging_user_home_missing`) or (b) follow option C. Both are
  out of scope for the current closure.
- **Recommendation**: comment on `render_wake_poll_response`
  noting the async/sync asymmetry; or store the variant's
  structured `which` in a future `wake_jobs.error_extra JSONB`
  column (a Phase-2 schema migration). Today, status-quo is
  acceptable.
- **Severity**: MINOR (1 round; no action this PR).

### R24-MIG1 — Migration 0013 rolling-restart hazard

- **Where**: `crates/sandbox/migrations/0013_wake_jobs_staging_path_missing_code.sql`.
- **Shape**: DROP CHECK + ADD CHECK with extended domain. The
  ADD CHECK is gated by an `IF NOT EXISTS` guard, the DROP CHECK
  is gated by `IF EXISTS` — idempotent. Same shape as 0012.
- **The hazard**: between commits `79871194` (db variant
  landed, migration extends domain) and `022f778a` (producer
  wires the variant), the controller that wrote the row uses
  the new variant only post-`022f778a`. A controller still at
  `79871194` would NEVER produce a `staging_path_missing` row
  (the variant exists in code but no producer fires). So far
  so good for forward-only.
- **Reverse direction**: a controller at `022f778a` (or later)
  produces `staging_path_missing` rows. If an operator rolls
  back to a controller at `79871194~` (or earlier — say
  `c729c2b8`), `WakeErrorCode::from_str_opt` LACKS the
  `staging_path_missing` arm and a SELECT on the old row would
  return `None` for `error_code`. The `wake_job_row_from_pg`
  contract treats `Some(unknown_code)` as `DataIntegrity` error
  (per r17-Q3 comment at `db.rs:3693-3705`). That's a 500 on
  any admin GET that touches the row → "rollback breaks
  reads".
- **Pre-launch posture**: per AGENTS.md, no production users —
  this is dev-discipline only, not a published-user-blocker.
  Rolling restarts between cluster nodes ARE in scope for the
  pre-launch cluster smoke (T-8b-stress harness), but a roll-FORWARD
  from old-code to new-code is fine (old controller writes only
  old codes; new controller reads all). A roll-BACKWARD (new
  back to old) is the hazard.
- **Recommendation**:
  - Document the migration's forward-only stance on its docstring
    (currently says "Forward-only. Idempotent." but doesn't
    spell out the reverse-direction `from_str_opt` gap).
  - For mid-deploy load, operators should drain `wake_jobs` of
    `staging_path_missing` rows (or wait T_KEEP=5min for sweep
    eviction) before rolling back. The T_KEEP eviction makes
    this self-healing within 5 min for read-side breakage; the
    `wake_jobs_gc` sweep at `db.rs` purges terminal rows older
    than T_KEEP.
  - The driver↔controller protocol does NOT carry the wire code
    (it's controller-emitted only), so driver-side rollbacks have
    no impact.
- **Severity**: MINOR — documented design constraint; not a
  shipping bug. Carry into the deferred backlog as a note on
  cluster runbook discipline.

### R24-SWEEP1 — Host-dir GC observability: zero-reap ticks are DEBUG-only

- **Where**: `crates/sandbox/src/sweep.rs:1146-1190`
  (`spawn_host_dir_gc`) + `:1109-1116` (per-reap INFO) +
  `:1180-1187` (per-tick DEBUG).
- **Current observability levels**:
  - Loop start: INFO with `interval_secs`, `grace_secs`,
    `host_state_dir` (one event per controller boot).
  - Per-reap (when a directory is actually removed): INFO with
    `sandbox_id`, `host_dir`, `sandbox_row_status`, `age_secs`
    ("sandbox host_dir GC: reaped").
  - Per-tick AGGREGATE (when at least one was scanned or reaped):
    DEBUG with `scanned`, `reaped` ("sandbox host_dir GC: tick").
  - Skipped sandboxes (non-terminal state / pending wake_jobs):
    DEBUG.
  - rm -rf failure on a reap: WARN with `sandbox_id`, `host_dir`,
    `error`.
  - Skip (no database): INFO at startup.
- **Concern**: an operator running with `RUST_LOG=info` sees the
  startup banner once, then NOTHING for 5 minutes between reap
  events on a quiet cluster. The standard "is the sweeper
  alive?" check requires no signal — but a deadlocked /
  silently-failing sweeper looks identical to a healthy quiet
  one for arbitrary intervals. The per-reap INFO covers the
  "reap happened" path; there's no "heartbeat I'm running"
  signal at INFO.
- **Comparison to peer sweepers**: the `wake_jobs_gc` sweep,
  `transient_state_takeover` sweep, and lease-watcher all have
  per-tick INFO with a delta-only filter (e.g. only log when
  scanned > 0 OR reaped > 0). The host_dir GC uses DEBUG for
  that case — likely because a populated cluster would log every
  5 minutes (8 reaps over an hour = 12 lines) which seems
  excessive but is in fact useful.
- **Recommendation**: promote the per-tick aggregate from DEBUG
  to INFO when `scanned + reaped > 0` (mirror the existing
  pattern in `wake_jobs_gc`). Optionally, add a once-per-hour
  heartbeat INFO that fires regardless of scan/reap activity so
  the operator can grep "host_dir GC: tick" and see 12 lines per
  hour from a healthy sweeper. Comment-only / 2-line change.
- **Severity**: MINOR — observability surface, not a
  correctness gap. Bundle with the next sweep.rs touch.

### Considered + dismissed

- **`SubmitRestoreError` → `RestoreHandlerError::StagingPreflight`
  field name mismatch**: the producer uses `which: &'static str`
  on both types — symmetric. The sink-side type also carries
  `sandbox_id_typed: String` for the wire form; producer-side
  `SubmitRestoreError::Preflight` carries `path: PathBuf` +
  `source: String` for the tracing logs. The split is structurally
  correct: tracing-only fields live on the producer-side type,
  wire-bound fields on the sink-side type. No nit.
- **`message` for sync path includes typed-id; async path
  has it in `extra.sandbox_id` AND in `message`**: this is
  technically duplicated information in the async path. Dismissed
  because (a) `extra.sandbox_id` is already the row's `sandbox_id`
  (a `pub` column distinct from `extra.wake_id`), so it'd be
  there even if no error fired, and (b) the `message`
  embedding makes operator grep-by-typed-id work
  ("grep sbx_abc" finds the wake_jobs.error_message column AND
  the wire envelope). Useful duplication.
- **`pub(crate)` → `pub` for the 4 new tokens**: only required
  by Rust integration-test mechanics. See R24-API2.

## §10.0 envelope state post-r24

### Inventory (delta from r23)

```
New since r23:
  staging_image_missing       (200 body — async-wake-poll Failed
                               state; OR 500 — sync-wake POST
                               map_restore_error) — wake_error_code
                               `staging_path_missing`
```

All other §10.0 codes (32 total) unchanged from r23.
**`staging_image_missing` is now the 9th wire code from the
`WakeErrorCode` family** (was 8 at r23).

### POST-endpoint envelope audit (sample)

| Endpoint | Required | 401 | 403 | 503 | Success | Post-T1 wire-shape drift? |
|---|---|---|---|---|---|---|
| `POST /admin/sandboxes/{id}/snapshot` | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 200 | none |
| `POST /admin/sandboxes/{id}/wake` (sync mode) | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 200 (with `agent_url`) | none |
| `POST /admin/sandboxes/{id}/wake` (async mode) | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 202 (`wake_id`) | none |
| `POST /admin/sandboxes/{id}/cold-boot` | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | `feature_disabled` 501 | none |
| `DELETE /admin/users/{id}` | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 200 (delete tombstone) | none |

All 5 write/mutating endpoints flow through the same
`admin_check_required(req, state, AdminRole::Full)` chokepoint
at `admin_handlers.rs:170-273`. No drift on the §10.0 envelope
across the T1-Full subset; the 4xx/5xx shapes are
single-source-of-truth.

## Cross-lens consensus

- **Architecture r24 (r24-A1 Phase 1)**: surfaced the same wire-shape
  drift as R23-API1 from the typed-struct angle. **Status**: this
  round's `SubmitRestoreError` IS the typed struct (option B
  from the architecture proposal). r24-A1 Phase 1 is therefore
  partially absorbed into 022f778a — only the cross-process
  protocol message (Phase 2) remains, and it's not on the surface
  yet.
- **Code-quality r25 (R25-I1 / R25-I2 / R25-S1)**: all closed in
  the same commit chain. The api-surface lens confirms the
  closure on the wire side; code-quality confirms it on the
  source side.
- **Security r25 (R25-S1)**: the host-path-leak closure verified
  here at R24-API1-VERIFY step 3 (Display path-free).
- **Concurrency r24 (R23-I1)**: terminal-overwrite counter wiring
  unchanged this round.
- **Test-coverage r25 (R22-T1 / new pin)**: the
  `staging_preflight_display_is_path_free` pin test is a new
  Negative-test entry; future Display drift fails it.

## Lens hand-off

- **To architecture r25**: r24-A1 Phase 2 (driver↔controller
  cross-process typed payload) is still open. The
  `SubmitRestoreError` boundary established this round is the
  template for any future cross-process-protocol typed messages.
- **To test-coverage r25**:
  - R22-API2 carry — controller-side `readyz` tests still
    synthesise response inline (not via `test::call_service`).
    Test-coverage owns the landing; agent-side pattern at
    `sandbox-agent/handlers.rs:1117-1164` is the template.
  - Pin the §10.0 envelope inventory above as a single
    enumeration test that loads every error-emitting code path
    and asserts the body conforms to the envelope shape (carry
    from r23).
- **To security r25**: R20-API1 schema-marker carry (now
  3-round + quadruply-motivated; the preflight is the 4th
  rewriter site). The driver-side validator is the natural
  enforcement point. Note: the `SubmitRestoreError` typed
  boundary doesn't address R20-API1 — that's a separate
  snapshot-artifact schema-marker concern.
- **To code-quality r26**: R19-API2 `pub → pub(crate)` sweep
  still open (4 new `pub` tokens this round are all justified
  by integration-test mechanics; see R24-API2). R22-API3
  (`rootfs_source` documentation asymmetric) still open;
  comment-only. R24-API2 nit (`SubmitRestoreError::preflight`
  constructor) optional.
- **To concurrency r25**: no api-surface findings cross over
  this round. The wake-machine submit branch matching pattern
  on `SubmitRestoreError` (the `if let Err(e @ ...) = ... {
  e.log_detail(...); let Self::Preflight { which, .. } = e else
  { unreachable!() };` shape at `wake_machine.rs:408-414`) is
  slightly awkward — `unreachable!()` is reachable only if Rust
  ever changes pattern-match exhaustiveness — but it's
  pattern-correct. The compiler verifies the variant is one of
  two; the `match` outside is the discriminator.

## Backlog carry table

| ID | First round | Status r24 | Severity | Lens to own |
|---|---|---|---|---|
| R19-API2 | r19 | Open (carry) | MINOR | code-quality |
| R20-API1 | r20 | Open (3-round carry; quadruple motivation) | IMPORTANT | security |
| R22-API2 | r22 | Open (carry) | MINOR | test-coverage |
| R22-API3 | r22 | Open (comment-only) | MINOR | code-quality |
| R23-API1 | r23 | **CLOSED** at `79871194` + `022f778a` | — | — |
| R23-API2 | r23 | Open (comment-only) | MINOR | code-quality |
| R23-API3 | r23 | Open (forward-pressure / rustdoc rule) | MINOR | code-quality |
| R24-API2 | r24 | **NEW** — observation only, no action | MINOR | — |
| R24-API3 | r24 | **NEW** — async/sync `extra` asymmetry | MINOR | architecture (Phase-2 schema decision) |
| R24-MIG1 | r24 | **NEW** — rolling-restart hazard documentation | MINOR | code-quality / docs |
| R24-SWEEP1 | r24 | **NEW** — sweep heartbeat visibility | MINOR | code-quality |

Net: r23 open = 6 → r24 open = 6 (one closure, four new MINOR,
all observability/documentation). No new IMPORTANT or CRITICAL.

## Trend

- **`pub`-token count**: r23 = 994; **r24 = 998**. Δ = +4
  (SubmitRestoreError enum + preflight constructor + log_detail
  method + a fields-modifier on the existing pub enum
  RestoreHandlerError variant which doesn't add a `pub` token
  directly but contributes via `pub` constructors used in
  tests). No cross-crate consumers exist outside integration
  tests.
- **`Result<_, String>`**: sandbox 166 (r23) → 182 (r24).
  Driver: not from `submit_restore_job` (now typed) but from
  test-helper proliferation; `assert_disk_image_present` and
  `nomad_post_blocking` still return `Result<_, String>`, and
  their test surfaces grew. Production-side typing is steadily
  improving (`SubmitRestoreError` is the third typed-error
  introduction this quarter after `WakeErrorCode` and
  `RestoreHandlerError`).
- **Net new wire envelope kinds r23→r24**: 1
  (`staging_image_missing`).
- **Wire-visible body-shape changes r23→r24**: 0. The new code
  is additive only — no existing endpoint changed status code
  or body shape.
- **New `WakeErrorCode` variants r23→r24**: 1
  (`StagingPathMissing`). All 9 variants now exercised in the
  triangle (`as_str` / `from_str_opt` / `wire_code`) — closure
  on the gold-standard rec from code-quality r25.
- **New rewriter sites for `_zsbx_path_schema_version`**: 0
  (`workspace_image_path` / `user_home_image_path` helpers
  reused — R25-I1 closure means the 4th rewriter that r23 flagged
  was unwound back to a deriver call, not a new inline join).
  R20-API1 motivation count holds at quadruple; no further
  drift.
- **Closure velocity r23→r24**: 1 closed (R23-API1, 3-round
  carry → unified close at `79871194` + `022f778a`). 4 new
  MINOR / observability / documentation findings — net flat
  (6 → 6). The closure is high-value (typed wire surface +
  security side-leak); the new findings are all comment-only
  or 2-line changes.
- **Backlog open-item count**: r23 = 6; **r24 = 6** (R19-API2
  carry, R20-API1 carry, R22-API2 carry, R22-API3 carry,
  R23-API2 carry, R23-API3 carry, R24-API2 new, R24-API3 new,
  R24-MIG1 new, R24-SWEEP1 new — minus R23-API1 closed).

## Pre-launch back-compat check

Per AGENTS.md "pre-launch, no back-compat" directive:

- **`RestoreBackend::submit_restore_job` signature change**
  (`Result<(), String> → Result<(), SubmitRestoreError>`): in-crate
  callers + integration-test callers updated in the same commit.
  No external (other workspace crate) consumer found. **Allowed
  under pre-launch rule**.
- **`WakeErrorCode` variant addition**: enums in Rust are
  forward-compatible at the source level (additive variants
  compile); pg `from_str_opt` is the runtime check. Roll-forward
  is fine; roll-backward fails on the new pg domain value — see
  R24-MIG1.
- **Migration 0013 wire impact**: forward-only domain
  expansion; mid-deploy hazard documented at R24-MIG1.
- **§10.0 envelope contract**: additive `staging_image_missing`
  code; no existing code's shape changed. Per AGENTS.md "wire
  formats are immutable contracts" — additive expansion is
  allowed.

**Verdict**: 022f778a's pre-launch posture is correct; the
trait signature change would be a CRITICAL back-compat
violation in a published environment. Pre-launch, it's the
right call.

## Two most-critical citations

1. **`crates/sandbox/src/restore_handler.rs:155-251`** —
   `SubmitRestoreError` (the new typed-error boundary at the
   `RestoreBackend` trait): defines the producer-to-classifier
   wire contract. The `Preflight` arm carries path + source
   structurally; the `log_detail` method emits them via tracing
   at the wake-machine boundary before the typed map collapses
   the variant into the path-free `RestoreHandlerError::StagingPreflight`.
   Adding a new typed preflight failure mode goes here.

2. **`crates/sandbox/src/db.rs:1529-1657`** — `WakeErrorCode`
   enum + `as_str` (pg form) + `from_str_opt` (pg → variant)
   + `wire_code` (variant → §10.0 wire code). All 9 variants
   exercised in three pin tests. Triangle pinned: `as_str
   ⇔ from_str_opt` round-trip + `wire_code` snake_case +
   admin-handler render exhaustiveness. Adding a new variant
   compile-breaks all three. **R23-API1 closure landed at this
   site** (variant + wire-code + migration); subsequent variant
   additions follow the same shape.
