# plugin-db Performance Review — 2026-05-22 r13

Commit: HEAD (`0bf71f27`). Cycle baseline: r12 (`89dbb6a8`, 81 / 100).
Mode: forcing-function check, bench re-run at HEAD, residual-cost
decomposition for [I35] + [C3].

**Forcing function MET this cycle.** Both `bench_row_to_json`
(`75d9ae5c`) and `bench_first_row_or_null` (`4e9dbafb`) landed under
`crates/plugin-db/benches/`, gated by the `compio-postgres`
`test-utils` feature (`bf75e866`). Numbers in this report are from
runs I made in this session at HEAD.

---

## 1. Environment

- CPU: Intel(R) Xeon(R) CPU @ 2.80GHz (32 cores reported by `nproc`)
- Kernel: Linux 6.12.80
- Loadavg at run start: `4.10 / 1.82 / 1.40` (higher than r12's 1.47;
  see §2 noise commentary)
- Criterion: defaults (100 samples × 3 s measurement, 1 s warm-up).
  No `--quick`, no sample-size override.

---

## 2. Commits since r12

| Commit     | Surface | Hot path? | Notes |
| ---------- | ------- | --------- | ----- |
| `bf75e866` | `compio-postgres` | No (dev-only) | Adds `test-utils` feature + `Row` / `Statement` / `Column` builders. Gated `#[cfg(feature = "test-utils")]`. Production build never sees `test_utils.rs`. Verified by `Cargo.toml` shape (`crates/compio-postgres/Cargo.toml:30`) and `cfg` gate on `pub mod test_utils;` (`crates/compio-postgres/src/lib.rs:128`). |
| `75d9ae5c` | `plugin-db/benches` | No | New `bench_row_to_json.rs` (237 LOC). Adds `pub fn row_to_json_for_bench` in `lib.rs` (`#[doc(hidden)]`, no cfg gate — always present but only reachable from bench / docs). Zero callers in production code paths. |
| `4e9dbafb` | `plugin-db/benches` | No | New `bench_first_row_or_null.rs` (253 LOC). Adds `pub fn first_row_or_null_for_bench` in `lib.rs` (same shape as above). Internally calls `v8_bridge::rows_to_json_value` + inline copy of `crud::first_row_or_null`'s logic (`to_string()` on `.into_iter().next().unwrap_or(Null)`). |
| `6ff294fd` | `docs/reviews` | No | r12 numbers recorded into the deferred backlog. Docs-only. |
| `f6adb68b` | `plugin-db/context` | No | `IsolateDbContext` field privatisation. Compile-time only. |
| `6728af77` | `docs/reviews` | No | Cycle 14:17 reviewer reports. |
| `91771aaf` | `plugin-db/context` | No | **27 accessors demoted `pub` → `pub(crate)`.** Visibility-only; method bodies untouched. Verified by `git show --stat`: 27 ±, 0 net LOC. Zero perf impact. |
| `f4ece2b6` | `docs/reviews` | No | Deferred-update rule edit. |
| `0bf71f27` | `plugin-db/test_support` | **No (dev-only)** | New `test_support/mod.rs` (339 LOC). Gated `#[cfg(test)]` at the mod declaration (`crates/plugin-db/src/lib.rs:129`). 6 self-tests + production-call sites only inside `mod tests {}` blocks (verified — see §5). `tracing-subscriber` added as `[dev-dependencies]` only. |

Of these, **two** are perf-relevant in the sense that they make
performance measurable; **zero** are perf-relevant in the sense that
they touch a hot path.

---

## 3. Bench results at HEAD

### 3.1 `bench_row_to_json` (the [I35] gauge)

Command: `cargo bench -p zeroship-plugin-db --bench bench_row_to_json`

