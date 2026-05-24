# Sandbox snapshot-restore code-quality review — 2026-05-25 r29

**Reviewer**: code-quality r29 (cron-pilot)
**HEAD**: `ce062846`. **Prior**: r28 (`568c1357`).
**Scope since r28**: 4 in-crate commits — R27-API2 cfg-gate (`c2e07b2f`), R28-C1 inline sleep (`9e1f6276`), R28-I1+I2 join! + typed half-dead-agent signal (`d00f12dd`), and the gcp-worker-startup.sh heredoc-backtick escape (`a3cfca10`). Plus 5 reviewer-artifact + housekeeping commits (no in-crate source).

## Summary

- **6 findings**: 0 critical, 0 important, 6 minor.
- **R28-C1 CLOSED at `9e1f6276`** — `CreateGuard::drop` cleanup future now inlines the delay + release. The rustdoc at `nomad_ch.rs:2274-2301` is exemplary: 27 lines explaining the compio 0.11 `Scheduler::clear` lifetime mismatch, naming `detach_isolated` + `spawn_delayed_release`, citing the long-lived ntex-worker runtime that makes `stop_inner`'s use of the helper correct, and explicitly warning future contributors not to unify the two call sites by changing the helper. Meets the r29 directive in full. Lock-poison recovery pattern (`unwrap_or_else(|p| p.into_inner())`) is consistent with the file's existing 4 sites (:374, :525, :535, :542).
- **R27-API2 CLOSED at `c2e07b2f`** — `_test_inject_sandbox` gated under `#[cfg(any(test, feature = "test-support"))]`. The `test-support` feature with a self dev-dep is the canonical idiom (tokio, axum, sqlx all use it); verified the rationale ("integration tests in `tests/` link as external crates where `cfg(test)` does NOT apply") — 6 integration test files in `crates/sandbox/tests/` call the function, confirming the gate's necessity. The author chose `cfg(any(test, feature = "test-support"))` over the simpler `cfg(test)` precedent at `freed_for_test` (:399) precisely because of the external-crate visibility constraint — the asymmetry is correct, not a smell.
- **R28-I1+I2 LAND at `d00f12dd`** — T5 + clock_resync now race under `futures::join!`; the half-dead-agent fingerprint surfaces via a typed `transport_error: bool` on both outcomes. 5 new tests pin the contract. **One latent smell in the typed-wrapper classifier — see R29-M1 below.**
- **STARTUP-HEREDOC-LEAK CLOSED at `a3cfca10`** — three backtick pairs in the `zsbx-ctl.service` heredoc escaped (`\`...\``). Verified the entire 511-616 heredoc block for remaining shell-special chars — no further `$(`, backtick, or unescaped `$VAR` that isn't an intended expansion (see "Heredoc cleanliness" below).
- **No new production unwrap()/expect() this cycle.** Verified `wake_machine.rs` (6 unwrap_or_* sites, zero bare), `restore_handler.rs` (all `.unwrap()` are inside `#[cfg(test)] mod unit_tests`, `mod real_backend_tests`, or `mod r12_i1_tests`).

## Carry table

| Finding | r28 state | r29 state |
| --- | --- | --- |
| **R28-C1** CreateGuard::drop delayed-release leak | OPEN CRITICAL (concurrency-r28) | **CLOSED at `9e1f6276`** (inline sleep + rustdoc + regression test) |
| **R27-API2** `_test_inject_sandbox` cfg gate | OPEN IMPORTANT (api-surface-r27) | **CLOSED at `c2e07b2f`** (`test-support` feature) |
| **R28-I1** T5 + clock_resync serial latency | OPEN IMPORTANT (concurrency-r28) | **CLOSED at `d00f12dd`** (futures::join!) |
| **R28-I2** half-dead-agent string-match fragility | OPEN IMPORTANT (concurrency-r28) | **PARTIALLY CLOSED at `d00f12dd`** — typed bool surfaces at the wake_machine boundary; classifier still string-matches `clock_resync_post_restore`'s error message (R29-M1) |
| **R28-M1** spawn_blocking panic-payload `{p:?}` → `Any { .. }` | OPEN | OPEN, **count grew from 5 → 6** (new site at `restore_handler.rs:3032`) |
| **R28-M2** `restore_handler.rs:3163` silent `unwrap_or(0)` on clock | OPEN | OPEN, unchanged |
| **R28-M3** `unwrap_or_default()` on `r.into_string()` | OPEN | OPEN, unchanged |
| **R27-M1** r27-S1 host parse userinfo rustdoc | OPEN | OPEN, unchanged (cosmetic) |
| **R27-M3** `is_char_boundary` decrement loop, N=2 | OPEN | OPEN, unchanged — extract trigger still N=3 |
| **R27-M4** `HYPHEN_POSITIONS.contains(&off)` linear scan | OPEN | OPEN, unchanged (cosmetic) |
| **R27-M5** `strip_filesystem_paths` ordering comment | OPEN | OPEN, unchanged (cosmetic) |

