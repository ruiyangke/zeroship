# Code-quality review — 2026-05-23 round 1

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: 8ad3cf3f
**Lens**: code-quality

## Summary

- 17 findings (2 critical = prod-crash risk, 8 important, 7 minor).
- The hot files are reasonably disciplined: most `.unwrap()` /`.expect()` hits are in `#[cfg(test)]` blocks or behind documented invariants. The real cost-of-ownership issues are **stringly-typed status checks**, **inconsistent `PoisonError` handling**, **magic timeouts repeated 10+ times**, and a few **giant `fn`s in `nomad_ch.rs`**.

## CRITICAL (prod-crash potential)

- `crates/sandbox/src/preview.rs:239,268` — `state.backend.session_auth(sandbox_id_opt.unwrap())…` and `let info = info_opt.unwrap();`
  Why: today `authorized` (line 187-192) guarantees both are `Some`, so it cannot trigger. But the next refactor of the authorization branching will quietly turn this into a prod panic; the file is a hot HTTP request path, so each one is a 5xx (or worse, the actix worker thread dies).
  Fix: bind `(Some(p), Some(info), true)` once in the `match` and pass `info`/`p` down by reference. No `unwrap()`s on the success arm.
- `crates/sandbox/src/preview.rs:629-631` — `let mode = mode.unwrap(); let dest = dest.unwrap(); let site = site.unwrap();` immediately after a guard that returned 400 on `None`.
  Why: same shape as above — the guard and the unwrap are 10 lines apart. A future "we also let `Sec-Fetch-Site: none` come through without `Sec-Fetch-Mode`" tweak deletes the guard but not the unwrap.
  Fix: re-bind in the guard (`let (Some(mode), Some(dest), Some(site)) = (mode, dest, site) else { return Err(...); }`).

## IMPORTANT (maintenance cost)

- `crates/sandbox/src/admin_handlers.rs:348-360` — `is_known_status(s: &str)` hard-codes 8 status strings; the `SandboxStatus` enum in `db.rs:1172-1189` has 14 variants. The 6 snapshot/restore lifecycle states (`Snapshotting`, `Snapshotted`, `SnapshottingAborted`, `SnapshottedSuspect`, `Restoring`, `RestoringCold`) are silently reported as "unknown" by `/admin/sandboxes?status=…` filtering. This is a stringly-typed copy that has already drifted.
  Fix: `SandboxStatus::from_str_opt(s).is_some()` and delete the local list. Make the param `SandboxStatus` at the handler boundary.

- `crates/sandbox/src/snapshot_store.rs:105-130` (trait `SnapshotStore`) — every method takes `sandbox_id: &str`, but every caller has a `Uuid` and formats it to `"sbx_<base62>"` at the call site. The store re-parses nothing; the `&str` is only used as a path segment.
  Why: this is the seam between Rust types and on-disk layout; today it's stringly so the same Uuid can be encoded two ways (simple-hyphen vs typed base62) and produce two L1 dirs. (`db.rs:1032-1033` shows both shapes already collide in `seal_filename_for_str`.)
  Fix: take `&Uuid` (or a `SandboxKey` newtype) and centralise the on-disk encoding in one helper.

- `crates/sandbox/src/registry.rs:196,205,239,252,298-299,308,319,331,347,349-350,363,365` — 31 `.read().unwrap()` / `.write().unwrap()` calls on `RwLock<HashMap>`s. By contrast `nomad_ch.rs` consistently uses `.write().unwrap_or_else(|p| p.into_inner())` to survive a poisoned mutex (21 sites). The codebase has two incompatible policies for `PoisonError`; if any sandbox HTTP handler panics under load, the registry locks poison and every subsequent request to a healthy sandbox dies. Same pattern in `backend/k8s.rs` (8 sites) and `backend/docker.rs` (6 sites).
  Fix: pick one policy (`into_inner`) and apply it everywhere via a tiny helper like `fn read_lock<T>(l: &RwLock<T>) -> RwLockReadGuard<T>`.

- `crates/sandbox/src/backend/nomad_ch.rs:879-1128` — `pub async fn stop(…)` is 249 LOC, 9 numbered steps in one body, mixed `errs.push(...)` and `tracing::warn!` per step. Sibling `lookup_source_vm_ops` (1516-1924) is **410 LOC** in a single body — well past the 200-LOC ceiling. These are change-amplifiers: every cluster bug-fix has had to re-read them in full.
  Fix: split each numbered step into its own `async fn step_drain_agent`, `step_purge_nomad`, etc.; keep the public fn as a thin orchestrator.

- `crates/sandbox/src/backend/nomad_ch.rs` and `restore_handler.rs` / `snapshot_handler.rs` — `Duration::from_secs(5)` appears 11×, `(10)` 5×, `(15)` 4×, `(30)` 4×, `(60)` 3× across these three files with no shared constants. Same shape for `Duration::from_millis(250)` poll-tick.
  Fix: a small `timeouts.rs` (or `consts` mod): `NOMAD_GET_TIMEOUT`, `NOMAD_STOP_TIMEOUT`, `AGENT_PROBE_TIMEOUT`, `ALLOC_POLL_INTERVAL`, `STARTUP_GRACE`. Today the operator has no way to tune any of these without grep-replacing literals.

