# Sandbox snapshot/restore — API-surface review (2026-05-24 r5)

Branch `feat/sandbox-snapshot-restore` @ `3e8bfad5`. Read-only delta
over r4 (`29afea72`). Verifies B19 (`15b4f9a8`), A3-partial
(`0aa93a0f`), S4 (`4fd92bef`).

## R4 carryovers — status

- **R4-S1 `Backend::vm_index_allocator()` pub**: STILL OPEN.
  `backend/mod.rs:449-456`. No demotion this round; still pub on
  `pub mod backend`, zero out-of-crate callers, still hands out
  `Arc<Mutex<VmIndexAllocator>>`.
- **R4-S2 `ErrorEnvelope::with_extra()` non-object drop**: STILL
  OPEN. `error_envelope.rs:93-104` byte-identical to r4; signature
  still `Value`, body still silently no-ops on non-object input.
- **r4 #4 `with_persistence` Result smell**: CLOSED. `lib.rs:276`
  now returns plain `Self` (doc-comment l. 265-271 cites the r3/r4/r5
  reviews). One asymmetry removed.
- **r4 #7 `AppState::config()` speculative pub**: STILL OPEN.
  `lib.rs:343-345` unchanged; zero out-of-crate callers.

## New findings (post-B19, post-S4)

### CRITICAL

1. **`Backend::register_restored()` is `pub` on `pub mod backend`**
   — `backend/mod.rs:487-505`. Same speculative-`pub` smell as R4-S1,
   but worse signature: takes a raw `[u8; 32] signing_key_bytes` by
   value (Ed25519 SK seed). Any out-of-crate caller can synthesize a
   bogus key, fabricate an `agent_url`, and install a phantom record
   into `NomadCHBackend::state` — bypassing the create-path's
   keypair-minting + sealed-record write. Zero out-of-crate callers
   (only `restore_handler.rs:1040` via the trait). The inner
   `NomadCHBackend::register_restored` is correctly `pub(crate)`
   (`nomad_ch.rs:1650`); the enum wrapper widens it for no reason.
   Should be `pub(crate) fn register_restored(...)`.

2. **`Backend::nomad_ch_handle()` returns `Arc<NomadCHBackend>` on
   pub surface** — `backend/mod.rs:467-474`. Hands the
   `Arc<NomadCHBackend>` to anyone; downstream code can call any
   `pub` method on `NomadCHBackend` directly, bypassing the enum
   contract (the comment at l. 34-40 promises "enum dispatch is the
   only contract"). The R5-A1 architecture finding flagged the same
   leak; from an API-surface lens it's a CRITICAL `pub`-to-`pub`
   internals escape hatch. Should be `pub(crate)`.

### MAJOR

3. **`RestoreBackend::register_restored` default impl silently
   returns `Ok(())`** — `restore_handler.rs:162-170`. The trait is
   `pub` on a `pub mod` (l. 100), consumed by `tests/sandbox_admin_e2e.
   rs:754`. The doc-comment (l. 156-159) calls the default a "no-op
   `Ok(())`" "so the in-crate StubRestoreBackend doesn't need to
   implement state-map registration just to keep the existing
   pg-gated tests compiling". That's an in-crate convenience baked
   into the **public** trait surface — every out-of-crate impl now
   silently inherits a no-op that masks the pre-B19 bug
   (`do_restore_inner` returns Ok, state-map insert never happens,
   slot leaks). Fix shape: drop the default impl (force every impl
   to be explicit) or split the registration step into a separate
   trait, so the production `RealRestoreBackend` is the only opt-in
   path and stubs must literally opt out.

4. **Builder accretion: 9 → 10 `with_*` builders on the API surface**
   — `with_admin_token`, `with_persistence`, `with_config`,
   `with_database`, `with_snapshot_store`, `with_ch_remote`,
   `with_restore_backend` (lib.rs:217-378), `with_token`
   (config.rs:599), `with_shared_allocator`, **`with_nomad_handle`**
   (restore_handler.rs:862, 879). The r4 review counted nine; B19
   added the tenth. None of these are guarded by a typed-builder
   pattern (compile-time required-field enforcement) — the
   construction site at `lib.rs:619-654` is now a sequence of
   `Option`-mediated chains where forgetting `.with_nomad_handle(h)`
   silently surfaces as a 500 at runtime on first wake. The "by
   accretion" pattern from r3/r4 has continued.

5. **`Backend::register_restored` is the only **non-async** method
   on the trait dispatch surface** — `backend/mod.rs:263-573`. Every
   sibling `Backend::*` is `async fn`; the new `register_restored` /
   `nomad_ch_handle` / `vm_index_allocator` are sync. The doc at
   l. 51-53 promises "Every method is async; long-running CLI
   shell-outs run on `compio::runtime::spawn_blocking`". The
   asymmetry isn't wrong (register_restored is in-memory only), but
   the trait-doc contract should either be widened or the method
   moved off the enum onto a side-channel API. Consistency smell.

### MINOR

6. **`err_safe()` visibility correct, naming pair OK** —
   `admin_handlers.rs:228-245`. Module-private `fn` (no `pub`); the
   r4 review's `error_response()` shorthand from `error_envelope.rs`
   sits at the bottom of the chain (l. 244 `error_response(sc, code,
   public_msg)`), so the layering is `err_safe → error_response →
   ErrorEnvelope`. Naming is fine — `err`/`err_safe` is a pair, and
   the file scoping is unambiguous. No finding here other than to
   record that S4 added no public surface.

7. **A3-partial introduced no new pub items** —
   `snapshot_store.rs:1-150` shows the existing pub set is intact;
   the hard_link change lives entirely inside `LocalDiskSnapshotStore::
   get` (private method). Confirmed.

8. **Trait default-impl + `signing_key_bytes: [u8; 32]` is a
   double-smell** — `restore_handler.rs:162-170` (trait) +
   `backend/mod.rs:487-494` (enum method). Both accept the SK as
   raw `[u8; 32]`; the `SandboxAuth` (l. 91-96) and the in-crate
   `signing_key: Arc<SigningKey>` wrap suggest the canonical
   in-memory form is `Arc<SigningKey>`. Passing raw bytes makes the
   surface "secret leaks fine" at the type level. Should accept
   `Arc<SigningKey>` (zero-copy clone, matches the rest of the
   crate's discipline).

## Summary

10 findings (2 CRITICAL, 3 MAJOR, 3 MINOR). r4-resolved: 1
(`with_persistence` now plain `Self`). r4-carryover: 3 (R4-S1,
R4-S2, r4 #7 `config()`). Builder count 9 → 10 (`with_nomad_handle`).
Two most-critical citations:

- `backend/mod.rs:487` — `Backend::register_restored` is `pub` and
  takes raw SK bytes by value, bypassing the create-path's
  key-minting + sealing.
- `backend/mod.rs:467` — `Backend::nomad_ch_handle()` hands
  `Arc<NomadCHBackend>` to any downstream caller, voiding the
  enum-as-only-contract promise in the module doc.

B19 added one new `pub` method on the enum trait dispatch surface
(`register_restored`), one new `pub` escape hatch
(`nomad_ch_handle`), one new `pub` trait method with a silent-Ok
default, and one new `with_*` builder — the API surface widened in
four places for one bug fix. The R5-A1 architecture finding
(enum-as-trait-with-escape-hatch) maps 1:1 to this surface
expansion.