## MINOR (new this round)

### [R29-M1] `ClockResyncOutcome` classifier string-matches the underlying error message — same anti-pattern its rustdoc claims to fix

**File**: `crates/sandbox/src/restore_handler.rs:3083-3097` (`clock_resync_post_restore_typed`).

```rust
match clock_resync_post_restore(agent_url, sandbox_id, signing_key_bytes).await {
    Ok(()) => ClockResyncOutcome::Ok,
    Err(message) => {
        // The underlying function's only "transport" branch wraps
        // its message with `format!("/_clock_resync transport: {e}")`
        // (see the ureq::Error fall-through above). A panic in
        // spawn_blocking surfaces as `clock_resync spawn_blocking
        // panic: …`, which is neither transport-layer nor a
        // routine non-200 — classify it as non-transport so the
        // half-dead-agent detector doesn't fire on a runtime bug.
        let transport_error =
            message.starts_with("/_clock_resync transport:");
        ClockResyncOutcome::Err { transport_error, message }
    }
}
```

**Why a smell**: The `ClockResyncOutcome` rustdoc at `:3035-3048` correctly argues that *"Without the structured boolean the caller would have to `contains(\"transport\")` on the free-text error message — fragile and not a contract. This enum makes the signal load-bearing."* The wrapper then **does that exact string-prefix match** at `:3091-3092`. The brittleness moved one level deeper, not away.

Concrete risk: `clock_resync_post_restore` (`:3009-3029`) has four `Err(...)` formatting sites — `status {status}: ...`, `status {code}: ...`, `transport: {e}`, and `clock_resync spawn_blocking panic: ...`. If a future contributor refactors any of these (e.g., changes `/_clock_resync transport:` to `/_clock_resync transport_error:` for symmetry with the new bool), the half-dead-agent detector silently breaks — every transport failure would classify as `transport_error=false`, and `parallel_asymmetric_failure_does_not_trip_half_dead_agent` test wouldn't catch it (that test only asserts the AND-gate, not that closed-port DOES set the bool true on `clock_resync`).

The protective test is `clock_resync_typed_surfaces_transport_error_on_closed_port` at `:3833`. That DOES catch a `starts_with` prefix change. So the regression net exists. But the right shape would be:

```rust
// in clock_resync_post_restore — return Result<(), ClockResyncError>
pub(crate) enum ClockResyncError {
    Transport(String),
    Status { code: u16, body_excerpt: String },
    Panic(String),
    NonceGen(String),
    ChallengeGen(String),
}
// then clock_resync_post_restore_typed just matches the enum variant.
```

**Severity**: MINOR. The test net catches a `starts_with` regression. The smell is the inconsistency between the rustdoc's argument ("don't string-match") and the implementation (string-match). Either move the classification into `clock_resync_post_restore` itself (return a structured error enum) or weaken the rustdoc's argument to "the wake_machine doesn't string-match — that's good enough."

---

### [R29-M2] Half-dead-agent rollback path has an unreachable `Ok` match arm

**File**: `crates/sandbox/src/wake_machine.rs:556-561`.

```rust
let clock_message = match &clock_resync_outcome {
    crate::restore_handler::ClockResyncOutcome::Err {
        message, ..
    } => message.clone(),
    crate::restore_handler::ClockResyncOutcome::Ok => String::new(),
};
```

This match is reached ONLY when `clock_resync_transport_error == true` (line :547 gates the block), which in turn requires `ClockResyncOutcome::Err { transport_error: true, .. }`. The `Ok` arm is structurally unreachable inside this branch.

**Why a smell**: A reader scanning the code sees a 5-line match where 2 lines are dead. `let-else` makes the invariant explicit:

```rust
let crate::restore_handler::ClockResyncOutcome::Err { message, .. } = &clock_resync_outcome
else {
    unreachable!("clock_resync_transport_error == true implies Err variant");
};
let clock_message = message.clone();
```

…or just don't include `clock_message` at all and dump the diagnostic from the typed enum at log-time via `?clock_resync_outcome` in the tracing macro.

A related concern: the rollback message at `:567-571` only carries `clock_message`, never the T5 transport diagnostic. T5 has its own inner spawn_blocking-formatted error (e.g., `"/version transport: connection refused"`) which gets dropped. Half the half-dead-agent forensic content is lost on the way to the database. Adding `t5_message` symmetrically would tighten RCA — but that's R28-M3 territory (the `Skipped` variant doesn't currently carry the underlying transport-layer error string, only `reason: &'static str`).

