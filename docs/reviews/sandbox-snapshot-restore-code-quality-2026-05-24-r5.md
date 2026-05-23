# Sandbox Snapshot/Restore — Code-Quality Review (Round 5)

Date: 2026-05-24
Branch: `feat/sandbox-snapshot-restore` @ `0aa93a0f` (actual HEAD; user
named `15b4f9a8` but branch advanced by one commit, A3-partial
`hard_link`, mid-review)
Scope: `crates/sandbox/**`
Prior rounds: r1 (17), r2 (7), r3 (8), r4 (8).

## Trend (`crates/sandbox/src/`, from `git show <sha>:`)

| Pattern | r3 | r4 | **r5** | delta r4→r5 | Trend |
|---|---|---|---|---|---|
| `Result<_, String>` | 153 | 155 | **159** | +4 | Still creeping up |
| `Duration::from_secs` | 62 | 62 | **62** | 0 | Holding |
| `.unwrap()` | 251 | 255 | **267** | +12 | Worsening |
| `.lock().unwrap()` | – | 19 | **25** | +6 | Worsening |
| `error_response(...)` | 0 | 0 | **22** | +22 | A4 landed |
| Distinct `err()` code strings (handlers) | – | – | **38** | – | New surface |
| `#[allow(dead_code)]` in `src/` | 7 | 7 | **7** | 0 | Holding |
| Top fn LOC | 301 | 388 (r4-mis-measured) | **352** (`from_config`) | – | None ≥500 |

Notes on r4 measurements:
- r4 reported `Result<_, String>` = 172. Recount at `a9e568a2:` via
  `git show` shows **155** for `src/` only. r4's 172 must have
  included `crates/sandbox/tests/**`. Apples-to-apples, the count is
  155→159 (+4), not 153→172 (+19). API-design regression is real but
  smaller than r4 claimed.
- r4 reported the top fn as `Display::fmt` for `SandboxConfig` at
  `config.rs:1502` = 388 LOC. **`config.rs` is 1051 lines total** at
  every commit since `a9e568a2`; that line/function does not exist.
  Real top fn is `lib.rs:417 fn from_config` = 352 LOC. r4 finding 7
  is unfounded — drop it.

## Findings

### MAJOR
1. **`.unwrap()` regressed 255 → 267 (+12)** this cycle: +5 in
   `backend/nomad_ch.rs` (B19 tests at `nomad_ch.rs:4554+`), +5 in
   `restore_handler.rs` (B19 `do_restore_inner` test paths +
   `register_restored` impl unwraps the `nomad_handle` option via
   `.ok_or_else(...)?` correctly but adjacent code adds raw
   `.unwrap()`s in test helpers). `.lock().unwrap()` separately rose
   19→25 (+6) — every B19 callsite uses the lock-poison-tolerant
   `.unwrap_or_else(|p| p.into_inner())` for `state.write()`
   (`nomad_ch.rs:1668`) but NOT for the `vm_index_allocator` lock in
   `restore_handler.rs:1041-1066`. Asymmetric handling of the same
   pattern in the same commit is a review-velocity hazard.

### CRITICAL (security carryover; quantified)
2. **S4 (from security-r4) confirmed unfixed**: `admin_handlers.rs`
   has **41 sites of `err(5xx, "<code>", format!("...: {e}"))`**
   spot-check 5/5 leak raw driver errors:
   - `admin_handlers.rs:245` — `format!("admin api: pool_app: {e}")`
   - `admin_handlers.rs:322` — `format!("query: {e}")`
   - `admin_handlers.rs:574` — `format!("query: {e}")`
   - `admin_handlers.rs:635` — `format!("query: {e}")`
   - `admin_handlers.rs:862` — `format!("pool_gdpr acquire: {e}")`
   These propagate `compio-postgres` driver-error strings
   (potentially including connection strings, table names, SQL
   fragments) into the wire envelope's `message` field. A4 landed
   the envelope shape but left the leak in the `message` body.
   `error_response` provides no redaction layer.

