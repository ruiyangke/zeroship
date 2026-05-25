# Sandbox snapshot-restore code-quality review — 2026-05-25 r32

**Reviewer**: code-quality r32 (cron-pilot)
**HEAD**: `7c44cc78`. **Prior**: r31 (`ba1df3f0`).
**Scope since r31**: 5 src-touching commits — `b75728ce` (vm_index_ceil→20, release_delay→2s), `a395f1b5` (driver pin, no src), `4d10ba45` (R31-M1 partial cleanup), `729f22dd` (controller pin, no src), `a6e517b2` (livez+alloc poll cadence tighten).

## Summary

- **6 findings**: 0 critical, 0 important, 6 minor.
- **`4d10ba45` partial-closes R31-M1**: 9 high-visibility surfaces fixed, ~18 inline-comment + test-name sites remain. See R32-M1.
- **`b75728ce` introduces doc-rot**: `vm_index_ceil` rustdoc still claims default 155; env-parse default is now 20. See R32-M2.
- **Two pre-existing rustc warnings** now visible: `WAKE_JOBS_T_KEEP` dead, `SandboxAuth` unused import. R32-M3.
- **Boolean-blindness on `stop_inner(.., remove_host_dir: bool)`** — natural sibling for the prompt-cited `StopDisposition` enum. R32-M4.
- **No new `String`-typed errors, no new bare prod `unwrap()`/`expect()`**, no panic-discipline regressions. Perf-cadence commits are pure-numeric.
- **clippy unavailable** in this nix toolchain (`error: no such command: clippy`); substituted `cargo check --tests`. R32-M6.

## Carry table

| Finding | r31 → r32 |
|---|---|
| **R31-M1** stale "wrapper" rustdoc | PARTIAL — see R32-M1 (~18 stragglers) |
| **R31-M2** test-naming clarity | OPEN, unchanged |
| **R31-M3** Drop unconditional gauge dec | OPEN, unchanged |
| **R31-M4** "single-tenant binary path" comment | OPEN, unchanged |
| **R30-M1** gc_stopper catch_unwind asymmetry | OPEN |
| **R29-M1** `ClockResyncOutcome` string-match | OPEN |
| **R29-M2** dead `Ok` arm | OPEN |
| **R29-M3** `Any { .. }` panic-payload (14 sites) | OPEN |
| **R28-M2 / R29-M5** silent `unwrap_or(0)` SystemTime | OPEN |
| **R29-M4 / M6** test format! / heredoc-comment | OPEN |
| **R30-M2..M6** gc_stop_* clusters | OPEN |
| **R27-M1/M3/M4/M5** cosmetic | OPEN |

---

## MINOR (new this round)

### [R32-M1] R31-M1 only partially closed — ~18 "wrapper" stragglers remain

**Scope**: `4d10ba45` ("clean up stale rustdoc references to deleted nomad-vm-wrapper.sh (R31-M1)") fixed 9 surfaces — module docs, `submit_restore_job` trait rustdoc, `NOMAD_ALLOC_ROOT` const, `derive_mac/derive_tap` rustdoc. R31-M1's "additionally" cluster (inline comments + test names) was not swept.

**Remaining sites** (filtered out legitimate generic "wrapper" usages like `AsyncWrapper`/`aead wrapper`):

| File:line | Why misleading |
|---|---|
| `restore_handler.rs:21` | Module-doc step (6) still tells a reader implementing a new restore backend to expect a wrapper to consume `ZSBX_RESTORE_FROM`. |
| `restore_handler.rs:1266` | Inline `do_restore_inner` comment — same false-attribution as R31-M1 item 9 (`:979-982`) which WAS fixed. Next-paragraph-down sibling. |
| `restore_handler.rs:1470, 1503` | `rewrite_config_json_rewrites_only_net_fields` rustdoc + inline. |
| `restore_handler.rs:1973` | `RealRestoreBackend` doc — wrapper's "PR 3f branch" reference. |
| `restore_handler.rs:2028` | Rustdoc on `memory_mb` field (production trait state). |
| `restore_handler.rs:2507-2515` | 9-line rationale block in `build_restore_nomad_job_json` — "the wrapper up-front validates" + "the wrapper's defensive `[ -f $PATH ]`". **Production code path, highest priority subset.** |
| `restore_handler.rs:2903, 3936` | UUID-format inline comments — mirrors R31-M1 fix at `nomad_ch.rs:1145` (create path), not done on restore path. |
| `restore_handler.rs:1454, 1461` | Test names `derive_mac_matches_wrapper_pattern` / `derive_tap_matches_wrapper_pattern`. See R32-M5. |
| `config.rs:335, 346` | `vm_index_floor`/`vm_index_ceil` rustdoc — also has R32-M2 doc-rot at `:346`. |
| `config.rs:403` | `host_fence_timeout_secs` rationale references "virtiofsd + the bash wrapper" process tree. |
| `config.rs:431` | `subnet_second_octet` rustdoc — "and the wrapper (which lays down the tap + IP) read the same value". |
| `backend/mod.rs:487` | `teardown_source_for_snapshot` trait rustdoc — `[ ! -f $ZSBX_WORKSPACE_IMG ]` gate now is a `TaskConfig.Disks` path-missing failure in the ch driver. |

