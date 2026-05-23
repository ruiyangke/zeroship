# Sandbox Snapshot/Restore — Code-Quality Review (Round 4)

Date: 2026-05-24
Branch: `feat/sandbox-snapshot-restore` @ `a9e568a2`
Scope: `crates/sandbox/**`
Prior rounds: r1 (17), r2 (7), r3 (8).

## Trend (`crates/sandbox/src/`)

| Pattern | r1 | r2 | r3 | **r4** | Trend |
|---|---|---|---|---|---|
| `Result<_, String>` | 142–164 | 167 | 153 | **172** | **REGRESSED** (A7 + new_fixture sites) |
| `Duration::from_secs` total | 30+ | 63 | 62 | **62** | Holding |
| `Duration::from_secs` (nomad_ch) | – | 63 | 25 | **25** | Holding |
| `.unwrap()` total | – | – | 251 | **255** | +4 (A7 fixture/tests) |
| `.lock().unwrap()` (any) | – | – | – | **19** (5 non-test) | New baseline |
| Poison-tolerant `into_inner` | 30 | 30 | 30 | **35** | Improving |
| `#[allow(dead_code)]` src/ | – | – | 7 | **7** | Holding |
| `_anchor` patterns in src/ | – | – | 2 | **3** (`sweep.rs:609 _db_anchor`) | +1 |
| `new_fixture()` adoption | – | – | – | **12 tests / 6 still struct-lit** | Mix |
| Top fn LOC | 410 | 266 | 301 | **388 (`Display::fmt` config.rs:1502)**, 364 (`from_config`) | New high |

Prior-round closures verified:
- **r3 R3-Q1 (sweep attempted)**: CLOSED at `a11ccb2d` — `snapshot_rows_chunked` builds `attempted` incrementally (`sweep.rs:476-508`). Test `idle_sweep_attempted_reflects_partial_shutdown` added.
- **r3 #1 / C2 (`stop_inner` persist.delete)**: CLOSED at `78320b56` — `persist.delete` now gated on `remove_host_dir`.
- **r3 #4 / R3-Q2 (infallible `Result<Self, String>`)**: **PARTIAL** — A7 (`config.rs:601-606`) correctly returns `Self`; A6 `with_persistence` (`lib.rs:272-278`) still returns `Result<Self, String>`, 4 in-crate `.expect("builder accepts …")` sites unchanged (`lib.rs:1559,1577,1580`).
- **r3 #3 / R3-Q3 (stale 60 fixtures)**: **REGRESSED** — A7's new `SandboxConfig::new_fixture()` (`config.rs:652`) copy-pasted `alloc_running_timeout_secs: 60`. Total `60` literal sites: **7 in src/** (was 5). All 11 prior tests now route through `new_fixture()`, so updating one literal would fix them — but the literal stayed at 60.

## Findings

### MAJOR
1. **`Result<_, String>` count regressed 153 → 172 (+19) this cycle** despite no new fallible operations. The new public `SandboxConfig::new_fixture()` (`config.rs:619-661`) is infallible but several adjacent helpers (`SandboxConfig::from_env` `config.rs:662+`, `NomadCHConfig::from_env`, `K8sConfig::from_env`) keep their `Result<_, String>` shape with no error type. The branch is moving *away* from the proposal's "typed errors at API boundaries" guideline (api-surface-r1) — not toward it. Fix: replace top-N `Result<_, String>` returns with a `thiserror`-derived `ConfigError` in one pass.

### MAJOR
2. **A7's new `SandboxConfig::new_fixture()` resurrects the stale `60` literal at `config.rs:652`** — and the rationale comment for `with_token` at `config.rs:594-600` explicitly cites R3-Q2 as the reason for using `-> Self`. The PR author knew about the R3 findings yet copy-pasted R3-Q3's stale-60 literal in the same commit. This is the 7th in-src copy; 3 of the 11 r3-noted call sites (`lib.rs:1378, 1510, 1657`) likewise still use struct-literals instead of routing through `new_fixture()`. Net: A7 widened the test-fixture mix instead of closing it.

