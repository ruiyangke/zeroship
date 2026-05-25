# Sandbox/snapshot-restore — api-surface r10 review

Date: 2026-05-25 (UTC)
HEAD at audit: `9678a840`
Round 10 of N.
Prior: r9 at `d1ffe857` (`docs/reviews/sandbox-snapshot-restore-api-surface-2026-05-25-r9.md`).

Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`. Read-only.

## Summary

7 findings (0 critical, 2 important, 5 minor). r9-resolved: 1 (R8-API1
fully closed at `10bddc20`). r9-carryover: 3 (sig.rs:120 stale doc,
db.rs:2839 stale "hyphenated form" comment, handlers.rs:670/821/837 raw
`{e}` leak — now in its 7th round). NEW r10: 4, all api-surface
(orphan-public fns, ExecBody / not_found over-pub, readyz wire-shape
edge case, capability-gate dead advertisement persists).

`Result<_, String>` count: 178 in sandbox (was 188, -10), 16 in
sandbox-agent (was 47, **-31, ~66% drop**). Migration noticeably
accelerated this round; agent crate is now near-floor.

## New `pub` items since r9 (audit)

Hash range `d1ffe857..9678a840`. `git diff <range> -- crates/sandbox{,-agent}/src/ | grep "^+pub "`:

| Commit | Item | Path | Visibility | Justified externally? |
|---|---|---|---|---|
| `10bddc20` | `boot_init_sandbox_id` | `crates/sandbox-agent/src/lib.rs:99` | `pub fn` | YES — the bin (`[lib]/[bin]` split) compiles against the lib's public surface; this is the **single** named entry point the binary reaches through. Docstring is explicit. Closes R8-API1 in full. |
| `0e71e5c4` | `claim_orphan_transient_for_recovery` | `crates/sandbox/src/db.rs:2500` | `pub async fn` | YES — three pg-gated regression tests in `crates/sandbox/tests/sandbox_pg_e2e.rs:3050,3125,…` consume it via the integration-test crate boundary, which forces `pub`. Internal sweep caller is `sweep.rs:182`. Cannot be `pub(crate)` without breaking the integration test entry. Acceptable. |
| `419c154b` | (none) | snapshot-aead negative tests | n/a | tests only, no surface delta |
| `6f314025` | (none) | `TieredSnapshotStore::put` spawn_blocking | n/a | impl-only |

**Net new `pub` items in `crates/sandbox-agent/`: +1 (`boot_init_sandbox_id`).
Net new `pub` items in `crates/sandbox/`: +1 (`claim_orphan_transient_for_recovery`).**
Both justified. Clean delta from r9 standpoint.

## R-carryover status

- **R8-API1** (`init_sandbox_id_from_env` `pub`→`pub(crate)`):
  **CLOSED** at `10bddc20`. Verified — `handlers.rs:106` now reads
  `pub(crate) fn init_sandbox_id_from_env(...)`. New `pub fn
  boot_init_sandbox_id` at `lib.rs:99` is the canonical bin-visible
  wrapper. Docstring explains the `[lib]/[bin]` rationale. Move R8-API1
  to CLOSED in `sandbox-snapshot-restore-deferred.md`.
- **R7-API2** (`clock.resync-v1` advertised but unread by controller):
  **STILL OPEN, 4th-round carry.** `version.rs:54` advertises
  `clock.resync-v1`; controller code at
  `restore_handler.rs:1551,1636` issues the resync UNCONDITIONALLY
  via `sig::sign("POST", path, …)`. `grep -rn "has_capability\b" crates/sandbox/src/`
  → zero hits. No `capabilities` field is read from the agent's
  `/version` response anywhere in `crates/sandbox/src/`. The capability
  array is decoration. Either gate the call site on
  `version_resp.capabilities.contains(&"clock.resync-v1")` or remove
  the advertisement (controller will surface a 404 on absent agents,
  which is acceptable feature-detection). Same as r7/r8/r9.
- **handlers.rs:670/821/837 raw `{e}` leak** (sandbox crate):
  **STILL OPEN, 7th-round carry.** Verbatim at the same line
  numbers — `format!("backend.stop: {e}")`, `format!("backend.exec: {e}")`,
  `format!("backend.file_tree: {e}")`. These reach the wire after
  passing through `err(500, code, ...)` → `error_response`, so they
  emit on the `message` field of the §10.0 envelope. Sibling
  `err_safe` exists at `admin_handlers.rs:228` for exactly this
  pattern (logs raw via `tracing::error!` + emits fixed prose) but
  the sandbox-side handlers.rs uses the unsafe `err()` variant. Three
  one-line fixes (`err` → `err_safe`). 7 rounds.

## NEW findings (post-r9)

### [R10-API1] `_test_build_auth_from_sealed` is `pub` but has zero callers (MINOR, api-surface-r10)

- **File**: `crates/sandbox/src/restore.rs:613`
- **Symptom**: Declared `pub fn _test_build_auth_from_sealed(...)`
  with `#[doc(hidden)]`. `grep -rn '_test_build_auth_from_sealed' .`
  returns the definition site only — zero call sites in the entire
  workspace (no production callers, no integration tests in
  `crates/sandbox/tests/*`, no doctests). The `_` prefix and
  `#[doc(hidden)]` advertise "test scaffolding" but the surface is
  live in release builds and there's no consumer.
