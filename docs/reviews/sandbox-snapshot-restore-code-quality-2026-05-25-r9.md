# Round-9 — Sandbox snapshot/restore code-quality review

- **HEAD** measured: `e7ecbbd6` (one commit past brief's `f1bed99a`: R5-S5 landed mid-cycle — `chmod 0444` alloc-side hard links). All metrics taken at `e7ecbbd6`; in-flight fixers (C1, R8-A4, R8-DEPLOY1+W1, R8-A3-5, A2b, A1, R5-S5) noted in passing.
- **Scope**: `crates/sandbox/src/**` + `crates/sandbox-agent/src/**` (read-only).

## Trend numbers (delta from r8)

| Metric | r7 sandbox | r8 sandbox | r8 sb-agent | **r9 sandbox** | **r9 sb-agent** | Δ vs r8 |
|---|---|---|---|---|---|---|
| `Result<_, String>` (all incl. tests) | 177 | 160 | 11 | **185** | **42** | sandbox **+25**; agent **+31** |
| `Result<_, String>` (src/, no tests) | — | — | — | **84 (est.)** | **13** | first src-only cut |
| `.unwrap()` (all) | 251 | 281 | 139 | **644** | **143** | reflects rg --count-all incl. tests |
| `.unwrap()` (src/ only) | — | — | — | **289** | **139** | sandbox **+8**; agent flat |
| `Duration::from_secs(N)` literals | n/a | 63 | 6 | **71** | **12** | sandbox **+8**; agent **+6** |
| Longest fn LOC (both crates) | n/a | ~242 | — | **273** (`main`) / **270** (`preview_proxy`) / **242** (`stop_sandbox`) | — | new high-water |

Note: r8's `Result<_, String>` agent count of 11 measured only `pub`-prefixed return sites; r9 includes all `-> Result<_, String>` (whether `pub`, `pub(crate)`, or free `fn`). On the apples-to-apples re-cut (signature returns only), sandbox-agent has 13 src sites — directionally up from r8's 11.

## Score: **71/100** (▼ 1 from r8's 72)

Net: R8-A4 closed cleanly for sandbox-agent — `error_envelope.rs` landed and `handlers.rs:223 fn err` now delegates to `error_from_status`, so r8 CRITICAL #1 is RESOLVED (one CRITICAL retired). But r8 CRITICAL #2 (handlers.rs:670/821/837 raw `{e}` leak) is **still open at round 5** — fixers in flight target sandbox-agent not sandbox. R5-S5 added another `Duration::from_secs(N)` site (snapshot_store), and `clock_resync` at agent handlers.rs:710 is now 147 LOC mixing OnceLock init, audit, LRU, syscall, and response shaping in one function. Stop_sandbox grew to 242 LOC. Per-pattern wobble continues. One CRITICAL closes; one new MAJOR opens (147-LOC handler); old CRITICAL persists.

---

## Findings

### CRITICAL

1. **`crates/sandbox/src/handlers.rs:670, 821, 837` — raw backend `{e}` leaks into `message` (carried 5 rounds; r8 marked it "4 rounds")**
   ```rust
   return err(500, "backend_stop_failed", format!("backend.stop: {e}"));
   Err(e) => err(500, "backend_exec_failed", format!("backend.exec: {e}")),
   Err(e) => err(500, "backend_file_tree_failed", format!("backend.file_tree: {e}")),
   ```
   `admin_handlers.rs:228 fn err_safe` exists precisely for this. Backend errors are `String`-typed (kubeconfig hints, kubelet stderr, container IDs). The cycle's R8-A4 work targeted sandbox-agent, leaving these three sites stale. **This is a wire-shape carrier, not a one-off.**

### MAJOR

2. **`crates/sandbox-agent/src/handlers.rs:710-857 clock_resync` is 147 LOC** — single async handler doing (a) skew-bypass signature verify, (b) JSON parse, (c) `OnceLock` sandbox-id assertion + 3 distinct audit/metric branches, (d) hex shape validation, (e) LRU contains+put under `Mutex`, (f) `libc::time_t` try_from, (g) `unsafe { libc::settimeofday }`, (h) errno render, (i) tracing + metrics + response. Each of (b)-(g) is independently unit-testable; bundled it costs ~7 mocks per test. Extract `fn validate_resync_body`, `fn check_replay_lru`, `fn set_realtime_clock(ts) -> io::Result<()>` — the `unsafe` block then lives in a 3-line function (r8 finding #8 anticipated this; the bypass surface has now grown).

3. **`crates/sandbox/src/handlers.rs:550-792 stop_sandbox` is 242 LOC** (carried verbatim from r8 #4) — still longest changed function. Same shape: backend RPC + two-phase pg flip + tombstone + audit + response, every error path bespoke prose. No split landed.

4. **`crates/sandbox/src/main.rs:23 fn main` is 273 LOC; `preview.rs:111 fn preview_proxy` is 270 LOC** — top-2 across both crates now exceed `stop_sandbox`. `main` builds the entire HTTP service inline (routing table + config wiring + signal handling); `preview_proxy` body-streams, headers, auth, and three error fall-throughs in one async block. Both refactor candidates predate this PR but escalated in r9's top-N because the sandbox-agent sweep brought new code into the surface.

5. **B22's `restore_handler.rs:1530 clock_resync_post_restore` still returns `Result<(), String>`** and stitches `format!("challenge gen: {e}")` / `format!("nonce gen: {e}")` / `format!("/_clock_resync transport: {e}")` (lines 1564, 1579, 1596) — three distinguishable error kinds collapsed into one opaque String. Caller can't decide "retry transport" vs "fail-fast on agent rejection". r8 MAJOR #3 unchanged.

### MINOR

6. **`crates/sandbox-agent/src/proxy.rs:537 fn _ref_imports`** — `#[allow(dead_code)]` function whose only purpose is to keep `sig`/`HeaderName`/`Uri`/`CanonicalKind` imports legal "for a future rev". This is a comment in code-form; deleting it and `use` lines together is the same diff and shrinks the file by 14 LOC. Dead-by-design code is still dead.

7. **`Duration::from_secs(N)` literals: 71 sandbox + 12 sandbox-agent = 83** (r8: 69; +14 across cycle, R5-S5 added a snapshot-store one, agent grew from 6→12). No central `mod timeouts` lifted. `restore_handler.rs` alone now spans 5/10/15/30s — the SLO budget can't be read from one place.

8. **`crates/sandbox-agent/src/files.rs` has 46 `.unwrap()` sites** (most of any agent file) — predominantly tests, but 8 in production paths (`unwrap_or_default`-adjacent contexts where `?` would carry the error to the caller). Drift from sandbox's idiom of typed errors.

9. **`crates/sandbox-agent/src/handlers.rs:802-815` — `LruCache` access guarded by `.lock().unwrap_or_else(|p| p.into_inner())`** (poison recovery). This is fine but the pattern is open-coded in 4 files; a `pub(crate) fn lock_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T>` helper would deduplicate. Same idiom in `restore_handler.rs` (lock recovery on resync challenge gen).

10. **R7-S1's `RESYNC_CHALLENGE_CAPACITY` bumped 4→32 at handlers.rs:82** with a thoughtful comment but no test exercising the eviction boundary. A `#[test] fn lru_evicts_at_capacity_plus_one()` would pin the comment's "12 vm_index × 3 retries = 36" math to a fact.

---

## Summary

10 findings (1 CRITICAL, 4 MAJOR, 5 MINOR). **Score: 71/100 (▼1 from r8's 72).** r8 CRITICAL #1 (sandbox-agent codeless envelope) is **CLOSED** via `error_envelope.rs`. r8 CRITICAL #2 (handlers.rs:670/821/837 raw `{e}` leak) is **OPEN at round 5** — no fixer in flight. New MAJOR (clock_resync 147 LOC). Trend numbers: sandbox `.unwrap()` src 281→289 (+8); `Duration::from_secs(N)` 69→83 (+14); `Result<_, String>` direction is signature-method-dependent (apples-to-apples agent count up 11→13).
