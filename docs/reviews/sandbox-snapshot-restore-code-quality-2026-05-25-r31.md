# Sandbox snapshot-restore code-quality review — 2026-05-25 r31

**Reviewer**: code-quality r31 (cron-pilot)
**HEAD**: `ba1df3f0`. **Prior**: r30 (`0ee106d2`).
**Scope since r30**: 5 in-crate commits — `ade8fb46` (r30-A1 NomadStopPermits), `cdcd670d` (T-7+T-8 raw_exec deletion), plus 3 cluster/pilot housekeeping commits (no in-crate source). Plus `c70a1d88` (docs close r30-A1 note — docs only).

## Summary

- **4 findings**: 0 critical, 0 important, 4 minor. **T-7+T-8 cutover and r30-A1 NomadStopPermits LAND clean.**
- **T-7+T-8 cutover CLOSED at `cdcd670d`** — `TaskDriverMode` enum, `task_driver_mode_from_env()`, `wrapper_path` field, and `test_env_lock` module all deleted. Zero dead-code orphans; no function that was only called from the deleted branch survives. The replacement tests (`nomad_job_spec_always_uses_ch_driver`, `nomad_restore_job_spec_always_uses_ch_driver`) are well-shaped: each asserts `Driver=="ch"` and `Config.command.is_null()`. No new bare `unwrap`/`expect` in production paths.
- **r30-A1 CLOSED at `ade8fb46`** — `NomadStopPermits` is idiomatic flume-bounded-channel semaphore. Drop semantics are correct; the structural invariant (`tokens_in_channel + guards_held == capacity`) is maintained by construction. The `expect` in `acquire()` documents the disconnect-is-impossible invariant and is acceptable discipline. The `let _ = try_send(())` in `NomadStopPermitGuard::Drop` is the deliberate "lose-permit-rather-than-panic" tradeoff — well-justified and documented.
- **Stale "wrapper" rustdoc cluster** — 30+ comment-line references to the deleted `nomad-vm-wrapper.sh` survive the cutover commit. Most are historical-narrative (the comments explain prior design and migration rationale) but a focused subset is actively misleading to a new contributor: they attribute responsibilities to a bash wrapper that no longer exists. See R31-M1.
- **No new bare `unwrap()`/`expect()` in production paths.** The `expect` calls added by `ade8fb46` in `NomadStopPermits::new` and `NomadStopPermits::acquire` are both structural-invariant assertions with explicit rustdoc explaining why the panic is unreachable. The `backend.state.write().unwrap()` in `stop_inner_acquires_permit_when_installed` is test-only.
- **R30-M1 (gc_stopper catch_unwind asymmetry)** — open, unchanged. No touch this cycle.

## Carry table

| Finding | r30 state | r31 state |
| --- | --- | --- |
| **R29-M1** `ClockResyncOutcome` string-match | OPEN | OPEN, unchanged |
| **R29-M2** dead `Ok` arm in rollback path | OPEN | OPEN, unchanged |
| **R29-M3** `Any { .. }` panic-payload sites | OPEN, 14 sites | OPEN, count unchanged (no new sites this cycle) |
| **R28-M2 / R29-M5** silent `unwrap_or(0)` on `SystemTime::now()` | OPEN, 2 sites | OPEN, unchanged |
| **R29-M4** test `format!` block-argument shape | OPEN | OPEN, unchanged |
| **R29-M6** heredoc-comment foot-gun shape | OPEN | OPEN, unchanged |
| **R30-M1** gc_stopper `catch_unwind` asymmetry | OPEN | OPEN, unchanged |
| **R30-M2** R29-C1 regression test scope | OPEN | OPEN, unchanged |
| **R30-M3** `#[allow(dead_code)]` on `pub fn` | OPEN | OPEN, unchanged |
| **R30-M4** `GcStopper` Send+Sync asymmetry | OPEN | OPEN, unchanged |
| **R30-M5** `gc_stop_chunked` discards `Vec<Result>` | OPEN | OPEN, unchanged |
| **R30-M6** `GC_STOP_CONCURRENCY` not env-overridable | OPEN | OPEN, unchanged |
| **R27-M1/M3/M4/M5** cosmetic carries | OPEN | OPEN, unchanged |

