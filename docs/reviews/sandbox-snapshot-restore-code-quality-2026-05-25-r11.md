# Sandbox/snapshot-restore — code-quality r11 review

Date: 2026-05-25 (UTC)
HEAD at audit: d2cfcb34
Round 11 of N.

## Summary

- **5 findings** (0 CRITICAL, 2 MAJOR, 3 MINOR). One major (R11-Q1) is a
  near-duplicate of the prior R9-S4/S4b uid-check shape — already openly
  tracked by `e4e5db60`'s commit body as "third sibling identified, NOT
  fixed in this commit" (R9-S4c). The remaining novel finding is
  R11-Q2 (uid-check duplication: same 5-line block now copy-pasted
  across two modules — strong signal for the missing shared helper).
- **Score: 76/100** (▲ 3 from r10's 73). +3 reflects:
  - R10-Q1 closed at `228569d3` with three wire-shape negative tests
    (sentinel-substring assertions; the cleanest possible regression-
    pin for a leak class) and `err_safe` correctly hoisted to
    `pub(crate)` rather than duplicated. **Round-6 carry resolved.**
  - No new prod `.unwrap()` sites added since r10. Same ~57 prod
    unwraps (registry/k8s/docker locks still bare — R10-Q3 unchanged).
  - No new `Result<_, String>` introduced in recent commits;
    `claim_orphan_transient_for_recovery` (landed pre-r10) uses typed
    `DatabaseError` end-to-end. `unregister_restored` (be246395)
    returns `bool` — appropriate (idempotent-on-missing).
  - Net `+93 LOC` in handlers.rs is **all tests** (R10-Q1 fix was 3
    one-line swaps + 88 LOC of test scaffolding).
- **Inertia signals** (open across multiple rounds):
  - R10-Q2 `clock_resync` 147 LOC — **round 3** (r9 #2 → r10 → r11).
  - R10-Q3 registry/k8s/docker bare-unwrap locks (45 sites) — **round 2** but
    really the inverse of r9 #9's "42 poison-recover sites" carry.
  - R10-Q4 `sig.rs:120` hyphenated UUID doc-comment — **round 3**
    (r9 api-surface #2 → r10 → r11). Smallest possible fix in the file;
    not closing it is a signal.
  - R10-Q5 `_ref_imports` dead-by-design fn — **round 3** (r9 #6 → r10 → r11).
  - R10-Q6 `Duration::from_secs(N)` 70 literals, no central timeouts
    mod — **round 4** (r9 #7 → r10 → r11; pre-r9 several rounds).
  - R10-Q7 `register_restored` default `Ok(())` — **round 7** (R5-Q1
    → R6-Q1 → R7-Q1 → R8-Q1 → R9-Q1 → R10-Q7 → R11). Same shape
    R7-S2 fixed for `derive_agent_url`. This is the longest-running
    open code-quality finding in the worktree.

## Clippy output (sandbox crate)

`clippy` is **not installed** in this environment (same status as
r10): `cargo 1.94.0`, `which rustup` → not found, `cargo-clippy` binary
absent. `~/.cargo/bin/` only contains cargo-expand / cargo-flamegraph
/ cargo-insta / cargo-llvm-cov / cargo-tarpaulin. Workspace
`Cargo.toml` declares `[workspace.lints.clippy] all = deny, pedantic =
warn, nursery = warn` — clippy gates at CI; cannot reproduce locally.
r11 falls back to grep-driven structural audit.

## Clippy output (sandbox-agent crate)

Same — clippy unavailable. Findings below are derived from
`Grep` + AST-by-eye on the source files.

## Trend numbers (delta from r10)

| Metric | r10 sandbox | **r11 sandbox** | r10 sb-agent | **r11 sb-agent** |
|---|---|---|---|---|
| `Result<_, String>` (whole-file grep) | 167 | **178** (+11; all in handlers.rs test mod for R10-Q1) | 15 | **16** |
| `.unwrap()` (all, incl. tests) | 296 | **~300** (+3 R10-Q1 test asserts via `body_json`) | 143 | **143** |
| `.unwrap()` (prod paths) | ~57 | **~57** (unchanged — registry 31, k8s 8, docker 6, preview 5, preview_share 1 + lib const-bound `NonZeroUsize`) | ~2 | **~2** |
| `Duration::from_secs(N)` literals | 64 | **64** | 6 | **6** |
| `err(500, ..., format!("...{e}"))` raw-leak sites | 3 (handlers.rs:670/821/837) | **0** ✓ R10-Q1 closed | 0 | **0** |
| Bare `.{read,write,lock}().unwrap()` (no poison-recover) | 51 (35+10+6) | **45** (registry 31, k8s 8, docker 6) | 0 | **0** |
| `unwrap_or_else(\|p\| p.into_inner())` poison-recover | 42 | **42** | 1 | **1** |
| TODOs / FIXMEs (prod) | 3 | **3** (db.rs:494, k8s.rs:495, snapshot_store_gcs.rs:1051; **none added** since r10) | 0 | **0** |
| Longest fn LOC | 272 (main) / 270 (preview_proxy) / 263 (do_restore_inner) | **unchanged** | 147 (`clock_resync`) | **148** (off-by-one from awk; same fn) |
| Test attributes (`#[test]` + `#[compio::test]`) | — | **306** sandbox | — | (not counted) |

**Reconciliation**:

- `Result<_, String>` "+11 sandbox": all in `handlers.rs` test mod (the
  R10-Q1 tests pass `&'static str` raws through `err_safe`; grep
  matches the `Result<_, String>` shape in test-only signatures).
  Prod count is **unchanged** from r10.
- "Bare lock unwrap" count dropped 51 → 45 not because anything was
  fixed, but because my r10 count of 35 in registry.rs included some
  test-mod sites; r11 confirmed 31 prod + a few test sites in
  registry. k8s: 8 (vs r10's 10 — same false-count, refined). docker:
  6 (unchanged). The MAJOR is still real — just smaller blast than
  r10 stated.

## Findings (NEW since r10)

### MAJOR

#### [R11-Q1] `db.rs::enforce_password_file_mode` (line 812) — same uid-check vulnerability as R9-S4/S4b, openly tracked in commit body but unfixed (MAJOR, code-quality-r11, **NEW** but pre-acknowledged at `e4e5db60`)

- **File**: `crates/sandbox/src/db.rs:812-831`
- **Symptom**: The function checks `meta.permissions().mode() & 0o777 != 0o400` and rejects, but does **not** check `meta.uid() != 0`. Same vulnerability shape that R9-S4 closed for `snapshot_aead.rs::RootKek::from_path` (`cca1e74d`) and R9-S4b closed for `persist.rs::AeadKey::from_path` (`e4e5db60`). A non-root attacker who can pre-create a `chmod 0400` file at `SANDBOX_DATABASE_PASSWORD_PATH` before systemd starts can supply an attacker-controlled DB password into the controller's DSN.
- **Commit `e4e5db60` body explicitly tracks this** (verbatim from the message):
  > "Third sibling identified, NOT fixed in this commit (separate scope): `crates/sandbox/src/db.rs::enforce_password_file_mode` (line ~812) loads `SANDBOX_DATABASE_PASSWORD_PATH` with mode 0o400 checked but uid not. Same fix shape applies; tracked separately to keep this commit focused on the AEAD-key loader."
- **Action**: Add `let uid = meta.uid(); if uid != 0 { return Err(DatabaseError::Validation(format!("SANDBOX_DATABASE_PASSWORD_PATH={path:?}: owner uid {uid} != 0..."))); }` between line 827 and 828. Identical pattern to the two siblings. Mechanical, <8 LOC. (Note: the threat model for a DB password is different from a KEK — the password protects only DB confidentiality, not snapshot confidentiality — so this is **MAJOR** not CRITICAL by impact. Still: same fix, same author, openly deferred.)
- **Inertia signal**: a known-issue-with-fix-shape openly identified in a commit body **6 commits ago** (`e4e5db60`), labelled "separate scope" but not landed in the 6-commit interval. This is the cleanest possible follow-up: 5-line edit, no behaviour change beyond the failure mode. Suggests either the author treats "separate scope" as a deferral signal larger than 6 commits or the deferred list isn't being walked.

#### [R11-Q2] uid-check + mode-check 0o400 + read-file logic copy-pasted across 2 (soon 3) modules — extract shared helper `validate_root_owned_secret_file(path, expected_len)` (MAJOR, code-quality-r11, **NEW**)

- **Files**: `crates/sandbox/src/snapshot_aead.rs:186-216` (RootKek::from_path), `crates/sandbox/src/persist.rs:333-368` (AeadKey::from_path); R11-Q1 above will add a third copy at `db.rs:812-831` when fixed.
- **Symptom**: Three near-identical blocks of:
  1. `std::fs::metadata(path)` with `format!("...stat: {e}")` map_err
  2. `permissions().mode() & 0o777 != 0o400` reject
  3. `meta.uid() != 0` reject (or missing in db.rs — R11-Q1)
  4. Length check (varies: AEAD 32, KEK 32, password — variable)
  5. `std::fs::read` or `File::open + read_exact`
  Differences are: env-var-name string in error messages, expected length (32 vs 32 vs variable), and storage type (`[u8; 32]` buf vs `String`). The control-flow shape is identical.
- **Action**: Introduce in `lib.rs` or a new `secret_file.rs` module:
  ```rust
  /// Validates that `path` exists, is owned by root, mode 0o400,
  /// and reads exactly `expected_len` bytes. Returns the raw bytes.
  pub(crate) fn read_root_owned_secret_file(
      path: &Path,
      env_var_name: &str,
      expected_len: Option<usize>,
  ) -> Result<Vec<u8>, String> { ... }
  ```
  All three call sites collapse to a 1-line invocation. Re-arms a future fourth-sibling vuln class (next time someone adds a secret-file loader) to fail-by-default with the right check. Net: ~30 LOC removed across 3 files + 1 helper with its own focused test.
- **Risk**: the three current sites have subtly different error-message phrasing (`"{ROOT_KEK_ENV}={path:?}: stat: {e}"` vs `"stat AEAD key file {path:?}: {e}"`); the wire-shape tests for each loader are coupled to those strings. The helper either accepts a `&str` prefix-format closure or the tests get updated. Either is small.
- **Inertia signal**: R9-S4 (round 5) flagged the KEK vuln, R9-S4b (round 5) flagged the AEAD vuln — both shipped fixes that duplicated the check rather than extract. With a third copy pending (R11-Q1), the extract case becomes structural.

### MINOR

#### [R11-Q3] `clock_resync` JSON-parse path leaks `serde_json::Error` to wire body (MINOR, code-quality-r11, **NEW**)

- **Files**: `crates/sandbox-agent/src/handlers.rs:592` and `:777`
  ```rust
  Err(e) => return err(400, format!("invalid JSON body: {e}")),
  ```
- **Symptom**: Same wire-leak class as the original R10-Q1 (just 400-level instead of 500-level). `serde_json::Error` carries the byte offset, line/col, and a brief reason ("expected `,`", "invalid type: string"). While the leak surface is small — no secrets, no host paths — the **shape** of having `format!("{e}")` flow into a 400 body is exactly what `err_safe` was built to centralise. Two call sites, both in the agent's `handlers.rs`.
  - Note: `err_safe` (sandbox crate) currently only logs raw via `tracing::error!` for `status >= 500`. For 400-level you'd just sanitize the wire body and drop the journald log (or keep it at `tracing::warn!`).
- **Action**: Two routes:
  1. **Tiny**: keep raw-on-wire (justified — 400-class signalling, no secrets in serde_json::Error); add a comment line citing R11-Q3 acknowledging the deliberate choice. ~2 LOC.
  2. **Symmetric**: introduce `err_safe` on the agent side too; route serde errors to journald only. The agent already has `crate::error_envelope::error_from_status` — that wrapper could grow a sanitized variant. ~15 LOC.
- I'd accept either (1) or (2). Important is that the decision becomes explicit rather than the current "this is what the code does." If choosing (1), grep across r10/r9 reviews — this leak shape was implicitly accepted as ok in r10 footnote.

#### [R11-Q4] Three new `pub(crate)` fns at `be246395` carry doc-comments but `R9-S4b`'s new test fns (`aead_key_from_path_*`) lack `///` summary doc — minor inconsistency (MINOR, code-quality-r11, **NEW**)

- **Files**: `crates/sandbox/src/persist.rs:1072` (`aead_key_from_path_rejects_non_root_owned_file`), `:1109` (`aead_key_from_path_accepts_root_owned_file_when_running_as_root`)
- **Symptom**: Both new test fns have block comments above them (`// The file is created by the test-runner process ...`) but **no `///` doc-comment**. Other tests in the same file use `/// ...` summary lines (e.g. `seal_filename_for_str_round_trips_with_canonical_uuid` at :1132 is preceded by a `//` block, also non-doc). The convention in the test module is **mixed**; only sometimes used. Not a regression — just a stylistic miss. Worth a one-pass cleanup *at the file scope* if doing so.
- **Action**: Either standardise on `///` (rustdoc-eligible — helpful when `cargo doc --document-private-items` runs in CI) or accept inline `//` block as the test-mod convention. Pick one. Either way: not blocking.

#### [R11-Q5] `register_restored` default no-op — still open (**carry from R5-Q1; round 7**)

- **File**: `crates/sandbox/src/restore_handler.rs:162-170`
- **Status**: Identical to R10-Q7. Verified at d2cfcb34: doc-comment still says "Default impl is a no-op `Ok(())` so the in-crate `StubRestoreBackend` (test scaffolding) doesn't need to implement state-map registration just to keep the existing pg-gated tests compiling." Body still `Ok(())`. The contrast with `derive_agent_url` at :187 (no default impl per R7-S2) is now stark: the same author wrote both, fixed one, left the other.
- **Why this matters more now**: be246395 added `unregister_restored` to `NomadCHBackend` (state-map cleanup). If a future restore-backend impl forgets to implement `register_restored`, then `unregister_restored` on the same impl has nothing to unregister — silent fail-CLOSE. But if a future impl implements `register_restored` only via the default no-op (deliberate or accidental), state-map entries never appear, so `unregister_restored` finds nothing — silent fail-OPEN at the ghost-state-entry layer. R10-C1 (the bug be246395 fixed) is **exactly** the failure mode the default no-op enables.
- **Action**: Remove the default impl. `StubRestoreBackend` provides an explicit `fn register_restored(...) -> Result<(), String> { Ok(()) }` with a comment "stub: no state map in tests." Mechanical change touching ~3 impls. Same shape R7-S2 used.
- **Inertia**: 7th round. This is the longest-running open code-quality finding in the worktree. The cost of the fix (~10 LOC) is tiny relative to the round-count.

## Closed by recent commits

- **[R10-Q1]** handlers.rs:670/821/837 raw `{e}` leak (`err` → `err_safe`) — **CLOSED at 228569d3** (round 6 carry resolved).
  - Verified: `grep 'err\(50[0-9].*format!.*\{e\}' crates/sandbox/src/*.rs` returns 0 matches at HEAD.
  - 3 new wire-shape tests added (`r10_q1_backend_{stop,exec,file_tree}_sanitizes_raw_driver_error` at handlers.rs:1303/:1327/:1350). Each feeds a sentinel-bearing raw error through `err_safe` and asserts (a) the sentinel substring is absent from the wire body and (b) the fixed public message is present. Cleanest possible regression-pin for this leak class.
  - `err_safe` correctly bumped from private `fn` to `pub(crate) fn` in admin_handlers.rs:228 (1-line change). Single source of truth for sanitization preserved.

## Carry-forward (still open)

| Item | Status | Round count |
|---|---|---|
| **[R10-Q2]** `clock_resync` 147 LOC | OPEN — unchanged (verified 148 LOC at HEAD by awk) | round 3 |
| **[R10-Q3]** registry.rs 31 + k8s.rs 8 + docker.rs 6 = 45 bare lock-unwrap sites vs 42 poison-recover idiom elsewhere | OPEN — unchanged | round 2 |
| **[R10-Q4]** sig.rs:120 hyphenated UUID stale doc-example | OPEN — unchanged | round 3 |
| **[R10-Q5]** proxy.rs:552 `_ref_imports` dead-by-design fn | OPEN — unchanged | round 3 |
| **[R10-Q6]** 64 sandbox + 6 sb-agent = 70 `Duration::from_secs(N)` literals, no central `timeouts` mod | OPEN — unchanged | round 4 |
| **[R10-Q7 / R11-Q5]** `register_restored` default `Ok(())` no-op | OPEN — unchanged; impact analysis updated (interaction with be246395 `unregister_restored`) | **round 7** (R5-Q1 origin) |
| **[r9 #3]** `stop_sandbox` 241 LOC | OPEN — unchanged | round 4 |
| **[r9 #4]** `main` 272 LOC / `preview_proxy` 270 LOC | OPEN — unchanged | round 3 |
| **[r9 #5]** `clock_resync_post_restore` `Result<(), String>` | OPEN — unchanged | round 4 |

## Hunt-list resolution

| # | Item from brief | Verdict |
|---|---|---|
| 1 | R10-Q1 closure verified? + 3 wire-shape tests | **Yes**. Confirmed via `git show 228569d3`. 3 new tests at `handlers.rs:1303/:1327/:1350`. Sentinel assertions correct. CLOSED. |
| 2 | err_safe consistency / other `err(500, ..., format!("{e}"))` | **All-clear**. `Grep err\(50[0-9].*format!.*\{e\}` returns empty. 3 remaining `err(500, ...)` sites in the crate (handlers.rs:1254 in test code, admin_handlers.rs:1165 fixed string, admin_handlers.rs:1560 in test code) all use fixed strings — none compose raw `e`. |
| 3 | uid-check duplication — 3rd site at `db.rs::enforce_password_file_mode` | **Confirmed**. Reported as **R11-Q1** (MAJOR — same vuln class, openly deferred in `e4e5db60` commit body). Also reported as **R11-Q2** (extract shared helper — uid-check + mode-check + length-check + read is now copy-pasted in 2 modules; fixing R11-Q1 makes it 3). |
| 4 | `Result<_, String>` audit, 5 highest-LOC modules | **No new** prod `Result<_, String>` since r10. Highest-LOC modules: `nomad_ch.rs` (4923 LOC, ~39 sites unchanged), `db.rs` (3023 LOC, fully typed via `DatabaseError`), `restore_handler.rs` (2367 LOC, ~22 sites unchanged), `lib.rs` (2267 LOC, ~5 boot-time sites — accepted convention), `admin_handlers.rs` (1781 LOC, well-typed via `RestoreHandlerError`/`SnapshotError`). `claim_orphan_transient_for_recovery` (added pre-r10 at `0e71e5c4`) is fully typed — Good. `unregister_restored` (be246395) returns `bool` (idempotent semantics) — Good. |
| 5 | Magic numbers in recent commits | `claim_orphan_transient_for_recovery.threshold_secs: i64` is passed in by caller (sweep.rs:187), sourced from `DEFAULT_TRANSIENT_TIMEOUT_SECS = 120` at sweep.rs:67 + env-overridable via `SANDBOX_TRANSIENT_STATE_TIMEOUT_SECS`. **Properly named.** No new magic ints since r10. `TRANSIENT_TAKEOVER_POLL_SECS = 30` (sweep.rs:73) similarly named. |
| 6 | TODO / FIXME audit | **3 TODOs total** (db.rs:494, k8s.rs:495, snapshot_store_gcs.rs:1051 — all pre-existing). **None added** by recent commits. |
| 7 | Newly-added pub fns doc-comments | `unregister_restored` (nomad_ch.rs:1706): has 12-line `///` doc-comment ✓. `contains_for_test` (nomad_ch.rs:1720): has 5-line `///` doc-comment ✓. `boot_init_sandbox_id` (sandbox-agent/lib.rs:99, pre-r10): has doc ✓. `claim_orphan_transient_for_recovery` (db.rs:2500, pre-r10): has 25-line `///` doc-comment ✓. **All recent pub fns properly documented**. (Minor exception: the new R9-S4b *test* fns lack `///` — see R11-Q4, MINOR.) |

## Inertia table

Tracking how long each finding has been carried.

| Finding | First raised | Rounds open | Round-count signal |
|---|---|---|---|
| **R10-Q7 / R11-Q5** `register_restored` default Ok(()) | R5-Q1 (round 5) | **7** | Long-standing. Fix is ~10 LOC, mechanical, with established precedent (R7-S2). |
| **R10-Q6** central timeouts mod | r9 #7 + earlier | 4 | Numbers stable (70 literals). Each round just confirms the count. |
| **r9 #5** `clock_resync_post_restore` Result<(), String> | r9 #5 | 4 | — |
| **r9 #3** stop_sandbox 241 LOC | r9 #3 | 4 | — |
| **R10-Q2** clock_resync 147 LOC | r9 #2 | 3 | — |
| **R10-Q4** sig.rs:120 hyphenated UUID | r9 api-surface #2 | 3 | One-line doc edit. Not closing this is itself the signal. |
| **R10-Q5** _ref_imports dead-fn | r9 #6 | 3 | 14 LOC delete. Same signal as R10-Q4. |
| **r9 #4** main 272 / preview_proxy 270 | r9 #4 | 3 | — |
| **R10-Q3** registry bare-lock-unwrap (45 sites) | r10 (escalated from r9 #9) | 2 | — |
| **R11-Q1** db.rs uid-check | e4e5db60 commit body (pre-r11) | 1 (formally) | Pre-acknowledged 6 commits ago as "separate scope, NOT fixed." Same fix as R9-S4/S4b. |
| **R11-Q2** uid-check helper extract | r11 | 1 | Becomes structural once R11-Q1 lands. |
| **R11-Q3** sb-agent JSON parse {e} | r11 | 1 | Acknowledge-or-route decision. |
| **R11-Q4** test fn doc-comments | r11 | 1 | Stylistic. |

## Score derivation

r10 = 73/100. Deltas:
- +5 R10-Q1 closure (round-6 CRITICAL gone — biggest single win since this axis began)
- +1 no new prod unwraps, no new TODO, no new `Result<_, String>`, all recent pub fns documented
- −1 R11-Q1 (MAJOR — pre-acknowledged 6 commits ago, still open; counts against inertia)
- −2 R11-Q5 / R10-Q7 (round 7 of `register_restored` default — at some point round-count itself is a code-quality signal)

Net: 73 + 5 + 1 − 1 − 2 = **76/100**.

## Recommendations for the next cycle

In rough impact-per-LOC order:

1. **R11-Q5 / R10-Q7** — `register_restored` default removal (~10 LOC, 3 impls touched). Closes the longest-running carry; same shape as R7-S2 which already shipped.
2. **R11-Q1** — `db.rs` uid check (~5 LOC). Pre-acknowledged in commit body; smallest possible fix; clears the third sibling.
3. **R11-Q2** — Extract `read_root_owned_secret_file` helper (~30 LOC removed, 1 helper added). Best done **with** R11-Q1 so the third call site uses the helper rather than the inline pattern.
4. **R10-Q4** — sig.rs:120 doc-edit (1 LOC).
5. **R10-Q5** — proxy.rs:552 delete `_ref_imports` (14 LOC removed).

Items 1–5 above total ~60 LOC of diff for **2 carry resolutions + 1 acknowledged-defer closure + 1 doc fix + 1 dead-code removal**. High ROI batch.
