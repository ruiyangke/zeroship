# Concurrency Review (r3) — sandbox snapshot/restore

**Branch:** `feat/sandbox-snapshot-restore` (HEAD `03d15012`)
**Scope:** `crates/sandbox/**`
**Date:** 2026-05-24 UTC · **Reviewer:** code-critic (Opus)
**Prior rounds:** r1 (11 findings) · r2 (8 findings)

---

## Prior-round closure status

- **r2 #1 (`stop_preserving_state` deletes sealed record despite "preserving state")** — **CLOSED** by `78320b56`. `backend/nomad_ch.rs:1192-1208` now gates `persist.delete` on the same `remove_host_dir` bool that gates `host_dir` rm; new test `stop_preserving_state_does_not_delete_sealed_record` (lines 4340-4429) pins the contract. Verified by reading the diff. The C2 fix did NOT introduce a snapshot-vs-snapshot race: `Persistence::{seal, delete}` are keyed per-sandbox-id file (`persist.rs:638-666`), so a concurrent `write_or_seal` on sandbox A's record cannot collide with a teardown deleting sandbox B's record; same-sandbox concurrent seal+delete is intentional last-write-wins per the "best-effort, idempotent" contract.
- **r2 #2 (non-Send future on `IdleSnapshotter`)** — **still open**. `sweep.rs:253-257` unchanged.
- **r2 #3 (C3: cancel-unsafe `do_restore_inner`)** — **still open**. See finding 1.
- **r2 #4 (unbounded detached L2 uploads)** — **still open**. `snapshot_store_gcs.rs:832-859` unchanged.
- **r2 #5 (token-mutex contention 3×)** — **still open**.
- **r2 #6 (sweep `join_all` shape blocked by trait)** — **partially addressed**. `sweep.rs:469-506` now uses `futures::future::join_all` over chunks; the trait still returns non-`Send` futures, but compio is single-threaded so this works. See finding 4.
- **r2 #7 (re-resolve source VM ops sync-after-async)** — **still open**.
- **r2 #8 (`attempted = rows.clone()` regardless of shutdown)** — **still open** (also tracked as R3-Q1 / r3-quality-r3).
- **r1 #1 (C1: lease-takeover sweep dead code)** — **still open**. `db.rs:2305` (`update_lessee`) confirmed still has zero non-test callers; nothing in `78320b56` / `2380605e` / `dec489a1` exercises it. See finding 5.

---

## CRITICAL

### 1. C3 confirmed: `do_restore_inner` cancel-window is 90s wide
`restore_handler.rs:286-412`. The cancel-window analysis from r2 still holds verbatim: between `backend.submit_restore_job` (line 383) and the final `db.update_sandbox_status(...Running, expected_generation, None).await` (line 397), there are TWO blocking ureq+sleep loops (`wait_for_alloc_running_blocking` + `wait_for_livez_blocking`, up to 60s+30s) AND a `std::fs` write in `rewrite_config_json` (line 379) — none in `spawn_blocking`. The `.await` boundary where compio can observe cancellation is the final `update_sandbox_status` at line 397, AFTER all the blocking work succeeded. If the handler future is dropped (client disconnect, shutdown) during the blocking section, no rollback fires: vm_index stays reserved (`RealRestoreBackend.reservations` Mutex at line 713), Nomad alloc is running, pg row stuck in `Restoring`. With C1 sweep dead, recovery is manual. **Smallest fix**: introduce a `scopeguard::guard` (or hand-rolled `Drop`) around the inner body that, on early drop, spawns a detached compio task to (a) `release_vm_index(snap.vm_index)`, (b) `backend.teardown_restore`, (c) CAS pg `Restoring → Snapshotted` via `g1`. The guard is `defuse()`'d on `Ok` return at line 411. This avoids changing the trait shape and adds ~30 LOC at the inner-fn boundary.

### 2. B17 wrapper concurrency: `ch-remote` background subshell vs. `wait $CH_PID`
`scripts/nomad-vm-wrapper.sh:388-419`. The B17 fix spawns a background subshell (`( ... ) &` at line 388-419) that polls `ch-remote --api-socket "$API_SOCK" ping` and then issues `resume`. Meanwhile the parent script proceeds to `wait "$CH_PID"` at line 449. Two concurrency hazards: **(a)** the subshell talks to the same Unix-domain `API_SOCK` that CH owns — CH serializes API requests, but Nomad's `SIGTERM` arriving during the ~10s polling window will run `cleanup` (line 278-289) which `kill -TERM`s `$CH_PID`. The subshell continues polling a dead socket for up to 50×0.2s = 10s, holding the trap from completing. Nomad's kill grace is 5s by default → SIGKILL of the parent wrapper, orphaning the subshell as a zombie owned by PID 1; the cleanup `wait $CH_PID` never blocks on the subshell so its exit is unobserved. **(b)** the trailing `for delay in 0.3 1 3` (lines 414-418) does `sleep 4.3s` total of `ip link set $TAP up` calls running CONCURRENT with CH's vCPU execution; if CH is in the middle of a virtio-net negotiation, an admin-down-then-up on the tap could in theory perturb LOWER_UP detection. The wrapper has no `wait` on the subshell PID, so a slow resume that takes longer than the controller's `wait_for_livez_blocking` budget would surface as a phantom 503 with no log correlation. **Fix shape**: capture `BG_PID=$!`, add a `kill $BG_PID 2>/dev/null` to the `cleanup` trap, and `wait $BG_PID` after `wait $CH_PID` returns.

---

## IMPORTANT

