// ─── analytics — telemetry emitter (V1 stub) ─────────────────────
//
// Per spec §8.2.8 + §28: tiny event emitter. V1 just logs to the
// console; a real telemetry endpoint comes later. Centralising it
// here means every emission goes through one path — when the real
// transport lands, every callsite picks it up automatically.
//
// `props` is best-effort serialisable. If a caller hands us a
// circular ref the JSON.stringify in the console pretty-printer will
// throw; we swallow it so a telemetry bug never breaks user flow.

export function track(event: string, props?: Record<string, unknown>): void {
  try {
    if (props === undefined) {
      console.info("[track]", event);
    } else {
      console.info("[track]", event, props);
    }
  } catch {
    // Telemetry must never break the host app.
  }
}
