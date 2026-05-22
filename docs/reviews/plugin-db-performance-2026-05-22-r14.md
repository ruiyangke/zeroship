# plugin-db Performance Review — 2026-05-22 r14

Commit: HEAD (`baa262c1`). Cycle baseline: r13 (`0bf71f27`, 82 / 100).
Mode: forcing-function check, bench re-run at HEAD, regression scan
of cycle 15:17 + 15:47 + 16:17 commits.

**Forcing function MISSED this cycle.** r13 named the V8-side
`JSON.parse` bench (`bench_v8_json_parse` or equivalent in
`crates/runtime/benches/`) as the gating item for sizing the [C3]
redesign. No such bench landed. The cycle 15:17 + 15:47 + 16:17
commits are correctness/observability/test work on the migration
audit pipeline — none touch the read path or the V8 boundary.

---

## 1. Environment

- CPU: Intel(R) Xeon(R) CPU @ 2.80GHz (32 cores via `nproc`)
- Kernel: Linux 6.12.80
- Loadavg at run start: `1.59 / 1.42 / 1.08` (post-run: `2.54 / 1.91 /
  1.29`). Quieter than r13's 4.10/1.82/1.40 — sub-µs points are
  cleaner this cycle.
- Criterion: defaults (100 samples × 3 s measurement, 1 s warm-up).
  No `--quick`, no sample-size override.

---

## 2. Commits since r13

| Commit     | Surface | Hot path? | Notes |
| ---------- | ------- | --------- | ----- |
| `14d7608f` | `plugin-db/validate.rs` | No (cold-start orchestrator) | F2 terminalisation of destructive-op Pending rows. Migration audit pipeline only. |
| `6afab751` | `plugin-db/audit.rs` + `validate.rs` | **Cold-start only** | F2 ValidationRefused terminal + idempotent `DROP CONSTRAINT IF EXISTS / ADD CONSTRAINT` on `ensure_audit_table_exists`. Adds **2 DDL round-trips per cold start per app** (lines 275-290). Not per request. See §4.1. |
| `02ead3f4` | `plugin-db/audit.rs` | No (error message) | Error UX: spells the allowed `app_id` alphabet in the error. String-format change in the rejection path. Never reached on valid traffic. |
| `d07616a2` | `plugin-db/validate.rs` | No (cold-start) | F1 warn-shape drift + `update_audit_status` terminal-list completion. Validation pipeline. |
| `7506bd73` | `plugin-db/tests/integration.rs` | No | +167 LOC integration test for the F2 CHECK ALTER upgrade path. Tests only. Zero production bytes. |
| `cbd21112` | `plugin-db/apply.rs` line 84 | **No (error path)** | F1 warn-shape unification on `write_audit_row` secondary-failure (the `Err(audit_err) =>` arm). Adds 3 structured tracing fields (`app_id`, `collection`, `transition`) on the *failed-to-write-audit* branch. Cold path — never reached when audit insert succeeds. |
| `6e54ebb9`, `44ec83db`, `baa262c1` | `docs/reviews/` | No | Reviewer reports + critique docs. Docs-only. |

Of these, **zero** edit a request-time hot path. One (`6afab751`)
adds two DDL round-trips per cold start per app — addressed in §4.1.

---

## 3. Bench results at HEAD

### 3.1 `bench_query_build` (no-regression check)

Command: `cargo bench -p zeroship-plugin-db --bench bench_query_build`

| Bench                     | r14 mean   | r13 mean   | Δ vs r13   | criterion verdict (vs saved baseline) |
| ------------------------- | ---------- | ---------- | ---------- | -------------------------------------- |
| `build_find/empty`        |   505.50 ns |  494.70 ns | +2.2 %     | "Performance has regressed." (+2.4%)   |
| `build_find/small`        |   982.28 ns | 1042.10 ns | −5.7 %     | "Performance has improved." (−4.9%)    |
| `build_find/complex`      |  3052.50 ns | 2976.30 ns | +2.6 %     | "No change in performance detected."   |
| `build_insert/small_doc`  |  1906.50 ns | 1898.00 ns | +0.4 %     | "No change in performance detected."   |