**Fix shape**: same as R31-M1 — replace "wrapper" with "ch driver"/"controller" depending on ownership. Half-cleaned is arguably worse: surface rustdoc reads clean (recently rewritten) so the reader trusts the inline-comment attribution that's still false.

**Severity**: MINOR. No code-correctness issue; doc-completeness gap on the restore reader-surface.

---

### [R32-M2] `vm_index_ceil` rustdoc claims default 155; env-parse default is now 20

**File**: `crates/sandbox/src/config.rs:344-355` (rustdoc) vs. `:931` (env-parse).

Rustdoc:
```rust
/// Must be ≤ 155 — the IP arithmetic in the wrapper + controller
/// is `10.99.{100+idx}.2` ...
/// `SANDBOX_NOMAD_CH_VM_INDEX_CEIL` (default 155).
```

Env-parse:
```rust
vm_index_ceil: parse_env("SANDBOX_NOMAD_CH_VM_INDEX_CEIL", 20u16)?,
```

`b75728ce` ("bump vm_index_ceil default 12→20") flipped the constant; the rustdoc was not updated. An operator sizing their fleet from the rustdoc will believe default is 155 and silently cap at 1/8th of the doc-described value. The "≤ 155" upper-bound clause is still correct (IP-arithmetic property).

**Fix shape**: rewrite closing paragraph to "default 20; upper bound 155 — third octet of 10.99.{100+idx}.2 overflows past 155".

**Severity**: MINOR. 1-paragraph rewrite.

---

### [R32-M3] Two pre-existing rustc warnings — dead const + unused import

```
warning: unused import: `SandboxAuth`
  --> crates/sandbox/src/restore.rs:43:31
warning: constant `WAKE_JOBS_T_KEEP` is never used
  --> crates/sandbox/src/sweep.rs:96:18
```

`WAKE_JOBS_T_KEEP` has a 12-line rustdoc claiming it's "a single source of truth for the proposal-documented value" but `grep -rn 'WAKE_JOBS_T_KEEP'` returns one result — the definition itself. The runtime read happens against `state.wake_lifecycle.wake_jobs_gc_retention_secs`; the env-default is hard-coded in `WakeLifecycleConfig::from_env`. Either gate with `#[allow(dead_code)]` + explicit "documentation role" rustdoc (matches `backend/mod.rs:589` R30-M3 pattern), or delete.

`SandboxAuth` in `restore.rs:43` is a stale refactor residue. Trivial fix.

Both pre-date r31; surfaced only because prior rounds did not run `cargo check --tests`. Worth nudging the verify discipline.

**Severity**: MINOR. Cosmetic; one-line fixes each.

---

### [R32-M4] `stop_inner(.., remove_host_dir: bool)` — boolean-blind with two opposite-value call sites

**File**: `crates/sandbox/src/backend/nomad_ch.rs:1430-1469`.

Two call sites (`:1375` `stop_inner(id, true)`, `:1407` `stop_inner(id, false)`). The 20-line rustdoc at `:1410-1429` explicitly notes the boolean gates *two* downstream behaviours (`rm -rf host_dir` AND `persist.delete`) — the name lies; passing `false` does NOT just preserve host_dir, it also preserves the sealed persist record.

The focus prompt cites the existing `StopDisposition` enum (r3-A4 carry) as the model. Same shape:

```rust
enum HostStateDisposition {
    /// Stop + clean: host_dir, sealed persist record, vm_index all reaped.
    Reap,
    /// Stop + preserve: host_dir AND sealed persist record survive
    /// so the next wake/restore can recover.
    PreserveForWake,
}
```