**Severity**: MINOR. The dead arm is defensive coding, not a bug. The lost T5 message is a forensic gap that pairs with R28-M3.

---

### [R29-M3] R28-M1 carry GROWS: 6 sites now print `Any { .. }` on spawn_blocking panic

**Files** (added one this cycle):
- `wake_machine.rs:364, :402-404, :454` — unchanged from r28
- `restore_handler.rs:919, :1026, :1080, :3289` — unchanged from r28
- `restore_handler.rs:3032` — **NEW** (`clock_resync spawn_blocking panic: {p:?}` introduced by the original `clock_resync_post_restore` shape, surfaced here because R28-M1 audit pulls forward in r29)
- `snapshot_handler.rs:366, :377, :403` — pre-existing, not previously catalogued
- `persist.rs:666, :685, :705, :721` — pre-existing, not previously catalogued
- `backend/nomad_ch.rs:880` — unchanged from r28

Same shape across all 14 sites: `format!("…spawn_blocking panic: {p:?}")`. `compio::runtime::JoinHandle::Err` is `Box<dyn Any + Send>` whose `Debug` impl emits the literal string `Any { .. }`. The std-lib panic payload (a `&'static str` or `String` from `panic!(...)`) is one downcast away:

```rust
.unwrap_or_else(|p| {
    let msg = p.downcast_ref::<&'static str>().copied()
        .or_else(|| p.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<non-string panic payload>");
    Err(format!("…spawn_blocking panic: {msg}"))
})
```

A shared `crate::compio_util::panic_msg(p: Box<dyn Any + Send>) -> String` helper dedupes 14 sites.

**Severity**: MINOR. The branches never fire in practice today, but every wake-failure observability blob that DOES route through one of these lines becomes literally unrecoverable (no panic message, no location, no payload type). Closing pays off the first time a spawn_blocking body ever panics.

---

### [R29-M4] T5 transport-fail tests construct `format!("http://{}", ...)` via a block-expression argument — awkward shape

**File**: `crates/sandbox/src/restore_handler.rs:4473-4486`.

```rust
let outcome2 = verify_agent_version_post_restore(
    &format!(
        "http://{}",
        {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let a = l.local_addr().unwrap();
            drop(l);
            a
        }
    ),
    ...
);
```

A 5-line block inside `format!`'s positional argument is hard to scan. The idiomatic shape — used at `:4477` of the SAME test file by the spawn_fake_agent helper — is:

```rust
let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
let addr = listener.local_addr().unwrap();
drop(listener);
let outcome2 = verify_agent_version_post_restore(&format!("http://{addr}"), ...);
```

Same number of lines (or fewer with implicit-capture format string), but the block boundary lines up with statement boundaries.

**Severity**: MINOR. Test code, no behavior impact. Three other tests in the same file (`half_dead_agent_fingerprint_detected_when_both_probes_transport_fail` at `:4751-4754`, `parallel_asymmetric_failure_does_not_trip_half_dead_agent` at `:4810-4813`) use the conventional shape — the new outcome2 fragment is the outlier.

---

### [R29-M5] R28-M2 carry, sharpened: silent `unwrap_or(0)` on `SystemTime::now()` also lives at `restore_handler.rs:2972-2975`

**File**: `crates/sandbox/src/restore_handler.rs:2972-2975` (`clock_resync_post_restore`, inside spawn_blocking).

```rust
let ts = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or(0);
```

r28 cited the identical pattern at `restore_handler.rs:3163` (T5 `/version` probe). The same critique applies twice now — `nomad_ch.rs:4117-4127`'s `unix_now()` panics with a 9-line rustdoc explaining *"the right behaviour for a clock that's gone backwards to before 1970 is to panic and let the orchestrator surface the problem; the silent-zero fallback hides catastrophic state."*

