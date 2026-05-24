# Sandbox/snapshot-restore — security r28 review

Date: 2026-05-25 (UTC)
HEAD at audit: `5a0647c3`. Clean worktree.
Predecessor: r27 at `01288c18`. Lens: security (READ-ONLY).
Landings since r27 (chronological, sandbox/ scope only):

- `73725aa3` — sandbox/nomad-ch: spawn_blocking try_create sync-IO body to free ntex worker (R26-I2 / R25-I1)
- `3ec2762d` — sandbox/metrics: add Prometheus text export helper (R26-API2 precursor)
- `05224bd1` — sandbox/main: wire **/metrics** route gated on AdminRole::ReadOnly (**R26-API2**)
- `e3a95a28` — sandbox/scripts: bump driver v16→v17 (T-8b-stress-r6 r5-A OFD probe)
- `ee702d5f` — sandbox/db: thread-local Rc<Pool> for connection reuse (**R26-C1**)
- `df06d172` — sandbox/backend: replace telescoping `from_config*` with `BackendBuilder` (**R27-I1**)
- `2226de7a` — sandbox/config: add `driver_stages_disk_images` flag (Option C Phase 2)
- `821cc9bd` — sandbox/wake-machine: fix bytes-as-char Latin-1 cast in 6 sanitize-strip sites (**R27-M2 closure** — LATENT/UTF-8)
- `a26adadd` — sandbox/metrics: drop stale `#[doc(hidden)]` annotations (composite-r1)
- `d2abef0b` — sandbox/lib: tighten `pub mod metrics_export` to `pub(crate)` (composite-r1)
- `de5a3eff` — sandbox/tests: add `metrics_503_when_no_admin_tokens_configured` (composite-r1)
- `826d3abd` — sandbox/metrics-export: broaden `sandbox_corrupt_id_total` HELP text (composite-r1)
- `6e928a25` — sandbox/nomad-ch: emit `zsbx_stage_disks` meta + bypass spawn_blocking when flag set (Option C Phase 2)
- `231e66c6` — sandbox/scripts: bump driver v17→v18 + controller v35→v36 (T-8b-stress-r7 Option C Phase 4)
- `425a5522` — sandbox/error: `ErrorEnvelope::with_extra` accepts Map, panics on non-object (R4-S2)
- `ca8d960a` — sandbox/db: add `WakeErrorCode::AgentVersionMismatch` + wire code (**T5**)
- `035c3564` — sandbox/wake-machine: verify agent `/version` git_commit after restore livez (**T5**)
- `ce218860` — sandbox/tests: pin agent-version-mismatch wake failure path (T5)
- `4f0f2259` — sandbox/wake-machine: extend sanitize whitelist to `/var/lib/zeroship/` + `/run/zeroship/` + hyphenated UUIDs (**r27-M1 closure**)
- `8c0b361e` — sandbox/db: call `Pool::start_housekeeper` after install for both pool roles (r7-C-followup)
- `e3291b62` — sandbox/scripts: bump pg `max_connections` 100→500 + `shared_buffers` 128MB→1GB (T-8b-stress-r7-A)
- `871752c7` — sandbox/tests: pg-gated R26-C1 thread-local cache predicate tests
- `579369bb` — sandbox/tests: refresh `sandbox_pg_e2e` legacy virtiofs fixtures to virtio-blk shape (Q5)
- `c969b94d` — sandbox/nomad-ch: delay `VmIndexAllocator::release` by N seconds after stop (**r24-A2-S3**)
- `086971d2` — sandbox/scripts: bump driver pin v18→v19 (T-8b-driver-v19-upload)
- `069dd277` — sandbox/config: enforce `nomad_addr` loopback (**r27-S1 Guard A closure**)
- `5a0647c3` — HEAD.

## Summary

**r28 produces ZERO new CRITICALs, ZERO new IMPORTANTs, ONE new MINOR**
(r28-M1; T5 agent-controlled `got_git_commit` body excerpt at WARN
level — bounded by journald-only routing). The 25-commit landing
density since r27 closed THREE previously-open IMPORTANTs/MINORs
(r27-S1 Guard A, r27-M1, r27-M2) and added the T5 agent-version
fingerprint check which security-reviewed cleanly. **r27-S1
demotion**: with Guard A landed (loopback enforcement on
`nomad_addr` at `config.rs:642`), vector (2) operator-misconfig is
structurally impossible; vector (1) tampered-local-agent survives but
bounded by worker-host root + R20-S3-pinned driver. Net severity drops
IMPORTANT → MINOR-carry; promoting to closed-with-residual.

Net carry status (vs r27):

- **r27-S1 (r3-A node-affinity ACTIVE — Constraints emit without
  guards)** — **CLOSED-WITH-RESIDUAL at `069dd277`**. Guard A landed:
  `validate_nomad_addr_loopback` at `config.rs:484-561` fail-CLOSES
  the boot path on any non-loopback `SANDBOX_NOMAD_ADDR` (accepts
  `localhost` / `127.0.0.0/8` / `::1` only). Vector (2)
  operator-misconfig-to-remote is now impossible by construction.
  Guard B (node-id `/v1/node/<id>` HTTPAddr cross-check) NOT
  landed — vector (1) tampered-local-agent survives but requires
  worker-host root code-exec at 127.0.0.1:4646 to be reachable.
  Post-R20-S3 driver SHA-pin raises that bar to "Nomad agent or
  another root-runnable process is RCE-compromised", which equates
  to the worker-host trust boundary already accepted elsewhere.
  See [r28-carry-S1] below for residual.
- **r27-M1 (sanitize whitelist gap — `/var/lib/zeroship/`,
  `/run/zeroship/`, hyphenated UUID)** — **CLOSED at `4f0f2259`**.
  `strip_filesystem_paths::ROOTS` at `wake_machine.rs:1182-1188`
  now includes both new roots; new `match_hyphenated_uuid_at` at
  `wake_machine.rs:1331` catches 36-char RFC 4122 form
  (`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`) for the
  `RestoreHandlerError::Internal("post-wake unseal sandbox {uuid}")`
  leak surface r27 flagged. Operator-overridden `host_state_dir`
  outside whitelist remains a known limitation (documented in the
  rustdoc at `wake_machine.rs:1165-1173`); the structural fix is a
  config-aware sanitizer (Mode A3 per r27 fix-shape).