---

## MINOR (new this round)

### [R31-M1] Stale "wrapper" rustdoc cluster — 9 sites are actively misleading after the T-8 cutover

**Scope**: `cdcd670d` deleted ~400 LOC of `raw_exec` / `nomad-vm-wrapper.sh` infrastructure but left 30+ comment lines that reference "the wrapper" throughout `nomad_ch.rs`, `restore_handler.rs`, `config.rs`, and `backend/mod.rs`. Most are historical narrative that a new reader can decode from context, but the following 9 are actively misleading — they attribute active responsibilities to a bash script that no longer ships:

**1. `nomad_ch.rs` module-doc, line 69:**
```rust
//! The controller computes only the **VM IP** (to reach the agent at
//! `http://10.99.<100+idx>.2:7777`); the wrapper script computes
//! everything else from `ZSBX_VM_INDEX`.
```
Post-cutover, the ch driver computes tap/MAC/IP from `vm_index` in the TaskConfig. The "wrapper script computes everything else" claim is false.

**2. `nomad_ch.rs`, line 103 (module-doc cleanup contract):**
```rust
//! and racing the still-alive prior wrapper for `tap=zsbx-nm-<idx>`.
```
The "prior wrapper" no longer exists. The prior ch driver process is the correct referent.

**3. `nomad_ch.rs`, line 158–161 (`NOMAD_ALLOC_ROOT` const rustdoc):**
```rust
/// here as a const because (a) it's already implicit in the wrapper's
/// `ZSBX_RUNTIME=${NOMAD_TASK_DIR}` expansion that the controller
/// reads back, ...
```
Post-cutover, the const's rationale should reference the driver's alloc-dir layout, not the wrapper's env-var expansion.

**4. `nomad_ch.rs`, line 224 (`SourceVmOpsHandle` rustdoc):**
```rust
///   - `api_socket` — path to cloud-hypervisor's HTTP API UDS,
///     `<alloc_dir>/ch/local/ch.sock` (the wrapper's `${ZSBX_RUNTIME}/ch.sock`).
```
The ch driver places the socket at this path; the wrapper is gone. A reader debugging socket connectivity would search for wrapper code that doesn't exist.

**5. `nomad_ch.rs`, line 2410 (`CreateGuard` struct rustdoc):**
```rust
/// and racing the still-alive prior wrapper for `tap=zsbx-nm-<idx>`.
```
Same as item 3 — referent is now the prior ch driver process.

**6. `config.rs`, line 313–314 (`runtime_dir` field rustdoc):**
```rust
/// Equivalent to the `ZSBX_HERE` env var in the demo wrapper. The wrapper `cd`s to
/// this dir at startup.
```
`ZSBX_HERE` was a wrapper env var; the field still exists but its relationship is now to the ch driver's `kernel` + `artifact_dir` config fields. A new operator reading this would look for `ZSBX_HERE` in driver code that doesn't exist.

**7. `config.rs`, lines 335 and 346 (`vm_index_floor`/`vm_index_ceil` rustdoc):**
```rust
/// gets a unique index; the wrapper computes `tap=zsbx-nm-<idx>`,
...
/// `ceil`. Must be ≤ 155 — the IP arithmetic in the wrapper +
```
Tap computation is now the driver's responsibility, not the wrapper's.

**8. `restore_handler.rs`, lines 415–416 (`submit_restore_job` trait rustdoc):**
```rust
/// `ZSBX_RESTORE_FROM=<alloc_dir>` must be set in the spawned
/// task's env so the wrapper's restore branch fires (PR 3f).
```
Post-cutover, `ZSBX_RESTORE_FROM` in `Env` is a debugging artefact. The actual restore is triggered by `Config.restore_from` in the typed TaskConfig. A contributor implementing a new backend that reads this doc would build the wrong contract.

**9. `restore_handler.rs`, lines 979–982 (inline comment in restore orchestrator):**
```rust
//    The path-bearing fields (disks[].path, fs[].socket,
//    serial.file) are NOT touched here; the wrapper rewrites
//    them at exec time because only the wrapper knows the
//    actual NOMAD_TASK_DIR (Nomad assigns the alloc UUID after
```
The wrapper no longer exists. Post-cutover, the ch driver is what "knows the actual NOMAD_TASK_DIR" and rewrites these fields internally. The comment is now a false attribution.

Additionally, `restore_handler.rs:1227` and `:1235` (`derive_mac`, `derive_tap` rustdoc) still cite "the wrapper script" as source-of-truth:
```rust
/// MAC derivation rule from `crates/sandbox/scripts/nomad-vm-wrapper.sh`:
/// `printf '12:34:56:78:9b:%02x' "$VM_INDEX"`.
...
/// Tap derivation rule: `zsbx-nm-<vm_index>`. Source-of-truth is
/// the wrapper script; this Rust copy must match.
```
`nomad-vm-wrapper.sh` no longer exists (deleted at `cdcd670d`). The source-of-truth is now the ch driver's `task_config.go::DeriveMAC` / tap-naming logic. The Rust copy must match the driver, not a deleted script.

**Why this matters beyond cosmetics**: items 8, 9, and the `derive_tap`/`derive_mac` rustdoc are on the restore path's public trait surface and production orchestrator logic. A future contributor implementing a new restore backend reading these docs would (a) believe `ZSBX_RESTORE_FROM` env is the activate switch, (b) believe the wrapper rewrites `config.json` path fields at exec time, and (c) look for the wrapper script to verify the MAC/tap formula — all three are false post-T-8. The error surface is limited to contributors working the restore path, but the cost of the confusion is high relative to the fix (rewording five comments).

**Fix shape**: replace "wrapper" with "ch driver" at the 9 sites above. The `NOMAD_ALLOC_ROOT` const's `(b)` clause should read "the ch driver's alloc-local socket path (`<alloc_dir>/ch/local/ch.sock`) is embedded in this layout". `derive_mac` and `derive_tap` rustdoc should name `nomad-driver-ch/ch/task_config.go` as the source-of-truth.

**Severity**: MINOR. The deleted `TaskDriverMode` code is fully gone; the stale comments do not represent dead code. The smell is the misleading attribution of active responsibilities to a deleted script, concentrated in the restore path's public API surface.

---

### [R31-M2] `nomad_job_spec_always_uses_ch_driver` test covers `build_nomad_job_json` public wrapper but does not cover `build_nomad_job_json_with` directly — misses the `mode` parameter removal side-effect

**File**: `crates/sandbox/src/backend/nomad_ch.rs:5771–5789`.

```rust
#[test]
fn nomad_job_spec_always_uses_ch_driver() {
    let cfg = make_cfg();
    let v = build_nomad_job_json(    // <— public wrapper
        "zsbx-ch", &cfg, 4, ...
    );
    let task = &v["Job"]["TaskGroups"][0]["Tasks"][0];
    assert_eq!(task["Driver"], "ch", "ch driver is unconditional ...");
    assert!(task["Config"]["command"].is_null(), "raw_exec `command` MUST NOT appear");
}
```

This test correctly exercises `build_nomad_job_json` (the public entry point). The pre-existing `ch_plugin_jobspec_includes_all_task_config_fields` test (`:5797–5925`) exercises `build_nomad_job_json_with` directly, and also asserts `task["Driver"] == "ch"` implicitly through the Config field walk. So the ch-driver invariant IS covered for both the public and internal builder.

The gap is narrower: the T-7+T-8 commit message says "replace with `nomad_job_spec_always_uses_ch_driver` and `nomad_restore_job_spec_always_uses_ch_driver`" — but the existing test suite already had `ch_plugin_jobspec_includes_all_task_config_fields` (and the analogous test in `restore_handler.rs`) which covered the ch-driver shape fully. The new cutover tests are redundant with those pre-existing tests in terms of Driver field coverage. This is not a correctness gap; it is a test-naming confusion: the "always uses" framing implies uniqueness but the assertion is a subset of `ch_plugin_jobspec_includes_all_task_config_fields`.

More concretely: the `ch_plugin_jobspec_does_not_include_command_field` test at `:5932` already asserts `config["command"].is_null()` on `build_nomad_job_json_with`. The new `nomad_job_spec_always_uses_ch_driver` test asserts the same on `build_nomad_job_json`. Both assertions pass because `build_nomad_job_json` delegates to `build_nomad_job_json_with`. The duplication is benign but the rationale comment ("Replace the deleted `test_env_lock` tests; assert ch driver is unconditional") would be clearer if it noted these as cutover-documentation tests (regression pins that the fallback branch is gone) rather than new invariant coverage.

**Fix shape**: add a one-line comment on `nomad_job_spec_always_uses_ch_driver` clarifying "T-7+T-8 cutover regression pin: asserts the public entry point emits ch unconditionally. Structural Driver-field coverage (all TaskConfig fields typed-checked) is in `ch_plugin_jobspec_includes_all_task_config_fields`." No code change needed; the tests are correct.

**Severity**: MINOR. Zero behavioural issue. The duplication is safer than a gap.

---

### [R31-M3] `NomadStopPermitGuard` Drop's silent `let _ = try_send(())` conceals a capacity invariant violation with no metric or log

**File**: `crates/sandbox/src/backend/nomad_ch.rs:657–668`.

```rust
impl Drop for NomadStopPermitGuard {
    fn drop(&mut self) {
        let _ = self.refill.try_send(());          // silent on full-channel
        crate::metrics::dec_nomad_stop_permits_in_use();
    }
}
```

The comment at `:659–665` documents the intent: "we'd rather lose a permit than panic in a Drop on a teardown path." This is the right policy — panicking in `Drop` is generally bad. However, if `try_send` returns `Err(Full)` (the only non-disconnect failure mode on a bounded channel), the `dec_nomad_stop_permits_in_use()` call still executes, decrementing the gauge — but the permit was NOT returned to the pool. The result is `permits_available() + guards_held < capacity`: the semaphore is now understated. The gauge shows `in_use` as 1 less than actual.

This scenario only fires on a programming error (a `NomadStopPermitGuard` constructed without going through `acquire()`, or a Sender cloned outside the module and used to pre-fill). Today, `NomadStopPermitGuard` has no `pub` constructor outside `acquire()`, and the `refill` Sender isn't accessible from outside the module — so the violation is unreachable. But:

1. `NomadStopPermitGuard` is `pub struct` with a `pub` field? Let me note: the `refill` field is not `pub` — it's private. So external construction is blocked by the type system.
2. A future refactor that adds a test helper constructing `NomadStopPermitGuard { refill: ... }` directly would silently overclock the gauge.

The defensible fix: check `try_send`'s result and emit a `tracing::error!` (never panic, but always log). The gauge decrement should be conditional on the `try_send` succeeding — otherwise the gauge diverges from reality:

```rust
impl Drop for NomadStopPermitGuard {
    fn drop(&mut self) {
        if self.refill.try_send(()).is_ok() {
            crate::metrics::dec_nomad_stop_permits_in_use();
        } else {
            // Programming error: permit was not acquired through
            // `NomadStopPermits::acquire()`. Log but do not panic.
            tracing::error!(
                "NomadStopPermitGuard::drop: try_send failed (channel full); \
                 permit leaked — in_use gauge NOT decremented to stay consistent"
            );
        }
    }
}
```

**Why this is minor and not important**: the `refill` field is private; no external caller can construct a rogue guard. The scenario is unreachable today. The current code is correct for all reachable states. The smell is the gauge-decrement executing unconditionally on a path where the permit was not actually returned — if the impossible happened, the operator's saturation view would undercount.

**Severity**: MINOR. The invariant violation is structurally unreachable today. The gauge decrement should be conditional on `try_send` success to avoid silent divergence if the unreachable becomes reachable.

---

### [R31-M4] `NomadStopPermits` is always installed via `AppState::from_config`, but `stop_inner` treats `nomad_stop_permits().is_none()` as a legitimate production path in comments

**File**: `crates/sandbox/src/backend/nomad_ch.rs:1461–1468`.

```rust
// `nomad_stop_permits().is_none()` is the unit-test +
// single-tenant binary path; in those builds the field is
// uninstalled, and the cap is a no-op (matches the legacy
// behaviour those paths already tolerate).
let _permit: Option<NomadStopPermitGuard> = match self.nomad_stop_permits() {
    Some(p) => Some(p.acquire().await),
    None => None,
};
```

The comment is accurate for unit tests. But the production single-tenant binary path — `zeroship serve myapp.js` — constructs a `NomadCHBackend` and serves HTTP. The comment implies this path intentionally bypasses the global cap. However `AppState::new_fixture` (`:541–579`) and `AppState::from_config` (`:583–...`) both construct a `NomadStopPermits` and call `install_nomad_stop_permits`. So in any `AppState`-mediated path, the cap IS installed. The `None` arm is reachable only from tests that construct `NomadCHBackend::new()` directly without going through `AppState`.

The "single-tenant binary" claim is either (a) an outdated note from before `new_fixture` was wired, or (b) referring to a hypothetical future binary that bypasses `AppState`. Neither is current production behaviour.

This is a documentation accuracy issue, not a correctness issue — the cap is in fact always installed on the production binary path. The misleading phrase is "single-tenant binary path" suggesting a known production deployment that bypasses the cap; in reality no such deployment exists.

**Fix shape**: update the comment to: `// None only when `NomadCHBackend` is constructed directly in unit tests without going through `AppState`. Production paths always install via AppState::from_config.`

