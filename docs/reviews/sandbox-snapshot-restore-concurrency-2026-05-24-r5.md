# Concurrency Review (r5) — sandbox snapshot/restore

**Branch:** `feat/sandbox-snapshot-restore` (HEAD `3e8bfad5`)
**Scope:** `crates/sandbox/**`
**Date:** 2026-05-24 UTC · **Reviewer:** code-critic (Opus)
**Prior rounds:** r1 (11) · r2 (8) · r3 (7) · r4 (7)

---

## Prior-round closure status

- **r4 #1 (C3 widened by `clear_snapshot_metadata` at `restore_handler.rs:484`)** — **STILL OPEN, widened again** (see finding 1).
- **r4 #2 (B17 wrapper background subshell un-reaped)** — **STILL OPEN, verbatim** at `nomad-vm-wrapper.sh:388-419` (see finding 2).
- **r4 #3 (R4-A2 / B19 RAII gap)** — **PARTIALLY CLOSED.** B19's `register_restored` (`nomad_ch.rs:1650-1681`) does insert into the state map post-livez, so the leak symptom is gone — but the insert is itself unguarded by RAII and adds a new failure window (finding 3).
- **r4 #4 (B18 shared allocator clean)** — **CLOSED.**
- **r4 #5 (T7 join_all no deadlock)** — **CLOSED.**
- **r4 #6 (A4 envelope concurrency-neutral)** — **CLOSED.** S4 `err_safe()` is synchronous; concurrency-neutral too.
- **r4 #7 (idle-eviction signal noise)** — **STILL OPEN.**

---

## CRITICAL

### 1. C3 widened a third time: `register_restored` adds a NEW post-livez await before status-CAS
`restore_handler.rs:450-463`. The B19 fix interposes **two awaits** (`persist.unseal(...).await` at line 451 and a sync `backend.register_restored(...)` at 456-463) **between** `wait_for_livez` (which sync-blocks the worker) at line 430-432 and the `update_sandbox_status(Running)` at line 481-483. If the handler future is dropped at the `unseal` await:

- The VM is **live and serving traffic** on the reserved vm_index slot.
- `state` map insert never runs → post-wake exec/stop/delete return 404 (pre-B19 symptom resurfaces for *cancelled* wakes).
- `release_vm_index` never runs because `stop_inner` hits the `None => Ok(())` idempotent branch (`nomad_ch.rs:988-989`).
- pg row stays in `Restoring` forever (no rollback runs; the future just dropped).
- The C1 sweep can't see it (lessee_updated_at is NULL).

**This is a new wedge state** distinct from the pre-livez wedge: a live VM with no controller record AND a wedged pg row. The `persist.unseal` await is also `compio::runtime::spawn_blocking` (`persist.rs:680`) — a 1-3 ms file read + AEAD decrypt, but still an async yield point. **Fix shape unchanged from r3 #1**: `scopeguard::guard` around the post-`reserve_vm_index` body, defused at line 495.

