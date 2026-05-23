# Concurrency Review (r2) — sandbox snapshot/restore

**Branch:** `feat/sandbox-snapshot-restore` (HEAD `3b888a2a`)
**Scope:** `crates/sandbox/**`
**Date:** 2026-05-24 UTC · **Reviewer:** code-critic (Opus)
**Prior round:** `sandbox-snapshot-restore-concurrency-2026-05-24-r1.md`

---

## r1 closure status

- **r1 #1 (lease-takeover sweep dead code)** — **still open** (tracked as deferred [C1]).
- **r1 #2 (90s blocking ureq/Command on ntex worker)** — **still open** (tracked as [A3]); no `spawn_blocking` wrap in `restore_handler.rs:298-411,789-816,818-833` nor `snapshot_handler.rs:335-338`.
- **r1 #3 (success branch leaks `vm_index` reservation)** — **still open**. `restore_sandbox` `Ok` arm at `restore_handler.rs:195-196` never calls `release_vm_index`; only the rollback `teardown_restore` path does (line 832). Re-confirmed.
- **r1 #4 (cached_token Mutex held across blocking HTTP)** — **still open**. `snapshot_store_gcs.rs:151-199` still holds the guard across `ureq::get(METADATA_TOKEN_URL).call()`. New `verify` path calls `access_token()` once per artifact file (line 677), tripling lock-contention windows under load.
- **r1 #5 (PoisonError policy split)** — **still open**. `registry.rs` lines 196, 205, 239, 252, 298, 299, 308, 319, 331, 347, 349, 350, 363, 365, 385, 390, 428, 430, 464, 474, 476, 483, 487, 496, 499, 511, 513, 527, 534, 540 all `.unwrap()`; sweep/restore/gcs use `unwrap_or_else(|p| p.into_inner())`.
- **r1 #6 (stale `g1` on restore rollback)** — **still open** (`restore_handler.rs:209-211`).
- **r1 #7 (CH-pause-then-GCS-fail orphan)** — **still open** (`snapshot_handler.rs:286-313`).
- **r1 #8 (`std::fs` on async path)** — **still open** (`restore_handler.rs:303, 309, 335, 379, 443, 474`).
- **r1 #9-11** — still open as listed.

The 11 r1 findings are unchanged by the three commits (`f32507ce`, `2e0d17f7`, `4a7e8e03`). The A2 fix to `GcsSnapshotStore::verify` is functionally correct and does not introduce new races.

---

## CRITICAL (new in r2)

### 1. `stop_preserving_state` deletes the sealed record despite "preserving state"
`backend/nomad_ch.rs:1160-1168` runs `persist.delete(sandbox_id)` unconditionally in `stop_inner` — the `remove_host_dir` bool gates ONLY `host_dir` removal at line 1119-1153. `teardown_source_for_snapshot` calls `stop_preserving_state` → `stop_inner(.., false)` → still wipes `<persist_dir>/sealed-records/<sbx>.bin`. Today wake doesn't consume the sealed record (the wake path in `admin_handlers.rs:1238-1290` never touches `state.persist`), so this leak is latent — but the next deploy that wires sealed-record-based key-recovery into wake (deferred [T5]) will hit a "sealed file gone" error every wake-after-snapshot. The "preserving state" name is a lie.

### 2. `IdleSnapshotter::snapshot_one` returns non-`Send` future
`sweep.rs:250-253` returns `Pin<Box<dyn Future<Output = ...> + 'a>>` — no `+ Send` bound. The trait requires `Send + Sync` on the impl type but lets the future itself be `!Send`. Today compio is single-threaded per worker, so the future never crosses thread boundaries; if compio ever moves to a multi-threaded executor (work-stealing) the bound omission silently breaks compilation only where a join_all over multiple snapshotters lands. Mirrors the documented T7 (per-iteration concurrency is sequential) — sequential is forced *because* the future isn't Send.

---

## IMPORTANT

