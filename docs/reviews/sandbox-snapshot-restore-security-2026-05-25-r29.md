# Sandbox/snapshot-restore — security r29 review

Date: 2026-05-25 (UTC)
HEAD at audit: `ce062846`. Clean worktree.
Predecessor: r28 at `5a0647c3` (`docs/reviews/sandbox-snapshot-restore-security-2026-05-25-r28.md`). Lens: security (READ-ONLY).

Landings since r28 (sandbox/ + scripts/ scope only):

- `46d1f692` — docs/deferred backlog close (r24-A2 substrates / r7-B). Out of security scope.
- `c969b94d` — sandbox/nomad-ch: `VmIndexAllocator::release` delay (r24-A2-S3). Re-verified at r28; carry.
- `086971d2` — sandbox/scripts: driver pin v18→v19 (T-8b-driver-v19-upload). Carry.
- `2468ab96` — docs/reviews: T-8b-stress-r9 BLOCKED ABORT artefact. Out of scope.
- `5a0647c3` — docs/deferred: STRESS-R9-RETRY entry. Out of scope.
- `2c0e7f… → 9e1f6276` — sandbox/nomad-ch: inline `vm_index` release delay in `CreateGuard::drop` (R28-C1). Internal control-flow fix; no security delta. Cross-ref security audit: spawn-on-current-runtime trap closes the leak surface but adds no new wire/auth surface.
- `57417911` — docs: close R28-C1 in deferred backlog. Out of scope.
- `c2e07b2f` — sandbox/nomad-ch: gate `_test_inject_sandbox` under `#[cfg(any(test, feature = "test-support"))]` (R27-API2 closure). **Security closure**: r28-carry-API2 surface stripped from production binaries; verified `cargo build -p zeroship-sandbox` no longer links the symbol per api-surface r28 R28-API-VERIFY1.
- `b759c82c` — docs: close R27-API2 in deferred backlog. Out of scope.
- `de7465ac` — pilot artefacts (security r28, concurrency r28, test-coverage r29). Out of scope.
- `def11cb4` — docs/reviews: T-8b-stress-r9 RED + heredoc-leak. Cross-ref: STARTUP-HEREDOC-LEAK is the upstream of r29-A1 architecture flag.
- `a3cfca10` — **sandbox/scripts: escape backticks in `zsbx-ctl.service` heredoc** (STARTUP-HEREDOC-LEAK fix). Closes one instance of the class flagged by r29-A1.
- `baf1b78c` — docs/reviews: T-8b-stress-r9-retry-2 ABORT. Out of scope.
- `508c3d76` — sandbox/scripts: default `EXTRA_WORKER_METADATA=install-ch-plugin-driver=1`. Operator-config; no security delta.
- `d00f12dd` — **sandbox/wake-machine: parallelize T5 + clock_resync via `futures::join!` + half-dead-agent detection** (R28-I1 + R28-I2). **Primary focal commit this round.**
- `ce062846` — pilot: round-41 reviewer artefacts (architecture r29 / performance r28 / api-surface r28). HEAD.

## Summary

**r29 produces ZERO new CRITICALs, ZERO new IMPORTANTs, ZERO new MINORs.**

The primary delta this round is R28-I1+I2 (`d00f12dd`) — the parallel-probe + half-dead-agent fingerprint landing. Security review verifies the new attack surface as **CLEAN**: the half-dead-agent detector requires BOTH `/version` and `/_clock_resync` to transport-fail (closed port / TCP timeout / DNS), the transport-error signal is constructed by **controller-side `ureq::Error`** (not by agent-controlled response body), and a tenant-RCE'd agent that tries to trigger the path achieves only self-DoS — the same shape as the pre-R28-I1 baseline (a non-detection wake would still roll back via the existing serial-failure path, just slower). **No amplification**, no new wire surface, no new auth surface, no new leak surface.

Cross-lens deltas reviewed:

