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
// The probe declares a `default.schema` so `env.db` is installed at app boot.

import { env } from "zeroship";
import { schema, t } from "@zeroship/db";

export const dbSchema = {
  // One tiny collection — a row per request bumps `db_writes`.
  hits: schema({
    path: t.string().required().max(256),
  }),
};

// The platform-emitted metric the harness asserts on (greppable constant).
export const PRIMARY_METRIC = "db_writes";

export default {
  schema: dbSchema,

  async fetch(request: Request, _env: any): Promise<Response> {
    // Drain the request body so the worker tallies ingress bytes.
    const inBody = await request.text();
    const u = new URL(request.url);

    // Drive a real db write + read. `env.db` is installed from `schema`.
    // The native primitive emits `db_writes` / `db_reads` on success —
    // platform-measured, not reported by this app.
    let wrote = false;
    let readBack = 0;
    try {
      const ins = await env.db.hits.insert({ path: u.pathname });
      wrote = !ins.error;
      const { data } = await env.db.hits.find({}).limit(1);
      readBack = Array.isArray(data) ? data.length : 0;
    } catch (_e) {
      // If the db namespace is unavailable in a degraded config, the
      // platform counters still flow; the harness falls back to those.
    }

    const payload = {
      ok: true,
      app: "metering-probe",
      metric: PRIMARY_METRIC,
      wrote,
      readBack,
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
