# sandbox-snapshot-restore — Test-Coverage Review (Round 8)

- **Date**: 2026-05-24
- **Branch**: `feat/sandbox-snapshot-restore` @ `c07cbb62`
- **Worktree**: `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`
- **Counts at HEAD**: sandbox lib 283, sandbox-agent lib 214, combined 497; pg suite separate.
- **r7 delta**: B22 = +7 sandbox-agent + +3 sandbox; R5-P1b = 0 (refactor).
- **Read-only**: source/proposal/deferred untouched.

Findings ordered by severity, file:line cited, then R7-S1 + R7-P1 regression sketches.

---

### [CRITICAL] r7-S1 replay-DoS testability — sketch only; zero coverage today
- **Files**: `crates/sandbox-agent/src/sig.rs:1467-1502` (replay test asserts SAME-process nonce LRU); `crates/sandbox-agent/src/handlers.rs:571-577` (`ClockResyncBody` has `ts` only — no `sandbox_id`, no challenge); `crates/sandbox/src/restore_handler.rs:1466-1473` (controller body is `{"ts": …}` only).
- **Why it matters**: the canonical body's only binding is `ts`. The agent's nonce LRU lives in guest RAM and is captured by the snapshot, but a freshly-restored guest's LRU has zero resync-nonces because no resync has ever run inside that snapshot's lifetime. A captured cycle-N resync POST therefore replays cleanly into cycle-N+1, pinning `CLOCK_REALTIME` to stale `T_old` and 401-DoS'ing every subsequent strict-skew RPC.
- **Today's test surface for this**: NONE. The B22 replay test (`sig.rs:1468`) only covers same-process LRU — exactly the case that doesn't model post-restore reality.

**Regression test sketch (lands with R7-S1 fix):**
```rust
// crates/sandbox-agent/src/handlers.rs tests
#[ntex::test]
async fn clock_resync_rejects_replay_across_restore_boundary() {
    // 1. Build state A with sandbox_id = sid_A; sign a resync bound to sid_A
    //    + per-restore challenge C_A. Submit → 200.
    // 2. Construct state B with sandbox_id = sid_B (fresh LRU — models a
    //    different sandbox post-restore). Submit the EXACT cycle-1 bytes
    //    (captured wire) → MUST be 401 because the canonical body now
    //    includes sid + challenge.
    // 3. Sister case: same sid_A, fresh state (models same-sandbox second
    //    restore) → challenge mismatch → 401.
}
#[test]
fn verify_kind_skew_bypass_canonical_binds_sandbox_id_and_challenge() {
    // Canonical-string assertion: changing sid OR challenge MUST change
    // the hash. Pin the canonical shape so a future refactor can't drop
    // a field silently.
}
```
Pair with a controller-side test in `restore_handler.rs` that asserts
`clock_resync_post_restore` includes both fields and a new challenge per
invocation (no controller-side state-reuse).

---

### [CRITICAL] r7-P1 wake/snapshot-latency canary — feasible NOW, still unwritten (4th round)
- **Files**: `crates/sandbox/src/snapshot_handler.rs:335-338` (`ch.pause` + `ch.snapshot` synchronous, no `spawn_blocking`); `:316-373` (`do_snapshot_inner` overall); `crates/sandbox/src/snapshot_store.rs:95` ("wrap in spawn_blocking" — trait doc lies about callers).
- **Why no regression test exists**: r4-T2 / r5-P1b both proposed the canary; nothing has landed. The trait shape (`&dyn ChRemoteClient`, `&dyn SnapshotStore`) is already injectable — `MockChRemote { pause_delay: Duration, snap_delay: Duration }` + `MockSlowStore { put_delay }` would slot directly into `do_snapshot_inner`.

