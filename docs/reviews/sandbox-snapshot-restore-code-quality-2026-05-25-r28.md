# Sandbox snapshot-restore code-quality review — 2026-05-25 r28

**Reviewer**: code-quality r28 (cron-pilot)
**HEAD**: `568c1357`. **Prior**: r27 (`8def39e2`).
**Scope since r27**: 22 commits touching `crates/sandbox/` (R27-I1 BackendBuilder land, R27-M2 Latin-1 fix land, R26-API2 `/metrics` exporter, R26-C1 thread-local Rc<Pool> cache, R26-I2 spawn_blocking wrap, R4-S2 ErrorEnvelope panic-on-non-object, Option C Phase 2 staging-locality flag, T5 agent /version fingerprint check).

## Summary

- **6 findings**: 0 critical, 0 important, 6 minor.
- **R27-I1 CLOSED at `df06d172`** — `BackendBuilder` lives at `backend/mod.rs:200-258`. The three telescoping `from_config*` constructors are deleted in one PR per AGENTS.md pre-launch policy; 12 call sites migrated.
- **R27-M2 CLOSED at `821cc9bd`** — all 6 `bytes[i] as char` sites in `wake_machine.rs` now route through `utf8_char_len_at()` + `out.push_str(&msg[i..i+c_len])`. 5 new `sanitize_strips_*_preserves_unicode_*` tests + 1 round-trip-composition test pin the fix.
- **No new production unwrap()/expect() since r27.** The 6 added `unwrap()`s in the diff are all in `#[cfg(test)]` or `mod tests` blocks. The 4 added `expect("backend")` are all in test fixtures.
- **R27-M3/M4/M5 still open** — none touched. Watch-items, not actionable.

## Carry table

| Finding | r27 state | r28 state |
| --- | --- | --- |
| **R27-I1** Backend `from_config*` telescoping | OPEN, breakeven crossed | **CLOSED at `df06d172`** (BackendBuilder, 12 call sites migrated) |
| **R27-M1** r27-S1 host parse userinfo / IPv6 zone-id rustdoc | OPEN | OPEN (no movement; cosmetic) |
| **R27-M2** `bytes[i] as char` Latin-1 cast in 6 sites | OPEN LATENT | **CLOSED at `821cc9bd`** (utf8_char_len_at helper + tests) |
| **R27-M3** `is_char_boundary` decrement loop duplicated | OPEN, N=2 | OPEN, N=2 — extract trigger still N=3 |
| **R27-M4** `HYPHEN_POSITIONS.contains(&off)` linear scan | OPEN | OPEN (cosmetic) |
| **R27-M5** `strip_filesystem_paths` ordering comment | OPEN | OPEN (cosmetic) |
| **R26-A1** BackendFailureDetail trait | DEFERRED | DEFERRED |

## MINOR (this round)

### [R28-M1] `compio::spawn_blocking` panic-payload formatting loses information across 5 production sites

**Files**:
- `wake_machine.rs:364` — `format!("spawn_blocking panic: {p:?}")` (snapshot get)
- `wake_machine.rs:402-404` — same pattern (submit_restore_job)
- `wake_machine.rs:454` — same pattern (wait_for_livez)
- `restore_handler.rs:3192` — same pattern (/version probe)
- `backend/nomad_ch.rs:818` — same pattern (cold-boot disk-image staging, R26-I2 landing)