R29 lifts this from "noted once" to "noted twice in the same file." The `restore_handler.rs` stance vs `nomad_ch.rs` stance is now provably divergent across both clock-using sites (T5 and clock_resync). Either harmonize the policy across the crate (3 sites total) or annotate the divergence with a short rustdoc citing `unix_now`'s rationale and the availability-bias choice this file makes.

**Severity**: MINOR. Behavior is correct for both call sites — the agent's nonce-window rejection (`ts=0` → 1970 → 400/401) routes through the existing `Skipped/Err` paths with availability bias. The smell is asymmetric clock-policy discipline within one crate.

---

### [R29-M6] Comments inside the `<<EOF` heredoc block (`gcp-worker-startup.sh:511-616`) remain a footgun class even after a3cfca10's three escapes

**File**: `crates/sandbox/scripts/gcp-worker-startup.sh:511-616` (zsbx-ctl.service heredoc, unquoted).

After a3cfca10's escape of three backticks at L595, :597, :599, I audited the full 105-line heredoc body for remaining shell-special characters:

- `$VAR` references — all 12 sites are **intentional expansion** (`$ART`, `$DATACENTER`, `$VM_INDEX_CEIL`, `$AEAD_KEY_PATH`, `$ROOT_KEK_PATH`, `$SNAPSHOT_BUCKET`).
- `` `...` `` backticks — 3 sites at :560 and the 3 newly-fixed at :595/:597/:599. **All escaped.** Lint clean.
- `$(...)` — 2 sites at :586 and :592. Both are **intentional shell expansions** (`$( [ "$IS_MIGRATOR" = "1" ] && echo "Environment=..." )`) that emit a `Environment=` line conditionally.
- `{...}` braces — `{wake_id, poll_url}` at :549, `${...}` — none. Plain `{...}` is not shell-special outside `${}`. Safe.
- `!` (history expansion) — interactive-only; not active in scripts. Safe.

**However**: the *shape* of "unquoted heredoc with operator-prose comments" is a structural footgun. Any future contributor adding a rustdoc-style comment with backticks, dollar signs, or `$(cmd)` repeats the STARTUP-HEREDOC-LEAK. Three preventive options ordered by cost:

1. (Lowest cost) Add a shellcheck `# shellcheck disable=SC2016` block-comment at L510 with a note: *"DO NOT use backticks or `$(...)` in comments inside this unquoted heredoc; if you need a literal `$` or backtick, escape it as `\$` / `` \` ``."*
2. (Medium cost) Split the heredoc: emit the systemd unit header via `<<EOF` (needing expansion) and the comment-heavy body via `<<'EOF'` (no expansion) appended via `>>`. Loses some readability.
3. (Highest cost) Move the systemd unit body out of the startup script into a templated file under `crates/sandbox/scripts/zsbx-ctl.service.template` (per the existing reference at L500 — `stress/zsbx-ctl.service.template`) and `envsubst` the env vars. Best cleanliness, more diff churn.

a3cfca10's commit message correctly invokes shellcheck-clean (`lint.sh exit 0`) — but shellcheck does NOT flag `` ` `` inside heredocs as a problem when the heredoc is unquoted (it considers backticks-inside-heredoc to be valid shell). The verification net is the e2e stress run, not the linter.

**Severity**: MINOR (no current bug, but the foot-gun shape persists). Strongly recommend option 1 — adding the preventive shellcheck comment at L510. Single-line, zero behavior change.

## Cleanliness verification

### Production `unwrap()` / `expect()` since r28 baseline

- `wake_machine.rs`: 6 sites — all `unwrap_or_else` / `unwrap_or(0)` / `unwrap_or(false)`. Zero bare `.unwrap()` / `.expect()`. Same as r28.
- `restore_handler.rs`: 65 occurrences of `unwrap`-class — manually verified each site is inside one of `#[cfg(test)] mod unit_tests` (`:1439`), `mod real_backend_tests` (`:3384`), or `mod r12_i1_tests` (`:4863`). Zero in production paths.
- `backend/nomad_ch.rs:4125`: pre-existing `.expect("system clock before UNIX_EPOCH")` in `unix_now()` documented at `:4117-4121`. Unchanged.

Production unwrap discipline holds.

### R28-C1 inline sleep — correctness verification

