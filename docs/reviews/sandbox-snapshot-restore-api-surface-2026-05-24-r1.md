# Sandbox snapshot/restore — API-surface review (2026-05-24 r1)

Branch `feat/sandbox-snapshot-restore` @ `b048b491`. Read-only review of
`crates/sandbox/**` focused on `pub` leaks, error-envelope drift,
trait-stability, `Result<_, String>` proliferation, dead-code anchors,
and stale TODOs.

## CRITICAL

1. **Error envelope is non-conforming on every site** — `crates/sandbox/src/admin_handlers.rs:179-192`, `crates/sandbox/src/handlers.rs:25-44`, `crates/sandbox/src/preview_share_handlers.rs:112-114`, `crates/sandbox/src/preview.rs:836-838`, `crates/sandbox/src/snapshot_handler.rs:0` and all map_*_error sites at `admin_handlers.rs:1041-1095`. Proposal § 10.0 mandates `{"error":"<kind>","message":"<human>",...}`. Every site emits `{"error": "<human prose>"}` (`err()`) or `{"error": code, "code": code}` (preview.rs) — i.e. `error` is the human message OR a duplicate of `code`, and there is no `message` field at all. The `feature_disabled` envelope at `admin_handlers.rs:1041-1044` is the *only* one that comes close (has both `error` and `message`) — every other operator endpoint is inconsistent with both the spec and itself. This is a wire contract; clients written to § 10.0 will mis-classify every error.

2. **`AppState.admin_token` is `pub`** — `crates/sandbox/src/lib.rs:98`. `admin_handlers.rs:128-136` calls out the footgun in a comment: anything constructing `AppState { admin_token: Some(Zeroizing::new(String::new())), .. }` opens unauthenticated admin. The defense-in-depth check exists, but the field should be `pub(crate)` with a constructor so external `AppState { .. }` literals cannot compile. Same hazard applies to every other `pub` field on `AppState` (lib.rs:50-109) — `snapshot_store`, `restore_backend`, `ch_remote`, `database`, `persist` are all reachable from any downstream crate that depends on `zeroship-sandbox`.

## MAJOR

3. **Library exposes 168 `pub` items across 24 modules; ~all of `lib.rs`'s `pub mod`s should be `pub(crate)`** — `crates/sandbox/src/lib.rs:11-31`. `admin_handlers`, `auth`, `db`, `files`, `handlers`, `metrics`, `persist`, `preview`, `preview_share`, `preview_share_handlers`, `preview_ws`, `registry`, `restore`, `restore_handler`, `snapshot_aead`, `snapshot_handler`, `snapshot_store`, `snapshot_store_gcs`, `sweep` are all `pub mod`. The crate doc-string (line 4-6) says only `AppState`, the `Backend` enum, and the registry need to be reachable by integration tests / examples. Everything else (e.g. `db::Database`, `snapshot_store::SnapshotMetadata`, `restore::RestoreOutcome`, the `SealedAuth` shape) is now part of the public surface and will break consumers when refactored. Tests that need internals can use `#[cfg(test)] pub use` or `pub(crate)` + a test-only re-export module.

4. **Public traits with unstable surface** — `SnapshotStore` (`snapshot_store.rs:97`), `RestoreBackend` (`restore_handler.rs:100`), `ChRemoteClient` (`snapshot_handler.rs:100`), `SourceVmOps` (`snapshot_handler.rs:189`). All `pub`. Adding a method (e.g. `list()`, `stat()`, `prefetch()`) is a breaking change for any downstream impl. v1 has exactly one production impl per trait inside this crate. Either seal with `pub(crate)` until v2 ships GCS retry / cross-host fetch, or add `#[doc(hidden)] fn __sealed(&self);` per the std-lib sealed-trait pattern. `RestoreBackend::release_vm_index` (line 108) returns `()` while `reserve_vm_index` returns `Result<(), String>` — asymmetric and untyped.