### 3. B18 root cause hypothesis: per-sandbox keypair is fresh, but the rootfs-template `/keys` may be stamped
`backend/nomad_ch.rs:588-590` mints a fresh Ed25519 keypair PER `create_sandbox` call — confirmed. `pubkey_hex` (line 599) is injected via `zsbx_pubkey=<hex>` on the cmdline (wrapper line 431). `scripts/init.sh:56-95` decodes the cmdline arg and writes to `/run/keys/controller-pubkey` (tmpfs `/run` — RAM-backed, NOT persisted across boot). So per-sandbox the keypair IS fresh and the in-VM target path IS volatile. **The race is NOT key generation.** Likely root causes for the slot-reuse 401:
  1. The agent process (`sandbox-agent`) caches the pubkey at startup (`controller-pubkey` read once); a slot reused after teardown skips `mkfs.ext4` on `workspace.img` (see `nomad_ch.rs:660-664` "idempotent: skip if exists" — but `workspace.img` is per-sandbox-uuid so this shouldn't fire) — but if the SAME `host_dir` path is reused for a new sandbox UUID (it isn't — see line 658, host_dir is per-UUID), this would matter. Confirmed not the cause.
  2. **Likely**: `wait_for_livez_with_fingerprint` (line 977 region) reads the `expected_fp` from somewhere stale. Worth checking whether `state.read()` returns a row from a PRIOR sandbox at the same vm_index that wasn't fully evicted from `backend.state` between teardown and create. `vm_index_allocator` (line 612) is independent of `state` HashMap — a brief race window between `state.remove(&old_id)` (in `stop_inner`) and `state.insert(&new_id)` (in `do_create_inner`) where the allocator hands out the freed index to the new create before the old state entry is removed would let `wait_for_livez_with_fingerprint` find the OLD entry's fingerprint. Audit `state` lock ordering at lines 1373-1395 vs the stop path's state removal.

### 4. T7 `join_all` chunk cap=4 vs pg pool=16 — NOT pool-starved, but unbounded across sweeps
`sweep.rs:493-496` + `db.rs:362,376`. The pg pool default is `SANDBOX_PG_POOL_MAX=16` (D-17), and `snapshot_handler::snapshot_sandbox` plus the post-snapshot `teardown_source_for_snapshot` together hold roughly 2-3 pg connections per row (CAS + clear_snapshot_metadata + idle-update). At chunk cap=4, peak in-flight = 4 rows × ~3 conns = 12 — within budget. **HOWEVER**: the sweep loop itself doesn't hold pg conns across chunks, but the `ControllerIdleSnapshotter::snapshot_one` body at `sweep.rs:340-369` overlaps `snapshot_sandbox` (which does several `pool.get()` round-trips internally) with the per-row pg work of the OTHER 3 in-flight rows. If a single GCS L2 upload (within `snapshot_store_gcs::put`) blocks for 60s on the network, all 4 in-flight rows are holding their pg conns for that entire window. Combined with concurrent admin handler traffic (which also draws from the same pool), 4 stuck snapshot rows + 4-12 concurrent admin requests = pool exhaustion → admin handler 500s. Sweep doesn't currently meter per-handler pg-conn watermark.

### 5. C1 still dead, NOT exercised accidentally by any commit since r2
Audited `78320b56`, `2380605e`, `dec489a1`. None touch `db.rs:2305` (`update_lessee`), `db.rs:1712-1724` (`update_sandbox_status`), or `db.rs:2361` (the sweep query). `update_lessee` still has zero non-test callers (verified by grep against `crates/sandbox/src/`). Lease-takeover remains a permanent wedge under any controller crash mid-Snapshotting/Restoring.

---

## MINOR

### 6. `RealRestoreBackend::reservations: Arc<Mutex<...>>` released by best-effort teardown only
`restore_handler.rs:713`. The `reservations` set is mutated under lock at `reserve_vm_index` (line 298 caller) and `release_vm_index` (caller from `teardown_restore` only, line 832). Per r1 #3 (still open), success path doesn't release — every successful wake leaks one slot in `reservations`. After a process lifetime of N wakes, `reserve_vm_index` returns `VmIndexUnavailable` for any source slot already used once. Tracked but worth noting that the C2 fix didn't fix it either.

### 7. `snapshot_rows_chunked` `shutdown()` checked once per chunk, not per row
`sweep.rs:469-472`. With cap=4, a chunk's 4 in-flight `snapshot_one` futures may each take ~30s under load; the `if shutdown() { break; }` at line 470 fires only at chunk boundaries → shutdown latency = up to one chunk's wall time. For cap=4 × 30s = 120s drain delay before the sweep loop yields. Add a `shutdown()` check inside the per-row future, or `select!` the join_all against a shutdown signal.

---

**Summary:** 7 findings — 2 CRITICAL (C3 confirmed, B17 wrapper subshell), 3 IMPORTANT (B18 root cause narrowed, pg pool sufficient but rebound risk, C1 still dead), 2 MINOR. **r2 #1 (C2) CLOSED**; **r2 #6 partially addressed** (sweep now uses join_all but trait Send-bound unchanged). All other r1/r2 findings remain open. Top two: **(1)** `restore_handler.rs:286-412` cancel-unsafe across 90s of blocking ureq + std::fs + sleep — needs scope-guard at line 411 boundary; **(2)** `scripts/nomad-vm-wrapper.sh:388-419` background subshell has no PID capture, no `wait`, and the cleanup trap doesn't reap it — orphan-zombie risk under Nomad SIGTERM during the 10s ch-remote-ping window.
