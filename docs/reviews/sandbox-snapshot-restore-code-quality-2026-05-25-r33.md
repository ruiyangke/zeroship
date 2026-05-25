# Sandbox snapshot-restore code-quality review — 2026-05-25 r33

**Reviewer**: code-quality r33 (cron-pilot)
**HEAD**: `b172cee0`. **Prior**: r32 (`7c44cc78`).
**Scope since r32**: 2 src-touching commits — `3bc689ca` (R32-M1 backend layer, 26 sites in `nomad_ch.rs` + 1 in `backend/mod.rs`) and `0fec9bc3` (R32-M1 restore+config layer, 8 sites in `restore_handler.rs` + 4 in `config.rs` + 2 test renames). One docs-only commit (`b172cee0`).

## Summary

- **3 findings**: 0 critical, 0 important, 3 minor (all R32 carries).
- **R32-M1 CLOSED clean.** Verified by `git grep wrapper crates/sandbox/src/`: 27 hits remain, all legitimate generic English usage (AsyncWrapper, AEAD wrapper, Result panic-catch wrapper from compio spawn, "thin wrapper for unit tests", `ClockResyncOutcome` typed wrapper, transport-error wrapper text in a test-assertion message). Two intentionally retained `(legacy: pre-T-8 the deleted bash wrapper did …)` parentheticals at `nomad_ch.rs:52` and `:1417` preserve deletion history — explicitly tagged, not false attribution.
- **R32-M5 CLOSED** via the two test renames in `0fec9bc3`: `derive_mac_matches_pinned_format` / `derive_tap_matches_pinned_format` are now in place at `restore_handler.rs:1455, 1462`.
- **R32-M2 still OPEN.** `vm_index_ceil` rustdoc now reads "Must be ≤ 155 — the IP arithmetic in the controller is `10.99.{100+idx}.2`" (line 349, wrapper attribution fixed in `0fec9bc3`) but the closing sentence at line 357-358 still says `SANDBOX_NOMAD_CH_VM_INDEX_CEIL` **(default 155)**. The env-parse default is **20** (line 931). See R33-M1.
- **R32-M3 still OPEN.** `cargo check -p zeroship-sandbox --tests` confirms both warnings persist verbatim (`SandboxAuth` unused at `restore.rs:43`, `WAKE_JOBS_T_KEEP` dead at `sweep.rs:96`). See R33-M2.
- **R32-M4 still OPEN.** `stop_inner(.., remove_host_dir: bool)` (`nomad_ch.rs:1450-1454`) unchanged. The 20-line rustdoc above it (now at `:1429-1449`) was rewritten in `3bc689ca` to drop the wrapper reference but still encodes "boolean gates two downstream behaviours". See R33-M3.
- **No new panic/unwrap regressions.** `git diff 7c44cc78..b172cee0 -- crates/sandbox/src/ | grep '^+' | grep -E '\.unwrap\(\)|\.expect\(|panic!|todo!|unimplemented!'` returns zero hits. The 42 edits are entirely doc/comment string rewrites + 2 test renames; no `?`-chain or `Result` surface changes.
- **No new `String`-typed errors.** Diff inspects clean — the `Result<(), String>` count is unchanged.

## Carry table

| Finding | r32 → r33 |
|---|---|
| **R32-M1** wrapper stragglers (25 sites) | **CLOSED** in `3bc689ca` + `0fec9bc3` |
| **R32-M2** `vm_index_ceil` rustdoc default 155 | OPEN — see R33-M1 |
| **R32-M3** two pre-existing rustc warnings | OPEN — see R33-M2 |
| **R32-M4** `stop_inner` boolean blindness | OPEN — see R33-M3 |
| **R32-M5** test names `…matches_wrapper_pattern` | **CLOSED** in `0fec9bc3` |
| **R32-M6** clippy unavailable | tooling gap, logged |
| **R31-M2** test-naming clarity | OPEN |
| **R31-M3** Drop unconditional gauge dec | OPEN |
| **R31-M4** "single-tenant binary path" comment | OPEN |
| **R30-M1** gc_stopper catch_unwind asymmetry | OPEN |
| **R29-M1** `ClockResyncOutcome` string-match | OPEN |
| **R29-M2** dead `Ok` arm | OPEN |
| **R29-M3** `Any { .. }` panic-payload (14 sites) | OPEN |
| **R28-M2 / R29-M5** silent `unwrap_or(0)` SystemTime | OPEN |
| **R29-M4 / M6** test format! / heredoc-comment | OPEN |
| **R30-M2..M6** gc_stop_* clusters | OPEN |
| **R27-M1/M3/M4/M5** cosmetic | OPEN |

---

## MINOR (carries; no new findings this round)

### [R33-M1] `vm_index_ceil` rustdoc still says "default 155" (R32-M2 carry)

**File**: `crates/sandbox/src/config.rs:357-358` (rustdoc) vs. `:931` (env-parse).

`0fec9bc3` fixed the wrapper-attribution clause earlier in the same paragraph ("IP arithmetic in wrapper + controller" → "IP arithmetic in controller", line 349), but the parenthetical default at the bottom was not touched:

```rust
/// … HA operators run multiple
/// controller hosts. `SANDBOX_NOMAD_CH_VM_INDEX_CEIL` (default
/// 155).
pub vm_index_ceil: u16,
```

vs.

```rust
vm_index_ceil: parse_env("SANDBOX_NOMAD_CH_VM_INDEX_CEIL", 20u16)?,
```

