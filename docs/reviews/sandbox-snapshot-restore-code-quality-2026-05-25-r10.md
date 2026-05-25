# Sandbox/snapshot-restore — code-quality r10 review

Date: 2026-05-25 (UTC)
HEAD at audit: 9678a840
Round 10 of N.

## Summary

- **7 findings** (1 CRITICAL, 2 MAJOR, 4 MINOR).
- **Score: 73/100** (▲ 2 from r9's 71). Net: r9 #8 (files.rs "8 prod unwraps") **disproved** — actually 0 prod; r9 #10 (RESYNC_CHALLENGE_CAPACITY no eviction-bound test) closed laterally by r9 AEAD negative-tests precedent (test-coverage axis); the long-standing handlers.rs:670/821/837 raw `{e}` leak still open at round 6; new MINOR uncovered: `registry.rs` carries 35 `RwLock::{read,write}().unwrap()` sites with **no** poison-recover (`unwrap_or_else(|p| p.into_inner())`) idiom while 42 sites elsewhere in the same crate use the recover pattern.

## Clippy output (sandbox crate)

`clippy` is **not installed** in this environment (`cargo 1.94.0` only; `which rustup` → not found; no `cargo-clippy` binary). Workspace `Cargo.toml` declares `[workspace.lints.clippy]` with `all = deny, pedantic = warn, nursery = warn` — clippy would gate at CI; we cannot reproduce here. r10 falls back to grep-driven structural audit (same approach r9 used).

## Clippy output (sandbox-agent crate)

Same as above — clippy unavailable. Findings below are derived from text-search + AST-by-eye on the source files.

## Trend numbers (delta from r9)

| Metric | r9 sandbox | **r10 sandbox** | r9 sb-agent | **r10 sb-agent** |
|---|---|---|---|---|
| `Result<_, String>` fn signatures (whole-file grep) | 185 | **167** | 42 | **15** |
| `pub fn -> Result<_, String>` signatures | — | **42** | — | **7** |
| `.unwrap()` (all, incl. tests) | 644 | **296** | 143 | **143** |
| `.unwrap()` (prod paths — corrected) | 289 (claimed) | **~57** (audited: registry=35, preview=5, k8s=10, docker=6, preview_share_handlers=1) | 139 (claimed) | **~2** (reap.rs LRU const-bound; exec.rs:224 was a comment hit) |
| `.expect(...)` (all) | — | **147** | — | **16** |
| `panic!(...)` | — | **16** | — | **2** |
| `Duration::from_secs(N)` literals | 71 | **64** | 12 | **6** |
| Longest fn LOC | 273 (`main`) | **272** (`main`), 270 (`preview_proxy`), 263 (`do_restore_inner`), 241 (`stop_sandbox`) | 147 (`clock_resync`) | **147** (`clock_resync` unchanged) |
| `format!(...{e}...)` sites (prod, all files) | — | **~96** | — | **~30** |
| `unwrap_or_else(\|p\| p.into_inner())` (poison-recover) | — | **42** | — | **1** |
| Module-wide `#![allow(unsafe_code)]` | — | **1** (`sandbox-agent/src/files.rs` — documented, openat2 RAII wrap) | — | — |
| Function-scoped `#[allow(unsafe_code)]` | — | **2** (`db.rs:2709` test mod; `sandbox-agent/handlers.rs:892` settimeofday) | — | — |

**Reconciliation with r9 numbers**: r9 #8 claimed "8 prod unwraps in `sandbox-agent/src/files.rs`" — the `mod tests {` block at line 497 contains all 46. r9 used `#[cfg(test)]` line offset (first match) as the cut, but earlier `#[cfg(test)] pub fn test_set_sandbox_id` at handlers.rs:135 short-circuited the heuristic to "0 prod" (a false negative for handlers.rs, false positive elsewhere). r10 cuts at the *outer* `mod tests {` block instead, yielding:

| File | r10 prod unwraps |
|---|---|
| `sandbox/src/registry.rs` | **35** (all `RwLock::{read,write}().unwrap()` — no poison-recover) |
| `sandbox/src/preview.rs` | **5** (`.unwrap()` on already-checked Options — comments document the invariant) |
| `sandbox/src/backend/k8s.rs` | **10** (`Mutex/RwLock::{lock,read,write}().unwrap()`) |
| `sandbox/src/backend/docker.rs` | **6** (same — lock unwraps) |
| `sandbox/src/preview_share_handlers.rs` | **1** |
| `sandbox-agent/src/reap.rs:200` | **1** (`NonZeroUsize::new(1024).unwrap()` — const-bound, infallible) |
| `sandbox-agent/src/exec.rs:224` | **0** (was a comment, not code) |

Total prod unwraps: **~57 sandbox + ~1 sandbox-agent** (after de-duping). r9's "281→289 +8" trend is therefore not a real prod-path regression — it was conflated with test-mod growth (registry's `mod tests` block has many test unwraps, AEAD negative tests added ~30 test unwraps, etc.).

