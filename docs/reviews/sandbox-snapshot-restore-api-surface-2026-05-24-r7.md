# Sandbox snapshot/restore — API-surface review (2026-05-24 r7)

Branch `feat/sandbox-snapshot-restore` @ `6f5d41b8`. Read-only delta over r6 (`29196e0c`).
Verifies R4-S1 / R5-API1 / R5-API2 closure at `93348b91`; audits B22's ~900 LOC of NEW public
surface (`6f5d41b8`); audits R6-P1 detach spawn (`4c090992`) and R6-A1 env-var rename (`2ead8692`).

## R-carryovers — status

- **R3-Q2** (`with_persistence` infallible-Result): CLOSED at `ac6a6bf2` (verified r6).
- **R4-S1** (`Backend::vm_index_allocator()` pub): **CLOSED** at `93348b91`. `backend/mod.rs:456` now `pub(crate)`.
- **R5-API1** (`Backend::register_restored` pub + raw `[u8;32]` SK): **CLOSED** at `93348b91`. `backend/mod.rs:491-510` now `pub(crate)` with key-footgun rationale in the doc comment.
- **R5-API2** (`Backend::nomad_ch_handle()` Arc-leak): **CLOSED** at `93348b91`. `backend/mod.rs:482` now `pub(crate)`.
- **R4-S2** (`ErrorEnvelope::with_extra(Value)` non-object drop): STILL OPEN. `error_envelope.rs:74-77` byte-identical.
- **r4 #7** (`AppState::config()` speculative pub): STILL OPEN.

## New findings (post-r6, B22-focused)

### CRITICAL

1. **`Verifier::verify_kind_skew_bypass` is `pub` on a `pub mod sig`** —
   `crates/sandbox-agent/src/sig.rs:356`. The crate exposes `pub mod sig` at `lib.rs:21`, so any
   external code holding an `Arc<Verifier>` (the type is `pub`, constructible via `Verifier::new`
   at `sig.rs:235`, and re-exported through `AppState.verifier` at `handlers.rs:41`) can call the
   skew-bypass surface directly. The doc comment at `sig.rs:316-355` calls this the "ONLY"
   skew-bypass path; the type system does not enforce that. A future agent contributor wiring a
   new handler can reach for `verify_kind_skew_bypass` instead of `verify_kind` and the compiler
   says nothing. Should be `pub(crate)` (only `handlers::verify_signed_skew_bypass` at
   `handlers.rs:257` calls it in-tree) or moved behind a sealed `ClockResync`-bound helper.

### MAJOR

2. **`/_clock_resync` capability advertised as feature-detectable; controller never reads it** —
   `crates/sandbox-agent/src/version.rs:49-52` claims "The controller feature-detects via this
   string so older agents (no resync endpoint) gracefully fall back". `Grep` across
   `crates/sandbox/src/**` for `clock.resync-v1` returns zero matches; `do_restore_inner` at
   `restore_handler.rs:485-498` calls `clock_resync_post_restore` unconditionally. An old
   (pre-v17) agent will 404, surfacing as `RestoreHandlerError::Backend("/_clock_resync status
   404: ...")` and rolling the row back to `Snapshotted`. That's "fail loudly", not "graceful
   fallback". Either remove the misleading comment OR add a real capability check on the
   `/version` response (which the controller already calls inside `wait_for_agent_livez`).

3. **`/_clock_resync` route has zero runbook / README surface** — added at
   `sandbox-agent/src/main.rs:171-180`; only documented in module comments
   (`handlers.rs:545-569`), `sig.rs:316-355`, the new metric
   (`sbx_agent_clock_resyncs_total` at `metrics.rs:49-52,191-196`), `version.rs:49-52`, and review
   artifacts. `grep clock_resync docs/runbooks/` → 0 hits. The endpoint is admin-equivalent (it
   sets `CLOCK_REALTIME` via `settimeofday(2)`); operators need to know its threat model and
   that on-host `journalctl` can be checked via the new counter. Same shape as r6 finding #2
   (B21 env-var triplet has zero runbook surface) — the discoverability gap IS the API-surface
   concern.

4. **`RestoreBackend::derive_agent_url` default impl returns `"http://127.0.0.1:0"` —
   silent landmine for future trait implementors** — `restore_handler.rs:182-184`. Any future
   `RestoreBackend` impl that forgets to override `derive_agent_url` AND configures with
   `persist=Some(...)` will issue `clock_resync_post_restore` against a dead loopback port,
   surfacing as a `transport: ...` error in `do_restore_inner` at `:494-498`. Same shape as
   R5-Q1 (`register_restored` default-Ok no-op): the silent default contradicts the safety
   property the impl enforces. Make it required, or have the default `panic!` / return a
   typed `Err` so misconfiguration fails loudly at trait-dispatch time, not after a
   network round-trip.

