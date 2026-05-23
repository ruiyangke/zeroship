# Sandbox snapshot/restore — API-surface review (2026-05-24 r6)

Branch `feat/sandbox-snapshot-restore` @ `29196e0c`. Read-only delta
over r5 (`3e8bfad5`). Verifies R5-S1 (`7a094786`), B21
(`b25a4ea1`), R3-Q2 (`ac6a6bf2`), S4's wire-vocabulary delta.

## R-carryovers — status

- **R4-S1 `Backend::vm_index_allocator()` pub**: STILL OPEN.
  `backend/mod.rs:449-456` byte-identical to r5.
- **R4-S2 `ErrorEnvelope::with_extra(Value)` non-object drop**:
  STILL OPEN. `error_envelope.rs:74-77` signature still `Value`; the
  silent drop at l. 94 (`.as_object()` check) remains. All 7 in-crate
  callers happen to pass `json!({...})` objects; the type still admits
  the bug.
- **R5-API1 `Backend::register_restored` pub + raw `[u8; 32]` SK**:
  STILL OPEN. `backend/mod.rs:487-505` byte-identical to r5.
- **R5-API2 `Backend::nomad_ch_handle() -> Arc<NomadCHBackend>`**:
  STILL OPEN. `backend/mod.rs:467-474` byte-identical. The arch-r6
  `SnapshotCapableBackend` trait-split is proposed in
  `…architecture-2026-05-24-r6.md` but **no in-source moves landed**
  (`git grep SnapshotCapableBackend crates/` → 0).
- **r4 #7 `AppState::config()` speculative pub**: STILL OPEN
  (`lib.rs:343-345`).
- **R3-Q2 `with_persistence` Result smell**: CLOSED at `ac6a6bf2`.
  All 10 `with_*` builders now return `Self`. One asymmetry removed.

## New findings (post-r5)

### CRITICAL

1. **`SANDBOX_PERSIST_NONE_OK` test escape hatch is read on the
   production boot path** — `lib.rs:435-441`. Documented only in the
   function-internal doc (l. 803-806), the assertion error message
   (l. 821), and this deferred file — `crates/sandbox/README.md`
   and `docs/runbooks/sandbox-*.md` make zero mention. Naming
   convention (`SANDBOX_*`) is identical to production env vars;
   an operator setting it "to make the controller boot" silently
   re-enables R5-S1's fail-OPEN (cluster bug #21). Should be
   `#[cfg(test)]`-gated, renamed `SANDBOX_TEST_*`, or moved to a
   `from_config_with_test_overrides(...)` constructor. Compare
   `SANDBOX_PG_OPTIONAL` (l. 456) — same shape, same problem; a
   precedent, not a justification.

### MAJOR

2. **B21's env-var triplet has zero runbook surface** —
   `SANDBOX_PERSIST_AUTH`, `SANDBOX_AEAD_KEY_PATH`,
   `SANDBOX_PERSIST_DIR`. `grep` across `crates/sandbox/README.md` +
   `docs/runbooks/sandbox-*.md` → 0 matches. Discoverable only via
   `persist.rs:584-613`, the boot-assert error string at
   `lib.rs:816-822`, and the GCE-specific
   `gcp-worker-startup.sh:303-388`. A fresh operator deploying on
   non-GCE infra has no doc. Boot validation itself is good
   (`AeadKey::from_path` checks len=32 + mode=0o400; the assert
   checks pairing). The discoverability gap IS the API-surface
   concern: env vars ARE the controller's public configuration.

3. **23 new error codes minted with no central wire-stable
   vocabulary doc** — `admin_handlers.rs:222-227` documents `code`
   as "the stable client contract"; S4 at `4fd92bef` minted 23 codes
   (`pg_query_failed`, `gdpr_collect_ids_failed`,
   `pg_tx_isolation_failed`, etc.) across 39 `err_safe(...)` call
   sites with no central registry. Per code-quality-r6, 16 of 23 are
   single-callsite. Either (a) intern in a `pub mod error_codes`
   const block of `&'static str`, OR (b) compress single-callsite
   codes into broader categories. Today every new handler invents
   a code and the wire contract drifts silently.

### MINOR

4. **Builder count steady at 10; `with_nomad_handle` is still the
   only "miss = runtime 500 on first wake" builder** —
   `restore_handler.rs:879`. R3-Q2's close brought all 10 to `Self`
   return-type parity. But forgetting to call `with_nomad_handle`
   silently surfaces as a 500 on first wake (per r5 finding #4) —
   every other builder either has a sane `None` default or refuses
   to boot. Asymmetry survives despite the shape unification.

5. **`assert_persist_required_when_snapshot_enabled` visibility
   correct** — `lib.rs:809` is `pub(crate)`; 5 unit tests cover the
   3-bool truth table (`lib.rs:1959-2012`). R5-S1 added zero new
   public items.

## Summary

5 findings (1 CRITICAL, 2 MAJOR, 2 MINOR). r5-resolved: 1 (R3-Q2
infallible-Result on `with_persistence`). r5-carryover: 5 (R4-S1,
R4-S2, R5-API1, R5-API2, r4 #7). Builder count steady at 10. No
in-source movement toward the `SnapshotCapableBackend` trait split
despite arch-r6 + r5-A1 proposing it.

Two most-critical citations:

- `lib.rs:435-441` — `SANDBOX_PERSIST_NONE_OK` read on the
  production boot path with no `#[cfg(test)]` gate or `_TEST_`
  prefix; an operator setting it to silence the R5-S1 fail-CLOSED
  assertion silently re-enables the bug #21 fail-OPEN.
- `backend/mod.rs:487-505` (R5-API1) + `:467-474` (R5-API2) —
  unchanged from r5; arch-r6's trait-split is proposed but
  `git grep SnapshotCapableBackend crates/` returns zero. B19's pub
  escape hatches now survive two review rounds.

**R-status**: R4-S1 OPEN, R4-S2 OPEN, R5-API1 OPEN, R5-API2 OPEN.
