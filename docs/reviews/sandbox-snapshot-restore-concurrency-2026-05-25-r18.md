# Sandbox/snapshot-restore — concurrency r18 review

Date: 2026-05-25 (UTC).
HEAD at audit: `ee57e23f` (branch `feat/sandbox-snapshot-restore`).
Round 18 of N. READ-ONLY.
Scope: PR2-FOLLOWUP commits `b2965097`, `96678eaa`, `fa4fe63c`,
`b2b6c3c9`, `4ab58eac`, `c3038389` against r17 carry-forward.

Prior round: `sandbox-snapshot-restore-concurrency-2026-05-25-r17.md`.

## Summary

- **8 findings** (1 CRITICAL carry, 3 IMPORTANT, 4 MINOR). 3 of r17's
  CRITICAL/IMPORTANT closed at PR2-FOLLOWUP.
- **R17-C1 CLOSED** at `96678eaa`: `update_wake_job_state` writes
  `lessee_updated_at = now()` unconditionally (db.rs:3056). Only path
  mutating wake_jobs state; every transition + terminal goes through
  it (wake_machine.rs:130, 163, 244, 273, 365, 388, 420, 438, 487-493).
  No bypass.
- **R17-C2 OPEN** (GATE-C2 queued for r20). Migration 0010 adds
  `wake_jobs_lessee_idx` partial index but NO UNIQUE INDEX on
  `(sandbox_id) WHERE state NOT IN ('ok','failed')`. `insert_wake_job`
  still bare INSERT (db.rs:2964-2987). TOCTOU window unchanged.
- **R17-C3 OPEN**, **R17-I1 OPEN** (GATE-I1), **R17-I2 OPEN** (GATE-I2).
- **Sanitizer LANDED** at wake_machine.rs:717-735; sync inside `drive()`,
  no race surface. Coverage gaps documented as R18-M1/M4.
- **WakeResponseMode fail-CLOSED**: `from_env` Result; `AppState::from_config`
  propagates via `?` at lib.rs:816-822.
- **R17-A5 CLOSED**: CreateGuard::drop on `detach_isolated`. Grep shows
  no remaining `compio::runtime::spawn(...).detach()` in production
  paths outside main.rs:115 (ntex preview-ws server) and lib.rs:2318
  (test fixture).

## Per-prompt-question audit

### Q1 — `lessee_updated_at` bump coverage

SQL (db.rs:3050-3058) sets `lessee_updated_at = now()` unconditional,
plus symmetric COALESCE on `error_code`/`error_message`/`agent_url`.

`UPDATE sandbox.wake_jobs` appears once in src (db.rs:3050). Test
fixtures (sandbox_pg_e2e.rs:4196-4219) mutate `agent_url` only for
CHECK-constraint pinning — not relevant to lessee. Terminal-state
writes (wake_machine.rs:130, 163) also route through
`update_wake_job_state`. **No bypass path. R17-C1 CLOSED.**

### Q2 — Sanitizer coverage

`sanitize_error_message` (wake_machine.rs:717-735) — 3-pass byte scan
+ 256-byte char-boundary truncate.

| Surface | Stripped? |
|---|---|
| RFC1918 IPv4 (10/8, 172.16/12, 192.168/16) + `:port` | YES |
| `http(s)://<rfc1918>:port/path` URLs (whole-unit redact) | YES |
| IPv6 link-local `fe80::*` + `%<zone>` + `]:port` | YES (case-insensitive) |
| 256-byte truncation, char-boundary safe | YES (post-redaction) |
| Loopback 127.0.0.0/8 | NO (documented — triage) |
| 169.254/16 (AWS IMDS) and 100.64/10 (CGN) | NO — gap, R18-M1 |

**Race**: sync inside `drive()` on the detach_isolated thread; single
writer per wake_id. Sanitizer cannot race with a concurrent write.
Unredacted message still in `tracing::warn!` at wake_machine.rs:154-160;
durable column gets sanitized form only. **No race surface.**

### Q3 — WakeResponseMode fail-CLOSED

`config.rs:893-902`: unrecognised env value → `Err`. lib.rs:816-817:

```rust
let wake_response_mode = crate::config::WakeResponseMode::from_env()
    .map_err(|e| format!("WakeResponseMode::from_env: {e}"))?;
```