`stop_inner(id, HostStateDisposition::PreserveForWake)` reads correctly at the call site without requiring the reader to recall the boolean convention or the 20-line rustdoc.

**Severity**: MINOR. Current code is correct; pure ergonomic gain. The rustdoc burden exists because the boolean is overloaded.

---

### [R32-M5] `derive_mac_matches_wrapper_pattern` / `derive_tap_matches_wrapper_pattern` test names freeze the wrapper-as-source-of-truth framing post-T-8

**File**: `crates/sandbox/src/restore_handler.rs:1454, 1461`.

`4d10ba45` rewrote the *rustdoc* on `derive_mac`/`derive_tap` to cite the ch driver's `task_config.go` as source-of-truth (correct). But the test names still encode the wrapper as canonical reference. A future contributor changing the derivation rule now updates the ch driver (new source-of-truth), the rustdoc (citing the driver), and a test named "matches_wrapper_pattern" — the test name is historically misleading.

**Fix shape**: rename to `derive_mac_matches_pinned_format` / `derive_tap_matches_pinned_format`. The "pinned" framing accurately describes the pin-against-drift purpose without naming a deleted component.

**Severity**: MINOR. Test names; no behavioural impact. Separated from R32-M1 because the fix is a rename.

---

### [R32-M6] `cargo clippy` unavailable in this sandbox — tooling gap

The focus prompt asks for `cargo clippy -p zeroship-sandbox --tests 2>&1 | head -30`. This nix toolchain returns:

```
error: no such command: `clippy`
help: find a package to install `clippy` with `cargo search cargo-clippy`
```

`cargo --list` confirms clippy is not installed. Substituted `cargo check -p zeroship-sandbox --tests` which caught the two warnings in R32-M3 but not clippy-pedantic lints (`needless_borrow`, `redundant_clone`, `large_enum_variant`, etc.). A future round should either (a) install clippy via `rustup component add clippy` in the dev-shell / nix overlay, or (b) downgrade the prompt ask to `cargo check --tests`.

**Severity**: MINOR. Tooling-availability gap; logged for cron-pilot owner.

---

## Cleanliness verification

**Diff scope**: `b75728ce` config (-3/+6, R32-M2); `4d10ba45` nomad_ch+restore_handler (-12/+14, R32-M1 partial); `a6e517b2` numeric cadence edits (-8/+8, no error-handling surface). Other two commits are scripts-only (out of scope).

**Prod `unwrap()`/`expect()`**: zero new. `unix_now` `expect("system clock before UNIX_EPOCH")` (`:4436`) unchanged, panic-discipline acceptable. `state.write().unwrap_or_else(|p| p.into_inner())` (`:1438`) unchanged.

**`String`-typed error regression**: `Result<(), String>` count unchanged; perf commits touched no error-handling code. R29-M1 remains canonical open carry.

**Boolean-blindness sweep**: beyond R32-M4, other `bool` params in `nomad_ch.rs` are test fixtures (`with_fence`, `permits_installed`) — right shape for test-helper APIs.

---

## Bottom line

r32 lands clean on perf-cadence (`a6e517b2`) and `vm_index_ceil` bump (`b75728ce`). `4d10ba45` is a partial close on R31-M1 — surface cleaned, inline-comment layer down preserves the same false attribution.

- **R32-M1** is dominant: 18 stale "wrapper" sites survive, including `build_restore_nomad_job_json` rationale block (`:2507-2515`), `do_restore_inner` orchestrator comment (`:1266`), four `NomadCHConfig` field rustdocs, `teardown_source_for_snapshot` trait rustdoc, and `derive_mac`/`derive_tap` test names. Comment-only; code correct.
- **R32-M2** is a 1-paragraph doc-rot — `vm_index_ceil` rustdoc claims default 155, env-parse is 20.
- **R32-M3** documents two pre-existing rustc warnings unsurfaced by prior rounds' verify-loop.
- **R32-M4** flags `stop_inner` boolean — natural sibling for the prompt-cited `StopDisposition` enum.
- **R32-M5** test-name straggler from R32-M1.
- **R32-M6** logs the clippy-tooling gap.

**HEAD `7c44cc78` reads production-ready.** No correctness or panic-discipline regressions; dominant carry is doc-completeness on the restore path.