All movement is within criterion's historical jitter band for these
shapes. Nothing in the 7 plugin-db-touching commits since r13 plausibly
affects SQL construction — `build_find` and `build_insert` only walk
the query builder, which none of the commits touched.

### 3.2 `bench_row_to_json` (the [I35] gauge)

Command: `cargo bench -p zeroship-plugin-db --bench bench_row_to_json`

| Bench                       | r14 mean (95% CI)         | r13 mean    | Δ vs r13 | criterion verdict |
| --------------------------- | ------------------------- | ----------- | -------- | ------------------ |
| `row_to_json/narrow_3cols`  | 216.43 – **216.69** – 217.02 ns | 230.76 ns | −6.1 %   | "Performance has improved." |
| `row_to_json/medium_10cols` | 1.2963 – **1.3010** – 1.3057 µs | 1.4676 µs | −11.4 %  | "Performance has improved." |
| `row_to_json/wide_50cols`   | 7.8950 – **7.9381** – 7.9986 µs | 7.7403 µs | +2.6 %   | "Performance has regressed." |

The narrow / medium improvements are the mirror of r13's loadavg-class
"regressions" (r13 ran at loadavg 4.10; r14 at 1.59). Same code, same
machine, quieter host — sub-µs points pop up. Wide row at HEAD is
7.94 µs vs r13's 7.74 µs; +200 ns on a path that has not changed.
**No commit since r13 touches `row_to_json`'s call graph.** I read the
diff for each: `audit.rs`, `apply.rs`, `validate.rs`, and `integration.rs`
are the only changed files, none on the read path. The +2.6 % is
machine-jitter, not regression.

The [I35] floor still reproduces in the brief's ~7.7 µs band.

### 3.3 `bench_first_row_or_null` (the [C3] gauge)

Command: `cargo bench -p zeroship-plugin-db --bench bench_first_row_or_null`

| Bench                                 | r14 mean (95% CI)         | r13 mean    | Δ vs r13 | criterion verdict |
| ------------------------------------- | ------------------------- | ----------- | -------- | ------------------ |
| `first_row_or_null/narrow_3cols`      | 561.71 – **563.74** – 565.90 ns | 556.28 ns | +1.3 %   | "Change within noise threshold." |
| `first_row_or_null/medium_10cols`     | 2.4099 – **2.4151** – 2.4216 µs | 2.4918 µs | −3.1 %   | "Performance has improved." |
| `first_row_or_null/wide_50cols`       | 12.018 – **12.057** – 12.098 µs | 11.979 µs | +0.7 %   | "No change in performance detected." |

Wide point reproduces almost exactly: 12.06 µs at r14 vs 11.98 µs at
r13 (Δ +80 ns on a 12 µs path). The brief's "11.66 µs at 50-col" sits
~3 % below this run's number, same band as r13.

### 3.4 Rust-side residual at wide rows (the brief's decomposition)

```
12.06 µs (first_row_or_null/wide_50cols, measured r14)
= 7.94 µs (row_to_json/wide_50cols, measured r14)
+ 4.12 µs (residual: rows_to_json_value Vec construction
           + into_iter().next().unwrap_or(Null)
           + .to_string())
```

r13 derived 4.24 µs for the same slice. r14: 4.12 µs. The two derived
numbers sit within their own combined CI bands. **The Rust-side tail
is ~4.1 µs.** The V8 `JSON.parse` half remains unmeasured at this
crate boundary — same status as r13.

---

## 4. Cycle 15:17 / 15:47 / 16:17 regression scan (brief item)

### 4.1 `6afab751` — `audit.rs` CHECK ALTER (cold-start)

The commit adds two new DDL statements to `ensure_audit_table_exists`
(`crates/plugin-db/src/audit.rs:275-290`):

```sql
ALTER TABLE "..."."__zeroship_migrations"
  DROP CONSTRAINT IF EXISTS __zeroship_migrations_status_chk;

ALTER TABLE "..."."__zeroship_migrations"
  ADD CONSTRAINT __zeroship_migrations_status_chk CHECK (
    status IN (...8 values...)
  );
```