`?` propagates to outer `Result<Self, String>`. Boot aborts on `=ASYNC`,
`=true`, `=on`, `=1`. `WakeLifecycleConfig::from_env` identical pattern
(lib.rs:821-822). **Both fail-CLOSED. Confirmed.**

### Q4 — R17-C2 TOCTOU re-verification

`admin_handlers.rs:1556-1577` → `find_pending_wake_for_sandbox` → fall
through to `insert_wake_job` at line 1642. No transaction; no UNIQUE
constraint.

**At c=1**: negligible probability — single client, single thread.

**At c=20 stress**: two POSTs in same ~ms window both see `None`, both
insert, both spawn WakeMachine. CAS race at wake_machine.rs:247:
CAS-loser short-circuits at line 254 (`Phase::Failed/Internal`) without
calling rollback — R17-I1 race shape doesn't fire on the CAS-loser
path. But `reserve_vm_index_with_retry` (line 262) is a SECOND race:
on the CAS-winning machine's `vm_index` slot, an unrelated reserve
contention (or the second wake's CAS-winner attempting the same
vm_index after the first machine releases via teardown) can hit
`rollback_and_classify` → `teardown_restore(vm_index)`, releasing the
winner's slot. **R17-I1 race lives at the reserve-loser path under
the TOCTOU. Priority confirmed: GATE-C2 first (one UNIQUE INDEX
defangs both R17-C2 and R17-I1's reachable shape).**

### Q5 — GC retention min vs polling interval

`config.rs:941`: `MIN_GC_RETENTION_SECS: u64 = 1`. Doc at lines 927-935
claims "minimum exceeds the client's max polling interval". 1 s floor
does NOT exceed a 1-5s polling interval; documented invariant is not
enforced by code. See **R18-I1**.

### Q6 — Detach helper expansion + new spawn sites

10 production `detach_isolated` sites: snap-health, snap-heartbeat,
snap-takeover, snap-transient, **wake-gc** (sweep.rs:336 — NEW in PR2
9f006c87), snap-idle-evict, snap-idle-gc, gcs Tiered::put L2,
teardown_source_for_snapshot, **create-rollbk** (nomad_ch.rs:2008 —
NEW b2965097), **wake-machine** (admin_handlers.rs:1675 — NEW
98032273).

Remaining `compio::runtime::spawn(...).detach()`: main.rs:115
(preview-ws server, intentional — runs on main runtime), lib.rs:2318
(test fixture). `compio::runtime::spawn_blocking` in wake_machine.rs
(lines 312, 351, 371, 520, 561) all execute INSIDE detach_isolated's
private runtime — correct hop. `std::thread::spawn` in nomad_ch.rs
(4634/4891/5113) all `#[cfg(test)]`. **No new spawn site missed.**

### Q7 — WakeMachine thread footprint at c=20

One OS thread + private compio runtime per WakeMachine; each runtime
spawns its own blocking pool. At c=20: ~20 dedicated wake threads +
4-8 blocking-pool threads per runtime → 80-180 OS threads steady,
~160-360 MB stack overhead during a wake storm. Acceptable — wake
concurrency bounded by `reserve_vm_index` slot availability and
per-host restore caps well below c=20.

**At c=200** (fleet wake-storm horizon): pool of pre-warmed runtimes
on an MPMC channel beats spawn-per-wake on RSS and on cold-start
latency. **Out of scope for PR2 ship. R18-M2.**

### Q8 — Carry-forward verification

| Tag | Status | Evidence |
|---|---|---|
| R17-C1 | CLOSED | db.rs:3056 `lessee_updated_at = now()` |
| R17-C2 | OPEN | migration 0010 partial index only, not unique |
| R17-C3 | OPEN | takeover sweep deferred (design §5) |
| R17-I1 | OPEN | rollback helpers still unconditional teardown |
| R17-I2 | OPEN | no `fail_register` token in tree; all 6 e2e tests use `persist: None` |
| R17-A5 | CLOSED | nomad_ch.rs:2008 detach_isolated |
| R16-M1 | OPEN | wake_machine.rs:410 borrow held across `clock_resync.await` |

## Findings

### [R18-C1] R17-C2 idempotency TOCTOU — still open (CRITICAL carry, GATE-C2)

- **File**: `admin_handlers.rs:1556-1642`,
  `migrations/0010_wake_jobs_hardening.sql`, `db.rs:2964-2987`.
- **Shape**: unchanged from r17 — `find_pending_wake_for_sandbox` then
  `insert_wake_job` with no transaction and no UNIQUE INDEX. Migration
  0010 explicitly does NOT add the partial UNIQUE INDEX on
  `(sandbox_id) WHERE state NOT IN ('ok','failed')`.
- **Why still matters**: c=1 smoke (current QA) won't observe; c=20
  stress will. r20 is the scheduled fixer. If stress runs before r20,
  manifests as paired wake_jobs rows + vm_index thrash hard to trace.
- **Action**: r20 fixer per existing plan. No new ask.

### [R18-I1] GC retention floor doesn't enforce its documented invariant (IMPORTANT)

- **File**: `config.rs:927-935` (doc) vs `:941` (`MIN_GC_RETENTION_SECS: u64 = 1`).
- **Shape**: doc says "minimum exceeds the client's max polling
  interval"; constant is 1 s. Client polling at 1-5 s gets zero
  protection from the floor. Default 300 s is fine; risk is operator
  setting env var for dev/test and promoting the config.
- **Action**: either raise floor to ≥ max_polling_interval + safety
  margin (suggest 10 s) OR rewrite the doc to say "MIN rejects
  sub-second values that race the controller's own
  terminal-write-then-poll; operators must set retention >
  client's max poll interval; no compile-time guard". Doc-fix
  preferred — code change may break dev workflows wanting fast cycles.

### [R18-I2] R17-I1 rollback pre/post-reserve still unconditional (IMPORTANT carry, GATE-I1)

- **File**: `wake_machine.rs:506-542` (`rollback_and_classify`) +
  `:544-579` (`rollback_with`).
- **Shape**: both helpers unconditionally call
  `teardown_restore(vm_index)`. Under R18-C1 at c=20, reserve-loser
  releases the reserve-winner's slot.
- **Action**: split into `rollback_pre_reserve` (no teardown) vs
  `rollback_post_reserve` (full teardown). R4-A2 LeasedVmSlot RAII
  dissolves both long-term. Queued r20.

### [R18-I3] R17-I2 register_restored failure-path untested (IMPORTANT carry, GATE-I2)

- **File**: `tests/sandbox_pg_e2e.rs::wake_machine_e2e` (6 tests).
- **Shape**: grep confirms zero hits for `fail_register` in the tree.
  All 6 tests use `persist: None`, which short-circuits at
  wake_machine.rs:432 (skips unseal + clock_resync + register triad).
  `rollback_with(RegisterFailed)` at line 428 has no e2e coverage.
- **Action**: r20 — `fail_register: bool` on `StubRestoreBackend` +
  test with a seeder-fixture `Persistence` so the triad runs.

### [R18-M1] Sanitizer omits 169.254/16 IPv4 link-local + 100.64/10 CGN (MINOR)

- **File**: `wake_machine.rs:748-809` (`match_rfc1918_at`).
- **Shape**: only RFC1918 (10/8, 172.16/12, 192.168/16). AWS IMDS at
  `169.254.169.254` and CGN (100.64/10) are cluster-topology too.
- **Severity MINOR** — unlikely to appear in wake-path error bodies
  today; comment lines 700-716 explicitly TODOs expansion during PR2
  smoke.
- **Action**: add the two ranges to `match_rfc1918_at` OR rename to
  `match_private_ipv4_at`.

### [R18-M2] WakeMachine thread footprint not pool-budgeted at c=200 (MINOR — horizon)

- **File**: `admin_handlers.rs:1675`.
- **Shape**: one OS thread + private runtime per wake. Linear RSS
  scaling. At c=200 → ~400-1600 threads + ~2 GB stacks during a
  fleet wake-storm.
- **Severity MINOR** at PR2 — actual c bounded by vm_index allocator
  + per-host restore caps well below c=200.
- **Action**: PR3+ horizon — worker pool fed by an MPMC channel. Pool
  size `min(num_cpus * 4, max_in_flight_wakes)`. Document the current
  limit on the proposal in the meantime.

### [R18-M3] R16-M1 SealedAuth Zeroize cross-await still present (MINOR carry)

- **File**: `wake_machine.rs:410` — `&sealed.signing_key_bytes`
  borrow across `clock_resync_post_restore.await` at 407-412.
- **Severity MINOR** because `SealedAuth` does not yet impl Zeroize
  (R16-M1 is the open work for that).
- **Action**: when R16-M1's `impl ZeroizeOnDrop` lands, this site
  needs a borrow-scope tightening (copy key bytes before the await
  OR drop the `sealed` binding pre-await).

### [R18-M4] Sanitizer constructs output via `as char` casts (MINOR — cosmetic)

- **File**: `wake_machine.rs:828, 834, 878, 935` (`out.push(bytes[i] as char)`).
- **Shape**: `bytes[i] as char` for a u8 produces a Latin-1 codepoint,
  not a UTF-8 byte. For ASCII input safe; for multi-byte UTF-8 input
  each byte becomes a separate codepoint, re-encoding the string into
  a different UTF-8 form. Current wake errors are ASCII (Rust Display
  impls, ureq error bodies), so no observable misbehavior.
- **Action**: use `push_str` on segments between redaction matches,
  OR `debug_assert!(msg.is_ascii())` at the head.

## Cross-lens consensus

- **R18-C1** remains the highest-priority concurrency gate. UNIQUE
  INDEX + ON CONFLICT closes the TOCTOU AND defangs R18-I2 — the
  reserve-loser race in R18-I2 is reachable only through R18-C1's
  TOCTOU at c=20.
- **R18-I1** is config-hardening — test-coverage lens may want a
  dev-workflow repro (retention=1, observe race, fail).
- **R18-I3** is shared with the test-coverage lens — backlog item for r19.
- **R18-M2** is perf-lens horizon, not blocking PR2 ship.

## Lens hand-off

- **Architecture r18**: confirm migration 0010's `wake_jobs_lessee_idx`
  partial index is read by the (still-unwritten) takeover sweep's
  query plan once it ships.
- **Test-coverage r18**: (1) `fail_register` flag + test using a seeder
  Persistence (R18-I3 / GATE-I2); (2) retention=1 doc-vs-code mismatch
  repro test (R18-I1).
- **Security r18**: R18-M1 sanitizer gaps + R18-M4 Latin-1 char cast —
  both in security lens scope.
- **API-surface r18**: poll endpoint wire shape unchanged across
  PR2-FOLLOWUP — no new surface to pin.

## Status block

```
Round 18 (PR2-FOLLOWUP complete):
  CLOSED at PR2-FOLLOWUP:
    R17-C1 (lessee_updated_at on every transition — 96678eaa),
    R17-A5 (CreateGuard::drop migrated — b2965097),
    R17-I1 thread-name fit (snap-health 15-byte pin — c3038389),
    + R16-S1/S2/S3/S4/S5 (security follow-ups).
  NEW: R18-C1 (R17-C2 TOCTOU carry — r20 GATE-C2),
       R18-I1 (GC retention MIN=1 doesn't enforce documented invariant),
       R18-I2 (R17-I1 rollback split — carry, GATE-I1),
       R18-I3 (R17-I2 fail_register coverage — carry, GATE-I2),
       R18-M1 (sanitizer omits 169.254/16 + 100.64/10),
       R18-M2 (thread footprint at c=200 — horizon),
       R18-M3 (R16-M1 Zeroize cross-await carry),
       R18-M4 (sanitizer Latin-1 char cast — cosmetic).

  CARRY: R4-A2 (15 cycles), R14-V1, R15-D1, R16-I2, R16-M1, R16-M2,
       R11-C1/C2 OPEN.

  ASK: (1) r20 GATE-C2 UNIQUE INDEX + ON CONFLICT (R18-C1).
       (2) r20 GATE-C3 takeover sweep (R17-C3 carry).
       (3) r20 GATE-I1 rollback split (R18-I2).
       (4) r20 GATE-I2 fail_register test (R18-I3).
       (5) Decide R18-I1 floor-vs-doc.
       (6) Prioritize R4-A2 (15 cycles).
```