- Each per-sandbox host path has a different name in each module:
  `backend/nomad_ch.rs` says `host_dir` ≡ `<host_state_dir>/<sandbox-id>/`;
  `restore_handler.rs` says `alloc_dir` ≡ `<host_state_dir>/<sandbox-id>/restore/`;
  `snapshot_handler.rs:675` introduces `snap_stage_dir` ≡ `<host_state_dir>/snap-stage/<sandbox-id>/`.
  Reader has to keep three layout invariants in their head; only one of them lives in code (`snap_stage_dir` helper). `host_dir` and `alloc_dir` are recomputed inline at many call sites.
  Fix: a `paths.rs` module with `host_dir(host_state, sbx)`, `restore_alloc_dir(host_state, sbx)`, `snap_stage_dir(host_state, sbx)`. Make `restore_alloc_dir` derived from `host_dir` so the parent invariant ("alloc is a child of host_dir") is type-enforced.

- `crates/sandbox/src/*.rs` (13 files) — 142 functions return `Result<_, String>`. Errors are formatted at the throw site (`format!("nomad GET {url}: {e}")`), so callers can't pattern-match on kind; the snapshot/restore handlers' `map_…_error` shims (`admin_handlers.rs:1049-1112`) demonstrate the cost. Stringly-typed errors crossing 13 files is the canonical "we'll regret this" smell.
  Fix: per-module error enums (`NomadError`, `RestoreError`) with `#[derive(thiserror::Error)]`; surface `Display` for the wire side, but keep the variants for in-code matching.

- `crates/sandbox/src/lib.rs:106-108, 313-353` — `Option<Arc<dyn SnapshotStore>>` / `Option<Arc<dyn ChRemoteClient>>` / `Option<Arc<dyn RestoreBackend>>` and a 40-line wiring block. The `Option` exists only so admin tests can leave the field unset; in prod the three are always `Some` after `from_config`. The current shape forces every call site to `.as_ref().ok_or_else(|| feature_disabled())` (see `admin_handlers.rs:1038`).
  Fix: a `SnapshotWiring` struct holding all three `Arc<dyn _>` non-optionally, with a `disabled()` constructor that returns stub impls — so the disabled-feature branch is a single check at construction.

## MINOR

- `crates/sandbox/src/restore_handler.rs:1295-1298` and `sweep.rs:411-412` and `snapshot_handler.rs:892-895` — three different `#[allow(dead_code)] fn _arc_anchor` / `_db_anchor` "silence-the-unused-import" workarounds. None have a tracking link. Better than `#[allow]` is a `pub use` re-export or just dropping the import.

- `crates/sandbox/src/backend/nomad_ch.rs:1493-1500` — `restore_from_sealed` is `#[allow(dead_code)]` and returns `Err("…round-8 deprecated…")` unconditionally. Either delete it and update the legacy tests, or move it under `#[cfg(test)]`.

- `crates/sandbox/src/snapshot_store_gcs.rs:774` — `// log a warn and exit. **TODO (GCS PR):** add a retry loop`. No issue link. Track or strip the marker.

- `crates/sandbox/src/db.rs:471` — `**TODO (round-1 fixer / IMPORTANT #10):**` references a review-cycle artefact that won't survive; tie to a real tracker or remove.

- `crates/sandbox/src/snapshot_store_gcs.rs:1130` — `eprintln!("GCS_TEST_BUCKET unset; skipping");` — even in a test, mixing `eprintln!` with `tracing` is a noise vector under `RUST_LOG=info`. Use `tracing::warn!`.

- `crates/sandbox/src/handlers.rs:1026,1047,1079,1115,1144` — `run_create_with_retry(3, Duration::from_secs(10), …)` x5. Both values are clearly intended to be the same policy; bind them as `MAX_CREATE_ATTEMPTS` and `CREATE_BACKOFF`.

- `crates/sandbox/src/restore_handler.rs:1047,1073,1077` — `last_status: Option<String>` and `cs == "running"` / `"failed"` / `"lost"` string compares against Nomad's `ClientStatus`. Fine for a foreign API, but worth a tiny `enum NomadClientStatus { Running, Failed, Lost, Pending, Other(String) }` parsed once.

## Patterns worth pulling into a shared util

- **`lock()` poison policy**: 91 `.lock()/.read()/.write()` sites across 7 files, split 30/61 between `unwrap_or_else(|p| p.into_inner())` and `.unwrap()`. One helper, one policy.
- **Per-sandbox path derivation**: `host_dir`, `restore_alloc_dir`, `snap_stage_dir`, `workspace_image_path` — collect into `paths.rs` and stop recomputing inline.
- **Magic durations** (`Duration::from_secs(5)/(10)/(15)/(30)`): 30+ occurrences in three files; one `timeouts.rs` with named constants.
- **`Result<_, String>` everywhere**: 142 sites. Move to typed `thiserror` enums per module — the boundary cost is ~30 LOC per crate, vs. the on-going cost of `.contains("not found")` matching at call sites.
- **`format!("sbx_{}", typed_id::uuid_to_base62(&id))`**: appears 30+ times. Single `fn sbx_typed(id: Uuid) -> String` (or `Display` impl on a `SandboxKey` newtype) and the on-disk-vs-Uuid drift in finding #2 disappears for free.
