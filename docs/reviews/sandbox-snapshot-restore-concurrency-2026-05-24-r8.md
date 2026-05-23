# Sandbox snapshot-restore — Concurrency Review r8 (2026-05-24)

Branch `feat/sandbox-snapshot-restore` @ `f2d89b61`. Read-only.
Prior rounds r1–r7. r8 focus: R7-P1 (3× spawn_blocking on
snapshot path) + R7-S1 (clock_resync challenge/sandbox-id bind)
audit. C3 / R4-A2 carryover.

---

## R7-P1 AUDIT — RACE-SAFE, ONE NEW EXPOSURE

Sequence in `snapshot_handler.rs:361-407`:

```
ch.pause(api_socket)             spawn_blocking → .await
ch.snapshot(api_socket, temp_dir) spawn_blocking → .await
store.put(sid, temp_dir, ch_ver)  spawn_blocking → .await
```

Each `.await` fully consumes the prior `JoinHandle` before the next
closure is constructed, so the three blocking workers never run
concurrently for the same `sandbox_id`. Strict sequencing matches
the doc-comment claim at `:351-354`.

Concurrent-snapshot defense: `update_sandbox_status(.., Snapshotting,
g0, None)` at `:269-271` CASes on `generation`; a second admin
snapshot or sweep call on the same `sandbox_id` re-reads `g1`,
gets `g0 != g0+1`, and bounces with `CasLost`. Cross-sandbox
parallelism is fine — `temp_dir` is per-sandbox
(`snap_stage_dir(.., sandbox_id)` at `admin_handlers.rs:1270`).

**Verdict: lock-safe.** See finding 1 for the new exposure window
the wrap opened up.

---

## R7-S1 AUDIT — REPLAY-SAFE, INIT ORDERED CORRECTLY

`RESYNC_CHALLENGES: OnceLock<Mutex<LruCache<String, ()>>>` at
`sandbox-agent/src/handlers.rs:70` is `std::sync::Mutex` (line 26
`use std::sync::{..., Mutex, OnceLock}`). Lock-hold scope at
`:797-809` is bounded: `contains` + conditional `put`, no await
inside the guard. The doc-comment at `:793-795` explicitly drops
the lock before the `settimeofday` syscall. Lock held across no
`.await`. Safe.

`SANDBOX_ID: OnceLock<String>` at `:59` set from `main.rs:97-102`
BEFORE `web::HttpServer::bind` (caller path runs after
`init_sandbox_id_from_env()`). First `clock_resync` request can
only land on a fully-bound listener, by which time `SANDBOX_ID`
is set. Init ordering correct. The 500 fallback at `handlers.rs:730-741`
is defense-in-depth, not the live path.

**Verdict: replay-safe, init-safe.** See finding 4 for an
LRU-capacity issue.

---

## FINDINGS

### CRITICAL

1. **C3 widened a 5th time by R7-P1 (`snapshot_handler.rs:355-407`).**
   The snapshot path now contains three `.await` points across
   pause/snapshot/put plus a 4th await on `update_snapshot_metadata`
   at `:410`. Drop after `ch.snapshot` Ok but before `store.put`
   leaves a paused VM with a fully-written staged artifact in
   `temp_dir` and a pg row stuck in `Snapshotting` — C1 sweep is
   still dead code so this wedges permanently. Rollback at `:296-321`
   only fires on the inner Err; future-drop bypasses it. R4-A2
   `LeasedVmSlot` RAII would close this; absent that, a scope-guard
   that on Drop CASes back to `Running` (or `Snapshotting_aborted`)
   is the minimum. C3 is now widened on BOTH the restore path
   (4 awaits: unseal/resync/register_restored + pg) AND the snapshot
   path (4 awaits: pause/snapshot/put + pg).

2. **R4-A2 LeasedVmSlot RAII — 4th cycle open.** With C3 now
   widened on BOTH snapshot AND restore paths, the case for a single
   `LeasedVmSlot { state_map_entry, vm_index_reservation,
   pg_transient_marker }` Drop-rolls-back guard is overwhelming.
   `backend/nomad_ch.rs:944-952,1126-1129` + `snapshot_handler.rs:269-407`
   + `restore_handler.rs:341-563`.

### MAJOR

