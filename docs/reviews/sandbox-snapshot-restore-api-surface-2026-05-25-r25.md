# Sandbox/snapshot-restore — api-surface r25 review

Date: 2026-05-25 (UTC). HEAD at audit: `2ead52c2` (prior api-surface
review r24 at `a482f00d`). Read-only.

Worktree: `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.

Landed since r24:

- `883df7fe` — `AppState::from_config` boot-time `/v1/agent/self`
  lookup; new `AppState.local_nomad_node_id: Option<String>` field;
  `Backend::from_config_full` (new 3-arg constructor preserves the
  source-compatibility of `from_config` / `from_config_with_persist`);
  new metric `inc_nomad_node_id_lookup_failure`. **r3-A precursor**.
- `9b623f44` — cold-boot emitter (`build_nomad_job_json_with`) emits
  top-level `Job.Constraints: [{LTarget, Operand, RTarget}]` when
  `local_nomad_node_id` is `Some`. **r3-A cold-boot half**.
- `34b52cf1` — `HOST_DIR_GC_GRACE_SECS` default 3600 → 600
  (sweep config tightening, R24-A1).
- `901dfbf2` — host_dir-GC eligibility-matrix unit tests
  (R25-T4 closure).
- `d258cd6b` — deferred backlog admin (R25-T4 + R24-A1 closures).
- `d71f1a8c` — restore-path emitter (`build_restore_nomad_job_json`)
  emits the SAME Job.Constraints block when `local_nomad_node_id`
  is set. **r3-A restore-path half** + new
  `RealRestoreBackend::with_local_nomad_node_id(...)` builder.
- `b562d3a1` — deferred backlog admin (r3-A entry closed).
- `add6d5ef` — pilot-round-29 reviewer artifacts (architecture r26,
  test-coverage r26, security r26); no source-of-truth impact.
- `2ead52c2` — sandbox/scripts bump (driver v14→v15, controller
  v34→v35, R20-S3 driver SHA256 verify); per brief, NO findings
  proposed on scripts/* this round.

## Summary

- **r3-A node-affinity is the load-bearing fix for the stress-r3 78%
  cross-node failure**. It lands across three commits (`883df7fe`,
  `9b623f44`, `d71f1a8c`) with a clean cold-boot/restore parity test
  (`node_affinity_constraints_parity_between_cold_boot_and_restore_emitters`
  at `restore_handler.rs:4414-4490`). The Constraints wire shape
  conforms to Nomad's JSON job-submit API. Audit details at
  **R25-API1** below. **No CRITICAL or IMPORTANT api-surface
  findings on the r3-A landing**.
- **`Backend::from_config_full` is the 3rd constructor**. Coexists
  with `from_config(cfg)` (1-arg) + `from_config_with_persist(cfg, persist)`
  (2-arg) by design — the 1-arg/2-arg forms thread `None` into the
  3-arg variant, keeping all 11 pre-existing call sites
  source-compatible. **Per AGENTS.md pre-launch no-back-compat
  directive**: source compat for in-crate test/example callers is NOT
  the "user back-compat" the directive targets — collapsing to one
  constructor with required `Option<String>` is a legal pre-launch
  follow-up, and tracked under **R25-API2** (MINOR / cleanup).
- **`AppState.local_nomad_node_id: Option<String>`** is `pub`-by-design
  on `AppState` (the field's doc-comment says "no security
  sensitivity — node-id is the local agent's self-reported
  identifier"). The typed-id discipline in the workspace (UUIDv7 +
  base62 + entity prefix, per AGENTS.md "typed_id everywhere") is
  for the `typed_id` family (`usr_…`, `app_…`, `ses_…`) — Nomad
  node IDs aren't typed_ids and the field name carries the qualifier
  explicitly. Newtype-vs-`String` analysed at **R25-API3** (MINOR
  / observation).
- **`fetch_local_nomad_node_id` / `parse_nomad_agent_self_node_id`
  return `Result<String, String>`** — same pattern as ~170 other
  `Result<_, String>` sites in `crates/sandbox/src/` (up from 166
  at r23). The typed-error trend (`WakeErrorCode`,
  `RestoreHandlerError`, `SubmitRestoreError`) hasn't reached the
  Nomad agent-self path; downside is that the call-site at
  `lib.rs:685-697` can't structurally branch on
  `NetworkError`/`ParseError`/`EmptyNodeId`. The caller demotes ALL
  failure shapes to one tracing.warn + one counter bump
  (`inc_nomad_node_id_lookup_failure`), so structural branching has
  no consumer today. Tracked under **R25-API4** (MINOR / forward-
  pressure on the larger Result<_, String> trend).
- **PascalCase/lowercase JSON tolerance** in
  `parse_nomad_agent_self_node_id` is documented and unit-tested
  (4 pin tests at `nomad_ch.rs:7195-7272`). The decoder accepts BOTH
  forms with lowercase preferred (Nomad 1.x live shape per the
  doc-comment). Wire-shape contract documented at **R25-API5**
  (MINOR / doc strengthening recommendation).
- **`inc_nomad_node_id_lookup_failure` metric name**
  (`sandbox_nomad_node_id_lookup_failures_total`) matches the
  existing pattern in `metrics.rs:138-162` (`sandbox_<noun>_<verb>_total`)
  — see `sandbox_wake_terminal_overwrite_blocked_total` (R22-I1),
  `sandbox_vm_index_leaks_total{reason}` (C-7-LT-2-PR2),
  `sandbox_wake_sync_uses_total` (C-7-LT-PR2). **No drift**.
- **R24-API3 carry**: async wake-poll `extra` STILL omits `which`.
  No movement this round (the typed `StagingPreflight` arm hasn't
  added structured async-path render). Carries to architecture r26
  for the Phase-2 schema decision.
- **R20-API1 carry**: schema-marker (`_zsbx_path_schema_version`).
  No new rewriter sites this round (the r3-A change is at the
  Job-object level, not the path-derivation level). **Carry held at
  3-round + quadruple-motivation; no further motivation accumulated**.
- **R22-API2 carry**: controller-side `readyz` test-binding gap.
  Test-coverage r26 owns the landing; no source-of-truth touches
  this round.
- **R22-API3 carry**: `rootfs_source` doc asymmetry. Code-quality
  r26 owns; no movement.
- **R19-API2 carry**: `pub → pub(crate)` sweep. New `pub`s this
  round (R25-API2-NEW below) include the cross-crate-test-reachable
  `Backend::from_config_full`, `RealRestoreBackend::with_local_nomad_node_id`
  (`pub(crate)`), `NomadCHBackend::with_local_nomad_node_id` (pub),
  `NomadCHBackend::local_nomad_node_id` (pub accessor),
  `AppState.local_nomad_node_id` (pub field), and
  `inc_nomad_node_id_lookup_failure` + `nomad_node_id_lookup_failures_value`
  in metrics. Net: r24 = 998 → **r25 = 1011** under the
  `grep -roE '\bpub\b'` methodology (Δ = +13). Three quarters of
  the new `pub`s are integration-test-reachable, justifying the
  visibility (see R25-API2).
- **R26-A1 / r26-A2 incoming from architecture r26**: r26-A1 is
  CRITICAL on the architecture lens (BackendFailureDetail trait +
  4 impls). The api-surface impact when/if it lands is sketched at
  **R25-API-CROSS-R26A1** below. r26-A2 (node-affinity-trade ADR)
  is api-surface-adjacent — the controller-as-twin shape is a
  topology decision, not a wire-surface change; api-surface-side
  recommendation at **R25-API-CROSS-R26A2**.
- **Backlog**: r24 = 6 → r25 = 6 (no closures, no new
  CRITICAL/IMPORTANT, four new MINOR all about the r3-A surface
  shape — see R25-API1 / R25-API2 / R25-API3 / R25-API4 / R25-API5;
  R25-API1 is the verify pass and not a backlog entry).

## CRITICAL

None.

## IMPORTANT

None new this round. Carries: R20-API1 (3-round / quadruple
motivation, held), R22-API2, R23-API1's effects intersect with
R26-A1 from architecture r26 (see cross-lens below) but this
lens has nothing CRITICAL or IMPORTANT to add.

### R25-API1-VERIFY — r3-A node-affinity Constraints wire-shape conformance pass

**Mandate**: confirm the new `Job.Constraints` block renders as a
valid Nomad jobspec-API JSON array with single-constraint shape
matching `${node.unique.id} = <node_id>` AND that both emitters
(cold-boot `build_nomad_job_json_with` + restore-path
`build_restore_nomad_job_json`) emit byte-identical Constraints
for the same node_id input.

**Audit checks** (all pass):

1. **Wire shape: top-level `Job.Constraints` array** ✓
   At `nomad_ch.rs:2570-2578` the cold-boot emitter sets
   `job["Constraints"]` to a JSON array containing exactly one
   constraint object. At `restore_handler.rs:2693-2701` the
   restore-path emitter does the same. The outer envelope
   (`serde_json::json!({ "Job": job })` at `nomad_ch.rs:2579` /
   `restore_handler.rs:2702`) wraps the job per Nomad's `POST
   /v1/jobs` body contract. **Nomad jobspec-API conformance**: the
   `Job.Constraints` field is a documented JSON-array shape in
   Nomad 1.x agent API (api/job.go::Job.Constraints
   `[]*Constraint`); constraint objects carry `LTarget` /
   `Operand` / `RTarget`. The emitted shape conforms.

2. **Constraint object field set: `{LTarget, Operand, RTarget}`** ✓
   At `nomad_ch.rs:2571-2577` and `restore_handler.rs:2694-2700`,
   the emitted object has exactly three fields:
   ```rust
   {
       "LTarget": "${node.unique.id}",
       "Operand": "=",
       "RTarget": node_id,
   }
   ```
   These are the three canonical Nomad Constraint fields (per
   `api/job.go::Constraint{LTarget, RTarget, Operand}`). Notably
   ABSENT (correctly): the optional `RegexpConstraint` /
   `VersionConstraint` / `SemverConstraint` discriminator fields,
   which Nomad infers from `Operand` (`=` → exact equality, no
   discriminator needed). **No extra fields**, no `Weight` (which
   would imply Affinity, not Constraint — see #4 below).

3. **`${node.unique.id}` interpolation form** ✓ Per Nomad's
   interpolation spec, `${node.unique.id}` is the per-client
   unique 36-char (or non-UUID) node ID — equal to the value
   `stats.client.node_id` returns from `/v1/agent/self`. This is
   the IDIOMATIC LTarget for per-node affinity in Nomad (vs.
   `node.id` which historically referred to the agent's
   self-reported short name; vs. `${node.datacenter}` which is
   coarser). The doc-comment at `nomad_ch.rs:2562-2564` correctly
   documents this. The verify chain — fetched id at
   `/v1/agent/self stats.client.node_id` → cached in
   `AppState.local_nomad_node_id` → emitted in `RTarget` → matched
   by Nomad against `${node.unique.id}` — is internally consistent.

4. **Fail-CLOSED choice (Constraints, not Affinity)** ✓ The
   commit emits `Job.Constraints`, not `Job.Affinities`. The
   semantic difference matters: a constraint with no matching
   client makes the alloc UNSCHEDULABLE (Nomad rejects with a
   placement-failure event); an affinity with no matching client
   would SOFTLY-prefer the target node but allow placement
   elsewhere. The cold-boot fix is for a hard correctness
   issue (the controller stages bytes locally → cross-node
   placement ENOENTs at the driver); a soft preference would
   re-open the bug under any scheduler tie-break. **Constraints
   is the correct shape**. The doc-comment at `nomad_ch.rs:2563-2566`
   names the fail-CLOSED choice ("Nomad rejects the alloc as
   unschedulable if no client matches, surfacing the
   misconfiguration loudly rather than silently scheduling
   elsewhere"). Pin-test asserts `Operand == "="` (not `"<="` /
   `regexp` / `version` — those would soften the match).

5. **Single-emission contract** ✓ Both emitters set
   `job["Constraints"]` to a 1-element array (lines
   `nomad_ch.rs:2571` `[{...}]` and `restore_handler.rs:2694`
   `[{...}]`). Pin-tests at
   `nomad_ch.rs:<not yet present at this site>` and
   `restore_handler.rs:4366` (`assert_eq!(arr.len(), 1, "r3-A
   emits exactly one constraint")`) lock the count. Adding a
   second constraint without updating these tests fails — small
   carry note: the cold-boot pin (the symmetric
   `restore_jobspec_includes_node_affinity_when_node_id_set`
   test at `restore_handler.rs:4344-4370`) covers BOTH
   emitters via the parity test at `:4414-4490`, so the
   single-emission contract has 2-of-2 pin coverage (the
   parity test fails if either emitter mutates the array shape
   away from the other).

6. **Cross-emitter parity invariant** ✓ At
   `restore_handler.rs:4414-4490`, the
   `node_affinity_constraints_parity_between_cold_boot_and_restore_emitters`
   pin asserts byte-identical `Job.Constraints` arrays from
   both `build_nomad_job_json_with` and `build_restore_nomad_job_json`
   given the SAME node_id input. A future drift (a constraint
   field rename on one path, an Operand change, a Weight
   addition) compile-passes but test-fails.

7. **Disabled-shape (None) fallback** ✓ Both emitters omit
   `Job.Constraints` entirely when `local_nomad_node_id` is
   `None` (lines `nomad_ch.rs:2570` `if let Some(node_id)` and
   `restore_handler.rs:2693`). The resulting jobspec carries no
   Constraints field at all (Nomad treats absent-Constraints as
   "no node restriction"); pre-r3-A random placement behaviour
   is preserved. Pin-test at `restore_handler.rs:4377-4400`
   asserts the field is absent (`v["Job"].get("Constraints").is_none()`).

**Verdict**: r3-A is structurally CONFORMANT to the Nomad jobspec
JSON API. The 7 mandated invariants hold under the as-shipped
code; future drift breaks at the three pin tests
(`restore_jobspec_includes_node_affinity_when_node_id_set`,
`restore_jobspec_omits_node_affinity_when_node_id_absent`,
`node_affinity_constraints_parity_between_cold_boot_and_restore_emitters`).

Outstanding asymmetry to flag (not a blocker): the COLD-BOOT
emitter at `nomad_ch.rs:2570-2578` has no dedicated pin test that
exercises the SHAPE in isolation — coverage is via the parity
test only, which compares it against the restore-side. If the
restore-side test moves, the cold-boot side becomes
contract-orphaned. **Recommendation**: add a sibling
`cold_boot_jobspec_includes_node_affinity_when_node_id_set` pin
mirroring `restore_jobspec_includes_node_affinity_when_node_id_set`
so the cold-boot shape is locked at-site. **Severity**: comment
only, test-coverage r26 owns landing.

## MINOR

### R25-API2 — `Backend::from_config_full` is a 3rd constructor; `from_config_full` consolidation is a legal pre-launch follow-up

- **Where**: `crates/sandbox/src/backend/mod.rs:188-234`.
- **Snippet**:
  ```rust
  pub fn from_config(cfg: &SandboxConfig) -> Result<Self, String> {
      Self::from_config_with_persist(cfg, None)
  }
  pub fn from_config_with_persist(
      cfg: &SandboxConfig,
      persist: Option<std::sync::Arc<crate::persist::Persistence>>,
  ) -> Result<Self, String> {
      Self::from_config_full(cfg, persist, None)
  }
  pub fn from_config_full(
      cfg: &SandboxConfig,
      persist: Option<std::sync::Arc<crate::persist::Persistence>>,
      local_nomad_node_id: Option<String>,
  ) -> Result<Self, String> { ... }
  ```
- **Shape**: each constructor delegates to the next one with
  defaults threaded — `from_config` calls `from_config_with_persist(cfg, None)`,
  which calls `from_config_full(cfg, persist, None)`. The chain
  preserves the 11 pre-existing call-site signatures (8 integration
  tests under `sandbox/tests/*`, 2 examples under
  `sandbox/examples/*`, 1 prior production callsite that the new
  `lib.rs:700` callsite supplanted).
- **Why minor**: this is the "Builder Λ Default" coexistence
  pattern. The judgement is sound under SOURCE-compat for the
  existing test/example callers. But per AGENTS.md pre-launch
  no-back-compat directive ("No migration shims, no detect-and-warn
  paths…If the new shape is right, the old one disappears in the
  same PR"), source-compat shims for **in-crate test/example
  callers** are NOT user-back-compat — the directive's targets are
  wire-format / SDK-contract / V8 RPC consumers. In-crate
  refactors at the cost of recompiling tests are legal pre-launch.
- **Trade-off**: collapsing to a single constructor
  ```rust
  pub fn from_config(
      cfg: &SandboxConfig,
      persist: Option<Arc<Persistence>>,
      local_nomad_node_id: Option<String>,
  ) -> Result<Self, String>;
  ```
  costs ~11 1-line edits across `sandbox/tests/*` and
  `sandbox/examples/*` (passing `None, None` at each site). The
  benefit is one constructor to maintain + one signature to read.
  The downside is purely ergonomic — tests that don't care about
  persist or node_id now thread two `None`s instead of zero.
- **Why this is the right call as-shipped**: r3-A landed in 3
  commits over <24h with stress-r3 RED 1/60; minimising the
  call-site diff de-risked the rollout. The consolidation belongs
  to a follow-up cleanup PR after stress GREEN, not to the r3-A
  landing itself.
- **Recommendation**: file a code-quality r26 backlog entry to
  collapse to one constructor in a follow-up cleanup PR, gated on
  stress GREEN. Note that this lens (api-surface) considers
  N>1 constructors a **lint-level** issue, not a wire-surface
  drift — none of the 3 constructors leak across the workspace
  crate boundary in shape (all return `Result<Backend, String>`).
- **Severity**: MINOR (cleanup recommendation; not blocking).

### R25-API3 — `local_nomad_node_id: Option<String>` shape vs. `NodeId(String)` newtype

- **Where**:
  - `crates/sandbox/src/lib.rs:225` (`AppState.local_nomad_node_id`)
  - `crates/sandbox/src/backend/nomad_ch.rs:202`
    (`NomadCHBackend.local_nomad_node_id`)
  - `crates/sandbox/src/restore_handler.rs:2071`
    (`RealRestoreBackend.local_nomad_node_id`)
  - `crates/sandbox/src/backend/mod.rs:218`
    (`from_config_full` parameter)
  - `crates/sandbox/src/backend/nomad_ch.rs:2337,2373` (emitter
    parameter as `Option<&str>`)
  - `crates/sandbox/src/restore_handler.rs:2499` (restore-emitter
    parameter as `Option<&str>`)
- **Snippet** (representative):
  ```rust
  pub local_nomad_node_id: Option<String>,
  ```
- **Shape decision**: a raw `String` rather than a typed wrapper
  like `pub struct NomadNodeId(String);` (with a parse-on-set
  constructor enforcing non-empty / shape constraints).
- **Why minor**: the workspace's typed-id discipline (per AGENTS.md
  "typed_id everywhere — UUIDv7 + base62 + entity prefix") covers
  IDENTIFIERS THE PLATFORM MINTS (`usr_…`, `app_…`, `ses_…`,
  `sbx_…`). Nomad's `${node.unique.id}` is an OPAQUE third-party
  identifier (Nomad's own UUID for the client agent, written into
  the agent's state.db file at first boot). The platform doesn't
  mint it; the platform RECEIVES it. Wrapping a foreign opaque
  identifier in a newtype has lower payoff than wrapping a
  platform-minted typed_id.
- **What a newtype WOULD give us**:
  - Compile-time prevention of crossing it with other strings (a
    function signature `fn x(node_id: NomadNodeId)` can't be
    called with a stray `String`).
  - One canonical parse-on-set site (the `From<String>` or
    `try_from` impl) where empty-string rejection lives — today
    the empty-string check is in `parse_nomad_agent_self_node_id`
    at `nomad_ch.rs:3192-3197`, but `with_local_nomad_node_id`
    doesn't recheck. A future caller bypassing the parser could
    inject an empty `Some("")` and the constraint emission would
    happily build `RTarget: ""` (which Nomad would reject at
    submit time, but the controller would attempt the submit).
  - One single place to add Debug-redaction (if the operator
    sensitivity ever rises — Nomad-version drift could in
    principle expose more sensitive info via the node_id field).
- **What a newtype would COST**:
  - One additional type to maintain, one `Display` impl, one
    `as_str()` accessor, and call-site edits at the 6 fields/
    parameters above + the existing test fixtures.
  - Marginal: the node_id is already at one validated boundary
    (the parser) and the workspace has 4+ other opaque strings
    in similar shape (`cfg.nomad_ch.datacenter`,
    `cfg.nomad_ch.nomad_addr`, etc.) that also aren't newtyped.
- **Why this is the right call as-shipped**: parity with the
  workspace's existing pattern for foreign-opaque-identifier
  fields. The empty-string concern is mitigated by the parser
  AND by Nomad's submit-time rejection. The
  `with_local_nomad_node_id(None)` shape is symmetric with the
  detection-failure path and doesn't need wrapping.
- **Recommendation**: keep as `Option<String>`. If a future
  finding raises the Nomad-version-drift concern (e.g., a Nomad
  2.x release returns a structured `client.identity` block that
  needs typed shape preservation), promote to a newtype then.
- **Severity**: MINOR (observation only; no action recommended).

### R25-API4 — `fetch_local_nomad_node_id` / `parse_nomad_agent_self_node_id` return `Result<String, String>` — no structural failure-mode branching

- **Where**:
  - `crates/sandbox/src/backend/nomad_ch.rs:3144-3157`
    (`fetch_local_nomad_node_id`)
  - `crates/sandbox/src/backend/nomad_ch.rs:3170-3199`
    (`parse_nomad_agent_self_node_id`)
- **Snippet**:
  ```rust
  pub(crate) async fn fetch_local_nomad_node_id(
      nomad_addr: &str,
  ) -> Result<String, String> { ... }

  pub(crate) fn parse_nomad_agent_self_node_id(body: &str) -> Result<String, String> { ... }
  ```
- **Distinct failure shapes** (all currently flattened to
  `Result<_, String>` with operator-readable text):
  1. **NetworkError** — `http_get_unsigned` returned `Err`
     (Nomad agent unreachable, DNS failure, TCP timeout).
     Surfaces as `format!("GET {url} → ...: {body}")` at line
     3150 OR the `?` operator propagates the underlying ureq
     error string.
  2. **NonSuccessStatus** — Nomad responded but not with 200.
     Surfaces as the explicit `Err(format!("GET {url} → status
     {}: {}", ...))` at lines 3149-3155.
  3. **ParseError** — `serde_json::from_str` failed.
     Surfaces as `Err("parse /v1/agent/self body: {e}"))` at
     `nomad_ch.rs:3172`.
  4. **MissingField** — JSON parsed but `stats.client.node_id`
     not present. Surfaces as `Err("missing
     stats.client.node_id in /v1/agent/self response …")` at
     `nomad_ch.rs:3187-3191`.
  5. **EmptyNodeId** — field present but empty string.
     Surfaces as `Err("stats.client.node_id present but empty
     in /v1/agent/self response")` at `nomad_ch.rs:3193-3197`.
- **Why minor**: the call-site at `lib.rs:685-697` demotes
  ALL 5 failure shapes to **one** tracing.warn + **one**
  counter bump (`inc_nomad_node_id_lookup_failure`) + `None`
  fallback. No caller branches on the variant; no metric is
  labelled by reason. A typed enum would deliver no consumer-
  visible value TODAY.
- **What a typed enum WOULD give us** (a la the
  `vm_index_leak{reason}` labelled-counter pattern in
  `metrics.rs:275-305`):
  ```rust
  pub(crate) enum NomadAgentSelfError {
      NetworkError(String),
      NonSuccessStatus { status: u16, body: String },
      ParseError(String),
      MissingField,
      EmptyNodeId,
  }
  ```
  - Operator-actionable distinction in the metric
    (`sandbox_nomad_node_id_lookup_failures_total{reason="network_error"}` vs
    `{reason="missing_field"}` — the former is a "Nomad agent
    is down" alert; the latter is a "Nomad agent is in
    server-mode-only, no client" misconfiguration).
  - Distinct WARN tracing (operator-actionable difference: the
    config message would differ).
- **What it would COST**: one new typed enum + Display impl + 5
  arms; call-site at `lib.rs:685` becomes a match. ~50 LOC.
  Marginal value because the operator-actionable distinction is
  exactly one bit ("nomad reachable but not in client mode" vs
  "everything else") — and that bit IS already captured by the
  `parse_nomad_agent_self_node_id_missing_client_block_errs`
  test's textual `err.contains("missing stats.client.node_id")`.
- **Trend impact**: sandbox `Result<_, String>` count r24 = 182
  → r25 = ~171 under my measurement (`grep -rEo
  'Result<[^,>]*,\s*String>' crates/sandbox/src`) — the count
  is sensitive to my regex and the r24 number used a slightly
  different methodology. The two new functions in this round
  contribute 2 to the count; the four pin tests contribute
  several more (test fixtures returning Result<_, String>).
- **Recommendation**: keep as `Result<_, String>` until ONE of:
  - (a) The operator alert chain wants a labelled metric (then
    promote to typed-enum + per-reason counter), OR
  - (b) `BackendFailureDetail` from r26-A1 lands and the
    workspace standard for typed-error-at-trait-boundary
    extends to free-standing helpers (then promote
    consistently).
  Neither trigger is hit today.
- **Severity**: MINOR (forward-pressure; carry to code-quality r26
  bundled with R19-API2 / R23-API3 typed-error sweep).

### R25-API5 — `parse_nomad_agent_self_node_id` PascalCase/lowercase JSON tolerance doc-strengthen recommendation

- **Where**: `crates/sandbox/src/backend/nomad_ch.rs:3160-3199`.
- **What's there today**: the function accepts BOTH key cases
  (`stats`/`client`/`node_id` AND `Stats`/`Client`/`NodeID`)
  via `.or_else(|| body.get("Stats"))` chains at lines
  3180-3185. The doc-comment at 3164-3169 explains:
  ```
  Field path: `stats.client.node_id`. Live Nomad 1.x agents emit
  lower-case Go-json-tag keys (`stats`/`client`/`node_id`); some
  older API references show PascalCase. Accept both for
  defensiveness — a server-mode-only agent has no `stats.client`
  block, which is the legitimate "no client node_id" shape and
  surfaces here as `Err("missing stats.client.node_id…")`.
  ```
- **What's well-documented**: the dual-shape acceptance, the
  rationale ("defensiveness against agent-version drift"), the
  server-mode-only behaviour.
- **What's under-documented**:
  - The Nomad VERSION RANGE the lowercase form was verified
    against. The doc says "Nomad 1.x live shape" and "Nomad's
    /v1/agent/self body shape (verified against Nomad 1.x)" at
    line 3173. Nomad 1.x is a 5-year range (1.0 through 1.10
    at time of writing). Pinning the verification version
    explicitly (e.g., "Verified against Nomad 1.7.2 client at
    [date]") would tell a future reader which behaviour drifted.
    This matters because the Nomad-API doc at
    `developer.hashicorp.com/nomad/api-docs/agent` doesn't
    formally version the shape; field-name changes between
    releases are possible.
  - The PRECEDENCE order under simultaneous-presence. The
    `.or_else()` chain at 3180-3185 prefers lowercase first,
    PascalCase second. If both forms coexist in a response
    (highly unlikely but not formally excluded), lowercase
    wins. The doc says "Accept both" but doesn't state the
    precedence — adding "lowercase preferred when both present"
    would close the ambiguity.
- **What's MISSING from the doc**: the WIRE FORMAT EXAMPLE.
  The test fixtures at `nomad_ch.rs:7196-7204` and
  `nomad_ch.rs:7214-7220` capture the shape but a one-line
  doc-example would let the future operator grok the contract
  without code-diving. E.g.:
  ```
  /// Expected JSON shape (Nomad 1.x lowercase form):
  ///   {"stats": {"client": {"node_id": "<uuid>"}}, ...}
  /// Tolerated alternative (PascalCase, older docs):
  ///   {"Stats": {"Client": {"NodeID": "<uuid>"}}, ...}
  ```
- **Recommendation**: 4-line doc-comment expansion at
  `nomad_ch.rs:3164-3169`. Pin the verification Nomad version,
  state the precedence, embed a one-line shape example. Pure
  comment-only change.
- **Severity**: MINOR (doc strengthening; no behavioural impact).

### R24-API3 (carry) — async wake-poll envelope still missing structured `which` field

- **Where**: `admin_handlers.rs:1971-2019`
  (`render_wake_poll_response`); unchanged this round.
- **Status carry**: r24 introduced this finding. The async
  wake-poll path's `extra` field includes `state`, `wake_id`,
  `sandbox_id`, `updated_at` but NOT `which`. The sync POST
  path's `extra` includes `which` + `sandbox_id`. The `which`
  value (workspace.img / user_home.img) is still recoverable
  from `message` ("staging image missing: workspace.img for
  sbx_…") because the `RestoreHandlerError::StagingPreflight`
  Display includes it at `restore_handler.rs:113-122`, but
  it's message-parsed rather than structured.
- **No movement r24 → r25**. The architecture proposal's
  Option B (schema-change to add `wake_jobs.error_extra
  JSONB`) is unaccepted; Option A (handler-side variant walk
  to synthesise `extra` from `error_code`) hasn't landed.
- **Severity**: MINOR (carry, no escalation).
- **Owner**: architecture r26 / Phase-2 schema decision.

## Considered + dismissed

- **`fetch_local_nomad_node_id` should be on `NomadCHBackend` not
  free-standing**: the function lives outside the `NomadCHBackend`
  impl block (it's a module-level `pub(crate) async fn`) so it can
  be called BEFORE backend construction in
  `AppState::from_config`. Moving it to a method would create a
  chicken-and-egg with the boot wiring (the backend constructor
  takes the node_id; the node_id requires the call). The
  free-standing shape is structurally correct. **No nit**.
- **`local_nomad_node_id` field on AppState should be
  pub(crate)**: the doc-comment at `lib.rs:223-224` explains
  "Field is `pub` (no security sensitivity — a node-id is the
  local agent's self-reported identifier, not a credential)".
  Out-of-crate integration tests in `sandbox/tests/*` might want
  to assert wiring (analogous to `admin_token()` accessor
  pattern at `lib.rs:307-309`). The `pub` is justified by the
  test-introspection design + the no-credential-leak posture.
  **No nit**.
- **`NomadCHBackend::local_nomad_node_id()` accessor is
  redundant**: the only caller is potential test introspection
  (`backend.nomad_ch_handle()?.local_nomad_node_id() == expected`).
  No production code reads it — the field is read internally at
  `nomad_ch.rs:792` via `self.local_nomad_node_id.as_deref()`.
  But the accessor pattern matches the existing
  `vm_index_allocator()` / `nomad_ch_handle()` accessor shape
  on the same struct, and the doc-comment at
  `nomad_ch.rs:430-435` names test-introspection as the rationale.
  **No nit**.
- **The Constraints array could be inlined into the
  `serde_json::json!` macro call** rather than a post-construction
  `job["Constraints"] = ...` mutation: the post-construction
  mutation pattern keeps the Constraints emission conditional on
  `local_nomad_node_id.is_some()` cleanly (without an
  `unwrap_or_else(Vec::new)` shim or a `null` literal in the
  emitted JSON). The current shape is structurally cleanest.
  **No nit**.
- **Cold-boot lacks an `includes_node_affinity_when_node_id_set`
  pin** like the restore path has: addressed in **R25-API1
  step 5** above; coverage exists via the cross-emitter parity
  test but isolated coverage is asymmetric. **Carry to
  test-coverage r26 for a sibling pin.** Comment only.

## §10.0 envelope state post-r25

### Inventory (delta from r24)

```
New since r24:  (none)
```

No new wire envelope kinds added this round. The 32 §10.0 codes
+ the 9 `WakeErrorCode` triangle entries are unchanged from r24.
**`staging_image_missing` remains the 9th and most-recent
`WakeErrorCode` family wire code**.

### POST-endpoint envelope audit (unchanged from r24)

| Endpoint | Required | 401 | 403 | 503 | Success | Post-r25 wire-shape drift? |
|---|---|---|---|---|---|---|
| `POST /admin/sandboxes/{id}/snapshot` | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 200 | none |
| `POST /admin/sandboxes/{id}/wake` (sync mode) | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 200 (with `agent_url`) | none |
| `POST /admin/sandboxes/{id}/wake` (async mode) | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 202 (`wake_id`) | none |
| `POST /admin/sandboxes/{id}/cold-boot` | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | `feature_disabled` 501 | none |
| `DELETE /admin/users/{id}` | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 200 (delete tombstone) | none |

r3-A's wire-shape change is BELOW the §10.0 envelope layer — it's
on the controller↔Nomad wire (Job.Constraints in the JSON the
controller POSTs to `/v1/jobs`), not on the platform↔client wire.
**No §10.0 drift this round.**

## Cross-lens consensus

### R25-API-CROSS-R26A1 — r26-A1 (`BackendFailureDetail` trait + 4 impls) wire-surface impact

Architecture r26 proposes a CRITICAL: a reusable
`BackendFailureDetail` trait with `fn log_detail(&self,
sandbox_id_typed: &str)` + `fn wire_summary(&self) -> String`,
and four impls (`LivezTimeoutDetail`, `RegisterFailedDetail`,
`ClockResyncFailedDetail`, `InternalUnsealDetail`). The four
target the wake-machine `rollback_with(_, _, code, e: String)`
sites that today take a free-text String error (and through
which path/IP/connection-string leaks can cross to RO admin).

**Wire-surface impact when/if r26-A1 lands**:

1. **Existing wire envelope shapes don't change.** The §10.0
   `{"error": "<wire_code>", "message": "...", "extra": {...}}`
   shape stays the same. What changes is the SOURCE of the
   `message` field: today the `rollback_with` `e: String` is
   embedded verbatim; post-r26-A1, `detail.wire_summary()` is
   the source, and it's structurally guaranteed path-free /
   IP-free / secret-free per the trait contract.
2. **`extra` field additions per failure code**:
   - `livez_timeout`: `extra` could carry `elapsed_ms`,
     `last_status` (the `LivezTimeoutDetail` struct fields per
     r26-A1's sketch). Architecture-side decision; api-surface-
     side recommendation is to add them STRUCTURALLY (per
     the sync path's pattern) rather than message-embedded.
   - `register_failed`: `extra` could carry `phase` (a
     `RegisterPhase` enum: `Probe` / `Register` / `Persist`).
     Operator-actionable distinction.
   - `clock_resync_failed`: `extra` could carry `vm_delta_secs`
     (the observed clock skew at failure time).
   - `internal_unseal`: `extra` could carry `kms_op` (which
     keystore op failed: `decrypt` / `wrap` / `unwrap`).
3. **Async wake-poll path symmetry**: r24-API3's async-path
   `which`-omission would extend across all 4 new failure
   modes if the same render-handler-walks-each-variant
   approach is taken. The Phase-2 `wake_jobs.error_extra
   JSONB` column approach would address the whole class
   uniformly. **Strong api-surface preference for the JSONB
   column**: it's the only structural fix that closes the
   sync/async-path asymmetry for all 5 typed wake errors
   (preflight + the 4 new from r26-A1).
4. **Wire-code domain extension**: r26-A1 doesn't add new
   `WakeErrorCode` variants (the existing `LivezTimeout`,
   `RegisterFailed`, `ClockResyncFailed`, `Internal` variants
   each gain a typed Detail struct). So the §10.0 inventory
   doesn't grow. The migration footprint at the pg
   `wake_jobs_error_code_check` level is ZERO — the typed
   restructure is internal to the producer side.
5. **Trait visibility**: `BackendFailureDetail` should be
   `pub(crate)` initially (no out-of-crate consumer). If the
   sandbox-agent crate ever needs to consume the same shape
   (it doesn't today — the agent emits structured errors
   over a different wire), promote to `pub`.

**Api-surface verdict on r26-A1**: NET POSITIVE for wire shape
discipline; aligned with the R25-S1 / SubmitRestoreError
pattern this round. The carry recommendation is to **bundle
r26-A1's wire-extra additions with the r24-API3 sync/async
asymmetry fix** so all 5 typed failure modes (preflight +
LivezTimeout + RegisterFailed + ClockResyncFailed + InternalUnseal)
get symmetric `extra` field shapes in one PR.

### R25-API-CROSS-R26A2 — r26-A2 (node-affinity ADR) wire-surface impact

Architecture r26-A2 documents 4 affordances broken by r3-A:
(1) Nomad-driven failover dies, (2) horizontal scaling on a
single sandbox is gone, (3) bin-packing efficiency is
worker-local, (4) worker-evacuation procedures change shape.
The ADR `2026-05-25-node-affinity-placement.md` does not yet
exist and should land with the r3-A code commit.

**Api-surface side of this question** (separated from the
architecture lens):

The r3-A wire surface itself is correct and conformant
(R25-API1 above). The api-surface-RELEVANT question is whether
the wire shape ITSELF needs to evolve to make the
controller-as-twin topology explicit. Considerations:

1. **Driver↔controller protocol**: the driver currently
   submits state transitions back to the controller via the
   sandbox-agent's gateway-driver interface (not via a typed
   wire — the driver's StartTask/StopTask state is the wire
   today, observed through Nomad alloc events). r3-A doesn't
   change this. The controller-as-twin topology means the
   driver's state events are uniquely correlated with ONE
   controller (the one on the same Nomad node).
2. **Controller-to-controller protocol**: there ISN'T one
   today. Controllers are independent. r3-A formalises this
   by name (every controller pins its own allocs to its own
   node). A future shared-storage migration (Option 2 from
   r26-A3) would need a NEW api-surface (cross-controller
   sandbox-claim protocol) — this is post-cutover-scope.
3. **Wire-format-immutable contracts** (AGENTS.md key
   invariant): `Manifest`, `RouteEntry`, `AppRecord`, `.zship`
   archive layout. None of these touch r3-A. **No
   immutable-wire-contract drift.**
4. **Operator-visible wire surfaces**: the §10.0 envelope is
   unchanged. The `/v1/agent/self` call is controller-internal
   (not exposed to operators). The Job.Constraints emission is
   controller-internal (operators see it indirectly via
   `nomad job inspect <jobid>` output, but that's Nomad's
   API not ours).

**Api-surface verdict on r26-A2**: the architecture-side ADR
is necessary; the api-surface side has NO wire-shape changes to
request. The ADR should NAME the api-surface invariants r3-A
PRESERVES (no §10.0 drift, no Manifest drift, no .zship
drift) so future readers know which surfaces are
post-cutover-discussible (Option 2 / Option 3 from r26-A3)
without re-litigating r3-A's correctness.

### Other cross-lens

- **Architecture r26 (r25-A1 / r25-A2 / r25-A4)**: closed via
  three landmark commits (`022f778a`, `3e853cc6`, `28fa64d1`).
  This round's r3-A landing is the next item architecture r26
  carries as IMPORTANT (r26-A2).
- **Code-quality r25**: typed-staging pattern is closed; no
  api-surface intersect this round.
- **Security r26**: r26-S? (haven't seen r26 yet) — the
  fetch_local_nomad_node_id error string is operator-readable
  + currently flat-`Result<_, String>` (R25-API4 above). Could
  carry over the `sanitize_error_message` mandate if the
  Nomad-version drift case ever surfaces a sensitive node-id
  shape — currently not a concern.
- **Test-coverage r26**: owns R22-API2 (readyz binding) +
  R25-API1's cold-boot-side sibling pin recommendation.
- **Concurrency r24 (R23-I1)**: terminal-overwrite counter
  unchanged this round.

## Lens hand-off

- **To architecture r25/r26**:
  - r26-A1's `BackendFailureDetail` trait should land with
    api-surface acceptance — the wire-shape impact is positive
    and the SubmitRestoreError pattern (this round's verify
    case) is the precedent.
  - r26-A2's ADR `2026-05-25-node-affinity-placement.md` should
    explicitly name "no wire-format drift in §10.0 / Manifest /
    .zship from r3-A" to lock the api-surface position.
- **To test-coverage r26**:
  - R22-API2 carry — controller-side `readyz` tests still
    synthesise response inline.
  - **NEW**: cold-boot-side `cold_boot_jobspec_includes_node_affinity_when_node_id_set`
    pin (R25-API1 step 5 above; currently coverage is via the
    parity test only).
  - Pin the §10.0 envelope inventory as a single enumeration
    test (r23 carry).
- **To security r25/r26**: R20-API1 schema-marker carry (now
  3-round + quadruply-motivated; r25 added no further
  motivation). The driver-side validator is the natural
  enforcement point. r3-A doesn't touch this surface.
- **To code-quality r26**:
  - R19-API2 `pub → pub(crate)` sweep still open (+13 `pub`
    tokens this round, ~3-5 of which could plausibly narrow
    to `pub(crate)`; see R25-API2's "Considered + dismissed"
    discussion of the accessor pattern).
  - R22-API3 (`rootfs_source` documentation asymmetric) still
    open.
  - R23-API2 / R23-API3 carries.
  - **NEW**: R25-API2 (constructor consolidation —
    `Backend::from_config_full` collapse to one constructor
    in a follow-up cleanup PR, gated on stress GREEN).
  - **NEW**: R25-API5 (`parse_nomad_agent_self_node_id`
    doc-strengthen: pin Nomad version, state precedence, add
    shape example).
- **To concurrency r25/r26**: no api-surface findings cross
  over this round. The boot-time `fetch_local_nomad_node_id`
  is sequential (no concurrency); the `with_local_nomad_node_id`
  installation is by-value at construction time (no shared
  mutability post-install — the field is `Option<String>` on
  an `Arc<NomadCHBackend>` and never re-written).

## Backlog carry table

| ID | First round | Status r25 | Severity | Lens to own |
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
| R25-API2 | r25 | **NEW** — 3rd constructor; cleanup consolidation candidate | MINOR | code-quality |
| R25-API3 | r25 | **NEW** — `Option<String>` vs newtype on Nomad node_id | MINOR | — (observation) |
| R25-API4 | r25 | **NEW** — `Result<_, String>` on fetch/parse — typed-enum forward-pressure | MINOR | code-quality |
| R25-API5 | r25 | **NEW** — doc-strengthen recommendation on parser | MINOR | code-quality |

Net: r24 open = 6 → r25 open = 10 (no closures, four new MINOR
all from the r3-A surface shape inspection). No new
CRITICAL/IMPORTANT. The trend remains observability/documentation
oriented.

## Trend

- **`pub`-token count**: r23 = 994; r24 = 998; **r25 = 1011**.
  Δ r24→r25 = +13. Net new `pub` surfaces this round (in
  approximate order of landing):
  - `Backend::from_config_full` (`pub fn`)
  - `AppState.local_nomad_node_id` (`pub` field on existing
    `pub struct`)
  - `NomadCHBackend::with_local_nomad_node_id` (`pub fn`)
  - `NomadCHBackend::local_nomad_node_id` (`pub fn` accessor)
  - `RealRestoreBackend::with_local_nomad_node_id`
    (`pub(crate) fn` — NOT pub, so this contributes 0 to the
    grep count)
  - `metrics::inc_nomad_node_id_lookup_failure` (`pub fn`)
  - `metrics::nomad_node_id_lookup_failures_value` (`pub fn`,
    `#[doc(hidden)]`)
  - Plus 4-5 additional pubs from sweep eligibility tests
    (R25-T4 closure at `901dfbf2`) and minor refactor edges
    that elevate test helpers.

  All net-new pubs are either justified by integration-test
  reachability (constructor + builder + accessor pattern is
  the precedent on `NomadCHBackend`, `RealRestoreBackend`,
  `AppState`) or by metric-exporter discipline (`#[doc(hidden)]`
  test-only readback).
- **`Result<_, String>`** (sandbox): r24 = 182 → r25 = ~171
  on my regex. Trend looks DOWN but the measurement is
  sensitive to the regex (counting `Result<X, String>` strict
  match vs `Result<_, String>` with whitespace tolerance).
  Two new uses from r3-A (`fetch_local_nomad_node_id`,
  `parse_nomad_agent_self_node_id`); typed-error introductions
  continue to outpace new String-error introductions on a
  net basis.
- **Net new wire envelope kinds r24→r25**: 0. No §10.0
  drift. r3-A's wire-shape change is controller↔Nomad
  (`Job.Constraints`), not platform↔client.
- **Wire-visible body-shape changes r24→r25**: 0.
- **New `WakeErrorCode` variants r24→r25**: 0. (r24 added
  `StagingPathMissing`; r25 added none.)
- **New rewriter sites for `_zsbx_path_schema_version`**: 0.
  R20-API1 motivation count holds at quadruple.
- **New constructors / coexisting constructors**: 1 (the
  3-arg `Backend::from_config_full`); see R25-API2 for the
  consolidation discussion.
- **Closure velocity r24→r25**: 0 closed; 4 new MINOR all
  related to the r3-A landing surface shape.
- **Backlog open-item count**: r24 = 6; **r25 = 10**
  (R19-API2 carry, R20-API1 carry, R22-API2 carry, R22-API3
  carry, R23-API2 carry, R23-API3 carry, R24-API2 carry,
  R24-API3 carry, R24-MIG1 carry, R24-SWEEP1 carry, R25-API2
  new, R25-API3 new, R25-API4 new, R25-API5 new — minus the
  one already-closed R23-API1).

## Pre-launch back-compat check

Per AGENTS.md "pre-launch, no back-compat" directive:

- **New `pub fn Backend::from_config_full`** alongside existing
  `from_config` + `from_config_with_persist`: SOURCE-compat
  for in-crate test/example callers; not user-back-compat.
  The directive's targets are wire-format / SDK-contract / V8
  RPC. In-crate constructor multi-arity is **allowed under
  pre-launch rule**; consolidation to one constructor is a
  legal pre-launch follow-up (R25-API2). The judgment call
  (keep 3 constructors at r3-A landing to minimise diff vs.
  consolidate immediately) is defensible — minimising diff
  during a 1/60 RED-stress hot fix is correct.
- **New `pub` surfaces on AppState / NomadCHBackend /
  RealRestoreBackend / metrics**: additive only; no signature
  changes on existing surfaces (the existing
  `NomadCHBackend::new` constructor is unchanged; the new
  `with_local_nomad_node_id` builder is additive). **Allowed**.
- **Wire shape r3-A `Job.Constraints` emission**: additive
  only — controller↔Nomad protocol gains a NEW jobspec field
  (`Job.Constraints`) when `local_nomad_node_id` is Some;
  pre-r3-A omission is the structural fallback when None.
  No existing field shape changed. Nomad accepts both shapes
  (Constraints array present or absent); the controller
  produces both shapes; no SDK-contract drift.
- **Migration**: NO new sql migrations this round.

**Verdict**: r3-A's pre-launch posture is correct. All new
api-surfaces (constructors, builders, fields, metrics) are
additive; the wire-shape change is additive at the
controller↔Nomad layer; the §10.0 wire envelope is
unchanged.

## Two most-critical citations

1. **`crates/sandbox/src/backend/nomad_ch.rs:2559-2580`** —
   the cold-boot emitter's r3-A Constraints block. The
   `${node.unique.id}` Nomad interpolation + `=` operand +
   `RTarget = local_nomad_node_id` shape is the load-bearing
   wire surface change for the stress-r3 78%
   cross-node-placement fix. Pinned for parity with the
   restore-path emitter at
   `restore_handler.rs:4414-4490`. Adding any second
   constraint here (e.g., per-datacenter or
   per-resource-pool affinity) **also requires** updating
   the restore-path emitter to match — the parity test
   guards against one-sided drift.

2. **`crates/sandbox/src/backend/nomad_ch.rs:3120-3199`** —
   `fetch_local_nomad_node_id` + `parse_nomad_agent_self_node_id`
   pair. The boot-time fetch is the load-bearing operator
   surface: a misconfigured Nomad agent (server-mode only,
   unreachable, or version-shape-drift) demotes the
   controller to pre-r3-A random-placement behaviour via
   `None`-fallback. The five failure shapes (NetworkError /
   NonSuccessStatus / ParseError / MissingField / EmptyNodeId)
   are flat-`Result<_, String>` today (R25-API4); the parser
   supports both PascalCase and lowercase JSON keys
   (R25-API5). The four pin tests at
   `nomad_ch.rs:7194-7273` lock the parser invariants;
   adding a new failure shape here means adding a new pin.