**Severity**: MINOR. Zero production impact. The misleading phrase could cause a future operator to believe a "single-tenant" deploy skips the global cap and is therefore safe to scale more aggressively.

---

## Cleanliness verification

### Dead-code orphan audit (T-7+T-8 cutover at `cdcd670d`)

All symbols deleted at `cdcd670d`:

| Deleted symbol | All callers confirmed deleted |
|---|---|
| `TaskDriverMode` enum | Yes — only caller was `task_driver_mode_from_env()` + `build_nomad_job_json_with` `mode` param |
| `task_driver_mode_from_env()` | Yes — only caller was `build_nomad_job_json` (its call site also deleted) |
| `wrapper_path` config field | Yes — only referent was `NomadCHConfig` (deleted) + `main.rs` startup log (fixed in same commit) |
| `test_env_lock` module | Yes — test-only module, deleted entirely |
| `nomad-vm-wrapper.sh` script | Yes — build/scripts reference deleted from `gcp-worker-startup.sh` at `c3670845` |

`grep` for `task_driver_mode_from_env\|TaskDriverMode\|wrapper_path\|test_env_lock\|SANDBOX_TASK_DRIVER` across all `crates/sandbox/src/` returns zero results. Orphan check passes.

`build_nomad_job_json_with` lost its `mode: TaskDriverMode` parameter. All call sites (production: `build_nomad_job_json` at `:2755`; tests: 5 sites in `nomad_ch.rs::tests` + 8 in `restore_handler.rs::tests`) have been updated. Confirmed by searching for any remaining `task_driver` reference in test call sites — zero.

