# Sandbox Snapshot/Restore — Code-Quality Review (Round 7)

Date: 2026-05-24
Branch: `feat/sandbox-snapshot-restore` @ `27aa393a`
Scope: `crates/sandbox/**`
Prior rounds: r1 (17), r2 (7), r3 (8), r4 (8), r5 (8), r6 (7).

## Trend (`crates/sandbox/src/`, src-only)

| Pattern | r5 | r6 | **r7 HEAD** | Δ r6→r7 |
|---|---|---|---|---|
| `Result<_, String>` | 159 | 157 | **177** | **+20** |
| `.unwrap()` | 277 | 277 | **281** | +4 |
| `.lock().unwrap()` | 25 | 25 | **25** | 0 |
| `Duration::from_secs(N)` literals | 62 | 62 | **63** | +1 |
| Distinct error codes (src, regex match) | 20 | 43 | **~29 err_safe/error_response + ~34 if "*_failed/_error" strings counted** | rough parity |
| `#[allow(dead_code)]` attribute uses | 7 | 7 | **7** (one migrated to `mod.rs:520`) | 0 |
| `_anchor()` fns | 3 | 3 | **3** | 0 |
| `std::thread::sleep` callsites in async paths | – | – | **5** (`restore_handler.rs:1353,1386`; `snapshot_handler.rs:609`; `nomad_ch.rs:3966,4279`) | – |
| Top fn LOC | 352 | 352 | **365** (`lib.rs:417 from_config`) | +13 |

**Brief's stated r6→r7 counts were stale.** `Result<_, String>` jumped 157→177 (+20, not -2 as brief suggested). `.unwrap()` 277→281. The R5-S1 boot-time assertion + R6-A1 env-var rename added new `Result<_, String>` boundaries on the boot path (`lib.rs:444-453`); no offsetting reduction landed.

## Findings

### MAJOR

1. **handlers.rs:670, :821, :837 STILL leak raw `{e}` to wire** — same three sites r6 flagged. `err(500, "backend_stop_failed", format!("backend.stop: {e}"))` etc. S4 sanitized 41 admin sites but never crossed into the creator-facing surface. r6 deferred this to a "sister-S4b fixer"; nothing landed. Three rounds (r5/r6/r7) without a touch; not gated on architecture. **Quote (`handlers.rs:670`)**: `return err(500, "backend_stop_failed", format!("backend.stop: {e}"));`

2. **`Result<_, String>` regressed +20 in one commit window**. r6 baseline `1066a319` reported 157; HEAD `27aa393a` is 177. The R6-A1 fix + R5-S1 fail-CLOSED both added new String-error returns on the boot path (`lib.rs:417-453`) and the wrapper `_clock_resync` plumbing. r3-Q2's effort to remove infallible `Result<_, String>` is being undone faster than it lands. Pattern: every new safety check adds another `Result<_, String>` boundary instead of an enum-typed error.

3. **`register_restored` trait default impl still silent `Ok(())`** (`restore_handler.rs:162-170`). r5-Q1 flagged this; r6 noted the doc comment now frames it as a feature; **r7: unchanged at HEAD**. The trait doc-comment at `:155-161` reads "*Default impl is a no-op `Ok(())` so the in-crate `StubRestoreBackend` (test scaffolding) doesn't need to implement state-map registration*" — production safety carved out for test ergonomics. Should be `Err(...)` or no default.

### MINOR

4. **B17 wrapper subshell IS closed at HEAD — deferred file is stale.** `nomad-vm-wrapper.sh:428` captures `RESUME_PID=$!`; `:284-287` does `kill -TERM` + `wait` inside the EXIT/INT/TERM trap. r6 MAJOR #4 and deferred [R6-C1] both list this as open across 5 rounds — they're wrong at HEAD. **Deferred-file hygiene finding**: pilot should close R6-C1 in the next commit. Wrapper hygiene fix landed silently between `1066a319` and `27aa393a`.

5. **Commit 93348b91 added a new `#[allow(dead_code)]`** (`backend/mod.rs:520`) for the now-`pub(crate)` `register_restored` delegator. Doc comment at `:512-519` admits trait-dispatch bypasses the enum delegator. Count stayed at 7 only because A6's `persist()` allow at `lib.rs:294` already existed. The pattern — "added because the symmetric variant exists, even though the call path doesn't use it" — is exactly what r4-A1 (typed-state-builder by accretion) warned about. `enum Backend` is growing dead arms; `SnapshotCapableBackend` trait (r3-A1, r5-A1) would delete these.

6. **R5-P1 partial landed: 1 MiB BufReader on SHA path** (`restore_handler.rs` compute_artifact_sha256). 16× syscall reduction on 1 GB reads is real and measurable. Net positive, narrowly scoped. `store.get` spawn_blocking (R5-P1b) still requires the `&dyn → Arc<dyn>` flip — unchanged at HEAD.

7. **`from_config` grew 352 → 365 LOC** (`lib.rs:417`). R5-S1 + R6-A1 added the persist-test-override block (`:444-453`). The function is now the largest in the crate; r4-A1's `AppStateBuilder` recommendation grows more pressing every round. Top 5 by LOC: `from_config` 365, `nomad_ch::try_create` 288, `nomad_ch::stop_inner` 287, `main::main` 273, `preview_proxy` 270. **`lookup_source_vm_ops` is no longer in the top 20** — confirmed 94 LOC at `nomad_ch.rs:1709-1802`, as r6 reported.

8. **5 `std::thread::sleep` callsites remain in async-reachable code** — `restore_handler.rs:1353` (`wait_for_alloc_running_blocking`), `:1386` (`wait_for_livez_blocking`), `snapshot_handler.rs:609`, `nomad_ch.rs:3966,4279`. These block the ntex worker for the sleep duration; should be `compio::time::sleep` inside `spawn_blocking` boundary (A3 carryover).

## Score: 73/100 (−4 from r6's 77)

- **Correctness 78** (−2) — silent-default `register_restored` carryover, no new bugs, but no closure either.
- **Performance 82** (=) — R5-P1 BufReader is a clean win; A3 sync I/O on compio worker still open.
- **Security 78** (−2) — handlers.rs:670/821/837 raw `{e}` carried for a 3rd round; same threat model as the admin sites S4 closed.
- **API Design 60** (−4) — `Result<_, String>` regressed +20; signature smell accelerated.
- **Rust Idioms 72** (−4) — new `#[allow(dead_code)]` for a delegator no caller uses; trait default-impl-as-feature framing entrenches the silent-no-op.

Score-trend: r4 77 → r5 76 → r6 77 → **r7 73**. r6's +1 came from R3-Q2 cleanup; r7's −4 is the `Result<_, String>` regression + 3-round carryover of MAJOR #1.
