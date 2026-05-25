# Sandbox snapshot/restore — API-surface review (2026-05-25 r9)

Branch `feat/sandbox-snapshot-restore` @ `d1ffe857` (post-merge; merged to
main at `23fe351a` in prior session). Read-only delta over r8 (`f2d89b61`).
Audits R8-API1 closure status, A4 envelope migration, B24 + B24-FOLLOWUP
surface impact, post-merge doc-comment freshness.

## R-carryover status

- **R7-API1** (`verify_kind_skew_bypass` pub→pub(crate)): **CLOSED** at
  `0a271d2f`. `sig.rs:420` now `pub(crate) fn`; closure note at `:419`
  ("Closes R7-API1 — sibling of R4-S1/R5-API1/R5-API2"). Deferred §R7-API1
  correctly marks CLOSED.
- **R8-A4** (sandbox-agent envelope): **CLOSED** at `fc3e9972` + `ae5cc977`.
  All 34 emission sites migrated; `error_envelope.rs` lifted into the
  agent crate; handlers.rs:201-208 funnels through `error_response`.
- **R8-API1** (2 pub items from R7-S1): **HALF-CLOSED**.
  - `ResyncBody` at `sig.rs:119`: **CLOSED** — now `pub(crate) struct`
    with `pub(crate)` fields (deferred entry §R8-API1 line 464 is stale).
  - `init_sandbox_id_from_env` at `handlers.rs:101`: **STILL OPEN** —
    byte-identical `pub fn`. Only caller is `main.rs:97` (same crate).
- **R7-API2** (`clock.resync-v1` advertised but unread): **STILL OPEN**.
  `version.rs:54` advertises; controller `restore_handler.rs:484-518`
  calls `submit_restore_job` + (later) clock_resync unconditionally —
  zero `has_capability("clock.resync-v1")` gate, no `/version` fetch
  by the controller anywhere in `crates/sandbox/src/`.
- **handlers.rs:670/821/837 raw `{e}`** (sandbox crate): **STILL OPEN**,
  6th-round carry (`backend.stop: {e}`, `backend.exec: {e}`,
  `backend.file_tree: {e}`). Identical text to r4.

## New findings (post-merge)

### MAJOR

1. **`init_sandbox_id_from_env` still `pub fn`** —
   `crates/sandbox-agent/src/handlers.rs:101`. The sole caller is
   `main.rs:97` (same crate). Five rounds since R7-S1 introduced it
   (`6f5d41b8`); R8-API1 explicitly named it; the fix is mechanical
   (`pub` → `pub(crate)`, ~10 chars). External crates can racily
   read-or-set the pubkey-sized invariant. Same one-line pattern that
   `ResyncBody` got at `sig.rs:119`. Sibling `test_set_sandbox_id` at
   `handlers.rs:131` is also `pub fn` (should be `pub(crate)` +
   `#[cfg(test)]`-already-gated → drop `pub` entirely).

2. **`sig.rs:120` doc comment stale post-B24-FOLLOWUP** —
   ```rust
   /// Sandbox UUID (string form, e.g. `019486f5-…`). Agent rejects
   ```
   The hyphenated example contradicts the canonical `.simple()`
   (32-char hex, no hyphens) form B24-FOLLOWUP (`66029821` +
   `9a61e66e`) established as the wire contract. Controller signs
   `sandbox_id.simple().to_string()` at `restore_handler.rs:1549`;
   agent reads `SANDBOX_AGENT_SANDBOX_ID` (set from `.simple()` in
   `nomad_ch.rs::ZSBX_SANDBOX_ID`). An implementer reading sig.rs:120
   will hand-roll the wrong format. Update example to
   `019486f5d4e07b428abf3d2c4e1a6f7c` (no hyphens).