### New `always_uses_ch_driver` tests — shape audit

`nomad_job_spec_always_uses_ch_driver` (`:5771–5789`):
- Calls the public entry point `build_nomad_job_json` with no env manipulation.
- Asserts `Driver == "ch"` and `Config.command.is_null()`.
- Does NOT need `set_var("SANDBOX_TASK_DRIVER", ...)` because the mode is now unconditional — the test is necessarily simpler than its predecessor. This is correct; the `test_env_lock` machinery was only needed to test the `SANDBOX_TASK_DRIVER` env branch that's now deleted.

`nomad_restore_job_spec_always_uses_ch_driver` (restore_handler.rs`:4823–4851`):
- Calls `build_restore_nomad_job_json` directly (internal builder).
- Asserts `Driver == "ch"`, `Config.command.is_null()`, and `Env.ZSBX_RESTORE_FROM` round-trips the path.
- The extra `ZSBX_RESTORE_FROM` assertion is a regression pin on the env-parity claim in the cutover docs. Well-shaped.

Both tests assert the negative (`command` is null) as well as the positive (`Driver == "ch"`). This is the right discipline — a future jobspec builder that accidentally emits both `"ch"` AND a `command` field would pass a Driver-only check but fail the null-command check.

### NomadStopPermits Drop semantics — structural invariant check