3. **R7-P1 opened a new compio-yields-mid-pause window.** Pre-r7,
   pause+snapshot+put ran sequentially on the async caller so no
   other compio task touched this sandbox until `update_snapshot_metadata`
   committed. Post-r7 the worker yields between each spawn_blocking
   `.await`. A peer task that races `lookup_source_vm_ops(sandbox_id)`
   between pause-Ok and snapshot-Ok will observe a paused VM still
   present in the registry (`registry.lookup` reads the same map
   `stop_inner` mutates); a peer that issues `/exec` against the
   in-VM agent will hang until the snapshot completes (CH is paused
   → tap idle). Not a wedge, but it changes the observable contract
   the audit was implicitly relying on. Document it in the trait
   doc at `snapshot_handler.rs:351-354`; ideally hold a per-sandbox
   `Snapshotting` flag in the state map so peers fail fast with
   `503 sandbox_busy`.

4. **R7-S1 LRU capacity = 4 is fragile against burst restores.**
   `handlers.rs:76` `RESYNC_CHALLENGE_CAPACITY: usize = 4`. The
   doc-comment justifies it by "controller calls /_clock_resync
   once per restore cycle, restore cycles are seconds apart at
   worst". This is wrong for the multi-tenant case: a single agent
   inside a single sandbox only sees its OWN sandbox's resyncs,
   but `do_restore_inner` could legitimately retry a partial-failure
   wake (current code path is fail-fast, but a future
   `RestoreHandlerError::Internal` retry-on-transient would push
   3+ challenges in seconds). 4 is small enough that genuine
   retry-after-network-blip could evict the original challenge
   and re-accept its replay. Bump to `RESYNC_CHALLENGE_CAPACITY:
   usize = 64` — still trivial memory cost inside the snapshot
   image, far more headroom.

5. **Unbounded detached spawn — R7-C1 still open.**
   `admin_handlers.rs:1310-1324` (R6-P1) detaches teardown with
   no JoinSet / semaphore. R7 flagged; r8 confirms no change.
   N concurrent admin snapshots ⇒ N background tasks each holding
   `Arc<AppState>` + 30–150 s Nomad HTTP slot. Cap with `Semaphore`
   or shared `JoinSet`.

### MINOR

6. **R7-S1 LRU pre-allocation timing.** `resync_challenges()` at
   `:137-143` lazily allocates via `OnceLock::get_or_init`. First
   resync after agent boot pays one mutex+LRU allocation under
   the auth-gated handler. Cheap, but reorder by calling
   `resync_challenges()` once at `main.rs:103` (post init_sandbox_id)
   to hide the allocation from the hot path.

7. **R7-P1 `unwrap_or_else(|p| Err(...))` swallows panic type.**
   `snapshot_handler.rs:366,377,401-405` formats the panic payload
   with `{p:?}` which for a Box<dyn Any> renders `Any { .. }` —
   not useful for forensics. Use `crate::backend::nomad_ch::format_panic`
   if it exists, or downcast to `&str`/`String`. Minor; affects
   debuggability only.

8. **R7-S1 derives `agent_url` via trait default `127.0.0.1:0`
   on `StubRestoreBackend` paths (`restore_handler.rs:182-184`).**
   r7-S2 (open). The `persist=None` guard at `:506` skips the
   resync call so the sentinel never fires in the in-repo
   `StubRestoreBackend` tests — but a future test that supplies
   `persist=Some` to a `StubRestoreBackend` (or a mis-wired prod
   backend that forgets the override) silently calls a port-0
   URL. Change the default to `panic!("derive_agent_url must be
   implemented")` or remove the default impl.

---

## DETACHED-TASK / OUTSTANDING WORK

| ID | Status | Note |
| --- | --- | --- |
| R7-P1 | CLOSED + audited race-safe; widened C3 (finding 1) |
| R7-S1 | CLOSED + audited replay-safe; LRU-cap fragile (finding 4) |
| C3 | OPEN — 5 widenings (r2, r4, r5, r7, r8) |
| R4-A2 | OPEN — 4th cycle; subsumes C3 + R5-A2 + R6-P1 owner |
| R7-C1 | OPEN — unbounded detached teardown spawn |
| R7-S2 | OPEN — `derive_agent_url` default sentinel |

---