- **Action**: Either delete (preferred — orphan dead code is worse
  than absent) or downgrade to `pub(crate) fn` + `#[cfg(test)]` if a
  future test is genuinely planned. Citation: scaffold was likely
  added speculatively and never wired.

### [R10-API2] `ExecBody` and `not_found` over-pub'd in sandbox-agent (MINOR, api-surface-r10)

- **Files**:
  - `crates/sandbox-agent/src/handlers.rs:568` — `pub struct ExecBody { pub cmd: String, ... }`
  - `crates/sandbox-agent/src/handlers.rs:264` — `pub fn not_found() -> HttpResponse`
- **Symptom**: Both are reachable from the bin (`main.rs:229` calls
  `handlers::not_found`; `exec_cmd` parses into `ExecBody` internally)
  but ONLY by the same crate's bin via `pub mod handlers`. Neither is
  re-exported from `lib.rs`, and `grep` shows no cross-crate consumer.
  Same anti-pattern that R8-API1 just closed for
  `init_sandbox_id_from_env` (which got the wrapper-at-lib treatment).
- **Action**: Two options:
  - **Option A** (mirror R8-API1 pattern): make `ExecBody` →
    `pub(crate) struct` (it's deserialise-only, never returned across
    the boundary) and add a `pub fn render_404() -> HttpResponse`
    wrapper at `lib.rs` that calls `handlers::not_found()` internally;
    flip `not_found` to `pub(crate)`. Three-line change.
  - **Option B** (accept the surface): document at the top of
    `handlers.rs` that "all `pub fn`s in this module are bin entry
    points; non-fn items SHOULD be `pub(crate)`". Cheaper and matches
    current reality.
  - Recommend Option A for `ExecBody` (it's a wire-shape struct, would
    benefit from the same narrow-surface treatment); Option B for
    `not_found` (true bin entry point — the rationale is symmetric
    with the other handler fns).

### [R10-API3] `persist::seal` / `unseal_one` / `unseal_dir` / `seal_filename_for{,_str}` are `pub fn` with intra-crate-only callers (MINOR, api-surface-r10)

- **Files**:
  - `crates/sandbox/src/persist.rs:272` — `pub fn seal_filename_for(sandbox_id: Uuid) -> String`
  - `crates/sandbox/src/persist.rs:281` — `pub fn seal_filename_for_str(...)`
  - `crates/sandbox/src/persist.rs:369` — `pub fn seal(sandbox_id: Uuid, auth: &SealedAuth, dir: &Path, key: &AeadKey) -> ...`
  - `crates/sandbox/src/persist.rs:439` — `pub fn unseal_one(path: &Path, key: &AeadKey) -> ...`
  - `crates/sandbox/src/persist.rs:506` — `pub fn unseal_dir(dir: &Path, key: &AeadKey) -> ...`
