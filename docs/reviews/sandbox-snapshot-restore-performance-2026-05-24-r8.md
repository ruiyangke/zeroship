# Performance review — 2026-05-24 round 8

**HEAD**: `f2d89b61` — incl. `79428d53` R7-P1 + `e95baa89` R7-S1. Static review only; no fresh cluster numbers.
**Baseline**: Appendix F c=4 — snapshot p50 50573 ms, wake p50 9235 ms.

## Summary

**8 findings** (2 CRITICAL, 4 IMPORTANT, 2 MINOR). R7-P1 spawn_blocking is correct; R7-S1 adds ~3–5 µs agent-side / ~100–200 µs controller-side per resync — negligible. The next wake-path long-pole is `submit_restore_job` + `wait_for_livez_blocking` still on the ntex worker (real sites `restore_handler.rs:1378` and `:1411`, not the brief's stale 1353/1386).

## R7-P1 audit (`79428d53`) — CORRECT

Closures at `snapshot_handler.rs:362-407` clone `Arc<dyn …>` + move owned `PathBuf`; no `&` borrows, no `MutexGuard`, no `Rc`. Panic propagates via `unwrap_or_else(|p| Err(...))`. Sequencing (`:351-354`): `pause` awaited before `snapshot` spawns. Internal `std::thread::sleep(50ms)` at `:669` now inside the spawn_blocking thread — fine.

## R7-S1 latency (`e95baa89`) — NEGLIGIBLE

Agent (handlers.rs:704-809): JSON parse 140B (~1–3 µs), UUID str-eq (~30 ns), hex iter (~150 ns), `LruCache::contains+put` (~250 ns), `Mutex::lock` (~50 ns). **~3–5 µs.** Controller (restore_handler.rs:1494-1527): 2× /dev/urandom (~10 µs), 48× `format!("{:02x}")` (~80–120 µs), `serde_json::to_string` (~5 µs). **~100–200 µs.** Both trivial vs 100–200 ms resync wall-clock.

## A3 remaining — slice 5 sketch

Brief's named lines are partially stale:

- `restore_handler.rs:1378` — `wait_for_alloc_running_blocking` 250 ms poll, called inline from `:1039` → `:461` **without spawn_blocking**. ~3–5 s on ntex worker.
- `restore_handler.rs:1411` — `wait_for_livez_blocking` 150 ms poll, `:1052` → `:466` **without spawn_blocking**. ~2–3 s.
- `snapshot_handler.rs:669` — already inside R7-P1 spawn_blocking. No change.
- `backend/nomad_ch.rs:3966 + :4279` — **inside `#[cfg(test)]`** (mod tests begins `:3210`); test mock. **Deferred-file note mis-targets prod.**

**Slice-5 shape (CRITICAL)**: wrap `backend.submit_restore_job(…)` + `backend.wait_for_livez(…)` at `restore_handler.rs:460-467` in `compio::runtime::spawn_blocking`. Needs `Arc<dyn RestoreBackend: Send+Sync>` or method-to-free-fn flip. Pattern: R5-P1b at `cdd2e677`. **Estimated reduction: 5–8 s c=1, 7–10 s c=4 → wake p50 9235 → ~2500–4000 ms.**

## CRITICAL

1. **`restore_handler.rs:460-467` — sync `submit_restore_job` + `wait_for_livez` on ntex worker.** Both use `nomad_get_blocking` + `std::thread::sleep` (`:1378`, `:1411`). At c=4 pegs all four ntex workers ~5–8 s. Fix: slice 5 above.

2. **`snapshot_handler.rs:92-105` — `ChRemoteClient` trait lacks `: Send + Sync`.** R7-P1's `Arc::clone` + move into `spawn_blocking` requires it; today's sole impl satisfies incidentally. A future `Rc<_>`-bearing impl fails at the call site, not at impl declaration. Same applies to `RestoreBackend` post-slice-5.

## IMPORTANT

3. **`restore_handler.rs:1584-1591` — `clock_resync_random_hex` uses 48× `format!("{:02x}")`.** ~80–120 µs avoidable per resync. Use a 16-byte lookup table.

4. **`restore_handler.rs:1322-1416` — pollers open fresh TCP per iteration.** No persistent `ureq::Agent`; at 150 ms cadence over ~3 s that's ~20 TCP handshakes per wake. Persistent agent saves ~100–300 ms per wake. Stacks on slice-5.

5. **`handlers.rs:76` — `RESYNC_CHALLENGE_CAPACITY=4` is small.** Today the controller never retries after 200 OK so eviction can't race; defensive bump to 16 closes a latent failure mode. RAM cost ~1 KB.

6. **`handlers.rs:728-742` — `boot_sandbox_id() == None` is a 500.** Correctly fail-LOUD, but no production wiring exports `SANDBOX_AGENT_SANDBOX_ID` yet — grep across `crates/sandbox/scripts/*` finds zero hits. Until v18+v5 + wrapper export ship, every wake 500s. Blocks c=4 re-measurement.

## MINOR

7. **`restore_handler.rs:1499-1564` — `clock_resync` spawn_blocking holds 10 s ureq timeout.** Worst-case parks one blocking thread 10 s on agent unreachability. Tighten to ~2 s.

8. **`snapshot_store.rs:175 :176` — 64 KiB scratch buffer allocated per `compute_artifact_sha256` call.** Hoist to stack array `[0u8; 64*1024]` or reuse across the 3-file loop. Negligible but trivial.

## A2b status — not on wake path

`grep '\.verify('` shows zero production callers in `restore_handler.rs` or `snapshot_handler.rs`. A2b's 1 GB re-stream cost remains latent (test-only + future sweep). Confirmed not on wake path.

## Post-A3-full next bottleneck

With slice-5 landed, wake p50 breakdown (estimated):
1. **`store.get` 1 GB SHA + AEAD decrypt** (in spawn_blocking since `cdd2e677`) — **~1.5–2 s; new dominant cost**.
2. Nomad `submit` RTT — ~500–800 ms.
3. First `/livez` 200 (VM resume bound) — ~500–1500 ms.
4. `clock_resync` RTT — ~100–200 ms.
5. pg awaits + state-map — ~50–100 ms.

**Next slice (post-A3)**: parallel SHA-of-3-files inside `compute_artifact_sha256`. Today sequential over config.json + memory-ranges + state.json. With `rayon::join` (or manual 3-thread fan-out inside spawn_blocking), the 1 GB memory-ranges parallelises onto n2-standard-4's 4 vCPUs; SHA wall ~1.5 s → ~0.6 s. **~1 s wake p50 saving.**

## Two most critical citations

- `restore_handler.rs:460-467` — sync `submit_restore_job` + `wait_for_livez` not wrapped in spawn_blocking; internal `std::thread::sleep` at `:1378` + `:1411` parks ntex worker 5–8 s per wake. **Largest remaining wake-path target.**
- `snapshot_handler.rs:92-105` — `ChRemoteClient` trait missing `: Send + Sync` bound; R7-P1 correctness rests on incidental impl auto-derivation.
