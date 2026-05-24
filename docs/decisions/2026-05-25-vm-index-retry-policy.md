# ADR — VmIndexRetryPolicy::from_host_fence_timeout — Empirical Teardown Model

- **Date:** 2026-05-25
- **Status:** Accepted
- **References:** `crates/sandbox/src/restore_handler.rs::VmIndexRetryPolicy::from_host_fence_timeout`;
  smoke reviews T-8b-smoke-r9, r10, r12, r13;
  architecture reviews r14, r15, r17, r19, r20.

## Context

The vm-index wake-retry budget must envelope the source VM's full teardown wall-time.
The budget is derived from `cfg.host_fence_timeout_secs` to track any future fence-config
bump automatically (R14-A6 recommendation; previously a second hard-coded constant).

Over 11 review cycles (C-4 through C-8c, C-7-LT-1), the formula evolved as empirical
measurements revealed that the teardown wall-time was not what early assumptions implied.
The history below documents each inflection point.

### C-7 (T-8b-smoke-r8)

The original v1 default was 60 × 2 s = 118 s wall-time, sized to envelope the worst
observed teardown (host_fence ~60 s + Nomad purge ~30 s ≈ 90 s). That budget exceeded the
60 s ntex client deadline. When the client disconnected, ntex dropped the wake handler
future mid-`compio::time::sleep.await`, leaving no success/exhausted log — a silent
failure. Fixed by capping at 25 × 2 s = 48 s (≥10 s headroom under the deadline).

### R14-A6 (architecture-r14)

Production backends should derive the policy from `cfg.host_fence_timeout_secs` via
`from_host_fence_timeout` rather than rely on the hard-coded default. The `Default` impl
remains at the C-7 constants (25 × 2 s = 48 s) as the test-contract anchor and the
unit-test fallback for backends without a `cfg` handle.

Formula at this point: `max_attempts = (host_fence_timeout_secs − 10) / 2 + 1`.

### C-8a (T-8b-smoke-r9)

The prior derivation took only the fence into account. At `host_fence_timeout_secs = 120`,
the formula produced a 110 s budget — 50 s past the 60 s ntex deadline, re-introducing the
C-7 silent-cancel failure. Fix: compute TWO ceilings and take the MIN:

- **fence-derived** (`teardown_estimate − HEADROOM`): the IDEAL ceiling — envelopes the
  observed source-teardown so a wake racing a fence-clear has a non-trivial chance of
  catching the release.
- **deadline-derived** (`CLIENT_DEADLINE − HEADROOM = 50 s`): the HARD ceiling — anything
  past this is silently dropped when the ntex client disconnects.

The hard ceiling wins when the operator runs a conservative fence; the ideal ceiling wins
for tight per-cluster overrides (e.g. cluster-smoke at 30 s).

### C-8b (T-8b-smoke-r10)

The fence-derived ceiling originally used `host_fence_timeout_secs` directly, implicitly
assuming `teardown_wall_time ≈ fence_timeout`. Smoke-r10 empirically measured the full
`stop()` pipeline at **60.164 s for `host_fence = 30 s`** — apparently 2× the fence.

Fix: `teardown_estimate = 2 × host_fence_timeout_secs` before subtracting headroom.

### Smoke-r13 retrospective (NON-NORMATIVE — empirical ground truth)

The 2× ratio turned out to be a **numeric coincidence**, not a compositional model.
The actual teardown semantics have two distinct paths:

- **Agent-dies path**: agent socket closes → `wait_for_agent_silent` fires on 2 consecutive
  connect-misses (fast) → `fence_passed=true` → slot released promptly.
- **Agent-hangs path**: `wait_for_agent_silent` probes time out for the full
  `host_fence_timeout` (hard-coded 30 s at `nomad_ch.rs`) → `fence_passed=true` → slot
  released; any residual hang past that leaks the slot (tracked via
  `sandbox_vm_index_leaks_total`).

The Nomad purge tail is NOT a significant second contributor at production fence values.
The root cause of the "60 s teardown at fence=30 s" signal was that `wait_for_agent_silent`
fired only once per 30 s budget due to the ureq+spawn_blocking probe wedge (fixed by
C-7-LT-2-PR1 at `40811d8b`). The 2× factor was an artifact of that upstream wedge.

We retain `teardown_estimate = 2 × host_fence_timeout_secs` as a **conservative safety
margin**, not as a model. The MIN-of-two design from C-8a still structurally prevents the
C-7 silent cancellation regardless of how this estimate is tuned.

### C-7-LT-1 (T-8b-smoke-r12)

In `WakeResponseMode::Sync` the budget is bound by both the fence-derived ceiling AND the
ntex client deadline — the tighter wins. In `WakeResponseMode::Async` the wake state
machine runs on a `detach_isolated` thread with no client-side cancellation, so the
deadline ceiling no longer applies. Budget becomes `2 × host_fence + HEADROOM` — a safety
margin past the empirical 2× source-teardown wall-time.

The `wake_mode` parameter was threaded into `from_host_fence_timeout` at this point.
`RealRestoreBackend` carries the mode; `AppState::from_config` threads
`WakeResponseMode::from_env()` through via the `with_wake_response_mode` builder.

## Decision

`teardown_estimate = 2 × host_fence_timeout_secs` is retained as a conservative safety
margin. The formula branches on `WakeResponseMode`:

- **Sync**: `effective_budget = MIN(teardown_estimate − HEADROOM, CLIENT_DEADLINE − HEADROOM)`
- **Async**: `effective_budget = teardown_estimate + HEADROOM`

Constants: `HEADROOM_SECS = 10`, `CLIENT_DEADLINE_SECS = 60`, `INTERVAL_SECS = 2`,
`MIN_ATTEMPTS = 1`.

## Consequences

- The MIN-of-two design (C-8a) prevents the C-7 silent-cancellation regression in Sync mode
  regardless of how conservative the fence is configured.
- Async mode (`detach_isolated`, no ntex client deadline) uses `2 × fence + 10` — envelopes
  the empirical 60.166 s teardown at fence=30 s with ~10 s slack (smoke-r12).
- The 2× factor is a safety margin; future tightening is safe if the probe-wedge root cause
  (now fixed by C-7-LT-2) is confirmed absent at next re-smoke.
- Sync mode is preserved for migration; async mode is the long-term target (C-7-LT R15-A1).
- Implementation: `crates/sandbox/src/restore_handler.rs::VmIndexRetryPolicy::from_host_fence_timeout`.
