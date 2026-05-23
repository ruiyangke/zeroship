# Sandbox Snapshot/Restore — Code-Quality Review (Round 3)

Date: 2026-05-24
Branch: `feat/sandbox-snapshot-restore` @ `4340e3b5`
Scope: `crates/sandbox/**`
Prior rounds: r1 (17), r2 (7).

## Trend (`crates/sandbox/src/`)

| Pattern | r1 | r2 | r3 | Trend |
|---|---|---|---|---|
| `Result<_, String>` | 142–164 | 167 | **153** | Improving |
| `Duration::from_secs` (src total / nomad_ch) | 30+ | 63 nomad_ch | **62 / 25** | Holding |
| `.unwrap()` (src) | – | – | 251 | Baseline |
| Poison-tolerant `into_inner` | 30 | 30 | 30 | Holding |
| Top fn LOC | 410 | 266 | **301 (`from_config`)** | No 500-LOC fn |
| `tokio::` in src/ | 0 | 0 | **0** | Clean |
| `#[allow(dead_code)]` in src/ | – | – | 7 | Baseline |

Prior-round closures verified:
- **r2 T7 (sweep sequential):** Closed; `sweep.rs:493` uses `join_all`; `sweep_concurrency_is_actually_concurrent` (`sweep.rs:664`) pins wall-time + max-in-flight.
- **r2 A2 (GCS SHA enforcement):** Closed (`f32507ce`).
- **r2 A5/A6 (`pub(crate)` + builders):** Closed (`2e0d17f7`, `d9b95c2e`).
- **r2 C2 (`stop_preserving_state` deletes sealed record):** **Still open** — confirmed real critical bug below.

## Findings

### CRITICAL
1. **`stop_inner` deletes the sealed signing key on the snapshot-teardown path** — `crates/sandbox/src/backend/nomad_ch.rs:1160-1168`. Runs unconditionally regardless of `remove_host_dir`:
   ```rust
   if let Some(persist) = &self.persist {
       if let Err(e) = persist.delete(sandbox_id).await { ... }
   }
   ```
   `stop_preserving_state` calls `stop_inner(.., false)` specifically to preserve state across snapshot→wake, then immediately torches the sealed record the restore path needs to authenticate the awakened VM. The log message ("next boot's restore loop … finds it unreachable") describes a *real* stop and is materially wrong here. The B15 test only asserts host_dir survives, not `persist`. **C2 r2-deferred — confirmed as a data-destroying bug.** Fix: gate `persist.delete()` behind `remove_host_dir`.

### MAJOR
2. **`run_idle_eviction_once` lies about "attempted" rows under shutdown** — `sweep.rs:443-446`. `attempted` is cloned from the full row set *before* the chunk loop, so when `shutdown()` short-circuits between chunks, callers see every row as attempted, including chunks that never ran. The function doc (line 415) explicitly promises the opposite. Fix: build the vec incrementally inside `snapshot_rows_chunked`, or return `Vec<(SandboxRow, AttemptResult)>`.

3. **Test fixtures hard-code `alloc_running_timeout_secs: 60` after T3 bumped the default to 120** — 11 call sites: `lib.rs:1267, 1399` (the *new A6 fixture, introduced this cycle*), `backend/docker.rs:832`, `backend/nomad_ch.rs:3449`, `config.rs:753`, plus six in `tests/sandbox_*.rs`. T3's fixer flagged the deviation; it remains unresolved, and A6 copy-pasted the stale `60` literal — pattern *worsened* this cycle. Tests now pin old timing; regressions exercising the new 120 s budget are masked.

### MAJOR (API)
4. **`AppState::with_persistence` returns `Result<Self, String>` that is statically `Ok`** — `lib.rs:262-268`. Docstring defends as "future invariants without signature break", but a fallible check added later is a *behavioural* break regardless of signature. Every caller writes `.expect("builder accepts …")` over an infallible op (`lib.rs:1477, 1495, 1498`). Adds 1 to the `Result<_, String>` count under a misleading-by-design API — exactly the systemic anti-pattern this branch has been escaping. Prefer `-> Self`; add `try_with_persistence` later if needed. (`with_admin_token` has a real empty-string footgun; this builder does not.)

### MINOR
5. **`#[allow(dead_code)] pub(crate) fn persist()` accessor in production source** — `lib.rs:265`. Docstring admits it's for tests + "future code." Belongs in `#[cfg(test)]`. Two of seven `src/` `#[allow(dead_code)]` annotations are now "anchor for future code" shape (`sweep.rs:591`, `snapshot_handler.rs:892`); this one regressed the pattern this cycle.

6. **`IdleSnapshotter` is `Send + Sync` but T7's doc claims per-row futures are "intentionally !Send under compio"** — `sweep.rs:253` vs. `sweep.rs:452`. Futures *are* !Send (no `+ Send` on the Box), but the contradictory framing in the new doc-comment misleads the next implementor.

7. **`join_all(parsed.iter().map(|(_, uuid)| snapshotter.snapshot_one(*uuid)))` is lifetime-fragile** — `sweep.rs:493-496`. Works today because `snapshotter` outlives the chunk; the next refactor that moves it or substitutes per-row owned closures will hit cryptic `'a` errors. Prefer `FuturesUnordered` or pre-collected `Vec<Pin<Box<…>>>`.

8. **Stale module-doc header contradicts the new chunked-concurrent shape** — `sweep.rs:13-16`: *"v1 keeps it simple by running serially in batches of N, no semaphore."* Contradicted by the next stanza (17-19) updated this cycle. Readers exiting after the first paragraph infer wrong behaviour.

## Score: 78/100

- **Correctness 70** — #1 is critical data loss on restore happy-path; #2 silently wrong telemetry under shutdown.
- **Performance 85** — T7 closed the concurrency hole; `cap=2` floor is L2-upload-bound.
- **Security 82** — A5/A6 closed the `pub` field surface; #1 is restore-correctness, not key disclosure.
- **API Design 75** — A6's `Result<Self, String>` is exactly the anti-pattern this branch has been escaping.
- **Rust Idioms 80** — No tokio creep, poison-tolerant Mutex policy held, no new `into_inner` in churned code. The `#[allow(dead_code)] pub(crate) fn` accessor is the one stylistic regression.
