# Sandbox snapshot/restore — API-surface review (2026-05-24 r2)

Branch `feat/sandbox-snapshot-restore` @ `3b888a2a`. Read-only delta over r1
(`b048b491`). Verifies r1 closures, checks new surface introduced by `2e0d17f7`
(A5), `f32507ce` (A2), and `4a7e8e03` (T6).

## Closure status carried from r1

- **A5 — `AppState.admin_token` was `pub`**: **CLOSED** at `2e0d17f7`.
  `lib.rs:106` now `pub(crate) admin_token: Option<zeroize::Zeroizing<String>>`,
  with a safe builder `AppState::with_admin_token` (`lib.rs:173-190`) that
  `Err`s on empty strings, a read-only accessor `admin_token()`
  (`lib.rs:198-200`), and a `new_fixture(...)` ctor (`lib.rs:213-227`). Three
  unit tests in `admin_token_setter_tests` (`lib.rs:1224-1269`) pin
  empty/non-empty/None semantics. Clean.
- **A4 — Error envelope §10.0 violated globally**: **STILL OPEN**. Verified
  at `admin_handlers.rs:179-196` (`unauthorized()` + `err()` still emit
  `{"error":"<prose>"}`, no `message` field) and `handlers.rs:21-44`
  (identical pattern duplicated). Only `feature_disabled` at
  `admin_handlers.rs:1043-1051` matches the spec.
- **`Result<_, String>` proliferation**: r1 counted 164; now **167** across
  16 files (`lib.rs:3`, `backend/mod.rs:17`, `backend/docker.rs:20`,
  `backend/k8s.rs:33`, `backend/nomad_ch.rs:38`, `restore_handler.rs:17`,
  `snapshot_handler.rs:10`, …). Drift is small but the wrong direction.
- **Dead-code anchors `_arc_anchor`/`_db_anchor`**: still present at
  `snapshot_handler.rs:893`, `restore_handler.rs:1296`, `sweep.rs:556`.
- **`pub mod` blast radius (`lib.rs:11-31`)**: unchanged — 21 `pub mod`s
  still re-exported; only `admin_token` was narrowed.

## New findings (post-A5/A2/T6)

### CRITICAL

1. **`AppState::new_fixture` is `pub` but builds production-shaped state with
   silent `None` wiring** — `lib.rs:213-227`. The doc comment says "tests
   mutate the still-`pub` fields directly". This is the A5 footgun, just at
   a different field: `state.persist = Some(arbitrary_arc)` /
   `state.snapshot_store = Some(arbitrary_dyn)` are still trivially settable
   from out-of-crate code because `lib.rs:65,71,114-116` left those fields
   `pub`. The A6 entry in deferred already calls out `persist`; the snapshot
   trio is the same hazard. Either gate `new_fixture` behind
   `#[cfg(any(test, feature = "test-fixtures"))]` or make all
   wiring fields `pub(crate)` and add `with_*` builders mirroring
   `with_admin_token`.

### MAJOR

2. **`ControllerIdleSnapshotter` is `pub` with `pub fn new`** —
   `sweep.rs:292-300`. The struct is wired only by `lib.rs:531-534` (a
   single in-crate construction). r1 finding #3's argument applies verbatim:
   downstream crates can now construct one with any `Arc<AppState>` and call
   `IdleSnapshotter::snapshot_one` on it from outside, bypassing the sweep
   loop's shutdown/concurrency gates. Should be `pub(crate) struct
   ControllerIdleSnapshotter` with `pub(crate) fn new`. Same fix shape
   applies to `RecordingIdleSnapshotter` (`sweep.rs:260`) — it carries
   `#[doc(hidden)]` but its `pub seen` / `pub fail` fields are still
   externally writable.

3. **`pub trait IdleSnapshotter` joins the unsealed-trait family** —
   `sweep.rs:249-254`. Same hazard r1 finding #4 raised against
   `SnapshotStore`/`RestoreBackend`/`ChRemoteClient`/`SourceVmOps`: one
   production impl (`ControllerIdleSnapshotter`), one test impl
   (`RecordingIdleSnapshotter`), zero out-of-crate impls — but the trait is
   `pub`, so adding `fn shutdown(&self)` or `fn snapshot_many(...)` is a
   breaking change. Seal with `pub(crate)`.

4. **`Arc<dyn …>` everywhere where a generic would do** — `lib.rs:114-116`
   stores `Option<Arc<dyn SnapshotStore>>`, `Option<Arc<dyn ChRemoteClient>>`,
   `Option<Arc<dyn RestoreBackend>>`. Each handler call pays a vtable
   indirection for traits with one prod impl each. `AppState<S, C, R>` with
   defaulted type params or an `enum SnapshotWiring { Disabled, Local(...),
   Tiered(...) }` would erase the vtable, surface the wiring shape in the
   type, and let `admin_handlers::snapshot_sandbox` dispatch statically.
   Cost: one type-param on `AppState`. Not a hot path today, but the trait
   objects also defeat `#[non_exhaustive]`/sealing options entirely.

### MINOR

5. **`AppState::admin_token()` accessor returns the secret as `&str`** —
   `lib.rs:198-200`. The whole reason `admin_token` was wrapped in
   `Zeroizing<String>` was to scrub the heap; handing out a `&str` lets any
   caller copy it into a non-zeroizing `String`. Either return
   `Option<&Zeroizing<String>>` or remove the accessor and force callers
   through `admin_check` (the only legitimate consumer).

6. **`pub use` re-exports**: none found in `lib.rs` — good.

7. **`#[doc(hidden)]` audit**: 17 occurrences across 6 files
   (`db.rs`, `restore_handler.rs`, `restore.rs`, `snapshot_handler.rs`,
   `sweep.rs`, `metrics.rs` with 11). `#[doc(hidden)] pub` is hiding-not-
   sealing; the items are still part of the SemVer surface. Of these, the
   `metrics.rs` cluster (11) is the worst — those should be `pub(crate)`.

8. **A2 fix at `f32507ce` — no new `pub` leakage**:
   `verify_canonical_sha256_from_streams` at `snapshot_store_gcs.rs:621` is
   correctly a private `fn`. Verified clean.

## Summary

- **Critical:** 1 (new) + **1 still open from r1** (A4 envelope) = 2
- **Major:** 3 new (`ControllerIdleSnapshotter` pub, unsealed
  `IdleSnapshotter` trait, `Arc<dyn …>` over generics) + **3 still open
  from r1** (`pub mod` blast radius, 167-site `Result<_, String>`,
  unsealed traits — now 5 of them with `IdleSnapshotter`)
- **Minor:** 3 new (`admin_token()` leaks `&str`, `#[doc(hidden)]` overused
  in metrics, dead-code anchors still present)

A5 cleanly closed; A4 still bleeding across every operator-visible
endpoint. The T6 land introduced one CRITICAL-equivalent surface
(`new_fixture` + still-`pub` wiring fields), plus a fresh trait/struct pair
that should have been `pub(crate)` from the start.