- **api-surface r28 R28-API2 (5 test-scaffolding `pub` items)**: from the security lens, only `Database::set_role_dsns_for_test` carries a *security shape* of attack (silent role-DSN override). Audit at HEAD: **no live wire surface** — `Database` is held inside `AppState` as `Arc<Database>` (`lib.rs` ownership), so obtaining the `&mut Database` receiver `set_role_dsns_for_test` requires is structurally impossible from any handler. Defense-in-depth concern only; cross-lens carry as **r29-carry-API2** below at security-lens MINOR. The other 4 items (`from_test_config`, `StubRestoreBackend`, `StubSourceVmOps`, `RecordingIdleSnapshotter`) are pure test scaffolding without security shape; defer to api-surface lens IMPORTANT for hygiene.
- **architecture r29 r29-A1 (11 heredoc class instances)**: from the security lens, the worry is **command-substitution + variable-expansion in unquoted bash heredocs that splice operator-controlled metadata values into systemd unit bodies + env files**. Audit at HEAD: the heredoc bodies that expand operator-controlled metadata (`$SANDBOX_TOKEN`, `$PG_PASSWORD`, `$SANDBOX_ADMIN_TOKEN`, `$DATACENTER`, `$VM_INDEX_CEIL`, `$SNAPSHOT_BUCKET`, `$ART`) ALL trust the worker-host GCP-metadata channel — the same boundary as the binary itself. **No NEW attack surface**, but the class hygiene point (r29-A1) carries cross-lens weight. Filed as **r29-carry-HEREDOC** below at security-lens MINOR.
- **R28-I1+I2 transport-error signal construction**: structurally safe. The `message.starts_with("/_clock_resync transport:")` heuristic is fed by the controller's own `format!("/_clock_resync transport: {e}")` on `ureq::Error` (non-Status, non-Body) — agent response body NEVER reaches this branch (non-200 → `ureq::Error::Status(code, r)` → `"/_clock_resync status {code}: {body_excerpt}"`, distinct prefix). The detector cannot be tripped by a malicious agent's response payload.

Net carry status (vs r28):

- **r28-M1** (T5 `/version` non-200 256-char body excerpt at WARN) — **unchanged at HEAD**; verified `restore_handler.rs:3308-3318` carries the same 256-char `body.chars().take(256).collect()` slice. Bounded char-boundary-safe; journald-only; no wire surface. MINOR carry.
- **r28-carry-S1** (Guard B node-id cross-check still missing) — **unchanged**; `fetch_local_nomad_node_id` at `nomad_ch.rs:3318-3332` unchanged. Vector (1) tampered-local-agent bounded by Guard A loopback enforcement + R20-S3 SHA-pin. MINOR carry.
- **r28-carry-M1** (`extract_failed_task_event_msgs` byte-truncation UTF-8 panic) — **unchanged**; `nomad_ch.rs:2890-2901` truncation cap still byte-indexed without `is_char_boundary` walk-back. MINOR carry.
- **r28-carry-API2** (`_test_inject_sandbox` cfg-gate) — **CLOSED at `c2e07b2f`**. Verified via api-surface r28 R28-API-VERIFY1 (nm count = 0 in `cargo build` without `--tests`). NEW cross-lens carry **r29-carry-API2** is the *next* layer (5 sibling items per R28-API2); security-lens highlights `set_role_dsns_for_test` as the highest-shape candidate but ratifies as defense-in-depth only (no live wire surface).
- **r28-carry-M3 / -M4 / -M5** — unchanged carries.
- **R22-S1 / R20-S3 / R27-S1 Guard A** — CLOSED carries; re-verified at HEAD.
- **R21-S1 / R21-S2 / R20-S2 / R19-S1 / R13-S1 / R9-S3** — unchanged IMPORTANT carries.

**Cutover gate status**: **R13-S1 (`--scopes=storage-rw` on worker GCE SA) remains the SOLE pre-cutover Ops blocker**. Controller-side pre-cutover is CLEAR of IMPORTANTs.

## CRITICAL

None.

## IMPORTANT

None.

## MINOR

### [r29-FOCAL-I12 OK] R28-I1+I2 parallel-probe + half-dead-agent fingerprint — security-CLEAN audit

- **Status**: **CLEAN** (NO new finding). Filed as security verification for the brief's focal question on attack-surface amplification.
- **Files**:
  - `crates/sandbox/src/restore_handler.rs:3035-3099` (new `ClockResyncOutcome` enum + `clock_resync_post_restore_typed` wrapper)
  - `crates/sandbox/src/restore_handler.rs:3134-3185` (existing `VersionCheckOutcome::Skipped` extended with `transport_error: bool` + `is_transport_error()` accessor)
  - `crates/sandbox/src/wake_machine.rs:487-638` (parallel `futures::join!` + half-dead-agent fingerprint detector + rollback message construction)

#### Audit notes — amplification surface

