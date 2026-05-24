# Sandbox/snapshot-restore — api-surface r26 review

Date: 2026-05-25 (UTC). HEAD at audit: `01b6a744` (prior api-surface
review r25 at `2ead52c2`). Read-only.

Worktree: `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.

Landed since r25:

- `7647cd4d` — `sanitize_error_message` widened with two new passes:
  `strip_filesystem_paths` (whitelist roots `/var/zeroship/`,
  `/opt/nomad/`, `/etc/zeroship/`) and `strip_typed_ids` (canonical
  `^[a-z]{3}_[A-Za-z0-9]{18,32}$`). Two new mask tokens introduced:
  `<redacted-path>` and `<redacted-typed-id>`. R22-S1 Mode A closure
  on the driver-side path-leak fork. 7 new sanitize_* pin tests +
  one idempotency test (483 → 488 sandbox-lib pass count).
- `cb2836a0` — R22-S1 deferred-backlog admin (closure entry).
- `d0d7abd6` — pilot round-30 reviewer artifacts (perf r25,
  api-surface r25, code-quality r26); no source-of-truth impact
  beyond the lens hand-offs.
- `6c475c30` — **R26-I1 precursor**: `SnapshotRowMeta` (struct +
  4 fields) and `read_snapshot_row` bumped from `pub(super)` /
  module-private to `pub(crate)`. Pure visibility hop; no callers
  change in this commit.
- `b5ec01a1` — **R26-I1 collapse**: `WakeSnapshotMeta` (the
  wake-machine local mirror) and its `read_snapshot_row` clone
  deleted. Wake-path now calls
  `crate::restore_handler::read_snapshot_row(...)` directly. Field-
  level divergence handled by the superset shape (`SnapshotRowMeta`
  retains `artifact_path` which wake-path ignores — 1 String per
  wake, negligible cost for the DRY win). Net diff: -56 / +21 lines
  in `wake_machine.rs`; workspace-wide -18 LOC after the precursor's
  +17-line rustdoc bump.
- `3dfa28d1` — deferred-backlog admin (R26-I1 entry closed).
- `01288c18` — T-8b-stress-r4 cluster review document (validation
  artifact; no code change).
- `01b6a744` — pilot round-31 reviewer artifacts (concurrency r26,
  security r27, test-coverage r27); no source-of-truth impact.
- `1e8fa7e8` — sandbox/scripts bump driver v15→v16 (T-8b-stress-r5
  r4-A reap-wait). **Per brief, NO findings proposed on scripts/*
  this round.**

## Summary

- **R26-I1 closure (`b5ec01a1`)**: structurally clean. One struct
  + one reader function across the sandbox crate (grep confirms);
  visibility shape is the minimum-disclosure path (`pub(crate)`
  not `pub`), the 4-field struct exposure is field-by-field and
  motivated. **Cross-emitter parity** intact — the r3-A
  `node_affinity_constraints_parity_between_cold_boot_and_restore_emitters`
  pin (`restore_handler.rs:4431-4510`) is unaffected by the
  R26-I1 collapse because Constraints emission is at the Job
  level, not at the snapshot-row reader level. **Verify at
  R26-API-VERIFY1** below.
- **r4-A driver counter `nomad_driver_ch_destroy_task_unreaped_total`**:
  the brief flags this as live in driver v16. **The metric name
  appears NOWHERE in the controller-side codebase** (grep
  `nomad_driver_ch\|destroy_task_unreaped` across
  `crates/sandbox/` returns 0 matches). The counter lives in the
  out-of-tree nomad-driver-ch repo and surfaces ONLY via the
  Nomad plugin's process-level state. Operators currently have
  no controller-side surface to scrape it — confirmed by
  T-8b-stress-r4 review (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-stress-r4.md:127-133`):
  > "None of these counters are exposed via the controller's
  > HTTP listeners in this deployment: ... driver runs as a
  > Nomad plugin (no standalone HTTP listener) ... The counters
  > exist in the source but lack an emission surface; this is
  > the same diagnostic gap stress-r3 flagged."
  This is the **r4-A observability hand-off gap**. Audit at
  **R26-API1** below — IMPORTANT (NEW).