- **Symptom**: All five are module-free-functions called only from
  within `persist.rs` or via the `Persistence` handle methods (which
  delegate to them inside `spawn_blocking`). `grep -rn 'persist::seal\b\|persist::unseal'`
  returns zero hits outside the file. `seal_filename_for` is called
  by `backend/nomad_ch.rs:4593` and `admin_handlers.rs:1036` — those
  ARE in-crate. No integration test consumes these directly. The
  AEAD-keyed sealing surface is supposed to be `Persistence`-handle-only
  (the handle is what bundles the key + spawn_blocking + audit hooks);
  exposing the free-fn variants as `pub` invites a future bypass.
- **Action**: Demote all five to `pub(crate) fn`. Cost: ~5
  one-token edits. No test or external consumer breaks (verified).
  Risk: a future contributor pulls `persist::seal` directly bypassing
  the `Persistence` handle's audit + spawn_blocking discipline. Now
  is the cheap fix window.

### [R10-API4] `readyz` returns non-§10.0 wire shape on 503 (MINOR, api-surface-r10)

- **File**: `crates/sandbox-agent/src/handlers.rs:498-510`
- **Symptom**: `readyz` on draining/reaper-down returns
  `{"status":"draining"}` or `{"status":"reaper-down"}` — neither
  is the §10.0 `{"error": "...", "message": "..."}` envelope.
  Adjacent code (`livez`, `version_info`, every error path) DOES use
  the envelope. `readyz` is the only non-envelope 503 in the agent.
- **Action**: Two readings:
  - **Strict §10.0**: emit `{"error":"draining","message":"draining"}`
    (or `"reaper_down"`). Aligns with the §10.0 invariant the rest
    of the agent now upholds.
  - **Probe-semantics carve-out**: `readyz` is consumed by Kubernetes
    /Nomad readiness probes, which read STATUS CODE only (not body).
    The body is operator-visible for debugging; the current shape is
    intentional. If carving out, add a comment at line 498:
    `// readyz: probe-only endpoint; body is debug-visible status,
    NOT a §10.0 envelope. Probe consumers branch on status code.`
  - Either resolves; the **un**explained drift is the finding.
    Recommend the carve-out + comment (orchestrator probes are a
    well-known special case; the operator-debug body shape is more
    readable as `{"status":"draining"}` than as an error envelope).

### [R10-API5] sig.rs:120 stale `019486f5-…` example (carryover from r9, IMPORTANT)

- **File**: `crates/sandbox-agent/src/sig.rs:120`
- **Symptom**: ResyncBody field doc reads:
  ```rust
  /// Sandbox UUID (string form, e.g. `019486f5-…`). Agent rejects
  ```
  Hyphenated example contradicts B24-FOLLOWUP's `.simple()` wire
  contract (controller emits at `restore_handler.rs:1549` via
  `sandbox_id.simple().to_string()` — 32-char hex, no hyphens). An
  implementer reading this docstring will hand-roll the wrong format
  and 401 every resync. R9 already flagged; **still verbatim**.
- **Action**: Replace `019486f5-…` with a hyphen-less example like
  `019486f5d4e07b428abf3d2c4e1a6f7c`. 30-char edit. 2nd round.

### [R10-API6] db.rs:2839 stale "hyphenated form" test comment (carryover from r9, MINOR)