These run **once per cold start per app** (the function is called
inside the cold-start orchestrator under the advisory lock, before
the app is published — `audit.rs:196-203` docstring). They do not run
per request, per query, or per migration apply.

**Per-cold-start cost**: unmeasured. Two extra `query_text_params`
round-trips through `compio-postgres`. On a same-host postgres these
are typically sub-millisecond each; for a remote DB across a LAN they
are RTT-bound (~0.2-0.5 ms). The benches in this report do not
exercise this path (`compio-postgres` `test-utils` mocks Rows but not
the connection / DDL surface).

**Verdict: not a request-time regression.** The cost is paid once,
amortised across the lifetime of the worker's hold on that
app's isolate. For an app handling thousands of requests after cold
start, the marginal per-request cost is rounding error. Worth noting
in the cold-start budget but not actionable as a perf regression.

If a measurement is wanted later, the natural place is the existing
`ensure_audit_table_exists` integration test in
`crates/plugin-db/tests/integration.rs` (lines added by `7506bd73`) —
wrap the call in `std::time::Instant` and emit the duration; the test
already provisions a real DB. r14 does not run that.

### 4.2 `cbd21112` — `apply.rs:84` F1 warn-shape unification

Diff: 13 added / 2 removed lines, all inside the `Err(audit_err) =>`
arm of the `write_audit_row` call at apply.rs line 65. Reached only
when audit insert fails — a cold error path. Adds three structured
tracing fields (`app_id`, `collection`, `transition`); on the success
path (the `Ok(id) =>` arm) the code is unchanged.

Per-warn cost is irrelevant by the brief's own framing (the path is
cold). Per-request cost is **zero** — the success arm wasn't touched.

**Verdict: not a regression.** Confirmed by reading apply.rs:60-98.

### 4.3 `7506bd73` — integration test only

`crates/plugin-db/tests/integration.rs` added 167 LOC. Pure test code.
Lib build is unchanged. Confirmed by `git show --stat 7506bd73`
showing one file in the `tests/` directory.

**Verdict: zero production-path bytes.**

### 4.4 `d07616a2`, `02ead3f4`, `14d7608f` — validate.rs / audit.rs UX + terminalisation

All three sit on the cold-start migration orchestrator pipeline. None
edit the read or write request-time path. `d07616a2` adjusts the
`update_audit_status` terminal list and tracing field shape — same
warn-half family as `cbd21112`, same error-path locality. `02ead3f4`
edits an error message string. `14d7608f` terminalises Pending rows
on destructive ops — also cold-start.

**Verdict: none are request-time regressions.**

---

## 5. Score

**82 / 100.** (r13: 82. Delta: **0.**)

### Why 0

r13 explicitly predicted this. From r13 §7: "if it lands without the
§7-item-1 bench, the next review can only confirm 'no production
regression' not 'win as designed.'" That is the exact posture of r14:

- **No perf-relevant commit landed.** The cycle 15:17 + 15:47 + 16:17
  surface is migration-pipeline correctness (F1 warn-shape, F2
  ValidationRefused terminal, status CHECK widening) and tests. All
  off the request hot path.
- **No regression introduced.** §3 shows the four bench harnesses
  reproduce within jitter; §4 shows the three potentially-touching
  commits land on cold-start / error paths.
- **No progress on the r13 forcing function.** `bench_v8_json_parse`
  did not land. The V8-side half of the [C3] decomposition is still
  conjecture.
- **No progress on [N9-I2] / [N9-I3] hot-path work.** Still on disk.

A flat score is the correct call. r13's +1 paid for the *capability*
to size [C3]. Nothing in r14 either uses that capability further or
introduces work that could be sized against it. Holding 82 reflects
"we know what to do, we haven't done it yet."

### What would have moved the score

- **+2 to +4**: a `bench_v8_json_parse` landing under
  `crates/runtime/benches/` with three shapes matching the
  `bench_row_to_json` width tiers. Would complete the [C3]
  decomposition and let r15 quote a real redesign target.