The fix at `nomad_ch.rs:2266-2316` is correctly scoped:

1. **Lifetime correctness**: `cleanup_future` now runs `sleep(release_delay).await; vm_index_allocator.lock().release(i)` inline. The short-lived runtime minted by `detach_isolated` runs `block_on(cleanup_future)` and cannot tear down until `cleanup_future` returns Ready. The release is guaranteed to execute before runtime drop.

2. **`spawn_delayed_release` unchanged**: The helper at `:367-394` still uses `compio::runtime::spawn(...).detach()`, which is correct for `stop_inner`'s long-lived runtime caller at the line referenced by the rustdoc (`:1324`). The bug was caller-side, not helper-side. The fix preserves that.

3. **Lock-poison recovery**: `unwrap_or_else(|p| p.into_inner())` matches the file's 4 existing sites. Consistent.

4. **Regression test coverage**: `create_guard_drop_releases_vm_index_under_isolated_runtime` at `:4619-4678` polls up to 5 s for the index reuse, with `release_delay = 100ms` — the load-bearing detail (existing tests used `Duration::ZERO`). The commit message confirms the test fails on revert.

5. **Rustdoc adequacy**: 27 lines (`:2274-2301`) explaining the compio 0.11 `Scheduler::clear` semantics, citing both call sites, and warning against unification. Meets the r29 directive in full.

### R28-I1 `futures::join!` — cancel-safety & arity

`futures::join!` at `wake_machine.rs:529-530`:
- Both futures take `&` borrows that outlive the `join!` await point (`agent_url: &str`, `signing_key_bytes: &[u8; 32]`).
- Each wraps a `compio::runtime::spawn_blocking` over a ureq call. spawn_blocking work runs on an OS thread independent of the await context — even outer-task cancellation cannot tear them mid-call.
- No shared mutable state, no Drop-side effects.

The parallel-arity test `parallel_t5_and_clock_resync_both_succeed_against_healthy_agent` at `:4691-4744` asserts `calls.load(...) == 2` — confirming both HTTP calls fired. The fake-agent helper at `:3806-3836` uses `Connection: close` so each request is a fresh `accept()`. Method-distinguishing via `req.starts_with(b"GET")` at `:4720` is sound.

Sound.

### R28-I2 half-dead-agent typed signal — shape definition

`VersionCheckOutcome::Skipped { reason, transport_error }` (`:3156-3159`) and `ClockResyncOutcome::Err { transport_error, message }` (`:3061-3064`) are well-defined:

- 6 sites in `verify_agent_version_post_restore` (`:3242, :3300, :3315, :3328, :3346, :3362`) set `transport_error: false` for sentinel / parse / non-200 paths, and `:3300` sets `true` for the transport-error fall-through.
- `ClockResyncOutcome` classification (`:3091-3092`) uses `starts_with("/_clock_resync transport:")` — see R29-M1 for the smell.
- `VersionCheckOutcome::is_transport_error()` accessor (`:3174-3182`) matches `Skipped { transport_error: true, .. }`. 7 callers updated; existing T5 tests now assert on the `transport_error` field per outcome class (lines `:4356, :4395, :4430, :4464, :4514, :4543`). New tests pin both directions of the AND-gate: `parallel_asymmetric_failure_does_not_trip_half_dead_agent` (`:4779-4831`) is the critical guard.

Test coverage is comprehensive. The contract is well-defined at the wake_machine boundary; R29-M1 is the one structural gap.

### R27-API2 `test-support` feature — idiom verification

`crates/sandbox/Cargo.toml:71-83`:

```toml
[features]
test-support = []

[dev-dependencies]
zeroship-sandbox = { path = ".", features = ["test-support"] }
```

The self-dev-dep-with-feature pattern is the canonical Rust idiom for exposing test-only helpers to integration tests in the `tests/` directory:
- `tokio` uses it (`test-util` feature)
- `axum` uses it (`__private_docs` feature)
- `sqlx` uses it (`migrate` feature gated similarly)

Confirmed via `cargo build -p zeroship-sandbox` (production) vs `cargo build -p zeroship-sandbox --tests` (auto-enables `test-support` via self-dev-dep). The 6 integration-test callers of `_test_inject_sandbox` (in `tests/sandbox_pg_e2e.rs`, `sandbox_typed_id_e2e.rs`, `sandbox_preview_e2e.rs`, `sandbox_preview_ws_e2e.rs`, `sandbox_preview_share_e2e.rs`) all link as external crates and resolve the symbol via the feature.