- **File**: `crates/sandbox/src/db.rs:2839` (was 2677 in r9; line
  shifted by ~160 due to `0e71e5c4`'s 164-line insertion above)
- **Symptom**: `// The file itself contains the UUID (hyphenated
  form).` — test is about `host_id`, which IS intentionally
  hyphenated (`Uuid::to_string()`), but the comment juxtaposed with
  B24-FOLLOWUP's `.simple()` discipline reads as a contradiction
  without explicit acknowledgement that host_id ≠ sandbox_id.
- **Action**: Append one sentence: `// host_id is the controller's
  persistent identity, unrelated to sandbox_id; intentionally retains
  hyphens unlike sandbox_id which uses .simple().` Same as r9
  recommendation. 2nd round.

### [R10-API7] Error envelope drift between sandbox and sandbox-agent (MINOR, api-surface-r10)

- **Files**:
  - `crates/sandbox/src/error_envelope.rs`
  - `crates/sandbox-agent/src/error_envelope.rs`
- **Symptom**: Side-by-side compare confirms the wire-shape invariant
  holds — both emit `{"error": "<code>", "message": "<prose>"}` at the
  top level. **Drift is in the helper surface, not the wire**:
  - sandbox crate: `ErrorEnvelope::new + with_extra + no_store +
    into_response`; supports extra fields and `Cache-Control: no-store`.
  - sandbox-agent: `ErrorEnvelope::new + into_response`; **no**
    `with_extra`, **no** `no_store`. Plus an `error_from_status(u16,
    msg)` legacy adapter that auto-maps 400/403/404/500 → known codes.
  - sandbox-agent's module docstring (`error_envelope.rs:34-42`) acknowledges this and explicitly defers the lift-to-core until "a third caller appears". That's reasonable.
- **Wire shape**: identical top-level `error` + `message` keys.
  Sandbox extras (key-namespaced fields like `expected`/`current`)
  are additive — older consumers ignoring unknown keys still parse.
  **Verdict: no wire drift; the helper-surface drift is documented
  and acceptable for now.**
- **Action**: NONE required this round. Track in deferred so a
  follow-up that introduces a 3rd HTTP-emitting crate can lift the
  envelope to `zeroship-core` per the agent's own docstring promise.
  This finding is informational, not a defect.

## Carry-forward (unchanged, still open)

- **sig.rs:120 stale `019486f5-…` example** — 2nd round (r10-API5).
- **db.rs:2839 (was :2677) stale "hyphenated form" comment** — 2nd
  round (r10-API6).
- **handlers.rs:670/821/837 raw `{e}` leak** — **7th-round carry.**
  Three lines, identical text since r4. Fix is `err(...)` → `err_safe(...)`.
- **R7-API2** capability-gate dead advertisement — 4th-round carry.
  Either gate or remove `clock.resync-v1` from the advertised set.

## Status of r9 findings

| r9 # | Title | r10 status |
|---|---|---|
| r9-1 | `init_sandbox_id_from_env` still `pub fn` (R8-API1 leftover) | **CLOSED at `10bddc20`** |
| r9-2 | `sig.rs:120` stale hyphenated example | **STILL OPEN** (r10-API5, 2nd carry) |
| r9-3 | `db.rs:2677` stale "hyphenated form" test comment | **STILL OPEN** (r10-API6, 2nd carry; line shifted to 2839) |
| r9-4 | `Result<_, String>` count high (188 + 47) | **PARTIAL** — now 178 + 16; agent crate dropped ~66%. Sandbox crate moved -10. |
| r9-5 | Zero new pub items since B24 | **REGRESSED INTENTIONALLY** — 2 new pubs (both justified, see audit table) |
| r9-6 | `test_set_sandbox_id` `pub` despite `#[cfg(test)]` | **STILL OPEN** (mentioned only, style-only, no fix needed per r9 ruling) |

## Two most-critical citations

1. **`crates/sandbox-agent/src/sig.rs:120`** — `Sandbox UUID (string
   form, e.g. \`019486f5-…\`)`. R9-flagged but not patched. The
   hyphenated example actively misleads implementers; the controller
   signs `.simple()` (no hyphens) at `restore_handler.rs:1549`. A
   reader following this docstring will 401 every resync. 30-char
   edit. **2nd round.**

2. **`crates/sandbox/src/handlers.rs:670/821/821/837`** — raw `{e}` on
   the `backend.stop` / `backend.exec` / `backend.file_tree` error
   path. Reaches the §10.0 envelope's `message` field. The sibling
   `err_safe` helper at `admin_handlers.rs:228` exists for exactly
   this. Three one-line fixes (`err(...)` → `err_safe(...)`).
   **7th-round carry.** Operational shame: the fix is mechanical,
   the helper is right there, and every round we re-cite the same
   three line numbers.