- **+3 to +5**: a [N9-I2] or [N9-I3] commit landing, ideally with a
  before/after bench delta in the commit message. Either would be a
  *measured* microsecond shaved off a request path — the first since
  [I35].
- **−2 to −5**: a regression on `build_find` or `row_to_json` traceable
  to one of the migration-pipeline commits. Confirmed absent.
- **−3 or worse**: a request-path edit shipped without a bench
  delta. None occurred this cycle.

---

## 6. Next-cycle forcing function (still V8-side)

Same as r13 §7, item 1, unchanged:

**`bench_v8_json_parse`** (or `bench_resolve_value_json`) lands
somewhere reachable — `crates/runtime/benches/` is the natural home —
driving a minimal V8 isolate and measuring `JSON.parse` of a string
produced by `serde_json::Value::to_string()` at the three width tiers
(narrow 3-col, medium 10-col, wide 50-col) the existing plugin-db
benches use.

Until this lands, r15 will face the same posture as r14: it can
confirm or refute regressions, and it can validate the Rust-side
4.1 µs tail, but it cannot size the [C3] redesign's win. Five
consecutive reviews (r10 → r14) have now flagged this gap. r13 named
it the explicit forcing function; r14 confirms it remains the
forcing function. The [C3] design work appearing in the design-loop
critique docs (`docs/reviews/cycle 16:17 rounds 3-4`) is the
right shape of motion, but a code-side measurement is what changes
the score.

Secondary options (unchanged from r13 §7):

2. [N9-I2] commit (`$in` Vec<String> allocation) — benchable via
   `build_find/complex`. Measurable today.
3. [N9-I3] commit (migrations Vec → String → V8 round-trip) — same V8
   boundary as [C3]; should not land without (1) above or in lockstep
   with [C3].
4. Direct [C3] implementation. Possible without (1) but the next
   review can only certify "no production regression," not "win as
   designed."

---

## 7. Anti-fabrication compliance

- All ns / µs figures in §3 are criterion `time:` lines from runs I
  performed in this session at HEAD `baa262c1`. Three full
  bench-target runs (`bench_query_build`, `bench_row_to_json`,
  `bench_first_row_or_null`), criterion defaults (100 samples × 3 s
  measurement). No `--quick`, no sample-size override.
- The "4.12 µs Rust-side residual" figure in §3.4 is the difference
  of two measured criterion means at the same shape at HEAD:
  `first_row_or_null/wide_50cols` 12.057 µs minus
  `row_to_json/wide_50cols` 7.9381 µs = 4.12 µs. Derived, flagged.
- The "Δ vs r13" columns in §3 are arithmetic on r13's reported
  means (which I re-read from `docs/reviews/plugin-db-performance-
  2026-05-22-r13.md` §3) and r14's means from this session.
- The "two DDL round-trips" claim in §4.1 is grounded in a direct
  `Read` of `crates/plugin-db/src/audit.rs:275-290` this session.
  The per-cold-start cost is explicitly marked unmeasured.
- The "0-line change to success arm" claim in §4.2 is grounded in
  `git show cbd21112` showing the diff confined to the
  `Err(audit_err) =>` block at apply.rs:83-95, and a `Read` of
  apply.rs:70-98 confirming the success arm is unchanged.
- The "+0 score delta" in §5 is qualitative reasoning, not a
  measurement.
- The "5 consecutive reviews flagging the V8 gap" claim in §6 is the
  natural span r10..r14 — verifiable by reading the prior reports
  but not re-verified here for compactness.

**Reproducibility footer.** All bench numbers reproducible at HEAD
`baa262c1` via:

```text
cargo bench -p zeroship-plugin-db --bench bench_query_build
cargo bench -p zeroship-plugin-db --bench bench_row_to_json
cargo bench -p zeroship-plugin-db --bench bench_first_row_or_null
```

Loadavg sensitivity per r13 §3 holds; expect ±5 % on sub-µs points
when contended. Wide-row points (>10 µs) are stable to ~1 %.
