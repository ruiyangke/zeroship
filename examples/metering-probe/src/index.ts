// metering-probe — the smallest faithful billing/metering exercise app.
//
// Metering is INFRASTRUCTURE: there is no `env.meter` creator API. The
// billing signal is platform-measured — the worker emits the five platform
// counters per dispatch, and the trusted data primitives emit raw usage
// metrics at their op boundary. App code can neither forge nor suppress
// them. So this probe simply DRIVES a measurable primitive op per request;
// the platform emits the metrics the E2E asserts on.
//
// Each request:
//   * reads the request body (so ingress_bytes is measurable),
//   * inserts one row via `env.db` (→ platform-emitted `db_writes`),
//   * reads it back via `env.db` (→ platform-emitted `db_reads`),
//   * returns a fixed, measurable response body (so egress_bytes is non-zero).
//
// The probe drives env.db so the platform emits DB usage metrics.
//
// MIGRATION-FIRST, and it was not always. This app used to declare its schema
// INLINE (`export default { schema: dbSchema }` built from `schema()`/`t.*`)
// with no `migrations/` directory at all. That is the #209 mechanism: the
// installer builds `env.db` from the generated runtime descriptor, which is
// folded from committed migrations, and an inline `schema` export is not a
// source for it. The app built and served fine, and EVERY insert failed --
// measured 2026-08-11, `wrote:false` and `readBack:0` on all 100 requests of
// tests/e2e_metering_billing.sh, which passed anyway because both of its
// env.db guards were unfailable (fixed eda51b973). Schema now comes from
// migrations/, exactly as examples/db-hitcounter does it.

import { env } from "zeroship";

// The platform-emitted metric the harness asserts on (greppable constant).
export const PRIMARY_METRIC = "db_writes";

export default {
  async fetch(request: Request, _env: any): Promise<Response> {
    // Drain the request body so the worker tallies ingress bytes.
    const inBody = await request.text();
    const u = new URL(request.url);

    // Drive a real db write + read. `env.db` is installed from the generated
    // runtime descriptor, which is folded from migrations/ -- NOT from any
    // export of this file. The native primitive emits `db_writes` /
    // `db_reads` on success only: platform-measured, not reported here.
    let wrote = false;
    let readBack = 0;
    let dbError: string | null = null;
    try {
      const ins = await env.db.hits.insert({ path: u.pathname });
      wrote = !ins.error;
      if (ins.error) dbError = ins.error.message ?? String(ins.error);
      const { data } = await env.db.hits.find({}).limit(1);
      readBack = Array.isArray(data) ? data.length : 0;
    } catch (e) {
      // Reported, not swallowed. The previous version caught this and said
      // "the harness falls back to the platform counters" -- the app-side twin
      // of the harness guard that could not fail, and between them a broken
      // env.db looked like a healthy run for as long as both stood.
      dbError = e instanceof Error ? e.message : String(e);
    }

    const payload = {
      ok: true,
      app: "metering-probe",
      metric: PRIMARY_METRIC,
      wrote,
      readBack,
      dbError,
      path: u.pathname,
      received_bytes: inBody.length,
      // Fixed filler so egress_bytes is comfortably non-zero.
      filler: "z".repeat(256),
    };
    return new Response(JSON.stringify(payload), {
      headers: { "content-type": "application/json" },
    });
  },
};
