# Sandbox snapshot/restore — API-surface review (2026-05-24 r8)

Branch `feat/sandbox-snapshot-restore` @ `f2d89b61`. Read-only delta over r7 (`c07cbb62`).
Verifies R7-S2 closure; audits R7-S1 agent-side surface (`e95baa89`) + R7-P1 `Arc<dyn>`
flip (`79428d53`).

## R-carryovers — status

- **R7-S2** (`derive_agent_url` sentinel default): **CLOSED** at `e95baa89`.
  `restore_handler.rs:187` is now `fn derive_agent_url(&self, vm_index: i16) -> String;`
  with no default body. Deferred file §R7-S2 is stale; should mark CLOSED.
- **R7-API1** (`Verifier::verify_kind_skew_bypass` pub on pub mod): **OPEN**.
  `sig.rs:413` byte-identical to r7. R7-S1 touched `sig.rs` (+57 LOC) and was the natural
  moment to land the `pub(crate)` restriction — did not.
- **R7-API2** (`clock.resync-v1` capability advertised but unread): **OPEN**.
  `version.rs:54` advertises; `restore_handler.rs:512-523` calls
  `clock_resync_post_restore` unconditionally with no `/version` capability gate.
- **R5-Q1** (`register_restored` default `Ok(())` no-op): **OPEN**.
  `restore_handler.rs:162-170` byte-identical. Same antipattern R7-S2 just closed one
  trait method up.

## New findings (post-r7, R7-S1 + R7-P1 focused)

### CRITICAL

1. **A4 §10.0 envelope was never extended to sandbox-agent** —
   `crates/sandbox-agent/src/handlers.rs:202-212` (`err()` emits `{"error":<human msg>}`
   with no `code` / `message` split); `:190` (`unauthorized()`); `:199` (`draining()`);
   `crates/sandbox-agent/src/proxy.rs:255` + `:259` (sibling `err()`/`err_with_code()`,
   the latter inverts A4 field order to `{"error":<msg>,"code":<code>}`). Call-site count:
   - `handlers.rs::err()`: 13× (lines 534, 562, 577, 594, 598, 606, 609, 611, 665, 719,
     740, 816, 843)
   - `handlers.rs::unauthorized()`: 11× (478, 501, 524, 569, 619, 640, 659, 712, 754,
     775, 806)
   - `handlers.rs::draining()`: 5×
   - `proxy.rs::err()` + `err_with_code()`: 4× (lines 98, 115, 194, 201)
   - `main.rs`: 1×

   **Total: ~34 non-A4 wire emissions across sandbox-agent.** R7-S1's brand-new
   `/_clock_resync` handler inherits at `:740, :816, :843`. Operators wiring one
   dashboard rule per `error` code cannot — the agent's `error` field IS the human
   prose. A4 closed the sandbox crate at 100% (`2928d5ae`); the agent stays at 0%.
   Lift `crates/sandbox/src/error_envelope.rs::ErrorEnvelope` to `zeroship-core` (or
   copy into the agent crate) and funnel all 34 sites.

2. **`Verifier::verify_kind_skew_bypass` still `pub` on `pub mod sig`** —
   `crates/sandbox-agent/src/sig.rs:413`. R7-S1 was the moment to land R7-API1; didn't.
   `Arc<Verifier>` is reachable via `AppState.verifier` (`handlers.rs:166`). Same shape
   R4-S1 / R5-API1 / R5-API2 closed at `93348b91`.

### MAJOR

3. **`pub fn init_sandbox_id_from_env()` over-exposed** —
   `crates/sandbox-agent/src/handlers.rs:95`. Only caller is `main.rs:97` (same crate).
   `pub` on `pub mod handlers` lets external harnesses race-init a controller pubkey-
   sized invariant. Should be `pub(crate)`.

4. **`pub struct ResyncBody` over-exposed in `pub mod sig`** —
   `crates/sandbox-agent/src/sig.rs:119`. Single deserialize site at `handlers.rs:717`.
   Three `pub` fields (`sandbox_id`, `ts`, `challenge`) leak the wire schema onto the
   crate's stable API. Wire contract belongs on the controller side
   (`restore_handler.rs::ClockResyncBody`); the agent's deserialization view should be
   `pub(crate)`.

5. **`init_sandbox_id_from_env` conflates env + file fallback; non-idempotent overwrite
   is silent** — `handlers.rs:95-118`. Body reads `/run/keys/sandbox-id` on env-unset;
   caller can't tell which source won. Second-call-with-different-id returns `Ok` and
   keeps the first (write-once OnceLock). Either split into two fns or return the
   resolved id + source so the caller can log it.

### MINOR

6. **`SANDBOX_ID` / `RESYNC_CHALLENGES` statics not `pub` (correct), but testability
   contract buried** — `handlers.rs:59,70`. Process-local mutable static surfaces a
   one-LRU-per-process hazard; the contract lives in a 30-line comment at `:145-155`
   instead of the module doc. The `#[allow(dead_code)]` on
   `test_clear_resync_challenges` (`:158`) defeats the lint that would catch accidental
   prod use.

7. **R7-P1's `Arc<dyn ChRemoteClient>` + `Arc<dyn SnapshotStore>` flip is the second
   spawn-blocking trait conversion without a documented convention** —
   `crates/sandbox/src/snapshot_handler.rs` modified at `79428d53` (cf. R5-P1b at
   `cdd2e677`). No external surface leak (`pub(crate)`-bound), but
   "spawn_blocking-friendly traits use `Arc<dyn>`" deserves a one-line note in
   `crates/sandbox/README.md` before a third trait flips.

## Summary

7 findings (2 CRITICAL, 3 MAJOR, 2 MINOR). r7-resolved: 1 (R7-S2). r7-carryover: 2
(R7-API1, R7-API2). NEW: A4 sandbox-agent gap (#1), 3 R7-S1 surface leaks (#3-5), LRU
testability (#6), `Arc<dyn>` convention (#7).

R7-S1 (`e95baa89`) added 3 `pub` items (`init_sandbox_id_from_env`, `ResyncBody`, and
left `verify_kind_skew_bypass` pub) — ~80 LOC of implementation-detail leak in one
commit, against an OPEN R7-API1 on the same module. R7-S2 itself IS correctly closed
at `:187`; deferred file should mark CLOSED.

**A4 sandbox-agent gap quantified**: ~34 wire-emission sites (`handlers.rs` 29 +
`proxy.rs` 4 + `main.rs` 1); zero A4-compliant. R7-S1's `/_clock_resync` contributes
3 of 34 (lines 740, 816, 843). Half-day fix; lifting `ErrorEnvelope` into
`zeroship-core` closes future agent crates as well.

Two most-critical citations:

- `crates/sandbox-agent/src/handlers.rs:202-212` — `err()` emits `{"error":<msg>}` (no
  `code`/`message`). ~34 call sites across the crate. R7-S1's `/_clock_resync` inherits
  at lines 740/816/843.

- `crates/sandbox-agent/src/sig.rs:413` + `handlers.rs:95` + `sig.rs:119` — three `pub`
  items that should be `pub(crate)`, layered on still-open R7-API1. Anti-pattern
  R4-S1 / R5-API1 / R5-API2 closed in-crate at `93348b91`; regressed twice across the
  sandbox-agent boundary in `6f5d41b8` + `e95baa89`.

**R-status**: R7-S2 CLOSED (deferred stale), R7-API1 OPEN, R7-API2 OPEN, R5-Q1 OPEN.
