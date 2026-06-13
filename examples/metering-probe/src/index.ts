// metering-probe — the smallest faithful billing/metering exercise app.
//
// A plain `{ fetch }` app (URL resources default to anon/public, so every
// request dispatches through the gateway with no session). Each request:
//
//   * reads the request body (so ingress_bytes is measurable),
//   * bumps a CUSTOM meter counter via `env.meter.increment("probe_hits")`
//     — the producer side of the metering pipeline the E2E asserts on,
//   * returns a fixed, measurable response body (so egress_bytes is non-zero),
//     including the metric's new running total so a caller can sanity-check
//     the synchronous increment.
//
// The five platform counters (requests, cpu_us, wall_us, egress_bytes,
// ingress_bytes) are fed automatically by the worker for every dispatch;
// this app only adds the custom one.
//
// The custom metric name is exported as a constant the harness greps for.
export const CUSTOM_METRIC = "probe_hits";

export default {
  async fetch(request: Request, env: any): Promise<Response> {
    // Drain the request body so the worker tallies ingress bytes.
    const inBody = await request.text();

    // Synchronous atomic bump of the per-app custom counter. The return is
    // the metric's new running total within this worker process.
    const total = env.meter.increment(CUSTOM_METRIC);

    const u = new URL(request.url);
    const payload = {
      ok: true,
      app: "metering-probe",
      metric: CUSTOM_METRIC,
      // The running in-process total (NOT the aggregated control-plane total).
      meter_total_in_process: total,
      path: u.pathname,
      received_bytes: inBody.length,
      // A chunk of fixed filler so egress_bytes is comfortably non-zero even
      // for an empty request body.
      filler: "z".repeat(256),
    };
    return new Response(JSON.stringify(payload), {
      headers: { "content-type": "application/json" },
    });
  },
};
