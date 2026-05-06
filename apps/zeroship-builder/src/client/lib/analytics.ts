// ─── analytics — telemetry emitter (V1 stub) ─────────────────────
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §8.2.8 + §28: tiny event emitter. V1 just logs to the
// console + keeps a ring-buffer of recent events for the dev events
// badge to surface (DevEventsBadge component below the fold). A real
// telemetry transport will land later; centralising it here means
// every emission goes through one path.
//
// `props` is best-effort serialisable. If a caller hands us a
// circular ref the JSON.stringify in the console pretty-printer will
// throw; we swallow it so a telemetry bug never breaks user flow.

export interface TrackedEvent {
  /** Event name as fired. */
  name: string;
  /** Best-effort serialisable props blob. May be undefined. */
  props?: Record<string, unknown>;
  /** ISO-8601 timestamp at emission time. */
  at: string;
}

const RING_SIZE = 50;
const ring: TrackedEvent[] = [];
const listeners = new Set<(events: ReadonlyArray<TrackedEvent>) => void>();

function notify(): void {
  // Defensive copy — listeners shouldn't mutate the ring directly.
  const snapshot = ring.slice();
  for (const l of listeners) {
    try { l(snapshot); } catch { /* listener bug, ignore */ }
  }
}

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
  // Ring-buffer push — keep the most recent RING_SIZE events. Older
  // entries fall off the head. We push to the front so the dev badge
  // shows newest-first without reversing.
  ring.unshift({ name: event, props, at: new Date().toISOString() });
  if (ring.length > RING_SIZE) ring.length = RING_SIZE;
  notify();
}

/**
 * Subscribe to the in-memory event ring. Returns an unsubscribe fn.
 * The first invocation gets the current snapshot synchronously so
 * mounting components don't have to wait for the next event.
 */
export function subscribeEvents(
  fn: (events: ReadonlyArray<TrackedEvent>) => void,
): () => void {
  listeners.add(fn);
  // Initial snapshot so the consumer renders without waiting.
  try { fn(ring.slice()); } catch {}
  return () => { listeners.delete(fn); };
}

/** Read-only snapshot of recent events. Newest first. */
export function recentEvents(): ReadonlyArray<TrackedEvent> {
  return ring.slice();
}
