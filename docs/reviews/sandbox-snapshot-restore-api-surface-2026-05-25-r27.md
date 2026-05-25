# Sandbox/snapshot-restore — api-surface r27 review

Date: 2026-05-25 (UTC). HEAD at audit: `568c1357` (prior api-surface
review r26 at `01b6a744`). Read-only.

Worktree: `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.

Note: uncommitted worktree edits on `nomad_ch.rs`, `config.rs`,
`restore_handler.rs`, `lib.rs`, `docker.rs` (in-flight bundle agent)
are NOT audited — the brief explicitly excludes them.

Landed since r26 (filtered to api-surface impact):

- `069dd277` — **r27-S1 Guard A**: `validate_nomad_addr_loopback` in
  `config.rs:467` rejects non-loopback `nomad_addr` at boot. Boot-
  fatal (per R26-API4 recommendation). No `/readyz` extension —
  the recommendation held. **r27-S1 carry CLOSED**.
- `4f0f2259` — **r27-M1**: sanitize whitelist widened to
  `/var/lib/zeroship` + `/run/zeroship` + hyphenated UUIDs. No new
  mask token introduced. R26-API5 token-unification carry unaffected.
- `dfccd049` — **r27-M2**: char-boundary truncation in nomad_ch
  error message cap. Internal correctness; no wire shape change.
- `821cc9bd` — **r27-M2 LATENT closure**: bytes-as-char cast fixed in
  6 sanitize-strip sites. Internal; no wire shape.
- `3ec2762d` — **R26-API2 precursor**: `metrics_export::render()`
  pure function landed. Public Prometheus text-exposition v0.0.4
  emitter over 13 counters + 1 gauge. New production accessor
  `lost_leadership_snapshot_by_op() -> Vec<(&'static str, u64)>`
  in `metrics.rs:417` plus 4 read accessors resurrected
  (`takeover_unreachable_value`, `takeover_corrupt_value`,
  `sandbox_corrupt_id_value`, plus implicit consumers).
- `05224bd1` — **R26-API2 wire-up**: `GET /metrics` mounted at root
  (`main.rs:173-176`); handler `admin_handlers::metrics_endpoint`
  (`:2055-2064`) gated on `AdminRole::ReadOnly`; sets
  `Content-Type: text/plain; version=0.0.4` + `Cache-Control: no-store`.
  Auth posture: admin-bearer-gated, NOT unauthenticated. **R26-API2
  carry CLOSED** (with a posture-choice note in §10.0 below).
- `a26adadd` — **composite-r1 #1**: 12 stale `#[doc(hidden)]`
  annotations dropped from production read accessors in `metrics.rs`.
  **composite-r1 MINOR #1 CLOSED**.
- `d2abef0b` — **composite-r1 #2**: `pub mod metrics_export` →
  `pub(crate) mod metrics_export` in `lib.rs:25`. **composite-r1
  MINOR #2 CLOSED**.
- `de5a3eff` — **composite-r1 #3**: integration pin
  `metrics_503_when_no_admin_tokens_configured` lands in
  `tests/sandbox_admin_e2e.rs:1288`. **composite-r1 MINOR #3 CLOSED**.
- `826d3abd` — **composite-r1 #4**: `sandbox_corrupt_id_total` HELP
  text broadened in `metrics_export.rs:69`. **composite-r1 MINOR
  #4 CLOSED**.
- `df06d172` — **R27-I1**: telescoping `from_config` / `_with_persist`
  / `_full` cascade replaced with `BackendBuilder`. 12 call sites
  migrated. Old constructors deleted in same PR (per "no
  back-compat"). **R25-API2 / R26-API3 carry CLOSED**.
- `ca8d960a` — **T5**: `WakeErrorCode::AgentVersionMismatch` lands
  as 10th variant. Triangle (`as_str` / `from_str_opt` / `wire_code`)
  intact at `db.rs:1680-1792`; migration 0014 extends
  `wake_jobs_error_code_check` to 10 values; `LATEST_MIGRATION_VERSION
  13 → 14`. `r16_api1_failed_state_renders_every_wake_error_code`
  pin extended at `admin_handlers.rs:2495-2541`. **T5 carry CLOSED**.
- `425a5522` — **R4-S2**: `ErrorEnvelope::with_extra` internal
  storage flipped `Option<Value>` → `Option<Map<String, Value>>`.
  Panics on non-object input (`error_envelope.rs:82-93`). Chained
  calls now MERGE. 5 new panic-pin tests + 1 merge pin at
  `:238-286`. **R4-S2 carry CLOSED**.
- `6e928a25` — **Option C Phase 2**: `zsbx_stage_disks` Nomad meta
  emission gated by `cfg.driver_stages_disk_images` flag (added
  by `2226de7a`); cold-boot branch only. New `pub bool` field on
  `SandboxConfig` (`config.rs:217`); no wire surface change
  (Nomad-internal meta, not in §10.0 envelope).
- `73725aa3` — **R26-I2 / R25-I1**: `spawn_blocking` wrapping of
  sync IO `try_create` body. Internal; no wire surface change.
- `ee702d5f` — **R26-C1 / R25-C1 / R23-P1 / R11-P1**: thread-local
  `Rc<Pool>` in `db.rs`. Internal pool wiring; no public API
  shape change.
- `035c3564` + `ce218860` — T5 wake-machine `/version` check +
  test pin. Internal flow + test; producer side of
  `AgentVersionMismatch`.
- `8c0b361e` — **r7-C-followup**: `Pool::start_housekeeper` after
  install. Internal driver wiring; no api surface impact.
- Documentation-only commits (`56e420ac`, `56c983b8`, `65b179ce`,
  `7624d440`, `d89fa250`, `bbadbe68`, `8f512153`, `7338a0aa`,
  `329cad05`, `3a8dce02`, `ebd369f2`, `ff7b878c`, `bb178538`,
  `568c1357`) — no source-of-truth impact.
- Pilot artifacts (`be8beb71`, `8def39e2`, `f7496ebe`, `054bd9ef`,
  `0b8cf6c2`, `e0aec919`) — review docs; no source impact.
- Cluster review artifacts (`cf6300bc`, `885e2abb`, `a948a38b`,
  `2f75e2f8`) + scripts bumps (`1e8fa7e8`, `e3a95a28`, `231e66c6`,
  `e3291b62`) — per brief, NO findings proposed on scripts/*
  this round.
- `871752c7` — pg-gated R26-C1 thread-local Rc<Pool> cache predicate
  tests. Internal test artifact; no api surface impact.

## Summary

- **R26-API1 (driver-side `nomad_driver_ch_destroy_task_unreaped_total`)**
  — UNRESOLVED. The brief notes the controller still lacks an
  operator-readable surface for this driver-side counter; the
  Phase-3 controller `/metrics` exporter that R26-API2 just landed
  covers ONLY the in-process atomics in `crate::metrics`, NOT the
  driver-side Nomad-plugin counters. Per architecture r28's
  staging-locality ADR (`docs/decisions/2026-05-24-staging-locality.md`,
  per `7624d440`), the federation question is documented but not
  closed. **Carry held** at IMPORTANT; cross-process metric hand-off
  still gated on architecture-side ADR landing.
- **R26-API2 (controller `/metrics` endpoint)** — **CLOSED at
  `05224bd1`**. Hand-rolled Prometheus text-exposition v0.0.4
  emitter in `metrics_export.rs:56-189`; route mounted at root
  (`main.rs:174`); admin-bearer gated. The auth-posture choice
  (gated, NOT unauthenticated) deviates from r26's recommendation
  ("unauthenticated, same posture as `/livez` / `/readyz`") — the
  commit message rationale at `05224bd1` weighs symmetry-with-
  `/admin/*` against the Prometheus open-by-default convention and
  picks symmetry. **Verify at R27-API-VERIFY1** below — judgment
  on the posture choice.
- **R26-API3 / R25-API2 (3-constructor cascade)** — **CLOSED at
  `df06d172`**. BackendBuilder lands as the consolidation; the
  positional 3-arg variant was rejected in favor of the
  builder-struct shape that r26 had explicitly ruled OUT
  ("prefer the POSITIONAL 3-arg variant…11 1-line edits is simpler
  than introducing a new public struct"). **The shipped design
  diverges from r26's recommendation**. The commit message
  rationale at `df06d172` weighs the 4th-orthogonal-field cliff
  (r27-A1 staging-locality ADR) against the cost of a new public
  struct and picks the builder. Audit at **R27-API1** below —
  judgment on the shape choice.
- **R26-API4 (Guard A `/readyz` exposure)** — **CLOSED**. r27-S1
  Guard A landed boot-fatal (per r26 recommendation) at
  `config.rs:467-543`; `/readyz` shape unchanged at
  `handlers.rs:132-141`. No follow-up. **Recommendation closure
  ratified**.
- **R26-API5 (sanitize mask token unification)** — **NO MOVEMENT**.
  Three shapes still in flight (`[redacted]`, `<redacted-path>`,
  `<redacted-typed-id>`). r27-M1 added two more whitelist entries
  + hyphenated UUID handling but didn't rename tokens. **Carry
  held** at MINOR for code-quality r28.
- **R24-API3 (sync/async wake-poll `extra` `which` asymmetry)** —
  **NO MOVEMENT**. Sync path (`admin_handlers.rs:1311-1314`) still
  embeds `extra: {which, sandbox_id}`; async-poll Failed branch
  (`:2001-2006`) still embeds `extra: {state, wake_id, sandbox_id,
  updated_at}` with no `which`. T5's `AgentVersionMismatch` landing
  did not touch this asymmetry. **Carry held** for architecture's
  Phase-2 `wake_jobs.error_extra JSONB` schema decision.
- **R20-API1 (schema-marker rewriter sites)** — **NO MOVEMENT**.
  No new commits touched the path-derivation rewriter sites this
  round. **Carry held** at IMPORTANT; quadruple motivation intact.
- **composite-r1 closures** — all 4 MINOR items closed within
  the round (`a26adadd`, `d2abef0b`, `de5a3eff`, `826d3abd`).
  **R27-API-VERIFY2** below ratifies the visibility shapes.
- **NEW R27-API1**: `BackendBuilder<'a>` ships as a `pub` struct
  with `pub` setters + `&'a SandboxConfig` lifetime — the shape
  r26 explicitly rejected. The wider API surface is motivated;
  the choice is defensible (and arch-r28 ratified the underlying
  4th-orthogonal-field cliff). Audit + ratification below. MINOR
  (closure ratification, not new finding).
- **NEW R27-API2**: `pub fn _test_inject_sandbox` (`nomad_ch.rs:1688`)
  is `pub` but functionally `#[cfg(test)]`-only. Author-acknowledged
  ("a `#[cfg(any(test, feature = "test-support"))]` gate would be
  cleaner"). Surface leak — IMPORTANT (NEW).
- **NEW R27-API3**: 12 of 14 `metrics::*_value` read accessors have
  ZERO external consumers but stay `pub`. With `metrics_export`
  now `pub(crate)` (per composite-r1 #2), these accessors are
  candidates for `pub(crate)` tightening. MINOR (NEW;
  cross-references composite-r1 #2's spirit).
- **NEW R27-API4**: `WakeErrorCode::AgentVersionMismatch` wire-code
  is `agent_version_mismatch` (identical to pg-column form). This
  is the FIRST variant where the two coincide; the other 9 either
  match (3 cases) or differ intentionally (6 cases). Documentation
  observation only — the design is defensible and the comment at
  `db.rs:1666-1672` calls it out. MINOR (NEW; cosmetic).
- **Backlog**: r26 = 12 → r27 = 11 (8 closures within round —
  R25-API2/R26-API3, R26-API2, R26-API4, composite-r1 #1-#4, R4-S2;
  3 new — R27-API1 ratification, R27-API2, R27-API3, R27-API4).

## CRITICAL

None.

## IMPORTANT

### [R27-API2] `pub fn _test_inject_sandbox` is unconditionally `pub` despite being test-only scaffolding

- **Where**: `crates/sandbox/src/backend/nomad_ch.rs:1688-1710`.
- **Snippet** (signature + author's own justification):
  ```rust
  // Marked `pub` rather than `pub(crate)` so integration tests
  // in `tests/` can call it; the `#[cfg(any(test, feature =
  // "test-support"))]` gate would be cleaner if we want to
  // strip it from production binaries — Phase 1 leaves it
  // unconditionally public with a "tests only" doc-comment
  // (the function name self-identifies as test scaffolding).
  pub fn _test_inject_sandbox(
      &self,
      sandbox_id: Uuid,
      user_id: &str,
      signing_key: SigningKey,
      agent_url: String,
      vm_index: u16,
  ) {
  ```
- **Why IMPORTANT**:
  1. **Test scaffolding ships in production binaries**. The
     function is compiled into every release `cargo build
     --release` of `zeroship-sandbox`. The signature accepts a
     raw `SigningKey` (the controller-side per-sandbox Ed25519
     SK) and an arbitrary `agent_url`. A reachable code path
     into `_test_inject_sandbox` from a network-facing handler
     would let an attacker inject an arbitrary sandbox into
     `NomadCHBackend::state` with a controller-trusted signing
     key and a controller-trusted agent URL.
  2. **No call paths in production**: an audit
     (`grep -rn "_test_inject_sandbox" crates/sandbox/src/`)
     returns zero references outside the function definition
     itself; the only callers are integration tests under
     `crates/sandbox/tests/`. So the production-binary surface
     IS dormant — but it's dormant code on the public API of
     the crate. A future commit that re-uses the helper from a
     handler module (because "it's already `pub`") promotes
     this from dormant to live.
  3. **The author already flagged the fix**. The inline comment
     at `:1682-1687` explicitly names the right shape:
     `#[cfg(any(test, feature = "test-support"))]`. The decision
     was deferred to "Phase 1" — that phase has since landed,
     this carry never closed.
  4. **R27 backlog audit cross-cuts**: the codebase already
     uses `#[cfg(test)] pub fn` at `nomad_ch.rs:400`
     (`freed_for_test`) — the precedent for the same shape
     this finding asks for. The asymmetry between `freed_for_test`
     (cfg-gated) and `_test_inject_sandbox` (not gated) is the
     anomaly to close.
- **Fix shape** (NOT prescribing; defers to code-quality r28):
  - **Option A** (minimal): gate `_test_inject_sandbox` with
    `#[cfg(any(test, feature = "test-support"))]`; add the
    feature to `Cargo.toml` with `default = []`; tests in
    `crates/sandbox/tests/` that consume it set the feature
    in their `[dev-dependencies]` entry. Pure subtractive change.
  - **Option B** (cleaner but more churn): move
    `_test_inject_sandbox` into a `pub mod test_support` that
    is itself `#[cfg(any(test, feature = "test-support"))]`.
    Groups present + future test-only helpers in one place.
  - Prefer Option A — minimum disclosure, mirrors
    `freed_for_test` exactly.
- **Severity**: **IMPORTANT** (test scaffolding on the public
  API surface; dormant attack vector if a future caller
  promotes it).
- **Owner**: code-quality r28.

### [R26-API1] (carry) driver-side `nomad_driver_ch_destroy_task_unreaped_total` still has no operator-readable surface

- **Where**: out-of-tree `nomad-driver-ch` repo emits the counter;
  no consumer-side mention in `crates/sandbox/src/`.
- **Status**: R26-API2 (`/metrics` exporter) landed in this round
  for the **controller-side** atomic counters — it does NOT
  federate driver-side counters. Per arch-r28 staging-locality
  ADR (`docs/decisions/2026-05-24-staging-locality.md`, landed
  via `bbadbe68`), the cross-process metric scrape topology is
  documented as SEPARATED (operators scrape both surfaces),
  matching r26's api-surface verdict — but the **operator-readable
  surface for the driver-side counter** still depends on Nomad's
  go-metrics fanout exposing plugin counters via the agent's
  `/v1/metrics`. Whether v18 driver emits via
  `MetricsCollector` is out of audit scope (out-of-tree).
- **Why still IMPORTANT**:
  1. The controller-side `/metrics` is the half R26-API2
     closes. The driver-side half is unchanged.
  2. The driver-side counter is the operator's measurement
     surface for the r4-A reap-wait validation. Without it,
     post-deploy validation falls back to e2e success rate
     (the same shape that delayed stress-r3/r4/r5/r6/r7/r8
     convergence).
- **Fix shape** (per r26): documentation hand-off; the
  controller side has shipped its half. No further controller-
  side action required; close the carry when the driver-side
  surface lands.
- **Severity**: IMPORTANT (carry from r26).
- **Owner**: out-of-tree nomad-driver-ch / observability ADR.

### [R20-API1] (carry) schema-marker rewriter sites unchanged

- **Where**: 4 path-derivation rewriter sites (per r19/r20/r25/r26
  carry) — no movement this round.
- **Brief asks**: "Still open?"
- **Confirmed: YES**. The Option C Phase 2 work (`6e928a25` +
  `2226de7a`) adds a `zsbx_stage_disks` meta emission gated on
  `cfg.driver_stages_disk_images` — this is a NEW emission, not
  a rewriter site, and is cold-boot-only per the comment at
  `config.rs:210-214`. Restore-path rewriters untouched.
- **Status**: 3-round carry + quadruple motivation; held.
- **Severity**: IMPORTANT (carry).
- **Owner**: security (driver-side validator landing).

## MINOR

### [R27-API1] R27-I1 BackendBuilder lands with `pub` struct + `&'a SandboxConfig` lifetime — diverges from r26's recommendation, ratifying the choice

- **Where**: `crates/sandbox/src/backend/mod.rs:200-258`.
- **Snippet** (full builder shape):
  ```rust
  #[must_use]
  #[allow(missing_debug_implementations)] // Persistence has no Debug; see sweep.rs convention
  pub struct BackendBuilder<'a> {
      cfg: &'a SandboxConfig,
      persist: Option<std::sync::Arc<crate::persist::Persistence>>,
      local_nomad_node_id: Option<String>,
  }

  impl<'a> BackendBuilder<'a> {
      pub fn with_persist(mut self, persist: Arc<Persistence>) -> Self { ... }
      pub fn with_local_nomad_node_id(mut self, node_id: String) -> Self { ... }
      pub fn build(self) -> Result<Backend, String> { ... }
  }

  impl Backend {
      pub fn builder(cfg: &SandboxConfig) -> BackendBuilder<'_> { ... }
  }
  ```
- **r26's recommendation** (`r26.md:362-369`): "prefer the
  POSITIONAL 3-arg variant (`from_config(cfg, persist, node_id)`)
  over the builder-struct variant — 11 1-line edits is simpler
  than introducing a new public struct, and the parameter count
  (3) is below the threshold where positional ergonomics break
  down."
- **What actually shipped** (`df06d172` commit message):
  > "Per arch r27-A1 staging-locality ADR + r26 VFIO / tap-leak
  > edges, a 4th orthogonal field is plausible-near-term and would
  > force a 4-level cascade."
- **Analysis** (audit the deviation, ratify or escalate):
  1. **r26's recommendation operated on the r25 motivation
     surface** (3 orthogonal fields). The arch-r27-A1 ADR landed
     mid-round (commit `bbadbe68` lands BEFORE `df06d172`); the
     ADR explicitly names "staging-locality" as a fourth-orthogonal
     field that the builder shape absorbs without reshaping the
     12 call sites. **The motivation surface shifted between
     r26's recommendation and r27-I1's landing**.
  2. **The builder's wider API surface is measurable**:
     - r25/r26 baseline: 3 `pub fn from_config*` constructors
       on `impl Backend`.
     - r27-I1 landing: 1 `pub fn Backend::builder` + 1 `pub
       struct BackendBuilder<'a>` + 3 `pub fn` on
       `impl BackendBuilder<'a>` (`with_persist`,
       `with_local_nomad_node_id`, `build`).
     - Net: 3 `pub fn` → 1 `pub fn` + 1 `pub struct` + 3
       `pub fn` = net +1 type, same fn count.
  3. **Lifetime annotation `&'a SandboxConfig`** does NOT leak
     awkwardly into callers — the builder is always constructed
     + consumed in the same expression at every call site
     (`lib.rs:705-714` is the production site;
     `tests/sandbox_pg_e2e.rs` follows the same pattern). The
     lifetime is implicit. r26's concern ("lifetime annotation
     on `&'a SandboxConfig` leaks awkwardly") was misframed —
     this is a build-then-consume pattern, not a held-reference
     pattern.
  4. **`#[must_use]` on the struct** (line 200) catches
     accidental dropped-builder bugs at compile time. Important
     for a pattern where a missing `.build()` is silent.
  5. **The 4th-field hypothesis**: the comment at `:188-199`
     names "VFIO-handoff or tap-leak edges" as plausible
     near-term extensions. arch-r28 ratified the staging-
     locality ADR. The hypothesis is documented; whether it
     materialises is out of scope.
- **Verdict on the deviation**: **DEFENSIBLE / NO ESCALATION**.
  The motivation surface (staging-locality ADR landed in the
  same round) shifted r26's cost-benefit math. The shipped
  builder shape is:
  - Self-documenting (callers name each optional field by
    method name).
  - `#[must_use]`-protected against dropped-builder bugs.
  - Extensible without call-site churn (the 4th-field rationale).
  - Same number of `pub fn` as before; +1 `pub struct`.
  The cost (one new public type) is paid; the benefit
  (extension cliff absorbed by setters, not by a 4-level cascade)
  is documented at arch-r28.
- **Recommended follow-up rustdoc strengthen**: the
  `Backend::builder` docstring at `:260-279` could MOVE the
  arch-r27-A1 staging-locality + 4th-orthogonal-field rationale
  from the commit message INTO the rustdoc, so future readers see
  why the builder shape was chosen over the positional consolidation
  without git archaeology. Currently the rustdoc says only
  "Replaces the prior 3-level telescoping constructor cascade
  (R27-I1)" — the WHY of the builder choice is implicit. Pure
  documentation strengthen; ~5 lines.
- **Severity**: MINOR (ratification of a defensible deviation;
  doc-strengthen recommendation forward to code-quality r28).
- **Owner**: code-quality r28 (rustdoc).

### [R27-API3] 12 of 14 `metrics::*_value` read accessors have zero external consumers; candidates for `pub(crate)` per composite-r1 #2 precedent

- **Where**: `crates/sandbox/src/metrics.rs` — 14 read accessors:
  ```
  metrics.rs:263  wake_sync_deprecated_value                      [external: 0]
  metrics.rs:296  vm_index_leak_value                             [external: 0]
  metrics.rs:316  wake_terminal_overwrite_blocked_value           [external: 1 site, sandbox_pg_e2e.rs ×3]
  metrics.rs:333  nomad_node_id_lookup_failures_value             [external: 0]
  metrics.rs:339  takeover_orphan_value                           [external: 0]
  metrics.rs:345  takeover_mismatched_value                       [external: 0]
  metrics.rs:354  takeover_unreachable_value                      [external: 0]
  metrics.rs:361  takeover_corrupt_value                          [external: 0]
  metrics.rs:368  sandbox_corrupt_id_value                        [external: 0]
  metrics.rs:385  takeover_lease_expiration_value                 [external: 0]
  metrics.rs:391  lost_leadership_value                           [external: 1 site, sandbox_pg_e2e.rs ×2]
  metrics.rs:399  lost_leadership_value_for_op                    [external: 0]
  metrics.rs:417  lost_leadership_snapshot_by_op                  [external: 0]
  metrics.rs:430  dead_hosts_observed_value                       [external: 0]
  metrics.rs:436  clock_rewind_value                              [external: 0]
  metrics.rs:443  heartbeat_lag_value                             [external: 0]
  ```
  External-consumer audit: `grep -rn "metrics::<accessor>"
  crates/sandbox/tests/` returns only `lost_leadership_value` and
  `wake_terminal_overwrite_blocked_value`. The other 12+2=14 (the
  `vm_index_leak_value` is parametric — counted once) have ZERO
  callers under `crates/sandbox/tests/`.
- **Composite-r1 #2 precedent** (closed in this round at `d2abef0b`):
  > "`metrics_export::render()` is consumed in exactly one place:
  > `admin_handlers::metrics_endpoint`, which lives in the same
  > crate… No integration test under `crates/sandbox/tests/` and no
  > external crate references `zeroship_sandbox::metrics_export::*`.
  > Exporting the module at `pub` was a defensive overshoot."
- **Apply the same lens to the 12 zero-external accessors**:
  - Sole production consumer: `crate::metrics_export::render`
    (now `pub(crate)`).
  - Sole test consumer: same-module `#[cfg(test)] mod tests`.
  - No external integration test.
  - Candidate for `pub fn` → `pub(crate) fn`.
- **Two-bucket split**:
  - **External-consumer bucket (keep `pub`)**: `lost_leadership_value`,
    `wake_terminal_overwrite_blocked_value`.
  - **Zero-external bucket (tighten to `pub(crate)`)**: the
    other 12.
- **Why MINOR not IMPORTANT**:
  1. Defensive overshoot is what composite-r1 #2 explicitly
     diagnoses. The same shape applies here at LEAST the same
     intensity (composite-r1 #2 had ONE consumer in same crate;
     these 12 accessors have ZERO external consumers).
  2. The producer side (`inc_*` functions) stays `pub` — those
     ARE called from across the crate's submodules and are the
     instrumentation surface. The narrowing is purely on the
     read side.
  3. The split is mechanical: a `pub(crate) fn x_value()`
     change costs 12 visibility-modifier edits and zero call-
     site touches (no test under `tests/` consumes these).
  4. **Why not IMPORTANT**: there's no security or correctness
     loss in keeping them `pub`. The MINOR is about API-surface
     tidiness — paying down the same overshoot composite-r1 #2
     just paid down on the module.
- **Recommendation**: tighten the 12 zero-external accessors to
  `pub(crate) fn` in one commit. Add a rustdoc note on the 2
  kept `pub` accessors documenting WHY they're broader (pg-gated
  integration test consumer) so future readers don't drift the
  symmetry.
- **Severity**: MINOR (api-surface tidiness; same lens
  composite-r1 #2 closed for the module declaration).
- **Owner**: code-quality r28.

### [R27-API4] `WakeErrorCode::AgentVersionMismatch` is the first variant where pg-column form and wire code coincide

- **Where**: `crates/sandbox/src/db.rs:1706` (pg-column `as_str`)
  + `:1790` (wire `wire_code`).
- **Snippet**:
  ```rust
  // pg-column form (as_str):
  Self::AgentVersionMismatch => "agent_version_mismatch",
  // wire form (wire_code):
  Self::AgentVersionMismatch => "agent_version_mismatch",
  ```
- **The triangle's 10 variants, pg vs. wire**:
  ```
  variant                  pg as_str                  wire_code
  ─────────────────────────────────────────────────────────────
  SlotUnavailable          slot_unavailable           vm_index_unavailable        [differs]
  SourceTeardownTimeout    source_teardown_timeout    source_teardown_timeout     [same]
  RestoreFailed            restore_failed             restore_backend_failed      [differs]
  LivezTimeout             livez_timeout              livez_timeout               [same]
  ClockResyncFailed        clock_resync_failed        clock_resync_failed         [same]
  RegisterFailed           register_failed            register_failed             [same]
  Internal                 internal                   internal_error              [differs]
  WakeWorkerAborted        wake_worker_aborted        wake_worker_aborted         [same]
  StagingPathMissing       staging_path_missing       staging_image_missing       [differs]
  AgentVersionMismatch     agent_version_mismatch     agent_version_mismatch      [same]
  ```
  Tally: 6 same / 4 differs.
- **Why the comment at `db.rs:1666-1672` notes "for this variant
  the internal-shape name and operator-facing name coincide"**:
  unlike `StagingPathMissing` (the previous T-series landing,
  which deliberately differs to use operator-facing
  `staging_image_missing`), `AgentVersionMismatch` describes the
  same phenomenon at both layers. No translation needed.
- **Why MINOR not nit**:
  1. The triangle's symmetry/asymmetry pattern carries semantic
     weight: differing names signal "internal shape != operator
     shape; consult the wire_code table"; matching names signal
     "no translation; pg column is the wire code". The 6:4 split
     is roughly balanced, so neither rule is the default.
  2. The new variant pushes the count to 6:4 same-vs-differs.
     Documentation review: the rustdoc on the triangle helpers
     (`db.rs:1726-1753` for `wire_code`'s map table) is
     hand-maintained — adding `AgentVersionMismatch` to that
     table would close the documentation drift. Currently the
     table at `:1731-1741` lists 8 variants; `StagingPathMissing`
     and `AgentVersionMismatch` are documented in prose
     below but not in the table.
- **Recommended doc-strengthen**: extend the markdown table at
  `db.rs:1731-1741` from 8 to 10 rows. Pure rustdoc edit.
- **Severity**: MINOR (cosmetic; documentation drift).
- **Owner**: code-quality r28.

### [R24-API3] (carry) async wake-poll envelope still missing structured `which` field

- **Where**: `admin_handlers.rs:1990-2008`
  (`render_wake_poll_response` Failed arm); unchanged this round.
- **Status carry**: r24 first flagged this; r25 / r26 / r27
  carry. T5's `AgentVersionMismatch` landing extended the wake-
  error-code triangle by one variant — the wake-poll Failed arm
  at `:1990-2007` correctly routes the new variant's `wire_code()`
  to `body["error"]`, but the EXTRA-fields asymmetry between
  sync POST (`:1311-1314`: `{which, sandbox_id}`) and async-poll
  GET (`:2001-2006`: `{state, wake_id, sandbox_id, updated_at}`)
  is unchanged.
- **Brief asks**: "T5 extension correctness". Answered:
  ✓ wire-CODE triangle correctly extended; the EXTRA-field
  asymmetry is orthogonal and remains held for Phase-2 schema
  decision.
- **Severity**: MINOR (carry; no escalation).
- **Owner**: architecture (Phase-2 `wake_jobs.error_extra JSONB`).

### [R26-API5] (carry) sanitize-widening mask token unification

- **Where**: `crates/sandbox/src/wake_machine.rs:748`
  (`REDACT_TOKEN`), `:1065` (`REDACT_PATH`), `:1119` (`REDACT_ID`).
- **Status**: r27-M1 (`4f0f2259`) extended the WHITELIST (added
  `/var/lib/zeroship` + `/run/zeroship` + hyphenated UUIDs)
  without changing the TOKEN format. r27-M2 LATENT closure
  (`821cc9bd`) fixed a bytes-as-char Latin-1 cast in 6 sanitize-
  strip sites — internal correctness, no token change.
- **Three shapes still in flight**: `[redacted]`, `<redacted-path>`,
  `<redacted-typed-id>`. r26's recommended unification (rename
  `REDACT_TOKEN` → split into `REDACT_URL` / `REDACT_IP` /
  `REDACT_HOST`, change literal `"[redacted]"` to
  `"<redacted-url>"` etc) is unimplemented.
- **Severity**: MINOR (carry; no movement; cosmetic).
- **Owner**: code-quality r28.

### Other carries (no movement)

- **R19-API2** — `pub` → `pub(crate)` sweep; this round adds
  R27-API3 to the same lens.
- **R22-API2** — controller-side readyz tests still synthesise
  response inline.
- **R22-API3** — `rootfs_source` doc asymmetry, comment-only.
- **R23-API2** / **R23-API3** — comment-only / forward-pressure.
- **R24-API2** — observation only.
- **R24-MIG1** — rolling-restart hazard documentation.
- **R24-SWEEP1** — sweep heartbeat visibility.
- **R25-API3** — `Option<String>` vs newtype on Nomad node_id.
- **R25-API4** — `Result<_, String>` typed-enum forward-pressure.
- **R25-API5** — doc-strengthen on parser.

## Considered + dismissed

- **R26-API2 chose admin-bearer-gated `/metrics` over r26's
  unauthenticated recommendation — should this escalate?**: r26
  recommended unauthenticated, same posture as `/livez` /
  `/readyz` (Prometheus convention + workspace's existing
  posture of network-policy gating over bearer-gating). The
  commit at `05224bd1` chose AdminRole::ReadOnly, citing
  symmetry-with-`/admin/*` and "counter values are operator-
  facing telemetry (fleet topology, takeover rate, leak rate)".
  - **Defensibility check**: the counter labels DO surface
    operationally-sensitive fleet topology — `op`-labelled
    `sandbox_ha_lost_leadership_total{op=...}` reveals every
    observed CAS-guarded op name; `vm_index_leaks_total{reason}`
    reveals when slot-leaks happen. These aren't secrets but
    they ARE attack-surface intelligence for a probe attacker.
    Admin-bearer gating raises the bar.
  - **Auth posture is operator-configurable downstream** — a
    deployment that wants Prometheus-convention (open + firewall)
    can run an unauthenticated relay sidecar. Hard-coding open
    in the controller forecloses the bearer-gated option.
  - **Verdict**: the deviation is defensible. **No nit**.
- **`pub mod metrics_export` was `pub` for 4+ rounds before
  composite-r1 #2 caught it — should api-surface r28 audit other
  modules for the same overshoot?**: composite-r1 #2 closed
  exactly one module declaration; the rest of the `pub mod x;`
  list in `lib.rs:11-39` is:
  - `admin_handlers` (HTTP handlers; consumed by `main.rs`
    and tests/) — `pub` warranted.
  - `auth` / `config` / `db` / `handlers` / `persist` / etc.
    — all consumed by integration tests. `pub` warranted.
  - `metrics` — consumed by 2 integration tests (per R27-API3
    audit). `pub` warranted, but read accessors inside it
    candidate for `pub(crate)`.
  No further module-level overshoots beyond what composite-r1
  closed. **No nit**.
- **`BackendBuilder` could `#[non_exhaustive]` the struct for
  future-proofing**: the struct is `pub` but its FIELDS are
  private; `#[non_exhaustive]` applies to fields/variants, not
  to methods. Since adding a new `.with_<field>()` setter is
  additive on the impl block (not a struct-literal break),
  `#[non_exhaustive]` is the wrong tool here. **No nit**.
- **The R4-S2 panic-on-non-object in `with_extra` is too harsh
  — should it silently coerce or return `Result`**: the
  commit-message rationale at `425a5522` enumerates the options
  (A=panic, B=warn-and-continue, C=`fn with_extra(Map)`). Option
  C was rejected for caller churn (every call site uses
  `json!({...})` which produces `Value::Object`); Option B was
  rejected because silent coercion is exactly what the bug was
  before the fix. Option A surfaces the bug at the first failing
  test. The 5 panic-pin tests at `error_envelope.rs:240-272`
  enforce the contract. **No nit**.
- **The R26-API2 `/metrics` exporter is hand-rolled (no Prometheus
  crate) — should this escalate to a dep-add?**: the renderer is
  ~80 LOC of pure-string concat in `metrics_export.rs:56-189`.
  The Prometheus client crate (`prometheus` on crates.io) is
  ~100KB compiled and pulls in `procfs` (Linux-only) + `protobuf`
  (for the v0.0.5 binary format). The hand-rolled version is
  spec-correct (text exposition v0.0.4 only; the 14 metrics
  + 1 gauge fit on one page of LOC). The dep-add tradeoff is
  unfavorable for the surface size. **No nit**.

## §10.0 envelope state post-r27

### Inventory (delta from r26)

```
New since r26:  WakeErrorCode::AgentVersionMismatch (wire code: agent_version_mismatch)
                GET /metrics route (new wire surface; non-§10.0 200 path)
```

The 32 §10.0 codes from r25/r26 extend by one — the 10th
`WakeErrorCode` family wire code is `agent_version_mismatch`.
The `/metrics` route's 200 path is `text/plain` (Prometheus
exposition), not JSON, so it sits OUTSIDE the §10.0 JSON-
envelope inventory — its error paths (401/403/503) DO use the
§10.0 envelope per the rustdoc at `admin_handlers.rs:2049-2051`.

### POST-endpoint envelope audit (delta from r26)

| Endpoint | Required | 401 | 403 | 503 | Success | Post-r27 wire-shape drift? |
|---|---|---|---|---|---|---|
| `POST /admin/sandboxes/{id}/snapshot` | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 200 | none |
| `POST /admin/sandboxes/{id}/wake` (sync) | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 200 (`agent_url`) | **+10th WakeErrorCode** |
| `POST /admin/sandboxes/{id}/wake` (async) | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 202 (`wake_id`) | **+10th WakeErrorCode** |
| `GET /admin/sandboxes/{id}/wake/{wake_id}` | RO | `unauthorized` | `insufficient_role` | `admin_api_disabled` | 200/202 | **+10th WakeErrorCode** |
| `POST /admin/sandboxes/{id}/cold-boot` | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | `feature_disabled` 501 | none |
| `DELETE /admin/users/{id}` | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 200 (delete tombstone) | none |
| `GET /readyz` | None | — | — | `backend_unhealthy` | `{"status":"ok"}` | none |
| **`GET /metrics`** | **RO** | `unauthorized` | `insufficient_role` | `admin_api_disabled` | **`text/plain; version=0.0.4`** | **NEW (R26-API2 closure)** |

The §10.0 envelope itself is unchanged; the `/metrics` row
fills the gap r26 flagged, with admin-bearer gating and a
text-plain success body.

## R27-API-VERIFY1 — R26-API2 closure verification pass

**Mandate**: audit the `/metrics` route's wire shape, auth
posture, and §10.0 envelope conformance on error paths.

**Audit checks** (all pass):

1. **Route mount** ✓ `main.rs:173-176` mounts
   `web::resource("/metrics")` at the root, NOT under `/admin/*`
   — Prometheus convention. The auth check inside the handler
   makes the mount-point choice cosmetic.
2. **Auth gate** ✓ `admin_handlers.rs:2056` calls
   `admin_check_required(&req, &state, AdminRole::ReadOnly)`.
   Accepts EITHER the Full or RO admin bearer. 503 when no
   admin tokens configured (covered by
   `tests/sandbox_admin_e2e.rs:1288`'s
   `metrics_503_when_no_admin_tokens_configured` integration
   pin).
3. **Content-Type header** ✓ `admin_handlers.rs:2061` sets
   `text/plain; version=0.0.4` — the Prometheus content-
   negotiation hint scrapers accept.
4. **Cache-Control header** ✓ `admin_handlers.rs:2062` sets
   `no-store` — intermediate caches can't stash a stale
   counter snapshot.
5. **Body shape** ✓ `metrics_export::render()` emits one
   `# HELP` + `# TYPE` per metric (per spec); counter blocks
   are alphabetized for stable diffs; labelled series sort
   their label values; body terminates with `\n` (per
   `metrics_export.rs:386-392` integration pin).
6. **Label escape** ✓ `metrics_export.rs:227-234` escapes
   `\`, `"`, `\n` per spec; integration pin at `:408-418`.
7. **NaN sentinel** ✓ `metrics_export.rs:257-263` renders NaN
   as the literal `NaN` (per spec); integration pin at
   `:398-402`.
8. **§10.0 envelope on error paths** ✓ 401 / 403 / 503 routes
   through `admin_check_required` which uses
   `ErrorEnvelope::new(...)` per `error_envelope.rs`.

**Verdict**: R26-API2 is STRUCTURALLY CLEAN. The auth posture
deviation from r26's recommendation is documented at the commit
message and dismissed at "Considered + dismissed" above; no
follow-up beyond the carry on R26-API1 (driver-side federation).

## R27-API-VERIFY2 — composite-r1 closure verification pass

**Mandate**: verify the 4 composite-r1 closures applied cleanly.

**Audit checks** (all pass):

1. **composite-r1 #1 (12 stale `#[doc(hidden)]` annotations
   dropped)** ✓ `grep -n "#\\[doc(hidden)\\]" metrics.rs`
   returns ONE match at `:351`, and that match is a comment
   reference NOT an attribute (`/// previously a test-only
   #[doc(hidden)] fn deleted at 370fdbba as`). Production-
   surface accessors carry no stale annotations. Rustdoc on
   each accessor now reads "Used by `metrics_export::render()`
   and by tests." (or equivalent) — proper credit to the
   consumer.
2. **composite-r1 #2 (`pub mod metrics_export` →
   `pub(crate)`)** ✓ `lib.rs:25` is `pub(crate) mod
   metrics_export;`. The module rustdoc at `:21-24` documents
   the visibility choice rationale inline.
3. **composite-r1 #3 (integration pin for
   `metrics_503_when_no_admin_tokens_configured`)** ✓
   `tests/sandbox_admin_e2e.rs:1288` integration test exists.
4. **composite-r1 #4 (`sandbox_corrupt_id_total` HELP text
   broadened)** ✓ `metrics_export.rs:69` reads "decode of a
   stored sandbox_id as a typed-id failed at any read site"
   — matches the producer-side semantic (all typed_id parse
   failures, not just restore).

**Verdict**: All 4 composite-r1 MINORs cleanly closed. The
visibility-shape narrowing is the minimum-disclosure path
(`pub(crate)` not `pub`); the doc-string updates accurately
reflect the production consumer.

## R27-API-VERIFY3 — R27-I1 BackendBuilder shape verification pass

**Mandate**: audit the BackendBuilder shape for `pub` discipline,
must_use, and lifetime annotation correctness.

**Audit checks** (all pass with one rustdoc-strengthen
recommendation captured at R27-API1):

1. **Struct visibility** ✓ `BackendBuilder<'a>` is `pub` —
   the builder is the entry point for backend construction
   from external test crates (e.g. `tests/sandbox_pg_e2e.rs`
   constructs backends via `Backend::builder(&cfg)`).
   Tightening to `pub(crate)` would break those tests.
2. **Field visibility** ✓ All 3 fields are private (no `pub`).
   External callers reach them only through `with_*` setters.
3. **`#[must_use]`** ✓ `backend/mod.rs:200`. Catches dropped-
   builder bugs at compile time.
4. **`#[allow(missing_debug_implementations)]`** ✓
   `backend/mod.rs:201`. `Persistence` has no `Debug` impl
   (sealed-records material); the comment cites `sweep.rs`
   precedent.
5. **Setter shape** ✓ Both `with_persist` and
   `with_local_nomad_node_id` take `T` not `Option<T>` —
   per builder convention, callers invoke setters only when
   they have a value.
6. **Lifetime annotation** ✓ `<'a>` on the struct;
   `BackendBuilder<'_>` on the `Backend::builder` return —
   the lifetime is implicit at every call site (builder is
   build-then-consume in one expression).
7. **Call-site migration** ✓ 12 sites migrated per commit
   message; spot-check at `lib.rs:705-714` (production
   AppState boot path) and
   `tests/sandbox_pg_e2e.rs` (3 sites) confirms the
   migration is mechanical: 1-3 lines per site.
8. **NomadCHBackend asymmetry** ✓ `BackendBuilder::
   with_local_nomad_node_id(String)` and
   `NomadCHBackend::with_local_nomad_node_id(Option<String>)`
   have different signatures — by design. The Backend-side
   builder is "call only when you have it"; the NomadCH-side
   setter is "replace any prior value" (the construction-time
   field is already `Option<String>`).

**Verdict**: R27-I1 is STRUCTURALLY CLEAN. The shape diverges
from r26's recommendation but is defensible per arch-r28
staging-locality ADR landing in the same round. Rustdoc
strengthen captured at R27-API1.

## Cross-lens consensus

### R27-API-CROSS-R28A1 — architecture r28 ratification

Architecture r28 landed the staging-locality ADR
(`docs/decisions/2026-05-24-staging-locality.md`, via
`bbadbe68`) — the same ADR the R27-I1 builder commit cites as
its 4th-orthogonal-field motivation. Cross-lens consistency:
architecture's recommendation pre-dates R27-I1's landing,
which means the builder shape's deviation from r26's
recommendation has architecture-side support. **No
escalation**.

### R27-API-CROSS-R27S1 — security r27 Guard A ratification

Security r27's r27-S1 Guard A landed boot-fatal at
`config.rs:467-543`. r26's R26-API4 recommendation ("keep
`/readyz` binary; Guard A boot-fatal") was followed exactly.
Cross-lens consensus: ratified. **No escalation**.

### R27-API-CROSS-R28D1 — composite-r1 closure ratification

Composite r1's 4 MINOR items closed in this round
(`a26adadd`, `d2abef0b`, `de5a3eff`, `826d3abd`). Verified
at R27-API-VERIFY2. The closures applied cleanly with
appropriate rustdoc updates. **No escalation**.

### Other cross-lens

- **Performance r26**: no api-surface intersect this round.
- **Test-coverage r28**: owns R22-API2 (readyz binding) +
  R25-API1's cold-boot-side sibling pin recommendation +
  the new `/metrics` route-level integration test landed
  at `tests/sandbox_admin_e2e.rs:1288`.
- **Concurrency r27**: thread-local `Rc<Pool>` (R26-C1 etc)
  is single-tasking IO scheduling; no api-surface impact.

## Lens hand-off

- **To architecture r29**:
  - r26-A1 (`BackendFailureDetail` carry) — still open.
  - R26-API1 (driver-side counter federation) — controller
    side closed via R26-API2; driver-side surface depends on
    out-of-tree nomad-driver-ch / Nomad agent metrics fanout.
    Documentation hand-off recommended in
    `docs/reference/sandbox-observability.md` (doesn't exist).
  - R24-API3 Phase-2 schema decision (`wake_jobs.error_extra
    JSONB`) still open.
- **To test-coverage r29**:
  - R22-API2 carry.
  - R25-API1 cold-boot-side sibling pin still open.
  - §10.0 envelope enumeration test (r23 carry).
  - `/metrics` route-level integration test LANDED at
    `tests/sandbox_admin_e2e.rs:1288`
    (`metrics_503_when_no_admin_tokens_configured`); extend
    coverage to 200/401/403 paths in r29.
- **To security r28**:
  - R20-API1 schema-marker carry (still 3-round + quadruply-
    motivated; r3-A doesn't touch this surface).
  - **NEW R27-API2**: `_test_inject_sandbox` cfg-gate
    recommendation — has security implications (test
    scaffolding on the public API surface).
- **To code-quality r28**:
  - **NEW R27-API1** rustdoc strengthen on `Backend::builder`
    (move 4th-orthogonal-field rationale from commit message
    into the rustdoc).
  - **NEW R27-API3** 12-of-14 `pub` → `pub(crate)` sweep on
    `metrics::*_value` accessors.
  - **NEW R27-API4** doc table extension at `db.rs:1731-1741`
    (add `StagingPathMissing` + `AgentVersionMismatch` rows).
  - R19-API2 `pub` → `pub(crate)` sweep — composite-r1 #2
    closed `metrics_export`; R27-API3 follow-up extends the
    same lens to `metrics::*_value`.
  - R22-API3 (`rootfs_source` doc asymmetry) carry.
  - R23-API2 / R23-API3 carries.
  - R25-API5 (parser doc-strengthen) carry; bundle with
    R26-API5.
  - **R26-API5 carry** sanitize mask token unification.
- **To concurrency r28**: no api-surface findings cross over
  this round.

## Backlog carry table

| ID | First round | Status r27 | Severity | Lens to own |
|---|---|---|---|---|
| R19-API2 | r19 | Open (carry; R27-API3 extends scope) | MINOR | code-quality |
| R20-API1 | r20 | Open (4-round carry; quadruple motivation; held) | IMPORTANT | security |
| R22-API2 | r22 | Open (carry) | MINOR | test-coverage |
| R22-API3 | r22 | Open (comment-only) | MINOR | code-quality |
| R23-API2 | r23 | Open (comment-only) | MINOR | code-quality |
| R23-API3 | r23 | Open (forward-pressure / rustdoc rule) | MINOR | code-quality |
| R24-API2 | r24 | Open (observation only) | MINOR | — |
| R24-API3 | r24 | Open (async/sync `extra` asymmetry; carry, no movement) | MINOR | architecture |
| R24-MIG1 | r24 | Open | MINOR | code-quality / docs |
| R24-SWEEP1 | r24 | Open | MINOR | code-quality |
| R25-API2 | r25 | **CLOSED at df06d172** (BackendBuilder shipped; deviation ratified at R27-API1) | — | — |
| R25-API3 | r25 | Open (observation) | MINOR | — |
| R25-API4 | r25 | Open | MINOR | code-quality |
| R25-API5 | r25 | Open (doc-strengthen) | MINOR | code-quality |
| R26-API1 | r26 | Open (driver-side half still has no operator surface) | IMPORTANT | architecture |
| R26-API2 | r26 | **CLOSED at 05224bd1** (`/metrics` shipped; verified R27-API-VERIFY1) | — | — |
| R26-API3 | r26 | **CLOSED at df06d172** (same closure as R25-API2; ratified R27-API1) | — | — |
| R26-API4 | r26 | **CLOSED** (Guard A boot-fatal landed `069dd277`; `/readyz` unchanged per recommendation) | — | — |
| R26-API5 | r26 | Open (carry; no movement; r27-M1 added whitelist not token rename) | MINOR | code-quality |
| composite-r1 #1 | r1 | **CLOSED at a26adadd** (12 stale `#[doc(hidden)]` dropped) | — | — |
| composite-r1 #2 | r1 | **CLOSED at d2abef0b** (`pub(crate) mod metrics_export`) | — | — |
| composite-r1 #3 | r1 | **CLOSED at de5a3eff** (`metrics_503` integration pin) | — | — |
| composite-r1 #4 | r1 | **CLOSED at 826d3abd** (HELP text broadened) | — | — |
| **R27-API1** | **r27** | NEW — R27-I1 BackendBuilder ratification + rustdoc-strengthen recommendation | MINOR | code-quality |
| **R27-API2** | **r27** | NEW — `pub fn _test_inject_sandbox` cfg-gate needed | **IMPORTANT** | security / code-quality |
| **R27-API3** | **r27** | NEW — 12 `metrics::*_value` accessors `pub` → `pub(crate)` candidates | MINOR | code-quality |
| **R27-API4** | **r27** | NEW — `AgentVersionMismatch` triangle doc-table drift | MINOR | code-quality |

Net: r26 open = 12 → r27 open = 11 (8 closures within round —
R25-API2/R26-API3, R26-API2, R26-API4, R4-S2, composite-r1
#1-#4; 4 new — R27-API1 ratification, R27-API2 IMPORTANT,
R27-API3 MINOR, R27-API4 MINOR). The R4-S2 closure was a
separate carry from error_envelope.rs (NOT in the prior
backlog table since r26 didn't track it explicitly); it
landed cleanly per `425a5522`.

## Trend

- **r17-r19**: §10.0 envelope discipline (RIPS pins, wire-code
  inventory) — settled.
- **r20-r22**: typed-error surface (WakeErrorCode triangle,
  RestoreHandlerError, sanitize_error_message) — landed +
  widened.
- **r23-r25**: pre-flight typed channels (StagingPathMissing,
  StagingPreflight), cross-emitter parity (r3-A node-affinity
  Constraints) — landed.
- **r26**: observability-API surface gap promoted to
  IMPORTANT — controller `/metrics` missing,
  driver-controller hand-off undefined.
- **r27 (this round)**: observability-API surface CLOSED on
  the controller side (R26-API2 landed `/metrics`). Backend
  construction API consolidated (R25-API2/R26-API3 landed
  BackendBuilder). 4 composite-r1 cleanups closed within round.
  The lens shifts from "what's missing" (r26's observation
  surface) back to "what's overshooting" (R27-API2 / R27-API3
  `pub` surfaces that overshoot their actual consumer set).
  **The defining api-surface theme of r27 is closure-density:
  8 of 12 r26 backlog items closed, primarily on the
  observability + construction lenses; 4 new findings, 3 of
  which are forward-pressure on minimum-disclosure (`pub` →
  `pub(crate)`) and 1 of which (R27-API2) is a security-
  adjacent test-scaffolding leak that was always-on but never
  audited before.**

The r27 backlog net-shrunk by 1 (12 → 11) despite 4 new
findings, because 8 closures landed within the round — a
high-conversion round driven by composite-r1 + R26-API2 +
R27-I1 all landing in tight sequence. The remaining
IMPORTANT items (R20-API1, R26-API1, R27-API2) are all
multi-round carries or new-finding hand-offs; no new
IMPORTANT bug-class emerged from the round's churn.
