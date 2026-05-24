# Sandbox/snapshot-restore — api-surface r16 review

Date: 2026-05-25 (UTC)
HEAD at audit: `792a7aa5`.
Round 16. Prior: r15 at `b8fae7b7`.
Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`, plus design
proposal `docs/proposals/c7-lt-async-wake.md` (uncommitted). Read-only.

## Summary

- **3 new findings**, all on the **C-7-LT design proposal**. PR1 has
  NOT landed yet (`git log` returns 0 commits naming `WakeResponseMode`,
  `WakeJobRow`, `WakeJobState`, `WakeErrorCode`). The api-surface lens
  pre-reviews the wire contract so PR2 lands clean.
- **0 new findings on landed code.** `b8fae7b7..792a7aa5` is one
  fail-CLOSED security fix (`da951dd9` A1-FOLLOWUP, adds 1×
  `pub(crate) fn`), one smoke-r11 review artifact, three deferred/
  closure paperwork commits, and the cycle's pilot bundle. Net `pub`
  delta: **0**.
- **4 carries open** at HEAD (R10-API1, R10-API4, R12-API1, R14-API2)
  — all re-verified live at `792a7aa5`.
- **Backlog**: 4 → 7 (3 new pre-review findings on the design
  proposal). The 3 new are advisory-blocking on PR2, not on landed
  code.
- **Prompt correction**: §10.0 envelope is **error-only** and uses
  `{"error": "<kind>", "message": "<human>", ...extra}` — NOT the
  `{"ok", "code", "data", "message"}` shape mentioned in the prompt.
  Cite: parent proposal `docs/proposals/sandbox-snapshot-restore.md:485-498`
  and production helper `crates/sandbox/src/error_envelope.rs:88-110`.

## CRITICAL

### R16-API1 — Proposal failed-state body diverges from §10.0 field names (NEW)

- **Where**: `docs/proposals/c7-lt-async-wake.md:54`.
- **Cite**: §10.0 envelope at `sandbox-snapshot-restore.md:485-498` +
  helper at `error_envelope.rs:88-110`.
- **Problem**: proposal emits `{"state":"failed", "error_code":"…",
  "error_message":"…", "failed_at":"…"}`. Field names diverge from
  every other endpoint in the crate: `error_code` should be `error`,
  `error_message` should be `message`. Today's
  `vm_index_unavailable` 503 at `admin_handlers.rs:1157-1163` emits
  `{error, message, requested}`. The proposal would create a
  second wire-shape on the same crate's HTTP surface.
- **Also**: success bodies (`wake_sandbox` happy path at
  `admin_handlers.rs:1456-1463`) are **flat** today — `{sandbox_id,
  vm_index, generation}`, no envelope wrapping. The proposal's poll
  success `{"state":"ok", "ready_at", "agent_url"}` is fine but
  should explicitly state in §2: "success bodies flat per existing
  convention; errors use §10.0."
- **Fix**: rename `error_code` → `error`, `error_message` → `message`
  on the failed-poll body. Then it's a §10.0 envelope plus
  `{"state":"failed", "failed_at"}` extras — clean reuse of
  `ErrorEnvelope::with_extra()`. Document the deliberate
  HTTP-200-with-error-body case in §2.
- **Severity**: CRITICAL — field names lock once PR2 ships.

### R16-API2 — Proposal `wake_<base62>` typed_id prefix is 4 chars; every existing prefix is 3 chars (NEW)

- **Where**: `docs/proposals/c7-lt-async-wake.md:34, 99, 226`.
- **Cite**: `crates/core/src/typed_id.rs:3, 67, 157-167` (format
  `{prefix}_{base62(uuidv7)}`). Production prefixes via
  `grep -rn 'generate("'` across `crates/`: `usr`, `app`, `ses`,
  `sbx`, `prj`, `evt`, `hst` — **all 3 chars**. No 4-char prefix
  exists today.
- **Problem**: `wake_` is the first 4-char prefix. Dashboards/log
  filters keyed on `[a-z]{3}_[A-Za-z0-9]{22}` would miss it.
  Violates the "typed_id everywhere" invariant in AGENTS.md.
- **Fix**: rename `wake_` → `wak_`. Add `pub fn new_wake_id() ->
  String { generate("wak") }` in `typed_id.rs` next to
  `new_session_id`. Update proposal §2/§3 schema comment/§8 regex
  assertion/§11.4. The **table** name `wake_jobs` stays — only the
  per-row id prefix changes.
- **Severity**: CRITICAL — typed_id prefix is a global invariant;
  the first 4-char precedent sets bad pattern everywhere.

### R16-API3 — Proposal idempotency POST status-code matrix is underspecified (NEW)

- **Where**: `docs/proposals/c7-lt-async-wake.md:75-76` +
  request-response table at line 31.
- **Cite**: RFC 9110 §15.3.x; analogous in S3 multipart, GCP LRO,
  Stripe idempotency.
- **Problem**: §2 says "Returns `202 Accepted`" for the wake request,
  idempotency says "second POST returns existing wake_id and 202."
  Implicit reading: **always 202** for POST regardless of whether it's
  a fresh wake or a replay. This conflates "I just started this" with
  "this was already running" — both matter for retry-after metric
  attribution and client behavior on terminal-already-reached.
- **Fix**: pin the matrix in §2:
  - First POST → `202 Accepted` (operation started).
  - In-flight replay → `202 Accepted` + `X-Zsbx-Wake-Replayed: 1` header
    OR `replayed: true` body field. Same `wake_id`.
  - Post-terminal replay (within `T_KEEP`) → `200 OK` with the same
    body shape as poll-terminal. Saves a round trip.
  - Post-eviction replay → fresh `wake_id`, fresh `202`.
- **Severity**: CRITICAL — replay status is locked once PR2 ships.

## IMPORTANT

### R14-API2 (4th-round carry) — Retry-After docstring drift, sequencing with C-7-LT

- **Where**: `restore_handler.rs:58` (docstring promises `Retry-After`),
  `admin_handlers.rs:1157-1163` (response builder emits no header).
- **State**: LIVE at `792a7aa5`. C-7 budget cap (`493d6c1e`) made the
  503 a live production path; smoke-r11 confirmed WAKE 0/1 RED.
- **Sequencing**: C-7-LT phase 5 deletes the synchronous path
  entirely. Recommend **drop the `(Retry-After)` parenthetical**
  (r15's option 2) now — adding the header is wasted code in a
  doomed path. 1-token fix.

### R10-API1 (7th-round carry) — `_test_build_auth_from_sealed` orphan `pub`

- **Where**: `crates/sandbox/src/restore.rs:613`.
- **State**: LIVE. Re-verified — only the definition site exists.
  **Now the longest-running api-surface carry.**
- **Recommended action**: include in next visibility-tightening
  cluster alongside C-7-LT PR1. 1-token edit (`pub fn` →
  `#[cfg(test)] fn` if same-file, else delete).