**Regression test sketch (lands with R7-P1 fix):**
```rust
// crates/sandbox/src/snapshot_handler.rs tests
#[ntex::test]
async fn snapshot_does_not_block_compio_worker_during_ch_pause() {
    // MockChRemote sleeps 500ms in pause(); MockSlowStore sleeps 500ms in
    // put(). Spawn a concurrent compio task that pings a 1ms ticker N
    // times during the snapshot. Assert: tick-count ≥ 0.8 * expected
    // (i.e. compio worker stayed responsive). Without spawn_blocking the
    // ticker stalls for the full pause+put window → tick-count = 0.
}
#[ntex::test]
async fn wake_latency_canary_concurrent_restores() {
    // 4 concurrent do_restore_inner calls + Arc<MockSlowStore> returning
    // 1 GiB bytes in 100ms simulated. Assert all 4 complete within
    // 4 * 100ms + slack (would FAIL today if store.get runs on the
    // compio worker).
}
```
Both are non-pg and ~60 LOC; the lack of one is why r6 + r7's perf
fixes ship without a guard.

---

### [HIGH] B22 `/_clock_resync` handler test depth — three gaps the cluster will hit before CI does
- **File**: `crates/sandbox-agent/src/handlers.rs:1305-1471`
- **Covered**: unauthenticated 401, far-future ts accepted, far-past ts accepted, tampered body 401, wrong-key 401, malformed JSON 400, same-process replay 401.
- **MISSING (edge cases the cluster will surface)**:
  1. **Oversized body** (DoS): the route accepts whatever ntex's default body cap allows; no test asserts a 1 MiB body containing `{"ts":..., "junk":"AAAA…"}` is either accepted-with-hash-mismatch-401 or capped. Compare with `proxy.rs:62` `DEFAULT_MAX_BODY_BYTES = 100 MiB`. A megabyte-of-junk body forces SHA-256 + signature verify per request — cheap DoS vector since `/_clock_resync` is reachable pre-clock-set.
  2. **`ts = 0` / `ts = u64::MAX`** (settimeofday clamping): `handlers.rs:600-602` clamps to `libc::time_t::try_from(ts)`. The "out of range" path (`Err → 400`) has no test; an attacker who has the controller key could otherwise DoS via `ts = u64::MAX` → `time_t` overflow → 400 spam.
  3. **Missing nonce / missing timestamp / missing signature headers**: `handlers.rs:228-260` `verify_signed_skew_bypass` reads three headers but the test set only exercises "all three present" or "none present". The three "exactly one missing" combinations are uncovered.
- **Impact**: each is a 5-line ntex test; absence costs a cluster cycle ($30) per regression.

---

### [HIGH] `derive_agent_url` Default impl returns `http://127.0.0.1:0` — uncovered (R7-S2)
- **Files**: `crates/sandbox/src/restore_handler.rs:182-184` (default impl), `:512` (call site uses returned value verbatim).
- **Symptom**: any `RestoreBackend` impl that forgets `derive_agent_url` gets a silent fail-OPEN — `127.0.0.1:0` connects to no listener, surfaces as transport error, but the diagnostic path muddles "agent down" vs "trait not implemented". No test pins the default's shape. A compile-time `panic!` default OR a unit test asserting the default returns a "REGRESSION" sentinel would catch this — same anti-pattern as R5-Q1's silent `Ok(())` default.

---

### [HIGH] Wrapper still zero behavioral coverage (R3-T3 / R5-T2, 4+ rounds open)
- **File**: `crates/sandbox/scripts/nomad-vm-wrapper.sh` (465 LOC; B17 fix at 380-419, B14b retry at 416-424).
- **State**: `crates/sandbox/tests/scripts_lint.rs:39-76` adds `shellcheck --severity=error` syntax-gate (R4-T1). Behavioural — start-branch vs restore-branch divergence, `sed -i` path-rewrite idempotency (line 367), `ch-remote ping` 50-attempt loop (line 399-406), `ch-remote resume` (line 409), tap re-up loop (line 422-424) — has ZERO in-repo test.
- **Estimated value**: each of B12/B13/B17 cost a cluster cycle (~$30) to surface. A `bats` smoke that mocks `cloud-hypervisor`, `ch-remote`, and `ip` (PATH override) and asserts the restore branch issues exactly one `ch-remote resume` after `ping` returns is ~40 LOC.

---