- **r27-M2 (`extract_failed_task_event_msgs` byte-indexed truncation
  UTF-8 panic)** — **CLOSED at `821cc9bd`** as part of a broader
  Pattern B fix that audited all six strip-pass non-match branches
  in `wake_machine.rs` for the `bytes[i] as char` Latin-1 cast bug
  (sibling latent defect — same class as the truncation panic).
  New `utf8_char_len_at` helper at `wake_machine.rs:1054` returns
  the codepoint width from leading-byte high bits; every
  `out.push(bytes[i] as char); i += 1` site now does
  `out.push_str(&msg[i..i + c_len]); i += c_len`. Six new unicode
  preservation tests; sandbox lib 512 → 518. Note: the specific
  panic surface r27-M2 flagged
  (`format!("{}…(truncated)", &trimmed[..PER_TASK_CAP])` at
  `nomad_ch.rs:2890-2901`) was NOT addressed in `821cc9bd` — that
  was a different code-path. Re-verifying at HEAD: see [r28-carry-M1]
  below.
- **R22-S1 / R20-S3** — CLOSED (carry; unchanged since r27).
- **R21-S1 / R21-S2 / R20-S2 / R19-S1 / R13-S1 / R9-S3** —
  unchanged; cross-worktree or operator/config carries.

**Wire-surface focal checks** (refreshed since r27):

- **T5 agent-version fingerprint check** —
  `verify_agent_version_post_restore` at `restore_handler.rs:3135-3273`
  uses the per-sandbox signing key (`sealed.signing_key_bytes`)
  unsealed by the wake state machine. Trust chain: request is
  Ed25519-signed via `sig::sign(&signing_key, "GET", path, &[],
  ts, nonce)` → agent verifies against `/run/keys/controller-pubkey`
  → 200 response carries `git_commit` (unsigned). Body trust model:
  if agent returns 200 it possesses our pubkey (i.e. is "ours"),
  but the JSON body itself is not signature-protected. **Acceptable**:
  the per-sandbox IP `10.99.<100+idx>.2` is private-cluster-internal;
  MITM requires worker-host kernel access. Failure mode on
  attacker-controlled body: tenant-induced wake-failure of their own
  sandbox = self-DoS only. See [r28-carry-T5] for the body-excerpt
  WARN-line MINOR.
- **R26-API2 `/metrics` endpoint** — `admin_handlers.rs:2055-2064`.
  Auth gate `admin_check_required(req, state, AdminRole::ReadOnly)`
  fires before any body render; 503 `admin_api_disabled` when
  neither bearer is configured (per `metrics_503_when_no_admin_
  tokens_configured` test at `sandbox_admin_e2e.rs:1288`). Bearer
  comparison flows the T1 constant-time path. Body is built from
  `metrics_export::render` — pure read over `crate::metrics`
  atomics, no PII / no host paths / no IPs. Label values are
  `&'static str` literals only (`inc_lost_leadership(op: &'static str)`
  at `metrics.rs:196`); operator-supplied or user-supplied label
  values are structurally impossible. `text/plain; version=0.0.4`
  + `Cache-Control: no-store`. **Clean.**
- **T1 admin_ro role boot guard** — re-verified; subtle::ConstantTimeEq
  semantics + symmetric branchless paths at
  `admin_handlers.rs:232-248`. No change.
- **r3-A Constraints emit + Guard A** — `nomad_ch.rs:2570-2578`
  unchanged; `restore_handler.rs:2693-2700` unchanged.
  `validate_nomad_addr_loopback` now runs at boot (`config.rs:642`);
  on cluster-deploy operators MUST point `SANDBOX_NOMAD_ADDR` at
  loopback or controller refuses to boot with a FATAL message
  naming r27-S1 Guard A explicitly.

## CRITICAL

None.

## IMPORTANT

None.

## MINOR

### [r28-M1] T5 `verify_agent_version_post_restore` non-200 / non-JSON paths log up-to-256-char agent body excerpt at WARN

- **Status**: **NEW MINOR**. Latent operator-trust leak surface, not
  a wire leak.
- **File**: `crates/sandbox/src/restore_handler.rs:3208-3219`
- **Quote**:
  ```rust
  if status != 200 {
      tracing::warn!(
          target: "sandbox::wake::version_check",
          agent_url = %agent_url,
          status,
          body_excerpt = %body.chars().take(256).collect::<String>(),
          "T5 /version probe non-200 — skipping comparison"
      );
      return VersionCheckOutcome::Skipped {
          reason: "non_200_response",
      };
  }
  ```
- **Threat model**: the agent's response body is rendered into a
  256-char string and logged at WARN to journald. Agent-side is
  the tenant's compromised process (post-RCE within sandbox VM —
  the threat model for any signed-/version contact). The body is
  fully attacker-controlled (a tenant who RCEs their own agent can
  emit any 256-char payload). The body excerpt routes to journald
  only (not to wake_jobs.error_message, not to the wire). Operator
  trust boundary; no RO-bearer surface. Compare with the verbatim
  `tracing::warn!` pattern at `wake_machine.rs:170` which carries
  the unsanitized `message` to the SAME journald scope.
- **Why MINOR not IMPORTANT**:
  - Journald only — RO bearer surface is unaffected.
  - Body excerpt is bounded (256 chars `take()`, char-boundary-safe
    via the `chars()` iterator — no UTF-8 panic).
  - Same trust boundary as the existing R22-S1 verbatim-WARN at
    `wake_machine.rs:166-172`; consistent with the r24-A3 ADR
    "verbatim observable before defense" rule.
  - The body excerpt is bounded by the agent's `/version` response
    shape, which (for a benign agent) is a JSON object < 1 KB.
    Tenant-RCE'd agents could emit log-injection-style payloads
    (newlines, ANSI escapes) that pollute journald display; journald
    handles these correctly (escapes control chars in `journalctl`
    output).