### R10-API4 + R12-API1 (7th + 5th round carry) — readyz §10.0 drift cluster

- **Where**: `crates/sandbox-agent/src/handlers.rs:498-510` +
  `crates/sandbox/src/handlers.rs:132-139`. Verbatim:
  `{"status":"draining"}`, `{"status":"reaper-down"}`,
  `{"status":"backend-unhealthy"}`, `{"status":"ready"}`.
- **State**: LIVE. Both sites unchanged at `792a7aa5`.
- **Sequencing**: orthogonal to C-7-LT (readyz is liveness, not
  wake). Carve out as "shape-stable-pre-§10.0" with a one-line
  comment, OR convert to envelope. No urgency unless SRE keys on
  `error`/`message`.

## MINOR

### M1 — `?sync=1` query param vs header

- **Where**: `docs/proposals/c7-lt-async-wake.md:138`.
- Query params show up as path-distinct in dashboards. A header
  (`X-Zsbx-Wake-Mode: sync`) is idiomatic for behavior-switches.
  Not a blocker for an explicitly deprecated path — but document
  the decision in §4. (Discoverability via `curl --get` argues for
  the query param; ops cleanliness argues against.)

### M2 — `WakeErrorCode` variants ungrounded in the proposal

- **Where**: `docs/proposals/c7-lt-async-wake.md:54, 106`.
- PR1 will define `WakeErrorCode` per the prompt. The proposal
  doesn't enumerate values, so PR1 will back-derive from
  `RestoreHandlerError`'s 8 variants at `restore_handler.rs:43-67`.
  Two variants (`StateMismatch`, `NotFound`) are pre-flight and
  return synchronously from the POST itself — they should NOT
  appear in `WakeErrorCode`. Poll-surfaced subset:
  `vm_index_unavailable`, `snapshot_corrupt`, `snapshot_store_failed`,
  `restore_backend_failed`, `config_rewrite_failed`, `database_failed`,
  `internal_error`. **PR1 spec gate**: pin `WakeErrorCode` variants
  1:1 with existing `error_envelope.rs` code strings — do not invent
  parallel codes.