### [HIGH] `ControllerIdleSnapshotter` non-pg coverage still ZERO (T8, 3+ rounds open)
- **File**: `crates/sandbox/src/sweep.rs:296-407` (96 LOC of bridge code: lookup_source_vm_ops, snap_stage_dir, snapshot_handler::snapshot_sandbox dispatch, best-effort teardown).
- **Today**: `crates/sandbox/tests/sandbox_pg_e2e.rs:2599-2749` exercises only `RecordingIdleSnapshotter` (a Mutex<Vec<Uuid>> recorder). The real `ControllerIdleSnapshotter::snapshot_one` body is byte-coverage-zero outside cluster smoke. `sweep.rs` has exactly one `#[test]` for the entire 844-LOC file (line 824), unrelated to this bridge.
- **Action shape**: synthesize a fake `AppState` with a stub `Backend` + `MockSnapshotStore` + `MockChRemote`; invoke `snapshot_one(sid)`; assert (a) `lookup_source_vm_ops` was called once, (b) on `Ok(_)` the post-success best-effort teardown is invoked, (c) on `Err(StateMismatch)` the row is left untouched. Non-pg, ~60 LOC.

---

### [MEDIUM] R6-proposed script-validation tests STILL unwritten (cycle 2)
- **Proposed**: `crates/sandbox/tests/systemd_unit_consistency.rs` (~40 LOC) — assert `SANDBOX_PERSIST_AUTH=1` + `SANDBOX_AEAD_KEY_PATH` + `SANDBOX_PERSIST_DIR` triplet appears in `gcp-worker-startup.sh` so B21 can never regress silently. `crates/sandbox/tests/script_artifact_consistency.rs` (~30 LOC) — assert `gcp-worker-startup.sh:~143` references the **same** rootfs version string as `bake-rootfs.sh` (closes B20's "stash never landed" failure mode).
- **Estimated value if landed**: B20 + B21 + the next-bake regression each cost a c=4 cluster cycle. The two tests together (~70 LOC, both `grep` + `assert`) would prevent at least one $30 cycle per quarter at current churn. CHEAP. Score lift on the script surface ≈ 15 points.

---

### [MEDIUM] `clock_resync_post_restore` controller-side error coverage thin
- **File**: `crates/sandbox/src/restore_handler.rs:1811-1861`
- **Covered**: 200 happy, agent 401, transport closed.
- **MISSING**: (a) **agent timeout** — the `ureq` call has `timeout(10s)` (line 1475); no test simulates a slow agent (sleeping fake-agent). Important because this 10s budget is on the critical wake path. (b) **500 from agent** (`settimeofday` EPERM in a non-PID-1 cluster smoke) — no `5xx` test. (c) **agent returns 200 with malformed body** — the controller currently ignores body content but a future check would silently regress.

---

### [LOW] `verify_kind_skew_bypass_still_records_nonce_for_replay_defense` (sig.rs:1468) couples replay semantics to a 1-day-future ts
- **File**: `crates/sandbox-agent/src/sig.rs:1475-1501`
- **Symptom**: the test uses `ts_now() + 86_400` for the nonce-replay assertion. If a future refactor accidentally re-enables the skew check on the bypass path, the test would fail with `SkewTooLarge` rather than `ReplayedNonce` — same outcome (test fails), but the failure message would mislead the bisect. A second variant with `ts = ts_now()` (in-window) would pin the test against false-negatives.

---

## Summary

8 findings (2 CRITICAL, 4 HIGH, 2 MEDIUM, 1 LOW). The two most critical: (1) **R7-S1 replay-DoS has no regression test sketched yet** — `clock_resync_replay_returns_401` at `crates/sandbox-agent/src/handlers.rs:1456` covers same-process LRU only, missing the cross-restore replay vector entirely; (2) **R7-P1 wake/snapshot latency canary remains unwritten across 4 rounds** despite `do_snapshot_inner` at `crates/sandbox/src/snapshot_handler.rs:316-373` already taking `&dyn` traits ready for `MockSlowStore` + `MockChRemote` injection. R7-S1 sketch (above): three-test triad asserting cross-restore replay 401, canonical-shape binding of `sandbox_id` + per-restore challenge, and controller-side challenge-freshness. R7-P1 sketch (above): two compio-tick canaries — one measures worker responsiveness during `ch.pause`/`store.put`, one measures wake-latency under 4-concurrent restore against `Arc<MockSlowStore>`. Combined ~120 LOC; closes 4-round regression-test debt and unblocks SLO claims.