1. **Trigger contract**: the half-dead-agent path fires iff `t5_outcome.is_transport_error() && clock_resync_outcome` matches `Err { transport_error: true, .. }` (wake_machine.rs:547). Both halves require:
   - T5: `verify_agent_version_post_restore` returned `Skipped { transport_error: true, .. }`. The ONLY `transport_error=true` site is `restore_handler.rs:3299-3302` on `Err(e) => …` from `ureq::Error` (i.e., the agent's HTTP layer did NOT respond — no `Status(code, r)` branch).
   - clock_resync: `clock_resync_post_restore_typed` returned `Err { transport_error: true, .. }`. The ONLY site that sets this bool is the heuristic at `restore_handler.rs:3091-3092`, `message.starts_with("/_clock_resync transport:")`, which matches `restore_handler.rs:3028`'s `Err(e) => Err(format!("/_clock_resync transport: {e}"))` — again, the `Err(e)` branch of ureq, NOT the `ureq::Error::Status` branch.

2. **Agent-controllability**: a tenant-RCE'd agent (the only attacker inside the per-sandbox 10.99.<100+idx>.2 IP) controls:
   - Whether to respond at all (closed socket → both probes get `ureq::Error` transport → both `transport_error=true` → half-dead fires).
   - The HTTP response body content on a 200 / non-200 (NEVER reaches `transport_error=true` because ureq parses the status line first → `Status(code, r)` branch).
   - The HTTP response status code (same — never `transport_error=true`).

   **The agent CANNOT trip `transport_error=true` via response content** — only by refusing to respond. The heuristic check `starts_with("/_clock_resync transport:")` is fed by a controller-side `format!` over a `ureq::Error` Display impl; the agent's body excerpt would land in the SIBLING branch `format!("/_clock_resync status {code}: {body_excerpt}")` (distinct prefix `"/_clock_resync status "`), which the heuristic does NOT match.

3. **Outcome on a tripped detector**: `wake_machine.rs:562-573` rolls back with `WakeErrorCode::ClockResyncFailed` and an error message of shape:

   ```text
   half_dead_agent: both /version and /_clock_resync transport-failed
   against http://10.99.<100+idx>.2:7000 (clock_resync: /_clock_resync transport: <ureq::Error display>)
   ```

   - The format includes `agent_url` (per-sandbox RFC1918 `10.99.<100+idx>.2:7000`) verbatim. The rollback message enters `sanitize_error_message` at `wake_machine.rs:165` before the pg `wake_jobs.error_message` write — and the post-r27-M1 sanitize whitelist (`wake_machine.rs:1182-1188`) strips RFC1918 hosts via `strip_ip_ports`. No leak via the pg column.
   - The unsanitized message ALSO routes to `tracing::warn!` at `wake_machine.rs:166-172` (journald only, RO-bearer surface unaffected). Operator-trust boundary; same shape as the existing R22-S1 verbatim-WARN.
   - The `target: "sandbox::wake::half_dead_agent"` is a stable static string (`&'static str`), label values structurally bounded — Prometheus exposition impossible since no counter on this target.

4. **Amplification vs. pre-I1+I2 baseline**:
   - **Pre-R28-I1**: serial T5 (10s timeout) → serial clock_resync (10s timeout). On a half-dead agent, total wake stall = 20s, then ClockResyncFailed rollback with `clock_resync transport: …`. Same wake-status outcome (Failed); same wire code (`ClockResyncFailed`); same sanitization pipeline; just slower.
   - **Post-R28-I1+I2**: parallel probes (max 10s), distinct WARN target, ClockResyncFailed rollback with `half_dead_agent:` prefix. Same wake-status outcome; same wire code; same sanitization pipeline; **faster failure**.
   - **Net delta from attacker's view**: half-dead-agent costs them ~10s instead of ~20s of self-DoS. From the controller's view: tighter SLO-dashboard signal + half the wake-budget burn under the failure path.

   **No amplification**. The "faster failure" delta is a defender win, not an attacker leverage point — a tenant who already controls their VM can DoS their own wake at lower cost than the controller's old serial path, but that costs them nothing they didn't already have (their VM was their VM).

5. **Cancel-safety + ordering audit**:
   - `futures::join!` awaits both futures to completion before yielding (`wake_machine.rs:529-530`). No `select!`; no early-return; no race between the two probes' completion order.
   - Both futures take `&[u8; 32]` (signing_key_bytes) and `&str` (agent_url) by borrow; these live on the wake-machine stack across the await (`wake_machine.rs:484-485, :518-528`). The `SealedAuth.signing_key_bytes` is read TWICE (once per probe) but never mutated; no data race.
   - The half-dead detection happens AFTER `join!` returns, so both probes' outcomes are fully materialized before the WARN/rollback decision (`wake_machine.rs:539-574`). No TOCTOU.
   - The half-dead detector is the FIRST branch on the joined results; if it fires, the wake rolls back and the T5-Mismatch/clock-resync-Err branches are never entered. Correct ordering: the half-dead case is a SUPERSET of the clock-resync-Err case (when both `transport_error=true`); the half-dead branch's distinct WARN + rollback message take precedence.

6. **`derive_agent_url` audit**: `wake_machine.rs:485` `let agent_url = self.backend.derive_agent_url(snap.vm_index);` — `vm_index` is the `u16` field of the snapshot row (validated at restore time via `VmIndexAllocator::reserve`, range `[1, 155]`). The derivation produces a fixed-shape URL `http://10.99.<100+idx>.2:7000`; no operator/tenant input enters the URL. Safe.

**Conclusion**: R28-I1+I2 lands security-clean. The structured boolean contract is non-forgeable from the agent side; the half-dead detector cannot be triggered for amplification beyond the self-DoS the tenant already has via their own VM. No new MINOR.

### [r29-carry-API2] `Database::set_role_dsns_for_test` is `pub fn` with security shape but no live wire surface

- **Status**: **CARRY** (downgraded from api-surface r28 R28-API2 IMPORTANT). Security-lens MINOR because the function is structurally unreachable from any handler at HEAD.
- **File**: `crates/sandbox/src/db.rs:548-556`
- **Quote**:
  ```rust
  /// Override the per-role DSNs after `from_test_config`. Used by
  /// the role-permission integration tests to exercise the actual
  /// `sandbox_app` / `sandbox_audit` / `sandbox_gdpr` connection
  /// paths against a CI Postgres where each role exists.
  #[doc(hidden)]
  pub fn set_role_dsns_for_test(&mut self, audit: String, gdpr: String) {
      self.config.dsn_audit = audit;
      self.config.dsn_gdpr = gdpr;
  }
  ```
- **Threat model — security lens**:
  - The function takes `&mut self` and overwrites `dsn_audit` + `dsn_gdpr`. If reachable from a handler, an attacker could silently re-point the audit-log writer + GDPR DELETE pool at an attacker-controlled Postgres — i.e., suppress / mirror audit rows or coerce wrong-tenant GDPR deletes against a phantom database.
  - **Reachability audit at HEAD**:
    - 0 `src/` call sites (verified via `grep -rn set_role_dsns_for_test crates/sandbox/src/`).
    - 0 `tests/` call sites either (only 3 doc-comment mentions at `sandbox_pg_e2e.rs:5739, :5937, :5939`). The function is **genuinely DEAD test scaffolding**.
    - `AppState` holds `Arc<Database>` (no `&mut`); all production handlers receive `Arc<AppState>`. Obtaining `&mut Database` would require `Arc::get_mut` after dropping all clones, which is unreachable from the request path (the controller holds an `Arc<AppState>` for the lifetime of the process).
  - **Residual concern**: defense-in-depth shape only. The `pub` visibility + `#[doc(hidden)]`-only gate means a future PR could:
    1. Add an `&mut Database` method to a handler (compiler would not catch the `set_role_dsns_for_test` exposure — the function appears in scope).
    2. Accidentally call `set_role_dsns_for_test` from a non-test path (the `pub` visibility makes it indistinguishable from production API in rustdoc-search results when not viewed through the `#[doc(hidden)]` lens).
  - Compare with `_test_inject_sandbox` at `nomad_ch.rs:1699-1721` — **CLOSED at `c2e07b2f`** with the same `#[cfg(any(test, feature = "test-support"))]` gate; the precedent + infrastructure are in place for the same fix here.
- **Why MINOR (security lens) not IMPORTANT**:
  - No live wire surface today (audit confirms 0 callers).
  - The api-surface lens already classifies this IMPORTANT for hygiene; security defers to api-surface for the priority weighting per the lens-split agreement.
  - If a future PR DOES land a path that reaches this function, severity escalates to CRITICAL because audit / GDPR DSN substitution is a structural integrity breach (mirrors the `_test_inject_sandbox` security shape — runtime state override via a test-only hook).
- **Fix shape** (NOT prescribing; tracked under api-surface r28 R28-API2):
  - `#[cfg(any(test, feature = "test-support"))]` + `pub(crate)` per the R27-API2 closure precedent. ~2 LOC. Strips the symbol from production binaries entirely.
- **Severity**: **MINOR (security lens; api-surface r28 R28-API2 IMPORTANT)** — defense-in-depth; no live exploit; cross-lens agreed fix shape.

### [r29-carry-API2-siblings] 4 sibling test-scaffolding `pub` items (`from_test_config`, `StubRestoreBackend`, `StubSourceVmOps`, `RecordingIdleSnapshotter`)

- **Status**: **CARRY** (cross-lens from api-surface r28 R28-API2). Security-lens **below MINOR** because none of the four has a security shape of attack.
- **Files / quotes** (per api-surface r28 R28-API2):
  - `db.rs:518` — `pub async fn Database::from_test_config(...)` — bypasses migrations + boot-fail-fast. `validate_dsn_scheme` IS in place at `db.rs:523`. Security shape: a caller could construct a `Database` against an attacker-controlled DSN, but `from_test_config` returns `Database`, not `Arc<Database>`; integrating it into `AppState` would require an explicit code path. No live surface.
  - `restore_handler.rs:1302-1323` — `pub struct StubRestoreBackend { … }` — unit-test impl of the `RestoreBackend` trait. Public fields (`pub fail_reserve: bool` etc.) could let a future non-test caller flip failure modes in production. Pure substitution-of-impl concern; no auth / wire / leak shape.
  - `snapshot_handler.rs:691` — `pub struct StubSourceVmOps` — symmetric.
  - `sweep.rs:492` — `pub struct RecordingIdleSnapshotter` — symmetric.
- **Threat model — security lens**: none of the four has a `signing_key` / `dsn` / `agent_url` injection shape (the lens R27-API2 + r29-carry-API2 used). The trait-stub structs WOULD enable a "silent fault-injection in production" pattern if their failure-mode flags were ever wired to a non-test path; but the structs are not constructed anywhere outside `tests/`. Defense-in-depth → below the MINOR security bar.
- **Severity**: **DEFER TO API-SURFACE** (no security-lens delta; api-surface IMPORTANT carries the hygiene weight).

### [r29-carry-HEREDOC] 11 unquoted bash heredocs splice operator-controlled GCP metadata into systemd unit + env files — same operator-trust boundary, no NEW security surface

- **Status**: **CARRY** (cross-lens from architecture r29 r29-A1 CRITICAL). Security-lens **MINOR** because the heredoc inputs trust the SAME boundary (GCP project metadata) as the binary itself.
- **Files** (per architecture r29 r29-A1):
  - `crates/sandbox/scripts/gcp-server-startup.sh:129` — nomad.hcl server config; `$DATACENTER`, `$PRIVATE_IP`.
  - `crates/sandbox/scripts/gcp-worker-startup.sh:216` — plugin-dir.hcl; literal.
  - `crates/sandbox/scripts/gcp-worker-startup.sh:287` — sysctl.d; literal.
  - `crates/sandbox/scripts/gcp-worker-startup.sh:305` — taps-up.sh; `$VM_INDEX_CEIL` (operator metadata).
  - `crates/sandbox/scripts/gcp-worker-startup.sh:321` — taps service unit; literal.
  - `crates/sandbox/scripts/gcp-worker-startup.sh:352` — nomad.hcl client config; `$DATACENTER`, `$PRIVATE_IP`, `$RETRY_JOIN`.
  - `crates/sandbox/scripts/gcp-worker-startup.sh:439` — **`sandbox-token.env` env-file with `SANDBOX_TOKEN=$SANDBOX_TOKEN`**.
  - `crates/sandbox/scripts/gcp-worker-startup.sh:445` — **`sandbox-db.env` env-file with `SANDBOX_DATABASE_URL=postgres://postgres:$PG_PASSWORD@$PG_HOST:5432/zeroship`**.
  - `crates/sandbox/scripts/gcp-worker-startup.sh:511` — **zsbx-ctl.service systemd unit** with many `Environment=…=$VAR` lines including `$DATACENTER`, `$VM_INDEX_CEIL`, `$ART`, `$AEAD_KEY_PATH`, `$ROOT_KEK_PATH`, `$SNAPSHOT_BUCKET`. THIS round's fix at `a3cfca10` escaped three backticks in COMMENT lines (`\`nomad_ch.rs:797\``); the security-relevant **EXPANSION** is in the `Environment=…=$X` lines.
  - `crates/sandbox/scripts/provision-gcp-cluster.sh:338` — usage-help; literal.
  - `crates/sandbox/scripts/bake-rootfs.sh:46` — bake-rootfs; literal.
- **Threat model — security lens**:
  - **Variable-expansion injection** (e.g., a `$SANDBOX_TOKEN` value containing `` ` `` or `$(...)` would EXECUTE during heredoc evaluation under unquoted `<<EOF`): the values come from `md install-ch-plugin-driver` / `gen_token` / `$PG_PASSWORD_FILE` etc. `gen_token` produces base64-charset (`a-zA-Z0-9+/`) — no shell-metachar risk. `md $X` reads from GCP project metadata — controlled by whoever has `compute.instanceAdmin` on the project, which is the SAME boundary as the binary itself (whoever can change metadata can also replace `controller-object` with a malicious binary). **No NEW attack surface**.
  - **Comment-line backtick** (architecture r29 r29-A1's specific class instance): the fix escaped this round; the next comment-line addition referencing `\`some_path.rs:N\`` would re-introduce it. Operator-trust boundary intact; the bug was a self-DoS (zsbx-ctl unit emission aborted → controller didn't start), not a tenant exfil.
  - **Newline / `EOF` injection**: a metadata value containing `\nEOF\n` would prematurely close the heredoc. For `$PG_PASSWORD` (`gen_token` base64) the charset excludes `\n`; for operator-typed `SANDBOX_TOKEN`, the gen_token default also excludes; operator-set values with embedded newlines would surface as boot-time failures (env-file parse error), not a covert injection. **No NEW attack surface**.
  - **Specific high-value lines audited**:
    - `sandbox-token.env:440` — `SANDBOX_TOKEN=$SANDBOX_TOKEN`. If operator-set token contains shell-metachars: heredoc executes them in the worker host context (root). **Operator-trust boundary; same root-equivalent boundary as the binary**.
    - `sandbox-db.env:446` — `SANDBOX_DATABASE_URL=postgres://postgres:$PG_PASSWORD@$PG_HOST:5432/zeroship`. Same boundary; `PG_PASSWORD` from `gen_token` is safe under the default flow.
- **Why MINOR (security lens) not IMPORTANT**:
  - Inputs ARE operator-controlled but the operator IS root-equivalent on the worker host (their startup script runs as root).
  - The architecture r29 r29-A1 CRITICAL framing is about **operational reliability** (self-DoS via misformed unit body) not about tenant exfiltration or auth bypass. Security lens defers to architecture for the priority weighting.
  - The systemd-drop-in refactor architecture r29 recommends (Option 1) would ALSO close the security surface (variable expansion only ever occurs against quoted-EOF heredocs), giving cross-lens reinforcement.
- **Fix shape** (NOT prescribing; tracked under architecture r29 r29-A1):
  - **Option A** (defense-in-depth, ~5 LOC): for the env-file heredocs at `:439-441` and `:445-447`, switch to `printf '%s=%s\n' SANDBOX_TOKEN "$SANDBOX_TOKEN" > "$ART/sandbox-token.env"`. Eliminates ALL shell expansion against the token value.
  - **Option B** (architecture r29 r29-A1 Option 1, ~40 LOC): systemd drop-ins with quoted-EOF heredocs + checked-in unit fixtures. Architecturally cleaner; security gets the same outcome.
- **Severity**: **MINOR (security lens; architecture r29 r29-A1 CRITICAL)** — operator-trust boundary; no NEW attack surface introduced by the heredoc pattern beyond what the operator already controls; cross-lens fix shape aligned.

### [r29-carry-r28-M1] T5 `verify_agent_version_post_restore` 256-char body excerpt at WARN — unchanged at HEAD

- **Status**: **CARRY** (unchanged since r28-M1).
- **File**: `crates/sandbox/src/restore_handler.rs:3308-3318`.
- **Severity**: **MINOR** — journald-only; bounded char-boundary-safe slice; operator-trust boundary.

### [r29-carry-S1] r27-S1 Guard B node-id cross-check still not landed

- **Status**: **CARRY** (unchanged since r28-carry-S1).
- **File**: `crates/sandbox/src/backend/nomad_ch.rs:3318-3332`.
- **Severity**: **MINOR (carry)** — bounded by Guard A loopback + R20-S3 driver SHA-pin.

### [r29-carry-M1] `extract_failed_task_event_msgs` byte-indexed truncation panic surface unchanged

- **Status**: **CARRY** (unchanged since r28-carry-M1).
- **File**: `crates/sandbox/src/backend/nomad_ch.rs:2890-2901`.
- **Severity**: **MINOR (carry)** — driver-side input is admin-trust-equivalent; recovery via takeover-sweep.

### [r29-carry-M3 / -M4 / -M5] r27/r28 ongoing carries

- **r29-carry-M3** (`admin_handlers.rs:1988` — wake-poll terminal=ok agent_url RFC1918 leak) — unchanged.
- **r29-carry-M4** (`admin_handlers.rs:1515` — snapshot success artifact_path; Full-bearer only) — unchanged.
- **r29-carry-M5** (`.github/workflows/ci.yml` — r24-A3 ADR not CI-enforced) — unchanged; POLICY.

## Focal-list checks (per r29 brief)

### Focal #1 — R28-I1+I2 parallel + half-dead-agent amplification surface

**Status**: **CLEAN**. See [r29-FOCAL-I12 OK] above. The half-dead-agent fingerprint is structurally non-forgeable from agent-controlled response content (the `transport_error: bool` is set only on controller-side `ureq::Error` Display-formatted strings, not on agent-supplied response bodies). A tenant who tries to trigger the path achieves only their own VM's self-DoS at lower cost than the pre-R28-I1 baseline (10s vs 20s). No new wire surface, no new auth surface, no new leak surface.

### Focal #2 — api-surface r28 R28-API2 5 test-scaffolding leaks — wire vs. cleanup debt classification

**Status**: see [r29-carry-API2] + [r29-carry-API2-siblings] above. Of the 5 items:

| Item | Security shape | Live wire surface? | Severity (security lens) |
|---|---|---|---|
| `Database::set_role_dsns_for_test` | YES — audit/GDPR DSN substitution | NO (no `&mut Database` in handlers; `AppState` holds `Arc<Database>`) | MINOR (carry; defense-in-depth) |
| `Database::from_test_config` | LIMITED — bypasses migrations + boot-fail-fast | NO (0 `src/` callers) | below MINOR (cleanup debt) |
| `StubRestoreBackend` | LIMITED — fault-injection flags | NO | below MINOR |
| `StubSourceVmOps` | NONE — pure trait stub | NO | below MINOR |
| `RecordingIdleSnapshotter` | NONE — pure trait stub | NO | below MINOR |

**Highest-shape item is `set_role_dsns_for_test`**: silent audit/GDPR DSN override would let an attacker re-point audit logs at a phantom DB (suppression) or coerce wrong-tenant GDPR DELETE traffic at a sentinel DB. Both are integrity breaches, not exfiltration. Bounded today by structural unreachability (no `&mut Database` paths in handlers); CRITICAL escalation iff a future PR exposes one.

### Focal #3 — r29-A1 11 heredocs — env-var / command injection surface

**Status**: see [r29-carry-HEREDOC] above. **No NEW security surface**. The 11 heredocs splice operator-controlled metadata into systemd units + env files; all sources (`gen_token`, GCP metadata) are within the operator-trust boundary already required to deploy the controller binary. The specific class instance fixed at `a3cfca10` was a self-DoS (boot abort), not an exfil shape. Cross-lens reinforcement: the architecture r29 r29-A1 Option 1 fix (quoted-EOF heredoc + drop-in unit body) ALSO closes the security defense-in-depth surface — implementing it once buys both reliability and security improvement.

**Highest-shape heredocs** (rank-ordered by security blast radius if compromised):

1. `gcp-worker-startup.sh:439-441` (`sandbox-token.env`) — `SANDBOX_TOKEN=$SANDBOX_TOKEN`. Pre-shared secret for the controller's primary bearer surface. Operator-trust boundary.
2. `gcp-worker-startup.sh:445-447` (`sandbox-db.env`) — `SANDBOX_DATABASE_URL` with `$PG_PASSWORD` embedded. Database credential. Operator-trust boundary.
3. `gcp-worker-startup.sh:511-616` (`zsbx-ctl.service`) — many `Environment=…=$X` lines including `$AEAD_KEY_PATH`, `$ROOT_KEK_PATH`, `$ART/sandbox-admin-token`. Paths to crypto material. Operator-trust boundary.

For each, the input source (`gen_token` / GCP metadata) is under the operator's GCP project IAM control. A compromise of that boundary already implies controller-binary compromise (since the operator also chooses `controller-object`).

### Focal #4 — refresh prior IMPORTANT/MINOR carries

**Re-verified at HEAD `ce062846`**:

- **R22-S1** sanitize_error_message ROOTS whitelist — unchanged at `wake_machine.rs:1182-1188`. CLOSED carry.
- **R20-S3** driver SHA-pin — `gcp-worker-startup.sh:187-199` carries v19 SHA. CLOSED carry.
- **r27-S1 Guard A** `validate_nomad_addr_loopback` — `config.rs:484-561, :642` unchanged. CLOSED carry.
- **r27-M1** sanitize whitelist + hyphenated UUID — `wake_machine.rs:1182-1188, :1331` unchanged. CLOSED carry.
- **r27-M2** Latin-1 cast in 6 sites — `wake_machine.rs:1054` unchanged. CLOSED carry.
- **R21-S1 / R21-S2 / R20-S2 / R19-S1 / R13-S1 / R9-S3** — unchanged IMPORTANT carries.

## Cross-lens consensus

- **R28-I1+I2 cross-lens read**: architecture r29 (covered separately under cycle 41's architecture r29) reads this as a parallelism + half-dead-agent fingerprint with sound cancel-safety. Concurrency r28 (cycle 40) had already greenlit the parallel-probe shape pre-landing. Performance r28 reads the latency improvement as wake-failure-path budget recovery (10s vs 20s on a half-dead-agent failure). Test-coverage r29 (cycle 40) ratified the 5 new tests + existing test updates. Security r29 (this review) verifies no new attack surface.
- **R28-API2 cross-lens consensus**: api-surface r28 IMPORTANT (hygiene + future-regression block); security r29 MINOR (no live wire surface, defense-in-depth only) for `set_role_dsns_for_test`; below-MINOR for the other 4 items. Fix shape `#[cfg(any(test, feature = "test-support"))]` agreed across lenses; same precedent as `_test_inject_sandbox` closure at `c2e07b2f`.
- **r29-A1 cross-lens consensus**: architecture r29 CRITICAL (class-level fix needed; recurrence cost = real $ per cluster cycle); security r29 MINOR (operator-trust boundary; no NEW security shape). The systemd-drop-in refactor architecture r29 recommends closes BOTH lenses' concerns — security gets defense-in-depth (no shell expansion against secret-bearing values), architecture gets class-level fix.
- **r28-M1 unchanged carry**: code-quality / architecture lenses have not picked up the body-excerpt strip recommendation; security stands by MINOR-deferred.

## Lens hand-off

- **Sandbox controller (Rust)** — security pre-cutover is CLEAR of IMPORTANTs. Post-cutover work:
  - **Guard B** (r29-carry-S1; ~30 LOC) — unchanged from r28.
  - **r29-carry-M1** (~5 LOC) — unchanged from r28.
  - **r29-carry-API2 `set_role_dsns_for_test` cfg-gate** (~2 LOC) — same precedent as `c2e07b2f`; mechanical fix; reduces defense-in-depth exposure. Cross-lens api-surface r28 R28-API2 IMPORTANT.
  - **r28-M1** (~3 LOC) — strip control chars from T5 body excerpt OR drop the excerpt. Cosmetic / log-hygiene.

- **Ops / cluster bring-up** — **R13-S1 (`--scopes=storage-rw`) remains the SOLE pre-cutover Ops blocker**. New cross-lens hand-off: **r29-A1 heredoc class** — architecture r29 recommends Option 1 (systemd drop-ins) as the structural fix; security agrees this closes a defense-in-depth surface (variable-expansion in unquoted env-file heredocs). Block stress-r10 on either the architecture-recommended Option 1 landing OR the lint.sh severity-gate promotion (architecture r29 Option 2).

- **nomad-driver-ch maintainers (cross-worktree)** — v19 landed at `086971d2`. Driver-side carries R19-S1 / R20-S2 / R21-S2 unchanged. No security-lens action required this round.

- **Forward-looking (BackendFailureDetail ADR per r27 Focal #6)** — still not landed at HEAD `ce062846`. The defensive-design recommendation stands.

- **CI (r29-carry-M5)** — unchanged. Process-side.

## Counts

- CRITICAL: 0 new; 0 carry.
- IMPORTANT: 0 new. Carries: R21-S1, R21-S2, R20-S2, R19-S1, R13-S1, R9-S3.
- MINOR: 0 new. Carries: r29-carry-API2 (cross-lens from api-surface r28 R28-API2), r29-carry-HEREDOC (cross-lens from architecture r29 r29-A1), r29-carry-r28-M1, r29-carry-S1, r29-carry-M1, r29-carry-M3 (=r27-M3), r29-carry-M4 (=r27-M4). Policy carry: r29-carry-M5 (=r27-M5).
- Closed this round (cross-lens awareness): **R27-API2** (`_test_inject_sandbox` cfg-gate) at `c2e07b2f` — verified strips from production binaries via api-surface r28 R28-API-VERIFY1. Security r28-carry-API2 (the previous round's tracking ID for the same surface) is **resolved**.
- LATENT-IMPORTANT: 0.
- Total NEW this round: **0 IMPORTANT, 0 MINOR**.
- **Cutover gate**: **R13-S1 (storage-rw scope)** remains the SOLE pre-cutover Ops blocker. **Controller-side pre-cutover remains CLEAR** of IMPORTANTs. r29-A1 heredoc class is the next-highest cross-lens priority (architecture CRITICAL; security MINOR cross-lens reinforcement).