### 3. Future cancellation during `do_restore_inner` leaves wedged row
`restore_handler.rs:286-412`. If the surrounding handler future is dropped (client disconnect, ntex worker shutdown, ctrl-c) **after** `submit_restore_job` succeeded but **before** the final `db.update_sandbox_status(...Running, g1, None).await` at line 397, no rollback fires: vm_index stays reserved (`reservations: Arc<Mutex<VmIndexReservations>>` at line 713), Nomad alloc keeps running, pg row stays `Restoring`. Recovery requires the transient-takeover sweep — but that sweep is dead code per r1 #1. The blocking calls inside this async fn (ureq, Command, `std::thread::sleep`) are NOT in `spawn_blocking`, so the worker is stuck inside them — cancellation can only land at the pg `.await`, which is exactly where the rollback CAS lives. Cancel-safety: zero.

### 4. `TieredSnapshotStore::put` detaches unbounded L2-upload tasks
`snapshot_store_gcs.rs:832-859`. Each tier `put` spawns a fire-and-forget `compio::runtime::spawn(async move { l2.put(...) })`. With the new T6 idle-eviction sweep auto-spawning (`lib.rs:530-535`), N concurrent idle-snapshots → N detached L2 uploads, each holding the GCS bucket connection + a full `Arc<L2>` clone. No bound, no metric, no back-pressure. Comment at line 816-820 acknowledges retry/metric is a follow-up; no upper bound is enforced today. Under GCS partial outage the detached tasks fan out without limit.

### 5. `GcsSnapshotStore::verify` triples token-mutex contention
A2 fix at `f32507ce` is correct (canonical hash recomputed end-to-end). New cost: `verify_canonical_sha256_from_streams` calls `open_object_stream` per artifact (3 files), each calling `access_token()` which holds `cached_token: Mutex` across `ureq::get(METADATA_TOKEN_URL).call()` (`snapshot_store_gcs.rs:151-199`). Per-verify lock-acquire fanout went 1× → 3×. Under N concurrent verifies (multi-sandbox L2 audit), serialization is 3N×. r1 #4 IMPORTANT was already flagged; r2 makes it worse.

### 6. Inner sweep loop holds non-`Send` borrow → blocks `join_all` rewrite (deferred [T7])
`sweep.rs:443-471`. `for r in chunk { snapshotter.snapshot_one(sid).await }` — the inner loop is serial. Per [T7] the fix is `join_all`, but `snapshot_one` returns `Pin<Box<dyn Future + 'a>>` borrowing `&'a self` (line 250-253). To `join_all` you need owned futures or `Send`-bounded futures; the trait can't deliver that with the current shape. Fixing T7 requires changing the trait — not just the loop. The deferred entry under-states the work.

---

## MINOR

### 7. `ControllerIdleSnapshotter` re-resolves source VM ops sync-after-async (best-effort race)
`sweep.rs:325-333` calls `state.backend.lookup_source_vm_ops(sandbox_id).await` BEFORE `snapshot_handler::snapshot_sandbox` does the pg row read + CAS. An admin-driven `snapshot_sandbox` can run in parallel, win the CAS, complete the snapshot, and tear down the source VM — all before the sweep's `snapshot_one` re-enters `snapshot_handler`. The sweep then sees `StateMismatch` (handled at sweep.rs:368-378) but has already paid a Nomad HTTP round-trip. Not unsafe, just wasted work.

### 8. Sweep's `attempted: Vec<SandboxRow> = rows.clone()` is computed regardless of shutdown
`sweep.rs:444`. The clone happens even if `state.shutdown_requested()` will fire on the next-statement-down check (line 446). For 100-row batches this is harmless; for a future increase to IDLE_BATCH_LIMIT it's pointless work during drain. Trivial fix: invert the order.

---

**Summary:** 8 findings — 2 CRITICAL (new), 4 IMPORTANT (new in r2 / refined r1), 2 MINOR (new). Zero r1 findings closed by the three commits under review; A2 (security) and A5 (api-surface) closures are real but live in other reviewer axes. Top two: **(1)** `stop_preserving_state` deletes the sealed record despite "preserving state" — bug #15 fix gated only the `host_dir` rm, not `persist.delete` (`backend/nomad_ch.rs:1160-1168` + `:1119` bool gate); **(2)** restore-future cancellation between Nomad-submit and the success CAS leaves the row wedged in `Restoring` with vm_index leaked and alloc still running, and the only recovery (transient-takeover sweep) is dead code per r1 #1 (`restore_handler.rs:286-412` + r1 #1).