The "≤ 155" upper-bound is correct (IP third-octet overflow at index 155 is a real constraint of the `10.99.{100+idx}.2` scheme). What's wrong is the **default**. An operator reading the rustdoc to size their fleet would believe they get 155 slots out of the box; they actually get 20 (an 8× shortfall). r32-fixer's diff covered the bigger semantic correction in the same paragraph but missed the trailing parenthetical.

**Fix shape** (unchanged from R32-M2): rewrite "(default 155)" → "(default 20; upper bound 155 — third octet of 10.99.{100+idx}.2 overflows past 155)". A 1-line touch in the same hunk that `0fec9bc3` already edited.

**Severity**: MINOR. Doc-completeness gap with deploy-sizing impact.

---

### [R33-M2] Two pre-existing rustc warnings still on `cargo check --tests` (R32-M3 carry)

```
warning: unused import: `SandboxAuth`
  --> crates/sandbox/src/restore.rs:43:31

warning: constant `WAKE_JOBS_T_KEEP` is never used
  --> crates/sandbox/src/sweep.rs:96:18
```

Bit-identical to r32. `0fec9bc3`/`3bc689ca` were doc-only, so the warning surface didn't move. The fix shape is unchanged: delete the unused import; either delete `WAKE_JOBS_T_KEEP` or gate with `#[allow(dead_code)]` + a "documentation-role" rustdoc per the `backend/mod.rs:589` R30-M3 pattern.

The continued presence of two warnings on a non-noisy crate is a small but real signal: it desensitizes the noise budget. New warnings introduced by an unrelated change get lost in the existing two.

**Severity**: MINOR. Two one-line edits.

---

### [R33-M3] `stop_inner(.., remove_host_dir: bool)` still boolean-blind (R32-M4 carry)

**File**: `crates/sandbox/src/backend/nomad_ch.rs:1450-1454`.

`3bc689ca` rewrote the 20-line rustdoc to drop wrapper references — now reads "the ch driver hands the missing disk path to CH at StartTask and CH refuses to boot (bug #15)" — but the boolean parameter is unchanged:

```rust
async fn stop_inner(
    &self,
    sandbox_id: Uuid,
    remove_host_dir: bool,
) -> Result<(), String> {
```

with both call sites still:

```rust
self.stop_inner(sandbox_id, true).await        // :1391  stop
self.stop_inner(sandbox_id, false).await       // :1426  stop_preserving_state
```

The rustdoc itself articulates the case for an enum (lines 1429-1449 explain the bool gates *two* behaviours: `rm -rf host_dir` AND `persist.delete`). The cited `StopDisposition` enum (r3-A4 carry) is the existing model in the same file.

**Fix shape** (unchanged from R32-M4):

```rust
enum HostStateDisposition {
    /// Stop + clean: host_dir, sealed persist record, vm_index all reaped.
    Reap,
    /// Stop + preserve: host_dir AND sealed persist record survive
    /// so the next wake/restore can recover.
    PreserveForWake,
}
```

Call sites then read self-documenting: `stop_inner(id, HostStateDisposition::PreserveForWake)`.

**Severity**: MINOR. Ergonomic; current code correct. No `unsafe`/correctness path.

---

## Cleanliness verification

**Diff scope**: `3bc689ca` (`backend/mod.rs` +4/-3, `nomad_ch.rs` +61/-48 — 26 wrapper attributions rewritten + 1 trace point added at `:1188-1196` for r32-T1); `0fec9bc3` (`restore_handler.rs` +42/-29 across 8 sites and 2 test renames, `config.rs` +12/-8 across 4 sites). `b172cee0` is `docs/reviews/` only.

**Prod `unwrap()`/`expect()`**: zero new (diff confirms — additions are 100% doc/comment edits + 1 `tracing::info!` call). `unix_now` `.expect("system clock before UNIX_EPOCH")` and `state.write().unwrap_or_else(|p| p.into_inner())` unchanged at their canonical sites.

**`?`-chain regressions**: none. The diff touches no `Result`-bearing expression.

**Boolean-blindness sweep**: no new bool-typed params introduced. The `with_fence` / `permits_installed` test fixtures noted in r32 remain; pure test-helper APIs, right shape.

**Trace addition** (`3bc689ca` lines 1191-1196): `tracing::info!` with structured fields (`sandbox_id = %sandbox_id`, `job = %job_id`, `elapsed_ms = %create_started.elapsed().as_millis()`). Matches the surrounding tracing conventions in the file (e.g., `:1218-1225` "alloc running" point). No format-string or panic-path concerns.

**Retained `(legacy: …)` parentheticals**: `nomad_ch.rs:52` and `:1417`. Both explicitly frame the deletion as historical context ("pre-T-8 cutover the deleted bash wrapper did this; the Go driver took over"). Not false attribution; preserves bug-archaeology trail for the cluster-r1 review that bug #15 references.

---

## Bottom line

r33 cycle is clean. Two R32 findings closed cleanly (`R32-M1`, `R32-M5`); three carry forward (`R32-M2`/M3/M4) and are renumbered into this round (R33-M1/M2/M3). No new code-quality findings — the 42 edits across two commits are doc/comment string rewrites and two test renames, with one structured-tracing addition that follows existing conventions in the file.

The carry shape is now compact:

- **R33-M1**: 1-line doc-rot fix (default 155 → default 20).
- **R33-M2**: 2 one-line cleanups (unused import + dead const).
- **R33-M3**: enum substitution for one boolean parameter (call-site readability).

**HEAD `b172cee0` reads production-ready.** No correctness, panic-discipline, or error-typing regressions; the dominant doc-completeness backlog from r31/r32 is now down to one paragraph in `config.rs`.