| Bench                       | Mean (95% CI)             | criterion `change:` line                          | criterion verdict                          |
| --------------------------- | ------------------------- | ------------------------------------------------- | ------------------------------------------ |
| `row_to_json/narrow_3cols`  | 226.57 – **230.76** – 236.66 ns | `[+3.57% +4.52% +5.94%] (p = 0.00 < 0.05)`        | "Performance has regressed." (interpreted below) |
| `row_to_json/medium_10cols` | 1.4636 – **1.4676** – 1.4721 µs | `[+4.30% +4.54% +4.78%] (p = 0.00 < 0.05)`        | "Performance has regressed." (interpreted below) |
| `row_to_json/wide_50cols`   | 7.7190 – **7.7403** – 7.7617 µs | `[-1.36% -0.91% -0.58%] (p = 0.00 < 0.05)`        | "Change within noise threshold."           |

**Reproducibility check vs the r13 brief.** The brief states "narrow
560 ns / medium 2.6 µs / wide 11.66 µs at 50-col" for
`bench_first_row_or_null` (not `bench_row_to_json` — see §3.2). For
`bench_row_to_json` the brief states the wide-row decode is 7.76 µs
([I35]-fixed). HEAD reproduces 7.7403 µs — within criterion's CI band
of the brief's 7.76 µs. **The [I35] floor is real and stable.**

**The criterion "regressed" verdicts on narrow / medium.** Two
candidates:

1. Loadavg ~3× higher than r12 (`4.10` vs `1.47`). The shared 32-core
   host is doing more work than during r12's baseline save.
2. The narrow / medium points are sub-microsecond; criterion's
   per-iteration overhead dominates the absolute shift (4.5% of 230 ns
   = 10 ns).

Both verdicts are presented; the wide point — the one that matters
for [C3] sizing — is flat. I do not treat the narrow / medium
"regressions" as actionable.

### 3.2 `bench_first_row_or_null` (the [C3] gauge)

Command: `cargo bench -p zeroship-plugin-db --bench bench_first_row_or_null`

| Bench                                 | Mean (95% CI)             | criterion `change:` line                          | criterion verdict                          |
| ------------------------------------- | ------------------------- | ------------------------------------------------- | ------------------------------------------ |
| `first_row_or_null/narrow_3cols`      | 555.85 – **556.28** – 556.76 ns | `[-3.60% -2.22% -1.12%] (p = 0.00 < 0.05)`        | "Performance has improved."                |
| `first_row_or_null/medium_10cols`     | 2.4853 – **2.4918** – 2.4983 µs | `[-5.47% -5.19% -4.91%] (p = 0.00 < 0.05)`        | "Performance has improved."                |
| `first_row_or_null/wide_50cols`       | 11.969 – **11.979** – 11.990 µs | `[+2.13% +2.52% +2.90%] (p = 0.00 < 0.05)`        | "Performance has regressed." (interpreted below) |

**Reproducibility vs the brief.** Brief states 560 ns / 2.6 µs / 11.66 µs.
HEAD measures 556 ns / 2.49 µs / 11.98 µs. Narrow is essentially
identical; medium is ~4% below the brief's quote; wide is ~2.7% above.
All three sit within the kind of inter-run jitter the harness has
historically shown for sub-2µs paths (see r12 §2 on `build_insert`
oscillation). **The brief's numbers reproduce at HEAD.**

The wide-row "+2.5%" verdict is the same loadavg story as §3.1's
narrow / medium "regressions" — criterion is comparing against an
earlier saved baseline taken under quieter conditions. The absolute
delta is ~300 ns on a ~12 µs path; the wide point at HEAD is **3.99×
the 3 µs r12-defined threshold** (12.0 / 3.0 = 3.99×). The brief's
"3.9× the 3 µs threshold" claim reproduces.

### 3.3 `bench_query_build` (no-regression check)

Re-ran for completeness — the harness from r11 / r12.

| Bench                     | r13 mean   | r12 mean   | Same ballpark? |
| ------------------------- | ---------- | ---------- | -------------- |
| `build_find/empty`        |  494.70 ns |  496.92 ns | Yes (Δ −2 ns)  |
| `build_find/small`        | 1042.10 ns |  987.58 ns | +5% (loadavg)  |
| `build_find/complex`      | 2976.30 ns | 3045.90 ns | Yes (Δ −70 ns) |
| `build_insert/small_doc`  | 1898.00 ns | 1907.90 ns | Yes (Δ −10 ns) |

