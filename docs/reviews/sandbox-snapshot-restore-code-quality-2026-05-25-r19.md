# Sandbox snapshot-restore code-quality review — 2026-05-25 r19

**Reviewer**: code-quality-r19 (cron-pilot)
**HEAD**: `b8654600`
**Prior round**: r18 (HEAD `370d13e6`)
**Lens**: code-quality
**Scope**: C-7-LT-2-PR1 (`40811d8b` nomad_ch.rs +364/-77 probe rewrite) + C-7-LT-2-PR2 (`bfff5acc` metrics.rs +93 LOC + nomad_ch.rs leak counter wires) + R17-S1 (`3c75a8ce` wake_machine.rs sanitizer +77/-5) + R18-I1 (`531db5c3` test-fixture hardening +84) + visibility tightening (`f27062c0` pub→pub(crate)).

## Summary

- **5 findings**: 0 critical, 1 important, 4 minor.
- `cargo test -p zeroship-sandbox --lib --release`: **414 pass / 1 ignored** (+12 over r18's 402, exactly matching the claim).
- `cargo build -p zeroship-sandbox --tests`: **2 warnings, unchanged from r18** (unused `SandboxAuth` import; unused `WAKE_JOBS_T_KEEP` const). **No new warnings introduced by PR1/PR2/R17-S1.**
- C-7-LT-2-PR1 is a clean structural fix: `compio::net::TcpStream::connect` + `compio::time::timeout` replaces ureq's request-deadline timeout that wedged on a SYN-blackhole TAP. The three pinned-contract tests (`pr1_probe_reachable_port_returns_true`, `pr1_probe_refused_port_returns_false_fast`, `pr1_probe_unroutable_address_returns_false_within_timeout`) lock in the 150 ms hard cap, the <50 ms ECONNREFUSED lane, and the <750 ms unroutable lane. The `pr1_parse_agent_probe_addr_accepts_expected_shapes` test pins canonical + scheme-less + empty + scheme-only inputs.
- C-7-LT-2-PR2 wires `inc_vm_index_leak("host_fence_timeout" | "wait_failed")` at both `stop_inner` leak paths and adds a defensive unknown-label fallback that folds to `host_fence_timeout` with a WARN log — operator-safe.
- R17-S1 sanitizer growth (`match_rfc1918_at` +29 LOC) handles the 169.254/16 + 100.64/10 cases inline. The function is still legible but is starting to drift toward a CIDR table — see [R19-M1].

## CRITICAL

None.

## IMPORTANT

### [R19-I1] `probe_agent_reachable_tcp` discards the `TcpStream` immediately on success — relies on `Drop` for a clean FIN; on RST-prone agents this can synthesise a half-open connection observed by the agent

- **File**: `crates/sandbox/src/backend/nomad_ch.rs:3428-3438`
- **Snippet**:
  ```rust
  match compio::time::timeout(connect_timeout, connect_fut).await {
      Ok(Ok(_stream)) => true, // SYN ACKed → socket alive
      Ok(Err(_)) => false,
      Err(_) => false,
  }
  ```
- **Issue**: the connect succeeds → we return `true` and the `_stream` falls out of scope. compio's `TcpStream::drop` issues a shutdown via io_uring CLOSE, which the kernel ultimately translates to a FIN (or RST if the send buffer is non-empty — not our case since we never wrote). In aggregate this is fine. But during teardown the agent's HTTP listener is racing its own close path; a flood of half-opened-then-closed sockets at 100 ms cadence could (a) bias the agent's epoll wakeup timing, (b) get classified as `time_wait` by the host kernel and accumulate. At ≤300 probes per fence (30 s × 10 Hz) on a typical TAP this is well below `net.ipv4.tcp_max_tw_buckets` (default 65536) but worth pinning a comment so a future cadence bump (or a long-fence operator override) doesn't silently exhaust the tw pool.
- **Suggested fix**: add a comment block to `probe_agent_reachable_tcp` documenting the implicit drop-as-shutdown contract + the per-probe tw-bucket cost; consider `_stream.shutdown().await` for an explicit FIN before drop (currently no-op on success-only path but makes the contract grep-able). NOT a behaviour bug — purely a docs/operator-contract refinement. Deferrable.
- **Why now**: PR1 is the first instance of *aggressive*, io_uring-native TCP probing in this codebase. Establishing the operator contract once (here) is cheaper than tracking down a tw-bucket alert later.

## MINOR

### [R19-M1] `match_rfc1918_at` parser duplication post-R17-S1 — 100.64/10 + 172.16/12 share a near-identical "parse second octet, range-check" block; refactor to a CIDR table is now justified

- **File**: `crates/sandbox/src/wake_machine.rs:751-833`
- **Issue**: post-R17-S1 the function has two structurally-identical "consume 1-3 digits, parse as u32, range-check, advance past `.`" blocks (172.16/12 second-octet at L782-800, 100.64/10 second-octet at L764-781). The 169.254/16 and 10/8 + 192.168/16 cases use static `starts_with` only. The duplication is 18 lines × 2; a `&[(&[u8], RangeInclusive<u32>)]` table-driven extractor would collapse both into ~10 LOC and make the next IANA-reserved addition (e.g. RFC 6890 special-purpose blocks if the threat model expands) a one-line table entry.
- **State**: not a bug; not blocking. The current shape is grep-able (each prefix's logic is co-located) and unit-tested. Refactor opportunity for a future round where the sanitizer grows further.
- **Suggested fix**: deferred. If/when a fourth dynamic-second-octet block lands, extract `match_prefix_with_octet_range(&[u8], RangeInclusive<u32>, octets_after: usize) -> usize` and call it from the 172/100 branches.

### [R19-M2] R17-Q1 still present (doc inflation in `from_host_fence_timeout`) AND r19-A4 says the doc's premise is factually wrong — extract to an ADR

- **File**: `crates/sandbox/src/restore_handler.rs:184-287` (~103 lines, unchanged since r18-M2)
- **Cross-lens reference**: r19 architecture review §r19-A4 finds the doc's "agent /shutdown → host-fence wait → Nomad job purge tail (~fence-shaped)" sequence describes the *wrong* pipeline. Actual order at `nomad_ch.rs:1037-1132` is Nomad-purge first, then fence; `wait_for_job_gone` is hard-coded to 30 s at L1051 (NOT fence-shaped). The "2× factor" the doc canonises is coincidence (30 + 30 = 60 at fence=30).
- **State of R17-Q1**: doc inflation now compounded by a factually-wrong premise that future operators will reverse-engineer. r18-M2 recommended ADR extraction; r19-A4 makes it nearly required.
- **Suggested fix**: either (a) prepend a `**NON-NORMATIVE / historical**` marker above the C-8a/C-8b/C-7-LT-1 narrative sections and tighten the rustdoc to the current contract only, OR (b) extract the whole commit-stamped narrative to `docs/decisions/2026-05-XX-c7-family-retry-policy.md` per r18-M2's suggestion. Either resolves both r18-M2's doc-inflation concern and r19-A4's factual-wrongness concern.
- **Why**: a docstring that codifies a wrong model wastes every future reader's first-pass time. Five rounds of commit-stamped accretion have made the cost real.

### [R19-M3] R17-Q2 doc-prose off-by-one still present (`36 attempts × 2 s = 70 s`)

- **File**: `crates/sandbox/src/restore_handler.rs:280-281`
- **Snippet**:
  ```text
  /// - fence=30, Async  → 2*30 + 10 = 70 s
  ///   → 36 attempts × 2 s = 70 s budget (envelopes smoke-r12's
  ```
- **Issue**: 36 × 2 = 72, not 70. The formula at L340 (`(effective_budget / INTERVAL_SECS).saturating_add(1)`) is correct — wall-time is `(attempts - 1) × interval` = 35 × 2 = 70 s. Only the doc prose is loose. Asserts in `c7_lt_1_async_mode_fence_30_yields_70s_budget` pin the correct `(attempts - 1) × interval` math.
- **State of R17-Q2**: unchanged since r18-M3. Trivial textual fix.
- **Suggested fix**: s/`36 attempts × 2 s = 70 s budget`/`36 attempts (35 sleeps × 2 s) = 70 s budget`/. Same loose prose appears at L284-285 (fence=20 Async, `26 attempts × 2 s`) and L286 (fence=120 Async, `126 attempts × 2 s`). All three lines have the same shape; one search-and-replace closes R17-Q2 entirely.

### [R19-M4] R17-Q3 still present (silent `WakeJobState::Failed` fallback at `db.rs:1588`)

- **File**: `crates/sandbox/src/db.rs:1588`
- **Snippet**:
  ```rust
  state: WakeJobState::from_str_opt(state_str).unwrap_or(WakeJobState::Failed),
  ```
- **State**: unchanged since r18-M4. Migration 0009 CHECK constraint makes this unreachable; the fallback is purely cosmetic. r17 recommended either `expect("CHECK constraint guarantees in-domain state")` or a coverage test for the fallback. Neither has landed.
- **Suggested fix**: same as r18-M4. Deferred — low risk, but the loud-panic version would be a clearer operator contract for "the DB shape diverged from the code's enum".

### [R19-M5] R18-I1 fixture-hardening pattern works but copies a 6-line `assert!(matches!(...))` block ten times — extract `insert_wake_job_fresh(&row)` helper

- **File**: `crates/sandbox/tests/sandbox_pg_e2e.rs:3692-3706` and 9 other sites (per `git show 531db5c3 --stat`)
- **Issue**: the assertion shape is small but the *site count* is now 10. A helper like:
  ```rust
  async fn insert_wake_job_fresh(db: &Database, row: &WakeJobRow) {
      assert!(
          matches!(
              db.insert_wake_job(row).await.expect("insert"),
              InsertWakeJobOutcome::Inserted
          ),
          "fresh insert must return Inserted"
      );
  }
  ```
  collapses 60 lines into 10 call sites + 9 lines of helper. PR3+ (the test suite is expected to grow) will benefit from the centralised assertion.
- **State**: deferred. R18-I1's core ask (assert `Inserted` outcome explicitly) is closed — see `563c1d19`. The DRY-it-up question is separable.
- **Suggested fix**: extract `insert_wake_job_fresh` next time a fresh-insert site is added; one new site is the breakeven point.

## Cross-lens consensus

- **PR1 is clean.** `compio::time::timeout(150ms, compio::net::TcpStream::connect(addr))` is the right io_uring-native primitive; the contract tests pin the 150 ms hard cap, the ECONNREFUSED clean-miss lane, and the unroutable-address lane (RFC 5737 TEST-NET-1). Zero unwraps in non-test code; the only `from_str` is gated on `if let Ok(addr) = …` with a hostname-resolve fallback. Idiomatic.
- **PR2's `inc_vm_index_leak` is correctly bounded.** Label set is closed (`host_fence_timeout` | `wait_failed`); the unknown-label arm folds-and-warns into `host_fence_timeout` rather than silently dropping (operator-safe). Backing storage is two `AtomicU64` constants — no `OnceLock`, no `lazy_static`, no race surface beyond `fetch_add` relaxed-ordering (the documented metric semantics). The Phase-3 exporter binding remains a pure-additive change.
- **R17-S1 is correct but starting to bloat.** `match_rfc1918_at` is still readable as a single function; if a sixth prefix lands, the function should table-ize. Documented in R19-M1.
- **Visibility tightening (`f27062c0`) is purely additive.** `detach_isolated`, `spawn_wake_jobs_gc`, and `WAKE_JOBS_*` are all crate-internal; the `pub(crate)` narrowing closes api-surface findings R17-API1 + R10-API1 without behaviour change. `_test_build_auth_from_sealed` is now `cfg(test)`-gated.
- **Saturating math discipline preserved.** No new `.unwrap()` / `.expect()` in production code (the `unwrap_or_else(|p| p.into_inner())` on the vm_index_allocator at `nomad_ch.rs:1136-1138` is the existing PoisonError recovery, not new).

## Lens hand-off — concurrency / architecture / api-surface

1. **Concurrency**: R19-I1 (TcpStream drop-as-shutdown contract under aggressive probing) is concurrency-adjacent — the time_wait bucket exhaustion path is a multi-probe-aggregate concern. Flagging for r19+.
2. **Architecture**: R19-M2 already cites r19-A4 — the architecture lens has the open ADR-extraction recommendation. r18-M2's "extract to `docs/decisions/`" is now structurally required given r19-A4's factual-wrongness finding.
3. **Api-surface**: PR1 adds `parse_agent_probe_addr` + `probe_agent_reachable_tcp` as private module functions (`fn`, not `pub`). Architecture-r19 §r19-A5 recommends extracting them into a shared `agent_probe.rs` for reuse by `wait_for_agent_livez`. No api-surface concern at private-function scope; api-surface lens to weigh in if extraction lands.
4. **Test coverage**: R18-M1 (deferred terminal-mid-race pin for `insert_wake_job`) unchanged. R19-M5 (helper extraction for fresh-insert fixture sites) is a DRY-not-correctness recommendation; test-cov can decide priority.
5. **Performance**: PR1's 150 ms connect-timeout + 100 ms cadence-subtract-elapsed pattern is documented at `nomad_ch.rs:3340-3352`. The wake_machine 2× pacing fix (probe latency subtracted from cadence) is structurally correct but performance-r19 should sanity-check that the ~5 ms io_uring overhead per probe doesn't compound at 300 probes/fence.
6. **No regressions**: lib tests 414/0/1, +12 over r18. Saturating-math discipline preserved; no new `.unwrap()` / `.expect()` in production code; no new `#[allow(dead_code)]`. Visibility tightening reduces api-surface (positive).