- **Controller exposes no `/metrics` endpoint**: confirmed at
  stress-r4 review (`:127-133`). `crates/sandbox/src/main.rs`
  binds routes for `/health`, `/livez`, `/readyz`, `/sandboxes/*`,
  `/admin/*`, `/preview/*` — but **no `/metrics`**. The Phase-3
  TODO is documented in `metrics.rs:3` ("no Prometheus framework
  dependency yet (Phase 3 wires a `/metrics` exporter)") but the
  Phase-3 work has not landed. **27 counters in `metrics.rs`
  have read-side accessors (`*_value` for tests) but no wire
  surface for operators**. Audit at **R26-API2** below —
  IMPORTANT (NEW).
- **R24-API3 carry (async wake-poll `extra` missing `which`)**:
  the brief asks whether the typed-StagingPathMissing landing
  closed the asymmetry. **It did NOT** — confirmed at
  `admin_handlers.rs:1971-2019`. Sync POST path embeds `extra:
  {which, sandbox_id}` at `:1311-1314`; async GET wake-poll
  Failed branch embeds `extra: {state, wake_id, sandbox_id,
  updated_at}` at `:2001-2006`. The `which` field is recoverable
  from the message body (the `RestoreHandlerError::StagingPreflight`
  Display impl at `restore_handler.rs:113-122` formats it inline)
  but NOT structured. **Carry held**, no movement r25 → r26.
- **R20-API1 4th-round carry (schema-marker, 4 rewriter sites)**:
  no movement this round. r3-A is a Job-level Constraints emission
  (no path-derivation rewriter touched). The driver-side validator
  is the natural enforcement point per multiple-round carry. **Carry
  held at 3-round + quadruple motivation; no further motivation
  accumulated this round either**. The brief's question — "still
  open?" — is answered yes; no new rewriter sites this round.
- **R25-API2 3-constructor pattern (`from_config` / `_with_persist`
  / `_full`)**: still as-shipped. Stress-r4 cluster review is
  RED (3/60 e2e), so the "consolidate post-GREEN" gate from r25
  is NOT yet open. The cluster cutover stays blocked on r4-A. **No
  change to the r25 recommendation**: hold the consolidation;
  re-evaluate post-GREEN. Verify at **R26-API3** below — MINOR
  (carry, no movement).
- **r27-S1 (security r27 IMP) `nomad_addr` loopback enforcement**:
  the security lens promoted r26-S1 LATENT → r27-S1 IMPORTANT after
  the r3-A Constraints emit landed without either Guard A (loopback
  enforcement in `NomadCHConfig::validate`) or Guard B (per-node
  `Node.HTTPAddr` cross-check). The api-surface question — should
  the guard's status surface via `/readyz` so operators can verify
  it fires? — analysed at **R26-API4** below. MINOR (forward-press
  on the `/readyz` body shape; recommendation gated on r27-S1
  Guard A landing).
- **R22-S1 sanitize widening (`7647cd4d`)**: three distinct mask
  tokens now in flight: `[redacted]` (URLs / IPs / hosts —
  `wake_machine.rs:748`), `<redacted-path>` (filesystem paths —
  `:1065`), `<redacted-typed-id>` (typed-IDs — `:1119`). The
  inconsistency is operator-facing: a single sanitized message
  can contain all three shapes. Should they unify under a common
  form? Audit at **R26-API5** below — MINOR (cosmetic but
  operator-visible).
- **Backlog**: r25 = 10 → r26 = 12 (no closures, two new IMPORTANT
  — both observability surface gaps).

## CRITICAL

None.

## IMPORTANT

### [R26-API1] r4-A driver counter `nomad_driver_ch_destroy_task_unreaped_total` has no operator-readable surface (cross-process metric hand-off gap)

- **Where**: out-of-tree `nomad-driver-ch` repo emits the counter
  per the brief; **no consumer-side mention** in
  `crates/sandbox/src/` or `docs/`. Grep verification:
  ```
  $ grep -rn "nomad_driver_ch_destroy_task_unreaped_total\|destroy_task_unreaped" \
      crates/sandbox/ docs/ 2>/dev/null
  (no matches)
  ```
- **Brief's framing**: "How does it surface to operators? Driver
  `/metrics` endpoint vs controller scraping? Hand-off mechanism
  between driver and controller is a recurring observability gap."
- **Current shape**:
  - The driver runs as a Nomad plugin (per
    `docs/runbooks/sandbox-nomad-ch.md` and `gcp-worker-startup.sh`).
    Nomad plugins do not expose their own HTTP listeners — the
    Nomad agent itself proxies plugin telemetry through the
    agent's `/v1/metrics` endpoint (Nomad's go-metrics fanout),
    NOT through a per-plugin `/metrics`.
  - The controller (`crates/sandbox/`) has **no Nomad-metrics
    scraper**. There is no code at any `grep -rn "nomad.*metrics\|
    GET /v1/metrics" crates/sandbox/`. The controller knows the
    Nomad agent's address (`cfg.nomad_ch.nomad_addr`) but doesn't
    poll metrics from it.
  - Even if the controller scraped Nomad's `/v1/metrics`, the
    `nomad_driver_ch_destroy_task_unreaped_total` counter would
    only surface if the Nomad agent exposes plugin-namespaced
    counters in its go-metrics fanout (per Nomad's plugin
    contract this requires the plugin to emit via the
    `MetricsCollector` interface; whether v16 does this is an
    open question — out of audit scope per "read-only").