**What** `compio::runtime::spawn_blocking` returns `JoinHandle<T>` whose `Err` is the panic payload from `catch_unwind` — concretely `Box<dyn Any + Send>` (see `compio-runtime-0.11.0/src/runtime/mod.rs:209-220`). `{p:?}` on `Box<dyn Any>` produces the literal string `Any { .. }` (the trait object's blanket `Debug` impl shows no information about the payload).

**Why a problem** All 5 sites build a user-facing error string from this: e.g. `"spawn_blocking panic: Any { .. }"` lands in `wake_jobs.error_message`, in `WakeErrorCode::RestoreFailed` envelope, and in the SLO dashboard. When the spawn_blocking body panics, the operator sees an opaque marker with zero forensic content — no panic message, no location, no payload type.

**Standard pattern** Downcast to `&str` and `String`:
```rust
.unwrap_or_else(|p| {
    let msg = p.downcast_ref::<&'static str>().copied()
        .or_else(|| p.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<non-string panic payload>");
    Err(format!("spawn_blocking panic: {msg}"))
})
```
This matches what `tokio::task::JoinError::into_panic` users do at the std-lib boundary. A shared helper (`crate::compio_util::panic_msg(p)`) would dedupe the 5 sites; alternately a `fn join_or_io_err<T>(jh: JoinHandle<Result<T,E>>) -> Result<T,E>` wrapper.

**Severity**: MINOR. The branches are never-taken in practice today (spawn_blocking bodies are mature paths — fs syscalls + Nomad HTTP), but if one ever fires the diagnostic is unrecoverable. Closing this would convert "Any { .. }" wake-failure mysteries into legible RCA. Same fix-shape across 5 sites means a shared helper is the right shape.

---

### [R28-M2] `restore_handler.rs:3163` silent `unwrap_or(0)` on `SystemTime::now().duration_since(UNIX_EPOCH)` — inconsistent with `nomad_ch.rs:4125` documented-panic counterpart

**File**: `crates/sandbox/src/restore_handler.rs:3160-3163` (in `verify_agent_version_post_restore`'s spawn_blocking body).

```rust
let ts = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or(0);
```

**Compare**: `backend/nomad_ch.rs:4117-4127` (`unix_now`) has a 9-line rustdoc that explicitly chose `.expect("system clock before UNIX_EPOCH")` over the silent-zero fallback, citing *"the right behaviour for a clock that's gone backwards to before 1970 is to panic and let the orchestrator surface the problem; the silent-zero fallback hides catastrophic state"*.

**Concrete impact in T5 path**: the agent verifies the request signature with timestamp `ts=0` (which is 1970-01-01) and applies its nonce-window enforcement — it will reject the request with `400` / `401` (clock too skewed). That routes through `verify_agent_version_post_restore` as `non_200_response` → `Skipped`, *not* `Mismatch`. So the silent fallback degrades to "wake proceeds with WARN" instead of "wake fails", which is the right availability call BUT directly contradicts the file's own documented policy two crates over.

**Severity**: MINOR (the behavior is correct for this caller — the T5 probe is additive and `Skipped` is the right outcome on a clock catastrophe). The smell is the asymmetric stance: identical `SystemTime::now()` patterns in the same crate, one panics with a justifying rustdoc, the other silently substitutes 0 with no comment. Either harmonize on `.expect(...)` for both (consistent with `unix_now`) or document the deliberate divergence at `restore_handler.rs:3163`.

---

### [R28-M3] `unwrap_or_default()` on `r.into_string()` swallows body-read errors at `restore_handler.rs:3181 / :3185`

**File**: `crates/sandbox/src/restore_handler.rs:3181, 3185` (T5 /version response handling inside spawn_blocking):

```rust
Ok(r) => {
    let status = r.status();
    let body = r.into_string().unwrap_or_default();
    Ok((status, body))
}
Err(ureq::Error::Status(code, r)) => {
    let body = r.into_string().unwrap_or_default();
    Ok((code, body))
}
```

**Why a smell** `ureq::Response::into_string()` fails on (1) body larger than ~10 MB default cap, (2) transport read error mid-body, (3) non-UTF-8 bytes. All three render as empty `body` post-`unwrap_or_default()`. The downstream JSON-parse at `:3220` then fails with "EOF while parsing" → `body_not_json` → `Skipped`. The body-read failure type is lost.

**Severity**: MINOR. Same availability bias as R28-M2 (Skipped > Fail), and `/version` is a tiny JSON object so body-too-large is implausible. But the `unwrap_or_default()` collapses three distinct failure modes into a single `body_not_json` reason; logging the underlying `r.into_string()` error would tighten the diagnostic. Could route to a 4th `Skipped { reason: "body_read_error" }` variant. Low yield.

---

### [R28-M4] R27-M3 carry: `is_char_boundary` decrement loop duplicated verbatim — still N=2 (extract at N=3)

**Sites**:
- `wake_machine.rs:862-864`:
  ```rust
  let mut end = ERROR_MESSAGE_MAX_BYTES;
  while end > 0 && !s.is_char_boundary(end) {
      end -= 1;
  }
  ```
- `backend/nomad_ch.rs:2985-2988`:
  ```rust
  let mut end = 2048;
  while end > 0 && !trimmed.is_char_boundary(end) {
      end -= 1;
  }
  ```

Identical 4-line decrement loop, different cap constants. N still 2; r27 said "trigger to extract at N=3". The `nomad_ch.rs:2977` comment already references the wake_machine.rs counterpart, confirming the awareness — not an oversight.

**Recommendation**: no action this round. Watch-item. A pubished `fn truncate_at_char_boundary(s: &str, max: usize) -> &str` in `crate::util` would absorb the third use cleanly when it arrives.

---

### [R28-M5] R27-M4 carry: `HYPHEN_POSITIONS.contains(&off)` linear scan — unchanged

**File**: `wake_machine.rs:1337-1340`:
```rust
const HYPHEN_POSITIONS: [usize; 4] = [8, 13, 18, 23];
for off in 0..UUID_LEN {
    let b = s[i + off];
    let is_hyphen_slot = HYPHEN_POSITIONS.contains(&off);
```

`matches!(off, 8 | 13 | 18 | 23)` is 1 LOC, drops the const, lets the compiler emit a jump-table. Pure clarity. No movement r27 → r28.

---

### [R28-M6] R27-M5 carry: `strip_filesystem_paths` ordering comment still self-contradictory

**File**: `wake_machine.rs:1175-1181`:
```rust
// r27-M1: order matters — `/var/lib/zeroship/` must come BEFORE
// `/var/zeroship/` is even ATTEMPTED, otherwise a substring
// match on `/var/zeroship/` would never run (it doesn't share a
// prefix with `/var/lib/zeroship/`, so order is actually safe
// either way here, but we keep the longest-prefix-first
// discipline so a future operator-prefix addition that DOES
// share a prefix lands correctly).
```

"order matters" + "actually safe either way" in the same comment. Intent is correct (preserve discipline) but the lede is wrong. Unchanged r27 → r28.

## Cleanliness verification

### Production `unwrap()` / `expect()` since `add6d5ef` baseline

Manually grep'd `wake_machine.rs`, `restore_handler.rs`, `backend/nomad_ch.rs`, `db.rs`, `error_envelope.rs`, `metrics_export.rs`, `admin_handlers.rs`:

- `wake_machine.rs` — **0 production `.unwrap()` / `.expect()`**. (R28-M1 wraps panic payloads via `unwrap_or_else(|p| Err(...))` — not an unwrap.)
- `restore_handler.rs` — `.unwrap_or(0)` at :3163 (R28-M2), `.unwrap_or_default()` at :3181 / :3185 (R28-M3). No unconditional `.unwrap()` / `.expect()` in production paths.
- `backend/nomad_ch.rs:4125` — `.expect("system clock before UNIX_EPOCH")` in `unix_now()` is pre-existing (blame `c3b14556b`, May 1) and documented (line 4117-4121).
- `error_envelope.rs:84` — `panic!("ErrorEnvelope::with_extra requires a JSON object; got: {extra:?}")` is INTENTIONAL (R4-S2 design: loud-fail on contract violation, see commit `425a5522` rationale).

### Compio Send/Sync discipline (R26-C1 thread-local Rc<Pool>)

`db.rs:51-58` declares two `thread_local!` `RefCell<Option<(String, Rc<Pool>)>>` cells. `Pool` is `!Send + !Sync` (per its own `Rc<TcpStream>` + `RefCell` internals — confirmed via the cited `compio-postgres/src/pool.rs:14-17,228`). The cache shape is forced — `Arc<OnceLock<Pool>>` would not compile.

Race handling at `install_pool` (`db.rs:74-90`): post-await re-check via `RefCell::borrow_mut().get_or_insert_with`-equivalent. Compio is single-threaded per worker, so the only race is intra-thread await-point interleaving. Local-build drops on race-loser path. **Logic is sound.**

One micro-smell: `cached_pool` borrows the `RefCell` immutably, returns `Some(Rc::clone(pool))`, drop. `install_pool` then re-borrows mutably. The `with(|c| ...)` closure boundaries enforce single-borrow scope. **No double-borrow risk.**

### BackendBuilder ownership (R27-I1 land verification)

`backend/mod.rs:202-258`: `BackendBuilder<'a>` holds `cfg: &'a SandboxConfig`, owns `persist: Option<Arc<Persistence>>` and `local_nomad_node_id: Option<String>`. `build()` consumes self (moves out via destructuring at :235-239), clones `cfg` once per backend variant (forced — the inner backends need owned configs). The `Arc::clone` semantics on `persist` are correct: cloned into each variant constructor.

`#[must_use]` is set (`:200`); `#[allow(missing_debug_implementations)]` is present at `:201` with a comment citing the `Persistence`-has-no-Debug constraint. Both are correct.

### r28-specific risk: T5 sentinel-handling correctness

`verify_agent_version_post_restore` (`restore_handler.rs:3135-3273`) is the new wake-failure path landed this cycle. Reviewed for soundness:

- **Sentinel #1** (controller `git_commit == "unknown" || is_empty()`) short-circuits to `Skipped` **before** any HTTP call. Test `verify_agent_version_skipped_when_controller_sha_unknown` pins `calls == 0`.
- **Sentinel #2** (agent `got_git_commit == "unknown"`) routes to `Skipped` AFTER the HTTP call. Acceptable — the agent had to be reachable to produce the response.
- **Transport error** → `Skipped` (availability bias).
- **Non-200** → `Skipped` (availability bias, but see R28-M2: with `ts=0` from `unwrap_or(0)` on clock-rewind, the agent will return 400/401 here).
- **Body not JSON** → `Skipped` (R28-M3 collapses 3 failure modes).
- **Body JSON, no `git_commit` field** → `Skipped` (legacy agent acceptance).
- **Match** → wake proceeds. **Mismatch** → wake rolls back with `WakeErrorCode::AgentVersionMismatch`.

Wake-machine call-site (`wake_machine.rs:483-538`) maps the three outcomes correctly: `Match` → debug log, `Skipped` → warn log + proceed, `Mismatch` → `rollback_with`. The `t5_agent_version_mismatch_maps_to_distinct_wire_code` test (`:1442`) pins the wire code is distinct from `LivezTimeout`, `RestoreFailed`, and `Internal`.

**Sound.**

## Bottom line

r28 lands clean on R27-I1, R27-M2, and the heavier R26-C1 / R26-API2 / R26-I2 / R4-S2 / T5 / Option C Phase 2 work.

- **Zero critical findings.** Zero important findings.
- **R28-M1** (spawn_blocking panic-payload formatting) is the most actionable carry — uniform fix across 5 sites, converts opaque `Any { .. }` markers into legible diagnostics. Suggested shape: shared `crate::compio_util::panic_msg(p)` helper.
- **R28-M2** is a documentation/discipline asymmetry — same `SystemTime::now()` pattern handled two ways in two files; harmonize or comment.
- **R28-M3** is body-read error squelching in the T5 path — defensible but lossy.
- **R27-M3/M4/M5 carry unchanged.** All cosmetic; M3's extract trigger remains N=3.

**Code-quality lens reads HEAD `568c1357` as production-ready.** The structural code-smell carries from r26-r27 (R26-I1, R27-I1) are now both closed. No new structural debt accumulated this cycle. The Option C Phase 2 staging-locality flag + T5 fingerprint check are landed cleanly with comprehensive test coverage; both are additive and reversible.

Production `unwrap()` / `expect()` discipline holds. Send/Sync invariants on the new R26-C1 thread-local cache hold. BackendBuilder ergonomics absorb the next 2-3 orthogonal extension fields without further refactor.

Next round's likely surface: r28-A1 (architecture lens — Option C Phase 4 cluster-stress flip readiness), continued T5 hardening if cluster-smoke surfaces version-skew false-positives.