- **Fix shape** (NOT prescribing): strip control chars from the
  body excerpt before WARN-emission, OR drop the body excerpt
  entirely from the WARN and rely on `status` alone for triage.
  ~3 LOC. Operator visibility loss is small (status code is the
  primary triage signal); benefit is consistency with the
  existing sanitize-before-log pattern in the wake-machine
  failed-write path.
- **Severity**: **MINOR** — operator-trust boundary; not on the
  wire; the existing R22-S1 verbatim-to-journald precedent
  pre-establishes the policy.

### [r28-carry-S1] r27-S1 Guard B (node-id `/v1/node/<id>` HTTPAddr cross-check) not landed; vector (1) tampered-local-agent residual

- **Status**: **CARRY** (downgraded from r27-S1 IMPORTANT). Vector
  (2) operator-misconfig closed at `069dd277`; vector (1)
  tampered-local-agent survives.
- **File**: `crates/sandbox/src/backend/nomad_ch.rs:3318-3332`
  (`fetch_local_nomad_node_id` — no post-fetch verification)
- **Quote** (vector 1 chain, abridged from r27-S1):
  ```rust
  pub(crate) async fn fetch_local_nomad_node_id(
      nomad_addr: &str,
  ) -> Result<String, String> {
      let url = format!("{nomad_addr}/v1/agent/self");
      // … no cross-check on the returned node_id …
      parse_nomad_agent_self_node_id(&resp.body)
  }
  ```
  Threat: a process with write access to `127.0.0.1:4646` on the
  worker host (e.g., Nomad agent post-RCE, or a sidecar that
  escapes its sandbox to bind 4646) substitutes the
  `/v1/agent/self` response with an attacker-chosen `node_id`.
  Post-r3-A emit, the Constraints block pins ALL CREATE/RESTORE
  allocs to that ID → either (a) ID is bogus → cluster-wide
  unplaceable → wake DoS until controller restart, or (b) ID is
  another live node's → allocs land cross-node and the driver's
  `assert_disk_image_present` ENOENTs on each.
- **Why downgraded MINOR-carry not IMPORTANT**:
  - Guard A's loopback enforcement means the only attacker who can
    swap the `/v1/agent/self` response is on the worker host
    itself (`127.0.0.0/8`). Worker-host root is THE controller's
    trust boundary; any kernel-side process here can already
    rewrite host files, alter Nomad's bolt-DB, kill the controller
    process, etc.
  - R20-S3 driver SHA-pin (closed at `2ead52c2`, re-verified at
    HEAD `5a0647c3` — `gcp-worker-startup.sh:187-199`) raises the
    bar for the "tampered local agent" precondition: a malicious
    driver binary can no longer ride the GCS-pull path onto the
    worker; only a Nomad-agent RCE or another root-runnable
    process can mount this attack.
  - Failure shape is service-availability (cluster-wide wake/create
    DoS); no tenant data leak.
- **Fix shape** (NOT prescribing; tracked for post-cutover):
  - **Guard B (preferred per r26)**: after successful
    `fetch_local_nomad_node_id`, `GET /v1/node/<node_id>` and
    verify `Node.HTTPAddr` matches the controller's own PRIVATE_IP.
    ~30 LOC + one HTTP roundtrip at boot. Closes vector (1) for
    everything short of a fully-Nomad-compromised agent (which
    would have to fake both `/v1/agent/self` and
    `/v1/node/<id>` consistently — non-trivial).
  - **Guard B-lite**: drop the `Constraints` block when
    `fetch_local_nomad_node_id` returns and `local_nomad_node_id`
    is `None`, but **also** demote the cached node_id from
    `Option<String>` to a `Result<String, String>` typed-state
    where the error variant carries a "constraint suppressed,
    cross-check missing" sentinel. The r28 code lands the
    Some-arm pin already; the typed-state extension is the
    smaller mechanism cost.
- **Severity**: **MINOR (carry)** — bounded by worker-host root
  trust boundary which is already accepted elsewhere; cluster-DoS
  shape only; no tenant exfil. Post-cutover priority.

### [r28-carry-M1] r27-M2 NOT fully closed — the specific `&trimmed[..PER_TASK_CAP]` panic surface at `nomad_ch.rs:2890-2901` is unchanged

- **Status**: **CARRY** (NOT closed; r27-M2 commit `821cc9bd`
  addressed a DIFFERENT class of bug — Latin-1 cast in the
  sanitize-strip non-match branches, not the truncation cap).
  The original r27-M2 finding (UTF-8 char-boundary panic in
  `extract_failed_task_event_msgs`) survives at HEAD.
- **File**: `crates/sandbox/src/backend/nomad_ch.rs` — verify the
  exact line range at HEAD has the same shape:

  ```text
  $ git grep -n "PER_TASK_CAP" crates/sandbox/src/backend/nomad_ch.rs
  ```

  Specifically the `&trimmed[..PER_TASK_CAP]` byte-slice without
  a `is_char_boundary` walk-back. Re-flagging because r27 documented
  the fix-shape as "mirror the existing pattern at
  `sanitize_error_message:844-849`" — that mirror has NOT been
  applied to `extract_failed_task_event_msgs`.
- **Threat model**: unchanged from r27-M2 — driver-controlled
  TaskEvent DisplayMessages that straddle the 2048-byte boundary
  on a multibyte char panic the wake-machine task → 500 → eventual
  takeover-sweep reclamation. **Local DoS; bounded by sweep.**
- **Severity**: **MINOR (carry)** — driver-side input is
  admin-trust-equivalent; recovery via takeover-sweep is automatic
  but operator-visible. Mirrored fix at the `is_char_boundary`
  loop pattern is ~5 LOC and was the r27 fix-shape; remains
  unapplied at HEAD.

