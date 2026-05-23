# Sandbox snapshot-restore — Concurrency Review r7 (2026-05-24)

Branch `feat/sandbox-snapshot-restore` @ `6f5d41b8`. Read-only.
Prior rounds r1–r6. r6 hypothesised #22 = guest wall-clock skew
(now confirmed + closed). r7 focus: R6-P1 detached teardown +
B22 clock_resync + post-B22 cancel-window width.

---

## R6-P1 RACE-SAFETY VERDICT — LOCK-SAFE, NEW UX RACE

`admin_handlers.rs:1310-1324` detaches teardown via
`compio::runtime::spawn(...).detach()`. Trace against a wake of
the same `sandbox_id`:

- `stop_inner` (`backend/nomad_ch.rs:982-990`) drops the state-map
  entry FIRST under `state.write()`.
- 30 s `wait_for_job_gone` + ≤120 s host_fence BEFORE
  `vm_index_allocator.lock().release(N)` (`:1126-1129`).
- Source slot `N` was `alloc()`-ed → `N < self.next` ∧
  `!freed.contains(&N)` (`:339`) → wake's `reserve_vm_index(N)`
  (`restore_handler.rs:354`) returns `Err("vm_index N already
  reserved")` → 503 `VmIndexUnavailable`.

`Arc<Mutex<VmIndexAllocator>>` is the serialisation point. No
window exists where `reserve(N)` Ok and `release(N)` un-fired —
the in-flight predicate at `nomad_ch.rs:338-341` covers it.
**Lock-safety claim holds.**

---

## FINDINGS

### CRITICAL

1. **R4-A2 RAII (5th round).** B22 added a 4th await between
   `unseal` and `register_restored`. The case for `LeasedVmSlot
   { state_map_entry, vm_index_reservation }` with Drop = rollback
   rose again. Would close C3 + R4-A2 + give R6-P1's detached
   task a clean owner. `backend/nomad_ch.rs:944-952,1126-1129` +
   new `:1650-1681` `register_restored`.

2. **C3 widened (4th time).** `restore_handler.rs:485-506` —
   cancel-unsafe window now spans `unseal().await` +
   `clock_resync_post_restore().await` (≤10 s ureq) +
   `register_restored()` + 2 pg awaits. Drop after `clock_resync`
   returns Ok but before `register_restored` leaks a live VM with
   synced clock that the state-map doesn't know; row wedges in
   `Restoring` permanently (C1 sweep dead).

### MAJOR

3. **Unbounded detached spawn.** `admin_handlers.rs:1311` —
   no semaphore. N concurrent admin snapshots stack N background
   tasks each holding `Arc<AppState>` + a Nomad HTTP slot for
   30–150 s. Cap via `Semaphore` or shared `JoinSet`.

4. **UX race shifted, not removed.** Pre-R6-P1 the handler held
   the request open 50 s; client saw 200 only when slot was free.
   Post-R6-P1 client sees 200 immediately, then a wake within
   ~120 s gets 503 `vm_index_unavailable`. Surface is technically
   correct; document or stamp `Retry-After`. `admin_handlers.rs:1325`.

5. **Two-lock straddle persists.** State-map removal
   (`stop_inner:982-990`) and `vm_index_allocator.release`
   (`:1126-1129`) are still separated by 60–120 s of fence work.
   Subsumed by finding 1 (RAII).

### MINOR

6. **Controller-NTP dependency.** `clock_resync_post_restore`
   (`restore_handler.rs:1439`) signs `SystemTime::now()` as the
   target ts. A skewed controller writes a wrong `CLOCK_REALTIME`;
   the agent's strict 5 s gate then validates against the bad
   time on both sides — silent failure mode. Document the
   controller-NTP requirement.

7. **B22 replay defense — VERIFIED.** `sig.rs:441-446` records
   the nonce in the LRU on the bypass path. Test at
   `sandbox-agent/src/sig.rs:1468-1503` pins it. An in-VM
   attacker cannot forge a new resync (needs controller private
   key); replay rejected up to `NONCE_TTL_S`. No additional
   single-in-flight enforcement needed — sig+nonce-LRU suffice.

8. **R6-C1 wrapper subshell — CLOSED + VERIFIED.**
   `scripts/nomad-vm-wrapper.sh:284-287` cleanup trap calls
   `kill -TERM "$RESUME_PID"` + `wait`; `RESUME_PID=$!` captured
   at `:428`; trap registered EXIT/INT/TERM at `:298`. Reaped
   under Nomad SIGTERM mid-poll. Closes 5-round B17 carry.

---

## B22 CONCURRENCY AUDIT — CORRECT WITH ONE WIDENING SIDE-EFFECT

Sequence at `restore_handler.rs:485-506`:

```
wait_for_livez Ok
  → p.unseal(sandbox_id).await          ← AWAIT
  → clock_resync_post_restore(...).await  ← AWAIT (≤10 s ureq)
  → register_restored(...)              ← sync
```

Serialisation with `register_restored` — fine. The pg row is at
`Restoring` (CAS-fenced via `g0→g1` at `:243`); backend state-map
entry still ABSENT (B19 inserts only at `register_restored`); any
racing admin op (stop / snapshot / exec) hits "sandbox not in
state map" 500 or `CasLost`. There is no window where the state
map says "ready" but the clock is broken — the comment at
`:477-484` is accurate.

`verify_kind_skew_bypass` (`sandbox-agent/src/sig.rs:356-369`)
still runs signature + body-hash + nonce-LRU; only `skip_skew_check`
flips (`:380-389`). Replay defended.

**Verdict:** B22 is concurrency-sound. Only side-effect is +10 s
on the C3 cancel-unsafe window (finding 2).

---

## SUMMARY

8 findings: 0 NEW critical from R6-P1/B22 themselves; 2 carried
critical (R4-A2 RAII still open, C3 widened 4th time); 3 major
(unbounded spawn, UX race shift, two-lock straddle); 3 minor
(controller-NTP, replay verified, R6-C1 verified). Wake-path
floor ~78/100 — #22 closed end-to-end (7/7 post-wake exec) but
cancel-unsafe `do_restore_inner` + dead lease-takeover sweep
still compound.

Most critical citations:

- `crates/sandbox/src/admin_handlers.rs:1310-1324` —
  `compio::runtime::spawn(...).detach()` unbounded; N admin
  snapshots stack N×(30 s Nomad + 120 s fence) background tasks.
- `crates/sandbox/src/restore_handler.rs:485-506` — C3
  cancel-unsafe window now spans unseal + clock_resync (≤10 s
  ureq) + register_restored + 2 pg awaits with no scope-guard.