The invariant `tokens_in_channel + guards_held == capacity` holds by construction:

- `NomadStopPermits::new(N)`: `flume::bounded(N)` + N `try_send(())` fills the channel to N. Guards held = 0. Invariant: N + 0 = N. ✓
- `acquire()`: one `recv_async()` removes a token. One guard is created. Invariant preserved. ✓
- `Drop(guard)`: one `try_send(())` adds a token. One guard is destroyed. Invariant preserved. ✓

The `try_send` in `Drop` can only return `Err(Full)` if `tokens_in_channel == N` at the moment of the send, which requires `guards_held == 0` (by invariant), which contradicts "we are in Drop of a guard". The failure is structurally unreachable through any path that goes through `acquire()` — confirmed by the private `refill` field in `NomadStopPermitGuard`.

**The only subtle point**: `NomadStopPermits::new()` uses the same `tx` (stored as `refill`) for BOTH the initial fill AND as the guard's refill channel. This means the initial `try_send` calls and the guard-drop `try_send` calls share capacity budget. The channel can hold exactly `capacity` items total. The 5-test battery in `nomad_ch.rs::tests` (`:8015–8290`) covers the balance, cap-enforcement, gauge-tracking, and stop_inner integration shapes — the design is correct and the tests are thorough.

### Production `unwrap()`/`expect()` since r30 baseline

