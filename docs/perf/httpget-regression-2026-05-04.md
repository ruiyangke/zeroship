# httpGet regression — 2026-05-04 investigation

## Confirmed numbers

| Commit / Bench | httpGet 16w (req/s) |
|---|---|
| 2026-04-19 (350afed dev mode) | 490,643 |
| 2026-04-27 (after-G-track) — peak | **534,189** |
| 2026-04-29 (8f21f3e baseline) | 512,000 (3-run avg) |
| **2026-05-01 `89394ee` (parent)** | **482,000** |
| **2026-05-01 `f90e264` (regression)** | **350,000** ← -27% in one commit |
| 2026-05-04 main HEAD `38f9200` | 330,000 (3-run avg) |

Total window: -180k req/s (-35%) between 2026-04-29 and 2026-05-04.

## Method

Bisect on a focused single-scenario bench (`--scenario=httpGet --duration=10s --target=v8-16w`).
Bisect path: HEAD (330k) → fetch-perf-v2 (264k) → fetch-perf (228k) → streams-native (352k)
→ sandbox-nomad-ch (352k) → narrowed to `89394ee/f90e264` window. Confirmed via 3-run
averages.

## Offending commit

**`f90e264` — "runtime: delete Headers polyfill, install native unconditionally"**
(2026-05-01 09:28).

Diffstat: `-178` LOC `embed/fetch.js`. Removes `ZEROSHIP_NATIVE_HEADERS` env-var gate in
`init.rs`. No callsite changes — `89394ee` had already moved callsites to native.

## Diagnosis

Switching `globalThis.Headers` from JS polyfill to the native `#[v8_class]` adds a V8
boundary crossing per `headers.get(...)` call AND per `Response.json(...)` allocation.

scenarios.js does ONE `request.headers.get("upgrade")` and ONE `Response.json({...})` per
`/hello` request. Two boundary crossings per request at ~315k req/s × 16 workers = ~10M
crossings/s. Each crossing pays the FunctionCallback prologue (External lookup, scope
setup, brand check, *self recovery).

Hot path today:
- `core/runtime.rs:1325 build_kernel_request` (fast — uses `build_kernel_headers`, skips WebIDL ctor)
- → JS `default.fetch`
- → `request.headers.get("upgrade")` (slow — full WebIDL boundary)
- → `Response.json({method, url})` (slow ctor)
- → `web/fetch/mod.rs inspect_response` (slow iterator walk)

The kernel Request build is on the fast-path; the per-request cost is on the
**user-visible Headers reads and Response construction**, not Request construction.

## Recovery options (ranked by leverage)

1. **V8 fastcall on `Headers.has` / `Headers.get` / `Headers.set`** (already in
   `crates/runtime-macros/TODO.md` Open as the "V8 fastcall" item). Turbofan inlines the
   `CFunction` shim, ~10-30 ns saved per call. Should recover ~50-80k req/s.
2. **`Response.json` fastcall** (single-arg primitive wrapper case). Less typed than #1
   because the body is an arbitrary JS value; would need a slow-path fallback.
3. **Cache `upgrade` lookup in Rust during `build_kernel_request`** — bench-specific
   shortcut. Re-introduces a special case for one header. Rejected as architectural
   noise.
4. **Revert `f90e264`** — re-introduces 178 LOC of spec-divergent JS that failed WPT,
   lacked Set-Cookie semantics, and snapshotted iterators. **Rejected.**

## Recommendation

**ACCEPT** for now. The 27% drop is the cost of correctness (moving from a buggy
polyfill to spec-compliant native). Fix-forward via Option 1 (V8 fastcall) when that
macro lands.

V8 fastcall ROI table in `crates/runtime-macros/TODO.md` already lists `Headers.has`
as a top candidate. This investigation promotes Headers.has + Headers.get to **Tier 1**
fastcall priority — they have empirical evidence of being on the request-per-second
critical path.

## Remaining gap to peak

After fastcall lands and recovers ~80k: ~410k req/s. Still ~120k below the 534k peak.
That residual likely comes from:
- `Response.json` constructor (slow path; see option 2)
- Body materialization changes from the same fetch.js delete chain

Out of scope for this investigation. Track separately if Tier-1 fastcall doesn't close
the gap.