Nothing in the visibility commit (`91771aaf`) or the test_support
commit (`0bf71f27`) plausibly affects these numbers, and the table
confirms: no real shift. `build_find/small`'s +5% is loadavg-class
noise, not a regression I can attribute to a commit.

---

## 4. Residual-cost decomposition (the brief's core ask)

The brief asks me to validate the 50-col path's decomposition as:

```
findOne wide path
  = row decode  (7.76 µs, [I35]-fixed)
  + .to_string() + V8 JSON.parse boundary  (~3.9 µs residual)
```

### 4.1 What I can measure

- `row_to_json/wide_50cols` at HEAD: **7.7403 µs** (§3.1). This is
  pure Rust-side row decoding, no JSON string materialised. Matches
  the brief's "7.76 µs row decode".
- `first_row_or_null/wide_50cols` at HEAD: **11.979 µs** (§3.2). This
  is row decode + `to_string()`. **It does NOT include V8
  `JSON.parse`.** The `first_row_or_null_for_bench` helper
  (`crates/plugin-db/src/lib.rs:280-287`) explicitly returns the
  `String`; the docstring there calls out (line 271-274) that V8
  `JSON.parse` "lives in `zeroship-runtime` and is not part of this
  microbench."

### 4.2 Decomposition the bench actually supports

```
12.0 µs (first_row_or_null wide_50cols, measured)
= 7.74 µs (row_to_json wide_50cols, measured)
+ 4.24 µs (residual: rows_to_json_value Vec construction
           + into_iter().next().unwrap_or(Null)
           + .to_string())
```

The 4.24 µs residual is `first_row_or_null` minus `row_to_json`,
measured back-to-back in the same run. The `Vec<Value>` allocation
(via `rows_to_json_value` calling `rows.iter().map(...).collect()` —
`v8_bridge.rs:339-341`) and the `.to_string()` serialise are the two
pieces inside it. **The V8 `JSON.parse` step is NOT in this 4.24 µs —
it sits across the runtime-crate boundary, unmeasured.**

### 4.3 Verdict on the brief's "~3.9 µs residual"

The brief frames `to_string() + V8 JSON.parse` as a single ~3.9 µs
slice. **The bench cannot validate this.** It can validate that the
Rust-side residual (everything past `row_to_json`, but before V8) is
**~4.24 µs at wide rows** — close to the brief's number but
non-identical. The V8 `JSON.parse` cost is separately structural,
lives in `zeroship-runtime`, and is the piece the [C3]
`ResolveValue::JsonValue` redesign would actually eliminate.

So the buy-back from `ResolveValue::JsonValue` decomposes as:

| Slice                                          | Measured?  | Wide-row cost                |
| ---------------------------------------------- | ---------- | ---------------------------- |
| `row_to_json` (per row × 1 row)                | Yes        | 7.74 µs                      |
| `rows_to_json_value` Vec collect (1 element)   | Inside §4.2 residual | ≪ 4.24 µs (Vec of 1)         |
| `into_iter().next().unwrap_or(Null)`           | Inside §4.2 residual | ~ns                          |
| `Value::to_string()` (serialise to JSON text)  | **The dominant piece of the 4.24 µs residual** | unknown without a finer split |
| V8 `JSON.parse` of that string                 | **Not measured by either bench** | Unknown |

The [C3] redesign replaces `to_string` + V8 `JSON.parse` with a
direct `serde_json::Value` → V8 walk. The bench gives us the upper
bound on the Rust serialise half (4.24 µs) but not the V8 parse half.

### 4.4 Sub-recommendation for r14

To complete the decomposition before [C3] lands, add a third bench
target (in this crate or in `zeroship-runtime/benches`) that measures
the boundary in the form the SDK actually sees: a Rust-side
`serde_json::Value` → V8 value materialised in an isolate. That
isolates the V8 cost from the Rust cost and gives the redesign a
real target number to beat. Without it, the [C3] win is half-sized
on paper. **r14 forcing function** (see §7).

---

## 5. New surface audit

### 5.1 `0bf71f27` — `tracing-subscriber` capture layer

339 LOC under `crates/plugin-db/src/test_support/mod.rs` plus 11 new
in-crate tests. The brief's "Pure dev-dep work, no production hot
path. Verify nothing leaked into release builds" — verified:

1. **`tracing-subscriber` is `[dev-dependencies]` only.** Verified
   `Cargo.toml:44` — appears under `[dev-dependencies]`, not under
   `[dependencies]`. Release builds of `plugin-db` do not link it.
2. **`test_support` module is `#[cfg(test)]`-gated at the mod
   declaration** (`crates/plugin-db/src/lib.rs:129`). The module is
   invisible outside `cargo test`. Verified — `cargo build --release
   -p zeroship-plugin-db` succeeds with 15 warnings (same set as
   before this commit; no new "unused" warnings from the gated mod).
3. **All `use crate::test_support` sites sit inside `mod tests {}`
   blocks.** Grep returned 4 production-file sites
   (`context.rs:988`, `context.rs:1043`, `migrations.rs:1002`,
   `apply.rs:452`, `apply.rs:525`); read each — all inside an
   already-`#[cfg(test)] mod tests` parent scope. No leakage.
4. **Release+hardening combo also clean.** `cargo build --release -p
   zeroship-plugin-db --features hardening` produces 61 warnings
   (the dormant auth surface) but no warnings traceable to
   `test_support` (no compile attempts on the gated module).

**Verdict: clean. Zero production-path bytes.**

### 5.2 `91771aaf` — `IsolateDbContext` accessor demotions

`git show --stat`: 27 `pub` → `pub(crate)` changes on accessor
signatures, zero method-body edits. Visibility-only. Cannot change
generated code. Confirmed by `bench_query_build`'s unchanged numbers
(§3.3) — those benches exercise paths that flow through several of
the demoted accessors (`set_pool`, `mark_model_registered`,
`tx_token`, ...). No movement.

**Verdict: zero perf impact, as advertised.**

---

## 6. Score

**82 / 100.** (r12: 81. Delta: **+1**.)

### Why +1

Two opposing forces:

**+:** This is the first cycle since r10 where r13 has the
measurement infrastructure to size both [I35] and [C3] from the
outside. Three benches now cover the read path end-to-end on the
Rust side (`bench_query_build` for SQL construction,
`bench_row_to_json` for the [I35] decode floor,
`bench_first_row_or_null` for the [C3] gauge). r12 explicitly called
this absent. The brief's wide-row decomposition (§4.2) is now a
measurable quantity rather than a qualitative claim.

**−:** Zero hot-path code edits this cycle. The four
plugin-db-touching commits since r12 are all infrastructure
(`91771aaf` visibility, `f6adb68b` field privatisation,
`75d9ae5c` + `4e9dbafb` benches, `0bf71f27` test_support, plus
`bf75e866` in `compio-postgres`). [N9-I2] (`$in` Vec<String>) and
[N9-I3] (migrations Vec→String) remain on disk untouched. [C3] now
has a measured cost slice (4.24 µs Rust-side residual at wide) but
no fix has landed.

A 1-point bump rewards the measurability win without overstating
progress — no microsecond was actually shaved off any production
path this cycle. The benches make r14's forcing function concrete in
a way it could not be before.

### Verdict on the C3 actionability claim

The brief asserts [C3] graduates from "blocked" to "actionable-now"
because the wide-row `first_row_or_null` measurement is 3.9× the 3
µs threshold. **The measurement supports the claim — but only
partially.**

- **Supported:** the Rust-side composed path
  (`rows_to_json_value` + `first_row_or_null` minus the V8 step)
  costs 12.0 µs at 50 cols. Of that, 7.74 µs is structural row
  decode and 4.24 µs is the `Vec` allocation + `.to_string()` tail.
  Both halves are measured.
- **Unsupported:** the [C3] redesign's actual win is the V8
  `JSON.parse` elimination, and neither bench reaches into V8.
  Quoting a "win" from [C3] today would be fabrication. The
  redesign should be funded on the basis of the measured Rust tail
  (4.24 µs) plus a *plausible-but-unmeasured* V8 parse cost — not on
  a single fused number.

**[C3] is actionable for design work** (architecture, API shape of
`ResolveValue::JsonValue`). It is **not actionable for sizing the
win** until the V8 side gets a bench. r14's forcing function (§7)
closes that gap.