5. **B22 added ~900 LOC; `Result<_, String>` regressed by ~20 in this commit** —
   `restore_handler.rs` now carries 21 `Result<_, String>` occurrences (was ~19 pre-B22);
   `clock_resync_post_restore` itself (`:1423-1484`) plus the new `clock_resync_nonce`
   (`:1493-1501`) both return `Result<_, String>`. Workspace total in `crates/sandbox/**` is
   170 (+39 in `crates/sandbox-agent/**`). The B22 fix had the opportunity to introduce a
   typed `ClockResyncError { TransportError, AgentStatus(u16), NonceGen, SpawnPanic }` —
   instead the four arms at `:1466-1480` are all stringified. Perpetuates the pattern code-
   quality-r7 (177 count) flags as a workspace smell.

### MINOR

6. **`ZEROSHIP_SANDBOX_TEST_DISABLE_PERSIST_ASSERTION` still has no docs outside lib.rs source** —
   R6-A1 renamed the env var (`2ead8692`) addressing r6's CRITICAL #1, but `grep` against
   `crates/sandbox/README.md` + `docs/runbooks/sandbox-*.md` for the new name returns zero hits.
   The rename surfaces test-only intent (good); the zero-runbook gap (r6 #1's deeper concern)
   carries over.

7. **R6-P1 detach spawn lifetime audit — clean** — `admin_handlers.rs:1310-1324`. The closure
   captures `state_for_teardown = Arc::clone(&state)` (a 'static `Arc<AppState>`) and
   `sandbox_id: Uuid` (Copy). `.detach()` discards the JoinHandle. No pub-by-accident lifetime
   leak. The detached task may outlive the request (intentional, per the inline comment at
   `:1283-1322`). Race-safety claim (shared `vm_index_allocator` rejects concurrent wake
   reservation) hinges on R4-A2 still being open — when the `LeasedVmSlot` RAII guard lands,
   re-verify this detach interaction.

8. **`clock_resync` handler accepts `ts=0` (Unix epoch) — controller side has no lower bound** —
   `handlers.rs:600-603` only checks `libc::time_t::try_from(parsed.ts)` (range only). The
   controller side at `restore_handler.rs:1439-1442` uses
   `SystemTime::now()` directly, so prod traffic always sends current ts; a compromised
   controller (out of threat model) could push the guest's wall clock backward to freeze the
   nonce LRU's TTL pruning at `sig.rs:NONCE_TTL_S`-bound expiry. Defense-in-depth: refuse `ts`
   more than e.g. 7 days in the past of the agent's current `unix_now()` even on the bypass
   path. The signature already binds `ts` to the body hash so the check is free.

## Summary

8 findings (1 CRITICAL, 4 MAJOR, 3 MINOR). r6-resolved: 3 (R4-S1, R5-API1, R5-API2 — three
B19-era `pub` accessors are now `pub(crate)`). r6-carryover: 2 (R4-S2, r4 #7). No in-source
movement toward the `SnapshotCapableBackend` trait split (R3-A1 / R5-A1 still open across
two rounds).

B22 surface audit verdict: the runtime fix is correct (skew-bypass binds to signature; nonce
LRU still defends replay; route is admin-equivalent via Ed25519, not a tokenless escape
hatch), but the type-system gates leaked — `verify_kind_skew_bypass` is `pub` when it should be
`pub(crate)`, the default `derive_agent_url` returns a dead-loopback sentinel, the capability
string is performative (no controller call site reads it), and the new free fn perpetuates
`Result<_, String>`.

Two most-critical citations:

- `sandbox-agent/src/sig.rs:356` — `Verifier::verify_kind_skew_bypass` is `pub`. The doc
  comment swears only `/_clock_resync` should call it; the type system says any holder of an
  `Arc<Verifier>` can. Same anti-pattern that R4-S1 / R5-API1 / R5-API2 just closed
  in-crate, regressed across the crate boundary.

- `sandbox-agent/src/version.rs:49-52` + `sandbox/src/restore_handler.rs:485-498` —
  `clock.resync-v1` capability advertised as feature-detectable for graceful fallback;
  controller calls `clock_resync_post_restore` unconditionally. Either wire the version-
  capability check into the controller or remove the misleading comment.

**R-status**: R4-S1 CLOSED, R5-API1 CLOSED, R5-API2 CLOSED, R4-S2 OPEN, r4 #7 OPEN.