3. **`db.rs:2677` test comment claims "hyphenated form" post-B24** —
   ```rust
   // The file itself contains the UUID (hyphenated form).
   ```
   Test `assert_eq!(on_disk.trim(), uuid.to_string())` at `:2680`
   relies on `Uuid::to_string()` (still hyphenated for host_id —
   that's correct; host_id is a different identifier from
   sandbox_id). But the comment juxtaposed with B24-FOLLOWUP's
   `.simple()` discipline reads as a contradiction without explicit
   note that host_id intentionally stays hyphenated. Add one sentence:
   "host_id is the controller's persistent identity, unrelated to
   sandbox_id; intentionally retains hyphens unlike sandbox_id which
   uses .simple()."

### MINOR

4. **`Result<_, String>` count holds at 188 sites in sandbox crate;
   47 in sandbox-agent** — pre-A4/A1/A2b/C1/B24 trend was
   ~190/sandbox + ~50/sandbox-agent (r8 noted similar magnitudes
   without exact counts). Stabilization is real but the absolute floor
   is high: every `String` error path is a structured-logging miss and
   blocks any future `thiserror`-keyed dashboard. Top offenders by
   file: `nomad_ch.rs` (30 — restore + alloc paths), `restore_handler.rs`
   (17 — already migrated to `RestoreHandlerError` enum in part,
   remainder are blocking-closure boundaries), `lib.rs` (4 — boot-time,
   acceptable). Recommend a per-file budget the next migration cycle
   can target (e.g. `nomad_ch.rs` → ≤10 in r10).

5. **Zero new pub items since B24** — `git diff a4c481e1^..HEAD --
   crates/sandbox/ crates/sandbox-agent/` shows 0 lines matching
   `^\+pub (fn|struct|enum|trait|const|static|mod)`. B24 and
   B24-FOLLOWUP are both pure-impl changes; clean surface delta.

6. **`handlers.rs::test_set_sandbox_id` is `pub fn` despite being
   `#[cfg(test)]`-gated** — `crates/sandbox-agent/src/handlers.rs:131`.
   The cfg gate makes it test-only at compile time, so `pub` adds no
   risk in prod builds — but the convention elsewhere in the agent
   (`reap.rs:71 pub fn test_set_healthy`) is also `pub`, so this is
   consistent style, not a bug. Note for future cleanup.

## Summary

6 findings (0 critical, 3 major, 3 minor). r8-resolved: 2 (R7-API1
fully, R8-API1 half — `ResyncBody`). r8-carryover: 3 (R8-API1
`init_sandbox_id_from_env`, R7-API2 capability gate, handlers.rs
:670/821/837 raw `{e}`). NEW: 2 stale doc comments (#2, #3) traceable
to B24-FOLLOWUP's `.simple()` switch.

**R8-API1 status**: half-CLOSED. `ResyncBody` (sig.rs:119) is now
`pub(crate) struct` with `pub(crate)` fields. `init_sandbox_id_from_env`
(handlers.rs:101) remains `pub fn` — 5-round carry, mechanical fix
deferred again. Deferred file §R8-API1 (line 463) should be split:
"ResyncBody CLOSED at fc3e9972; init_sandbox_id_from_env OPEN".

Two most-critical citations:

- `crates/sandbox-agent/src/handlers.rs:101` — `pub fn
  init_sandbox_id_from_env() -> Result<(), String>`. Single intra-crate
  caller (`main.rs:97`); `pub(crate)` is the right visibility. 5-round
  carry post-R7-S1. Same anti-pattern R4-S1/R5-API1/R5-API2/R7-API1
  closed; `ResyncBody` got the fix sibling-side in fc3e9972 but this
  item was missed in the same sweep.

- `crates/sandbox-agent/src/sig.rs:120` — `Sandbox UUID (string form,
  e.g. \`019486f5-…\`)`. The hyphenated example contradicts
  B24-FOLLOWUP's `.simple()` wire contract (controller emits at
  `restore_handler.rs:1549` per `66029821`). An implementer following
  this docstring would emit the wrong format and 401 every resync.