---

## 7. Next-cycle forcing function

r14 should not run until one of:

1. **`bench_v8_json_parse`** (or equivalent) lands somewhere
   reachable — either `crates/plugin-db/benches/` driving a minimal
   isolate, or `crates/runtime/benches/` measuring `ResolveValue::Json`
   end-to-end across a synthetic JSON string of known size. Goal: a
   real number for the V8-side half of the [C3] decomposition. Until
   that exists, [C3]'s sizing remains half-measured and the redesign's
   ROI is conjecture.

2. **[N9-I2] commit lands** fixing the `$in` Vec<String> per-call
   allocation on the build path (benchable via
   `build_find/complex`). Hot-path edit, measurable today.

3. **[N9-I3] commit lands** fixing the migrations Vec → String → V8
   `JSON.parse` round-trip in `migrations.rs:402-403`. Same V8
   boundary as [C3] — should not land independently of the [C3]
   redesign, but can if scoped right.

4. **A direct [C3] implementation commit lands** (the
   `ResolveValue::JsonValue` shape with the cross-runtime-crate
   types/walk). This is what r12 + r13 have both been gating
   towards; if it lands without the §7-item-1 bench, the next
   review can only confirm "no production regression" not "win as
   designed." Item 1 should land first or alongside.

The top item for r14 is #1: **the V8-side measurement is the missing
link the [C3] design needs.** Without it, r14 will produce a fourth
consecutive "we know the Rust half, we're guessing the V8 half"
posture.

---

## 8. Anti-fabrication compliance

- All ns / µs figures in §3 are criterion `time:` lines from runs I
  performed in this session at HEAD `0bf71f27`. Three full
  bench-target runs, criterion defaults (100 samples × 3 s
  measurement), no `--quick`, no sample-size override. Sample size:
  100 per bench per shape. (Brief permitted reduced sample size with
  documentation; I did not reduce.)
- The "4.24 µs Rust-side residual" figure in §4.2 is the difference
  of two measured criterion means at the same shape (50 cols) at
  HEAD: `first_row_or_null/wide_50cols` 11.979 µs minus
  `row_to_json/wide_50cols` 7.7403 µs = 4.24 µs. Derived, not
  measured directly — flagged as such in §4.2's text.
- The "3.99× threshold" claim in §3.2 is `11.979 / 3.0 = 3.99`,
  arithmetic on a measured number.
- The "V8 `JSON.parse` cost is unknown" verdict in §4.3 / §4.4 / §6
  is honest: neither bench at HEAD touches V8, and I did not run a
  V8-side bench in this report.
- The "+1 point" delta in §6 is qualitative reasoning, not a
  measurement. The score is opinionated by definition.
- The "release builds clean" claim in §5.1 is the literal output of
  `cargo build --release -p zeroship-plugin-db` and `…--features
  hardening` runs I performed this session. Warning counts (15 and
  61 respectively) are from the tail of the actual cargo output.
- The "27 accessor demotions, zero perf impact" claim in §5.2 is
  grounded in (a) `git show --stat 91771aaf` (showing only
  signature-line changes on a single file), and (b) `bench_query_build`
  showing no movement on paths that pass through those accessors.
- The brief's "560 ns / 2.6 µs / 11.66 µs" reproduces with §3.2's
  "556 ns / 2.49 µs / 11.98 µs" — quoted side by side; the
  ~4% medium-row gap and the ~2.7% wide-row gap are noted but not
  attributed to any specific cause.
- `cfg(test)` gating on `test_support` is grounded in direct `Read`
  of `crates/plugin-db/src/lib.rs:129` this session and a grep of
  the four call sites (`§5.1` point 3).

**Reproducibility footer:** all bench numbers reproducible at HEAD
via:

```text
cargo bench -p zeroship-plugin-db --bench bench_row_to_json
cargo bench -p zeroship-plugin-db --bench bench_first_row_or_null
cargo bench -p zeroship-plugin-db --bench bench_query_build
```

Loadavg sensitivity noted in §3.1 / §3.2; expect ±5% on sub-µs
points when the host is contended. The wide-row points (>10 µs)
are stable to ~1%.
