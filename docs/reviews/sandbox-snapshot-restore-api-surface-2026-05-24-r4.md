# Sandbox snapshot/restore — API-surface review (2026-05-24 r4)

Branch `feat/sandbox-snapshot-restore` @ `29afea72`. Read-only delta over r3
(`03d15012`). Verifies A4 closure (5 commits `33f569ec`…`2928d5ae`),
A7 (`a9e568a2`), B18 (`469e22c8`, `b4ddb98b`).

## A4 closure — VERIFIED

- `crates/sandbox/src/error_envelope.rs` exists; module is
  `pub(crate) mod error_envelope` (`lib.rs:16`). Public surface is the
  `ErrorEnvelope` struct (`pub(crate)`), three builders
  (`new` / `with_extra` / `no_store`), `into_response`, and the
  `error_response()` shorthand. All five items are `pub(crate)`. Five
  in-module tests pin the wire shape (renders `error`+`message`; extra
  merges; reserved-key clobber blocked; `no_store` header; default no
  `Cache-Control`).
- `grep "\"error\":" crates/sandbox/src/` now returns **zero** live
  `error_response`-bypass survivors. The single literal match at
  `restore_handler.rs:1294` is a **test fixture** (fake-Nomad 500 body),
  not a handler emission — does not count.
- All four legacy helpers route through `error_response`:
  `handlers.rs::err` (l. 38-48), `admin_handlers.rs::err` (l. 193-201),
  `preview.rs::{uniform_401,uniform_404,uniform_413,err_with_code}`
  (l. 830-848), `preview_share_handlers.rs::{unauthorized,not_found,
  bad_request}` (l. 99-113). Adherence 18/18 = **100 %**.

## Result<_, String> trend

172 sites across 16 files (r3: 153 / r2: 167 / r1: 164). **Regressed +19
since r3** — `nomad_ch.rs` 35→38, `k8s.rs` 30→33, `docker.rs` 18→20,
plus `backend/mod.rs` now contributes 17 (was 0 in r3 count). The B18
plumbing (`with_shared_allocator`, `vm_index_allocator` wiring through
the trait) added Result-of-String returns rather than introducing a
typed error. Not blocking A4, but the direction has flipped again.

## New findings

### CRITICAL

1. **`Backend::vm_index_allocator()` is `pub` on a `pub mod backend`**
   — `backend/mod.rs:440-447`. Zero out-of-crate callers (only consumer
   is `lib.rs:617`, in-crate). Same speculative-`pub` smell flagged
   for `AppState::config()` in r3 #3, but worse: this returns an
   `Arc<Mutex<VmIndexAllocator>>` — handing the slot-pool lock to any
   downstream crate would let an external caller `lock()` the mutex
   and deadlock create/restore on that worker. Should be
   `pub(crate) fn vm_index_allocator(&self)`. The underlying
   `nomad_ch::VmIndexAllocator` (`nomad_ch.rs:269`) and its `pub fn
   {new,alloc,release,reserve}` (l. 282-328) are also `pub` on a `pub
   mod`, exposing the entire allocator surface despite no external
   contract.

### MAJOR

2. **`with_extra` silently drops non-object `Value`s** —
   `error_envelope.rs:93-104`. If a caller passes
   `with_extra(json!("string"))` or `with_extra(json!([1,2]))`, the
   `extra.as_object()` check returns `None` and the extra is discarded
   with no warning, no debug-assert, no panic. Every existing caller
   passes `json!({…})` so it works in practice, but the signature
   `fn with_extra(self, extra: Value)` accepts anything `Value` and
   the only invariant lives in the implementation. Should be
   `fn with_extra(self, extra: serde_json::Map<String, Value>)` — the
   type system then enforces "object" at the call site, eliminating
   the silent-drop class of bug.

3. **`code: &'static str` does not constrain to snake_case** —
   `error_envelope.rs:56-60,116-120`. Module doc (l. 24-28) says
   `code` is snake_case, but the type accepts any `&'static str`. A
   future caller can pass `"BadRequest"`, `"sandbox not found"`, or
   `""` and the wire body will carry it verbatim. The current 38
   callers are clean, but the discipline is by-convention, not
   by-type. Cheap fix: a `debug_assert!(code.chars().all(|c|
   c.is_ascii_lowercase() || c == '_') && !code.is_empty())` inside
   `ErrorEnvelope::new`. The cost-conscious version: pin a small enum
   `ErrorCode` and route the 38 sites through it. Either kills the
   regression risk.

4. **Builder Result-smell from r3 #6 unchanged, plus new infallible
   builder** — `lib.rs:273` `with_persistence` still returns
   `Result<Self, String>` and unconditionally succeeds (l. 278:
   `Ok(self)`). A7 added `with_token` (`config.rs:599`) returning
   `Self`; B18 added `with_shared_allocator` (`restore_handler.rs:764`)
   returning `Self`. The crate now has **8 infallible-`Self` builders
   and 1 infallible-`Result<Self, String>` builder** — the asymmetry
   is the smell. Resolve: drop the `Result` wrap on `with_persistence`.

### MINOR

5. **`error_envelope::test_helpers` is `pub(crate)` but `#[cfg(test)]`
   only** — `error_envelope.rs:124-139`. Visibility is correct
   (`pub(crate) mod test_helpers`), but the module is `#[cfg(test)]`,
   so the `pub(crate)` is dead surface to non-test in-crate code. Drop
   to `pub(super)` to match its actual reach (only `tests` below it
   uses `body_json`); if other src-tree tests start using it, widen
   then.

6. **17 `#[doc(hidden)] pub` unchanged from r2/r3** — same 11 in
   `metrics.rs`, 2 in `db.rs`, 1 each in `restore_handler.rs`,
   `snapshot_handler.rs`, `restore.rs`, `sweep.rs`. r3 #5 carried;
   still the cluster `metrics.rs:204-279` should be `pub(crate)`.

7. **`AppState::config()` accessor still has zero out-of-crate
   callers** — `lib.rs:343-345`. r3 #3 carried — A6b doc rationale
   (l. 339-342) says "out-of-crate integration tests sometimes need
   to read back", but `grep` across `crates/sandbox/tests/` shows
   zero hits. Either narrow to `pub(crate)` or land the test that
   justifies the widening.

## Summary

- **Critical:** 1 — `Backend::vm_index_allocator()` `pub` exposes
  the slot-pool lock externally
- **Major:** 3 — `with_extra` silent drop on non-object, `code:
  &'static str` lacks snake_case enforcement, `with_persistence`
  Result-smell unchanged
- **Minor:** 3 — `test_helpers` over-visible, `doc(hidden)` cluster,
  speculative `pub fn config()`

A4 cleanly closed (5 commits, 21 new wire-shape tests, 18/18 sites
compliant). A7 + B18 added two new `with_*` builders bringing the
total to **9**; the `with_persistence` `Result` asymmetry from r3 #6
is now starker against 8 infallible siblings. B18 introduced the only
new critical: `Backend::vm_index_allocator()` should be `pub(crate)`.
`Result<_, String>` count regressed 153→172.