## Cross-lens consensus

- **architecture r16**: also flags synchronous-wake contract as
  structurally exhausted (R15-A1 → C-7-LT). api-surface converges:
  PR2's wire contract is make-or-break for whether C-7-LT closes the
  5-bug cluster cleanly. The 3 R16-API findings are pre-implementation
  gating.
- **code-quality r16**: should pick up `_test_build_auth_from_sealed`
  orphan — 7 rounds, cheapest open finding any lens.
- **concurrency r16**: orthogonal this round.

## Lens hand-off

- **To architecture r17**: validate proposal §3 Option B
  `wake_jobs` lessee-CAS against existing sandboxes-table CAS;
  share recovery-sweep code.
- **To security r17**: pre-review §2 `agent_url` field exposure —
  who's authorized to poll? Today's `POST /wake` is admin-bearer
  gated; if gateway code polls `/wake/{wake_id}`, same path. If
  user code polls, capability check needed.
- **To test-coverage r17**: confirm proposal §8 PG tests cover the
  R16-API3 replay-status matrix (4 cases).

## PR2 spec gates (must get right for clean API)

1. **§10.0 field-name parity**: `error` not `error_code`, `message`
   not `error_message` on failed-state poll body. (R16-API1.)
2. **3-char typed_id prefix**: `wak_` not `wake_`. Wire through
   `crates/core/src/typed_id.rs::new_wake_id()` rather than ad-hoc
   `generate("wake")`. (R16-API2.)
3. **Idempotency status-code matrix**: 202 / 202+replay-marker /
   200 / 202 per R16-API3. Document in the new
   `docs/reference/wake-contract.md` the proposal §10 calls for.
4. **`WakeErrorCode` = existing envelope codes 1:1**: reuse the
   snake_case kinds already on the wire; do not invent a parallel
   enum. (M2.)
5. **`?sync=1` deprecation telemetry**: WARN log already specified;
   ADD `sandbox_wake_sync_uses_total{client}` counter so phase 5's
   "zero sync uses for a minor" gate has data.

## Trend

- **`pub`-token count** (`^[[:space:]]*pub[[:space:](]`):
  - r15: 848
  - **r16: 848** — flat. (A1-FOLLOWUP +1 `pub(crate)` counts under
    both `pub` and `pub(crate)` by canonical regex — net 0.)
- **`Result<_, String>`**: sandbox 161 (5 rounds flat), sandbox-agent
  14 (7 rounds flat).
- **`RestoreBackend` trait method count**: 8 (flat).
- **Net new wire endpoints r15→r16**: **0** in landed code; **+1
  designed** (`GET /wake/{wake_id}`) in C-7-LT proposal.
- **Closure velocity**: r14→r15 = 2; **r15→r16 = 0** (predicted 0-1;
  backlog now needs judgment calls, not mechanical edits).
- **Backlog open-item count**:
  - r15: 4
  - **r16: 7** (carries + 3 new design-pre-review). New ones are
    advisory-blocking on PR2, not on landed code.

## Two most-critical citations

1. **`docs/proposals/c7-lt-async-wake.md:54`** —
   `{"state":"failed", "error_code":"…", "error_message":"…"}`.
   Field names diverge from §10.0 (`error`, `message`) used at
   `crates/sandbox/src/error_envelope.rs:88-110` and every landed
   endpoint. Must be fixed pre-PR2.
2. **`docs/proposals/c7-lt-async-wake.md:99`** —
   `wake_id TEXT PRIMARY KEY, -- typed_id wake_<base62>`. 4-char
   prefix; every other typed_id in the codebase uses 3 chars
   (`crates/core/src/typed_id.rs:157-167` + 7 ad-hoc
   `generate("xyz")` sites). Violates AGENTS.md "typed_id
   everywhere" invariant.