5. **`Result<_, String>` proliferation: 164 occurrences across 16 files**, with the worst offenders driving `.contains("...")` matches that will break on rewording. Top offenders by signal-loss:
   - `backend/mod.rs:178,187,218,246,254,267,275,289,297,310,318,337,359,384,416,433,478` — the entire `Backend` enum public API is `Result<_, String>`. `restore_from_sealed`'s caller at `restore.rs:379` does `e.contains("doesn't yet support") || e.contains("backend mismatch")` — substring match on an error message that is rewritten in three call sites of `mod.rs`. Should be a `BackendError { Unsupported, NotFound, Io(_), … }` enum.
   - `restore_handler.rs:100-140` — every trait method on `RestoreBackend` returns `Result<_, String>`; the handler immediately wraps them in `RestoreHandlerError::Backend(String)` (line 82-83) losing structure.
   - `snapshot_handler.rs:103,108,203` — `ChRemoteClient` and `SourceVmOps` return `Result<_, String>`; the handler then wraps in `SnapshotHandlerError::ChRemote(String)` / `Internal(String)`.
   - `db.rs` — `Database` methods return `Result<_, DatabaseError>` (good!), but `lib.rs:189,212,224` immediately stringify with `format!("...: {e}")`, defeating the typed error. The 25+ `format!("...: {e}")` sites in `lib.rs` alone collapse pg-error variants into opaque strings.

## MINOR

6. **`#[allow(dead_code)] fn _arc_anchor()` / `_db_anchor()` are compiler-comments masquerading as code** — `snapshot_handler.rs:892-894`, `restore_handler.rs:1295-1297`, `sweep.rs:411-412`. The restore_handler one claims to "silence unused-Arc warning if no caller imports the alias" but `Arc<Mutex<_>>` is used live at lines 713 and 739 — the anchor is dead code AND its stated justification is false. The snapshot_handler one is the only place in that file referencing `Arc` (line 45 import is otherwise unused on the non-test path) — delete the import and the anchor together. `sweep.rs::_db_anchor(_: &Database)` is similar: `Database` is used live at line 37, 113, 173 — the anchor is pure cargo-cult.

7. **`AdminSandboxRow` struct is `pub` with 15 `pub` fields** — `admin_handlers.rs:212-229`. This is a response DTO; making the struct `pub` plus every field `pub` means a downstream consumer can read AND construct it (relevant if any handler trusts a deserialized value). Either `pub(crate)` the struct or seal the constructor (`#[non_exhaustive]` at minimum).

8. **`ListSandboxesQuery` fields all `pub`** — `admin_handlers.rs:198-205`. Same issue. The deserializer needs the fields visible to `serde`, not to other crates. `pub(crate)` is sufficient.

9. **Stale TODOs (proposal-tagged, no tracking link):**
   - `db.rs:471` — "TODO (round-1 fixer / IMPORTANT #10): every method on `Database` calls `open_pool` and drops the pool". 13-line comment, no issue link, marked "current pattern is correct, just slow on the hot path" — but `from_config` runs this on every admin endpoint. Either land the thread-local or remove the apologetic comment.
   - `snapshot_store_gcs.rs:774` — "TODO (GCS PR): add a retry loop". The fire-and-forget upload silently drops on transient GCS error; no metric is incremented (compare to the `l2_upload_failed_total` the TODO promises). Until that lands the L2 tier is effectively best-effort with no observability.
   - `backend/k8s.rs:495` — "see TODO in restore_from_sealed" but no such TODO exists at the destination (the round-8 restore is nomad-ch-only; the k8s path returns `Err` at `backend/mod.rs:366-371`). Stale cross-reference.

10. **`pub const DEFAULT_BACKING_VERSIONS` JSON-as-string sentinel** — `snapshot_handler.rs:211-212`. Hardcoded `{"keys":"unknown","userhome":"unknown","rootfs_overlay":"unknown"}` as a `&'static str`. CHECK constraint–satisfying placeholder; should be `pub(crate)` and ideally a typed struct that serializes — a string literal isn't validated against the CHECK at compile time and the next column-name change will silently 500.

11. **`pub mod backend` re-exports leak internal types** — `backend/docker.rs:76` `pub(crate) struct DockerSandbox` (good), but `backend/mod.rs:167-170` exposes `Backend::Docker(docker::DockerBackend)`, `K8s(k8s::K8sBackend)`, `NomadCh(nomad_ch::NomadCHBackend)` — all backend types and their internals are pub via the enum variants. Variants should hold `pub(crate)` newtypes or the enum itself should be `#[non_exhaustive]`. Today a downstream crate can pattern-match on `Backend::NomadCh(b)` and call any `pub` method on `NomadCHBackend`.

## Summary

- **Critical:** 2 (error envelope drift across every handler; `pub` mutable state on `AppState`)
- **Major:** 3 (over-broad `pub mod` exports, public traits with no stability hedge, 164-site `Result<_, String>` smear)
- **Minor:** 6 (dead-code anchors, pub DTO fields, stale TODOs, sentinel const, leaky enum variants)

Total: 11 findings. The error-envelope and `pub` AppState items should block PR-merge; the rest are pre-1.0 cleanup that can land alongside the typed-error refactor.