- **Why IMPORTANT**:
  1. **Stress-r4 RED gate is a `destroy_task_unreaped`-class
     bug**. The cluster review (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-stress-r4.md:151-165`)
     pinpoints "CH rootfs.img ExclusiveWrite lock retained
     across stop→wake" — the wedge that r4-A's reap-wait targets.
     Operators need to MEASURE the reap-wait's effect post-v16
     to validate the fix. Without an operator-readable surface,
     post-deploy validation falls back to "did the e2e success
     rate move?" (the same shape that prevented r1-r4 from
     converging quickly).
  2. **The hand-off gap is systemic**: r4-A is the FOURTH
     cross-process counter in this lineage with no operator
     surface. Stress-r4 enumerated the same gap for
     `vm_index_leak`, `terminal_overwrite_blocked`,
     `takeover_claims`, `taps_orphaned_total`,
     `nomad_node_id_lookup_failures_total` (cluster-r4 review
     `:127`). The r4-A counter compounds this.
  3. **Wire-surface api-surface impact**: when (if) a controller-
     side `/metrics` endpoint lands (see R26-API2), should it
     forward driver-side counters? Two shapes are possible:
     - **Federated**: controller's `/metrics` proxies Nomad's
       `/v1/metrics` (or scrapes-and-relays) so a single
       operator scrape gets controller + driver + Nomad-self
       counters. Wire-shape risk: namespacing conflicts (Nomad's
       `nomad_*` vs sandbox's `sandbox_*` — already separated by
       prefix; collision unlikely).
     - **Separated**: controller's `/metrics` exposes only the
       in-process counters (`sandbox_*`); operators scrape the
       Nomad agent directly for `nomad_driver_ch_*`.
     The api-surface tradeoff is single-scrape-target convenience
     vs. cross-trust-boundary leakage (the controller would be
     forwarding metrics from a process it doesn't own — if Nomad
     ever exposes operationally-sensitive labels there, the
     controller becomes a leak conduit). **Api-surface
     recommendation**: SEPARATED. Each crate owns its own
     `/metrics` surface; operators scrape both.
- **Fix shape (NOT prescribing; defers to architecture r27)**:
  1. **Short term** — `crates/sandbox/scripts/snapshot_stress.py`
     (the e2e harness) directly scrape the Nomad agent's
     `/v1/metrics` endpoint between cycles, log the counter
     deltas, and write them to the stress-summary JSON. This
     gives operators MEASUREMENT during smoke/stress without
     waiting on the controller-side `/metrics` exporter.
     ~30 LOC in Python; no Rust-side change.
  2. **Medium term** — when R26-API2's controller `/metrics`
     endpoint lands, document the federation choice in
     `docs/reference/sandbox-observability.md` (does not yet
     exist; cross-cutting with R26-API2).
- **Severity**: **IMPORTANT** (operator can't validate r4-A's
  effect post-deploy; same gap that delayed stress-r3 / stress-r4
  diagnosis convergence).
- **Owner**: stress harness for the short-term scrape;
  architecture r27 for the federation ADR.

### [R26-API2] Controller exposes no `/metrics` endpoint — 27 counters in `metrics.rs` have no operator wire surface

- **Where**:
  - `crates/sandbox/src/main.rs:141-275` — enumerated route
    table. Routes: `/health`, `/livez`, `/readyz`, `/sandboxes/*`,
    `/admin/*`, `/preview/*`. **No `/metrics`**.
  - `crates/sandbox/src/metrics.rs:1-3` — "no Prometheus
    framework dependency yet (Phase 3 wires a `/metrics`
    exporter)". Phase 3 has not landed.
  - 27 counters/gauges in `metrics.rs` (enumerated via
    `grep -c "^static [A-Z_]*: " crates/sandbox/src/metrics.rs`
    = 27). Examples:
    - `sandbox_ha_takeover_total{reason}`
    - `sandbox_ha_lost_leadership_total{op}`
    - `sandbox_ha_heartbeat_lag_seconds`
    - `sandbox_vm_index_leaks_total{reason}`
    - `sandbox_terminal_overwrite_blocked_total`
    - `sandbox_taps_orphaned_total`
    - `sandbox_nomad_node_id_lookup_failures_total`
    - `sandbox_wake_sync_uses_total`
    - 19 more.
- **Snippet** (route table; main.rs):
  ```rust
  .service(web::resource("/health").route(web::get().to(...)))
  .service(web::resource("/livez").route(web::get().to(...)))
  .service(web::resource("/readyz").route(web::get().to(handlers::readyz)))
  .service(web::resource("/sandboxes")...)
  // ... no /metrics
  ```
- **Brief's framing**: "stress-r4 surfaced 'controller exposes no
  /metrics endpoint at 9091/9092' — confirmed at stress-r4 review.
  Important api-surface finding: operator-facing metrics endpoint
  missing or undocumented. What's the controller's metrics-export
  shape?"
- **Status**:
  - The counters are **incremented but unreadable** outside the
    process. Tests can read via `*_value` accessors
    (`metrics.rs:358` "Read-side (tests + future /metrics
    exporter)") — these accessors are tests-only today.
  - The architecture decision to ship counters BEFORE the
    exporter is sound (the call-site instrumentation has to
    land first; ripping it out later costs more). But the
    "Phase 3 wires the exporter" promise has carried for
    multiple rounds without landing.
- **Why IMPORTANT**:
  1. **Stress validation cannot use the counters**. Cluster-r4
     review explicitly: "Controller log grep across the
     60-cycle window found ZERO instances of any counter name.
     The counters exist in the source but lack an emission
     surface; this is the same diagnostic gap stress-r3
     flagged." The MEASUREMENT-driven gate on cutover (per
     `feedback_never_estimate.md` — "never produce ns/%/LOC/
     speedup estimates without a measurement") has no surface
     to measure FROM.
  2. **R26-API1 is downstream of this** — without a controller
     `/metrics` surface, the driver-side counter hand-off
     question is moot. Operators have nowhere to scrape FROM.
  3. **Alert / dashboard work cannot start**. SRE patterns
     (alerting on `rate(sandbox_ha_takeover_total[5m])`,
     dashboarding `sandbox_vm_index_leaks_total{reason}` by
     reason) assume scrapable metrics. None can begin until
     `/metrics` exists.
  4. **Wire-format-immutability risk LOW once it lands**: the
     atomic-counter shape (`AtomicU64::load`) maps 1:1 to
     Prometheus text-exposition format; the exporter is purely
     additive. Counter NAMES already follow Prometheus
     convention (`sandbox_<noun>_<verb>_total`). Adding the
     route is unlikely to break call sites.
- **Fix shape (NOT prescribing; cross-cuts performance r25 and
  architecture r27)**:
  1. **Minimal exporter** — `GET /metrics` returns Prometheus
     text-exposition format hand-rolled from the 27 atomic
     reads. ~100 LOC; no new dep. The existing `*_value`
     test accessors are the read path. Idiomatic format:
     ```
     # HELP sandbox_ha_takeover_total ...
     # TYPE sandbox_ha_takeover_total counter
     sandbox_ha_takeover_total{reason="lease_expiration"} 0
     ```
  2. **Auth**: bearer-gated under `SANDBOX_ADMIN_TOKEN_PATH`
     (same as `/admin/*`) OR unauthenticated (kubelet-style;
     counter values are not secrets per the workspace's
     existing posture — `/readyz` is also unauthenticated per
     `handlers.rs:127-130`). **Api-surface recommendation**:
     unauthenticated, same posture as `/livez` / `/readyz` —
     Prometheus scrapers are typically network-policied
     rather than bearer-gated.
  3. **Documentation**: `docs/reference/sandbox-observability.md`
     (doesn't exist; cross-cuts R26-API1 federation question)
     names which counters exist + their semantic types.
- **Severity**: **IMPORTANT** (operator-facing
  observability blocker; cutover-validation depends on it).
- **Owner**: architecture r27 for the design (Prometheus crate
  dep vs. hand-rolled; auth posture; cross-process federation).
  Test-coverage r27 for the route-level integration test once
  it lands.

## MINOR

### [R26-API3] R25-API2 carry — 3-constructor pattern still in place; consolidation gate still closed

- **Where**: `crates/sandbox/src/backend/mod.rs:182-234`
  (unchanged from r25).
- **Status carry**: r25 introduced this MINOR; r25's verdict was
  "consolidate post-stress GREEN". Stress-r4 (latest) is RED
  (3/60 e2e per `T8b-stress-r4.md:155`), so the post-GREEN gate
  has NOT opened. **r4-A (driver v16 reap-wait) is in flight**
  per the brief's "stress-r5" context; consolidation cannot
  proceed pre-GREEN. **No movement r25 → r26**.
- **Verify the 13 call-sites are stable**:
  ```
  $ grep -rn "from_config(\|from_config_with_persist(\|from_config_full(" \
      crates/sandbox/ --include="*.rs"
  → 13 sites: 9 use from_config, 1 uses from_config_with_persist,
              1 uses from_config_full, 2 are the impl itself,
              1 is the AppState::from_config caller (different fn)
  ```
  Three of the 9 `from_config` callers are tests under
  `crates/sandbox/tests/*` (sandbox_admin_e2e, sandbox_pg_e2e ×3,
  sandbox_typed_id_e2e, sandbox_preview_e2e, sandbox_preview_ws_e2e);
  two are examples (lifecycle_e2e, stress_e2e). All these would
  need a 2-`None`-thread on consolidation — purely mechanical.
- **Brief asks**: "Should we consolidate to one constructor that
  accepts a builder/options struct now that r3-A has settled?"
- **Builder/options struct vs. positional 3-arg**: r25 sketched
  the positional 3-arg consolidation. A **builder-struct
  variant** would be:
  ```rust
  pub struct BackendConfig<'a> {
      pub cfg: &'a SandboxConfig,
      pub persist: Option<Arc<Persistence>>,
      pub local_nomad_node_id: Option<String>,
  }
  impl Backend {
      pub fn build(opts: BackendConfig) -> Result<Self, String>;
  }
  ```
  - **Pro**: each caller names the field they care about;
    new optional params don't break call sites (`..Default::default()`).
  - **Con**: 11+ call sites become 4-5 lines each (struct-literal
    formatting); the builder struct itself becomes a new public
    surface to maintain; lifetime annotation on `&'a SandboxConfig`
    leaks awkwardly.
  - **Workspace parallel**: `compio_postgres::ConnectOptions` uses
    a builder; `SandboxConfig::from_env` is a single function.
    There is no consistent workspace pattern.
- **Api-surface recommendation**: HOLD the consolidation behind
  the GREEN gate. When it opens, prefer the POSITIONAL 3-arg
  variant (`from_config(cfg, persist, node_id)`) over the
  builder-struct variant — 11 1-line edits is simpler than
  introducing a new public struct, and the parameter count
  (3) is below the threshold where positional ergonomics
  break down.
- **Severity**: MINOR (carry, no movement).
- **Owner**: code-quality (carry from r25's hand-off).

### [R26-API4] r27-S1 Guard A — should `nomad_addr` validation status surface via `/readyz`?

- **Where (where Guard A would live, NOT YET LANDED)**:
  `crates/sandbox/src/config.rs:425-472`
  (`NomadCHConfig::validate`). Currently validates URL scheme
  (`http://` / `https://`); the security-r27 fix proposes
  adding host-restriction (`127.0.0.1` / `::1` / `localhost` /
  unix-socket) to refuse boot on non-loopback `nomad_addr`.
- **Brief asks**: "controller will add `nomad_addr` loopback
  enforcement. Wire-shape impact: should `nomad_addr` validation
  be exposed via `/readyz` so operators can verify the guard
  fires?"
- **Current `/readyz` shape** (`handlers.rs:132-142`):
  ```rust
  pub async fn readyz(state: State) -> HttpResponse {
      if state.backend.is_healthy() {
          HttpResponse::Ok().json(&serde_json::json!({"status": "ok"}))
      } else {
          error_response(SERVICE_UNAVAILABLE, "backend_unhealthy",
              "backend probe failed; service not ready")
      }
  }
  ```
  Binary surface: 200 if backend probe succeeded, 503 otherwise.
  No structured detail; no boot-time validation status.
- **Two design choices**:
  1. **Guard-A failures are boot-fatal** (the security-r27
     proposal at `r27.md:237-240`: "refuses to boot when the
     URL host is NOT 127.0.0.1 / ::1 / localhost / a unix-socket
     path"). A controller that booted at all is, BY CONSTRUCTION,
     past Guard A. So `/readyz` does not need to report it —
     a misconfigured controller wouldn't be running to answer.
     → `/readyz` shape unchanged.
  2. **Guard-A failures are degraded-but-running** (alternative:
     boot succeeds, but the validation result is a `bool` on
     `AppState`, and Guard A's failure forces a permanently-degraded
     mode where the controller refuses to submit jobs). Under
     this design, `/readyz` could expose `{"status":"ok",
     "nomad_addr_loopback_enforced": true}` so operators verify
     remotely.
  - **Workspace pattern**: `config.rs:435-444` (the existing
    `vm_index_floor < 1` check) returns `Err` from `validate()`
    → boot fails. This is the BOOT-FATAL pattern. Per parity,
    Guard A should also be boot-fatal.
- **Recommended shape**: BOOT-FATAL (choice 1). `/readyz` shape
  unchanged. Operators verify the guard by:
  - Looking at the controller's startup log (Guard A would log
    `tracing::info!("nomad_addr loopback enforcement: {addr}")`
    on success; `tracing::error!` + exit on failure).
  - Attempting to start the controller with a non-loopback
    `SANDBOX_NOMAD_ADDR` and asserting it fails. Pin-test belongs
    in `config.rs` tests, not in `/readyz`.
- **API-surface rationale**: `/readyz` is a binary liveness
  signal. Stuffing it with structured "feature is on" data
  drifts toward `/status` (which doesn't exist and shouldn't —
  it's a footgun for accidentally exposing config). Boot-time
  validation that succeeded is information the OPERATOR
  established at deploy time, not a runtime probe.
- **Brief's question — should it surface?** **NO**. Boot-fatal
  is the right shape; `/readyz` stays binary.
- **Severity**: MINOR (design recommendation forward to r27-S1
  Guard A; no current code change).
- **Owner**: security r27 / r27-S1 implementation.

### [R26-API5] R22-S1 sanitize-widening introduces three distinct mask token formats — operator-facing inconsistency

- **Where**: `crates/sandbox/src/wake_machine.rs`:
  - `:748` — `const REDACT_TOKEN: &str = "[redacted]";`
    (URLs / IPs / hosts)
  - `:1065` — `const REDACT_PATH: &str = "<redacted-path>";`
    (filesystem paths)
  - `:1119` — `const REDACT_ID: &str = "<redacted-typed-id>";`
    (typed-IDs)
- **Snippet** (representative composition; one message can
  contain all three):
  ```
  Input:  "wake sbx_ABCDE...XYZ failed at /var/zeroship/ch/sbx_ABCDE.../home.img on 10.99.103.2"
  Output: "wake <redacted-typed-id> failed at <redacted-path> on [redacted]"
  ```
- **Brief asks**: "These differ from `[redacted]` (used for
  IPs/URLs). Operator-facing inconsistency? Should they be
  unified?"
- **Analysis**:
  - **Three distinct shapes**: `[brackets]` vs.
    `<angle-redacted-noun>` vs. `<angle-redacted-noun>`.
  - **Two of three name the category** (`-path`, `-typed-id`);
    one is unqualified (`[redacted]` is overloaded —
    URL/IP/host all collapse to it).
  - **Operator-readability tradeoff**: a category-named token
    tells the operator WHAT was redacted, which aids triage
    (a typed-ID mask in an error means the leak path was
    tenant-identity; a path mask means tenant-topology; an
    IP mask means cluster-internal addressing). Unifying ALL
    to `[redacted]` would lose this signal. Unifying ALL to
    `<redacted-category>` would gain it.
  - **Test fixture impact**: the existing sanitize_* tests
    (`wake_machine.rs:1267-1497`) pin the EXACT tokens
    (`assert_eq!(s, "connect failed to [redacted]");`,
    `assert_eq!(s, "disk[1] <redacted-path> does not exist");`).
    Unifying tokens would require updating ~30 test
    assertions — mechanical but invasive.
  - **No external consumer**: the redacted strings land in
    `wake_jobs.error_message` (pg column) → `error_response`
    body's `message` field. No machine-parsable consumer
    today; humans read them.
- **Three shapes are plausibly defensible**:
  1. **Unify to category-named** (preferred): `<redacted-url>`,
     `<redacted-ip>`, `<redacted-host>`, `<redacted-path>`,
     `<redacted-typed-id>`. ~30 LOC edits in tests + 1-line
     const changes. PRO: operator sees what was redacted.
     CON: more vocabulary to remember.
  2. **Unify to bracket-uncategorized**: `[redacted]`
     everywhere. PRO: simplest. CON: loses category signal.
  3. **Status quo**: three shapes. PRO: zero churn. CON:
     reads inconsistent; new readers wonder if the difference
     means something.
- **Brief asks "should they be unified?"** — **YES, to the
  category-named form**. Three shapes today, one shape
  (parameterised by category) tomorrow.
- **Why MINOR not IMPORTANT**: the inconsistency is cosmetic;
  no machine consumer parses the mask tokens. Operator
  confusion is the only downside. A follow-up commit
  (~30 test-edit + 3 const changes) closes it cleanly.
- **Recommendation**: rename `REDACT_TOKEN` →
  `REDACT_NETWORK` (or split into `REDACT_URL` / `REDACT_IP`
  / `REDACT_HOST`) and change literal `"[redacted]"` to
  `"<redacted-url>"` / `"<redacted-ip>"` / `"<redacted-host>"`.
  Update test fixtures. Pure rename.
- **Severity**: MINOR (cosmetic, operator-facing).
- **Owner**: code-quality r27 (carry; bundle with R25-API5
  doc-strengthen).

### [R24-API3] (carry) async wake-poll envelope still missing structured `which` field

- **Where**: `admin_handlers.rs:1971-2019`
  (`render_wake_poll_response`); unchanged this round.
- **Brief asks**: "did the typed-StagingPathMissing landing
  actually close this asymmetry? Confirm."
- **Confirmed: NO**. The async path (`render_wake_poll_response`
  at `:1990-2008`) embeds `extra: {state, wake_id, sandbox_id,
  updated_at}` on the Failed branch. The sync path
  (`admin_handlers.rs:1305-1316`) embeds `extra: {which,
  sandbox_id}` for the StagingPreflight arm. The `which` value
  IS recoverable from the message body via the
  `RestoreHandlerError::StagingPreflight` Display
  (`restore_handler.rs:113-122`) — text-parsable, not
  structured.
- **Status carry**: r24 first flagged this; r25 carried; r26
  carries. The typed `StagingPathMissing` arm DID land
  (R23-API1 → R25-S1 closure), but it closed the wire-CODE
  asymmetry, not the wire-EXTRA-field asymmetry. The
  Phase-2 schema change (`wake_jobs.error_extra JSONB`)
  carries forward.
- **Severity**: MINOR (carry, no escalation).
- **Owner**: architecture r27 / Phase-2 schema decision.

## Considered + dismissed

- **R26-I1's `pub(crate)` exposure of 4 SnapshotRowMeta fields
  widens API surface**: the field exposure is necessary (the
  wake-machine caller reads three of the four:
  `snap.sha256`, `snap.vm_index`, `snap.user_id` at
  `wake_machine.rs:312, :449, :458`; the fourth `artifact_path`
  goes through `do_restore_inner` to `submit_restore_job`
  which the wake path doesn't call BUT is on the struct so the
  cold-boot caller's contract holds). Marking the fields
  `pub(crate)` is the minimum-disclosure shape — a method-only
  accessor would force every read site to go through `get_*()`
  shims, which is over-engineered for in-crate consumption.
  **No nit**.
- **`SnapshotRowMeta` should be `#[non_exhaustive]`**: not
  applicable to `pub(crate)` types — `#[non_exhaustive]` is
  for downstream-crate field-addition resilience, and
  there are no downstream crates of `crate::restore_handler`.
  **No nit**.
- **The `artifact_path` cost (one extra String per wake read)
  could be elided via a `WakeSnapshotMeta` borrowed-view newtype**:
  the cost is a single `String` allocation per WAKE (not per
  request) — wake is itself a multi-second operation involving
  pg queries, file IO, and a Nomad submit. The String
  allocation is rounding error. The DRY win (one reader, one
  test) is the load-bearing argument; over-engineering a
  zero-copy variant defeats it. **No nit**.
- **r4-A driver-side `nomad_driver_ch_destroy_task_unreaped_total`
  could be merged into `sandbox_taps_orphaned_total`**: no —
  they measure DIFFERENT phenomena. `taps_orphaned` is a sweep-
  side observation post-hoc; `destroy_task_unreaped` is a
  driver-side count of incomplete DestroyTask cycles. They'll
  correlate at scale but aren't the same metric. Keep separate.
  **No nit**.
- **`render_wake_poll_response` async-path could add `which`
  via a typed-error walk (Option A from r24-API3)**: per
  cluster-r4 the controller v35 architecture choice is
  Phase-2 JSONB column. Handler-side variant walk is the
  fallback; it's not preferable. **Carry as-is**.

## §10.0 envelope state post-r26

### Inventory (delta from r25)

```
New since r25:  (none)
```

No new wire envelope kinds added this round. The 32 §10.0 codes
+ the 9 `WakeErrorCode` triangle entries are unchanged from r25.
`staging_image_missing` remains the 9th and most-recent
`WakeErrorCode` family wire code.

### POST-endpoint envelope audit (unchanged from r25)

| Endpoint | Required | 401 | 403 | 503 | Success | Post-r26 wire-shape drift? |
|---|---|---|---|---|---|---|
| `POST /admin/sandboxes/{id}/snapshot` | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 200 | none |
| `POST /admin/sandboxes/{id}/wake` (sync mode) | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 200 (with `agent_url`) | none |
| `POST /admin/sandboxes/{id}/wake` (async mode) | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 202 (`wake_id`) | none |
| `POST /admin/sandboxes/{id}/cold-boot` | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | `feature_disabled` 501 | none |
| `DELETE /admin/users/{id}` | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 200 (delete tombstone) | none |
| `GET /readyz` | None | — | — | `backend_unhealthy` | `{"status":"ok"}` | none |
| (`GET /metrics`) | — | — | — | — | — | **MISSING — R26-API2** |

The §10.0 envelope is unchanged. The `/metrics` row is the gap
R26-API2 flags.

## R26-API-VERIFY1 — R26-I1 closure verification pass

**Mandate (from brief)**: "R26-I1 DRY collapse landed. Audit
visibility shape. Cross-emitter parity contract test still
passing?"

**Audit checks** (all pass):

1. **Single definition of `SnapshotRowMeta`** ✓
   ```
   $ grep -rn "struct SnapshotRowMeta\|struct WakeSnapshotMeta" \
       crates/sandbox/src/
   crates/sandbox/src/restore_handler.rs:758:pub(crate) struct SnapshotRowMeta {
   ```
   One definition. `WakeSnapshotMeta` is fully deleted (its only
   residual is a comment block at `wake_machine.rs:722-733`
   explaining the deletion). Field exposure: 4 fields, all
   `pub(crate)`, all motivated (3 read by wake-path, 1 retained
   for cold-boot caller contract).

2. **Single definition of `read_snapshot_row`** ✓
   ```
   $ grep -rn "fn read_snapshot_row\|fn.*read_snapshot_row" \
       crates/sandbox/src/
   crates/sandbox/src/restore_handler.rs:781:pub(crate) async fn read_snapshot_row(
   ```
   One definition. The wake-machine duplicate at
   `wake_machine.rs:712-769` (pre-collapse) is fully deleted.

3. **Visibility shape audit**:
   - Struct: `pub(crate)` — minimum-disclosure for cross-module
     access. NOT `pub` (would leak to downstream crates,
     unwarranted). NOT `pub(super)` (the prior shape; doesn't
     reach `wake_machine`). ✓
   - Fields: 4 × `pub(crate)`. NOT method accessors (would force
     `get_sha256()` / `get_vm_index()` shims; over-engineered
     for in-crate consumption). NOT public reads via a single
     `as_ref()` method (loses field-level type discrimination).
     ✓
   - Function: `pub(crate)` — symmetric with struct. ✓

4. **Cross-emitter parity contract test still passing** ✓
   The R26-I1 collapse touched `wake_machine.rs:712-769`
   (deleted `WakeSnapshotMeta` + reader) and the swap site at
   `wake_machine.rs:266-285`. The cross-emitter parity test
   (`node_affinity_constraints_parity_between_cold_boot_and_restore_emitters`
   at `restore_handler.rs:4431-4510`) operates at the
   Job-Constraints level, NOT the snapshot-row reader level —
   it asserts byte-identical `Job.Constraints` arrays from
   `build_nomad_job_json_with` (cold-boot) vs.
   `build_restore_nomad_job_json` (restore). R26-I1 doesn't
   touch either of these emitters; the parity invariant is
   unaffected.
   - Sister pins also intact:
     `restore_jobspec_includes_node_affinity_when_node_id_set`
     (`:4361-4387`),
     `restore_jobspec_omits_node_affinity_when_node_id_absent`
     (`:4394-4417`).

5. **Error-shape preservation** ✓
   Pre-collapse wake-machine reader returned
   `Result<WakeSnapshotMeta, String>`; the call site at
   `wake_machine.rs:267` mapped `Err(e)` into
   `Phase::Failed { code: Internal, message: ... }`.
   Post-collapse the shared reader returns
   `Result<SnapshotRowMeta, RestoreHandlerError>`; the call site
   at `wake_machine.rs:272-284` maps the new
   `RestoreHandlerError::Display` into the same Phase::Failed
   shape with `format!("read_snapshot_row: {e}")`. The wire
   message changes from a bespoke string to the
   `RestoreHandlerError` Display impl — which is the same shape
   for the error variants the wake-path encounters
   (`Internal(msg)`, `NotFound(typed_id)`). Wire-readable code
   is `WakeErrorCode::Internal` in both pre- and post-collapse
   shapes; per §10.0 envelope, the wire CODE is unchanged.

6. **Test suite intact** ✓ Per the commit message, `cargo test
   -p zeroship-sandbox --lib` reports 488 passed / 0 failed /
   1 ignored — same as baseline. No new tests required (pure
   refactor).

**Verdict**: R26-I1 is STRUCTURALLY CLEAN. The collapse achieved
the DRY win without enlarging the API surface (visibility was
narrowed from "two separate readers in two modules" to "one
reader at `pub(crate)`"); the cross-emitter parity contract is
preserved by virtue of operating at a different layer. **No
follow-up api-surface findings on R26-I1 itself**.

## Cross-lens consensus

### R26-API-CROSS-R27S1 — security r27 r27-S1 wire-shape impact

Security r27 promoted r26-S1 LATENT → r27-S1 IMPORTANT after
the r3-A Constraints emit landed without Guard A
(`nomad_addr` loopback enforcement in `NomadCHConfig::validate`)
or Guard B (per-node `Node.HTTPAddr` cross-check).

**Wire-surface impact when/if r27-S1 Guard A lands**:

1. **Boot-fatal posture**: per R26-API4 above, Guard A failure
   is boot-fatal. No `/readyz` shape change.
2. **Operator-visible startup log**: Guard A should `tracing::info!`
   on success (`"nomad_addr loopback enforcement: {addr}"`),
   `tracing::error!` + process exit on failure. The success log
   is the operator's verification surface — alternative to a
   `/readyz` extension.
3. **Test posture**: pin-tests in `config.rs` should cover
   accept (loopback variants) + reject (public IPs, DNS names,
   bare hostnames). The `validate()` function is already
   unit-testable.
4. **No §10.0 drift**: Guard A is a config-validation rejection,
   not a runtime error envelope. The §10.0 surface is unaffected.

**Api-surface verdict on r27-S1 Guard A**: aligned with the
boot-fatal pattern (`vm_index_floor < 1` validation precedent).
No wire-shape additions recommended.

### R26-API-CROSS-R26I1 — code-quality r26 R26-I1 closure ratification

Code-quality r26 owned the R26-I1 closure; api-surface r26
ratifies the visibility shape (`pub(crate)` not `pub`; field
exposure motivated; collapse preserves error-message and
wire-code semantics). No api-surface escalation. **Code-quality
r26 → api-surface r26: AGREED, closure intact**.

### R26-API-CROSS-R26A1 — architecture r26's `BackendFailureDetail` (carry from r25)

Architecture r26's CRITICAL `BackendFailureDetail` trait + 4
impls has not landed this round; r25's cross-lens recommendation
(bundle the wire-extra additions with r24-API3 sync/async
asymmetry fix) carries forward.

### Other cross-lens

- **Performance r25**: no api-surface intersect this round.
- **Test-coverage r27**: owns R22-API2 (readyz binding) +
  R25-API1's cold-boot-side sibling pin recommendation
  (`cold_boot_jobspec_includes_node_affinity_when_node_id_set`).
- **Concurrency r26**: no api-surface findings cross over this
  round.

## Lens hand-off

- **To architecture r27**:
  - **NEW R26-API1**: driver-side counter
    `nomad_driver_ch_destroy_task_unreaped_total` has no
    operator-readable surface. Recommend a federation ADR
    naming SEPARATED scrape topology (controller `/metrics`
    + Nomad agent `/v1/metrics`, no proxying).
  - **NEW R26-API2**: controller `/metrics` endpoint missing.
    Recommend a Phase-3 design: hand-rolled Prometheus
    text-exposition format, unauthenticated, route added to
    `main.rs:141-275` route table.
  - r26-A1 (`BackendFailureDetail` carry from r25) — still
    open.
  - r26-A2 (node-affinity ADR `2026-05-25-node-affinity-placement.md`)
    — still open per stress-r4 review.
- **To test-coverage r27**:
  - R22-API2 carry — controller-side `readyz` tests still
    synthesise response inline.
  - R25-API1 cold-boot-side sibling pin still open.
  - §10.0 envelope enumeration test (r23 carry).
  - **NEW**: route-level integration test for `/metrics`
    once R26-API2 lands.
- **To security r27 / r27-S1**:
  - R26-API4 — `/readyz` shape recommendation: keep binary
    (Guard A failures are boot-fatal). No structured detail.
  - R20-API1 schema-marker carry (still 3-round + quadruply-
    motivated; r3-A doesn't touch this surface).
- **To code-quality r27**:
  - R19-API2 `pub → pub(crate)` sweep still open. R26-I1
    landed a model bump (`pub(super)` → `pub(crate)`, no
    `pub`); use it as a template.
  - R22-API3 (`rootfs_source` doc asymmetry) still open.
  - R23-API2 / R23-API3 carries.
  - R25-API2 (constructor consolidation) — held behind stress
    GREEN gate, still RED at stress-r4.
  - R25-API5 (parser doc-strengthen) — bundle with R26-API5.
  - **NEW R26-API5**: sanitize mask token unification
    (`[redacted]` / `<redacted-path>` / `<redacted-typed-id>`
    → `<redacted-url>` / `<redacted-ip>` / `<redacted-host>` /
    `<redacted-path>` / `<redacted-typed-id>`).
- **To concurrency r27**: no api-surface findings cross over
  this round. R26-I1's collapse is single-threaded reader code
  on both sides.

## Backlog carry table

| ID | First round | Status r26 | Severity | Lens to own |
|---|---|---|---|---|
| R19-API2 | r19 | Open (carry) | MINOR | code-quality |
| R20-API1 | r20 | Open (3-round carry; quadruple motivation; held) | IMPORTANT | security |
| R22-API2 | r22 | Open (carry) | MINOR | test-coverage |
| R22-API3 | r22 | Open (comment-only) | MINOR | code-quality |
| R23-API1 | r23 | CLOSED at r24 | — | — |
| R23-API2 | r23 | Open (comment-only) | MINOR | code-quality |
| R23-API3 | r23 | Open (forward-pressure / rustdoc rule) | MINOR | code-quality |
| R24-API2 | r24 | Open (observation only, no action) | MINOR | — |
| R24-API3 | r24 | Open (async/sync `extra` asymmetry; carry, no movement) | MINOR | architecture (Phase-2 schema decision) |
| R24-MIG1 | r24 | Open (rolling-restart hazard documentation) | MINOR | code-quality / docs |
| R24-SWEEP1 | r24 | Open (sweep heartbeat visibility) | MINOR | code-quality |
| R25-API2 | r25 | Open (3rd constructor; held behind stress GREEN) | MINOR | code-quality |
| R25-API3 | r25 | Open (`Option<String>` vs newtype on Nomad node_id) | MINOR | — (observation) |
| R25-API4 | r25 | Open (`Result<_, String>` on fetch/parse — typed-enum forward-pressure) | MINOR | code-quality |
| R25-API5 | r25 | Open (doc-strengthen recommendation on parser) | MINOR | code-quality |
| R26-I1 | r26 | **CLOSED at b5ec01a1** (DRY collapse landed; verified at R26-API-VERIFY1) | — | — |
| **R26-API1** | **r26** | **NEW** — driver-side counter has no operator surface (cross-process metric hand-off gap) | **IMPORTANT** | architecture |
| **R26-API2** | **r26** | **NEW** — controller exposes no `/metrics` endpoint (27 counters unreadable) | **IMPORTANT** | architecture |
| **R26-API3** | **r26** | Status carry — R25-API2 reformulated; held behind stress GREEN | MINOR | code-quality |
| **R26-API4** | **r26** | **NEW** — r27-S1 Guard A `/readyz` exposure recommendation (rejected; binary shape preserved) | MINOR | security (forward) |
| **R26-API5** | **r26** | **NEW** — sanitize-widening mask token unification recommendation | MINOR | code-quality |

Net: r25 open = 10 → r26 open = 12 (1 closure on R26-I1, two
new IMPORTANT — R26-API1 + R26-API2 — both observability
surface gaps, three new MINOR — R26-API4 is a forward-pressure
recommendation already closed at this lens, R26-API5 is
cosmetic). The trend has shifted from "wire shape conformance"
(r3-A's settled correctness) to "operator-readable observability
surface" (R26-API1 + R26-API2 — both flag the same gap).

## Trend

- **r17-r19**: §10.0 envelope discipline (RIPS pins, wire-code
  inventory) — settled.
- **r20-r22**: typed-error surface (WakeErrorCode triangle,
  RestoreHandlerError, sanitize_error_message) — landed +
  widened.
- **r23-r25**: pre-flight typed channels (StagingPathMissing,
  StagingPreflight), cross-emitter parity (r3-A node-affinity
  Constraints) — landed.
- **r26 (this round)**: observability hand-off surface
  (controller `/metrics`, driver→controller counter federation,
  cross-process metric scrape topology). The wire-shape
  discipline lessons accumulated over r17-r25 now need to
  extend INTO the observability surface — the same "one
  canonical shape, pinned, tested" pattern that closed §10.0
  drift needs to close the `/metrics` shape before alerts /
  dashboards / cutover-validation can begin. **The shift from
  wire-API surface to observability-API surface is the
  defining api-surface theme of r26**.

The r26 backlog grew by net +2 because two related
observability gaps were promoted from latent ("Phase 3 wires
this") to IMPORTANT (cutover validation cannot proceed without
them). Both are gateable on architecture r27's design pass.