### MAJOR
3. **A4's 38 distinct error-code strings are inconsistent across
   handlers**. Counted from `err(<status>, "<code>",...)` literals:
   - 9× `invalid_user_id` (consistent — good)
   - 6× `sandbox_not_found` in `handlers.rs` + `admin_handlers.rs`
   - 2× `not_found` (via `ErrorEnvelope::new(..., "not_found", ...)`)
     in `preview.rs` for what are also "sandbox not found" cases
     (`preview.rs:620, 638, 650`).
   - 1× `file_not_found` (`handlers.rs:857`) and 1× generic 404
     `not_found` (`preview.rs`) — same HTTP status, different code.
   Three variants (`sandbox_not_found` / `not_found` /
   `file_not_found`) for what is conceptually one wire-shape error
   class. Builder-facing UIs that key off `code` will not be able to
   render a single "not found" message. A4's wire-shape tests
   (21 new) pin the **shape** but not the **vocabulary** — they
   assert `code` is *a string*, not that the same logical error
   uses the same string everywhere.

### MAJOR
4. **B19's `register_restored` is short and clean — does NOT
   copy-paste `create`** (`nomad_ch.rs:1650-1685`). 37 LOC: derives
   `host_dir`/`job_id` from the sandbox_id (no duplication), uses
   `Entry::Vacant`/`Occupied` to refuse clobber, returns an explicit
   Err with the sandbox_id interpolated. Lock-poison-tolerant.
   **But**: the new trait method `RestoreBackend::register_restored`
   at `restore_handler.rs:162-170` has a `Result<(), String>` default
   impl that returns `Ok(())` (i.e., silently no-ops). This
   propagates the `Result<_, String>` count by +1 and gives the
   StubRestoreBackend a way to compile while skipping the
   state-map insert — exactly the silent-no-op the doc-comment at
   `restore_handler.rs:1023-1030` warns against on `RealRestoreBackend`.
   Pick one: either the trait method is mandatory (no default), or
   the default Err's loudly.

### MAJOR
5. **`do_restore_inner` grew from 162→170 LOC and now carries a
   `persist: Option<&Persistence>` plumb-through that's documented
   "only None in tests"** (`restore_handler.rs:178-181`). Production
   code passes `Some(_)`; the `None` branch logs a `tracing::warn!`
   and skips the state-map insert (`restore_handler.rs:462-471`).
   This is API-design smell: the test path differs in observable
   behaviour from the prod path. A test that drives `restore_sandbox`
   with `persist=None` will pass without exercising the post-wake
   registration, even though prod will always register. Prefer two
   constructors (`restore_sandbox_with_persist` /
   `restore_sandbox_test_only`) or make persist non-Option and
   provide a stub `Persistence` for tests.

### MINOR
6. **R3-Q3 fixture bump (`28f60d73`) correctly bumped all 7 fixtures
   from 60→120**: `grep -n "alloc_running_timeout_secs: 60" src/`
   returns zero hits at HEAD. R3-Q3 regression in r4 (the
   copy-pasted 60 in `new_fixture`) is fixed. No effect on
   `Result<_, String>` count (as expected; pure literal swap).

### MINOR
7. **`scripts_lint.rs` integration test (R4-T1, `4e6c70c1`)** is a
   3-line shellcheck wrapper that fails the build if `shellcheck
   --severity=error` returns non-zero on the wrapper script.
   Lightweight, correctly scoped. No new `#[allow(dead_code)]`.
   Net positive.

### MINOR
8. **No new `#[allow(dead_code)]` in src/** since r4 (count holds at
   7). The 3 `_anchor` patterns from r4 also hold. B19 did not
   introduce any new ones — the `restore_from_sealed` legacy method
   it sits next to has had its `#[allow(dead_code)]` since round-8.

## Score: 76/100 (+1 from r4)

- **Correctness 80** (+2) — B19 closes the cluster-smoke "sandbox
  not found" + vm_index-leak regression with a focused, clobber-safe
  insert. A3-partial `hard_link` removes a 1 GB copy from the wake
  hot path.
- **Performance 82** (+2) — A3-partial landed; hard_link bypasses
  the 1 GB `memory-ranges` copy on local-disk get. A3 full
  (spawn_blocking around the snapshot/restore I/O) still open.
- **Security 78** (−7) — S4 quantified at 41 raw-`{e}` leak sites in
  `admin_handlers.rs`; A4 wire-shape work did not address the
  payload-content leak.
- **API Design 68** (+3) — A4 unified envelope shape, but error-code
  vocabulary is inconsistent (3 variants for "not found"). Trait
  default impl on `register_restored` is a silent-no-op smell.
- **Rust Idioms 74** (−4) — `.lock().unwrap()` rose +6; new B19
  code mixes poison-tolerant `unwrap_or_else(|p| p.into_inner())`
  for the state lock with raw `.unwrap()` for the allocator lock in
  the same call chain.