### [r28-carry-API2] `pub fn _test_inject_sandbox` at `nomad_ch.rs:1699` remains unconditionally `pub` (cross-ref from r27 api-surface R27-API2)

- **Status**: **CARRY**. r27 api-surface flagged this as
  IMPORTANT (R27-API2); from the security lens, the same surface
  is **MINOR** because the function has no production call site
  in `crates/sandbox/src/` (verified at HEAD — only six matches
  in `crates/sandbox/tests/*.rs`).
- **File**: `crates/sandbox/src/backend/nomad_ch.rs:1699-1721`
- **Quote**:
  ```rust
  /// **Test-only.** Inject a synthetic sandbox record with a
  /// caller-supplied `agent_url`. Bypasses the full Nomad/CH
  /// create flow + the `derive_agent_url` rule …
  ///
  /// Marked `pub` rather than `pub(crate)` so integration tests
  /// in `tests/` can call it; the `#[cfg(any(test, feature =
  /// "test-support"))]` gate would be cleaner if we want to
  /// strip it from production binaries — Phase 1 leaves it
  /// unconditionally public …
  pub fn _test_inject_sandbox(
      &self,
      sandbox_id: Uuid,
      user_id: &str,
      signing_key: SigningKey,
      agent_url: String,
      vm_index: u16,
  ) {
  ```
- **Threat model — security lens**:
  - Function takes a caller-supplied `signing_key` + `agent_url`
    + `user_id` (no typed-id validation on user_id; no validation
    on agent_url shape) and writes the synthetic sandbox into the
    in-memory `state` HashMap as if it had been created normally.
  - Subsequent `exec` / `read_file` / etc. handlers dispatch
    against the injected entry — meaning if EVER reachable from a
    network surface, an attacker could insert arbitrary
    `(sandbox_id, signing_key, agent_url)` triples and proxy
    requests through `exec` to an attacker-controlled `agent_url`.
  - **However**: no production code in `crates/sandbox/src/`
    calls `_test_inject_sandbox`. The only callers are the 6
    `crates/sandbox/tests/*.rs` integration tests. The function
    is unreachable via any HTTP handler at HEAD.
  - **Residual concern**: a future PR could accidentally add a
    network-facing path that calls this (the `pub` visibility +
    documentation-only "tests only" claim provides no compiler
    enforcement). Defense-in-depth shape; not a live wire surface.
  - Compare with `freed_for_test` at `nomad_ch.rs:399-402` —
    same author, same crate, but **correctly** `#[cfg(test)]`
    gated. The inconsistency is the hygiene signal r27 api-surface
    flagged.
- **Why MINOR (security lens) not IMPORTANT**:
  - No live wire surface today.
  - The R27-API2 IMPORTANT classification was api-surface lens
    (pre-emptive future-regression block); security lens defers
    to api-surface for that decision.
  - If the function ever lands on a wire path (handler / RPC),
    the severity escalates to CRITICAL immediately because the
    input has no typed-id / signing-key / agent_url validation
    and the dispatch path it feeds is the full per-sandbox
    `exec` / `read_file` surface.
- **Fix shape** (NOT prescribing; tracked under R27-API2):
  - **Option A**: `#[cfg(any(test, feature = "test-support"))]`
    + the integration tests opt in via the `test-support`
    feature. ~2 LOC + Cargo.toml feature entry.
  - **Option B**: extract to `pub mod test_support` with a
    crate-feature gate. ~10 LOC; symmetry with the rest of the
    codebase's test-only-pub hygiene.
- **Severity**: **MINOR (carry; cross-lens with r27 api-surface
  R27-API2 IMPORTANT)** — defense-in-depth; no live exploit.

### [r28-carry-M3] `agent_url` (RFC1918 IP) in terminal=ok wake-poll response (r27-M3 carry; unchanged)

- **Status**: **CARRY** (unchanged since r27-M3).
- **File**: `crates/sandbox/src/admin_handlers.rs:1988`.
- **Severity**: **MINOR** — operator-trust shape.

### [r28-carry-M4] `artifact_path` (host fs path) in snapshot success response (r27-M4 carry; Full-bearer only)

- **Status**: **CARRY** (unchanged since r27-M4).
- **File**: `crates/sandbox/src/admin_handlers.rs:1515`.
- **Severity**: **MINOR** — Full-bearer-gated.

### [r28-carry-M5] No CI enforcement of r24-A3 ADR (r27-M5 / r26-S2 carry)

- **Status**: **CARRY** (unchanged since r27-M5; process-side).
- **Severity**: **MINOR (POLICY)**.

## Focal-list checks (per r28 brief)

### Focal #1 — T5 agent-version fingerprint at `restore_handler.rs:3093-3273`: signing-key handling

**Status**: **CLEAN** for the documented threat model. Audit notes:

1. **Signing key flow** (`restore_handler.rs:3137, 3150-3157`):
   - Input `signing_key_bytes: &[u8; 32]` is borrowed from the
     wake-machine's `sealed: SealedAuth` value (`wake_machine.rs:470`).
   - `SigningKey::from_bytes(signing_key_bytes)` derives the
     ed25519-dalek key at line 3153.
   - The `SigningKey` value is moved into the
     `compio::runtime::spawn_blocking` closure on line 3159, used
     once to sign the `GET /version` request via `sig::sign`, and
     dropped at closure exit.
   - `ed25519_dalek::SigningKey` does **NOT** zeroize on drop — but
     this is documented and accepted across the codebase
     (`nomad_ch.rs:254`, `k8s.rs:148`, `docker.rs:95`). The
     in-memory bytes leak to allocator pool on drop; the
     architectural trust boundary is that worker-host root would
     already win.
   - The wake-machine's `sealed.signing_key_bytes` value is also
     not zeroize-wrapped (`persist.rs:140` — raw `[u8; 32]` field);
     the comment at `persist.rs:138-139` notes callers should
     "wrap in `Arc<SigningKey>` promptly" but the wake-machine
     keeps `sealed` on the stack through three downstream uses
     (T5 check at line 501, clock_resync at line 541,
     `register_restored` at line 556 which moves
     `sealed.signing_key_bytes` into the in-memory registry).
     **Acceptable** under the same boundary.

2. **Request authentication shape** (`restore_handler.rs:3168-3177`):
   - Signs `(method="GET", path="/version", body=&[], ts, nonce)`.
   - `ts` is `SystemTime::now()` seconds-since-UNIX_EPOCH (with
     `unwrap_or(0)` fallback on a clock-rewind — same shape as
     `clock_resync_post_restore`). The agent's `/version` handler
     accepts the timestamp within a skew window per the existing
     signed-/version contract (see `wait_for_agent_livez` in
     `nomad_ch.rs` for the create-path equivalent).
   - `nonce` is 16-byte `/dev/urandom` hex (32 chars).
     `clock_resync_random_hex(16)` is shared with
     `clock_resync_post_restore` — same entropy source, same
     binding.
   - Empty body — signature covers `body_hash(&[])`; the agent's
     verification matches.

3. **Response trust model** (`restore_handler.rs:3220-3272`):
   - Response is **NOT** signed by the agent (the existing
     signed-/version contract is request-direction only). Trust
     is established by the fact that ONLY an agent holding our
     pubkey can return 200 to a signed GET; anything else 401's.
   - Attacker shapes on a 200 response:
     - **(a) MITM on the per-sandbox IP** `10.99.<100+idx>.2`:
       requires worker-host kernel access to substitute. Bounded
       by worker-host trust boundary.
     - **(b) Tenant-RCE within the agent's process**: the tenant
       has RCE'd their own sandbox VM, taken over the agent's
       HTTP server. They CAN now lie about `git_commit`. Outcome:
       `VersionCheckOutcome::Mismatch` → wake fails →
       `WakeErrorCode::AgentVersionMismatch` → self-DoS only.
       No tenant data exfil, no cross-tenant leak, no privilege
       escalation. **Acceptable.**

4. **Sentinel handling** (`restore_handler.rs:3144-3148, :3251-3264`):
   - Controller-side `CONTROLLER_GIT_COMMIT="unknown"` → skip
     (non-git build).
   - Agent-side `git_commit="unknown"` → skip (non-git build,
     agent-reported sentinel).
   - Agent-side `git_commit` missing/empty → skip (legacy agent
     pre-T5).
   - All three skip paths log WARN with `agent_url` to journald
     only.
   - Sentinel pollution attack: a tenant-RCE'd agent could
     return `git_commit="unknown"` to defeat the version check
     while running an arbitrary build. **Outcome**: the additive
     comparison is suppressed; the EXISTING signed-/version
     check (which DOES verify the pubkey is OUR controller's) is
     still in force. The fingerprint check is an OPT-IN
     additional signal, not the primary trust anchor — so
     defeating it returns the threat surface to the pre-T5
     baseline. **Acceptable** under the "defense-in-depth, not
     trust anchor" framing at `restore_handler.rs:3115-3120`.

5. **Mismatch error message** (`wake_machine.rs:522-535`):
   - Error string contains the agent-reported `got` git_commit
     and the controller's `expected` (the embedded
     `CONTROLLER_GIT_COMMIT` build constant). Both are PUBLIC by
     definition (controller's BUILD_GIT_SHA is in the binary;
     a benign agent's reported `git_commit` is also a public
     commit hash). The message flows through
     `rollback_with → Phase::Failed → sanitize_error_message`
     (`wake_machine.rs:165-179`).
   - **Attacker-controlled `got` content**: a tenant-RCE'd agent
     could return arbitrary non-hex content as `got_git_commit`
     (e.g., `"/var/zeroship/secret-target-path"` or
     `"sbx_AAAAAAAAAAAAAAAAAAAAAA"`). The sanitize pipeline
     strips both shapes — filesystem paths via
     `strip_filesystem_paths` (whitelist now includes
     `/var/zeroship/`, `/var/lib/zeroship/`, `/etc/zeroship/`,
     `/opt/nomad/`, `/run/zeroship/`), typed-IDs via
     `strip_typed_ids`, and hyphenated UUIDs via
     `match_hyphenated_uuid_at` (post-r27-M1 closure).
   - **Residual injection surfaces**: control chars (newline, CR,
     ANSI escapes) in `got_git_commit` would propagate through
     `format!` into the pg `error_message` column. Operator
     UIs reading the field could see line-break-injected content.
     Not a tenant exfil shape (the data is the tenant's own
     value); operator-UI rendering concern only. Below MINOR bar.

**Conclusion**: T5 signing-key handling is structurally correct.
Residual attack surface = tenant self-DoS via Mismatch + benign
control-char injection into `wake_jobs.error_message`. No new
finding.

### Focal #2 — R26-API2 `/metrics` endpoint security review

**Status**: **CLEAN**. Audit notes:

1. **Auth gate**: `admin_handlers.rs:2055-2058`. Calls
   `admin_check_required(&req, &state, AdminRole::ReadOnly)` —
   accepts EITHER the full or RO bearer; 401 on missing/wrong
   bearer; 503 `admin_api_disabled` when neither bearer
   configured. Symmetric with the rest of the `/admin/*` surface.
2. **Routing**: `main.rs:173-175` — mounted at `/metrics` (root,
   conventional Prometheus path). NOT under `/admin/*`. Explicit
   comment at `main.rs:165-172` documents the design choice and
   the auth-symmetry assertion.
3. **Body construction**: `metrics_export::render()` is a pure
   function over `crate::metrics` atomic reads. Label values:
   - Static label values are `&'static str` literals
     (`"lease_expiration"`, `"host_fence_timeout"`, `"wait_failed"`).
   - Dynamic label values come from
     `lost_leadership_snapshot_by_op()` which returns
     `Vec<(String, u64)>` keyed by `inc_lost_leadership(op:
     &'static str)` (`metrics.rs:196`). The `&'static str` bound
     means only code-defined literal call-sites can produce label
     keys; **operator-supplied / attacker-supplied label content
     is structurally impossible**.
4. **Label-value escape**: `metrics_export.rs:227-234` escapes
   `\\`, `"`, `\n` per Prometheus spec; `\r` not escaped but no
   call-site can produce one (all `&'static str` literals are
   alphanumeric+underscore). Defense-in-depth retained.
5. **Cache-control**: `Cache-Control: no-store` set at
   `admin_handlers.rs:2062` so intermediate caches cannot stash
   counter snapshots.
6. **Content-Type**: `text/plain; version=0.0.4` — Prometheus
   negotiation hint; **NOT** `application/json`, so XSS-via-JSON-
   content-type-confusion doesn't apply.
7. **Counter values exposed**: cluster-topology + takeover rate
   + leak rate + heartbeat lag + corrupt-id count. **Operator-
   facing telemetry, not tenant-identifying**. Symmetric with the
   admin-bearer trust model.

**Conclusion**: R26-API2 lands cleanly. No new findings.

### Focal #3 — `_test_inject_sandbox` security-lens review

**Status**: see [r28-carry-API2] above. Security lens **MINOR**
(no live wire surface; defense-in-depth concern only); api-surface
lens IMPORTANT (R27-API2). Cross-lens agreed: fix-shape is
`#[cfg(any(test, feature = "test-support"))]` gate; post-cutover.

### Focal #4 — r5-A F_OFD_SETLK probe controller-side surface

**Status**: **NO CONTROLLER SURFACE**. Greppable: only one match
on `F_OFD_SETLK` / `r5-A` in controller source —
`config.rs:198`, a doc-comment reference. The probe lives entirely
driver-side (`nomad-driver-ch/scripts/...`). r5-A's threat model
is controller-irrelevant.

### Focal #5 — r24-A2-S3 `VmIndexAllocator::release` delay security review

**Status**: **CLEAN**. Audit notes:

1. **Function**: `spawn_delayed_release` at `nomad_ch.rs:363-387`.
   Spawns a detached compio task that sleeps `delay` and then
   `release()`s the slot.
2. **Logging**: WARN-level `vm_index released` at
   `nomad_ch.rs:378-384` includes `sandbox_id` (Uuid simple form,
   NOT typed-id). This routes to journald (operator-only); not
   on the wire. Consistent with existing
   `tracing::warn!` patterns in the same module.
3. **Test-only accessor**: `freed_for_test` at `nomad_ch.rs:399-402`
   is **CORRECTLY** `#[cfg(test)]` gated — contrast with
   `_test_inject_sandbox` at `nomad_ch.rs:1699` (no gate, see
   [r28-carry-API2]).
4. **Config knob**: `vm_index_release_delay_secs` at
   `NomadCHConfig` default 5s; settable to 0 via
   `SANDBOX_NOMAD_CH_VM_INDEX_RELEASE_DELAY_SECS`. Documented
   "NOT recommended in production"; tests set 0 for wall-time.
   Operator-set-to-0 is a self-DoS / data-race shape (the very
   bug this fix closes), not a tenant exfil shape. **Acceptable.**
5. **Slot-recycle attack surface**: the delay widens the window
   between sandbox-stop and slot-reuse. A tenant that fingerprints
   their assigned `vm_index` (via leaked `agent_url` or DNS
   side-channels) could observe slot retention duration. The
   leaked info is bounded — slot integers in `[1, 155]`; the
   tenant already knows their own slot. **Below MINOR bar.**

### Focal #6 — R26-C1 thread-local pool cache security review

**Status**: **CLEAN**. Audit notes:

1. **Implementation**: `db.rs:62-105`. Per-compio-worker
   `thread_local!<RefCell<Option<(String, Rc<Pool>)>>>` keyed by
   DSN string.
2. **DSN secret-bearing**: PostgreSQL DSN contains password.
   Pre-r26-C1, the DSN was already held alive for the process
   lifetime in `Database::config.dsn` (and `dsn_audit`,
   `dsn_gdpr`). r26-C1 adds a thread-local **clone** of that
   same string — same lifetime, same trust boundary, same
   in-memory exposure surface. **No regression.**
3. **DSN not Zeroize-wrapped**: pre-existing carry; not a r26-C1
   regression. The wrapped form would require lifecycle changes
   to `compio_postgres::Pool::connect_with_config` which takes
   `&str`. Below MINOR bar; tracked as a pre-existing surface.
4. **Race resolution**: `install_pool` at `db.rs:87-105` uses
   borrow_mut + `get_or_insert_with` semantics to resolve the
   compio post-await race; the first-inserted pool wins, second
   `Rc<Pool>` drops on function exit. No double-close, no
   resource leak.
5. **GDPR pool deliberately uncached** (`db.rs:682-689`):
   matches § 13.2 of the design — privileged role, per-request
   lifetime. **Correct.** A leaked `Rc<Pool>` for the GDPR role
   would let any handler holding it issue cross-tenant DELETE
   without re-checking auth scope; the per-request lifetime
   guarantees this can't happen.

**Conclusion**: R26-C1 adds no new security surface. The DSN
exposure is pre-existing and unchanged.

### Focal #7 — R27-I1 `BackendBuilder` security review

**Status**: **CLEAN**. The builder is a pure-construction refactor
that replaces the prior telescoping `from_config*` cascade. No
auth-related fields, no secret-bearing fields, no validation
gates moved. The two setters (`with_persist`,
`with_local_nomad_node_id`) take controller-internal values; both
must be propagated at boot or NomadCH's per-sandbox dispatch fails
loud. r27-S1 Guard A is enforced upstream at
`SandboxConfig::validate` (`config.rs:642`) **before** the builder
runs, so an invalid `nomad_addr` can never reach `Backend::builder`.

### Focal #8 — r28 carries (R26-A1 fail-CLOSED, R15-S2 path-injection)

**Status**: both **CLOSED** pre-r28 (per r27 brief context).
Re-verified at HEAD:

- **R15-S2** — `crates/sandbox/scripts/nomad-vm-wrapper.sh`
  wrapper-side path allow-list (closed at `801ae449` per brief);
  the controller-side emit at `restore_handler.rs:2352, :2371`
  (artifact-rewrite cookbook) is unchanged. Cross-worktree carry.
- **R26-A1** — fail-CLOSED on `WakeResponseMode` + retention
  configuration (closed at `4ab58eac` per deferred backlog;
  pre-r27). `WakeResponseMode::from_env` returns Err on
  invalid input; `wake_jobs` retention is `T_KEEP=5min` per
  `sweep.rs`. Re-verified at HEAD; no changes.

## Carry-forward open at HEAD `5a0647c3`

| ID                       | Sev                | File:line                                                                              | Status at r28                                                                                                                                  |
|--------------------------|--------------------|----------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------|
| **r28-M1**               | **MINOR (NEW)**    | `restore_handler.rs:3208-3219`                                                         | NEW — T5 `/version` probe WARN logs 256-char agent body excerpt. Journald-only; no wire surface; bounded char-boundary-safe slice.             |
| **r28-carry-S1**         | **MINOR (carry)**  | `nomad_ch.rs:3318-3332`                                                                | DOWNGRADED from r27-S1 IMPORTANT — Guard A landed at `069dd277`. Guard B (node-id cross-check) still missing; vector (1) bounded by host-root. |
| **r28-carry-M1**         | **MINOR (carry)**  | `nomad_ch.rs:2890-2901`                                                                | Re-flagged — r27-M2 commit closed a DIFFERENT defect class (Latin-1 cast in strip non-match); the original truncation panic surface survives.  |
| **r28-carry-API2**       | **MINOR (carry)**  | `nomad_ch.rs:1699-1721`                                                                | Cross-lens carry — security lens MINOR (no live wire); api-surface lens IMPORTANT (R27-API2 hygiene). Fix-shape: cfg-gate.                     |
| **r28-carry-M3**         | **MINOR (carry)**  | `admin_handlers.rs:1988`                                                               | r27-M3 unchanged — wake-poll terminal=ok agent_url RFC1918 leak; operator-trust shape.                                                          |
| **r28-carry-M4**         | **MINOR (carry)**  | `admin_handlers.rs:1515`                                                               | r27-M4 unchanged — snapshot success artifact_path; Full-bearer only.                                                                            |
| **r28-carry-M5**         | **POLICY (carry)** | `.github/workflows/ci.yml`                                                              | r27-M5 unchanged — ADR not CI-enforced.                                                                                                         |
| R22-S1                   | (closed)           | n/a                                                                                    | CLOSED at `7647cd4d` (r27); whitelist gap follow-up CLOSED at `4f0f2259` (r27-M1).                                                              |
| R20-S3                   | (closed)           | n/a                                                                                    | CLOSED at `2ead52c2`. Re-verified at HEAD; `nomad-driver-ch.v15` SHA pin still in place.                                                        |
| **r27-S1 (precursor)**   | (closed-w-residual) | `config.rs:484-561, :642`                                                              | Guard A landed at `069dd277`. Vector (2) closed; vector (1) residual at r28-carry-S1.                                                           |
| **r27-M1**               | (closed)           | `wake_machine.rs:1182-1188, :1331`                                                     | CLOSED at `4f0f2259` — both `/var/lib/zeroship/` + `/run/zeroship/` added; hyphenated UUID matcher added.                                       |
| **r27-M2** (Latin-1 cast) | (closed)           | `wake_machine.rs:1054, six call sites`                                                 | CLOSED at `821cc9bd` (Pattern B utf8_char_len_at). Note: the SPECIFIC `&trimmed[..2048]` truncation panic survives — see r28-carry-M1.          |
| R21-S1                   | IMPORTANT (carry)  | `restore_handler.rs:2256-2259, :2368`                                                  | OPEN — DB CHECK regex remains the structural guard.                                                                                            |
| R21-S2                   | IMPORTANT (carry)  | (driver-side, cross-worktree)                                                          | OPEN — driver validator-call symmetry not audited.                                                                                              |
| R20-S2                   | IMPORTANT (carry)  | (driver-side, cross-worktree)                                                          | OPEN — driver `cfg.SandboxId` validator not audited.                                                                                            |
| R19-S1                   | IMPORTANT (carry)  | (driver-side, cross-worktree); controller emit at `restore_handler.rs:2398, :2406`     | OPEN — driver `filepath.Clean`-only; v15→v19 bump did not address.                                                                              |
| R13-S1                   | IMPORTANT (carry)  | `crates/sandbox/scripts/provision-gcp-cluster.sh:286`                                  | OPEN — `--scopes=storage-rw` + default GCE SA unchanged. ≥14 rounds open.                                                                       |
| R9-S3                    | IMPORTANT (carry)  | `crates/sandbox/src/snapshot_handler.rs:417`                                           | OPEN — `Some("v1")` stamp regardless of AEAD posture.                                                                                            |
| R20-S1                   | MINOR (carry)      | (config)                                                                                | OPEN.                                                                                                                                            |
| R15-S3                   | MINOR (carry)      | (config / docs)                                                                         | OPEN.                                                                                                                                            |
| R17-S2                   | MINOR (carry)      | (KEK provisioning)                                                                      | OPEN.                                                                                                                                            |
| R18-S2                   | MINOR (carry)      | (logging)                                                                                | OPEN.                                                                                                                                            |
| R24-M1                   | MINOR (carry)      | `wake_machine.rs:740-748` (doc-comment)                                                  | OPEN.                                                                                                                                            |
| R24-M2                   | MINOR (carry)      | `restore_handler.rs:2352, :2371`                                                         | OPEN.                                                                                                                                            |
| Q1, Q2                   | (deferred)         | `crates/sandbox/src/snapshot_aead.rs` (DEK derivation); GCS x-goog-hash retry path       | OPEN — pre-existing deferred items unchanged this round.                                                                                         |

## Cross-lens consensus

- **r27-S1 → r28-carry-S1 downgrade**: architecture / api-surface
  r28 reads should see this as Guard A's loopback enforcement now
  blocking the misconfig vector unconditionally. Concurrency lens:
  the boot-time `validate_nomad_addr_loopback` is single-threaded
  (called from `SandboxConfig::from_env` before backend
  instantiation) — no race surface. Code-quality: the validator is
  ~80 LOC with clear scheme-strip + host-parse + IpAddr-loopback
  check; new unit tests at `config.rs:1527-1635` pin the matrix.
- **T5 closure consensus**: the security review verifies signing-
  key handling cleanly. Architecture (per r28) should see the T5
  fingerprint as the partial-rollout-skew detector it was designed
  to be — additive over the existing signed-/version trust anchor
  (`wait_for_agent_livez`). Test-coverage: `ce218860` pins the
  Mismatch → `AgentVersionMismatch` failure path.
- **R26-API2 / `/metrics` consensus**: api-surface (per the new
  Prometheus exposition) and security agree the endpoint is wire-
  clean. Label values are structurally bounded to `&'static str`;
  there is no operator/tenant input path into the metric body. The
  composite-r1 `metrics_503_when_no_admin_tokens_configured` test
  pins the fail-CLOSED auth shape.
- **R27-API2 lens-split agreement**: security MINOR (no live wire
  surface), api-surface IMPORTANT (hygiene + future-regression
  block). Fix-shape `#[cfg(any(test, feature = "test-support"))]`
  agreed by both lenses; post-cutover priority.
- **r27-M2 partial closure observation**: code-quality r28 may
  want to re-open the `extract_failed_task_event_msgs` truncation
  panic surface as a separate MINOR (it's a sibling bug to the
  Latin-1 cast that was closed at `821cc9bd`, not the same bug).
  Logging here for cross-lens visibility.

## Lens hand-off

- **Sandbox controller (Rust)** — primary security delta this
  round is the **CLOSED r27-S1 IMPORTANT via Guard A landing**.
  No new IMPORTANT carries. Remaining post-cutover security work:
  - **Guard B** (r28-carry-S1; ~30 LOC) — node-id cross-check via
    `GET /v1/node/<node_id>`; closes vector (1) modulo a fully-
    Nomad-compromised local agent. Defense-in-depth; not blocking.
  - **r28-carry-M1** (~5 LOC) — apply the existing
    `is_char_boundary` walk-back pattern from
    `sanitize_error_message:863-864` to the
    `extract_failed_task_event_msgs` truncation cap at
    `nomad_ch.rs:2890-2901`. Latent panic surface; bounded by
    takeover-sweep recovery.
  - **r28-carry-API2** (~2 LOC + Cargo.toml) — `cfg`-gate
    `_test_inject_sandbox`. Cross-lens (api-surface IMPORTANT).
  - **r28-M1** (~3 LOC) — strip control chars from T5 body
    excerpt before WARN-emission OR drop the body excerpt
    entirely. Cosmetic / log-hygiene.

- **Ops / cluster bring-up** — **R13-S1 (`--scopes=storage-rw`)
  remains the sole pre-cutover Ops blocker**. R20-S3 closure
  raised the "tampered driver binary" bar; Guard A closure
  removed the `nomad_addr`-misconfig blast radius. The next layer
  is removing the runtime write-scope to the artifact bucket
  (cosign-signed binaries shipped in the boot image).

- **nomad-driver-ch maintainers (cross-worktree)** — v19 landed.
  Driver-side carries R19-S1 / R20-S2 / R21-S2 unchanged across
  v15→v17→v18→v19 bumps. r5-A OFD-probe driver work (T-8b-stress-r6)
  is a different driver-axis fix; doesn't address the controller-
  audited carries.

- **Forward-looking (BackendFailureDetail ADR per r27 Focal #6)** —
  still not landed at HEAD `5a0647c3`. Greppable: zero
  `BackendFailureDetail` matches in `crates/sandbox/src/`. The
  defensive-design recommendation in r27 Focal #6 stands — when
  this lands, route through `Display → sanitize_error_message`
  for the wake-machine error_message column write. Mode A's
  whitelist (now five roots + hyphenated UUID + typed-IDs) is the
  end-of-pipeline anchor.

- **CI (r28-carry-M5 / r27-M5 / r26-S2)** — unchanged. Process-side.

## Counts

- CRITICAL: 0 new; carry: 0.
- IMPORTANT: 0 new. Carries: R21-S1, R21-S2, R20-S2, R19-S1,
  R13-S1, R9-S3. **r27-S1 promoted closed-with-residual** (Guard A
  landed; vector (2) closed; vector (1) downgraded to
  r28-carry-S1 MINOR).
- MINOR: 1 new (**r28-M1** T5 WARN body excerpt). Re-flagged /
  downgraded carries: r28-carry-S1, r28-carry-M1, r28-carry-API2,
  r28-carry-M3 (=r27-M3), r28-carry-M4 (=r27-M4). Policy carry:
  r28-carry-M5 (=r27-M5).
- Closed this round: **r27-S1 Guard A** at `069dd277` (vector 2);
  **r27-M1** at `4f0f2259` (whitelist + hyphenated UUID);
  **r27-M2 Latin-1-cast class** at `821cc9bd` (note: original
  truncation panic surface is re-flagged as r28-carry-M1).
- LATENT-IMPORTANT: 0.
- Total NEW this round: 0 IMPORTANT, 1 MINOR.
- **Cutover gate**: **R13-S1 (storage-rw scope)** remains the
  SOLE pre-cutover Ops blocker. **Controller-side pre-cutover is
  CLEAR** of IMPORTANTs; r28-carry-S1 (Guard B) is the next-
  highest-leverage post-cutover security fix.