## Findings (NEW since r9)

### CRITICAL

#### [R10-Q1] handlers.rs:670/821/837 raw backend `{e}` leak — **round 6 carry, still open** (CRITICAL, code-quality-r10)

- **Files**: `crates/sandbox/src/handlers.rs:670, 821, 837`
- **Symptom**: Brief notes this carry-forward against sandbox-agent, but the leak is **in `crates/sandbox/`**, not sandbox-agent. Verified at HEAD:
  ```
  670: return err(500, "backend_stop_failed", format!("backend.stop: {e}"));
  821: Err(e) => err(500, "backend_exec_failed", format!("backend.exec: {e}")),
  837: Err(e) => err(500, "backend_file_tree_failed", format!("backend.file_tree: {e}")),
  ```
  `admin_handlers.rs:228 fn err_safe` exists precisely for this pattern (the doc-comment says "Operators recover the raw error from journald keyed by `tracing::error!` line below; the `code` field on the wire is the stable client contract"). Backend errors are `String`-typed and can contain kubeconfig hints (k8s), kubelet stderr, container IDs, Nomad alloc IDs. The CRITICAL has now persisted **6 rounds** (r5 → r6 → r7 → r8 → r9 → r10).
- **Action**: Three call-site swaps: `err(...)` → `err_safe(... , raw=e)`. Mechanical; <20 LOC patch.

### MAJOR

#### [R10-Q2] `crates/sandbox-agent/src/handlers.rs:736-883 clock_resync` 147 LOC unchanged (MAJOR, code-quality-r10, carry from r9 #2)

- **Files**: `crates/sandbox-agent/src/handlers.rs:736-883`
- **Symptom**: Function is still 147 LOC (verified — opening `pub async fn clock_resync(req, state, body)` at :736, closing `}` at :883). Same shape r9 documented:
  - signature verify with skew-bypass (:743)
  - JSON parse with `format!("...: {e}")` leak (:751) — itself a minor `{e}` issue at the agent
  - `OnceLock` boot id assertion + 3 distinct audit/metric branches (:760-787)
  - hex-shape validation (:793-808)
  - LRU contains+put under `Mutex` (:828-841)
  - `libc::time_t::try_from` (:846)
  - `unsafe { libc::settimeofday(&tv, null) }` (:867)
  - errno render (:868-876)
  - tracing + metrics + json response (:877-882)
- **Action**: Extract three pure helpers — `fn validate_resync_body(body, agent_id) -> Result<ResyncBody, ResyncError>`, `fn check_replay_lru(challenge) -> Result<(), ResyncError>`, `fn set_realtime_clock(ts: u64) -> io::Result<()>`. The `unsafe` block then lives in a 4-line function (single SAFETY block, single-purpose). Handler shrinks to ~40 LOC of orchestration. Each helper independently unit-testable without mocking `HttpRequest`/`State`/`Bytes`.

#### [R10-Q3] `registry.rs` uses bare `.unwrap()` on 35 RwLock/Mutex guards while 42 sites elsewhere use poison-recover idiom (MAJOR, code-quality-r10, **NEW**)