New `expect()` calls introduced by `ade8fb46`:
- `NomadStopPermits::new:594` — `expect("flume::bounded(N) accepts first N try_sends")` in a loop that pre-fills the channel. Structurally infallible: `try_send` on a bounded channel with `len < capacity` CANNOT fail. The `expect` is a compile-time assertion, not a runtime guard. Acceptable.
- `NomadStopPermits::acquire:617–619` — `expect("NomadStopPermits tokens channel is never disconnected...")` on `recv_async()`. The semaphore holds the only surviving `Sender` clone (`refill`), so disconnect is impossible for the lifetime of `self`. The `expect` correctly documents the invariant. Acceptable.
- `backend.state.write().unwrap()` at `:8217` — test-only (`#[compio::test]`). Not a production path.

Zero new bare production unwraps. Production unwrap discipline holds for this round's new code.

### Cfg-gate uniformity

No new test-scaffolding `pub` items added this round. Prior `R28-API2` cfg-gate sweep unchanged. No regression.

---

## Bottom line

r31 lands clean on both T-7+T-8 raw_exec deletion and r30-A1 NomadStopPermits.

- **Zero critical findings. Zero important findings.**
- **R31-M1** is the most substantive new finding: 9 actively misleading "wrapper" comment sites across the restore path's public trait surface and production orchestrator. The `submit_restore_job` rustdoc (restore_handler.rs:415–416) and the `config.json` rewrite comment (restore_handler.rs:979–982) attribute active responsibilities to a deleted script. Fix is comment-only rewording; the code is correct. Priority: medium — impacts restore-path contributors.
- **R31-M2** is a test-naming clarity issue — the new cutover tests are redundant regression pins (correct intent) but their name implies they are the only Driver-coverage tests when `ch_plugin_jobspec_includes_all_task_config_fields` already covered the same invariant more thoroughly.
- **R31-M3** is the `NomadStopPermitGuard` Drop's unconditional gauge decrement — should be conditioned on `try_send` success to avoid gauge divergence if the unreachable becomes reachable through a future test helper.
- **R31-M4** is the stale "single-tenant binary path" comment in `stop_inner` — the `None` arm of the permit check is test-only, not a known production deployment mode; the comment could mislead an operator reasoning about cap coverage.

**Code-quality lens reads HEAD `ba1df3f0` as production-ready.** The T-7+T-8 deletion is clean: zero orphans, correct replacement tests, and the stale-comment cluster is confined to documentation (no dead code). The r30-A1 semaphore implementation is structurally correct; its flume-bounded-channel pattern is idiomatic for zero-tokio Rust and the three-test battery (balance, cap-enforcement, gauge-tracking) plus the stop_inner integration test are the right coverage surface. The six open r30 minor carries are unchanged; R31-M1's comment cluster is the only new actionable item requiring code touch.
