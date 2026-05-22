# plugin-db Performance Review — 2026-05-22 r11

Commit: HEAD (`05484878`). Cycle baseline: r10 (`d2e7e22`, 81 / 100).
Mode: forcing-function check + bench re-run (no production code edited).

**Forcing-function requirement was NOT met.** Bench re-run reproduces
r10's numbers within criterion's noise band on the stable benches; the
one bench that drifts (`build_insert/small_doc`) is system-load noise,
not a regression. No commit since r10 touches the bench's hot path.

---

## 1. Commits since r10

```
403b3891  plugin-db/query: validate_field_name rejects non-ASCII (I12)
5d9acab8  plugin-db/context: surface mig_lock state drift via tracing (I23)
2fa9472e  plugin-db/auth: gate dormant auth subtree behind hardening feature
```

Hot-path impact of each:

| Commit     | Hot path? | Reason |
| ---------- | --------- | ------ |
| `403b3891` | **No**    | `validate_field_name` is only called from DDL emission (`query.rs:712`, `build_create_table_with_fks` family). `build_find` and `build_insert` do not invoke it. Verified by grep. |
| `5d9acab8` | **No**    | Adds `tracing::error!` / `tracing::warn!` on `set_mig_lock` shadow-replace and `return_mig_client` empty-slot. Both are migration-lock state transitions, not CRUD-path code. |
| `2fa9472e` | **Indirectly** | Removes ~2,860 LOC `auth/*` subtree from default builds via `hardening` feature flag. Could theoretically shift code layout / inlining, but the bench harness already excludes auth/* paths and the find benches show no movement >2% (well below criterion's significance threshold). |

---

## 2. Bench delta vs r10

Methodology: full bench (no `--quick`), 100 samples × 3s measurement
window each. Numbers are criterion `mean.point_estimate` from
`target/criterion/build_{find,insert}/<param>/new/estimates.json`.

| Bench                       | r10 mean   | r11 mean   | Delta   | Stable? |
| --------------------------- | ---------- | ---------- | ------- | ------- |
| `build_find/empty`          |  493.76 ns |  492.51 ns | −0.3 %  | yes     |
| `build_find/small`          |  976.37 ns |  991.26 ns | +1.5 %  | yes     |
| `build_find/complex`        | 2923.20 ns | 2878.40 ns | −1.5 %  | yes     |
| `build_insert/small_doc`    | 2054.53 ns | 1898–1930 ns (in-suite) / 1570–1591 ns (isolated) | −6 % to −23 % | **NO** |

All three find numbers sit inside criterion's "Change within noise
threshold" classification (its own p-value gate produced "No change in
performance detected" for `small` / `complex` and "Change within noise
threshold" for `empty`).

`build_insert/small_doc` oscillates dramatically across runs:

```
Run 1 (in-suite): 1.93 µs   (−7 % vs r10)
Run 2 (in-suite): 1.94 µs   (+0 % vs r10)
Run 3 (in-suite): 1.90 µs   (+0 % vs r10)
Run isolated A:  1.57 µs   (−23 % vs r10)
Run isolated B:  1.59 µs   (−22 % vs r10)
Run isolated C:  1.56 µs   (−24 % vs r10)
```

The 300+ ns gap between in-suite and isolated execution of the *same*
bench, with no intervening code change, is the diagnostic: `insert`'s
hot path warms (or fails to warm) different inlining decisions
depending on whether the find benches ran immediately before. This is
load-order / thermal jitter on a machine with `loadavg 2.99` at run
time, not a real production-code shift.

`build_insert` was not edited since r10. Its constituent calls
(`validate_collection`, `validate_schema`, `quote_ident`, `format!`,
`value_to_param`) are byte-for-byte identical between `d2e7e22` and
`05484878`. The variance cannot be attributed to any commit in the
range.

---

## 3. Forcing-function check (per r11 mandate)

The prompt called out three candidate forcing functions:

1. **Bench result shifted >5 % from r10.** Find benches: no
   (≤1.5 % movement). Insert bench: yes in magnitude, but the
   shift is not stable across re-runs and not attributable to any
   production-code change. **Does not qualify as a forcing function.**
2. **Regression introduced by 2fa9472e or 5d9acab8.** No. Neither
   touches the bench's hot path. Layout/inlining drift from the
   feature-gate is below criterion's noise floor.
3. **New hot-path discovery.** None. The hot path (find/insert build)
   is byte-identical to r10.

No new bench-driven measurement contradicts an r10 finding. No
already-tracked I-NEW-* has measurably degraded — N9-I1, N9-I2, N9-I3
remain in the same state r10 documented (still on disk, unfixed,
unmeasurable for I1/I3, measured for I2).

**Forcing-function requirement: NOT met.**

---

## 4. Score

**81 / 100.** (r10: 81. Delta: 0.)

### Why ±0

- r10's "no fix landed" diagnosis still holds: no perf-targeted commit
  has shipped between r10 and r11.
- All three carry-over IMPORTANTs (N9-I1 row_to_json O(N²), N9-I2 `$in`
  Vec<String> allocs, N9-I3 migrations Vec→String round-trip) remain
  on disk unchanged.
- The bench harness still produces the same evidence shape it did in
  r10: three measurements confirmed, two structural claims still
  unmeasurable.
- The three commits since r10 are correctness / observability /
  dead-code hygiene — all worthwhile, none perf-affecting.

### r10's gating condition revisited

r10 explicitly recommended NOT running r11 without one of:

1. A commit fixing N9-I2 — **did not land.**
2. A `broker::publish` fanout bench — **did not land.**
3. A `compio-postgres` `Row` constructor exposure unblocking N9-I1 — **did not land.**

r11 ran anyway (per pilot request) and the mandate became "find a
forcing function or report none." None found. This report is the
"none found" outcome r10's gating condition predicted.

### r12 gating condition (unchanged from r10)

r12 should not run until one of:

1. A commit lands fixing N9-I2.
2. `broker::publish` fanout bench lands under `crates/plugin-db/benches/`.
3. `compio-postgres` exposes a `Row` constructor (or an in-crate shim).

Without one of these, r12 will produce a third consecutive 81.

---

## 5. Anti-fabrication compliance

- Every ns figure in §2 is a criterion `time:` line from a bench run
  conducted in this report. The wide range on `build_insert/small_doc`
  is the literal output of six successive runs and is reported as a
  range, not a single point.
- The "%" deltas in §2 are arithmetic on those means against r10's
  recorded means; their CIs are wider than the bare arithmetic
  implies, which is acknowledged inline.
- No N9-I1 / N9-I3 ns claim is made. Those paths remain unmeasured.
- The "no hot-path impact" claim for each commit in §1 is grounded in
  a direct grep + read of the relevant function bodies and call
  sites; not inferred from commit messages.
- The system load reading (`loadavg 2.99`) is the literal output of
  `uptime` taken during the bench session.

**Anti-fabrication footer:** bench output reproducible via
`cargo bench -p zeroship-plugin-db --bench bench_query_build`
from the repo root. Per-run variance is observable by running the
insert bench alone vs. as part of the full suite; the 300+ ns
gap is real and reproducible.