- **Files**: `crates/sandbox/src/registry.rs` (35 sites: 196, 205, 239, 252, 298, 299, 308, 319, 331, 347, 349, 350, 363, 365, 385, 390, 428, 430, 464, 474, 476, 483, 487, 496, 499, 511, 513, 527, 534, 540, 552, 559, 563, 574, 584)
- **Symptom**: A poisoned lock here panics the entire ntex worker. Compare with `sweep.rs:294`, `sweep.rs:776`, `backend/nomad_ch.rs` (≥17 sites), `backend/k8s.rs:806` — all use `unwrap_or_else(|p| p.into_inner())` to keep going across a poison. `registry.rs` is the single hot-path `SandboxRegistry` shared by every HTTP request — a panic in *any* handler under any lock here cascades to a worker death.
  - Backend `k8s.rs` + `docker.rs` (16 sites combined) also use bare `.unwrap()` on guards — same MAJOR class, smaller blast radius (admin-side calls only).
- **Action**: Two viable shapes:
  1. **Mechanical**: introduce `pub(crate) fn lock_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T>` and `read_recover/write_recover<T>(r: &RwLock<T>)` helpers in `lib.rs`; sed-replace the 51 sites (35 registry + 10 k8s + 6 docker). Net: ~3 new helper fns, 0-LOC bloat at call sites (identical len after rename).
  2. **Structural**: switch `parking_lot::{Mutex, RwLock}` (no poison concept). Bigger diff, removes the conceptual question entirely. parking_lot is already a transitive dep via lru. The whole crate-wide poison idiom would dissolve.
- Recommend (1) for r10, (2) as a v2 cleanup.

### MINOR

#### [R10-Q4] `crates/sandbox-agent/src/sig.rs:120` doc-comment example contradicts B24-FOLLOWUP `.simple()` wire form (MINOR, code-quality-r10, **carried from r9 api-surface #2**)

- **Files**: `crates/sandbox-agent/src/sig.rs:120`
- **Symptom**: Doc example uses hyphenated UUID `019486f5-…`; B24-FOLLOWUP (`66029821` + `9a61e66e`) established `.simple()` (32-char hex, no hyphens) as the canonical wire form. Controller signs `sandbox_id.simple().to_string()` at `restore_handler.rs:1549`; agent reads `SANDBOX_AGENT_SANDBOX_ID` set from `.simple()` at `nomad_ch.rs::ZSBX_SANDBOX_ID`. An implementer reading sig.rs:120 will hand-roll the wrong format.
- **Action**: Change `019486f5-…` → `019486f5d4e07b428abf3d2c4e1a6f7c` (no hyphens). One-line doc edit. r9 api-surface review flagged this — it remains unfixed in the r10 cycle.

#### [R10-Q5] `_ref_imports` dead-by-design fn still present (MINOR, code-quality-r10, **carry from r9 #6**)