### MAJOR
3. **`AppState::with_persistence` is the lone surviving infallible `Result<Self, String>` builder** — `lib.rs:272-278`. A7 set the precedent (returns `Self`, cites R3-Q2 by name); A6b's 5 builders return `Self`; only A6 remains `Result<_, String>`. 4 in-crate call sites (`lib.rs:1559,1577,1580` and a 4th) carry `.expect("builder accepts …")` annotations as evidence the API is wrong. The asymmetry across A5/A6/A6b/A7 builders is a code-review-velocity hazard: the next builder author will guess from the wrong neighbour.

### MAJOR (PERF, side-effect of code-quality review)
4. **A3's spawn_blocking adoption did NOT happen this cycle** — `snapshot_handler.rs:329-347` still calls `std::fs::create_dir_all`, `ch.pause/snapshot` (sync `ureq+Command`), and `store.put` (sync 1 GB SHA + AEAD + rename) directly on the compio worker. The trait's own contract (`snapshot_store.rs:95`) says callers must wrap in `spawn_blocking`; the only caller doesn't. r3 listed this as deferred; r4 confirms zero progress while A7 added 145 lines of *unrelated* config-builder code.

### MINOR
5. **Test fixtures use struct-literal vs `new_fixture()` inconsistently** — 12 out-of-crate test call sites use `new_fixture()`; 6 in-src test fixtures (`lib.rs:1378, 1510, 1657`, `config.rs:868`, `backend/docker.rs:832`, `backend/nomad_ch.rs:3489`) still struct-literal `NomadCHConfig { … }`. The split exists because `new_fixture()` is on `SandboxConfig` only — there's no `NomadCHConfig::new_fixture()` for the in-src tests that build just the nested struct. Fix: lift the `NomadCHConfig` defaults out of `new_fixture()` into `NomadCHConfig::new_fixture()`; in-src tests call that.

### MINOR
6. **`_arc_anchor` / `_db_anchor` pattern is metastasising** — 3 sites now: `restore_handler.rs:1296`, `snapshot_handler.rs:893`, `sweep.rs:609` (new this cycle). Each is `#[allow(dead_code)] fn _X_anchor()` to silence unused-import warnings whose imports could just be deleted or `#[cfg(test)]`-gated. The "anchor for future code" rationale (lib.rs:286-292 docstring on `persist()`) is dead-code-by-design. Fix: delete the anchors; gate the imports correctly.

### MINOR
7. **`Display::fmt` for `SandboxConfig` is 388 LOC** (`config.rs:1502`), now the longest fn in the crate — pushed past `from_config` (364). A pretty-printer this large suggests struct fields proliferated past what `Debug`/`Serialize`-with-derive would handle. Either auto-derive or split into per-section `fmt` helpers.

### MINOR
8. **B17 wrapper subshell at `nomad-vm-wrapper.sh:388-419` is `( … ) &` with no `BG_PID=$!`** — the trap-cleanup at line 449's `wait $CH_PID` exit path cannot explicitly kill the orphan resume subshell; relies on the inner `ch-remote ping/resume` failing fast once CH dies. Acceptable today (≤14s total bounded budget) but a code-review-velocity hazard. Simpler than Rustifying: capture `RESUME_PID=$!`, add `kill $RESUME_PID 2>/dev/null` to the trap. 3-line fix.

## Score: 75/100 (down from 78)

- **Correctness 78** — R3-Q1 + C2 closed; A3 still open (sync 1 GB on compio worker is a live correctness/throughput hazard).
- **Performance 80** — Unchanged; A3 untouched.
- **Security 85** — A7 closed the last `pub` credential field; no new exposure.
- **API Design 65** (down from 75) — `Result<_, String>` count regressed +19; A6/A6b/A7 builder asymmetry persists; fixture-mix widened.
- **Rust Idioms 78** — `_anchor` pattern grew to 3 sites; poison-tolerant `into_inner` finally improving.