### 2. B17 wrapper background subshell — un-reaped, verbatim from r3 & r4
`nomad-vm-wrapper.sh:388-419`. Re-verified at HEAD `3e8bfad5`: no `BG_PID=$!` at line 419, no `wait $BG_PID` in `cleanup` (lines 278-289), trap still kills only `$CH_PID`. The retry loop at `:414-418` runs unbounded by the trap. Recent commits `15b4f9a8`, `0aa93a0f`, `4fd92bef` touched Rust only — wrapper script unchanged since `dec489a1` (Bug #17 close). Under Nomad SIGTERM during the 10s ping window (line 391-398) the subshell continues against a dead socket while parent exits → orphaned PID-1 zombie + log noise on a cleanly-stopped VM.

---

## IMPORTANT

### 3. B19 `Backend::nomad_ch_handle()` borrow-lifetime trap
`backend/mod.rs:467-474` returns `Option<Arc<NomadCHBackend>>` (owned), not `&Arc<…>` as the brief implied. Audited the only caller at `lib.rs:618` (`let nomad_handle = backend.nomad_ch_handle();`) — the `Arc` is moved into `RealRestoreBackend::with_nomad_handle` and stored. **No borrow held across await; this is safe.** However the bigger smell stands: `register_restored` is a 5th `Err("backend X doesn't support…")` arm on the Backend enum (`mod.rs:499-503`), per R5-A1. Architecturally this should be a `&dyn SnapshotCapableBackend`. Concurrency-wise the wiring is sound.

### 4. A3-partial hard_link TOCTOU between `remove_file` and retried `hard_link`
`snapshot_store.rs:259-264`. After `hard_link(&from, &to)` returns `AlreadyExists`, the code does `remove_file(&to)?; hard_link(&from, &to)?;`. Window: another worker thread or process could win the race and re-create `to` between the remove and the second hard_link, causing the second call to fail `AlreadyExists` again. Real but microscopic: the only writer of `<alloc_dir>/{memory-ranges,config.json,state.json}` is this very handler holding the pg lease, and the post-A3-partial commit message confirms "same fs root in production". Per-sandbox alloc_dir means cross-sandbox collision is impossible. **Concurrency severity: low.** Recommend an `OpenOptions::new().create_new(true)` rename-into-place if this ever becomes a multi-writer concern; or wrap in a retry loop with bounded attempts so a malicious/buggy second writer can't induce a spurious 500.

### 5. `wait_for_agent_livez` is NOT a concurrency hazard — but the SYNC restore-path equivalent IS
`nomad_ch.rs:2861-2959` (the async impl used by the **create** path) is well-behaved: probes are serialized per-call but each HTTP probe runs inside `compio::runtime::spawn_blocking`, so one slow probe doesn't fence the runtime worker. 150 ms async sleep between probes. `last_fp` / `last_version_status` are stack-local. No race between cluster bootstrap and probe — `wait_for_agent_livez` is called *after* `submit_nomad_job` returns. **Async path: clean.**

**However** the **restore path** uses a different helper: `wait_for_livez_blocking` (`restore_handler.rs:1322-1341`) — pure sync `nomad_get_blocking` + `std::thread::sleep(150ms)`, NO `spawn_blocking` wrapper, called from `do_restore_inner` at line 430-432 which is itself an `async fn`. **This blocks the compio worker for up to 30s per wake**. A3 (deferred backlog) already names this; reaffirmed here. Also note: the restore variant **skips** the signed `/version` fingerprint check entirely (line 1316-1321 comment), so it has no stale-tenant defense — fine for v1 single-slot, but a footgun for v2 cluster-fallback.

### 6. `register_restored` Entry::Occupied returns Err — what unblocks a stuck slot?
`nomad_ch.rs:1664-1680`. If a wake races a duplicate wake (e.g., two `restore` handlers landing for the same sandbox_id), the second call returns Err. The caller maps to `RestoreHandlerError::Backend` and rollback fires (line 249 `teardown_restore`) — but `teardown_restore` does NOT call `state.write().remove()` (it can't; there's no entry yet for THIS call). The OTHER successful wake's entry persists. Outcome is correct but the error message ("would clobber live record; refusing") is silent about which slot won. Add the existing `agent_url` to the error string so operators can correlate.

---

## MINOR

### 7. `register_restored` default trait impl returns `Ok(())` silently
`restore_handler.rs:162-170`. Per R5-Q1 in the deferred backlog — a default `Ok(())` means a future backend (Docker, K8s, in-process tests forgetting `#[automock]`) silently re-introduces the pre-B19 wedge. Should be `Err("not implemented")` so missing impls fail loudly. Concurrency-adjacent: the silence is what made the pre-B19 leak undetectable.

---

**Summary:** 7 findings — 2 CRITICAL (C3 widened a third time via `register_restored` await pair; B17 wrapper subshell verbatim from r3/r4), 4 IMPORTANT (B19 handle wiring sound; A3-partial TOCTOU microscopic; restore-path `wait_for_livez_blocking` fences compio worker; Entry::Occupied error string unhelpful), 1 MINOR (silent default trait impl). **r4 closed: #4, #5, #6**; **r4 partially closed: #3 (B19 wired but RAII still missing)**; **r4 still open: #1 (C3, now wider), #2 (B17), #7 (sweep signal noise)**. Top two citations: **(1)** `restore_handler.rs:450-463` — B19's `unseal().await` + `register_restored()` interposed before `update_sandbox_status(Running)` extends the C3 cancel-unsafe window AND introduces a "live VM + no controller record + Restoring pg row" wedge state distinct from the pre-livez wedge; **(2)** `nomad-vm-wrapper.sh:388-419` — un-reaped background subshell unchanged since `dec489a1`, no PID capture, trap kills only `$CH_PID`, leaves zombie under Nomad SIGTERM. C3 fix needs a `scopeguard::guard` across the post-`reserve_vm_index` body; B17 fix needs `BG_PID=$!` + `kill $BG_PID; wait $BG_PID` in `cleanup`.