- **Files**: `crates/sandbox-agent/src/proxy.rs:552`
- **Symptom**: 10-line `#[allow(dead_code)] fn _ref_imports() { … }` whose entire purpose is "keep `sig`/`HeaderName`/`Uri`/`CanonicalKind` imports legal for a future rev." A comment in code form. Verified still present at HEAD (line 552, unchanged from r9 finding #6).
- **Action**: Delete the function and the four `use` lines that only it references (the imports are otherwise unused). Net: -14 LOC.

#### [R10-Q6] `Duration::from_secs(N)` literals: 64 sandbox + 6 sandbox-agent = 70 sites unchanged in shape from r9 (MINOR, code-quality-r10, **carry from r9 #7**)

- **Files**: 70 sites spread across 18 files; `restore_handler.rs` alone spans 5/10/15/30s in flow-control timeouts.
- **Symptom**: Hard-coded second counts at each call site. No `mod timeouts { pub const SUBMIT_RESTORE_DEADLINE_S: u64 = 30; … }` central definition. The SLO budget for restore (submit → livez → resync → register) can't be read from one place, so any operator-tuning sweep requires a grep+diff across both crates.
- **Action**: Introduce `crates/sandbox/src/timeouts.rs` with the named constants for each phase (submit, livez_poll, resync_transport, alloc_running). Use in `restore_handler.rs` first; sandbox-agent has only 6 sites and is lower-priority.

#### [R10-Q7] `register_restored` trait default `Ok(())` no-op still open (MINOR, code-quality-r10, **R5-Q1 carry-forward — 6 rounds**)

- **Files**: `crates/sandbox/src/restore_handler.rs:162-170`
  ```rust
  fn register_restored(
      &self,
      _sandbox_id: Uuid,
      _vm_index: i16,
      _signing_key_bytes: [u8; 32],
      _user_id: &str,
  ) -> Result<(), String> {
      Ok(())
  }
  ```
- **Symptom**: A new backend impl that forgets to override silently passes — exactly the silent fail-OPEN pattern R7-S2 removed for `derive_agent_url` (lines 180-187). The doc-comment justifies the default impl as "so the in-crate `StubRestoreBackend` (test scaffolding) doesn't need to implement state-map registration just to keep the existing pg-gated tests compiling." That trade ships a silent failure to production.
- **Action**: Same fix shape R7-S2 used — remove the default impl, force every impl (including stubs) to override. `StubRestoreBackend` can `return Ok(())` explicitly with a comment — the failure mode is then a compile error for new impls, not a silent runtime no-op. Mechanical change touching ~3 impls.

## Recent landed since r9 — quick QA pass

| Commit | File | Code-quality assessment |
|---|---|---|
| `10bddc20` boot_init_sandbox_id wrapper | sandbox-agent/lib.rs:99 | Clean — `pub` justified (bin entry; lib/bin split forces it), `init_sandbox_id_from_env` correctly `pub(crate)`. Doc comment explicitly calls out that this is the canonical surface bin code reaches through. Good. |
| `0e71e5c4` CAS-orphan claim + `bf2fa3bd` followup | sandbox/db.rs:2500 (claim_orphan_transient_for_recovery), sweep.rs:181 (call site) | Clean. New `pub async fn claim_orphan_transient_for_recovery` uses typed `DatabaseError` errors (no `Result<_, String>`); pre-condition checks with `DatabaseError::Validation`; CAS-lost vs not-found distinguished explicitly. Sweep.rs:201 matches on `DatabaseError::CasLost { .. }`. Two new test fns in `sandbox_pg_e2e.rs` exercise self-host refusal + ABA-safe lessee bump. Reviewed for `unwrap()` — none. |
| `6f314025` Tiered::put spawn_blocking | sandbox/snapshot_store_gcs.rs:1066 | Clean. Removed stale "// Synchronous I/O inside the task" TODO comment; closure body now sync; `.detach()` preserves fire-and-forget. -3 LOC net. |
| `419c154b` AEAD negative tests (+237 LOC test module) | sandbox/snapshot_aead.rs:957+ | Clean. 7 new `aead_decrypt_rejects_*` tests for header validation arms (bad magic, version, cipher_tag, nonce_prefix, truncated, chunk length over/under). Assert on error-message substring (only way to pin the arm since all collapse to `InvalidArtifact(String)`). All in `mod tests` — proper test isolation. |

## Hunt-list resolution

| # | Item from brief | Verdict |
|---|---|---|
| 1 | clippy on sandbox crate | Cannot run — `cargo clippy` not in PATH (no rustup; nix-store cargo only). Workspace declares deny/warn levels at root Cargo.toml `[workspace.lints]`. |
| 2 | clippy on sandbox-agent | Same — unavailable. |
| 3 | unwrap/expect/panic prod audit | Done. Production unwraps audited: ~57 sandbox + 1 sandbox-agent. Bulk justified (lock unwraps, const-bound `NonZeroUsize`, post-`is_some` Options). One systematic gap: `registry.rs` 35 sites of bare lock unwrap (no poison-recover) — **R10-Q3** above. Expect: 147 sandbox + 16 sandbox-agent (most are infallible constructors or test asserts). Panic: 16 + 2 (test-mod). |
| 4 | format!("...: {e}") leak | Verified. Original brief said handlers.rs:670/821/837 in `sandbox-agent`; actually `sandbox` crate. Still open. Sandbox-agent has its own format!(`{e}`) sites at handlers.rs:566, 751 (JSON parse paths — argued acceptable since `serde_json::Error` is shape-only, no secrets) and several in files.rs (path prefixes ok). |
| 5 | Arc::clone vs .clone() | sandbox: 28 explicit `Arc::clone` calls; sandbox-agent: 0. Consistent in sandbox; no anti-patterns spotted. |
| 6 | Result<_, String> as anti-pattern | sandbox: 167 fn signatures (42 `pub`); sandbox-agent: 15 fn signatures (7 `pub`). Down from r9's higher whole-file counts. Most concentrated in `nomad_ch.rs` (39), `k8s.rs` (33), `restore_handler.rs` (22). Boot-time `from_env` sites are accepted convention (start-up errors → operator log → exit). |
| 7 | new pub fns last 8 commits | Two new public items:<br>• `claim_orphan_transient_for_recovery` (sandbox/db.rs) — justified (consumed by sweep + e2e tests).<br>• `boot_init_sandbox_id` (sandbox-agent/lib.rs) — justified (bin/lib split forces a public lib surface; well-documented). Plus `pub(crate) unregister_restored` + `pub(crate) contains_for_test` at be246395 — appropriate scope.<br>No surface bloat. |
| 8 | stale doc comments | `sig.rs:120` hyphenated UUID example (R10-Q4). The `_ref_imports` dead-fn (R10-Q5) is the most aggressive stale-by-design. No other stale references to renamed types spotted. |
| 9 | magic numbers | `RESYNC_CHALLENGE_CAPACITY = 32` (commented r9 #10), `NONCE_TTL_S = 30`, `NONCE_CACHE_CAPACITY = 10_000`, `NonZeroUsize::new(1024)` reap stash. All have comments. `Duration::from_secs(N)` literals — R10-Q6 (70 sites). |
| 10 | half-rename naming | `clear_snapshot_metadata` is the only `fn clear_` site (db.rs:2279). No `clear_full_X`/`clear_X` half-renames spotted. |

## Carry-forward

- **[handlers.rs:670/821/837]** raw `{e}` leak — still open at **`crates/sandbox/`** (NOT sandbox-agent as the brief said; r9 also pointed at sandbox crate). Round 6.
- **[R5-Q1]** `register_restored` default `Ok(())` — still open at `restore_handler.rs:162-170`. Round 6.
- **[r9 #2 / R10-Q2]** `clock_resync` 147 LOC. Round 2.
- **[r9 #3]** `stop_sandbox` 241 LOC (was 242 in r9; trivial shift, no split landed). Round 3.
- **[r9 #4]** `main` 272 LOC (was 273); `preview_proxy` 270 LOC unchanged. Round 2.
- **[r9 #5]** `clock_resync_post_restore` `Result<(), String>` at `restore_handler.rs:1668` (current LOC: 86). Round 3.
- **[r9 #6 / R10-Q5]** `_ref_imports` dead-by-design fn. Round 2.
- **[r9 #7 / R10-Q6]** 70 `Duration::from_secs(N)` literals, no central `timeouts` mod. Round 3.
- **[r9 #8]** **DISPROVED** — `sandbox-agent/files.rs` has 0 prod unwraps (all 46 are in `mod tests`). The r9 cut-off heuristic produced a false positive. Mark as RESOLVED-BY-MEASUREMENT.
- **[r9 #9]** Lock-poison-recover idiom open-coded 42 times in sandbox crate; r10 escalates this to MAJOR (R10-Q3) because `registry.rs` (35 sites) is the *opposite* — bare `.unwrap()` with no recover.
- **[r9 #10]** `RESYNC_CHALLENGE_CAPACITY` eviction-boundary test missing. **Status carried to test-coverage axis** — not strictly a code-quality concern.
