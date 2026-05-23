# Round-8 — Sandbox snapshot/restore code-quality review

- **HEAD**: `c07cbb62` on `feat/sandbox-snapshot-restore`
- **Scope**: `crates/sandbox/src/**` + `crates/sandbox-agent/src/**` (added by B22)
- **Mode**: read-only

## Trend numbers (delta from r7)

| Metric | r7 (sandbox-only) | r8 sandbox | r8 sandbox-agent | r8 combined | Δ |
|---|---|---|---|---|---|
| `Result<_, String>` | 177 | **160** | **11** | **171** | sandbox −17 ; combined −6 |
| `.unwrap()` | 251 | **281** | **139** | **420** | sandbox **+30** ; combined **+169** |
| `.lock().unwrap()` | 24 | **25** | **6** | **31** | sandbox +1 ; combined +7 |
| `Duration::from_secs(N)` literals | n/a | **63** | **6** | **69** | new measurement |
| distinct `err_safe`/`error_response` codes | n/a | **59** | **0** | **59** | sandbox-agent has no `code` field |

(Note: agent `.unwrap()` is dominated by `#[cfg(test)]` — but >30 land in production paths inc. `proxy.rs`/`exec.rs`/`files.rs`.)

## Score: **72/100** (▼ 1 from r7's 73)

Net: sandbox-side `Result<_, String>` improved (−17), but B22 brought ~900 LOC and exposed sandbox-agent's older idioms to the review surface — codeless envelopes, raw `{e}` leaks, and a one-off `Result<_, String>` ABI on a new public handler. The agent crate drags the combined score back down.

---

## Findings

### CRITICAL

1. **`crates/sandbox-agent/src/handlers.rs:77-87` — agent error envelope drops `code`, diverges from sandbox §10.0 contract**
   ```rust
   fn err(status: u16, msg: impl Into<String>) -> HttpResponse {
       …  resp.json(&json!({"error": s}))   // no `code`, no `message`
   }
   ```
   Sandbox handlers emit `{"error":<code>,"message":<msg>}` (handlers.rs:38-48); the agent emits `{"error":<msg>}`. **B22's new `clock_resync` (handlers.rs:579-637) inherits the broken shape** — every 400/500 it produces (e.g. line 594, 602, 629) is unparseable by clients that branch on `code`. The two surfaces should converge before they fork further.

2. **`crates/sandbox/src/handlers.rs:670, 821, 837` — raw backend `{e}` still leaked into `message` (carried 4 rounds)**
   ```rust
   return err(500, "backend_stop_failed", format!("backend.stop: {e}"));
   Err(e) => err(500, "backend_exec_failed", format!("backend.exec: {e}")),
   Err(e) => err(500, "backend_file_tree_failed", format!("backend.file_tree: {e}")),
   ```
   `admin_handlers.rs` introduced `err_safe()` 4 rounds ago precisely to split wire-message from operator-log; these three call-sites never migrated. The `String`-typed backend errors carry kubeconfig hints, container IDs, and full kubelet stderr — should route through `err_safe()` like every PG path.

### MAJOR

3. **B22 `restore_handler.rs:1444-1505 clock_resync_post_restore` — new `Result<(), String>` ABI + new magic timeout literal**
   B22 had a clean slate to introduce a typed error (`ClockResyncError`) and a named const for the 10s timeout; instead it added one more `Result<_, String>` and one more `Duration::from_secs(10)` literal to a 69-count pile. The String error then gets `format!("…")`-stitched through `do_restore_inner`'s match arms with no way to discriminate "transport failed (retry)" from "agent rejected (don't retry)".

4. **`crates/sandbox/src/handlers.rs:550-792 stop_sandbox` is 242 lines** — longest function in the changed surface; mixes registry mutation, backend RPC, two-phase PG flip (Stopping→Stopped→tombstone), CAS-lost telemetry, audit event insert, and response shaping. Cyclomatic complexity is high and every error path is bespoke prose. Splitting into `stop_backend_and_record()` + `tombstone_and_audit()` would let each piece be unit-testable.

5. **R5-P1b test cleanup left on the floor — `sandbox_pg_e2e.rs:2501, 2543, 2568, 2769, 2884` repeat the same 2-line `Arc<dyn SnapshotStore>` upcast**
   ```rust
   let store: std::sync::Arc<dyn zeroship_sandbox::snapshot_store::SnapshotStore> =
       std::sync::Arc::new(LocalDiskSnapshotStore::new(&store_root));
   ```
   A `fn make_test_store(root: &Path) -> Arc<dyn SnapshotStore>` helper (5 LOC) collapses 5 sites and protects against the next ripple. R5-P1b knew it was touching all 5 — should have shipped the helper in the same commit.

6. **`crates/sandbox/src/restore_handler.rs:341-560 do_restore_inner` ≈ 220 lines, fanning in 9 free-fn helpers** (`build_restore_nomad_job_json`, `nomad_post_blocking`, `wait_for_alloc_running_blocking`, `wait_for_livez_blocking`, `clock_resync_post_restore`, …). The handler-as-orchestrator pattern is OK, but the helpers all have positional `&str`/`Duration` args and string errors — would benefit from a `RestoreCtx` struct bundling `agent_url`, `signing_key`, timeouts so the call sites stop spelling the same 4-tuple.

### MINOR

7. **`crates/sandbox/src/restore_handler.rs:1514-1522 clock_resync_nonce`** opens `/dev/urandom` on every call instead of using `getrandom` (already in the workspace tree via `rand`/`ring`). Mirrors `restore.rs:random_hex` only because the comment says so; not a perf issue (one call per wake) but it's a Linux-only syscall in a function that's otherwise pure.

8. **`crates/sandbox-agent/src/handlers.rs:600-621 clock_resync` — `#[allow(unsafe_code)]` is module-wide effective** because it's the only `unsafe` block. The annotation reads well; consider extracting `fn set_realtime_clock(ts: i64) -> io::Result<()>` so the `unsafe` lives in a 3-line function with a unit test against `EPERM` rather than inline in an HTTP handler.

9. **69 `Duration::from_secs(N)` literals across both crates, no central timeout const table** — `restore_handler.rs` alone has 4 different timeouts (5/10/15/30s) sprinkled across nomad_post/get/delete + clock_resync; one `mod timeouts { … }` block would make the SLO budget readable in one place.

10. **`crates/sandbox-agent/src/handlers.rs:602` — `format!("ts out of range: {}", parsed.ts)` echoes attacker-controlled `ts`** into the response body. Low-risk (signature-bound endpoint, no XSS surface in a JSON body returned to the controller) but unnecessary; the value is already in the structured log on line 626.

---

## Summary

Sandbox crate is trending the right way on systemic patterns (`Result<_, String>` 177→160); B22's 900 LOC arrived using sandbox-agent's older codeless envelope, re-opening the §10.0 wire-shape inconsistency at a brand-new public endpoint. handlers.rs:670/821/837 are now 4 rounds stale. R5-P1b shipped the trait flip but skipped the obvious test helper. Score 72/100 (−1).
