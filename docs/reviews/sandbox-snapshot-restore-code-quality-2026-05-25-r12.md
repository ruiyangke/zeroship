# Sandbox/snapshot-restore — code-quality r12 review

Date: 2026-05-25 (UTC)
HEAD at audit: ae946cba
Round 12 of N.

## Summary

- **4 findings** (0 CRITICAL, 2 MAJOR, 2 MINOR).
- **Score: 78/100** (▲ 2 from r11's 76). +2 reflects:
  - **R11-Q1 CLOSED at `2c10f63a`** — `db.rs::enforce_password_file_mode`
    third-sibling uid-check shipped (matches R9-S4 / R9-S4b idiom; 3
    new tests at `db.rs:2956/:2998/:3040`). The "openly deferred" item
    landed within 1 review-round of being escalated.
  - **R9-S4d shipped at `b4c3ef27`** — fourth-sibling
    `load_admin_token` uid check (sandbox lib 312 → 319 with this and
    T-7). Mirrors R9-S4/S4b/S4c. Solid pattern propagation.
  - **T-7 (`5fe36805`) is clean code** — adds `TaskDriverMode` enum +
    `task_driver_mode_from_env()` + `build_nomad_job_json_with(...,
    mode)`. Exhaustive `match` on 2-variant enum, no fall-through
    arms, no `_ => ` default. 7 new tests (319 total) pin both
    branches + the wire-shape invariants on each. The split between
    `build_nomad_job_json` (env-reading entry) and
    `build_nomad_job_json_with` (test-friendly explicit-mode helper)
    is exactly the right shape — tests don't reach for `set_var`,
    they pin the mode parameter directly. The driver-name magic
    strings (`"raw_exec"`, `"ch"`, `"ch_plugin"`) each carry a doc
    comment citing the upstream source of truth.
  - **R11-A2 update** — T-7 makes the eventual nomad_ch.rs split
    EASIER, not harder. See R12-A1 below.
- −2 from the four longest-running carries (`register_restored` r8,
  `clock_resync` r4, `Duration::from_secs` central-mod r5,
  `_ref_imports` r4) which are all 1-token / mechanical fixes that
  remain open. Round-count itself is becoming a code-quality signal.
- **Inertia signals** (unchanged from r11 unless noted):
  - **R10-Q7 / R11-Q5** `register_restored` default `Ok(())` — **round 8**
    (R5-Q1 origin). Longest-running open finding.
  - **R10-Q2** `clock_resync` 147 LOC — **round 4**.
  - **R10-Q3** registry/k8s/docker 45 bare lock-unwraps — **round 3**.
  - **R10-Q4** sig.rs:120 hyphenated UUID stale doc — **round 4**.
  - **R10-Q5** proxy.rs:552 `_ref_imports` dead-fn — **round 4**.
  - **R10-Q6** 70 `Duration::from_secs` literals, no central mod — **round 5**.
  - **R11-A1** 4-site secret-loader extract — **round 2**, now FOUR
    fully-shipped uid-checks (R9-S4 / S4b / S4c / S4d) all carrying
    the same shape. Extract case has gone from structural to obvious.
  - **db.rs:494 TODO** — landed at `27e1a8b2` (2026-05-05), 20 days
    open. Comment claims "next round picks it up"; nothing has.
    See R12-Q1 below.

## Clippy output (sandbox crate)

Same as r10/r11 — clippy not installed in this environment (`cargo
1.94.0`, no rustup, no `cargo-clippy` binary). Workspace
`[workspace.lints.clippy] all = deny, pedantic = warn, nursery = warn`
gates at CI; r12 falls back to grep + AST-by-eye on the source files.

## Clippy output (sandbox-agent crate)

Same — clippy unavailable.

## Trend numbers (delta from r11)

| Metric | r11 sandbox | **r12 sandbox** | r11 sb-agent | **r12 sb-agent** |
|---|---|---|---|---|
| Test count (cargo runs) | 312 | **319** (+7, all T-7) | 240 | **242** (+2, R9-S4d) |
| `Duration::from_secs(N)` literals (across both) | 70 | **70** | — | — |
| `err(50x, ..., format!("...{e}"))` raw-leak sites | 0 | **0** ✓ | 0 | 0 (still 2 `err(400)` sites, R11-Q3 carry) |
| Bare `.{read,write,lock}().unwrap()` (registry+k8s+docker prod) | 45 | **45** | 0 | 0 |
| `unwrap_or_else(\|p\| p.into_inner())` poison-recover | 42 | **61** (full-tree count, includes test-mod sites) | 1 | 1 |
| TODOs / FIXMEs (prod) | 3 | **3** (unchanged: db.rs:494, k8s.rs:495, snapshot_store_gcs.rs:1074) | 0 | 0 |
| `pub fn` / `pub(crate) fn` / `pub async fn` | — | **299** | — | **68** |
| Longest fn LOC | 272 (main) / 270 (preview_proxy) / 263 (do_restore_inner) | **unchanged** | 147 (`clock_resync`) | **148** (off-by-one from awk; same fn) |
| File LOC top-5 | nomad_ch 4923, db 3023, restore_handler 2367, lib 2267, admin_handlers 1781 | **nomad_ch 5371 (+448 from T-7), db 3108 (+85 from R9-S4c), restore_handler 2367, lib 2412 (+145 from R9-S4d), admin_handlers 1781** |

**Reconciliation**:

- `nomad_ch.rs` grew **+448 LOC** at T-7 — 65 LOC of new helper
  (TaskDriverMode + task_driver_mode_from_env + the typed Config
  branch in build_nomad_job_json_with) plus ~380 LOC of new tests
  (T7_ENV_LOCK guard + 7 named tests with full assertions). Body-to-
  test ratio is ~1:6 for this commit, which is healthy.
- `lib.rs` grew **+145 LOC** at R9-S4d — 4 new tests + the uid-check
  branch in `load_admin_token`.
- `db.rs` grew **+85 LOC** at R9-S4c — 3 new tests + the uid-check
  branch in `enforce_password_file_mode`.
- r11 reported 42 poison-recover sites; r12 grep shows 61 across the
  full tree, but the delta is all in **test modules** (the boot-
  loader test files added at R9-S4b/c/d each carry the
  `unwrap_or_else(|p| p.into_inner())` pattern on their local
  serialising mutex). Prod poison-recover count unchanged.

## Findings (NEW since r11)

### MAJOR

#### [R12-Q1] `Database::open_pool` TODO at `db.rs:494-507` — 20 days open, claims "next round picks it up" but no follow-up commit exists (MAJOR, code-quality-r12, **NEW** — first time flagged as code-quality, perf side surfaced at r11)

- **File**: `crates/sandbox/src/db.rs:494-507` (the 14-line doc-comment block above `async fn open_pool`).
- **Commit**: introduced at `27e1a8b2` (2026-05-05 06:20 -0700) — **20 days ago** as of HEAD `ae946cba`.
- **Symptom**: The comment opens with **`**TODO (round-1 fixer / IMPORTANT #10):**`** and ends with **"Documenting here so the next round picks it up; the current pattern is correct, just slow on the hot path."** No "next round" has touched the file's pool-creation path — `git log --pretty=format:"%h %ad %s" --date=short -- crates/sandbox/src/db.rs` shows 16 commits since `27e1a8b2`, none of which add a thread-local pool. The TODO is **stale** in the sense that the deferral signal it carried (~"will get to this in days") is no longer accurate.
- **Why MAJOR (not minor)**:
  1. Every `Database::*` method (~40 callsites across `db.rs`) hits this hot path. The TODO's own admission is "round-trips a TCP connect + auth handshake on every call." For a controller that calls e.g. `claim_orphan_transient_for_recovery` on a 30s sweep tick, every cycle pays the pool-creation cost.
  2. The comment **mis-signals the maintenance plan**. A reader who follows the code path expects the optimization is imminent (the comment was authored to communicate that); instead it has sat for 20 days. New contributors reading the file get false confidence ("don't worry, it's a known issue, someone's on it").
  3. The TODO is referenced indirectly by the perf-axis review trail (R11-P1) but has never been re-evaluated for **code-quality drift** — the comment itself is what's drifted.
- **Action**: One of —
  1. **Implement the thread-local pool** (the TODO's intended fix). Sketch: `thread_local! { static POOL: RefCell<Option<Pool>> = const { RefCell::new(None) }; }` initialized lazily per worker thread. Estimated ~40 LOC + 2-3 tests. The `Send + Clone` ntex factory bound is satisfied because each thread initializes its own — no cross-thread sharing.
  2. **Rewrite the comment to reflect reality**: drop the "next round picks it up" line and the round-1 ticket reference, replace with "Phase-1 simple pool — re-evaluate when controller-side latency budget tightens. Bench at [link to perf doc] shows ~1.5ms TCP+auth per call." Documents the trade rather than implying an open task. Estimated ~5 LOC.
  3. **File a tracking issue** and replace the TODO with `// See issue #N for pool-churn perf bug.` That moves the deferral signal out of source where it can rot.

  I'd accept (2) or (3) for code-quality; (1) is the real fix but is a perf decision. **Whatever happens, the current state — a 20-day-stale TODO that promises imminent action — is the worst combination.**
- **Inertia signal**: 20-day-old "next round" promises are the canonical "broken windows" anti-pattern. The other 2 prod TODOs (k8s.rs:495, snapshot_store_gcs.rs:1074) are similarly stale; they don't make the same "imminent" promise so they're not as misleading, but a single sweep of all 3 wouldn't be wasted.

### MINOR

#### [R12-Q2] T-7 driver name strings `"raw_exec"` / `"ch"` / `"ch_plugin"` are not constants (MINOR, code-quality-r12, **NEW**)

- **File**: `crates/sandbox/src/backend/nomad_ch.rs:2244` (env-value match), `:2403` (RawExec arm `"raw_exec"`), `:2443` (ChPlugin arm `"ch"`). The literal `"ch_plugin"` appears at `:2244` (env-var match) and in 5 test bodies.
- **Symptom**: The three magic strings are scattered across the file. Each is paired with a doc-comment citing the upstream source of truth (`PluginName` const in `nomad-driver-ch/ch/driver.go`, the env-var contract), but the strings themselves are not extracted as Rust `const`s. A typo in any of the test bodies — e.g. `"chplugin"` instead of `"ch_plugin"` — would NOT fail to compile and would silently route the test through `RawExec` (the `_ =>` arm). Several tests have this fragility.
- **Action**: Extract:
  ```rust
  /// Env-var value that opts a fleet into the Go plugin driver.
  /// Matches `task_driver_mode_from_env`'s match arm.
  pub(crate) const SANDBOX_TASK_DRIVER_CH_PLUGIN: &str = "ch_plugin";

  /// Nomad driver name for raw_exec (the bash-wrapper transport).
  pub(crate) const NOMAD_DRIVER_RAW_EXEC: &str = "raw_exec";

  /// Nomad driver name for the Go plugin. Matches
  /// `nomad-driver-ch/ch/driver.go::PluginName`. A drift here decouples
  /// the controller from the plugin and the plugin-load handshake fails
  /// with "driver not found".
  pub(crate) const NOMAD_DRIVER_CH_PLUGIN: &str = "ch";
  ```
  Then use them in `task_driver_mode_from_env`, both arms of the
  match in `build_nomad_job_json_with`, and in each test body.
  Estimated ~15 LOC + ~20 LOC of test changes (mechanical
  replacement).
- **Why MINOR**: T-7 already has a test that explicitly pins
  `task["Driver"]` to the string `"ch"` (with a comment citing
  `PluginName` at `nomad-driver-ch/ch/driver.go:36`) — so a future
  drift in the controller-side string IS caught. The risk is
  asymmetric: the test pins the driver-name string, but no test
  pins the env-value string. A typo in the env-value match arm would
  silently fall through to RawExec.
- **Adjacent**: The env-var name `"SANDBOX_TASK_DRIVER"` appears 4
  times (the match in `task_driver_mode_from_env` + 3 test
  `set_var`/`remove_var` calls inside `with_task_driver_env`).
  Bundle into the same const extract.

#### [R12-Q3] T-7 `T7_ENV_LOCK` + `with_task_driver_env` is the 2nd copy of the env-mutating-test pattern; first copy is `db.rs::tests::ENV_LOCK` (MINOR, code-quality-r12, **NEW**)

- **Files**: `crates/sandbox/src/backend/nomad_ch.rs:4076-4101` (new T-7 helper), `crates/sandbox/src/db.rs:tests::ENV_LOCK` (pre-existing).
- **Symptom**: Both define a `static Mutex<()>` to serialise
  parallel cargo-test-runner threads that mutate process-global env
  vars. Both wrap the mutation in `#[allow(unsafe_code)]` because
  `std::env::set_var` / `remove_var` are unsafe in edition 2024+
  threading model. Both use the `unwrap_or_else(|p| p.into_inner())`
  poison-recover idiom. Both restore the env on the way out.
  - T-7's commit body explicitly cites the precedent:
    > "We serialise env-touching tests with a Mutex — same pattern as
    > `crates/sandbox/src/db.rs::tests::ENV_LOCK`."
  - This is **acknowledged in the commit body** as a near-duplicate
    of an existing pattern — the same shape as R11-Q2's secret-
    loader observation.
- **Action**: Extract to a crate-private `test_env_helpers.rs` module
  (gated `#[cfg(test)]`):
  ```rust
  /// Serialises env-mutating tests across the crate. Each callsite
  /// names the env-var(s) it touches; new callers pass the names
  /// they'll mutate so the lock-scope is documented.
  pub(crate) fn with_env<R>(
      vars: &[(&str, Option<&str>)],
      f: impl FnOnce() -> R,
  ) -> R { ... }
  ```
  Estimated ~40 LOC consolidated + 2 callsites updated. Doc the
  pattern once.
- **Why MINOR**: each copy is well-commented and self-contained. The
  bug surface is the env-var name strings (which `with_env` would
  centralise into the call site, leaving them inline anyway). The
  win is mostly maintainability — future env-var-mutating tests get
  a known pattern to copy.
- **Adjacent**: When R11-A1 (secret-loader extract) lands, the same
  test files will gain another mutex (the `with_root_env` test
  variant). 3 copies is the breakpoint where extraction stops being
  optional.

## Closed by recent commits since r11

- **[R11-Q1]** `db.rs::enforce_password_file_mode` uid-check — **CLOSED at `2c10f63a`** (1 review-round after escalation in r11). 3 new tests at `db.rs:2956 / :2998 / :3040` pin the loose-perms, non-root-owned, and root-owned-when-running-as-root behaviour. Same shape as R9-S4 / S4b. **No follow-up regressions** seen at HEAD.
- **R9-S4d** (admin token uid check) — **CLOSED at `b4c3ef27`**. Mirrors the 3 siblings. Tests at `lib.rs:1601 / :1636`.

## Carry-forward (still open)

| Item | Status | Round count |
|---|---|---|
| **[R10-Q2]** `clock_resync_post_restore` 87 LOC + agent `clock_resync` 148 LOC | OPEN — unchanged | round 4 |
| **[R10-Q3]** registry.rs + k8s.rs + docker.rs 45 bare lock-unwrap sites | OPEN — unchanged | round 3 |
| **[R10-Q4]** sig.rs:120 hyphenated UUID stale doc-example | OPEN — unchanged (`019486f5-…` example still present; actual format is 32-hex no-hyphens) | round 4 |
| **[R10-Q5]** proxy.rs:552 `_ref_imports` dead-by-design fn | OPEN — unchanged | round 4 |
| **[R10-Q6]** 70 `Duration::from_secs(N)` literals, no central `timeouts` mod | OPEN — unchanged | round 5 |
| **[R10-Q7 / R11-Q5]** `register_restored` default `Ok(())` | OPEN — unchanged | **round 8** (R5-Q1 origin) |
| **[R11-A1]** 4-site secret-loader extract (now FOUR shipped uid checks: snapshot_aead / persist / db / lib) | OPEN — 4th site landed at `b4c3ef27`; extract case is now obvious | round 2 |
| **[R11-Q3]** sandbox-agent `handlers.rs:592/:777` raw JSON-parse `{e}` to wire body | OPEN — unchanged | round 2 |
| **[R11-Q4]** R9-S4b test fns lack `///` doc comments | OPEN — unchanged | round 2 |
| **[r9 #3]** `stop_sandbox` 241 LOC | OPEN — unchanged | round 5 |
| **[r9 #4]** `main` 272 LOC / `preview_proxy` 270 LOC | OPEN — unchanged | round 4 |
| **[r9 #5]** `clock_resync_post_restore` `Result<(), String>` | OPEN — unchanged | round 5 |

## Hunt-list resolution

| # | Item from brief | Verdict |
|---|---|---|
| 1 | `Database::open_pool` audit — TODO claims temporary, how long? | **R12-Q1**. Landed at `27e1a8b2` (2026-05-05), **20 days open**. The "next round picks it up" line is stale. Comment is actively misleading: it implies an imminent fix that hasn't been planned in 20 days. MAJOR. |
| 2 | T-7 nomad_ch.rs — anti-patterns? | Mostly clean. **Exhaustive `match` on 2-variant `TaskDriverMode`** (no `_ =>` fall-through, no missing arms — future enum extension causes a compile error). RawExec/ChPlugin branches produce a tuple `(driver_name, config)` so the JSON shape is built once below the match — no copy-pasted struct construction. **Two minor concerns**: (a) magic strings `"raw_exec"` / `"ch"` / `"ch_plugin"` not extracted to consts (R12-Q2); (b) `T7_ENV_LOCK` is the 2nd copy of the env-mutating-test pattern (R12-Q3). Neither is load-bearing. |
| 3 | `err_safe` coverage — any new sites that should also use it? | **No new sandbox-side leaks**. `grep -nE 'err\(50[0-9].*format!.*\{e\}' crates/sandbox/src/*.rs` returns 0 matches. The only `format!("...{e}")` shapes routed to wire are: (a) `handlers.rs:592/:777` 400-level JSON-parse (already R11-Q3 carry); (b) secret-loader validation paths in `db.rs:830/836/843` and `lib.rs:947/952/958/965` — these are **bootstrap-time** errors that surface to systemd journald via panic-on-boot, NOT to the HTTP wire surface, so `err_safe`'s log-then-sanitize idiom doesn't apply. |
| 4 | R11-A1 helper extract — any new 5th site? | **No 5th secret-loader site added**. The 4 sites (snapshot_aead / persist / db / lib) are exactly the four R9-S4/S4b/S4c/S4d closed; all four now have uid checks. Extract case is now structural — 4 copies of nearly identical code, each with its own test trio (~5 tests × 4 = 20 tests that all assert the same uid-check shape against different env-var names). |
| 5 | Capability list — new entries since R7-API2 (`c8000537`)? | **No new capabilities added** since R7-API2 closed. `git log --oneline -- crates/sandbox-agent/src/version.rs` shows `c8000537` is HEAD. Current 14-entry list (with `clock.resync-v1` mandatory pin at `version.rs:175`) is unchanged. R11-T3 mandate (presence-test pin) is satisfied for the one mandatory capability. |
| 6 | Test-count trajectory | sandbox lib 296 → 319 (+23 over 5 cycles); sandbox-agent 231 → 242 (+11 over 5 cycles). **Healthy growth**. The +7 at T-7 (`5fe36805`) and +3 each at R9-S4 / R9-S4b / R9-S4c / R9-S4d are all *defensive* tests pinning new invariants alongside their fixes; no "test count goosing" where tests are added but don't pin behaviour. Body-to-test LOC ratio at T-7 was ~1:6 — driven by 7 detailed jobspec-shape pins, not boilerplate. Trajectory is **sustainable** at this pace; the per-commit "+N tests with each fix" pattern is exactly what `R11-T3` mandated. |
| 7 | TODO / FIXME audit | **3 prod TODOs** unchanged from r11: `db.rs:494` (the 20-day-stale one, R12-Q1), `k8s.rs:495` (referenced from snapshot store, pre-r9), `snapshot_store_gcs.rs:1074` (retry-loop placeholder, GCS phase). **Zero TODOs in sandbox-agent**. Net: **no new TODOs added** since r11. |
| 8 | Does T-7 make R11-A2 (nomad_ch.rs split before T-8) EASIER or HARDER? | **EASIER**. The new `TaskDriverMode` enum + `task_driver_mode_from_env()` + `build_nomad_job_json_with(...)` form a **natural seam** for the split: everything driver-related (the enum, the env-reader, the two helpers, the 7 new tests) is co-located in a ~120-LOC region at `nomad_ch.rs:2210-2330` plus `:4076-4350` for the tests. Pulling these into `crates/sandbox/src/backend/nomad_ch/task_driver.rs` is **mechanical** — `pub(crate)` visibility is already in place. See R12-A1 below. |

## R12-A1 — Effect of T-7 on the eventual nomad_ch.rs split

**Snapshot at HEAD `ae946cba`**: `nomad_ch.rs` is **5371 LOC** (+448
from T-7). The pre-T-7 LOC was ~4923 (r11 cited 4923). The growth is
NOT a regression — it's a **clean addition of a typed alternative
path** rather than a sprawl through the existing path.

**Pre-T-7 (r11 view)**: a hypothetical split would have had to peel
the existing `build_nomad_job_json` apart by editing its body —
moving the env block, the resources block, and the Driver/Config
construction into separate modules without breaking the wire-shape
tests. High-risk surgery.

**Post-T-7 (r12 view)**: T-7 has **already done the hardest part of
the split**:

1. `task_driver_mode_from_env()` is a single env-reader. Moving it
   to a new module is `mv` + `pub(crate)` adjustment.
2. `TaskDriverMode` is a pure enum with `#[derive(Debug, Clone, Copy,
   PartialEq, Eq)]`. Trivially relocatable.
3. The driver-mode-conditional logic is now **isolated to the
   `match mode { ... }` block at `:2402-2464`** (62 LOC). Everything
   outside that block is mode-agnostic. The split could pull this
   `match` into a dedicated `fn driver_and_config(mode, ...) -> (&str,
   serde_json::Value)` helper.
4. The 7 new tests are **independently relocatable** — they all use
   `build_nomad_job_json_with(...)` (the explicit-mode form), so
   none rely on env state when the helper moves modules.
5. The `T7_ENV_LOCK` + `with_task_driver_env` test helper (~30 LOC)
   is the only piece that needs careful handling — it sits in `mod
   tests` and would need to either move with the helpers or stay in
   place (the test-only crate-private mutex pattern is per-file
   convention).

**Recommended split structure** (for R11-A2 follow-up):

```
crates/sandbox/src/backend/nomad_ch/
├── mod.rs                  (re-exports + Backend impl)
├── jobspec.rs              (build_nomad_job_json{,_with} + helpers)
├── task_driver.rs          (TaskDriverMode + task_driver_mode_from_env)
├── client.rs               (NomadClient + HTTP plumbing)
└── tests/
    ├── jobspec_tests.rs
    └── task_driver_tests.rs
```

Estimated split: ~400 LOC moves per submodule, ~1500 LOC total
relocations, 0 logic changes. **Lower risk after T-7 than before
T-7**. The brief's R11-A2 concern that T-7 would COMPLICATE the
split is unfounded — it has structurally pre-paid the refactoring
cost by introducing the right seams.

## Inertia table

Tracking how long each finding has been carried.

| Finding | First raised | Rounds open | Round-count signal |
|---|---|---|---|
| **R10-Q7 / R11-Q5** `register_restored` default Ok(()) | R5-Q1 (round 5) | **8** | Long-standing. Fix is ~10 LOC, mechanical, R7-S2 precedent. |
| **R10-Q6** central timeouts mod | r9 #7 + earlier | 5 | Numbers stable (70 literals). Each round just confirms. |
| **R10-Q2** clock_resync 147 LOC | r9 #2 | 4 | — |
| **R10-Q4** sig.rs:120 hyphenated UUID | r9 api-surface #2 | 4 | One-line doc edit. Not closing it is itself the signal. |
| **R10-Q5** _ref_imports dead-fn | r9 #6 | 4 | 14 LOC delete. |
| **r9 #4** main 272 / preview_proxy 270 | r9 #4 | 4 | — |
| **r9 #3** stop_sandbox 241 LOC | r9 #3 | 5 | — |
| **r9 #5** clock_resync_post_restore Result<(), String> | r9 #5 | 5 | — |
| **R10-Q3** registry bare-lock-unwrap (45 sites) | r10 (escalated from r9 #9) | 3 | — |
| **R11-A1** secret-loader extract | r11 | 2 | All 4 sites now shipped. |
| **R11-Q3** sb-agent JSON parse {e} | r11 | 2 | Acknowledge-or-route. |
| **R11-Q4** test fn doc-comments | r11 | 2 | Stylistic. |
| **R12-Q1** db.rs:494 stale TODO | 27e1a8b2 (2026-05-05) | 1 formally; 20 days in-code | "Next round picks it up" — never has. |
| **R12-Q2** T-7 driver-name magic strings | r12 | 1 | — |
| **R12-Q3** 2nd copy env-mutating-test pattern | r12 | 1 | — |

## Score derivation

r11 = 76/100. Deltas:

- +3 R11-Q1 closed at `2c10f63a` (1-round turnaround on the pre-acknowledged
  uid-check sibling — high signal that the deferral list is being walked)
- +2 R9-S4d closed at `b4c3ef27` (4th sibling shipped, with 4 new tests)
- +1 T-7 (`5fe36805`) is cleanly-shaped: exhaustive match, no magic-string
  smell beyond the 3-string MINOR R12-Q2, test/body ratio ~6:1 with the
  right pinning shape
- −1 R12-Q1 (MAJOR — 20-day-stale TODO that mis-signals an imminent fix)
- −1 R10-Q7 / R11-Q5 (round 8, no movement — the longest-running finding
  in the worktree continues without action)
- −2 the cluster of round-4+ minor carries (R10-Q4 hyphen-UUID, R10-Q5
  `_ref_imports`, R10-Q6 70 Duration literals) — these are 1-token-each
  mechanical fixes; their continued openness across 4+ rounds is a
  meta-signal about backlog drain priority

Net: 76 + 3 + 2 + 1 − 1 − 1 − 2 = **78/100**.

## Recommendations for the next cycle

In rough impact-per-LOC order:

1. **R10-Q7 / R11-Q5** — `register_restored` default removal (~10 LOC,
   3 impls touched). Closes round-8 carry; same shape as R7-S2.
2. **R12-Q1** — `db.rs:494` TODO cleanup (~5 LOC if rewriting the
   comment, ~40 LOC if implementing the thread-local pool). Drop
   the "next round picks it up" promise either way.
3. **R11-A1** — 4-site secret-loader extract (`read_root_owned_secret_file`).
   Now structural after R9-S4d shipped — 4 fully-symmetric copies in 4
   modules. ~30 LOC removed, ~20 LOC helper + 5 tests.
4. **R10-Q4** — `sig.rs:120` hyphenated UUID doc-edit (1 LOC). Round 4.
5. **R10-Q5** — `proxy.rs:552` `_ref_imports` delete (14 LOC). Round 4.
6. **R12-Q2** — T-7 driver-name string consts (~15 LOC + test updates).
   Quick win, pairs naturally with whoever lands T-8.
7. **R12-Q3** — env-mutating-test helper extract (~40 LOC). Defer until
   3rd copy lands; current 2 copies are tolerable.

Items 1-5 above total ~70 LOC of diff for **3 carry resolutions + 1
acknowledged-deferral closure + 1 doc fix + 1 dead-code removal**.
High ROI batch — every item closes a finding that's been open ≥4
rounds.