The rustdoc at `nomad_ch.rs:1693-1700` correctly cites the `freed_for_test` precedent at `:399` (which uses bare `#[cfg(test)]`) AND explains the asymmetric choice — `_test_inject_sandbox` needs the additional `feature = "test-support"` arm because of external-crate visibility from the `tests/` directory, where `cfg(test)` does not apply. Asymmetry is intentional and correct.

Idiomatic. Production-grade.

### Heredoc cleanliness (a3cfca10)

Audit of `gcp-worker-startup.sh:511-616` (zsbx-ctl.service `<<EOF` heredoc, unquoted):

| Char class | Sites | Status |
|---|---|---|
| `$VAR` (intended expansion) | 12 | Correct |
| `` `cmd` `` (command substitution) | 6 | All 6 escaped (`\``) — :560 (pre-existing), :595, :597, :599 (new from a3cfca10), plus 2 in the L560 wrapper-comment |
| `$(cmd)` (command substitution) | 2 | Both intentional shell expansions at :586, :592 |
| `${VAR}` (parameter expansion) | 0 | Safe |
| `!` (history expansion) | 0 | Safe (non-interactive anyway) |

Other heredocs in the script:
- `:216` — quoted `<<'EOF'` — no expansion, safe by construction.
- `:287` — `<<EOF` body is plain sysctl lines, no specials.
- `:305` — `<<EOF` correctly escapes `\$(seq...)`, `\$idx`, `\$tap`, `\$host_ip`.
- `:321` — `<<EOF` body has only `$ART`-equivalents (intended).
- `:352` — `<<EOF` body uses `$DATACENTER`, `$PRIVATE_IP`, `$RETRY_JOIN` (all intended).
- `:439`, `:445` — `<<EOF` token/db env files, `$SANDBOX_TOKEN` / `$PG_PASSWORD` (intended).

**No further unescaped shell-special chars.** a3cfca10 is complete. The structural foot-gun shape persists — see R29-M6 for the preventive recommendation.

## Bottom line

r29 lands clean on R28-C1 (the critical concurrency fix), R27-API2 (the api-surface gate), R28-I1+I2 (the parallelization + structured half-dead-agent signal), and STARTUP-HEREDOC-LEAK (the heredoc escape).

- **Zero critical findings.** Zero important findings.
- **R29-M1** (typed-wrapper still string-matches the underlying error) is the most actionable new smell — the wrapper's rustdoc and implementation contradict each other. Fix-shape: move the classification into `clock_resync_post_restore` itself via a structured `ClockResyncError` enum.
- **R29-M2** is a 5-line dead-arm + a forensic gap on the T5 message path; cosmetic / lossy.
- **R29-M3** is R28-M1 sharpened — same shape, but the count grew this cycle (5 → 6 production sites; 14 if `snapshot_handler.rs` + `persist.rs` are counted in audit scope).
- **R29-M4** is test-code style only.
- **R29-M5** strengthens R28-M2 — the silent-zero clock pattern lives at TWO sites in `restore_handler.rs` (T5 + clock_resync), provably divergent from `nomad_ch.rs`'s panic-with-rustdoc stance.
- **R29-M6** documents the heredoc-comment foot-gun shape; preventive shellcheck comment recommended.

**Code-quality lens reads HEAD `ce062846` as production-ready** with the three biggest structural carries from r27/r28 now closed (R27-I1, R27-API2, R28-C1, R28-I1, R28-I2). Production unwrap/expect discipline holds (zero new bare unwraps; the 14 audited `{p:?}`-on-Any sites are pre-existing). Send/Sync invariants on R26-C1 thread-local cache still hold (no touch this cycle). BackendBuilder ergonomics still absorbing fields cleanly.

The R28-I2 typed-boolean contract is the cycle's most interesting design move — it correctly identifies that string-matching is brittle and introduces a structured signal at the wake_machine boundary. R29-M1 is the one place the signal-propagation chain stops short: the classifier itself string-matches one level deeper. Closing R29-M1 (push the classification into `clock_resync_post_restore` via a structured error enum) would complete the contract end-to-end.

Next round's likely surface: continued T5 hardening if stress-r9-retry-2 surfaces version-skew false-positives; the R28-M1 / R29-M3 panic-payload fix is a low-yield bulk cleanup whenever a contributor has a quiet hour.
